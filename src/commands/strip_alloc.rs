use std::path::PathBuf;
use bch_bindgen::c;
use bch_bindgen::opt_set;
use bch_bindgen::fs::Fs;
use anyhow::bail;
use crate::wrappers::bch_err_str;

fn strip_alloc_usage() {
    println!("bcachefs strip-alloc - remove alloc info and journal from a filesystem");
    println!("Usage: bcachefs strip-alloc [OPTION]... <devices>\n");
    println!("Removes metadata unneeded for running in read-only mode");
    println!("Alloc info and journal will be recreated on first RW mount\n");
    println!("Options:");
    println!("  -h, --help              Display this help and exit\n");
    println!("Report bugs to <linux-bcachefs@vger.kernel.org>");
}

pub fn cmd_strip_alloc(argv: Vec<String>) -> anyhow::Result<()> {
    let mut devs: Vec<PathBuf> = Vec::new();

    let mut args = argv.iter().skip(1); // skip "strip-alloc"
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                strip_alloc_usage();
                return Ok(());
            }
            s if s.starts_with('-') => {
                bail!("Unknown option: {}", s);
            }
            _ => {
                devs.push(PathBuf::from(arg));
            }
        }
    }

    if devs.is_empty() {
        strip_alloc_usage();
        bail!("Please supply device(s)");
    }

    loop {
        let mut opts: c::bch_opts = Default::default();
        opt_set!(opts, nostart, 1);

        let fs = Fs::open(&devs, opts)
            .map_err(|e| anyhow::anyhow!("Error opening filesystem: {}", e))?;

        let ret = unsafe { c::rust_strip_alloc_check(fs.raw) };
        match ret {
            0 => {
                println!("Stripping alloc info from {}", devs[0].display());
                unsafe { c::rust_strip_alloc_do(fs.raw) };
                return Ok(());
            }
            1 => {
                println!("Filesystem not clean, running recovery");
                let ret = unsafe { c::bch2_fs_start(fs.raw) };
                if ret != 0 {
                    bail!("Error starting filesystem: {}", bch_err_str(ret));
                }
                // exit and reopen — drop triggers bch2_fs_exit
                drop(fs);
                continue;
            }
            _ => {
                bail!("capacity too large for alloc info reconstruction");
            }
        }
    }
}
