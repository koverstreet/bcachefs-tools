// SPDX-License-Identifier: GPL-2.0

//! A file's own ioctls that need only the filesystem. vfs/ioctl.rs's
//! dispatch answers them here, with what each needs of the file - its inode
//! number, whether it's a directory - and keeps the VFS's checks of
//! ownership and permission, which come in as arguments or a callback.

use core::mem::{offset_of, size_of};
use core::ops::{ControlFlow, Range};
use crate::alloc::{accounting, capacity::DiskReservation, disk_groups_format};
use crate::{bch_warn, btree_trans};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{self, BkeySC};
use crate::btree::iter::{is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::bcachefs_ioctl::{IoctlGetDamage, IoctlReaddirFlags, IoctlSnapshotNode,
    IoctlSnapshotNodeV2, IoctlSnapshotTreeQuery, IoctlSnapshotTreeQueryV2,
    IoctlSubvolReaddir, IoctlSubvolToPath, IoctlUnpoison, ReaddirFlags, IoctlReaddirEntryHead,
    IoctlSubvolDirentHead};
use crate::data::extents_format::{bch_extent_flags, ExtentFlags};
use crate::data::reconcile::trigger::SetNeedsReconcileCtx;
use crate::data::reflink_format::ReflinkP;
use crate::opts::InodeOpts;
use crate::posix::dirent_format::DT_SUBVOL;
use crate::snapshots::format::{SnapshotState, SnapshotTree, SubvolumeState};
use crate::snapshots::subvolume::SubvolInum;
use crate::data::ptrs::{BkeyVPtrs, PtrsMut};
use crate::data::reconcile::needs_reconcile::update_reconcile_opts;
use crate::posix::dirent::{self, Dirent};
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::init::damage;
use crate::posix::inode_opts::get_io_opts;
use crate::posix::namei;
use crate::snapshots::{snapshot, subvolume};
use crate::util::{Printbuf, alloc::Vec, os_str::OsStrExt, vstructs::{plain_bytes, Plain}};
use crate::os::{ioctl::{UserArg, UserVstruct}, kernel::{capable, Capability}};
use kernel::uaccess::{UserPtr, UserSlice};
use crate::format::{Btree, BkeyType, MetadataVersion};

/// FS_IOC_GETFSLABEL's: the label, without its NUL - and if it fills its
/// field, without its last byte, as C had it.
pub fn get_label(fs: &Fs, user: UserArg<[u8; c::FSLABEL_MAX as usize]>) -> Result<(), BchError> {
    const LABEL_SIZE: usize = disk_groups_format::LABEL_SIZE as usize;
    const { assert!(LABEL_SIZE < c::FSLABEL_MAX as usize) };

    let label = fs.sb_lock().sb().label;
    let mut len = label.iter().position(|&b| b == 0).unwrap_or(LABEL_SIZE);
    if len == LABEL_SIZE {
        len -= 1;
        bch_warn!(fs, "label is too long, return the first {len} bytes");
    }
    user.bytes_at(0, len).writer().write_slice(&label[..len])?;
    Ok(())
}

/*
 * Readdir-style replies - readdir_flags', the subvolume list's: records of a
 * header, a NUL-terminated name and zeroes to a multiple of 8, the header's
 * reclen their size, appended to the caller's buffer while they fit.
 *
 * Userspace is written with no btree locks held: a fault on the buffer - an
 * mmap of a file here - goes through our read path, which takes them. An
 * entry is built in memory, from the key, and the transaction unlocked to
 * copy it out - the key's last use: the walks relock, or begin anew, at
 * their next key.
 */

/// The caller's buffer, @used bytes of it filled, and whether an entry
/// didn't fit; @rec, an entry on its way.
struct Entries {
    buf:  u64,
    size: u32,
    used: u32,
    full: bool,
    rec:  Vec<u8>,
}

impl Entries {
    fn new(buf: u64, size: u32) -> Self {
        Entries { buf, size, used: 0, full: false, rec: Vec::new() }
    }

    /// Append an entry, @name after the header @hdr makes for its reclen, if
    /// it fits. Unlocks @t.
    fn emit<H: Plain>(&mut self, t: &TransAttempt<'_, '_>, name: &[u8], hdr: impl FnOnce(u32) -> H)
        -> Result<bool, BchError>
    {
        let hdr_bytes = size_of::<H>();
        let reclen = (hdr_bytes + name.len() + 1).next_multiple_of(8);
        if self.used as usize + reclen > self.size as usize {
            self.full = true;
            return Ok(false);
        }

        self.rec.clear();
        self.rec.extend_from_slice(plain_bytes(&hdr(reclen as u32)))?;
        self.rec.extend_from_slice(name)?;
        self.rec.extend_from_slice(&[0; 8][..reclen - hdr_bytes - name.len()])?;

        t.unlock();
        let at = UserPtr::from_addr((self.buf as usize).wrapping_add(self.used as usize));
        UserSlice::new(at, reclen).writer().write_slice(&self.rec)?;
        self.used += reclen as u32;
        Ok(true)
    }

    /// ERANGE if the next entry wouldn't fit even an empty buffer.
    fn check_room(&self) -> Result<(), BchError> {
        if self.full && self.used == 0 { Err(Errcode::ERANGE_readdir_flags_buf.into()) } else { Ok(()) }
    }
}

/* A file's damage, and readdir_flags */

/// BCHFS_IOC_GET_DAMAGE's: inode @inum's damage, as its subvolume sees it.
/// The count is the true one, however few entries the caller had room for.
pub fn get_damage(fs: &Fs, inum: SubvolInum, user: UserVstruct<IoctlGetDamage>)
    -> Result<(), BchError>
{
    let mut arg = user.head()?;
    if arg.pad != 0 {
        return fs.throw(Errcode::EINVAL_get_damage_pad);
    }

    let errors = {
        let trans = btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;
            damage::accumulate(t, inum.inum, snapshot)
        })?
    };

    let entries = user.tail(&arg);
    for (i, e) in errors.iter().take(entries.len()).enumerate() {
        entries.set(i, &e.into())?;
    }
    arg.nr_entries = errors.len() as u32;
    user.set_head(&arg)
}

