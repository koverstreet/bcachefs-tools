use std::ffi::{CString, c_char};
use std::process;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::c;
use bch_bindgen::opt_set;
use clap::Parser;

use crate::commands::format::{
    take_opt_value, take_short_value, opts_usage_str, metadata_version_current,
};
use crate::commands::opts::{bch_opt_lookup, parse_opt_val};
use crate::key::Passphrase;
use crate::util::parse_human_size;
use crate::wrappers::super_io::SUPERBLOCK_SIZE_DEFAULT;
use crate::wrappers::sysfs;

const BCH_REPLICAS_MAX: u32 = 4;

extern "C" {
    fn rust_image_create(
        fs_opt_strs: c::bch_opt_strs,
        fs_opts: c::bch_opts,
        format_opts: c::format_opts,
        dev_opts: c::dev_opts,
        src_path: *const c_char,
        keep_alloc: bool,
        verbosity: u32,
    );

    fn rust_image_update(
        src_path: *const c_char,
        dst_image: *const c_char,
        keep_alloc: bool,
        verbosity: u32,
    ) -> i32;
}

fn image_create_usage() {
    let fs_opts = opts_usage_str(
        c::opt_flags::OPT_FORMAT as u32 | c::opt_flags::OPT_FS as u32,
        c::opt_flags::OPT_DEVICE as u32,
    );
    let dev_opts = opts_usage_str(
        c::opt_flags::OPT_DEVICE as u32,
        c::opt_flags::OPT_FS as u32,
    );

    print!("\
bcachefs image create - create a filesystem image from a directory
Usage: bcachefs image create [OPTION]... <image>

Options:
      --source=path            Source directory (required)
  -a, --keep-alloc             Include allocation info in the filesystem
                               6.16+ regenerates alloc info on first rw mount
{fs_opts}\
      --replicas=#             Sets both data and metadata replicas
      --encrypted              Enable whole filesystem encryption (chacha20/poly1305)
      --passphrase_file=file   File containing passphrase used for encryption/decryption
      --no_passphrase          Don't encrypt master encryption key
  -L, --fs_label=label
  -U, --uuid=uuid
      --superblock_size=size
      --version=version        Create filesystem with specified on disk format version

Device specific options:
{dev_opts}\
      --fs_size=size           Size of filesystem on device
  -l, --label=label            Disk label

  -f, --force
  -q, --quiet                  Only print errors
  -v, --verbose                Verbose filesystem initialization
  -h, --help                   Display this help and exit

Report bugs to <linux-bcachefs@vger.kernel.org>
");
}

pub fn cmd_image_create(argv: Vec<String>) -> Result<()> {
    let opt_flags = c::opt_flags::OPT_FORMAT as u32
        | c::opt_flags::OPT_FS as u32
        | c::opt_flags::OPT_DEVICE as u32;

    let mut source: Option<String> = None;
    let mut keep_alloc = false;
    let mut encrypted = false;
    let mut no_passphrase = false;
    let mut passphrase_file: Option<String> = None;
    let mut fs_label: Option<String> = None;
    let mut uuid_bytes: Option<[u8; 16]> = None;
    let mut format_version: Option<u32> = None;
    let mut superblock_size: u32 = SUPERBLOCK_SIZE_DEFAULT;
    let mut verbosity: u32 = 1;

    let mut dev_label: Option<String> = None;
    let mut dev_fs_size: u64 = 0;
    let mut dev_opts: c::bch_opts = Default::default();

    let mut fs_opts: c::bch_opts = Default::default();
    let mut deferred_opts: Vec<(usize, String)> = Vec::new();

    let mut image_path: Option<String> = None;

    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];

        if arg == "--" {
            i += 1;
            if i < argv.len() {
                image_path = Some(argv[i].clone());
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

            if let Some((opt_id, opt)) = bch_opt_lookup(&name) {
                if opt.flags as u32 & opt_flags != 0 {
                    let val_str = if let Some(v) = inline_val {
                        v.to_string()
                    } else if opt.type_ != c::opt_type::BCH_OPT_BOOL {
                        take_opt_value(None, &argv, &mut i, raw_name)?
                    } else {
                        "1".to_string()
                    };

                    match parse_opt_val(opt, &val_str)? {
                        None => deferred_opts.push((opt_id as usize, val_str)),
                        Some(v) => {
                            if opt.flags as u32 & c::opt_flags::OPT_DEVICE as u32 != 0 {
                                unsafe { c::bch2_opt_set_by_id(&mut dev_opts, opt_id, v) };
                            } else if opt.flags as u32 & c::opt_flags::OPT_FS as u32 != 0 {
                                unsafe { c::bch2_opt_set_by_id(&mut fs_opts, opt_id, v) };
                            }
                        }
                    }

                    i += 1;
                    continue;
                }
            }

            match name.as_str() {
                "source" => {
                    source = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "keep_alloc" => keep_alloc = true,
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
                    dev_fs_size = parse_human_size(&val)?;
                }
                "superblock_size" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let size = parse_human_size(&val)?;
                    superblock_size = (size >> 9) as u32;
                }
                "label" => {
                    dev_label = Some(take_opt_value(inline_val, &argv, &mut i, raw_name)?);
                }
                "version" => {
                    let val = take_opt_value(inline_val, &argv, &mut i, raw_name)?;
                    let c_val = CString::new(val.as_str())?;
                    format_version =
                        Some(unsafe { c::version_parse(c_val.as_ptr() as *mut _) });
                }
                "force" => {}
                "quiet" => verbosity = 0,
                "verbose" => verbosity = verbosity.saturating_add(1),
                "help" => {
                    image_create_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            match arg.as_bytes()[1] {
                b'L' => {
                    fs_label = Some(take_short_value(arg, &argv, &mut i, 'L')?);
                }
                b'l' => {
                    dev_label = Some(take_short_value(arg, &argv, &mut i, 'l')?);
                }
                b'U' => {
                    let val = take_short_value(arg, &argv, &mut i, 'U')?;
                    let u = uuid::Uuid::parse_str(&val).map_err(|_| anyhow!("Bad uuid"))?;
                    uuid_bytes = Some(*u.as_bytes());
                }
                b'a' => keep_alloc = true,
                b'f' => {}
                b'q' => verbosity = 0,
                b'v' => verbosity = verbosity.saturating_add(1),
                b'h' => {
                    image_create_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        // Positional: image path
        image_path = Some(arg.clone());
        i += 1;
    }

    let source = source.ok_or_else(|| {
        image_create_usage();
        anyhow!("--source is required")
    })?;

    let image_path = image_path.ok_or_else(|| {
        image_create_usage();
        anyhow!("please supply an image path")
    })?;

    // Handle encryption
    if passphrase_file.is_some() && !encrypted {
        bail!("--passphrase_file requires --encrypted");
    }
    if passphrase_file.is_some() && no_passphrase {
        bail!("--passphrase_file, --no_passphrase are incompatible");
    }

    let passphrase: Option<Passphrase> = if encrypted && !no_passphrase {
        Some(if let Some(ref path) = passphrase_file {
            Passphrase::new_from_file(path)?
        } else {
            Passphrase::new_from_prompt_twice()?
        })
    } else {
        None
    };

    // Determine format version
    let _ = process::Command::new("modprobe")
        .arg("bcachefs")
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status();

    let kernel_version = sysfs::bcachefs_kernel_version() as u32;
    let current_version = metadata_version_current();

    let version = format_version.unwrap_or_else(|| {
        if kernel_version > 0 {
            current_version.min(kernel_version)
        } else {
            current_version
        }
    });

    // Build C format_opts
    let label_cstr = fs_label.as_ref().map(|l| CString::new(l.as_str())).transpose()?;
    let dev_label_cstr = dev_label.as_ref().map(|l| CString::new(l.as_str())).transpose()?;
    let source_cstr = CString::new(source.as_str())?;
    let path_cstr = CString::new(image_path.as_str())?;

    let mut fmt_opts: c::format_opts = Default::default();
    fmt_opts.version = version;
    fmt_opts.superblock_size = superblock_size;
    fmt_opts.encrypted = encrypted;
    if let Some(ref l) = label_cstr {
        fmt_opts.label = l.as_ptr() as *mut c_char;
    }
    if let Some(bytes) = uuid_bytes {
        fmt_opts.uuid.b = bytes;
    }
    if let Some(ref p) = passphrase {
        fmt_opts.passphrase = p.get().as_ptr() as *mut c_char;
    }

    // Build bch_opt_strs for deferred options
    let mut fs_opt_strs: c::bch_opt_strs = Default::default();
    for &(id, ref val) in &deferred_opts {
        let cstr = CString::new(val.as_str())?;
        let ptr = unsafe { libc::strdup(cstr.as_ptr()) };
        unsafe { fs_opt_strs.__bindgen_anon_1.by_id[id] = ptr };
    }

    // Build dev_opts
    let mut c_dev_opts: c::dev_opts = Default::default();
    c_dev_opts.path = path_cstr.as_ptr();
    if let Some(ref l) = dev_label_cstr {
        c_dev_opts.label = l.as_ptr();
    }
    c_dev_opts.fs_size = dev_fs_size;
    c_dev_opts.opts = dev_opts;

    // rust_image_create either returns on success or calls exit() on error
    unsafe {
        rust_image_create(
            fs_opt_strs,
            fs_opts,
            fmt_opts,
            c_dev_opts,
            source_cstr.as_ptr(),
            keep_alloc,
            verbosity,
        );
    }

    unsafe { c::bch2_opt_strs_free(&mut fs_opt_strs) };

    Ok(())
}

/// Update a filesystem image, minimizing changes
#[derive(Parser, Debug)]
#[command(about = "Update a filesystem image, minimizing changes")]
pub struct ImageUpdateCli {
    /// Source directory
    #[arg(short = 's', long = "source")]
    source: String,

    /// Include allocation info in the filesystem
    #[arg(short = 'a', long = "keep-alloc")]
    keep_alloc: bool,

    /// Only print errors
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// Verbose filesystem initialization
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,

    /// Image file to update
    #[arg(required = true)]
    image: String,
}

pub fn cmd_image_update(argv: Vec<String>) -> Result<()> {
    let cli = ImageUpdateCli::parse_from(argv);

    let verbosity: u32 = if cli.quiet {
        0
    } else {
        1 + cli.verbose as u32
    };

    let source_cstr = CString::new(cli.source.as_str())?;
    let image_cstr = CString::new(cli.image.as_str())?;

    let ret = unsafe {
        rust_image_update(
            source_cstr.as_ptr(),
            image_cstr.as_ptr(),
            cli.keep_alloc,
            verbosity,
        )
    };

    if ret != 0 {
        // Error messages already printed by C code
        process::exit(ret);
    }

    Ok(())
}
