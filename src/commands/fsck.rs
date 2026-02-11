use std::ffi::CString;
use std::fmt::Write;
use std::os::unix::io::RawFd;
use std::process;

use anyhow::{anyhow, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::Parser;

use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::printbuf::Printbuf;

// _IOW(0xbc, 19, struct bch_ioctl_fsck_offline) — sizeof = 24
const BCH_IOCTL_FSCK_OFFLINE: libc::c_ulong = 0x4018bc13;
// _IOW(0xbc, 20, struct bch_ioctl_fsck_online) — sizeof = 16
const BCH_IOCTL_FSCK_ONLINE: libc::c_ulong = 0x4010bc14;

/// Filesystem check and repair
#[derive(Parser, Debug)]
#[command(about = "Check an existing filesystem for errors")]
pub struct FsckCli {
    /// Automatic repair (no questions)
    #[arg(short = 'p', short_alias = 'a')]
    auto_repair: bool,

    /// Don't repair, only check for errors
    #[arg(short = 'n')]
    no_repair: bool,

    /// Assume "yes" to all questions
    #[arg(short = 'y')]
    yes: bool,

    /// Force checking even if filesystem is marked clean
    #[arg(short = 'f')]
    force: bool,

    /// Additional mount options
    #[arg(short = 'o')]
    mount_opts: Vec<String>,

    /// Don't display more than 10 errors of a given type
    #[arg(short = 'r', long = "ratelimit_errors")]
    ratelimit_errors: bool,

    /// Use the in-kernel fsck implementation
    #[arg(short = 'k', long = "kernel")]
    kernel: bool,

    /// Don't use the in-kernel fsck implementation
    #[arg(short = 'K', long = "no-kernel")]
    no_kernel: bool,

    /// Be verbose
    #[arg(short = 'v')]
    verbose: bool,

    /// Device path(s)
    #[arg(required = true)]
    devices: Vec<String>,
}

fn setnonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

fn do_splice(rfd: RawFd, wfd: RawFd) -> i32 {
    let mut buf = [0u8; 4096];
    let r = unsafe { libc::read(rfd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if r < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) {
            return 0;
        }
        return -1;
    }
    if r == 0 {
        return 1;
    }

    let mut off = 0usize;
    while off < r as usize {
        let w = unsafe {
            libc::write(
                wfd,
                buf[off..].as_ptr() as *const libc::c_void,
                r as usize - off,
            )
        };
        if w < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                unsafe {
                    let mut fds: libc::fd_set = std::mem::zeroed();
                    libc::FD_SET(wfd, &mut fds);
                    libc::select(wfd + 1, std::ptr::null_mut(), &mut fds, std::ptr::null_mut(), std::ptr::null_mut());
                }
                continue;
            }
            return -1;
        }
        off += w as usize;
    }
    0
}

fn splice_fd_to_stdinout(fd: RawFd) -> i32 {
    setnonblocking(libc::STDIN_FILENO);
    setnonblocking(fd);

    let mut stdin_closed = false;

    loop {
        unsafe {
            let mut fds: libc::fd_set = std::mem::zeroed();
            libc::FD_SET(fd, &mut fds);
            if !stdin_closed {
                libc::FD_SET(libc::STDIN_FILENO, &mut fds);
            }
            libc::select(fd + 1, &mut fds, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        }

        let r = do_splice(fd, libc::STDOUT_FILENO);
        if r < 0 { return r; }
        if r > 0 { break; }

        let r = do_splice(libc::STDIN_FILENO, fd);
        if r < 0 { return r; }
        if r > 0 { stdin_closed = true; }
    }

    // The return code from fsck itself is returned via close()
    unsafe { libc::close(fd) }
}

fn fsck_online(fs: &BcachefsHandle, opt_str: &str) -> Result<i32> {
    let c_opts = CString::new(opt_str)?;
    let fsck = c::bch_ioctl_fsck_online {
        flags: 0,
        opts: c_opts.as_ptr() as u64,
    };

    let fsck_fd = unsafe {
        libc::ioctl(fs.ioctl_fd_raw(), BCH_IOCTL_FSCK_ONLINE, &fsck)
    };
    if fsck_fd < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        let err_str = unsafe {
            std::ffi::CStr::from_ptr(c::bch2_err_str(errno)).to_string_lossy()
        };
        return Err(anyhow!("BCH_IOCTL_FSCK_ONLINE error: {}", err_str));
    }

    Ok(splice_fd_to_stdinout(fsck_fd))
}

