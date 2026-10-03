// FUSE mount for bcachefs.
//
// Implements the fuser::Filesystem trait over bcachefs's internal btree
// operations, allowing a bcachefs filesystem to be mounted without kernel
// support. Uses the fuser crate (pure Rust FUSE implementation).
//
// Key design notes:
// - Inode numbers: FUSE uses flat u64. bcachefs uses (subvol, inum) pairs.
//   Currently hardcoded to subvolume 1 with root inum 4096 mapped to FUSE
//   ino 1. This is a FUSE protocol limitation — snapshot subvolumes with
//   colliding inode numbers cannot be represented in a single FUSE mount.
// - Daemonization: Must fork() before spawning threads (Linux constraint).
//   bcachefs's shrinker threads and fs_start happen after fork. The daemon
//   lives as long as the mount, not the process that mounted it, so it gets
//   a systemd scope of its own: stopping the caller's scope (xfstests stops
//   one per test) would otherwise kill it and leave a dead mount.
// - I/O alignment: All reads and writes must be block-aligned. Unaligned
//   requests get read-modify-write treatment in the write handler.
// - Inode lifetime: there is no VFS inode cache here, so we keep the part of
//   it that matters - how many references the kernel holds (FUSE's lookup
//   count: one per entry we hand it, dropped by forget). When that reaches
//   zero we do what bch2_evict_inode() does: an inode with no links left is
//   deleted. Unlink only queues an inode for deletion; without this nothing
//   ever deleted it, its space was never freed, and unmount left the
//   filesystem marked clean with deleted inodes outstanding. FUSE doesn't
//   promise a forget for every inode at unmount, so destroy() evicts
//   whatever is still referenced.
// - Unmount: a plain fuse mount never sends FUSE_DESTROY - the daemon only
//   finds out when /dev/fuse goes dead - so umount returns while destroy()
//   and bch2_fs_exit() are still running, and an fsck or remount straight
//   after gets EBUSY. A fuseblk mount (block device source) sends DESTROY
//   and waits for the reply, so umount returns once the filesystem is shut
//   down. fuser only mounts plain fuse, so for block devices we mount
//   fuseblk ourselves and hand fuser the /dev/fuse fd. The kernel claims the
//   source device exclusively for fuseblk, so we open with noexcl and claim
//   the other devices ourselves. Image files can't be fuseblk sources and
//   keep plain fuse, race and all.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bch_bindgen::fs::FsExt;
use bch_bindgen::c;
use bch_bindgen::data::io::block_on;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::{accounting, btree, dirent, namei, str_hash};
use bcachefs_kernel::btree::iter::{CommitFlags, CommitOpts};
use bcachefs_kernel::inode;
use bcachefs_kernel::opt_set;

use crate::util::AlignedBuf;

/// Guard that calls rcu_unregister_thread on drop (i.e. thread exit).
struct RcuGuard;

impl Drop for RcuGuard {
    fn drop(&mut self) {
        eprintln!("fuse worker thread exiting, unregistering RCU");
        unsafe { c::rust_fuse_rcu_unregister() };
    }
}

thread_local! {
    static THREAD_INITIALIZED: Cell<bool> = const { Cell::new(false) };
    // Hold the guard so it lives until the thread exits
    static RCU_GUARD: Cell<Option<RcuGuard>> = const { Cell::new(None) };
}

/// Ensure the current thread has a valid `current` task_struct and
/// is registered with URCU for btree operations.
/// fuser spawns worker threads that don't run the sched_init() constructor,
/// so `current` starts as NULL and RCU isn't set up.
fn ensure_thread_init() {
    THREAD_INITIALIZED.with(|init| {
        if !init.get() {
            unsafe { c::rust_fuse_ensure_current() };
            unsafe { c::rust_fuse_rcu_register() };
            RCU_GUARD.with(|g| g.set(Some(RcuGuard)));
            init.set(true);
            eprintln!("fuse worker thread initialized (current + RCU)");
        }
    });
}

use fuser::{
    Config, FileAttr, FileType, Filesystem, MountOption,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request, TimeOrNow,
    Errno, FileHandle, FopenFlags, Generation,
    INodeNo, OpenFlags, RenameFlags,
    BsdFileFlags, WriteFlags, LockOwner,
};

const TTL: Duration = Duration::MAX;

const BCACHEFS_ROOT_INO: u64 = 4096;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const DT_FIFO: u32 = 1;
const DT_CHR:  u32 = 2;
const DT_DIR:  u32 = 4;
const DT_BLK:  u32 = 6;
const DT_REG:  u32 = 8;
const DT_LNK:  u32 = 10;
const DT_SOCK: u32 = 12;

fn map_root_ino(ino: INodeNo) -> c::subvol_inum {
    let ino: u64 = ino.0;
    c::subvol_inum {
        subvol: 1,
        inum: if ino == 1 { BCACHEFS_ROOT_INO } else { ino },
    }
}

fn unmap_root_ino(ino: u64) -> u64 {
    if ino == BCACHEFS_ROOT_INO { 1 } else { ino }
}

fn mode_to_filetype(mode: u32) -> FileType {
    match rustix::fs::FileType::from_raw_mode(mode) {
        rustix::fs::FileType::RegularFile     => FileType::RegularFile,
        rustix::fs::FileType::Directory       => FileType::Directory,
        rustix::fs::FileType::Symlink         => FileType::Symlink,
        rustix::fs::FileType::BlockDevice     => FileType::BlockDevice,
        rustix::fs::FileType::CharacterDevice => FileType::CharDevice,
        rustix::fs::FileType::Fifo            => FileType::NamedPipe,
        rustix::fs::FileType::Socket          => FileType::Socket,
        _                                     => FileType::RegularFile,
    }
}

fn dtype_to_filetype(dtype: u32) -> FileType {
    match dtype {
        DT_DIR  => FileType::Directory,
        DT_REG  => FileType::RegularFile,
        DT_LNK  => FileType::Symlink,
        DT_BLK  => FileType::BlockDevice,
        DT_CHR  => FileType::CharDevice,
        DT_FIFO => FileType::NamedPipe,
        DT_SOCK => FileType::Socket,
        _       => FileType::RegularFile,
    }
}

/// Bytes the daemonising child sends its parent over the sync pipe.
///
/// The parent cannot see the child's stderr -- daemon mode sends it to
/// /dev/null, deliberately, since 8d2ea5aef1 -- so anything it is to report has
/// to come through here. Reporting every failure as a FUSE problem sends people
/// looking in the wrong place.
const CHILD_OK: u8            = 0;
const CHILD_ERR_FS_START: u8  = 1;
const CHILD_ERR_MOUNT: u8     = 2;
/// Between the two: the filesystem is up but we never reached fuser::mount2.
const CHILD_ERR_SETUP: u8     = 3;

/// Bounded well under a pipe buffer so the child never blocks writing it, even
/// if the parent is slow to read.
const CHILD_MSG_MAX: usize = 512;

fn signal_parent(fd: OwnedFd, byte: u8) {
    let _ = File::from(fd).write_all(&[byte]);
}

/// Report a failure stage and why, in one write.
fn signal_parent_err(fd: OwnedFd, byte: u8, reason: &str) {
    let mut buf = Vec::with_capacity(1 + CHILD_MSG_MAX);
    buf.push(byte);
    // Truncate on a character boundary, keeping whole characters: the parent
    // decodes this as UTF-8.
    let end = reason
        .char_indices()
        .map(|(i, ch)| i + ch.len_utf8())
        .take_while(|&e| e <= CHILD_MSG_MAX)
        .last()
        .unwrap_or(0);
    buf.extend_from_slice(reason[..end].as_bytes());
    let _ = File::from(fd).write_all(&buf);
}

