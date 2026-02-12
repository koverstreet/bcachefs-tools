use std::ffi::{CString, c_char};
use std::process;

use anyhow::{anyhow, bail, Result};
use bch_bindgen::c;
use bch_bindgen::printbuf::Printbuf;
use clap::Parser;

use crate::commands::format::take_opt_value;
use crate::commands::opts::bch_opt_lookup;
use crate::key::Passphrase;
use crate::wrappers::format::format_opts_default;

extern "C" {
    fn rust_migrate_fs(
        fs_path: *const c_char,
        fs_opt_strs: c::bch_opt_strs,
        fs_opts: c::bch_opts,
        format_opts: c::format_opts,
        force: bool,
    ) -> i32;

    fn rust_migrate_superblock(
        dev_path: *const c_char,
        sb_offset: u64,
    ) -> i32;
}

fn migrate_usage() {
    print!("\
bcachefs migrate - migrate an existing filesystem to bcachefs
Usage: bcachefs migrate [OPTION]...

Options:
  -f fs                        Root of filesystem to migrate(s)
      --encrypted              Enable whole filesystem encryption (chacha20/poly1305)
      --no_passphrase          Don't encrypt master encryption key
  -F                           Force, even if metadata file already exists
  -h, --help                   Display this help and exit

Report bugs to <linux-bcachefs@vger.kernel.org>
");
}

pub fn cmd_migrate(argv: Vec<String>) -> Result<()> {
    let opt_flags = c::opt_flags::OPT_FORMAT as u32;

    let mut fs_path: Option<String> = None;
    let mut encrypted = false;
    let mut no_passphrase = false;
    let mut force = false;

    let mut fs_opts: c::bch_opts = Default::default();
    let mut deferred_opts: Vec<(usize, String)> = Vec::new();

    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];

        if arg.starts_with("--") && arg.len() > 2 {
            let opt_part = &arg[2..];
            let (raw_name, inline_val) = match opt_part.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (opt_part, None),
            };
            let name = raw_name.replace('-', "_");

            // Try bcachefs option table (OPT_FORMAT options)
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

                    unsafe { c::bch2_opt_set_by_id(&mut fs_opts, opt_id, v) };
                    i += 1;
                    continue;
                }
            }

            match name.as_str() {
                "encrypted" => encrypted = true,
                "no_passphrase" => no_passphrase = true,
                "help" => {
                    migrate_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            match arg.as_bytes()[1] {
                b'f' => {
                    i += 1;
                    if i >= argv.len() {
                        bail!("-f requires a value");
                    }
                    fs_path = Some(argv[i].clone());
                }
                b'F' => force = true,
                b'h' => {
                    migrate_usage();
                    return Ok(());
                }
                _ => bail!("unknown option: {}", arg),
            }

            i += 1;
            continue;
        }

        bail!("unexpected argument: {}", arg);
    }

    let fs_path = fs_path.ok_or_else(|| {
        migrate_usage();
        anyhow!("please specify a filesystem to migrate")
    })?;

    // Build format_opts with version detection
    let mut fmt_opts = format_opts_default();
    fmt_opts.encrypted = encrypted;

    // Handle encryption passphrase
    let passphrase: Option<Passphrase> = if encrypted && !no_passphrase {
        Some(Passphrase::new_from_prompt_twice()?)
    } else {
        None
    };

    if let Some(ref p) = passphrase {
        fmt_opts.passphrase = p.get().as_ptr() as *mut c_char;
    }

    let fs_path_cstr = CString::new(fs_path.as_str())?;

    // Build bch_opt_strs for deferred options
    let mut fs_opt_strs: c::bch_opt_strs = Default::default();
    for &(id, ref val) in &deferred_opts {
        let cstr = CString::new(val.as_str())?;
        let ptr = unsafe { libc::strdup(cstr.as_ptr()) };
        unsafe { fs_opt_strs.__bindgen_anon_1.by_id[id] = ptr };
    }

    let ret = unsafe {
        rust_migrate_fs(
            fs_path_cstr.as_ptr(),
            fs_opt_strs,
            fs_opts,
            fmt_opts,
            force,
        )
    };

    unsafe { c::bch2_opt_strs_free(&mut fs_opt_strs) };

    if ret != 0 {
        process::exit(1);
    }

    Ok(())
}

/// Migrate superblock to standard location
#[derive(Parser, Debug)]
#[command(about = "Create default superblock after migrating")]
pub struct MigrateSuperblockCli {
    /// Device to create superblock for
    #[arg(short = 'd', long = "dev")]
    device: String,

    /// Offset of existing superblock
    #[arg(short = 'o', long = "offset")]
    offset: u64,
}

pub fn cmd_migrate_superblock(argv: Vec<String>) -> Result<()> {
    let cli = MigrateSuperblockCli::parse_from(argv);

    let dev_cstr = CString::new(cli.device.as_str())?;

    let ret = unsafe {
        rust_migrate_superblock(dev_cstr.as_ptr(), cli.offset)
    };

    if ret != 0 {
        process::exit(ret);
    }

    Ok(())
}
