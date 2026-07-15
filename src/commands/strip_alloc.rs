use std::path::PathBuf;
use bch_bindgen::c;
use bch_bindgen::opt_set;
use anyhow::bail;
use clap::Parser;
use crate::wrappers::bch_err_str;

#[derive(Parser, Debug)]
#[command(about = "Strip alloc info for read-only use")]
pub struct Cli {
    /// Device(s) to strip
    #[arg(required = true)]
    devices: Vec<PathBuf>,
}

fn cmd_strip_alloc(cli: Cli) -> anyhow::Result<()> {
    let devs = cli.devices;

    loop {
        let mut opts: c::bch_opts = Default::default();
        opt_set!(opts, nostart, 1);

        let fs = crate::device_scan::open_scan(&devs, opts)
            .map_err(|e| anyhow::anyhow!("Error opening filesystem: {}", e))?;

        if unsafe { (*fs.raw).sb.clean } == 0 {
            println!("Filesystem not clean, running recovery");
            let ret = unsafe { c::bch2_fs_start(fs.raw) };
            if ret != 0 {
                bail!("Error starting filesystem: {}", bch_err_str(ret));
            }
            drop(fs);
            continue;
        }

        // Check total capacity — reconstruction is limited to ~1TB
        let mut capacity: u64 = 0;
        for dev in 0..fs.nr_devices() {
            if let Some(ca) = fs.dev_get(dev) {
                capacity += (ca.mi.nbuckets * ca.mi.bucket_size as u64) << 9;
            }
        }
        if capacity > 1u64 << 40 {
            bail!("capacity too large for alloc info reconstruction");
        }

        println!("Stripping alloc info from {}", devs[0].display());
        unsafe { c::rust_strip_alloc_do(fs.raw) };
        return Ok(());
    }
}

pub const CMD: super::CmdDef = typed_cmd!("strip-alloc", "Strip alloc info for read-only use", Cli, cmd_strip_alloc);