/// Convert a raw C return value (negative bcachefs error code) to a fuser Errno.
/// Walks the bcachefs error hierarchy to the root standard errno.
fn err(ret: i32) -> Errno {
    let e = BchError::from_raw(if ret < 0 { -ret } else { ret });
    Errno::from_i32(e.errno())
}

/// Convert a BchError to a fuser Errno.
fn bch_err(e: &BchError) -> Errno {
    Errno::from_i32(e.errno())
}

/// @req supplies the owner: the caller's fsuid and fsgid, which is what the
/// kernel gives a new inode. bch2_inode_init_late() then applies a setgid
/// parent's group, and the kernel has already applied the umask and stripped
/// setgid where the caller isn't entitled to it.
fn fuse_create_inode(
    fs:    &Fs,
    req:   &Request,
    dir:   c::subvol_inum,
    name:  &[u8],
    mode:  u16,
    rdev:  u64,
) -> Result<c::bch_inode_unpacked, BchError> {
    let qstr = dirent::qstr(name);
    let mut dir_u: c::bch_inode_unpacked = Default::default();
    let mut inode: c::bch_inode_unpacked = Default::default();
    let mut subvol: c::bch_subvolume = Default::default();

    inode::init_early(fs, &mut inode);

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new(),
        |t| {
            namei::create_trans(
                t,
                dir,
                &mut dir_u,
                &mut inode,
                &mut subvol,
                &qstr,
                req.uid(),
                req.gid(),
                mode,
                rdev,
                c::subvol_inum::default(),
                0,
            )
        },
    )?;

    Ok(inode)
}

fn fuse_unlink(fs: &Fs, dir: c::subvol_inum, name: &[u8]) -> Result<(), BchError> {
    let qstr = dirent::qstr(name);
    let mut dir_u: c::bch_inode_unpacked = Default::default();
    let mut inode: c::bch_inode_unpacked = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            namei::unlink_trans(
                t,
                dir,
                &mut dir_u,
                c::subvol_inum::default(),
                &mut inode,
                &qstr,
                false,
            )
        },
    )
}

fn fuse_link(
    fs:        &Fs,
    inum:      c::subvol_inum,
    newparent: c::subvol_inum,
    name:      &[u8],
) -> Result<c::bch_inode_unpacked, BchError> {
    let qstr = dirent::qstr(name);
    let mut dir_u: c::bch_inode_unpacked = Default::default();
    let mut inode: c::bch_inode_unpacked = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new(),
        |t| namei::link_trans(t, newparent, &mut dir_u, inum, &mut inode, &qstr),
    )?;

    Ok(inode)
}

/// Whether @name exists in @dir.
fn dirent_exists(fs: &Fs, dir: c::subvol_inum, name: &[u8]) -> Result<bool, BchError> {
    let qstr = dirent::qstr(name);
    let lookup = inode::find_by_inum(fs, dir)
        .and_then(|dir_u| str_hash::hash_info_init(fs, &dir_u))
        .and_then(|hash_info| dirent::lookup(fs, dir, &hash_info, &qstr));

    match lookup {
        Ok(_) => Ok(true),
        Err(e) if e.matches_errno(libc::ENOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

fn fuse_rename(
    fs:       &Fs,
    src_dir:  c::subvol_inum,
    src_name: &[u8],
    dst_dir:  c::subvol_inum,
    dst_name: &[u8],
    mode:     c::bch_rename_mode,
) -> Result<(), BchError> {
    let src_qstr = dirent::qstr(src_name);
    let dst_qstr = dirent::qstr(dst_name);
    let mut src_dir_u: c::bch_inode_unpacked = Default::default();
    let mut dst_dir_u: c::bch_inode_unpacked = Default::default();
    let mut src_inode_u: c::bch_inode_unpacked = Default::default();
    let mut dst_inode_u: c::bch_inode_unpacked = Default::default();
    let mut src_opt_change: c::inode_opt_change = Default::default();
    let mut dst_opt_change: c::inode_opt_change = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new(),
        |t| {
            namei::rename_trans(
                t,
                src_dir,
                &mut src_dir_u,
                dst_dir,
                &mut dst_dir_u,
                &mut src_inode_u,
                &mut dst_inode_u,
                &src_qstr,
                &dst_qstr,
                mode,
                &mut src_opt_change,
                &mut dst_opt_change,
            )
        },
    )?;

    namei::rename_opt_changes_finish(fs, &mut src_opt_change, &mut dst_opt_change)
}

#[allow(clippy::too_many_arguments)]
fn fuse_setattr(
    fs:         &Fs,
    inum:       c::subvol_inum,
    mode:       Option<u16>,
    uid:        Option<u32>,
    gid:        Option<u32>,
    size:       Option<u64>,
    atime_flag: i32,
    atime:      u64,
    mtime_flag: i32,
    mtime:      u64,
    ctime:      Option<u64>,
) -> Result<c::bch_inode_unpacked, BchError> {
    let mut inode_out: c::bch_inode_unpacked = Default::default();

    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            let now = fs.current_time();
            let mut iter = btree::iter::BtreeIter::uninit();
            let mut inode_u: c::bch_inode_unpacked = Default::default();

            let t = inode::peek(
                t,
                &mut iter,
                &mut inode_u,
                inum,
                btree::iter::BtreeIterFlags::INTENT,
            )?;

            if let Some(mode) = mode {
                inode_u.bi_mode = mode;
            }
            if let Some(uid) = uid {
                inode_u.bi_uid = uid;
            }
            if let Some(gid) = gid {
                inode_u.bi_gid = gid;
            }
            if let Some(size) = size {
                inode_u.bi_size = size;
            }
            if atime_flag == 1 {
                inode_u.bi_atime = atime;
            }
            if atime_flag == 2 {
                inode_u.bi_atime = now;
            }
            if mtime_flag == 1 {
                inode_u.bi_mtime = mtime;
            }
            if mtime_flag == 2 {
                inode_u.bi_mtime = now;
            }
            // Every attribute change is a status change. The kernel only
            // sends a ctime with the writeback cache, so it's ours to set:
            inode_u.bi_ctime = ctime.unwrap_or(now);

            let t = inode::write(t, &mut iter, &mut inode_u)?;
            inode_out = inode_u;
            Ok(t)
        },
    )?;

    Ok(inode_out)
}

/// Shrinking a file: zero the rest of the block the new EOF falls in, then drop
/// everything past it. The kernel's own truncate zeroes that tail in the page
/// cache; without it, truncating down and back up reads the old bytes again
/// (xfstests generic/029). Growing needs only the new i_size, which
/// fuse_setattr() writes.
fn fuse_truncate(fs: &Fs, inum: c::subvol_inum, new_size: u64) -> Result<(), BchError> {
    let bi = inode::find_by_inum(fs, inum)?;
    if new_size >= bi.bi_size {
        return Ok(());
    }

    let block_size = fs.block_bytes();
    let tail = (new_size & (block_size - 1)) as usize;
    if tail != 0 {
        let block_start = new_size - tail as u64;
        let mut buf = AlignedBuf::new(block_size as usize);
        block_on(fs.read(inum, block_start, &bi, &mut buf))?;

        if buf[tail..].iter().any(|&b| b != 0) {
            buf[tail..].fill(0);
            let replicas = std::cmp::max(inode::opts_get_inode(fs, &bi).data_replicas as u32, 1);
            block_on(fs.write(bi.bi_inum, block_start, inum.subvol as u32,
                              replicas, &buf, new_size))?;
        }
    }

    fs.truncate(inum, new_size)
}

/// What bch2_fsync() does once the page cache is written back, which here it
/// already is - writes complete before we reply to them. The kernel flushes
/// the journal only as far as the inode's last update; we don't track that,
/// so we flush all of it: more than the minimum, never less.
fn fuse_fsync(fs: &Fs) -> Result<(), BchError> {
    if unsafe { (*fs.raw).opts.journal_flush_disabled } != 0 {
        return Ok(());
    }
    fs.journal_flush()
}

