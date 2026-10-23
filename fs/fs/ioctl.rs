// SPDX-License-Identifier: GPL-2.0

//! A file's own ioctls that need only the filesystem. vfs/ioctl.rs's
//! dispatch answers them here, with what each needs of the file - its inode
//! number, whether it's a directory - and keeps the VFS's checks of
//! ownership and permission, which come in as arguments or a callback.

use core::mem::offset_of;
use core::mem::size_of;
use core::ops::ControlFlow;
use crate::accounting;
use crate::bch_warn;
use crate::btree::bkey;
use crate::btree::iter::{is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, TransAttempt};
use crate::btree_trans;
use crate::c;
use crate::dirent::{self, Dirent};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::init::damage;
use crate::namei;
use crate::snapshots::{snapshot, subvolume};
use crate::util::Printbuf;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::ioctl::{UserArg, UserVstruct};
use crate::util::kernel::{capable, Capability};
use crate::util::os_str::OsStrExt;
use crate::util::vstructs::{plain_bytes, Plain};
use kernel::uaccess::{UserPtr, UserSlice};

/// FS_IOC_GETFSLABEL's: the label, without its NUL - and if it fills its
/// field, without its last byte, as C had it.
pub fn get_label(fs: &Fs, user: UserArg<[u8; c::FSLABEL_MAX as usize]>) -> Result<(), BchError> {
    const LABEL_SIZE: usize = c::BCH_SB_LABEL_SIZE as usize;
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

/// The caller's buffer, @used bytes of it filled; @rec, an entry on its way.
struct Entries {
    buf:  u64,
    size: u32,
    used: u32,
    rec:  KVVec<u8>,
}

impl Entries {
    fn new(buf: u64, size: u32) -> Self {
        Entries { buf, size, used: 0, rec: KVVec::new() }
    }

    /// Append an entry, @name after the header @hdr makes for its reclen, if
    /// it fits. Unlocks @t.
    fn emit<H: Plain>(&mut self, t: &TransAttempt<'_, '_>, name: &[u8], hdr: impl FnOnce(u32) -> H)
        -> Result<bool, BchError>
    {
        let hdr_bytes = size_of::<H>();
        let reclen = (hdr_bytes + name.len() + 1).next_multiple_of(8);
        if self.used as usize + reclen > self.size as usize {
            return Ok(false);
        }

        self.rec.clear();
        self.rec.extend_from_slice(plain_bytes(&hdr(reclen as u32)), GFP_KERNEL)?;
        self.rec.extend_from_slice(name, GFP_KERNEL)?;
        self.rec.extend_from_slice(&[0; 8][..reclen - hdr_bytes - name.len()], GFP_KERNEL)?;

        t.unlock();
        let at = UserPtr::from_addr((self.buf as usize).wrapping_add(self.used as usize));
        UserSlice::new(at, reclen).writer().write_slice(&self.rec)?;
        self.used += reclen as u32;
        Ok(true)
    }
}

/* A file's damage, and readdir_flags */

/// BCHFS_IOC_GET_DAMAGE's: inode @inum's damage, as its subvolume sees it.
/// The count is the true one, however few entries the caller had room for.
pub fn get_damage(fs: &Fs, inum: c::subvol_inum, user: UserVstruct<c::bch_ioctl_get_damage>)
    -> Result<(), BchError>
{
    let mut arg = user.head()?;
    if arg.pad != 0 {
        return Err(c::EINVAL.into());
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
 * into the caller's buffer, as bch_ioctl_readdir_entry records, and returns
 * a cursor, pos, to resume from: a full buffer ends the call, not the
 * listing.
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

const READDIR_FLAGS_ALL: u32 =
    c::BCH_READDIR_recursive | c::BCH_READDIR_damaged | c::BCH_READDIR_subvolumes_only;

/// Whether @flags' filter keeps dirent @d, in @dir - whose snapshot is @view
/// - pointing at @target.
fn readdir_filter(t: &TransAttempt<'_, '_>, flags: u32, dir: c::subvol_inum, view: u32,
                  d: Dirent<'_>, target: c::subvol_inum) -> Result<bool, BchError> {
    match flags & !c::BCH_READDIR_recursive {
        0 => Ok(true),
        c::BCH_READDIR_damaged => {
            // a subvolume's root is in its own snapshot
            let snapshot = if target.subvol != dir.subvol {
                subvolume::get_snapshot(t, target.subvol as u32)?
            } else {
                view
            };
            damage::inode_has_damage(t, target.inum, snapshot)
        }
        c::BCH_READDIR_subvolumes_only => Ok(u32::from(d.d_type()) == c::DT_SUBVOL),
        _ => Err(c::EINVAL.into()),
    }
}

fn readdir_plain(fs: &Fs, dir: c::subvol_inum, flags: u32, out: &mut Entries, pos: u64)
    -> Result<u64, BchError>
{
    let trans = btree_trans!(fs);
    let view = lockrestart_do(&trans, |t| subvolume::get_snapshot(t, dir.subvol as u32))?;

    let mut cursor = pos;
    let mut iter = BtreeIter::new(&trans, c::btree_id::dirents, bkey::pos(dir.inum, pos),
                                  BtreeIterFlags::empty());
    subvolume::for_each_in_subvolume_max_in_trans(&trans, &mut iter, bkey::pos(dir.inum, u64::MAX),
                                                  dir.subvol as u32, BtreeIterFlags::empty(), |t, _, k| {
        let Some(d) = Dirent::new(k) else { return Ok(ControlFlow::Continue(())) };
        let Some(target) = dirent::read_target(t, dir, d)? else { return Ok(ControlFlow::Continue(())) };
        let next = k.k.p.offset + 1;

        if readdir_filter(t, flags, dir, view, d, target)? &&
           !out.emit(t, d.name().as_bytes(), |reclen| c::bch_ioctl_readdir_entry {
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
    dir:        c::subvol_inum,
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
    A: FnMut(&TransAttempt<'_, '_>, c::subvol_inum) -> Result<bool, BchError>,
{
    /// Qualify @n and emit it, named by its path from the directory: not the
    /// directory itself, whose path from itself is empty.
    fn emit(&mut self, t: &TransAttempt<'_, '_>, n: c::subvol_inum, d_type: u8)
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

        /*
         * Skipped, not truncated: the limit of the u16 name length the
         * retired READDIR_FLAGS' entries had - these have none.
         */
        if name.len() + 1 > u16::MAX as usize {
            return Ok(Emitted::Skipped);
        }

        if !(self.accessible)(t, n)? {
            return Ok(Emitted::Skipped);
        }

        let hdr = |reclen| c::bch_ioctl_readdir_entry { reclen, d_type, inum: n.inum, ..Default::default() };
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
        let mut iter = BtreeIter::new(&trans, c::btree_id::damage, cursor,
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each(&trans, |t, _, k| {
            let (p, inum) = (k.k.p, k.k.p.offset);

            if inum != last_handled && snapshot::is_ancestor(t, view, p.snapshot) {
                if k.k.key_type() != c::bch_bkey_type::KEY_TYPE_damage {
                    last_handled = inum;
                } else {
                    let n = c::subvol_inum { subvol: self.dir.subvol, inum };
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
        let mut iter = BtreeIter::new(&trans, c::btree_id::subvolumes, bkey::pos(0, pos),
                                      BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |t, _, k| {
            let subvol = k.k.p.offset;

            if let Some(s) = subvolume::val(k) {
                if s.state_decoded() == Some(c::bch_subvolume_state::SUBVOLUME_STATE_live) {
                    let n = c::subvol_inum { subvol, inum: s.inode.get() };
                    if let Emitted::Full = self.emit(t, n, c::DT_SUBVOL as u8)? {
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
pub fn readdir_flags<A>(fs: &Fs, dir: c::subvol_inum, is_dir: bool, accessible: A,
                        user: UserArg<c::bch_ioctl_readdir_flags>) -> Result<(), BchError>
where
    A: FnMut(&TransAttempt<'_, '_>, c::subvol_inum) -> Result<bool, BchError>,
{
    let arg = user.read()?;
    if arg.pad != 0 || arg.used != 0 || arg.flags & !READDIR_FLAGS_ALL != 0 {
        return Err(c::EINVAL.into());
    }
    if !is_dir {
        return Err(c::ENOTDIR.into());
    }

    type Q = c::bch_ioctl_readdir_flags;
    let mut out = Entries::new(arg.buf, arg.buf_size);

    // only what the call moved: an unrecursive cursor is one word
    if arg.flags & c::BCH_READDIR_recursive == 0 {
        let pos = readdir_plain(fs, dir, arg.flags, &mut out, arg.pos[0])?;
        user.write_at(offset_of!(Q, pos), &pos)?;
        return user.write_at(offset_of!(Q, used), &out.used);
    }

    let mut r = Recursive { dir, dir_path: Printbuf::new(), path: Printbuf::new(), accessible, out };
    let pos = match arg.flags & !c::BCH_READDIR_recursive {
        c::BCH_READDIR_damaged         => r.damaged(fs, arg.pos)?,
        c::BCH_READDIR_subvolumes_only => [r.subvols(fs, arg.pos[0])?, 0],
        // unfiltered: an honest tree walk, not yet
        _ => return Err(c::EOPNOTSUPP.into()),
    };
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
/// file's, by their paths from its root, as bch_ioctl_subvol_dirent entries -
/// those @accessible says the caller may reach. The cursor is a subvolume
/// id; a subvolume whose path can't be found is skipped.
pub fn subvolume_list<A>(fs: &Fs, parent: u32, mut accessible: A,
                         user: UserArg<c::bch_ioctl_subvol_readdir>) -> Result<(), BchError>
where
    A: FnMut(&TransAttempt<'_, '_>, c::subvol_inum, u32) -> Result<bool, BchError>,
{
    type Q = c::bch_ioctl_subvol_readdir;
    let arg = user.read()?;
    if arg.pad != 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_subvol_readdir_pad);
    }

    let mut out = Entries::new(arg.buf, arg.buf_size);
    let mut cursor = arg.pos;
    {
        let trans = btree_trans!(fs);
        let mut path = Printbuf::new();
        let mut iter = BtreeIter::new(&trans, c::btree_id::subvolume_children,
                                      bkey::pos(parent.into(), arg.pos.into()), BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |t, _, k| {
            if k.k.p.inode != u64::from(parent) {
                return Ok(ControlFlow::Break(()));
            }
            let child = k.k.p.offset as u32;
            let s = subvolume::get(t, child, true)?;
            let root = c::subvol_inum { subvol: child.into(), inum: s.inode.get() };

            let found = accessible(t, root, parent)? && {
                path.reset();
                match namei::inum_to_path_in_subvol(t, root, parent, &mut path) {
                    Err(e) if !is_restart(&e) => false,
                    r => { r?; true }
                }
            };

            if found {
                let otime = fs.time_to_timespec(s.otime.lo.get() as i64);
                let hdr = |reclen| c::bch_ioctl_subvol_dirent {
                    reclen,
                    subvolid:        child,
                    flags:           s.flags.get(),
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

    user.write_at(offset_of!(Q, pos), &cursor)?;
    user.write_at(offset_of!(Q, used), &out.used)
}

/// BCH_IOCTL_SUBVOLUME_TO_PATH's: subvolume @subvolid's path from the
/// filesystem's root, with its NUL; ERANGE if the buffer won't hold it.
pub fn subvolume_to_path(fs: &Fs, user: UserArg<c::bch_ioctl_subvol_to_path>) -> Result<(), BchError> {
    let arg = user.read()?;
    if arg.buf_size == 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_subvol_to_path_no_buf);
    }

    let mut path = Printbuf::new();
    {
        let trans = btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            path.reset();
            let s = subvolume::get(t, arg.subvolid, false)?;
            let root = c::subvol_inum { subvol: arg.subvolid.into(), inum: s.inode.get() };
            namei::inum_to_path(t, root, &mut path)
        })?;
    }

    let p = relative(path.as_bytes());
    if p.len() + 1 > arg.buf_size as usize {
        return Err(c::ERANGE.into());
    }
    let mut w = UserSlice::new(UserPtr::from_addr(arg.buf as usize), p.len() + 1).writer();
    w.write_slice(p)?;
    w.write_slice(&[0])?;
    Ok(())
}

/// What a snapshot tree query answers besides its nodes: the tree, and how
/// many nodes it has and were returned.
struct SnapshotTreeReply {
    tree:  c::SnapshotTree,
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
        return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin);
    }

    let (tree_id, tree) = {
        let trans = btree_trans!(fs);
        lockrestart_do(&trans, |t| {
            let tree_id = if tree_id != 0 {
                tree_id
            } else {
                match fs.snapshots.tree(subvolume::get(t, subvol, false)?.snapshot.get()) {
                    0  => return Err(c::ENOENT.into()),
                    id => id,
                }
            };
            Ok((tree_id, snapshot::tree_lookup(t, tree_id)?))
        })?
    };

    let mut r = SnapshotTreeReply { tree, nr: 0, total: 0 };
    {
        let trans = btree_trans!(fs);
        let mut iter = BtreeIter::new(&trans, c::btree_id::snapshots, bkey::POS_MIN,
                                      BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |t, _, k| {
            let Some(s) = snapshot::val(k) else { return Ok(()) };
            if s.tree() != tree_id || s.state_compat() == c::bch_snapshot_state::SNAPSHOT_STATE_deleted {
                return Ok(());
            }

            let id = k.k.p.offset as u32;
            let mut node = c::bch_ioctl_snapshot_node_v2 {
                id,
                parent:   s.parent(),
                children: s.children(),
                subvol:   s.subvol(),
                flags:    s.flags.get(),
                ..Default::default()
            };
            // summed as C did, wrapping: a corrupt counter is a wrong answer, not a panic
            for v in accounting::snapshot_btrees(fs, id) {
                node.nr_keys   = node.nr_keys.wrapping_add(v[0]);
                node.key_bytes = node.key_bytes.wrapping_add(v[1]);
                node.sectors   = node.sectors.wrapping_add(v[2]);
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
        return Err(c::ERANGE.into());
    }
    Ok(())
}

/// BCH_IOCTL_SNAPSHOT_TREE's: see snapshot_tree().
pub fn snapshot_tree_v1(fs: &Fs, subvol: u32, user: UserArg<c::bch_ioctl_snapshot_tree_query>)
    -> Result<(), BchError>
{
    type Q = c::bch_ioctl_snapshot_tree_query;
    const NODE: usize = size_of::<c::bch_ioctl_snapshot_node>();

    let arg = user.read()?;
    if arg.pad != 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_tree_query_pad);
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
pub fn snapshot_tree_v2(fs: &Fs, subvol: u32, user: UserArg<c::bch_ioctl_snapshot_tree_query_v2>)
    -> Result<(), BchError>
{
    type Q = c::bch_ioctl_snapshot_tree_query_v2;
    const NODE: usize = size_of::<c::bch_ioctl_snapshot_node_v2>();

    let arg = user.read()?;
    // a caller that doesn't say its node size can't be given nodes: we'd
    // have to guess its stride
    if arg.node_size == 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_snapshot_tree_query_pad);
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
