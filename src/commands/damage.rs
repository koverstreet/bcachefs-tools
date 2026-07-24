// commands/damage.rs — show recorded filesystem damage
//
// The damage btree persistently records which inodes were damaged by
// errors and repairs. Two ioctls, layered: BCHFS_IOC_READDIR_FLAGS with
// damaged finds the files - cost proportional to recorded damage,
// not tree size - and BCHFS_IOC_GET_DAMAGE per file returns its
// accumulated error list (unioned across ancestor snapshots) as
// bch_sb_error_id, printed with the same names fsck and the superblock
// error counters use.

use std::ffi::CString;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bch_bindgen::c;
use clap::{Parser, Subcommand};

use crate::util::{open_dir, sb_error_name};
use crate::wrappers::ioctl::{bch_ioc_none, bch_ioc_wr};

const BCHFS_IOC_GET_DAMAGE_NR: u32 = 70;
const BCHFS_IOC_READDIR_FLAGS_NR: u32 = 69;
const BCHFS_IOC_CLEAR_DAMAGE_NR: u32 = 71;

/// DT_SUBVOL from dirent_format.h; not in the generated bindings.
const DT_SUBVOL: u8 = 16;

/// Show recorded damage
#[derive(Parser, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Option<Subcommands>,

    /// File to report recorded damage on
    path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// List damaged files in a directory
    Ls {
        /// Recurse: damaged files anywhere under this directory
        #[arg(short = 'R')]
        recursive: bool,

        /// Directory to list (default: current directory)
        path: Option<PathBuf>,
    },

    /// Clear recorded damage. Requires owning the file, like chattr;
    /// snapshots keep their view of the record.
    Clear {
        /// Recurse: also clear every damaged file under this directory
        #[arg(short = 'R')]
        recursive: bool,

        path: PathBuf,
    },
}

// ---- Ioctl layer ----

/// One accumulated damage record, unpacked from
/// bch_sb_field_error_entry_v2 - the same record the errors superblock
/// section keeps: error id, a saturating occurrence count, and first and
/// last occurrence times.
struct DamageEntry {
    id:    u32,
    nr:    u64,
    first: i64,
    last:  i64,
}

fn fmt_time(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| t.to_string())
}

impl DamageEntry {
    fn first_seen(&self) -> String {
        fmt_time(self.first)
    }
    fn last_seen(&self) -> String {
        fmt_time(self.last)
    }
}