fn bcachefs_kernel_version() -> u64 {
    let path = "/sys/module/bcachefs/parameters/version";
    if std::fs::metadata(path).is_ok() {
        unsafe { c::read_file_u64(libc::AT_FDCWD, c"/sys/module/bcachefs/parameters/version".as_ptr()) }
    } else {
        0
    }
}

fn should_use_kernel_fsck(devs: &[String]) -> bool {
    let kernel_version = bcachefs_kernel_version();
    if kernel_version == 0 {
        return false;
    }

    let current = c::bcachefs_metadata_version::bcachefs_metadata_version_max as u64 - 1;
    if kernel_version == current {
        return false;
    }

    let dev_paths: Vec<std::path::PathBuf> = devs.iter().map(|d| d.as_str().into()).collect();
    let mut opts = bcachefs::bch_opts::default();
    opt_set!(opts, nostart, 1);
    opt_set!(opts, noexcl, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, read_only, 1);

    let fs = match Fs::open(&dev_paths, opts) {
        Ok(fs) => fs,
        Err(_) => return false,
    };

    let sb_version = unsafe { (*(*fs.raw).disk_sb.sb).version as u64 };

    let ret = (current < kernel_version && kernel_version <= sb_version) ||
              (sb_version <= kernel_version && kernel_version < current);

    if ret {
        let mut buf = Printbuf::new();
        let _ = write!(buf, "fsck binary is version ");
        unsafe { c::bch2_version_to_text(buf.as_raw(), std::mem::transmute(current as u32)) };
        let _ = write!(buf, " but filesystem is ");
        unsafe { c::bch2_version_to_text(buf.as_raw(), std::mem::transmute(sb_version as u32)) };
        let _ = write!(buf, " and kernel is ");
        unsafe { c::bch2_version_to_text(buf.as_raw(), std::mem::transmute(kernel_version as u32)) };
        let _ = write!(buf, ", using kernel fsck");
        println!("{}", buf);
    }

    ret
}

fn is_blockdev(path: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => {
            use std::os::unix::fs::FileTypeExt;
            m.file_type().is_block_device()
        }
        Err(_) => true,
    }
}

