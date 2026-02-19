/// FUSE mount for bcachefs.
///
/// Implements the fuser::Filesystem trait over bcachefs's internal btree
/// operations, allowing a bcachefs filesystem to be mounted without kernel
/// support. Uses the fuser crate (pure Rust FUSE implementation).
///
/// Key design notes:
/// - Inode numbers: FUSE uses flat u64. bcachefs uses (subvol, inum) pairs.
///   Currently hardcoded to subvolume 1 with root inum 4096 mapped to FUSE
///   ino 1. This is a FUSE protocol limitation — snapshot subvolumes with
///   colliding inode numbers cannot be represented in a single FUSE mount.
/// - Daemonization: Must fork() before spawning threads (Linux constraint).
///   bcachefs's shrinker threads and fs_start happen after fork.
/// - I/O alignment: All reads and writes must be block-aligned. Unaligned
///   requests get read-modify-write treatment in the C shim layer.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;

use fuser::{
    Config, FileAttr, FileType, Filesystem, MountOption,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request, TimeOrNow,
    Errno, FileHandle, FopenFlags, Generation,
    INodeNo, OpenFlags, RenameFlags,
    BsdFileFlags, WriteFlags, LockOwner,
};

use log::debug;

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
    match mode & libc::S_IFMT as u32 {
        m if m == libc::S_IFREG as u32 => FileType::RegularFile,
        m if m == libc::S_IFDIR as u32 => FileType::Directory,
        m if m == libc::S_IFLNK as u32 => FileType::Symlink,
        m if m == libc::S_IFBLK as u32 => FileType::BlockDevice,
        m if m == libc::S_IFCHR as u32 => FileType::CharDevice,
        m if m == libc::S_IFIFO as u32 => FileType::NamedPipe,
        m if m == libc::S_IFSOCK as u32 => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

fn err(ret: i32) -> Errno {
    Errno::from_i32(if ret < 0 { -ret } else { ret })
}

struct BcachefsFs {
    c: *mut c::bch_fs,
}

// Safety: bch_fs is internally synchronized with its own locking.
unsafe impl Send for BcachefsFs {}
unsafe impl Sync for BcachefsFs {}

impl BcachefsFs {
    fn inode_to_attr(&self, bi: &c::bch_inode_unpacked) -> FileAttr {
        let ts_a = unsafe { c::rust_bch2_time_to_timespec(self.c, bi.bi_atime as i64) };
        let ts_m = unsafe { c::rust_bch2_time_to_timespec(self.c, bi.bi_mtime as i64) };
        let ts_c = unsafe { c::rust_bch2_time_to_timespec(self.c, bi.bi_ctime as i64) };
        let blksize = unsafe { c::rust_block_bytes(self.c) };
        let nlink = unsafe { c::rust_inode_nlink_get(bi as *const _ as *mut _) };

        FileAttr {
            ino: INodeNo(unmap_root_ino(bi.bi_inum)),
            size: bi.bi_size,
            blocks: bi.bi_sectors,
            atime: ts_to_systime(ts_a),
            mtime: ts_to_systime(ts_m),
            ctime: ts_to_systime(ts_c),
            crtime: UNIX_EPOCH,
            kind: mode_to_filetype(bi.bi_mode as u32),
            perm: (bi.bi_mode & 0o7777) as u16,
            nlink,
            uid: bi.bi_uid,
            gid: bi.bi_gid,
            rdev: bi.bi_dev as u32,
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
        debug!("bcachefs fuse: init");
        Ok(())
    }

    fn destroy(&mut self) {
        debug!("bcachefs fuse: destroy");
        unsafe { c::bch2_fs_exit(self.c) };
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        debug!("fuse_lookup(dir={}, name={:?})", dir.inum, name);

        let mut inum: c::subvol_inum = unsafe { std::mem::zeroed() };
        let mut bi: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };

        let ret = unsafe {
            c::rust_fuse_lookup(
                self.c, dir,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
                &mut inum, &mut bi,
            )
        };

        if ret != 0 {
            // Negative dentry caching: return empty entry for ENOENT
            if ret == -(libc::ENOENT as i32) {
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

        let attr = self.inode_to_attr(&bi);
        reply.entry(&TTL, &attr, Generation(bi.bi_generation as u64));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let inum = map_root_ino(ino);
        debug!("fuse_getattr(inum={})", inum.inum);

        let mut bi: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
        let ret = unsafe { c::bch2_inode_find_by_inum(self.c, inum, &mut bi) };
        if ret != 0 {
            reply.error(err(ret));
            return;
        }

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
        let inum = map_root_ino(ino);
        debug!("fuse_setattr(inum={})", inum.inum);

        let mut bi: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };

        let (atime_flag, atime_val) = match &atime {
            None => (0, 0),
            Some(TimeOrNow::Now) => (2, 0),
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                let ts = c::timespec { tv_sec: d.as_secs() as i64, tv_nsec: d.subsec_nanos() as i64 };
                (1, unsafe { c::rust_timespec_to_bch2_time(self.c, ts) })
            }
        };
        let (mtime_flag, mtime_val) = match &mtime {
            None => (0, 0),
            Some(TimeOrNow::Now) => (2, 0),
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                let ts = c::timespec { tv_sec: d.as_secs() as i64, tv_nsec: d.subsec_nanos() as i64 };
                (1, unsafe { c::rust_timespec_to_bch2_time(self.c, ts) })
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
        let inum = map_root_ino(ino);
        debug!("fuse_readlink(inum={})", inum.inum);

        let mut bi: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
        let ret = unsafe { c::bch2_inode_find_by_inum(self.c, inum, &mut bi) };
        if ret != 0 {
            reply.error(err(ret));
            return;
        }

        let size = bi.bi_size as usize;
        let block_size = unsafe { c::rust_block_bytes(self.c) } as usize;
        let aligned_size = (size + block_size - 1) & !(block_size - 1);

        let layout = std::alloc::Layout::from_size_align(aligned_size, 4096).unwrap();
        let buf = unsafe { std::alloc::alloc(layout) };
        if buf.is_null() {
            reply.error(Errno::from_i32(libc::ENOMEM));
            return;
        }

        let ret = unsafe {
            c::rust_fuse_read_aligned(self.c, inum, aligned_size, 0, buf as *mut _)
        };
        if ret != 0 {
            unsafe { std::alloc::dealloc(buf, layout) };
            reply.error(err(ret));
            return;
        }

        let data = unsafe { std::slice::from_raw_parts(buf, size) };
        let end = data.iter().position(|&b| b == 0).unwrap_or(size);
        reply.data(&data[..end]);
        unsafe { std::alloc::dealloc(buf, layout) };
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
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        debug!("fuse_mknod(dir={}, name={:?}, mode={:#o})", dir.inum, name, mode);

        let mut new_inode: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
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
        debug!("fuse_mkdir(dir={}, name={:?})", parent.0, name);
        self.mknod(req, parent, name, mode | libc::S_IFDIR as u32, umask, 0, reply);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        debug!("fuse_unlink(dir={}, name={:?})", dir.inum, name);

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
        debug!("fuse_rmdir(dir={}, name={:?})", parent.0, name);
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
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        let link_bytes = link.as_os_str().as_bytes();
        debug!("fuse_symlink(dir={}, name={:?}, link={:?})", dir.inum, name, link);

        let mut new_inode: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            c::rust_fuse_symlink(
                self.c, dir,
                name_bytes.as_ptr() as *const _,
                name_bytes.len() as u32,
                link_bytes.as_ptr() as *const _,
                link_bytes.len() as u32,
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
        let src_dir = map_root_ino(parent);
        let dst_dir = map_root_ino(newparent);
        let src_bytes = name.as_bytes();
        let dst_bytes = newname.as_bytes();
        debug!("fuse_rename(src_dir={}, {:?} -> dst_dir={}, {:?})",
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
        let src_inum = map_root_ino(ino);
        let parent = map_root_ino(newparent);
        let name_bytes = newname.as_bytes();
        debug!("fuse_link(ino={}, newparent={}, name={:?})",
               src_inum.inum, parent.inum, newname);

        let mut inode_u: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
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
        debug!("fuse_open(ino={})", ino.0);
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
        let inum = map_root_ino(ino);
        let size = size as usize;
        debug!("fuse_read(ino={}, offset={}, size={})", inum.inum, offset, size);

        let mut bi: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
        let ret = unsafe { c::bch2_inode_find_by_inum(self.c, inum, &mut bi) };
        if ret != 0 {
            reply.error(err(ret));
            return;
        }

        let end = std::cmp::min(bi.bi_size, offset + size as u64);
        if end <= offset {
            reply.data(&[]);
            return;
        }
        let read_size = (end - offset) as usize;

        let block_size = unsafe { c::rust_block_bytes(self.c) } as u64;
        let aligned_start = offset & !(block_size - 1);
        let pad_start = (offset - aligned_start) as usize;
        let aligned_end = (offset + read_size as u64 + block_size - 1) & !(block_size - 1);
        let aligned_size = (aligned_end - aligned_start) as usize;

        let layout = std::alloc::Layout::from_size_align(aligned_size, 4096).unwrap();
        let buf = unsafe { std::alloc::alloc(layout) };
        if buf.is_null() {
            reply.error(Errno::from_i32(libc::ENOMEM));
            return;
        }

        let ret = unsafe {
            c::rust_fuse_read_aligned(
                self.c, inum,
                aligned_size, aligned_start as i64,
                buf as *mut _,
            )
        };

        if ret != 0 {
            unsafe { std::alloc::dealloc(buf, layout) };
            reply.error(err(ret));
            return;
        }

        let data = unsafe { std::slice::from_raw_parts(buf.add(pad_start), read_size) };
        reply.data(data);
        unsafe { std::alloc::dealloc(buf, layout) };
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
        let inum = map_root_ino(ino);
        let size = data.len();
        debug!("fuse_write(ino={}, offset={}, size={})", inum.inum, offset, size);

        let mut written: usize = 0;
        let ret = unsafe {
            c::rust_fuse_write(
                self.c, inum,
                data.as_ptr() as *const _,
                size, offset as i64,
                &mut written,
            )
        };

        if ret != 0 && written == 0 {
            reply.error(err(ret));
            return;
        }

        reply.written(written as u32);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let dir = map_root_ino(ino);
        debug!("fuse_readdir(dir={}, offset={})", dir.inum, offset);

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
            let file_type = match dtype as u32 {
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
        debug!("fuse_statfs");

        let usage = unsafe { c::rust_bch2_fs_usage_read_short(self.c) };
        let block_size = unsafe { c::rust_block_bytes(self.c) } as u64;
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
        let dir = map_root_ino(parent);
        let name_bytes = name.as_bytes();
        debug!("fuse_create(dir={}, name={:?}, mode={:#o})", dir.inum, name, mode);

        let mut new_inode: c::bch_inode_unpacked = unsafe { std::mem::zeroed() };
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
            reply.error(err(ret));
            return;
        }

        let attr = self.inode_to_attr(&new_inode);
        reply.created(
            &TTL, &attr,
            Generation(new_inode.bi_generation as u64),
            FileHandle(0),
            FopenFlags::FOPEN_KEEP_CACHE,
        );
    }
}

pub fn cmd_fusemount(args: Vec<String>) -> anyhow::Result<()> {
    use clap::Parser;
    use crate::device_scan::scan_sbs;

    #[derive(Parser, Debug)]
    #[command(name = "fusemount")]
    struct Cli {
        /// Mount options (-o key=value,...)
        #[arg(short = 'o')]
        options: Option<String>,

        /// Run in foreground
        #[arg(short = 'f')]
        foreground: bool,

        /// Device(s) to mount (dev1:dev2:...)
        device: String,

        /// Mountpoint
        mountpoint: String,
    }

    let cli = Cli::parse_from(args);

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
        MountOption::Subtype("bcachefs".to_string()),
    ];

    // Daemonize before spawning threads.
    // fork() only preserves the calling thread, so this must happen
    // before bch2_fs_start and linux_shrinkers_init.
    if !cli.foreground {
        unsafe {
            let pid = libc::fork();
            if pid < 0 {
                anyhow::bail!("fork failed");
            }
            if pid > 0 {
                std::process::exit(0);
            }
            libc::setsid();
        }
    }

    unsafe { c::linux_shrinkers_init() };

    let ret = unsafe { c::bch2_fs_start(fs_raw) };
    if ret != 0 {
        unsafe { c::bch2_fs_exit(fs_raw) };
        anyhow::bail!("Error starting filesystem: {}", ret);
    }

    let bcachefs_fs = BcachefsFs { c: fs_raw };
    fuser::mount2(bcachefs_fs, &cli.mountpoint, &config)?;

    Ok(())
}