/// BCHFS_IOC_GET_DAMAGE: nr_entries in is capacity, out is the true
/// count - retry bigger if we undershot. The argument is the header
/// immediately followed by its flexible entries array; allocate u64s
/// for alignment and view them through the real struct.
fn get_damage(fd: &OwnedFd) -> std::io::Result<Vec<DamageEntry>> {
    const HDR: usize   = mem::size_of::<c::bch_ioctl_get_damage>() / 8;
    const ENTRY: usize = mem::size_of::<c::bch_sb_field_error_entry_v2>() / 8;
    let mut cap = 16u32;
    loop {
        let mut buf = vec![0u64; HDR + ENTRY * cap as usize];
        let arg = buf.as_mut_ptr().cast::<c::bch_ioctl_get_damage>();
        unsafe { (*arg).nr_entries = cap };

        let ret = unsafe {
            libc::ioctl(fd.as_raw_fd(),
                        bch_ioc_wr::<c::bch_ioctl_get_damage>(BCHFS_IOC_GET_DAMAGE_NR),
                        arg)
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let nr = unsafe { (*arg).nr_entries };
        if nr > cap {
            cap = nr;
            continue;
        }

        return Ok(unsafe { (*arg).entries.as_slice(nr as usize) }.iter()
            .map(|e| unsafe {
                DamageEntry {
                    id:    c::BCH_SB_ERROR_ENTRY_V2_ID(e) as u32,
                    nr:    c::BCH_SB_ERROR_ENTRY_V2_NR(e),
                    first: c::BCH_SB_ERROR_ENTRY_V2_FIRST(e) as i64,
                    last:  c::BCH_SB_ERROR_ENTRY_V2_LAST(e) as i64,
                }
            }).collect());
    }
}

struct Entry {
    d_type: u8,
    /// A name, or in recursive mode a path relative to the ioctl'd
    /// directory
    name:   Vec<u8>,
}

/// One BCHFS_IOC_READDIR_FLAGS call, advancing the opaque cursor. An
/// empty result means enumeration is complete.
fn readdir_flags(fd: &OwnedFd, flags: u32, pos: &mut [u64; 2]) -> std::io::Result<Vec<Entry>> {
    let mut buf = vec![0u8; 64 << 10];
    let mut arg = c::bch_ioctl_readdir_flags {
        pos: *pos,
        buf: buf.as_mut_ptr() as u64,
        buf_size: buf.len() as u32,
        flags,
        used: 0,
        pad: 0,
    };

    let ret = unsafe {
        libc::ioctl(fd.as_raw_fd(),
                    bch_ioc_wr::<c::bch_ioctl_readdir_flags>(BCHFS_IOC_READDIR_FLAGS_NR),
                    &mut arg as *mut _)
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    *pos = arg.pos;

    const D_TYPE: usize   = mem::offset_of!(c::bch_ioctl_readdir_entry, d_type);
    const NAME_LEN: usize = mem::offset_of!(c::bch_ioctl_readdir_entry, name_len);
    const NAME: usize     = mem::offset_of!(c::bch_ioctl_readdir_entry, name);

    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset + NAME < arg.used as usize {
        let rec = &buf[offset..];
        let name_len = u16::from_ne_bytes(rec[NAME_LEN..NAME_LEN + 2].try_into().unwrap()) as usize;
        let reclen = (NAME + name_len).next_multiple_of(8);
        if name_len == 0 || offset + reclen > arg.used as usize {
            break;
        }

        entries.push(Entry {
            d_type: rec[D_TYPE],
            /* name_len includes the nul */
            name:   rec[NAME..NAME + name_len - 1].to_vec(),
        });
        offset += reclen;
    }
    Ok(entries)
}

// ---- Commands ----

/// The damage details for a listed entry, best effort: only inode types
/// we can open and ioctl (a device node's fd would take the ioctl to
/// the device driver). Recursive mode reports DT_UNKNOWN, so stat then.
fn entry_errors(dir: &OwnedFd, e: &Entry) -> Option<Vec<DamageEntry>> {
    let cname = CString::new(e.name.clone()).ok()?;

    let mut d_type = e.d_type;
    if d_type == libc::DT_UNKNOWN {
        let mut st: libc::stat = unsafe { mem::zeroed() };
        if unsafe { libc::fstatat(dir.as_raw_fd(), cname.as_ptr(), &mut st,
                                  libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return None;
        }
        d_type = match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => libc::DT_REG,
            libc::S_IFDIR => libc::DT_DIR,
            _ => return None,
        };
    }
    if d_type != libc::DT_REG && d_type != libc::DT_DIR && d_type != DT_SUBVOL {
        return None;
    }

    let fd = unsafe {
        libc::openat(dir.as_raw_fd(), cname.as_ptr(),
                     libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
    };
    if fd < 0 {
        return None;
    }
    get_damage(&unsafe { OwnedFd::from_raw_fd(fd) }).ok()
}

fn cmd_ls(path: &Path, recursive: bool) -> Result<()> {
    let dir = open_dir(path)?;
    let flags = c::BCH_READDIR_damaged |
        if recursive { c::BCH_READDIR_recursive } else { 0 };

    let mut pos = [0u64; 2];
    loop {
        let entries = readdir_flags(&dir, flags, &mut pos)
            .context("BCHFS_IOC_READDIR_FLAGS")?;
        if entries.is_empty() {
            return Ok(());
        }

        for e in &entries {
            let name = String::from_utf8_lossy(&e.name);
            match entry_errors(&dir, e) {
                Some(errs) if !errs.is_empty() =>
                    println!("{name}: {}",
                             errs.iter().map(|e| sb_error_name(e.id))
                                 .collect::<Vec<_>>().join(" ")),
                _ => println!("{name}"),
            }
        }
    }
}

fn cmd_show(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("statting {}", path.display()))?;
    if !meta.is_file() && !meta.is_dir() {
        bail!("{}: not a regular file or directory", path.display());
    }

    let f = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let errs = get_damage(&f.into()).context("BCHFS_IOC_GET_DAMAGE")?;

    for e in errs {
        println!("{:<48} nr {:<8} first {}  last {}",
                 sb_error_name(e.id), e.nr, e.first_seen(), e.last_seen());
    }
    Ok(())
}

fn clear_fd(fd: &OwnedFd) -> std::io::Result<()> {
    let r = unsafe {
        libc::ioctl(fd.as_raw_fd(), bch_ioc_none(BCHFS_IOC_CLEAR_DAMAGE_NR))
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn cmd_clear(path: &Path, recursive: bool) -> Result<()> {
    let f = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    clear_fd(&f.into())
        .with_context(|| format!("clearing damage on {}", path.display()))?;

    if !recursive {
        return Ok(());
    }

    /*
     * The damaged-only recursive listing is also the work list: entries
     * vanish from it as they're cleared, and the cursor only moves
     * forward, so clearing behind it is safe. Per-entry failures
     * (ownership, racing renames) are reported and skipped.
     */
    let dir = open_dir(path)?;
    let flags = c::BCH_READDIR_damaged | c::BCH_READDIR_recursive;
    let mut failed = 0u64;

    let mut pos = [0u64; 2];
    loop {
        let entries = readdir_flags(&dir, flags, &mut pos)
            .context("BCHFS_IOC_READDIR_FLAGS")?;
        if entries.is_empty() {
            break;
        }

        for e in &entries {
            let name = String::from_utf8_lossy(&e.name).into_owned();
            let Ok(cname) = CString::new(e.name.clone()) else { continue };

            let fd = unsafe {
                libc::openat(dir.as_raw_fd(), cname.as_ptr(),
                             libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            };
            if fd < 0 {
                eprintln!("{name}: {}", std::io::Error::last_os_error());
                failed += 1;
                continue;
            }

            if let Err(err) = clear_fd(&unsafe { OwnedFd::from_raw_fd(fd) }) {
                eprintln!("{name}: {err}");
                failed += 1;
            }
        }
    }

    if failed != 0 {
        bail!("failed to clear {failed} files");
    }
    Ok(())
}

pub fn damage(cli: Cli) -> Result<()> {
    match cli.subcommands {
        Some(Subcommands::Ls { recursive, path }) =>
            cmd_ls(&path.unwrap_or_else(|| ".".into()), recursive),
        Some(Subcommands::Clear { recursive, path }) =>
            cmd_clear(&path, recursive),
        None =>
            cmd_show(&cli.path.context("a path, or the ls subcommand, is required")?),
    }
}

pub const CMD: super::CmdDef = typed_cmd!("damage", "Show recorded filesystem damage", Cli, damage);