fn loopdev_alloc(path: &str) -> Option<String> {
    let output = std::process::Command::new("losetup")
        .args(["--show", "-f", path])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!("error executing losetup: {}", output.status);
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn loopdev_free(path: &str) {
    let _ = std::process::Command::new("losetup")
        .args(["-d", path])
        .status();
}

pub fn cmd_fsck(argv: Vec<String>) -> Result<()> {
    let cli = FsckCli::parse_from(argv);

    if cli.auto_repair {
        // Automatic run, called by the system — we don't need checks here
        process::exit(0);
    }

    let kernel = if std::env::var("BCACHEFS_KERNEL_ONLY").is_ok() {
        Some(true)
    } else if cli.kernel {
        Some(true)
    } else if cli.no_kernel {
        Some(false)
    } else {
        None // auto-detect
    };

    let mut opts_str = String::from("degraded,fsck,fix_errors=ask,read_only");

    if cli.yes {
        opts_str.push_str(",fix_errors=yes");
    }
    if cli.no_repair {
        opts_str.push_str(",nochanges,fix_errors=no");
    }
    for o in &cli.mount_opts {
        opts_str.push(',');
        opts_str.push_str(o);
    }
    if cli.ratelimit_errors {
        opts_str.push_str(",ratelimit_errors");
    }
    if cli.verbose {
        opts_str.push_str(",verbose");
    }

    let devices = &cli.devices;

    // Check if any device is a mountpoint/directory (online fsck)
    if devices.len() == 1 {
        if let Ok(m) = std::fs::metadata(&devices[0]) {
            if m.is_dir() {
                println!("Running fsck online");
                let fs = BcachefsHandle::open(&devices[0])?;
                let ret = fsck_online(&fs, &opts_str)?;
                process::exit(ret);
            }
        }
    }

    // Check if any device is mounted (online fsck)
    for dev in devices {
        let c_dev = CString::new(dev.as_str())?;
        if unsafe { c::dev_mounted(c_dev.as_ptr()) != 0 } {
            println!("Running fsck online");
            let fs = BcachefsHandle::open(dev)?;
            let ret = fsck_online(&fs, &opts_str)?;
            process::exit(ret);
        }
    }

    if kernel == Some(true) {
        let _ = std::process::Command::new("modprobe")
            .arg("bcachefs")
            .status();
    }

    let kernel_probed = kernel.unwrap_or_else(|| should_use_kernel_fsck(devices));

    if kernel_probed {
        println!("Running in-kernel offline fsck");

        let mut loopdevs: Vec<String> = Vec::new();
        let mut dev_ptrs: Vec<u64> = Vec::new();
        let mut c_devs: Vec<CString> = Vec::new();

        for dev in devices {
            if is_blockdev(dev) {
                let c_dev = CString::new(dev.as_str())?;
                dev_ptrs.push(c_dev.as_ptr() as u64);
                c_devs.push(c_dev);
            } else {
                match loopdev_alloc(dev) {
                    Some(l) => {
                        let c_dev = CString::new(l.as_str())?;
                        dev_ptrs.push(c_dev.as_ptr() as u64);
                        c_devs.push(c_dev);
                        loopdevs.push(l);
                    }
                    None => {
                        for l in &loopdevs { loopdev_free(l); }
                        if kernel == Some(true) {
                            return Err(anyhow!("error setting up loop devices"));
                        }
                        // Fall through to userspace fsck
                        return run_userspace_fsck(devices, &opts_str);
                    }
                }
            }
        }

        // Allocate fsck struct with flexible array
        let base_size = std::mem::size_of::<c::bch_ioctl_fsck_offline>();
        let total_size = base_size + dev_ptrs.len() * std::mem::size_of::<u64>();
        let layout = std::alloc::Layout::from_size_align(total_size, 8).unwrap();
        let fsck_ptr = unsafe { std::alloc::alloc_zeroed(layout) } as *mut c::bch_ioctl_fsck_offline;

        let c_opts = CString::new(opts_str.as_str())?;
        unsafe {
            (*fsck_ptr).opts = c_opts.as_ptr() as u64;
            (*fsck_ptr).nr_devs = dev_ptrs.len() as u64;
            let devs_array = (*fsck_ptr).devs.as_mut_ptr();
            for (i, ptr) in dev_ptrs.iter().enumerate() {
                *devs_array.add(i) = *ptr;
            }
        }

        let ctl_fd = unsafe { libc::open(c"/dev/bcachefs-ctl".as_ptr(), libc::O_RDWR) };
        let fsck_fd = if ctl_fd >= 0 {
            let fd = unsafe { libc::ioctl(ctl_fd, BCH_IOCTL_FSCK_OFFLINE, fsck_ptr) };
            unsafe { libc::close(ctl_fd); }
            fd
        } else {
            -1
        };

        unsafe { std::alloc::dealloc(fsck_ptr as *mut u8, layout); }

        for l in &loopdevs { loopdev_free(l); }

        if fsck_fd < 0 && kernel.is_none() {
            return run_userspace_fsck(devices, &opts_str);
        }

        if fsck_fd < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            let err_str = unsafe {
                std::ffi::CStr::from_ptr(c::bch2_err_str(errno)).to_string_lossy()
            };
            return Err(anyhow!("BCH_IOCTL_FSCK_OFFLINE error: {}", err_str));
        }

        let ret = splice_fd_to_stdinout(fsck_fd);
        process::exit(ret);
    }

    run_userspace_fsck(devices, &opts_str)
}

fn run_userspace_fsck(devices: &[String], opts_str: &str) -> Result<()> {
    println!("Running userspace offline fsck");

    let dev_paths: Vec<std::path::PathBuf> = devices.iter().map(|d| d.as_str().into()).collect();

    let mut fs_opts = bcachefs::bch_opts::default();
    let c_opts_str = CString::new(opts_str)?;
    let c_opts_ptr = c_opts_str.into_raw();
    let mut parse_later = Printbuf::new();
    let ret = unsafe {
        let r = c::bch2_parse_mount_opts(
            std::ptr::null_mut(),
            &mut fs_opts,
            parse_later.as_raw(),
            c_opts_ptr,
            false,
        );
        // Reclaim the CString to free it
        let _ = CString::from_raw(c_opts_ptr);
        r
    };
    if ret != 0 {
        process::exit(ret);
    }

    let fs = Fs::open(&dev_paths, fs_opts)?;

    let mut buf = Printbuf::new();
    let ret = unsafe { c::bch2_fs_fsck_errcode(fs.raw, buf.as_raw()) };
    if ret != 0 {
        eprint!("{}", buf);
    }

    let ret2 = unsafe { c::bch2_fs_exit(fs.raw) };

    // Prevent Fs::drop from calling bch2_fs_exit again
    std::mem::forget(fs);

    if ret2 != 0 {
        let err_str = unsafe {
            std::ffi::CStr::from_ptr(c::bch2_err_str(ret2)).to_string_lossy()
        };
        eprintln!("error shutting down filesystem: {}", err_str);
        process::exit(ret | 8);
    }

    process::exit(ret)
}
