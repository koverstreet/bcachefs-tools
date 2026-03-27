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
//   bcachefs's shrinker threads and fs_start happen after fork.
// - I/O alignment: All reads and writes must be block-aligned. Unaligned
//   requests get read-modify-write treatment in the write handler.

use std::cell::Cell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bch_bindgen::c;
use bch_bindgen::data::io::block_on;
use bch_bindgen::errcode::BchError;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;

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
    match mode & libc::S_IFMT {
        m if m == libc::S_IFREG => FileType::RegularFile,
        m if m == libc::S_IFDIR => FileType::Directory,
        m if m == libc::S_IFLNK => FileType::Symlink,
        m if m == libc::S_IFBLK => FileType::BlockDevice,
        m if m == libc::S_IFCHR => FileType::CharDevice,
        m if m == libc::S_IFIFO => FileType::NamedPipe,
        m if m == libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
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

struct BcachefsFs {
    c: *mut c::bch_fs,
    /// Write end of a pipe used to signal the parent process that the
    /// FUSE mount is established. Written in init(), None in foreground mode.
    signal_fd: Option<i32>,
}

// Safety: bch_fs is internally synchronized with its own locking.
unsafe impl Send for BcachefsFs {}
unsafe impl Sync for BcachefsFs {}

impl BcachefsFs {
    fn fs(&self) -> std::mem::ManuallyDrop<Fs> {
        unsafe { Fs::borrow_raw(self.c) }
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

fn ts_to_systime(ts: c::timespec) -> SystemTime {
    if ts.tv_sec >= 0 {
        UNIX_EPOCH + Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    } else {
        UNIX_EPOCH
    }
}

impl Filesystem for BcachefsFs {
    fn init(&mut self, _req: &Request, _config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        eprintln!("bcachefs fuse: init callback fired");
        // Signal parent that mount is established
        if let Some(fd) = self.signal_fd.take() {
            eprintln!("bcachefs fuse: signaling parent (fd={})", fd);
            unsafe {
                let byte = 0u8;
                libc::write(fd, &byte as *const _ as *const _, 1);
                libc::close(fd);
            }
        }
        eprintln!("bcachefs fuse: init returning Ok");
        Ok(())
    }

    fn destroy(&mut self) {
        eprintln!("bcachefs fuse: destroy");
        unsafe { c::bch2_fs_exit(self.c) };
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_lookup(dir={}, name={:?})", dir.inum, name);

        let mut inum: c::subvol_inum = Default::default();
        let mut bi: c::bch_inode_unpacked = Default::default();

        let ret = unsafe {
            c::rust_fuse_lookup(
                self.c, dir,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
                &mut inum, &mut bi,
            )
        };

        if ret != 0 {
            let e = BchError::from_raw(-ret);
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
            reply.error(err(ret));
            return;
        }

        eprintln!("  lookup -> ok inum={}", inum.inum);
        let attr = self.inode_to_attr(&bi);
        reply.entry(&TTL, &attr, Generation(bi.bi_generation as u64));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_getattr(inum={})", inum.inum);

        let bi = match self.fs().inode_find_by_inum(inum) {
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
        _ctime: Option<SystemTime>,
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

        let mut bi: c::bch_inode_unpacked = Default::default();
        let fs = self.fs();

        let (atime_flag, atime_val) = match &atime {
            None => (0, 0),
            Some(TimeOrNow::Now) => (2, 0),
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                let ts = c::timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ };
                (1, fs.timespec_to_time(ts))
            }
        };
        let (mtime_flag, mtime_val) = match &mtime {
            None => (0, 0),
            Some(TimeOrNow::Now) => (2, 0),
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                let ts = c::timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ };
                (1, fs.timespec_to_time(ts))
            }
        };

        let ret = unsafe {
            c::rust_fuse_setattr(
                self.c, inum, &mut bi,
                mode.is_some() as i32, mode.unwrap_or(0) as u16,
                uid.is_some() as i32, uid.unwrap_or(0),
                gid.is_some() as i32, gid.unwrap_or(0),
                size.is_some() as i32, size.unwrap_or(0),
                atime_flag, atime_val,
                mtime_flag, mtime_val,
            )
        };

        if ret != 0 {
            reply.error(err(ret));
            return;
        }

        reply.attr(&TTL, &self.inode_to_attr(&bi));
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        ensure_thread_init();
        let inum = map_root_ino(ino);
        eprintln!("fuse_readlink(inum={})", inum.inum);

        let fs = self.fs();
        let bi = match fs.inode_find_by_inum(inum) {
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
        _req: &Request,
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

        let mut new_inode: c::bch_inode_unpacked = Default::default();
        let ret = unsafe {
            c::rust_fuse_create(
                self.c, dir,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
                mode as u16, rdev as u64,
                &mut new_inode,
            )
        };

        if ret != 0 {
            reply.error(err(ret));
            return;
        }

        let attr = self.inode_to_attr(&new_inode);
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
        self.mknod(req, parent, name, mode | libc::S_IFDIR, umask, 0, reply);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        ensure_thread_init();
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        eprintln!("fuse_unlink(dir={}, name={:?})", dir.inum, name);

        let ret = unsafe {
            c::rust_fuse_unlink(
                self.c, dir,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
            )
        };

        if ret != 0 {
            reply.error(err(ret));
        } else {
            reply.ok();
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("fuse_rmdir(dir={}, name={:?})", parent.0, name);
        self.unlink(req, parent, name, reply);
    }

    fn symlink(
        &self,
        _req: &Request,
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
        let mut new_inode: c::bch_inode_unpacked = Default::default();
        let ret = unsafe {
            c::rust_fuse_create(
                self.c, dir,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
                (libc::S_IFLNK | 0o777) as u16, 0,
                &mut new_inode,
            )
        };

        if ret != 0 {
            reply.error(err(ret));
            return;
        }

        // Write link target (include NUL terminator, like the C code did)
        let fs = self.fs();
        let block_size = fs.block_bytes();
        let link_with_nul_len = link_bytes.len() + 1;
        let padded = (link_with_nul_len as u64).div_ceil(block_size) * block_size;

        let mut buf = AlignedBuf::new(padded as usize);
        buf[..link_bytes.len()].copy_from_slice(link_bytes);
        // buf is zero-initialized, so NUL terminator and padding are already 0

        let sym_inum = c::subvol_inum { subvol: dir.subvol, inum: new_inode.bi_inum };
        if let Err(e) = block_on(fs.write(new_inode.bi_inum, 0, dir.subvol as u32,
                                          1, &buf, link_with_nul_len as u64)) {
            reply.error(bch_err(&e));
            return;
        }

        // Re-read inode to get updated state
        let new_inode = match self.fs().inode_find_by_inum(sym_inum) {
            Ok(bi) => bi,
            Err(e) => { reply.error(bch_err(&e)); return; }
        };

        let attr = self.inode_to_attr(&new_inode);
        reply.entry(&TTL, &attr, Generation(new_inode.bi_generation as u64));
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        ensure_thread_init();
        let src_dir = map_root_ino(parent);
        let dst_dir = map_root_ino(newparent);
        let src_bytes = name.as_bytes();
        let dst_bytes = newname.as_bytes();
        eprintln!("fuse_rename(src_dir={}, {:?} -> dst_dir={}, {:?})",
               src_dir.inum, name, dst_dir.inum, newname);

        let ret = unsafe {
            c::rust_fuse_rename(
                self.c,
                src_dir, src_bytes.as_ptr() as *const _, src_bytes.len() as u32,
                dst_dir, dst_bytes.as_ptr() as *const _, dst_bytes.len() as u32,
            )
        };

        if ret != 0 {
            reply.error(err(ret));
        } else {
            reply.ok();
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

        let mut inode_u: c::bch_inode_unpacked = Default::default();
        let ret = unsafe {
            c::rust_fuse_link(
                self.c, src_inum, parent,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
                &mut inode_u,
            )
        };

        if ret != 0 {
            reply.error(err(ret));
            return;
        }

        let attr = self.inode_to_attr(&inode_u);
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
        let bi = match fs.inode_find_by_inum(inum) {
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
        let bi = match fs.inode_find_by_inum(inum) {
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
        let mut opts: c::bch_inode_opts = Default::default();
        unsafe { c::bch2_inode_opts_get_inode(self.c, &bi as *const _ as *mut _, &mut opts) };
        let replicas = std::cmp::max(opts.data_replicas as u32, 1);

        // Write aligned buffer
        let new_i_size = offset + size as u64;
        if let Err(e) = block_on(fs.write(bi.bi_inum, aligned_start, inum.subvol as u32,
                                          replicas, &buf, new_i_size)) {
            reply.error(bch_err(&e));
            return;
        }

        // Update inode times
        let ret = unsafe { c::rust_fuse_update_inode_after_write(self.c, inum) };
        if ret != 0 {
            reply.error(err(ret));
            return;
        }

        reply.written(size as u32);
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
            let file_type = match dtype {
                t if t == libc::DT_DIR as u32 => FileType::Directory,
                t if t == libc::DT_REG as u32 => FileType::RegularFile,
                t if t == libc::DT_LNK as u32 => FileType::Symlink,
                t if t == libc::DT_BLK as u32 => FileType::BlockDevice,
                t if t == libc::DT_CHR as u32 => FileType::CharDevice,
                t if t == libc::DT_FIFO as u32 => FileType::NamedPipe,
                t if t == libc::DT_SOCK as u32 => FileType::Socket,
                _ => FileType::RegularFile,
            };
            let full = reply.add(INodeNo(unmap_root_ino(ino)), pos, file_type, name_str);
            if full { -1 } else { 0 }
        }

        let ret = unsafe {
            c::rust_fuse_readdir(
                self.c, dir, pos,
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

        let usage = unsafe { c::rust_bch2_fs_usage_read_short(self.c) };
        let block_size = self.fs().block_bytes();
        let shift = unsafe { (*self.c).block_bits } as u64;

        let mut nr_inodes: u64 = 0;
        unsafe { c::rust_fuse_count_inodes(self.c, &mut nr_inodes) };

        reply.statfs(
            usage.capacity >> shift,
            (usage.capacity - usage.used) >> shift,
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
        _req: &Request,
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

        let mut new_inode: c::bch_inode_unpacked = Default::default();
        let ret = unsafe {
            c::rust_fuse_create(
                self.c, dir,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
                mode as u16, 0,
                &mut new_inode,
            )
        };

        if ret != 0 {
            eprintln!("  create -> err {}", BchError::from_raw(-ret));
            reply.error(err(ret));
            return;
        }

        eprintln!("  create -> ok inum={}", new_inode.bi_inum);
        let attr = self.inode_to_attr(&new_inode);
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

pub fn cmd_fusemount(cli: Cli) -> anyhow::Result<()> {
    use crate::device_scan::scan_sbs;

    let mut bch_opts = c::bch_opts::default();
    opt_set!(bch_opts, nostart, 1);

    let sbs = scan_sbs(&cli.device, &bch_opts)?;
    let devs: Vec<_> = sbs.iter().map(|(p, _)| p.clone()).collect();

    let fs = Fs::open(&devs, bch_opts)
        .map_err(|e| anyhow::anyhow!("Error opening filesystem: {}", e))?;
    let fs_raw = fs.raw;
    // BcachefsFs::destroy takes ownership — prevent Fs double-free
    std::mem::forget(fs);

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(cli.device.clone()),
        // Use CUSTOM instead of Subtype — fuser categorizes Subtype as
        // "Fusermount" group, which is only passed when using the fusermount3
        // helper. With a direct mount syscall (as root), Subtype gets
        // silently dropped and the mount shows as "fuse" instead of
        // "fuse.bcachefs" in /proc/mounts.
        MountOption::CUSTOM("subtype=bcachefs".to_string()),
    ];
    // Worker threads get current + RCU via ensure_thread_init() with
    // a Drop guard for cleanup. No need to restrict to single-threaded.

    if cli.foreground {
        unsafe { c::linux_shrinkers_init() };
        let ret = unsafe { c::bch2_fs_start(fs_raw) };
        if ret != 0 {
            unsafe { c::bch2_fs_exit(fs_raw) };
            anyhow::bail!("Error starting filesystem: {}", ret);
        }
        let bcachefs_fs = BcachefsFs { c: fs_raw, signal_fd: None };
        fuser::mount2(bcachefs_fs, &cli.mountpoint, &config)?;
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
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("pipe() failed");
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        anyhow::bail!("fork() failed");
    }

    if pid > 0 {
        // Parent: wait for child to signal mount readiness
        unsafe { libc::close(pipe_fds[1]) };
        let mut buf = [0u8; 1];
        let n = unsafe {
            libc::read(pipe_fds[0], buf.as_mut_ptr() as *mut _, 1)
        };
        unsafe { libc::close(pipe_fds[0]) };

        if n == 1 && buf[0] == 0 {
            std::process::exit(0);
        } else {
            let mut status = 0i32;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            anyhow::bail!("FUSE mount failed in child process");
        }
    }

    // Child
    unsafe {
        libc::close(pipe_fds[0]);
        libc::setsid();
    }

    // Redirect stderr to a log file so debug output is visible
    if let Ok(f) = std::fs::File::create("/tmp/bcachefs-fuse.log") {
        use std::os::unix::io::IntoRawFd;
        unsafe { libc::dup2(f.into_raw_fd(), 2) };
    }

    unsafe { c::linux_shrinkers_init() };

    eprintln!("fusemount: starting filesystem");
    let ret = unsafe { c::bch2_fs_start(fs_raw) };
    if ret != 0 {
        eprintln!("fusemount: bch2_fs_start failed: {}", BchError::from_raw(-ret));
        unsafe { c::bch2_fs_exit(fs_raw) };
        unsafe {
            let byte = 1u8;
            libc::write(pipe_fds[1], &byte as *const _ as *const _, 1);
            libc::close(pipe_fds[1]);
        }
        std::process::exit(1);
    }
    eprintln!("fusemount: filesystem started, calling fuser::mount2");

    let bcachefs_fs = BcachefsFs { c: fs_raw, signal_fd: Some(pipe_fds[1]) };

    match fuser::mount2(bcachefs_fs, &cli.mountpoint, &config) {
        Ok(()) => {
            eprintln!("fusemount: fuser::mount2 returned normally (unmounted)");
        }
        Err(e) => {
            eprintln!("fusemount: fuser::mount2 failed: {}", e);
            unsafe {
                let byte = 1u8;
                libc::write(pipe_fds[1], &byte as *const _ as *const _, 1);
                libc::close(pipe_fds[1]);
            }
            std::process::exit(1);
        }
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("fusemount", "FUSE mount", Cli, cmd_fusemount);