/*
 * BCHFS_IOC_READDIR_FLAGS lists a directory's entries that pass its filter
 * into the caller's buffer, as IoctlReaddirEntry records, and returns
 * a cursor, pos, to resume from: a full buffer ends the call, not the
 * listing - unless the next entry wouldn't fit even an empty one, which is
 * ERANGE: a recursive listing's names are paths, of any length.
 *
 * Unrecursive, it's readdir with a filter, and the cursor is a dirent's
 * offset. Recursive, it doesn't walk the tree: it iterates its filter's
 * candidates - the damage btree's keys, or the subvolumes - and qualifies
 * each: is it under the directory, what's its path from it, and may the
 * caller see it - the permissions a walk down to it would have checked,
 * which are the VFS's, and so a callback. A candidate that doesn't qualify,
 * as one racing a rename might not, is skipped. The cursor is the
 * candidate's position in its btree.
 */

/// Whether @flags' filter keeps dirent @d, in @dir - whose snapshot is @view
/// - pointing at @target.
fn readdir_filter(t: &TransAttempt<'_, '_>, flags: ReaddirFlags, dir: SubvolInum, view: u32,
                  d: Dirent<'_>, target: SubvolInum) -> Result<bool, BchError> {
    let filter = flags.difference(ReaddirFlags::RECURSIVE);
    if filter.is_empty() {
        Ok(true)
    } else if filter == ReaddirFlags::DAMAGED {
        // a subvolume's root is in its own snapshot
        let snapshot = if target.subvol != dir.subvol {
            subvolume::get_snapshot(t, target.subvol as u32)?
        } else {
            view
        };
        damage::inode_has_damage(t, target.inum, snapshot)
    } else if filter == ReaddirFlags::SUBVOLUMES_ONLY {
        Ok(u32::from(d.d_type()) == DT_SUBVOL)
    } else {
        t.fs().throw(Errcode::EINVAL_readdir_filter_flags)
    }
}

