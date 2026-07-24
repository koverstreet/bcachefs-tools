use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::raw::c_char;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use bch_bindgen::c;
use crossterm::{cursor, execute, terminal};
use rustix::ioctl::{self, Getter};

/// Page-aligned buffer for O_DIRECT IO.
///
/// Allocates zeroed memory at 4096-byte alignment. Derefs to `[u8]` for
/// convenient slice access; freed automatically on drop.
pub struct AlignedBuf {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedBuf {
    pub fn new(size: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(size, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "aligned allocation failed");
        Self { ptr, layout }
    }
}

impl std::ops::Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }
}

impl std::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

/// Open a directory fd for directory-scoped ioctls and *at() calls.
pub fn open_dir(path: &Path) -> Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    Ok(f.into())
}

/// The name of a bch_sb_error_id - the same table fsck and the
/// superblock error counters use.
pub fn sb_error_name(id: u32) -> String {
    if id < c::bch_sb_error_id::BCH_FSCK_ERR_MAX as u32 {
        unsafe { CStr::from_ptr(*c::bch2_sb_error_strs.as_ptr().add(id as usize)) }
            .to_string_lossy()
            .into_owned()
    } else {
        format!("(unknown error {id})")
    }
}

/// Parse a human-readable size string (e.g. "1G", "512M") via bch2_strtoull_h.
pub fn parse_human_size(s: &str) -> Result<u64> {
    let cstr = CString::new(s)?;
    let mut val: u64 = 0;
    if unsafe { c::bch2_strtoull_h(cstr.as_ptr(), &mut val) } != 0 {
        return Err(anyhow!("invalid size: {}", s));
    }
    Ok(val)
}

pub fn fmt_sectors_human(sectors: u64) -> String {
    fmt_bytes_human(sectors << 9)
}

pub fn fmt_bytes_human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T", "P"];
    if bytes == 0 { return "0B".to_string() }
    let mut val = bytes as f64;
    for unit in UNITS {
        if val < 1024.0 || *unit == "P" {
            return if val >= 100.0 {
                format!("{:.0}{}", val, unit)
            } else if val >= 10.0 {
                format!("{:.1}{}", val, unit)
            } else {
                format!("{:.2}{}", val, unit)
            };
        }
        val /= 1024.0;
    }
    format!("{}B", bytes)
}

pub fn fmt_num_human(n: u64) -> String {
    const UNITS: &[&str] = &["", "K", "M", "G", "T"];
    let mut val = n as f64;
    for unit in UNITS {
        if val < 1000.0 || *unit == "T" {
            return if val >= 100.0 {
                format!("{:.0}{}", val, unit)
            } else if val >= 10.0 {
                format!("{:.1}{}", val, unit)
            } else if unit.is_empty() {
                format!("{}", n)
            } else {
                format!("{:.2}{}", val, unit)
            };
        }
        val /= 1000.0;
    }
    format!("{}", n)
}

/// Get the size of a file or block device in bytes.
pub fn file_size(f: &File) -> Result<u64> {
    let meta = f.metadata()?;
    if meta.file_type().is_block_device() {
        // _IOR(0x12, 114, size_t): the size encoded in the number is
        // sizeof(size_t), not of the u64 we read into - they differ on 32
        // bit, so take the number bindgen computed for the target.
        const BLKGETSIZE64: ioctl::Opcode = bch_bindgen::c::BLKGETSIZE64 as ioctl::Opcode;
        Ok(unsafe { ioctl::ioctl(f, Getter::<BLKGETSIZE64, u64>::new()) }?)
    } else {
        Ok(meta.len())
    }
}

/// Parse a comma-separated flag list against a C string table.
///
/// Returns a bitmask of matched flags, or an error if any flag is unrecognized.
pub fn read_flag_list(s: &str, list: &[*const c_char], what: &str) -> Result<u64> {
    let c_str = CString::new(s)?;
    let v = unsafe { c::bch2_read_flag_list(c_str.as_ptr(), list.as_ptr()) };
    if v == u64::MAX {
        bail!("Bad {} {}", what, s);
    }
    Ok(v)
}

pub fn run_tui<F>(f: F) -> Result<()>
where F: FnOnce(&mut io::Stdout) -> Result<()>
{
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = f(&mut stdout);

    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    result
}
