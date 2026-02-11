use std::ffi::{CStr, CString, c_char};
use std::io;
use std::path::PathBuf;
use std::process;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;

use crate::commands::opts::bch_opt_lookup;
use crate::key::Passphrase;
use crate::util::parse_human_size;
use crate::wrappers::printbuf::Printbuf;
use crate::wrappers::sysfs;

const BCH_REPLICAS_MAX: u32 = 4;

fn metadata_version_current() -> u32 {
    c::bcachefs_metadata_version::bcachefs_metadata_version_max as u32 - 1
}

/// Capture filtered opts usage as a Rust String.
/// flags_all: all these bits must be set in the option.
/// flags_none: none of these bits may be set in the option.
fn opts_usage_str(flags_all: u32, flags_none: u32) -> String {
    let ptr = unsafe { c::rust_opts_usage_to_str(flags_all, flags_none) };
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::free(ptr as *mut _) };
    s
}

fn format_usage() {
    let fs_opts = opts_usage_str(
        c::opt_flags::OPT_FORMAT as u32 | c::opt_flags::OPT_FS as u32,
        c::opt_flags::OPT_DEVICE as u32,
    );
    let dev_opts = opts_usage_str(
        c::opt_flags::OPT_DEVICE as u32,
        c::opt_flags::OPT_FS as u32,
    );

    print!("\
bcachefs format - create a new bcachefs filesystem on one or more devices
Usage: bcachefs format [OPTION]... <devices>

Options:
{fs_opts}\
      --replicas=#             Sets both data and metadata replicas
      --encrypted              Enable whole filesystem encryption (chacha20/poly1305)
      --passphrase_file=file   File containing passphrase used for encryption/decryption
      --no_passphrase          Don't encrypt master encryption key
  -L, --fs_label=label
  -U, --uuid=uuid
      --superblock_size=size
      --version=version        Create filesystem with specified on disk format version instead of the latest
      --source=path            Initialize the bcachefs filesystem from this root directory

Device specific options:
{dev_opts}\
      --fs_size=size           Size of filesystem on device
  -l, --label=label            Disk label

  -f, --force
  -q, --quiet                  Only print errors
  -v, --verbose                Verbose filesystem initialization
  -h, --help                   Display this help and exit

Device specific options must come before corresponding devices, e.g.
  bcachefs format --label cache /dev/sdb /dev/sdc

Report bugs to <linux-bcachefs@vger.kernel.org>
");
}

/// Per-device configuration accumulated during parsing.
struct DevConfig {
    path: String,
    label: Option<String>,
    fs_size: u64,
    opts: c::bch_opts,
}

/// Get the value for a long option, consuming the next argv entry if needed.
fn take_opt_value<'a>(
    inline_val: Option<&'a str>,
    argv: &'a [String],
    i: &mut usize,
    name: &str,
) -> Result<String> {
    if let Some(v) = inline_val {
        Ok(v.to_string())
    } else {
        *i += 1;
        if *i >= argv.len() {
            bail!("option --{} requires a value", name);
        }
        Ok(argv[*i].clone())
    }
}

/// Get the value for a short option, consuming the next argv entry if needed.
/// If the arg has chars after the flag letter (e.g. "-Lfoo"), use those.
fn take_short_value(arg: &str, argv: &[String], i: &mut usize, flag: char) -> Result<String> {
    if arg.len() > 2 {
        Ok(arg[2..].to_string())
    } else {
        *i += 1;
        if *i >= argv.len() {
            bail!("-{} requires a value", flag);
        }
        Ok(argv[*i].clone())
    }
}

/// Parsed format configuration — all state from argument parsing.
struct FormatConfig {
    devices:         Vec<DevConfig>,
    force:           bool,
    quiet:           bool,
    initialize:      bool,
    encrypted:       bool,
    no_passphrase:   bool,
    passphrase_file: Option<String>,
    source:          Option<String>,
    fs_label:        Option<String>,
    uuid_bytes:      Option<[u8; 16]>,
    format_version:  Option<u32>,
    superblock_size: u32,
    fs_opts:         c::bch_opts,
    deferred_opts:   Vec<(usize, String)>,
}