fn readdir_plain(fs: &Fs, dir: SubvolInum, flags: ReaddirFlags, out: &mut Entries, pos: u64)
    -> Result<u64, BchError>
{
    let trans = btree_trans!(fs);
    let view = lockrestart_do(&trans, |t| subvolume::get_snapshot(t, dir.subvol as u32))?;

    let mut cursor = pos;
    let mut iter = BtreeIter::new(&trans, Btree::dirents, bkey::pos(dir.inum, pos),
                                  BtreeIterFlags::empty());
    subvolume::for_each_in_subvolume_max_in_trans(&trans, &mut iter, bkey::pos(dir.inum, u64::MAX),
                                                  dir.subvol as u32, BtreeIterFlags::empty(), |t, _, k| {
        let Some(d) = Dirent::new(k) else { return Ok(ControlFlow::Continue(())) };
        let Some(target) = dirent::read_target(t, dir, d)? else { return Ok(ControlFlow::Continue(())) };
        let next = k.k.p.offset + 1;

        if readdir_filter(t, flags, dir, view, d, target)? &&
           !out.emit(t, d.name().as_bytes(), |reclen| IoctlReaddirEntryHead {
               reclen, d_type: d.d_type(), inum: target.inum, ..Default::default()
           })? {
            return Ok(ControlFlow::Break(()));
        }
        cursor = next;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(cursor)
}

/// A recursive listing: the directory, its path, and what qualifies a
/// candidate.
struct Recursive<A> {
    dir:        SubvolInum,
    dir_path:   Printbuf,
    path:       Printbuf,
    accessible: A,
    out:        Entries,
}

enum Emitted {
    Yes,
    Skipped,
    Full,
}

impl<A> Recursive<A>
where
    A: FnMut(&TransAttempt<'_, '_>, SubvolInum) -> Result<bool, BchError>,
{
    /// Qualify @n and emit it, named by its path from the directory: not the
    /// directory itself, whose path from itself is empty.
    fn emit(&mut self, t: &TransAttempt<'_, '_>, n: SubvolInum, d_type: u8)
        -> Result<Emitted, BchError>
    {
        if !namei::inum_is_descendant(t, n, self.dir)? {
            return Ok(Emitted::Skipped);
        }

        self.path.reset();
        namei::inum_to_path(t, n, &mut self.path)?;

        // the root's path is "/", and any other's a prefix of its children's
        // up to a '/'
        let (dir, path) = (self.dir_path.as_bytes(), self.path.as_bytes());
        let plen = dir.len();
        if !(path.len() > plen && path.starts_with(dir) && (plen == 1 || path[plen] == b'/')) {
            return Ok(Emitted::Skipped);
        }
        let name = &path[if plen == 1 { 1 } else { plen + 1 }..];

        if !(self.accessible)(t, n)? {
            return Ok(Emitted::Skipped);
        }

        let hdr = |reclen| IoctlReaddirEntryHead { reclen, d_type, inum: n.inum, ..Default::default() };
        Ok(if self.out.emit(t, name, hdr)? { Emitted::Yes } else { Emitted::Full })
    }

    /// Damaged: the damage btree's keys, one per snapshot version of an
    /// inode with damage, sorted by inum, that the directory's snapshot sees.
    /// A whiteout is damage cleared in this view, inherited records with it.
    /// Each inum is emitted once, and the cursor moves past every version of
    /// one handled: a buffer that filled between two would otherwise emit it
    /// twice.
    fn damaged(&mut self, fs: &Fs, pos: [u64; 2]) -> Result<[u64; 2], BchError> {
        let trans = btree_trans!(fs);
        let view = lockrestart_do(&trans, |t| {
            self.dir_path.reset();
            let view = subvolume::get_snapshot(t, self.dir.subvol as u32)?;
            namei::inum_to_path(t, self.dir, &mut self.dir_path)?;
            Ok(view)
        })?;

        let mut cursor = bkey::spos(0, pos[0], pos[1] as u32);
        let mut last_handled = 0;
        let mut iter = BtreeIter::new(&trans, Btree::damage, cursor,
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each(&trans, |t, _, k| {
            let (p, inum) = (k.k.p, k.k.p.offset);

            if inum != last_handled && snapshot::is_ancestor(t, view, p.snapshot) {
                if k.k.key_type() != BkeyType::damage {
                    last_handled = inum;
                } else {
                    let n = SubvolInum { subvol: self.dir.subvol, inum };
                    match self.emit(t, n, c::DT_UNKNOWN as u8)? {
                        Emitted::Full    => return Ok(ControlFlow::Break(())),
                        Emitted::Yes     => last_handled = inum,
                        Emitted::Skipped => {}
                    }
                }
            }

            cursor = if inum == last_handled {
                bkey::spos(0, inum, u32::MAX).successor()
            } else {
                p.successor()
            };
            Ok(ControlFlow::Continue(()))
        })?;
        Ok([cursor.offset, cursor.snapshot.into()])
    }

    /// Subvolumes: the live ones - an unlinked or deleted subvolume has no
    /// dirent, so no path - by their roots.
    fn subvols(&mut self, fs: &Fs, pos: u64) -> Result<u64, BchError> {
        let trans = btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            self.dir_path.reset();
            namei::inum_to_path(t, self.dir, &mut self.dir_path)
        })?;

        let mut cursor = pos;
        let mut iter = BtreeIter::new(&trans, Btree::subvolumes, bkey::pos(0, pos),
                                      BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |t, _, k| {
            let subvol = k.k.p.offset;

            if let Some(s) = subvolume::val(k) {
                if s.state_decoded() == Some(SubvolumeState::live) {
                    let n = SubvolInum { subvol, inum: s.inode.get() };
                    if let Emitted::Full = self.emit(t, n, DT_SUBVOL as u8)? {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
            cursor = subvol + 1;
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(cursor)
    }
}

/// BCHFS_IOC_READDIR_FLAGS's: see above. @dir is the directory - ENOTDIR if
/// @is_dir says it isn't one - and @accessible whether the caller may see
/// what a recursive listing found.
pub fn readdir_flags<A>(fs: &Fs, dir: SubvolInum, is_dir: bool, accessible: A,
                        user: UserArg<IoctlReaddirFlags>) -> Result<(), BchError>
where
    A: FnMut(&TransAttempt<'_, '_>, SubvolInum) -> Result<bool, BchError>,
{
    let arg = user.read()?;
    let Some(flags) = ReaddirFlags::from_bits(arg.flags).filter(|_| arg.pad == 0 && arg.used == 0) else {
        return fs.throw(Errcode::EINVAL_readdir_flags_args);
    };
    if !is_dir {
        return fs.throw(Errcode::ENOTDIR_readdir_flags);
    }

    type Q = IoctlReaddirFlags;
    let mut out = Entries::new(arg.buf, arg.buf_size);

    // only what the call moved: an unrecursive cursor is one word
    if !flags.contains(ReaddirFlags::RECURSIVE) {
        let pos = readdir_plain(fs, dir, flags, &mut out, arg.pos[0])?;
        out.check_room()?;
        user.write_at(offset_of!(Q, pos), &pos)?;
        return user.write_at(offset_of!(Q, used), &out.used);
    }

    let mut r = Recursive { dir, dir_path: Printbuf::new(), path: Printbuf::new(), accessible, out };
    let filter = flags.difference(ReaddirFlags::RECURSIVE);
    let pos = if filter == ReaddirFlags::DAMAGED {
        r.damaged(fs, arg.pos)?
    } else if filter == ReaddirFlags::SUBVOLUMES_ONLY {
        [r.subvols(fs, arg.pos[0])?, 0]
    } else {
        // unfiltered: an honest tree walk, not yet
        return fs.throw(Errcode::EOPNOTSUPP_readdir_flags_unfiltered);
    };
    r.out.check_room()?;
    user.write_at(offset_of!(Q, pos), &pos)?;
    user.write_at(offset_of!(Q, used), &r.out.used)
}

/*
 * A file's subvolumes and snapshots: the subvolumes under the file's, a
 * subvolume's path, and the file's snapshot tree - or any, for an admin.
 * Paths come back relative: without inum_to_path()'s leading '/'.
 */

fn relative(path: &[u8]) -> &[u8] {
    &path[path.iter().take_while(|&&b| b == b'/').count()..]
}

/// BCH_IOCTL_SUBVOLUME_LIST's: the subvolumes whose parent is @parent, the
/// file's, by their paths from its root, as IoctlSubvolDirent entries -
/// those @accessible says the caller may reach. The cursor is a subvolume
/// id; a subvolume whose path can't be found is skipped. ERANGE if the next
/// entry wouldn't fit even an empty buffer: a path's length has no limit.
pub fn subvolume_list<A>(fs: &Fs, parent: u32, mut accessible: A,
                         user: UserArg<IoctlSubvolReaddir>) -> Result<(), BchError>
where
    A: FnMut(&TransAttempt<'_, '_>, SubvolInum, u32) -> Result<bool, BchError>,
{
    type Q = IoctlSubvolReaddir;
    let arg = user.read()?;
    if arg.pad != 0 {
        return fs.throw(Errcode::EINVAL_subvol_readdir_pad);
    }

    let mut out = Entries::new(arg.buf, arg.buf_size);
    let mut cursor = arg.pos;
    {
        let trans = btree_trans!(fs);
        let mut path = Printbuf::new();
        let mut iter = BtreeIter::new(&trans, Btree::subvolume_children,
                                      bkey::pos(parent.into(), arg.pos.into()), BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |t, _, k| {
            if k.k.p.inode != u64::from(parent) {
                return Ok(ControlFlow::Break(()));
            }
            let child = k.k.p.offset as u32;
            let s = subvolume::get(t, child, true)?;
            let root = SubvolInum { subvol: child.into(), inum: s.inode.get() };

            let found = accessible(t, root, parent)? && {
                path.reset();
                match namei::inum_to_path_in_subvol(t, root, parent, &mut path) {
                    Err(e) if !is_restart(&e) => false,
                    r => { r?; true }
                }
            };

            if found {
                let otime = fs.time_to_timespec(s.otime.lo.get() as i64);
                let hdr = |reclen| IoctlSubvolDirentHead {
                    reclen,
                    subvolid:        child,
                    flags:           s.flags.into_bits().get(),
                    snapshot_parent: s.creation_parent.get(),
                    otime_sec:       otime.tv_sec as u64,
                    otime_nsec:      otime.tv_nsec as u32,
                    ..Default::default()
                };
                if !out.emit(t, relative(path.as_bytes()), hdr)? {
                    return Ok(ControlFlow::Break(()));
                }
            }
            cursor = child.wrapping_add(1);
            Ok(ControlFlow::Continue(()))
        })?;
    }

    out.check_room()?;
    user.write_at(offset_of!(Q, pos), &cursor)?;
    user.write_at(offset_of!(Q, used), &out.used)
}

/// BCH_IOCTL_SUBVOLUME_TO_PATH's: subvolume @subvolid's path from the
/// filesystem's root, with its NUL; ERANGE if the buffer won't hold it.
pub fn subvolume_to_path(fs: &Fs, user: UserArg<IoctlSubvolToPath>) -> Result<(), BchError> {
    let arg = user.read()?;
    if arg.buf_size == 0 {
        return fs.throw(Errcode::EINVAL_subvol_to_path_no_buf);
    }

    let mut path = Printbuf::new();
    {
        let trans = btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            path.reset();
            let s = subvolume::get(t, arg.subvolid, false)?;
            let root = SubvolInum { subvol: arg.subvolid.into(), inum: s.inode.get() };
            namei::inum_to_path(t, root, &mut path)
        })?;
    }

    let p = relative(path.as_bytes());
    if p.len() + 1 > arg.buf_size as usize {
        return fs.throw(Errcode::ERANGE_subvol_to_path_buf);
    }
    let mut w = UserSlice::new(UserPtr::from_addr(arg.buf as usize), p.len() + 1).writer();
    w.write_slice(p)?;
    w.write_slice(&[0])?;
    Ok(())
}

/// What a snapshot tree query answers besides its nodes: the tree, and how
/// many nodes it has and were returned.
struct SnapshotTreeReply {
    tree:  SnapshotTree,
    nr:    u32,
    total: u32,
}

/*
 * BCH_IOCTL_SNAPSHOT_TREE and _v2: the nodes of snapshot tree @tree_id - 0
 * for the tree of @subvol, the file's; another is an admin's to ask - that
 * aren't deleted, each with its accounting summed over the snapshotted
 * btrees - memory's, current without a write buffer flush. @size of them
 * are returned, @node_size bytes of each, at nodes(i): a v1 node is a
 * prefix of a v2's. ERANGE if there were more, the reply written
 * regardless.
 */
fn snapshot_tree(fs: &Fs, subvol: u32, tree_id: u32, size: u32, node_size: usize,
                 nodes: impl Fn(u32) -> UserSlice,
                 put: impl FnOnce(&SnapshotTreeReply) -> Result<(), BchError>) -> Result<(), BchError> {
    if tree_id != 0 && !capable(Capability::SysAdmin) {
        return fs.throw(Errcode::EPERM_non_admin);
    }

    let (tree_id, tree) = {
        let trans = btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            let tree_id = if tree_id != 0 {
                tree_id
            } else {
                match fs.snapshots.tree(subvolume::get(t, subvol, false)?.snapshot.get()) {
                    0  => return fs.throw(Errcode::ENOENT_snapshot_tree_of_subvol),
                    id => id,
                }
            };
            Ok((tree_id, snapshot::tree_lookup(t, tree_id)?))
        })?
    };

    let mut r = SnapshotTreeReply { tree, nr: 0, total: 0 };
    {
        let trans = btree_trans!(fs);
        let mut iter = BtreeIter::new(&trans, Btree::snapshots, bkey::POS_MIN,
                                      BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |t, _, k| {
            let Some(s) = snapshot::val(k) else { return Ok(()) };
            if s.tree() != tree_id || s.state_compat() == SnapshotState::deleted {
                return Ok(());
            }

            let id = k.k.p.offset as u32;
            let mut node = IoctlSnapshotNodeV2 {
                id,
                parent:   s.parent(),
                children: s.children(),
                subvol:   s.subvol(),
                flags:    s.flags.into_bits().get(),
                ..Default::default()
            };
            // summed as C did, wrapping: a corrupt counter is a wrong answer, not a panic
            for v in accounting::snapshot_btrees(fs, id) {
                node.nr_keys   = node.nr_keys.wrapping_add(v.keys);
                node.key_bytes = node.key_bytes.wrapping_add(v.key_bytes);
                node.sectors   = node.sectors.wrapping_add(v.sectors);
            }

            r.total += 1;
            if r.nr < size {
                t.unlock();
                nodes(r.nr).writer().write_slice(&plain_bytes(&node)[..node_size])?;
                r.nr += 1;
            }
            Ok(())
        })?;
    }

    put(&r)?;
    if size != 0 && size < r.total {
        return fs.throw(Errcode::ERANGE_snapshot_tree_buf);
    }
    Ok(())
}

/// BCH_IOCTL_SNAPSHOT_TREE's: see snapshot_tree().
pub fn snapshot_tree_v1(fs: &Fs, subvol: u32, user: UserArg<IoctlSnapshotTreeQuery>)
    -> Result<(), BchError>
{
    type Q = IoctlSnapshotTreeQuery;
    const NODE: usize = size_of::<IoctlSnapshotNode>();

    let arg = user.read()?;
    if arg.pad != 0 {
        return fs.throw(Errcode::EINVAL_snapshot_tree_query_pad);
    }

    snapshot_tree(fs, subvol, arg.tree_id, arg.nr, NODE,
        |i| user.bytes_at(offset_of!(Q, nodes) + i as usize * NODE, NODE),
        |r| {
            user.write_at(offset_of!(Q, master_subvol), &r.tree.master_subvol.get())?;
            user.write_at(offset_of!(Q, root_snapshot), &r.tree.root_snapshot.get())?;
            user.write_at(offset_of!(Q, nr), &r.nr)?;
            user.write_at(offset_of!(Q, total), &r.total)
        })
}

/// BCH_IOCTL_SNAPSHOT_TREE_v2's: see snapshot_tree().
/// The node size is negotiated: the smaller of the caller's and ours, and
/// ours returned first.
pub fn snapshot_tree_v2(fs: &Fs, subvol: u32, user: UserArg<IoctlSnapshotTreeQueryV2>)
    -> Result<(), BchError>
{
    type Q = IoctlSnapshotTreeQueryV2;
    const NODE: usize = size_of::<IoctlSnapshotNodeV2>();

    let arg = user.read()?;
    // a caller that doesn't say its node size can't be given nodes: we'd
    // have to guess its stride
    if arg.node_size == 0 {
        return fs.throw(Errcode::EINVAL_snapshot_tree_query_pad);
    }
    let node_size = (arg.node_size as usize).min(NODE);
    user.write_at(offset_of!(Q, node_size), &(NODE as u32))?;

    snapshot_tree(fs, subvol, arg.tree_id, arg.nr, node_size,
        |i| user.bytes_at(offset_of!(Q, nodes) + i as usize * node_size, node_size),
        |r| {
            user.write_at(offset_of!(Q, master_subvol), &r.tree.master_subvol.get())?;
            user.write_at(offset_of!(Q, root_snapshot), &r.tree.root_snapshot.get())?;
            user.write_at(offset_of!(Q, nr), &r.nr)?;
            user.write_at(offset_of!(Q, total), &r.total)
        })
}

/*
 * A file's reflinked data, and its poisoned extents: walks over the file's
 * extents, in its subvolume's snapshot, that update keys and commit each.
 *
 * Each commit has a disk reservation, released before the next key so it
 * can't accumulate across the range: what a commit allocates isn't new
 * data, but the reconcile trigger may pad a key with invalid-device
 * pointers - raising its replica count, or restoring its accounted
 * durability - and charges extra_disk_res for them.
 *
 * A reflink_p's options are its file's, and with may_update_options set
 * they're propagated to the indirect extents it points at: a loop over the
 * reflink btree of its own, run with ignore_restarts() once the reflink_p's
 * key is done.
 */

/// Bring the indirect extents in @range of the reflink btree in line with
/// @opts: their reconcile state, each commit its own.
fn propagate_to_reflink_v(fs: &Fs, trans: &BtreeTrans<'_>, opts: &InodeOpts, range: Range<u64>)
    -> Result<(), BchError>
{
    let res = DiskReservation::new(fs);
    let flags = BtreeIterFlags::INTENT | BtreeIterFlags::NOT_EXTENTS;
    let mut iter = BtreeIter::new(trans, Btree::reflink, bkey::pos(0, range.start), flags);
    iter.for_each_max_commit(trans, bkey::SPOS_MAX, flags, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
        res.put();
        if k.k.start_pos() >= bkey::pos(0, range.end) {
            return Err(Errcode::fc_break.into());
        }
        update_reconcile_opts(t, None, opts, iter, 0, k,
                              SetNeedsReconcileCtx::SET_NEEDS_RECONCILE_opt_change)
    })
}

/// BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS's: set may_update_options on
/// @inum's reflink_ps, an admin's to do, and propagate their options.
pub fn reflink_p_set_may_update_opts(fs: &Fs, inum: SubvolInum) -> Result<(), BchError> {
    if !capable(Capability::SysAdmin) {
        return fs.throw(Errcode::EPERM_non_admin);
    }
    fs.request_incompat_feature(MetadataVersion::reflink_p_may_update_opts)?;

    let trans = btree_trans!(fs);
    let res = DiskReservation::new(fs);
    let flags = BtreeIterFlags::INTENT | BtreeIterFlags::TRIM;
    let mut iter = BtreeIter::new(&trans, Btree::extents, bkey::pos(inum.inum, 0), flags);
    subvolume::for_each_in_subvolume_max_in_trans(&trans, &mut iter, bkey::pos(inum.inum, u64::MAX),
                                                  inum.subvol as u32, flags, |t, iter, k| {
        res.put();
        let Some(p) = k.arm::<ReflinkP>() else { return Ok(()) };
        if p.v().may_update_options() {
            return Ok(());
        }

        // before the commit: it's the key's last use
        let (opts, range) = (get_io_opts(t, None, k)?, p.refd());

        t.bkey_make_mut(iter, p, UpdateTriggerFlags::empty(),
                        |n| Ok(n.v_mut().set_may_update_options(true)))?;
        t.commit(Some(&res), CommitFlags::NO_ENOSPC)?;

        t.ignore_restarts(|trans| propagate_to_reflink_v(fs, trans, &opts, range))
    })
}

/// BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS's: propagate the options of @inum's
/// reflink_ps - all of them, which may update. A reflink_p that may not is
/// skipped for an admin, and an error otherwise.
pub fn reflink_p_propagate_opts(fs: &Fs, inum: SubvolInum) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    let flags = BtreeIterFlags::TRIM;
    let mut iter = BtreeIter::new(&trans, Btree::extents, bkey::pos(inum.inum, 0), flags);
    subvolume::for_each_in_subvolume_max_in_trans(&trans, &mut iter, bkey::pos(inum.inum, u64::MAX),
                                                  inum.subvol as u32, flags, |t, _, k| {
        let Some(p) = k.arm::<ReflinkP>() else { return Ok(()) };
        if !p.v().may_update_options() {
            return if capable(Capability::SysAdmin) {
                Ok(())
            } else {
                fs.throw(Errcode::reflink_p_may_update_options_unset)
            };
        }

        let (opts, range) = (get_io_opts(t, None, k)?, p.refd());
        t.ignore_restarts(|trans| propagate_to_reflink_v(fs, trans, &opts, range))
    })
}

/// Clear @k's poison flag, if it's poisoned: an update at @iter.
fn unpoison_extent(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(k) = k.arm::<PtrsMut>() else { return Ok(()) };
    let flags = k.extent_flags(fs);
    if !flags.contains(ExtentFlags::poisoned) {
        return Ok(());
    }
    fs.request_incompat_feature(MetadataVersion::extent_flags)?;

    // room for a flags entry, if it has none
    let room = (usize::from(k.k.u64s) + size_of::<bch_extent_flags>() / size_of::<u64>()) * size_of::<u64>();
    let mut n = t.bkey_reassemble_room(k, room)?;
    n.extent_flags_set(fs, flags - ExtentFlags::poisoned).expect("room for a flags entry: reassembled with it");
    t.update(iter, n, UpdateTriggerFlags::empty())
}

/// unpoison_extent() each indirect extent reflink_p @p maps.
fn unpoison_reflink(t: &TransAttempt<'_, '_>, p: BkeySC<'_, ReflinkP>) -> Result<(), BchError> {
    let live = p.live();
    let mut iter = BtreeIter::new(t, Btree::reflink, bkey::pos(0, live.start), BtreeIterFlags::INTENT);
    iter.for_each_norestart(t, |iter, k| {
        if k.k.start_pos() >= bkey::pos(0, live.end) {
            return Ok(ControlFlow::Break(()));
        }
        unpoison_extent(t, iter, k)?;
        Ok(ControlFlow::Continue(()))
    })
}

/// BCHFS_IOC_UNPOISON's: clear the poison flag on @inum's extents in a
/// range, and the indirect extents its reflink_ps there map. @owner, whether
/// the caller owns the file - the VFS's to say - is asked once the
/// argument's checked. no_enospc: it's a recovery path, and a full
/// filesystem mustn't be why the owner can't get at data that's still
/// there.
pub fn unpoison(fs: &Fs, inum: SubvolInum, owner: impl FnOnce() -> bool, arg: IoctlUnpoison)
    -> Result<(), BchError>
{
    if arg.flags != 0 || arg.pad != 0 {
        return fs.throw(Errcode::EINVAL_unpoison_args);
    }
    if arg.len == 0 {
        return Ok(());
    }
    if !owner() {
        return fs.throw(Errcode::EPERM_non_admin_or_owner);
    }

    let start = bkey::pos(inum.inum, arg.offset >> 9);
    let end   = bkey::pos(inum.inum, arg.offset.wrapping_add(arg.len) >> 9);

    let trans = btree_trans!(fs);
    let res = DiskReservation::new(fs);
    let mut iter = BtreeIter::new(&trans, Btree::extents, start, BtreeIterFlags::INTENT);
    subvolume::for_each_in_subvolume_max_in_trans(&trans, &mut iter, end, inum.subvol as u32,
                                                  BtreeIterFlags::INTENT, |t, iter, k| {
        res.put();
        match k.arm::<ReflinkP>() {
            Some(p) => unpoison_reflink(t, p)?,
            None    => unpoison_extent(t, iter, k)?,
        }
        t.commit(Some(&res), CommitFlags::NO_ENOSPC)
    })
}