fn fuse_update_inode_after_write(fs: &Fs, inum: c::subvol_inum) -> Result<(), BchError> {
    btree::iter::trans_commit_do(
        fs,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            let now = fs.current_time();
            let mut iter = btree::iter::BtreeIter::uninit();
            let mut inode_u: c::bch_inode_unpacked = Default::default();

            let t = inode::peek(
                t,
                &mut iter,
                &mut inode_u,
                inum,
                btree::iter::BtreeIterFlags::INTENT,
            )?;
            inode_u.bi_mtime = now;
            inode_u.bi_ctime = now;
            inode::write(t, &mut iter, &mut inode_u)
        },
    )
}

struct BcachefsFs {
    /// Shut down (Fs's Drop: bch2_fs_exit()) exactly once on every path:
    /// destroy() takes it, and if the mount fails before fuser hands us to a
    /// FilesystemHolder - whose Drop calls destroy() - it goes down with us.
    fs: Option<Fs>,
    /// Write end of a pipe used to signal the parent process that the
    /// FUSE mount is established. Written in init(), None in foreground mode.
    signal_fd: Option<OwnedFd>,
    /// Kernel references (FUSE lookup counts) per inum: see "Inode lifetime".
    lookups: Mutex<HashMap<u64, u64>>,
}

// Safety: bch_fs is internally synchronized with its own locking.
unsafe impl Send for BcachefsFs {}
unsafe impl Sync for BcachefsFs {}

impl BcachefsFs {
    fn fs(&self) -> &Fs {
        self.fs.as_ref().expect("fuse request after destroy()")
    }

    /// The kernel now holds a reference: every reply that hands it an entry.
    /// Taken before the reply goes out, so a forget can't arrive first.
    fn inode_get(&self, inum: u64) {
        *self.lookups.lock().unwrap().entry(inum).or_insert(0) += 1;
    }

    /// The kernel dropped @nlookup references; evict at zero.
    fn inode_put(&self, inum: u64, nlookup: u64) {
        let unreferenced = {
            let mut lookups = self.lookups.lock().unwrap();
            match lookups.get_mut(&inum) {
                Some(n) => {
                    *n = n.saturating_sub(nlookup);
                    *n == 0 && lookups.remove(&inum).is_some()
                }
                None => false,
            }
        };

        if unreferenced {
            self.inode_evict(inum);
        }
    }

    /// bch2_evict_inode(): nothing references the inode any more, so if it
    /// has no links left, delete it. A subvolume root with no links is the
    /// subvolume deletion path's to delete, not ours.
    fn inode_evict(&self, inum: u64) {
        let fs = self.fs();
        let inum = c::subvol_inum { subvol: 1, inum };
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => {
                eprintln!("bcachefs fuse: evicting inode {}: lookup error {}", inum.inum, e);
                return;
            }
        };

        if Fs::inode_nlink_get(&bi) != 0 || inode::is_subvolume_root(&bi) {
            return;
        }

        if let Err(e) = inode::rm(&fs, inum) {
            eprintln!("bcachefs fuse: deleting unlinked inode {}: {}", inum.inum, e);
        }
    }

    fn inode_to_attr(&self, bi: &c::bch_inode_unpacked) -> FileAttr {
        let fs = self.fs();
        let ts_a = fs.time_to_timespec(bi.bi_atime as i64);
        let ts_m = fs.time_to_timespec(bi.bi_mtime as i64);
        let ts_c = fs.time_to_timespec(bi.bi_ctime as i64);
        let blksize = fs.block_bytes() as u32;
        let nlink = Fs::inode_nlink_get(bi);

        FileAttr {
            ino: INodeNo(unmap_root_ino(bi.bi_inum)),
            size: bi.bi_size,
            blocks: bi.bi_sectors,
            atime: ts_to_systime(ts_a),
            mtime: ts_to_systime(ts_m),
            ctime: ts_to_systime(ts_c),
            crtime: UNIX_EPOCH,
            kind: mode_to_filetype(bi.bi_mode as u32),
            perm: (bi.bi_mode & 0o7777),
            nlink,
            uid: bi.bi_uid,
            gid: bi.bi_gid,
            rdev: bi.bi_dev,
            blksize,
            flags: 0,
        }
    }
}

/// Times before 1970 are valid on bcachefs: a timespec's tv_sec is then
/// negative, with tv_nsec still counting forwards from it.
fn ts_to_systime(ts: c::timespec) -> SystemTime {
    let nsec = Duration::from_nanos(ts.tv_nsec as u64);
    if ts.tv_sec >= 0 {
        UNIX_EPOCH + Duration::from_secs(ts.tv_sec as u64) + nsec
    } else {
        UNIX_EPOCH - Duration::from_secs(ts.tv_sec.unsigned_abs()) + nsec
    }
}

fn systime_to_ts(t: SystemTime) -> c::timespec {
    let (sec, nsec) = match t.duration_since(UNIX_EPOCH) {
        Ok(d)  => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            match d.subsec_nanos() {
                0 => (-(d.as_secs() as i64), 0),
                n => (-(d.as_secs() as i64) - 1, 1_000_000_000 - n),
            }
        }
    };
    c::timespec { tv_sec: sec as _, tv_nsec: nsec as _ }
}