fn parse_format_args(argv: Vec<String>) -> Result<FormatConfig> {
    let opt_flags = c::opt_flags::OPT_FORMAT as u32
        | c::opt_flags::OPT_FS as u32
        | c::opt_flags::OPT_DEVICE as u32;

    let mut devices: Vec<DevConfig> = Vec::new();
    let mut force = false;
    let mut no_passphrase = false;
    let mut quiet = false;
    let mut initialize = true;
    let mut encrypted = false;
    let mut passphrase_file: Option<String> = None;
    let mut source: Option<String> = None;
    let mut fs_label: Option<String> = None;
    let mut uuid_bytes: Option<[u8; 16]> = None;
    let mut format_version: Option<u32> = None;
    let mut superblock_size: u32 = 2048; // SUPERBLOCK_SIZE_DEFAULT (sectors = 1MB)

    // Per-device accumulator
    let mut cur_label: Option<String> = None;
    let mut cur_fs_size: u64 = 0;
    let mut cur_dev_opts: c::bch_opts = Default::default();
    let mut unconsumed_dev_option = false;

    let mut fs_opts: c::bch_opts = Default::default();
    let mut deferred_opts: Vec<(usize, String)> = Vec::new();

    macro_rules! push_device {
        ($path:expr) => {{
            devices.push(DevConfig {
                path: $path,
                label: cur_label.take(),
                fs_size: cur_fs_size,
                opts: std::mem::take(&mut cur_dev_opts),
            });
            cur_fs_size = 0;
            unconsumed_dev_option = false;
        }};
    }

    // Parse arguments (skip argv[0] which is the command/program name)
    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];

        // Handle -- separator: everything after is device paths
        if arg == "--" {
            i += 1;
            while i < argv.len() {
                push_device!(argv[i].clone());
                i += 1;
            }
            break;
        }

        if arg.starts_with("--") && arg.len() > 2 {
            let opt_part = &arg[2..];
            let (raw_name, inline_val) = match opt_part.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (opt_part, None),
            };
            let name = raw_name.replace('-', "_");

            // Try bcachefs option table first
            if let Some((opt_id, opt)) = bch_opt_lookup(&name) {
                if opt.flags as u32 & opt_flags != 0 {
                    let val_str = if let Some(v) = inline_val {
                        v.to_string()
                    } else if opt.type_ != c::opt_type::BCH_OPT_BOOL {
                        take_opt_value(None, &argv, &mut i, raw_name)?
                    } else {
                        "1".to_string()
                    };

                    let c_val = CString::new(val_str.as_str())?;
                    let mut v: u64 = 0;
                    let mut err = Printbuf::new();
                    let ret = unsafe {
                        c::bch2_opt_parse(
                            std::ptr::null_mut(),
                            opt,
                            c_val.as_ptr(),
                            &mut v,
                            err.as_raw(),
                        )
                    };

                    if ret == -(c::bch_errcode::BCH_ERR_option_needs_open_fs as i32) {
                        deferred_opts.push((opt_id as usize, val_str));
                        i += 1;
                        continue;
                    }

                    if ret != 0 {
                        let msg = err.as_str();
                        if msg.is_empty() {
                            bail!("invalid option: {}", val_str);
                        }
                        bail!("invalid option: {}", msg);
                    }

                    let opt_id_enum: c::bch_opt_id =
                        unsafe { std::mem::transmute(opt_id as u32) };
                    if opt.flags as u32 & c::opt_flags::OPT_DEVICE as u32 != 0 {
                        unsafe { c::bch2_opt_set_by_id(&mut cur_dev_opts, opt_id_enum, v) };
                        unconsumed_dev_option = true;
                    } else if opt.flags as u32 & c::opt_flags::OPT_FS as u32 != 0 {
                        unsafe { c::bch2_opt_set_by_id(&mut fs_opts, opt_id_enum, v) };
                    }

                    i += 1;
                    continue;
                }
            }

            // Format-specific long options
            match name.as_str() {
                "replicas" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let v: u32 = val.parse().map_err(|_| anyhow!("invalid replicas"))?;
                    if v == 0 || v > BCH_REPLICAS_MAX {
                        bail!("invalid replicas");
                    }
                    opt_set!(fs_opts, metadata_replicas, v as u8);
                    opt_set!(fs_opts, data_replicas, v as u8);
                }
                "encrypted" => encrypted = true,
                "passphrase_file" => {
                    passphrase_file =
                        Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "no_passphrase" => no_passphrase = true,
                "fs_label" => {
                    fs_label = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "uuid" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let u = uuid::Uuid::parse_str(&val).map_err(|_| anyhow!("Bad uuid"))?;
                    uuid_bytes = Some(*u.as_bytes());
                }
                "fs_size" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    cur_fs_size = parse_human_size(&val)?;
                    unconsumed_dev_option = true;
                }
                "superblock_size" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let size = parse_human_size(&val)?;
                    superblock_size = (size >> 9) as u32;
                }
                "label" => {
                    cur_label = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                    unconsumed_dev_option = true;
                }
                "version" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let c_val = CString::new(val.as_str())?;
                    format_version =
                        Some(unsafe { c::version_parse(c_val.as_ptr() as *mut _) });
                }
                "no_initialize" => initialize = false,
                "source" => {
                    source = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "force" => force = true,
                "quiet" => quiet = true,
                "verbose" => {}
                "help" => {
                    format_usage();
                    process::exit(0);
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            // Short options
            match arg.as_bytes()[1] {
                b'L' => {
                    fs_label = Some(take_short_value(arg, &argv, &mut i, 'L')?);
                }
                b'l' => {
                    cur_label = Some(take_short_value(arg, &argv, &mut i, 'l')?);
                    unconsumed_dev_option = true;
                }
                b'U' => {
                    let val = take_short_value(arg, &argv, &mut i, 'U')?;
                    let u = uuid::Uuid::parse_str(&val).map_err(|_| anyhow!("Bad uuid"))?;
                    uuid_bytes = Some(*u.as_bytes());
                }
                b'f' => force = true,
                b'q' => quiet = true,
                b'v' => {}
                b'h' => {
                    format_usage();
                    process::exit(0);
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        // Positional argument: device path
        push_device!(arg.clone());
        i += 1;
    }

    // Validations
    if unconsumed_dev_option {
        bail!("Options for devices apply to subsequent devices; got a device option with no device");
    }

    if devices.is_empty() {
        format_usage();
        bail!("Please supply a device");
    }

    if source.is_some() && !initialize {
        bail!("--source, --no_initialize are incompatible");
    }

    if passphrase_file.is_some() && !encrypted {
        bail!("--passphrase_file requires --encrypted");
    }

    if passphrase_file.is_some() && no_passphrase {
        bail!("--passphrase_file, --no_passphrase are incompatible");
    }

    Ok(FormatConfig {
        devices,
        force,
        quiet,
        initialize,
        encrypted,
        no_passphrase,
        passphrase_file,
        source,
        fs_label,
        uuid_bytes,
        format_version,
        superblock_size,
        fs_opts,
        deferred_opts,
    })
}

pub fn cmd_format(argv: Vec<String>) -> Result<()> {
    let mut cfg = parse_format_args(argv)?;

    // Handle encryption
    let passphrase: Option<Passphrase> = if cfg.encrypted && !cfg.no_passphrase {
        let p = if let Some(ref path) = cfg.passphrase_file {
            Passphrase::new_from_file(path)?
        } else {
            Passphrase::new_from_prompt_twice()?
        };
        cfg.initialize = false;
        Some(p)
    } else {
        None
    };

    // Determine format version (modprobe first to read kernel version)
    let _ = process::Command::new("modprobe")
        .arg("bcachefs")
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status();

    let kernel_version = sysfs::bcachefs_kernel_version() as u32;
    let current_version = metadata_version_current();

    let version = cfg.format_version.unwrap_or_else(|| {
        if kernel_version > 0 {
            current_version.min(kernel_version)
        } else {
            current_version
        }
    });

    if cfg.source.is_none() {
        if std::env::var_os("BCACHEFS_KERNEL_ONLY").is_some() {
            cfg.initialize = false;
        }

        if version != current_version {
            println!("version mismatch, not initializing");
            cfg.initialize = false;
        }
    }

    // Build C format_opts
    let label_cstr = cfg.fs_label.as_ref().map(|l| CString::new(l.as_str())).transpose()?;
    let source_cstr = cfg.source.as_ref().map(|s| CString::new(s.as_str())).transpose()?;

    let mut fmt_opts: c::format_opts = Default::default();
    fmt_opts.version = version;
    fmt_opts.superblock_size = cfg.superblock_size;
    fmt_opts.encrypted = cfg.encrypted;
    if let Some(ref l) = label_cstr {
        fmt_opts.label = l.as_ptr() as *mut c_char;
    }
    if let Some(ref s) = source_cstr {
        fmt_opts.source = s.as_ptr() as *mut c_char;
    }
    if let Some(bytes) = cfg.uuid_bytes {
        fmt_opts.uuid.b = bytes;
    }
    if let Some(ref p) = passphrase {
        fmt_opts.passphrase = p.get().as_ptr() as *mut c_char;
    }

    // Build bch_opt_strs for deferred options
    let mut fs_opt_strs: c::bch_opt_strs = Default::default();
    for &(id, ref val) in &cfg.deferred_opts {
        let cstr = CString::new(val.as_str())?;
        let ptr = unsafe { libc::strdup(cstr.as_ptr()) };
        unsafe { fs_opt_strs.__bindgen_anon_1.by_id[id] = ptr };
    }

    // Build C dev_opts — CStrings must outlive c_devices
    let dev_cstrs: Vec<(CString, Option<CString>)> = cfg.devices.iter()
        .map(|dev| Ok((
            CString::new(dev.path.as_str())?,
            dev.label.as_ref().map(|l| CString::new(l.as_str())).transpose()?,
        )))
        .collect::<Result<_>>()?;

    let mut c_devices: Vec<c::dev_opts> = cfg.devices.iter()
        .zip(&dev_cstrs)
        .map(|(dev, (path_c, label_c))| {
            let mut c_dev = c::dev_opts::default();
            c_dev.path = path_c.as_ptr();
            if let Some(ref l) = label_c {
                c_dev.label = l.as_ptr();
            }
            c_dev.fs_size = dev.fs_size;
            c_dev.opts = dev.opts;
            c_dev
        })
        .collect();

    // Open all devices for format
    for c_dev in &mut c_devices {
        let ret = unsafe { c::open_for_format(c_dev, 0, cfg.force) };
        if ret != 0 {
            let path = unsafe { CStr::from_ptr(c_dev.path) }.to_string_lossy();
            bail!("Error opening {}: {}", path, io::Error::from_raw_os_error(-ret));
        }
    }

    // Call bch2_format
    let dev_list = c::dev_opts_list {
        nr: c_devices.len(),
        size: c_devices.len(),
        data: c_devices.as_mut_ptr(),
        preallocated: Default::default(),
    };

    let sb = unsafe { c::bch2_format(fs_opt_strs, cfg.fs_opts, fmt_opts, dev_list) };
    if sb.is_null() {
        bail!("bch2_format returned null");
    }

    // Print superblock
    if !cfg.quiet {
        let mut buf = Printbuf::new();
        buf.set_human_readable(true);
        let fields = 1u32 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32;
        buf.sb_to_text_with_names(std::ptr::null_mut(), sb, false, fields, -1);
        print!("{}", buf);
    }
    unsafe { libc::free(sb as *mut _) };

    // Initialize filesystem
    if cfg.initialize {
        let dev_paths: Vec<PathBuf> = cfg.devices.iter().map(|d| PathBuf::from(&d.path)).collect();
        let open_opts: c::bch_opts = Default::default();

        let fs = Fs::open(&dev_paths, open_opts)
            .map_err(|e| anyhow!("error opening {}: {}", cfg.devices[0].path, e))?;

        if let Some(ref src) = cfg.source {
            let src_c = CString::new(src.as_str())?;
            let ret = unsafe { c::rust_fmt_build_fs(fs.raw, src_c.as_ptr()) };
            if ret != 0 {
                return Err(anyhow!(
                    "error copying from {}: {}",
                    src,
                    io::Error::from_raw_os_error(-ret)
                ));
            }
        }

        // Fs::drop calls bch2_fs_exit
    }

    // Free deferred option strings
    unsafe { c::bch2_opt_strs_free(&mut fs_opt_strs) };

    Ok(())
}
