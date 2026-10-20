
use anyhow::Result;
use bcachefs_kernel::c;
use bcachefs_kernel::init::passes;
use bcachefs_kernel::opt_set;
use clap::Parser;

use crate::util::read_flag_list;

use bcachefs_kernel::util::printbuf::Printbuf;

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

fn cmd_recovery_pass(cli: RecoveryPassCli) -> Result<()> {

    let mut passes_to_set: u64 = 0;
    let mut passes_to_unset: u64 = 0;

    for s in &cli.set {
        passes_to_set |= read_flag_list(s, &passes::bch2_recovery_passes[..], "recovery pass")?;
    }

    for s in &cli.unset {
        passes_to_unset |= read_flag_list(s, &passes::bch2_recovery_passes[..], "recovery pass")?;
    }

    passes_to_set = passes::passes_to_stable(passes_to_set);
    passes_to_unset = passes::passes_to_stable(passes_to_unset);

    let devs: Vec<std::path::PathBuf> = cli.devices.iter().map(|d| d.as_str().into()).collect();

    let mut fs_opts = c::bch_opts::default();
    opt_set!(fs_opts, nostart, 1);
    opt_set!(fs_opts, will_not_start, 1);

    let fs = crate::device_scan::open_scan(&devs, fs_opts)?;

    let scheduled = {
        let mut sb = fs.sb_lock();

        let ext_u64s = (std::mem::size_of::<c::bch_sb_field_ext>() / std::mem::size_of::<u64>()) as u32;
        let ext = sb.field_get_minsize::<c::bch_sb_field_ext>(ext_u64s)
            .ok_or_else(|| anyhow::anyhow!("Error getting sb_field_ext"))?;

        let r = &mut ext.recovery_passes_required[0];
        r.set((r.get() & !passes_to_unset) | passes_to_set);
        let scheduled = r.get();

        if passes_to_set != 0 || passes_to_unset != 0 {
            fs.write_super_ret(&mut sb)
                .map_err(|e| anyhow::anyhow!("error writing superblock: {e}"))?;
        }
        scheduled
    };

    let mut buf = Printbuf::new();
    write!(buf, "Scheduled recovery passes: ");
    if scheduled != 0 {
        passes::prt_stable(&mut buf, scheduled);
    } else {
        write!(buf, "(none)");
    }
    println!("{}", buf);

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("recovery-pass", "Manage recovery passes", RecoveryPassCli, cmd_recovery_pass);