impl Filesystem for BcachefsFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        eprintln!("bcachefs fuse: init callback fired");

        // File handles (NFS export, open_by_handle_at()) outlive the kernel's
        // inode cache. Without this the kernel can only resolve a handle
        // whose inode is still cached, and returns ESTALE otherwise; with it,
        // it asks us - a lookup of "." or ".." in the handle's inode, see
        // lookup().
        if let Err(unsupported) = config.add_capabilities(fuser::InitFlags::FUSE_EXPORT_SUPPORT) {
            eprintln!("bcachefs fuse: kernel lacks {unsupported:?}: file handles go stale \
                       once their inode leaves the cache");
        }
        // Signal parent that mount is established
        if let Some(fd) = self.signal_fd.take() {
            eprintln!("bcachefs fuse: signaling parent");
            signal_parent(fd, CHILD_OK);
        }
        eprintln!("bcachefs fuse: init returning Ok");
        Ok(())
    }

    fn destroy(&mut self) {
        eprintln!("bcachefs fuse: destroy");
        ensure_thread_init();

        let referenced: Vec<u64> = self.lookups.lock().unwrap().drain().map(|(inum, _)| inum).collect();
        for inum in referenced {
            self.inode_evict(inum);
        }

        self.fs = None;
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        ensure_thread_init();
        self.inode_put(map_root_ino(ino).inum, nlookup);
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_lookup(dir={}, name={:?})", dir.inum, name);

        let fs = self.fs();
        let qstr = dirent::qstr(name_bytes);
        let lookup = match name_bytes {
            // Only sent to resolve a file handle (FUSE_EXPORT_SUPPORT, see
            // init()); there are no dirents for them. The parent is
            // bch2_get_parent()'s, less the subvolume - we serve only one.
            b"." => inode::find_by_inum(&fs, dir).map(|bi| (dir, bi)),
            b".." => inode::find_by_inum(&fs, dir).and_then(|dir_u| {
                let parent = c::subvol_inum {
                    subvol: dir.subvol,
                    inum:   if dir.inum == BCACHEFS_ROOT_INO { dir.inum } else { dir_u.bi_dir },
                };
                inode::find_by_inum(&fs, parent).map(|bi| (parent, bi))
            }),
            _ => inode::find_by_inum(&fs, dir)
                .and_then(|dir_u| str_hash::hash_info_init(&fs, &dir_u))
                .and_then(|hash_info| dirent::lookup(&fs, dir, &hash_info, &qstr))
                .and_then(|inum| inode::find_by_inum(&fs, inum).map(|bi| (inum, bi))),
        };

        let (inum, bi) = match lookup {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  lookup -> err {}", e);
                // Negative dentry caching: return empty entry for ENOENT
                if e.matches_errno(libc::ENOENT) {
                    let attr = FileAttr {
                        ino: INodeNo(0),
                        size: 0, blocks: 0,
                        atime: UNIX_EPOCH, mtime: UNIX_EPOCH,
                        ctime: UNIX_EPOCH, crtime: UNIX_EPOCH,
                        kind: FileType::RegularFile, perm: 0,
                        nlink: 0, uid: 0, gid: 0, rdev: 0,
                        blksize: 0, flags: 0,
                    };
                    reply.entry(&TTL, &attr, Generation(0));
                    return;
                }
                reply.error(bch_err(&e));
                return;
            }
        };

        eprintln!("  lookup -> ok inum={}", inum.inum);
        let attr = self.inode_to_attr(&bi);
        self.inode_get(inum.inum);
        reply.entry(&TTL, &attr, Generation(bi.bi_generation as u64));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_getattr(inum={})", inum.inum);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => {
                eprintln!("  getattr -> err {}", e.raw());
                reply.error(bch_err(&e));
                return;
            }
        };

        eprintln!("  getattr -> ok");
        reply.attr(&TTL, &self.inode_to_attr(&bi));
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_setattr(inum={})", inum.inum);

        let fs = self.fs();

        let parse_time = |time: &Option<TimeOrNow>| match time {
            None => (0, 0),
            Some(TimeOrNow::Now) => (2, 0),
            Some(TimeOrNow::SpecificTime(t)) =>
                (1, fs.timespec_to_time(systime_to_ts(*t)) as u64),
        };

        let (atime_flag, atime_val) = parse_time(&atime);
        let (mut mtime_flag, mtime_val) = parse_time(&mtime);
        let ctime = ctime.map(|t| fs.timespec_to_time(systime_to_ts(t)) as u64);

        if let Some(size) = size {
            // A size change modifies the file: truncate(2) sends only the
            // size, and leaves mtime to us as it does to a kernel filesystem.
            // Asked before truncating, which already changes it.
            let size_changed = match inode::find_by_inum(&fs, inum) {
                Ok(bi) => bi.bi_size != size,
                Err(e) => { reply.error(bch_err(&e)); return; }
            };
            if size_changed && mtime_flag == 0 {
                mtime_flag = 2;
            }

            if let Err(e) = fuse_truncate(&fs, inum, size) {
                reply.error(bch_err(&e));
                return;
            }
        }

        let bi = match fuse_setattr(
            &fs,
            inum,
            mode.map(|mode| mode as u16),
            uid,
            gid,
            size,
            atime_flag,
            atime_val,
            mtime_flag,
            mtime_val,
            ctime,
        ) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        reply.attr(&TTL, &self.inode_to_attr(&bi));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_readlink(inum={})", inum.inum);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let size = bi.bi_size as usize;
        let block_size = fs.block_bytes() as usize;
        let aligned_size = (size + block_size - 1) & !(block_size - 1);

        let mut buf = AlignedBuf::new(aligned_size);

        if let Err(e) = block_on(fs.read(inum, 0, &bi, &mut buf)) {
            reply.error(bch_err(&e));
            return;
        }

        let end = buf[..size].iter().position(|&b| b == 0).unwrap_or(size);
        reply.data(&buf[..end]);
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_mknod(dir={}, name={:?}, mode={:#o})", dir.inum, name, mode);

        let fs = self.fs();
        let new_inode = match fuse_create_inode(&fs, req, dir, name_bytes, mode as u16, rdev as u64) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        let attr = self.inode_to_attr(&new_inode);
        self.inode_get(new_inode.bi_inum);
        reply.entry(&TTL, &attr, Generation(new_inode.bi_generation as u64));
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        eprintln!("fuse_mkdir(dir={}, name={:?})", parent.0, name);
        self.mknod(req, parent, name, mode | S_IFDIR, umask, 0, reply);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_unlink(dir={}, name={:?})", dir.inum, name);

        let fs = self.fs();
        match fuse_unlink(&fs, dir, name_bytes) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("fuse_rmdir(dir={}, name={:?})", parent.0, name);
        self.unlink(req, parent, name, reply);
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        let link_bytes = link.as_os_str().as_bytes();
        eprintln!("fuse_symlink(dir={}, name={:?}, link={:?})", dir.inum, name, link);

        // Create the symlink inode
        let fs = self.fs();
        let new_inode = match fuse_create_inode(&fs, req, dir, name_bytes, (S_IFLNK | 0o777) as u16, 0) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        // Write link target, NUL terminated but with i_size excluding the NUL,
        // as the kernel's page_symlink() does: stat reports the target's length.
        let block_size = fs.block_bytes();
        let link_with_nul_len = link_bytes.len() + 1;
        let padded = (link_with_nul_len as u64).div_ceil(block_size) * block_size;

        let mut buf = AlignedBuf::new(padded as usize);
        buf[..link_bytes.len()].copy_from_slice(link_bytes);
        // buf is zero-initialized, so NUL terminator and padding are already 0

        let sym_inum = c::subvol_inum { subvol: dir.subvol, inum: new_inode.bi_inum };
        if let Err(e) = block_on(fs.write(new_inode.bi_inum, 0, dir.subvol as u32,
                                          1, &buf, link_bytes.len() as u64)) {
            reply.error(bch_err(&e));
            return;
        }

        // Re-read inode to get updated state
        let fs = self.fs();
        let new_inode = match inode::find_by_inum(&fs, sym_inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let attr = self.inode_to_attr(&new_inode);
        self.inode_get(new_inode.bi_inum);
        reply.entry(&TTL, &attr, Generation(new_inode.bi_generation as u64));
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        ensure_thread_init();
        let src_dir = map_root_ino(parent);
        let dst_dir = map_root_ino(newparent);
        let src_bytes = name.as_bytes();
        let dst_bytes = newname.as_bytes();
        eprintln!("fuse_rename(src_dir={}, {:?} -> dst_dir={}, {:?}, flags={})",
               src_dir.inum, name, dst_dir.inum, newname, flags);

        if flags.contains(RenameFlags::RENAME_WHITEOUT) {
            reply.error(Errno::EINVAL);
            return;
        }

        let fs = self.fs();

        // The mode, chosen as bch2_rename2() chooses it. BCH_RENAME means
        // "there is no target" and isn't checked: renaming onto an existing
        // name with it inserts a second dirent of that name. The kernel holds
        // both directories locked for the whole request, so the target can't
        // appear or vanish between this lookup and the rename.
        let dst_exists = match dirent_exists(&fs, dst_dir, dst_bytes) {
            Ok(v)  => v,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let mode = if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            if !dst_exists {
                reply.error(Errno::ENOENT);
                return;
            }
            c::bch_rename_mode::BCH_RENAME_EXCHANGE
        } else if dst_exists {
            if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                reply.error(Errno::EEXIST);
                return;
            }
            c::bch_rename_mode::BCH_RENAME_OVERWRITE
        } else {
            c::bch_rename_mode::BCH_RENAME
        };

        match fuse_rename(&fs, src_dir, src_bytes, dst_dir, dst_bytes, mode) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        ensure_thread_init();
        let src_inum = map_root_ino(ino);
        let parent = map_root_ino(newparent);
        let name_bytes = newname.as_bytes();
        eprintln!("fuse_link(ino={}, newparent={}, name={:?})",
               src_inum.inum, parent.inum, newname);

        let fs = self.fs();
        let inode_u = match fuse_link(&fs, src_inum, parent, name_bytes) {
            Ok(inode) => inode,
            Err(e)    => { reply.error(bch_err(&e)); return; }
        };

        let attr = self.inode_to_attr(&inode_u);
        self.inode_get(inode_u.bi_inum);
        reply.entry(&TTL, &attr, Generation(inode_u.bi_generation as u64));
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        eprintln!("fuse_open(ino={})", ino.0);
        reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        let size = size as usize;
        eprintln!("fuse_read(ino={}, offset={}, size={})", inum.inum, offset, size);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let end = std::cmp::min(bi.bi_size, offset + size as u64);
        if end <= offset {
            reply.data(&[]);
            return;
        }
        let read_size = (end - offset) as usize;

        let block_size = fs.block_bytes();
        let aligned_start = offset & !(block_size - 1);
        let pad_start = (offset - aligned_start) as usize;
        let aligned_end = (offset + read_size as u64).div_ceil(block_size) * block_size;
        let aligned_size = (aligned_end - aligned_start) as usize;

        let mut buf = AlignedBuf::new(aligned_size);

        if let Err(e) = block_on(fs.read(inum, aligned_start, &bi, &mut buf)) {
            reply.error(bch_err(&e));
            return;
        }

        reply.data(&buf[pad_start..pad_start + read_size]);
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        let size = data.len();
        eprintln!("fuse_write(ino={}, offset={}, size={})", inum.inum, offset, size);

        let fs = self.fs();
        let bi = match inode::find_by_inum(&fs, inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let block_size = fs.block_bytes();

        // Compute alignment
        let aligned_start = offset & !(block_size - 1);
        let pad_start = (offset - aligned_start) as usize;
        let aligned_end = (offset + size as u64).div_ceil(block_size) * block_size;
        let aligned_size = (aligned_end - aligned_start) as usize;

        let mut buf = AlignedBuf::new(aligned_size);

        // RMW: read partial start block
        if pad_start > 0 {
            let mut start_block = AlignedBuf::new(block_size as usize);
            if let Err(e) = block_on(fs.read(inum, aligned_start, &bi, &mut start_block)) {
                reply.error(bch_err(&e));
                return;
            }
            buf[..block_size as usize].copy_from_slice(&start_block);
        }

        // RMW: read partial end block (if different from start)
        let pad_end = (aligned_end - offset - size as u64) as usize;
        if pad_end > 0 && !(pad_start > 0 && aligned_size == block_size as usize) {
            let end_block_offset = aligned_end - block_size;
            let buf_offset = aligned_size - block_size as usize;
            let mut end_block = AlignedBuf::new(block_size as usize);
            if let Err(e) = block_on(fs.read(inum, end_block_offset, &bi, &mut end_block)) {
                reply.error(bch_err(&e));
                return;
            }
            buf[buf_offset..].copy_from_slice(&end_block);
        }

        // Overlay user data
        buf[pad_start..pad_start + size].copy_from_slice(data);

        // Get inode opts for replicas
        let opts = inode::opts_get_inode(&fs, &bi);
        let replicas = std::cmp::max(opts.data_replicas as u32, 1);

        // Write aligned buffer. new_i_size is the file's size after the write,
        // not where this write ends: bch2_write() drops whatever of the buffer
        // lies past it, and a write into the middle of the file would lose the
        // rest of its last block (stored as a short inline extent).
        let new_i_size = std::cmp::max(bi.bi_size, offset + size as u64);
        if let Err(e) = block_on(fs.write(bi.bi_inum, aligned_start, inum.subvol as u32,
                                          replicas, &buf, new_i_size)) {
            reply.error(bch_err(&e));
            return;
        }

        // Update inode times
        if let Err(e) = fuse_update_inode_after_write(&fs, inum) {
            reply.error(bch_err(&e));
            return;
        }

        reply.written(size as u32);
    }

    // Unimplemented, these answer ENOSYS - which the kernel takes to mean the
    // filesystem needs no fsync: it stops asking, and every fsync after that
    // returns success with nothing made durable (xfstests generic/034, files
    // missing after a simulated power failure).
    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool,
             reply: ReplyEmpty) {
        ensure_thread_init();
        eprintln!("fuse_fsync(ino={})", map_root_ino(ino).inum);
        match fuse_fsync(&self.fs()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn fsyncdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool,
                reply: ReplyEmpty) {
        ensure_thread_init();
        eprintln!("fuse_fsyncdir(ino={})", map_root_ino(ino).inum);
        match fuse_fsync(&self.fs()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(bch_err(&e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(ino);
        eprintln!("fuse_readdir(dir={}, offset={})", dir.inum, offset);

        let mut pos = offset;

        // Handle . and ..
        if pos == 0 {
            if reply.add(INodeNo(unmap_root_ino(dir.inum)), 1, FileType::Directory, ".") {
                reply.ok();
                return;
            }
            pos = 1;
        }
        if pos == 1 {
            if reply.add(INodeNo(1), 2, FileType::Directory, "..") {
                reply.ok();
                return;
            }
            pos = 2;
        }

        // Read remaining entries via C shim with callback
        unsafe extern "C" fn filldir(
            ctx: *mut std::ffi::c_void,
            name: *const std::ffi::c_char,
            name_len: std::ffi::c_uint,
            ino: u64,
            dtype: std::ffi::c_uint,
            pos: u64,
        ) -> std::ffi::c_int {
            let reply = unsafe { &mut *(ctx as *mut ReplyDirectory) };
            let name_bytes = unsafe {
                std::slice::from_raw_parts(name as *const u8, name_len as usize)
            };
            let name_str = OsStr::from_bytes(name_bytes);
            let file_type = dtype_to_filetype(dtype);
            let full = reply.add(INodeNo(unmap_root_ino(ino)), pos, file_type, name_str);
            if full { -1 } else { 0 }
        }

        let ret = unsafe {
            c::rust_fuse_readdir(
                self.fs().raw, dir, pos,
                &mut reply as *mut ReplyDirectory as *mut _,
                Some(filldir),
            )
        };

        if ret != 0 {
            reply.error(err(ret));
        } else {
            reply.ok();
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        ensure_thread_init();
        eprintln!("fuse_statfs");

        let fs = self.fs();
        let usage = fs.usage_read_short();
        let block_size = fs.block_bytes();
        // usage is in 512 byte sectors - as bch2_statfs():
        let shift = block_size.trailing_zeros() as u64 - 9;

        let nr_inodes = accounting::nr_inodes(&fs);

        reply.statfs(
            usage.capacity >> shift,
            usage.free >> shift,
            (usage.capacity - usage.used) >> shift,
            nr_inodes,
            u64::MAX,
            block_size as u32,
            255,
            block_size as u32,
        );
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_create(dir={}, name={:?}, mode={:#o})", dir.inum, name, mode);

        let fs = self.fs();
        let new_inode = match fuse_create_inode(&fs, req, dir, name_bytes, mode as u16, 0) {
            Ok(inode) => inode,
            Err(e)    => {
                eprintln!("  create -> err {}", e);
                reply.error(bch_err(&e));
                return;
            }
        };

        eprintln!("  create -> ok inum={}", new_inode.bi_inum);
        let attr = self.inode_to_attr(&new_inode);
        self.inode_get(new_inode.bi_inum);
        reply.created(
            &TTL, &attr,
            Generation(new_inode.bi_generation as u64),
            FileHandle(0),
            FopenFlags::FOPEN_KEEP_CACHE,
        );
    }
}

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "fusemount")]
pub struct Cli {
    /// Mount options (-o key=value,...)
    #[arg(short = 'o')]
    pub options: Option<String>,

    /// Run in foreground
    #[arg(short = 'f')]
    pub foreground: bool,

    /// Device(s) to mount (dev1:dev2:...)
    pub device: String,

    /// Mountpoint
    pub mountpoint: String,
}

fn parse_fuse_mount_options(
    device: &str,
    options: Option<&str>,
) -> anyhow::Result<(c::bch_opts, Vec<MountOption>, libc::c_ulong)> {
    let mut mount_options = vec![
        MountOption::FSName(device.to_string()),
        // Use CUSTOM instead of Subtype — fuser categorizes Subtype as
        // "Fusermount" group, which is only passed when using the fusermount3
        // helper. With a direct mount syscall (as root), Subtype gets
        // silently dropped and the mount shows as "fuse" instead of
        // "fuse.bcachefs" in /proc/mounts.
        MountOption::CUSTOM("subtype=bcachefs".to_string()),
        // Paired with allow_other (SessionACL::All, see cmd_fusemount()):
        // other users get in, and the kernel checks their access against the
        // mode bits we report, as for any filesystem. We do no checking of our
        // own, so allow_other without this would let anyone do anything.
        MountOption::DefaultPermissions,
    ];
    let parsed = options
        .map(super::mount::parse_mountflag_options)
        .unwrap_or_default();
    let mut bch_opts = bcachefs_kernel::opts::parse_mount_opts(None, parsed.fs_opts.as_deref(), true)?;

    opt_set!(bch_opts, nostart, 1);

    // read-only is both a fuser option (carried in parsed.fuse_options) and a
    // filesystem-open concern - the latter isn't a mount flag, so set it here:
    if parsed.flags & libc::MS_RDONLY != 0 {
        opt_set!(bch_opts, read_only, 1);
    }

    mount_options.extend(parsed.fuse_options);

    // fuser defaults to nodev,nosuid, as fusermount does for an unprivileged
    // user mounting something untrusted. This is root mounting a block device,
    // as trusted as a kernel mount - so like mount(8), only what -o asks for:
    if !mount_options.contains(&MountOption::NoDev) {
        mount_options.push(MountOption::Dev);
    }
    if !mount_options.contains(&MountOption::NoSuid) {
        mount_options.push(MountOption::Suid);
    }

    Ok((bch_opts, mount_options, parsed.flags))
}

/// Move the daemon into a transient systemd scope of its own.
///
/// Why: the daemon has to live as long as the mount, but it starts life in the
/// cgroup of whoever ran mount(8), and systemd kills every process in a unit
/// when that unit stops - a login session ending, a service that mounted
/// something restarting, or xfstests, which runs each test in a scope and
/// stops it afterwards. The daemon dies, and what's left is worse than an
/// unmount: the mount stays, every access returns ENOTCONN, and the kernel
/// still holds the device, so fsck and a fresh mount get EBUSY until someone
/// unmounts the corpse.
///
/// How: the D-Bus call `systemd-run --scope` makes, with the daemon's pid.
/// Without systemd there is no scope to leave. Failing isn't fatal to the
/// mount - the daemon then lives and dies with its caller's scope, as before -
/// so it's a warning.
fn move_to_own_scope(pid: libc::pid_t, device: &str, mountpoint: &str) {
    if !Path::new("/run/systemd/system").exists() {
        return;
    }

    if let Err(e) = start_transient_scope(pid, device, mountpoint) {
        eprintln!("warning: couldn't give the fuse daemon a systemd scope of its own ({e}); \
                   it will exit with whatever scope ran the mount");
    }
}

/// StartTransientUnit through sd-bus, dlopen()ed: libsystemd is there wherever
/// systemd is, and this is the only thing we'd link it for.
fn start_transient_scope(pid: libc::pid_t, device: &str, mountpoint: &str) -> Result<(), String> {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};

    #[repr(C)]
    struct SdBusError {
        name:       *const c_char,
        message:    *const c_char,
        need_free:  c_int,
    }

    type BusNew         = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
    type BusSetAddress  = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
    type BusSetClient   = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type BusStart       = unsafe extern "C" fn(*mut c_void) -> c_int;
    type BusUnref       = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type BusErrorFree   = unsafe extern "C" fn(*mut SdBusError);
    type BusCallMethod  = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char,
                                               *const c_char, *const c_char, *mut SdBusError,
                                               *mut *mut c_void, *const c_char, ...) -> c_int;

    let lib = unsafe { libc::dlopen(c"libsystemd.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if lib.is_null() {
        return Err("libsystemd.so.0 not found".into());
    }

    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            let p = unsafe { libc::dlsym(lib, $name.as_ptr()) };
            if p.is_null() {
                return Err(format!("libsystemd has no {}", $name.to_string_lossy()));
            }
            unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
        }};
    }

    let bus_new          = sym!(c"sd_bus_new", BusNew);
    let bus_open_system  = sym!(c"sd_bus_open_system", BusNew);
    let bus_set_address  = sym!(c"sd_bus_set_address", BusSetAddress);
    let bus_set_client   = sym!(c"sd_bus_set_bus_client", BusSetClient);
    let bus_start        = sym!(c"sd_bus_start", BusStart);
    let bus_unref        = sym!(c"sd_bus_flush_close_unref", BusUnref);
    let bus_error_free   = sym!(c"sd_bus_error_free", BusErrorFree);
    let bus_call_method  = sym!(c"sd_bus_call_method", BusCallMethod);

    let errno = |what: &str, ret: c_int| format!("{what}: {}", std::io::Error::from_raw_os_error(-ret));

    // As root, talk to systemd directly, as systemctl and systemd-run do:
    // there may be no D-Bus daemon (minimal systems, test VMs).
    let mut bus: *mut c_void = std::ptr::null_mut();
    let ret = if Path::new("/run/systemd/private").exists() {
        let ret = unsafe { bus_new(&mut bus) };
        if ret < 0 {
            return Err(errno("sd_bus_new", ret));
        }
        unsafe {
            let ret = bus_set_address(bus, c"unix:path=/run/systemd/private".as_ptr());
            if ret < 0 { ret } else {
                let ret = bus_set_client(bus, 0);
                if ret < 0 { ret } else { bus_start(bus) }
            }
        }
    } else {
        unsafe { bus_open_system(&mut bus) }
    };
    if ret < 0 {
        unsafe { bus_unref(bus) };
        return Err(errno("connecting to systemd", ret));
    }

    let unit = CString::new(format!("bcachefs-fuse-{pid}.scope")).unwrap();
    let description = CString::new(format!("bcachefs fuse daemon: {device} on {mountpoint}"))
        .unwrap_or_default();
    let mut error = SdBusError {
        name:       std::ptr::null(),
        message:    std::ptr::null(),
        need_free:  0,
    };

    // Array lengths are ints to sd_bus_message_append(), as in systemd-run:
    let ret = unsafe {
        bus_call_method(bus,
            c"org.freedesktop.systemd1".as_ptr(),
            c"/org/freedesktop/systemd1".as_ptr(),
            c"org.freedesktop.systemd1.Manager".as_ptr(),
            c"StartTransientUnit".as_ptr(),
            &mut error, std::ptr::null_mut(),
            c"ssa(sv)a(sa(sv))".as_ptr(),
            unit.as_ptr(), c"fail".as_ptr(),
            3 as c_int,
            c"PIDs".as_ptr(), c"au".as_ptr(), 1 as c_int, pid as u32,
            c"Description".as_ptr(), c"s".as_ptr(), description.as_ptr(),
            c"CollectMode".as_ptr(), c"s".as_ptr(), c"inactive-or-failed".as_ptr(),
            0 as c_int)
    };

    let result = if ret >= 0 {
        Ok(())
    } else if !error.message.is_null() {
        Err(unsafe { CStr::from_ptr(error.message) }.to_string_lossy().into_owned())
    } else {
        Err(errno("StartTransientUnit", ret))
    };

    unsafe {
        bus_error_free(&mut error);
        bus_unref(bus);
    }
    result
}

/// Run @f with stderr going to a scratch file, shown only if @f fails.
///
/// A mount helper prints nothing on success - xfstests counts any output as a
/// failure. But opening the filesystem in-process logs to stderr ("starting
/// version", options, devices), where the kernel would log to dmesg, and if
/// the open fails that log is what says why.
fn stderr_unless_error<T, E>(f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    use rustix::fs::{memfd_create, MemfdFlags};
    use rustix::stdio::dup2_stderr;
    use std::io::Seek;
    use std::os::fd::AsFd;

    // Can't capture: printing the log is better than losing the mount
    let Ok((log, saved)) = memfd_create(c"fusemount-log", MemfdFlags::CLOEXEC)
        .and_then(|log| Ok((log, rustix::io::dup(std::io::stderr().as_fd())?)))
    else {
        return f();
    };

    let _ = dup2_stderr(&log);
    let ret = f();
    let _ = dup2_stderr(&saved);

    if ret.is_err() {
        let mut log = File::from(log);
        let _ = log.seek(std::io::SeekFrom::Start(0))
            .and_then(|_| std::io::copy(&mut log, &mut std::io::stderr()));
    }
    ret
}

/// How the FUSE mount is made: see "Unmount" in the notes at the top.
enum FuseMount {
    /// Plain fuse via fuser::mount2(), for image files.
    Fuse,
    /// fuseblk, mounted by us, for block devices.
    Fuseblk {
        source:   PathBuf,
        ms_flags: libc::c_ulong,
    },
}

/// Mount fuseblk on @mountpoint and return the /dev/fuse fd to serve it on.
///
/// The option string is what fuser builds for plain fuse, plus blksize, so the
/// two kinds of mount behave the same apart from unmount. That includes
/// allow_other with default_permissions, and nodev/nosuid only when asked for
/// - see parse_fuse_mount_options().
fn mount_fuseblk(
    source:     &Path,
    mountpoint: &str,
    ms_flags:   libc::c_ulong,
    blksize:    u32,
) -> std::io::Result<OwnedFd> {
    let dev_fuse = OpenOptions::new().read(true).write(true).open("/dev/fuse")?;
    let rootmode = std::fs::metadata(mountpoint)?.mode() & libc::S_IFMT;
    let data = format!(
        "fd={},rootmode={:o},user_id={},group_id={},blksize={},subtype=bcachefs,\
         allow_other,default_permissions",
        dev_fuse.as_raw_fd(), rootmode,
        rustix::process::getuid().as_raw(), rustix::process::getgid().as_raw(),
        blksize,
    );

    use rustix::mount::MountFlags;
    let flags = MountFlags::from_bits_retain(ms_flags as _);
    let data = std::ffi::CString::new(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    rustix::mount::mount(source, mountpoint, "fuseblk", flags, Some(data.as_c_str()))?;

    Ok(dev_fuse.into())
}

/// Mount and serve until unmounted. Either way @bcachefs_fs - and with it the
/// filesystem - is gone by the time this returns.
fn fuse_run(
    bcachefs_fs: BcachefsFs,
    mountpoint:  &str,
    config:      &Config,
    mount:       &FuseMount,
) -> std::io::Result<()> {
    let FuseMount::Fuseblk { source, ms_flags } = mount else {
        return fuser::mount2(bcachefs_fs, mountpoint, config);
    };

    // fuseblk wants a power of two from 512 to the page size:
    let blksize = (bcachefs_fs.fs().block_bytes() as usize)
        .clamp(512, rustix::param::page_size()) as u32;

    let fd = mount_fuseblk(source, mountpoint, *ms_flags, blksize)?;

    // Session::from_fd() reads FUSE_INIT, which the kernel only sends once
    // the mount exists - hence mount first. From here a failure leaves a
    // mount with nothing serving it; detach it rather than leave it hanging.
    fuser::Session::from_fd(bcachefs_fs, fd, config.acl, config.clone())
        .and_then(|se| se.spawn())
        .and_then(|bg| bg.join())
        .inspect_err(|_| {
            let _ = rustix::mount::unmount(mountpoint, rustix::mount::UnmountFlags::DETACH);
        })
}

/// Claim the devices the kernel doesn't (everything but the fuseblk source)
/// exclusively, for the life of the mount: we opened them noexcl so the
/// kernel's claim on the source doesn't collide with ours.
fn claim_devices(devs: &[PathBuf]) -> anyhow::Result<Vec<File>> {
    devs.iter()
        .filter(|d| std::fs::metadata(d).map(|m| m.file_type().is_block_device()).unwrap_or(false))
        .map(|d| OpenOptions::new().read(true).custom_flags(libc::O_EXCL).open(d)
             .map_err(|e| anyhow::anyhow!("{}: device in use: {}", d.display(), e)))
        .collect()
}

pub fn cmd_fusemount(cli: Cli) -> anyhow::Result<()> {
    use crate::device_scan::scan_sbs;

    let (mut bch_opts, mount_options, ms_flags) =
        parse_fuse_mount_options(&cli.device, cli.options.as_deref())?;

    let sbs = stderr_unless_error(|| scan_sbs(&cli.device, &bch_opts))?;
    let devs: Vec<PathBuf> = sbs.iter().map(|(p, _)| p.clone()).collect();

    let mount = match devs.first() {
        Some(d) if std::fs::metadata(d).map(|m| m.file_type().is_block_device()).unwrap_or(false) =>
            FuseMount::Fuseblk { source: d.clone(), ms_flags },
        _ => FuseMount::Fuse,
    };

    if matches!(mount, FuseMount::Fuseblk { .. }) {
        opt_set!(bch_opts, noexcl, 1);
    }

    let fs = stderr_unless_error(|| Fs::open(&devs, bch_opts))
        .map_err(|e| anyhow::anyhow!("Error opening filesystem: {}", e))?;

    // Held until we exit: across the fork, the child's copies keep the claims.
    let _claims = match &mount {
        FuseMount::Fuseblk { .. } => claim_devices(&devs[1..])?,
        FuseMount::Fuse => Vec::new(),
    };

    let mut config = Config::default();
    config.mount_options = mount_options;
    // allow_other: a filesystem is for every user, not just whoever mounted
    // it. Safe only because mount_options carries default_permissions.
    config.acl = fuser::SessionACL::All;
    // Worker threads get current + RCU via ensure_thread_init() with
    // a Drop guard for cleanup. No need to restrict to single-threaded.

    if cli.foreground {
        unsafe { c::linux_shrinkers_init() };
        fs.start().map_err(|e| anyhow::anyhow!("Error starting filesystem: {}", e))?;
        let bcachefs_fs = BcachefsFs {
            fs: Some(fs),
            signal_fd: None,
            lookups: Mutex::new(HashMap::new()),
        };
        if let Err(e) = fuse_run(bcachefs_fs, &cli.mountpoint, &config, &mount) {
            anyhow::bail!("Error mounting filesystem: {}", e);
        }
        return Ok(());
    }

    // Daemonize with pipe-based synchronization.
    //
    // The parent must not return until the FUSE mount is established,
    // otherwise mount(8) reports success before the mountpoint is usable.
    // The child signals readiness from the FUSE init() callback, which
    // fires after the kernel has acknowledged the mount.
    //
    // fork() must happen before spawning threads (linux_shrinkers_init,
    // bch2_fs_start) because only the calling thread survives fork().
    let (read_fd, write_fd) = rustix::pipe::pipe()?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        anyhow::bail!("fork() failed");
    }

    if pid > 0 {
        // The child owns the filesystem now: the parent's copy must never
        // shut it down, not even on the error paths below.
        std::mem::forget(fs);

        move_to_own_scope(pid, &cli.device, &cli.mountpoint);

        // Parent: wait for child to signal mount readiness
        drop(write_fd);
        let mut pipe = File::from(read_fd);

        // Read the status byte on its own. On success the child carries on as
        // the daemon, holding its end of the pipe open, so there is no EOF to
        // wait for and reading to end here would hang the mount.
        let mut status = [0u8; 1];
        let got = pipe.read(&mut status)?;

        if got == 1 && status[0] == CHILD_OK {
            std::process::exit(0);
        } else {
            // A failing child writes stage and reason in a single write and
            // then exits, so the rest is already queued and EOF follows.
            let mut buf = Vec::with_capacity(1 + CHILD_MSG_MAX);
            if got == 1 {
                buf.push(status[0]);
            }
            let _ = pipe.take(CHILD_MSG_MAX as u64).read_to_end(&mut buf);
            let pid = rustix::process::Pid::from_raw(pid)
                .ok_or_else(|| anyhow::anyhow!("invalid child pid {}", pid))?;
            let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());

            let reason = String::from_utf8_lossy(buf.get(1..).unwrap_or_default());
            let reason = reason.trim();
            match buf.first().copied() {
                Some(CHILD_ERR_FS_START) if !reason.is_empty() =>
                    anyhow::bail!("error starting filesystem: {reason}"),
                Some(CHILD_ERR_FS_START) =>
                    anyhow::bail!("error starting filesystem"),
                Some(CHILD_ERR_MOUNT) if !reason.is_empty() =>
                    anyhow::bail!("FUSE mount failed: {reason}"),
                Some(CHILD_ERR_MOUNT) =>
                    anyhow::bail!("FUSE mount failed"),
                Some(CHILD_ERR_SETUP) if !reason.is_empty() =>
                    anyhow::bail!("filesystem started but the mount was never attempted: {reason}"),
                Some(CHILD_ERR_SETUP) =>
                    anyhow::bail!("filesystem started but the mount was never attempted"),
                _ =>
                    anyhow::bail!("child exited without reporting a reason"),
            }
        }
    }

    // Child
    drop(read_fd);
    rustix::process::setsid()?;

    // The daemon needs its own http server: only the calling thread survives
    // fork(), so the parent's is gone. This is the call that used to happen
    // from a pthread_atfork child handler -- which also fired in the child of
    // every other fork, including the one that execs fusermount3, where
    // binding a socket and spawning threads is not allowed.
    crate::http::bch2_start_http_lazy();

    // Daemon mode must not inherit the caller's stderr or grow a fixed log
    // file under /tmp; foreground mode still leaves debug output visible.
    if let Ok(f) = std::fs::File::create("/dev/null") {
        rustix::stdio::dup2_stderr(&f)?;
    }

    unsafe { c::linux_shrinkers_init() };

    eprintln!("fusemount: starting filesystem");
    // process::exit() skips destructors: drop fs explicitly before it.
    if let Err(e) = fs.start() {
        eprintln!("fusemount: bch2_fs_start failed: {}", e);
        drop(fs);
        signal_parent_err(write_fd, CHILD_ERR_FS_START, &format!("{e:#}"));
        std::process::exit(1);
    }
    eprintln!("fusemount: filesystem started, mounting");

    let signal_fd = match write_fd.try_clone() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("fusemount: couldn't duplicate the signal fd: {e}");
            drop(fs);
            signal_parent_err(write_fd, CHILD_ERR_SETUP,
                              &format!("couldn't duplicate the signal fd: {e}"));
            std::process::exit(1);
        }
    };
    let bcachefs_fs = BcachefsFs {
        fs: Some(fs),
        signal_fd: Some(signal_fd),
        lookups: Mutex::new(HashMap::new()),
    };

    match fuse_run(bcachefs_fs, &cli.mountpoint, &config, &mount) {
        Ok(()) => {
            eprintln!("fusemount: unmounted");
        }
        Err(e) => {
            eprintln!("fusemount: mount failed: {}", e);
            // The filesystem is already shut down: by destroy() if the
            // session started, else dropped with bcachefs_fs.
            signal_parent_err(write_fd, CHILD_ERR_MOUNT, &format!("{e}"));
            std::process::exit(1);
        }
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("fusemount", "FUSE mount", Cli, cmd_fusemount);

#[cfg(test)]
mod tests {
    use super::{parse_fuse_mount_options, systime_to_ts, ts_to_systime};
    use bcachefs_kernel::opt_get;
    use bch_bindgen::c;
    use fuser::MountOption;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn parse_fuse_mount_options_sets_bcachefs_read_only_and_fuse_ro() {
        let (opts, mount_options, ms_flags) =
            parse_fuse_mount_options("/dev/test", Some("ro,norecovery")).unwrap();

        assert_eq!(opt_get!(opts, read_only), 1);
        assert!(ms_flags & libc::MS_RDONLY != 0);
        assert!(mount_options.contains(&MountOption::RO));
        assert!(mount_options.contains(&MountOption::FSName("/dev/test".to_string())));
        assert!(mount_options.contains(&MountOption::CUSTOM("subtype=bcachefs".to_string())));
    }

    #[test]
    fn timespec_systime_round_trip_including_before_1970() {
        for (sec, nsec) in [(0i64, 0u32), (1, 500_000_000), (1_700_000_000, 123),
                            (-1, 0), (-1, 500_000_000), (-2_000_000_000, 999_999_999)] {
            let ts = c::timespec { tv_sec: sec as _, tv_nsec: nsec as _ };
            let t = ts_to_systime(ts);
            let back = systime_to_ts(t);
            assert_eq!((back.tv_sec as i64, back.tv_nsec as u32), (sec, nsec),
                       "round trip of {sec}.{nsec:09}");
        }

        // -0.5s is tv_sec -1, tv_nsec 500000000:
        let half_before = UNIX_EPOCH - Duration::from_millis(500);
        let ts = systime_to_ts(half_before);
        assert_eq!((ts.tv_sec as i64, ts.tv_nsec as u32), (-1, 500_000_000));
    }

    /// We mount with allow_other and do no permission checks of our own, so
    /// without default_permissions every user could do anything.
    #[test]
    fn parse_fuse_mount_options_always_sets_default_permissions() {
        for options in [None, Some("ro"), Some("nodev,nosuid")] {
            let (_opts, mount_options, _ms_flags) =
                parse_fuse_mount_options("/dev/test", options).unwrap();
            assert!(mount_options.contains(&MountOption::DefaultPermissions),
                    "no default_permissions with -o {options:?}");
        }
    }

    #[test]
    fn parse_fuse_mount_options_preserves_supported_fuse_flags() {
        let (_opts, mount_options, _ms_flags) = parse_fuse_mount_options(
            "/dev/test",
            Some("nodev,nosuid,noexec,noatime,dirsync,sync"),
        )
        .unwrap();

        for option in [
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoExec,
            MountOption::NoAtime,
            MountOption::DirSync,
            MountOption::Sync,
        ] {
            assert!(mount_options.contains(&option));
        }
        // asked for nodev/nosuid, so not overridden - fuser rejects the pair:
        assert!(!mount_options.contains(&MountOption::Dev));
        assert!(!mount_options.contains(&MountOption::Suid));
    }

    /// Like a kernel mount, not fuser's nodev,nosuid defaults: device nodes
    /// and setuid binaries work unless -o turns them off.
    #[test]
    fn parse_fuse_mount_options_defaults_to_dev_suid() {
        let (_opts, mount_options, _ms_flags) =
            parse_fuse_mount_options("/dev/test", None).unwrap();

        assert!(mount_options.contains(&MountOption::Dev));
        assert!(mount_options.contains(&MountOption::Suid));
    }
}
