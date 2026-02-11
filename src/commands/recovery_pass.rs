use std::ffi::CString;
use std::fmt::Write;

use anyhow::{bail, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::Parser;

use crate::wrappers::printbuf::Printbuf;

/// List and manage scheduled recovery passes
#[derive(Parser, Debug)]
#[command(about = "List and manage scheduled recovery passes")]
pub struct RecoveryPassCli {
    /// Schedule a recovery pass
    #[arg(short = 's', long = "set")]
    set: Vec<String>,

    /// Deschedule a recovery pass
    #[arg(short = 'u', long = "unset")]
    unset: Vec<String>,

    /// Device path(s)
    #[arg(required = true)]
    devices: Vec<String>,
}

pub fn cmd_recovery_pass(argv: Vec<String>) -> Result<()> {
    let cli = RecoveryPassCli::parse_from(argv);

    let mut passes_to_set: u64 = 0;
    let mut passes_to_unset: u64 = 0;

    for s in &cli.set {
        let c_str = CString::new(s.as_str())?;
        passes_to_set |= unsafe {
            c::read_flag_list_or_die(
                c_str.as_ptr().cast_mut(),
                c::bch2_recovery_passes.as_ptr(),
                c"recovery pass".as_ptr(),
            )
        };
    }

    for s in &cli.unset {
        let c_str = CString::new(s.as_str())?;
        passes_to_unset |= unsafe {
            c::read_flag_list_or_die(
                c_str.as_ptr().cast_mut(),
                c::bch2_recovery_passes.as_ptr(),
                c"recovery pass".as_ptr(),
            )
        };
    }

    passes_to_set = unsafe { c::bch2_recovery_passes_to_stable(passes_to_set) };
    passes_to_unset = unsafe { c::bch2_recovery_passes_to_stable(passes_to_unset) };

    let devs: Vec<std::path::PathBuf> = cli.devices.iter().map(|d| d.as_str().into()).collect();

    let mut fs_opts = bcachefs::bch_opts::default();
    opt_set!(fs_opts, nostart, 1);

    let fs = Fs::open(&devs, fs_opts)?;

    unsafe {
        let _sb_lock = crate::wrappers::sb_lock(fs.raw);

        let ext = c::bch2_sb_field_get_minsize_id(
            &mut (*fs.raw).disk_sb,
            c::bch_sb_field_type::BCH_SB_FIELD_ext,
            (std::mem::size_of::<c::bch_sb_field_ext>() / std::mem::size_of::<u64>()) as u32,
        ) as *mut c::bch_sb_field_ext;

        if ext.is_null() {
            bail!("Error getting sb_field_ext");
        }

        let mut scheduled = u64::from_le((*ext).recovery_passes_required[0]);

        if passes_to_set != 0 || passes_to_unset != 0 {
            (*ext).recovery_passes_required[0] &= !passes_to_unset.to_le();
            (*ext).recovery_passes_required[0] |= passes_to_set.to_le();
            scheduled = u64::from_le((*ext).recovery_passes_required[0]);
            fs.write_super();
        }

        drop(_sb_lock);

        let mut buf = Printbuf::new();
        let _ = write!(buf, "Scheduled recovery passes: ");

        if scheduled != 0 {
            buf.prt_bitflags(
                c::bch2_recovery_passes.as_ptr(),
                c::bch2_recovery_passes_from_stable(scheduled),
            );
        } else {
            let _ = write!(buf, "(none)");
        }

        println!("{}", buf);
    }

    Ok(())
}
