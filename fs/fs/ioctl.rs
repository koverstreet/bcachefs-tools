// SPDX-License-Identifier: GPL-2.0

//! A file's own ioctls that need only the filesystem. vfs/ioctl.rs's
//! dispatch answers them here, with what each needs of the file - its inode
//! number, whether it's a directory - and keeps the VFS's checks of
//! ownership and permission, which come in as arguments or a callback.

use core::mem::offset_of;
use core::mem::size_of;
use core::ops::ControlFlow;
use crate::bch_warn;
use crate::btree::bkey;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, TransAttempt};
use crate::btree_trans;
use crate::c;
use crate::dirent::{self, Dirent};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::damage;
use crate::namei;
use crate::snapshots::{snapshot, subvolume};
use crate::util::Printbuf;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::ioctl::{UserArg, UserVstruct};
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
