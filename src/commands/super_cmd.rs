// show-super: Display superblock contents for a bcachefs device.
//
// Key detail: we use ca->disk_sb.sb (per-device superblock) not c->disk_sb.sb
// (filesystem-level copy), because __copy_super deliberately omits fields like
// magic and layout from the fs-level copy. The per-device copy has everything.
//
// Field selection: by default prints ext + members + errors sections. The -f
// flag selects specific sections (or "all"), -F prints a single field with no
// header for scripting use (e.g. `bcachefs show-super -F version`).


use anyhow::{anyhow, Result};
use bch_bindgen::fs::FsExt;
use bcachefs_kernel::c;
use bcachefs_kernel::opt_set;
use bcachefs_kernel::sb::sb_field_type;
use clap::Parser;

use bcachefs_kernel::util::printbuf::Printbuf;

/// Print superblock information to stdout
#[derive(Parser, Debug)]
#[command(about = "Print superblock information to stdout", disable_help_flag = true)]
pub struct ShowSuperCli {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    _help: (),

    /// Superblock fields to print (comma-separated, or "all")
    #[arg(short = 'f', long = "fields")]
    fields: Option<String>,

    /// Print a single superblock field only (no header)
    #[arg(short = 'F', long = "field-only")]
    field_only: Option<String>,

    /// Print superblock layout
    #[arg(short, long)]
    layout: bool,

    /// Device path
    device: String,
}

fn cmd_show_super(cli: ShowSuperCli) -> Result<()> {

    let ext_bit = sb_field_type::ext.bit();
    let mut fields = ext_bit;
    let mut field_only = None;
    let mut print_default_fields = true;

    let field_type = |name: &str| sb_field_type::from_name(name)
        .ok_or_else(|| anyhow!("invalid superblock field: {name}"));

    if let Some(ref f) = cli.fields {
        fields = if f == "all" {
            !0
        } else {
            f.split(',').map(field_type).try_fold(0, |m, t| t.map(|t| m | t.bit()))?
        };
        print_default_fields = false;
    }

    if let Some(ref f) = cli.field_only {
        field_only = Some(field_type(f)?);
        print_default_fields = false;
    }

    let mut fs_opts = c::bch_opts::default();
    opt_set!(fs_opts, noexcl, 1);
    opt_set!(fs_opts, nochanges, 1);
    opt_set!(fs_opts, no_version_check, 1);
    opt_set!(fs_opts, nostart, 1);

    let fs = bcachefs_kernel::fs::Fs::open(&[std::path::PathBuf::from(&cli.device)], fs_opts)?;

    // Use the per-device superblock, not c->disk_sb.sb — the filesystem-level
    // copy omits fields like magic and layout that __copy_super doesn't transfer.
    for ca in fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_bch2_online_devs) {
        let sb = ca.disk_sb.sb();

        let mut fields = fields;
        if print_default_fields {
            if sb.field::<c::bch_sb_field_members_v2>().is_some() {
                fields |= sb_field_type::members_v2.bit();
            } else {
                fields |= sb_field_type::members_v1.bit();
            }
            fields |= sb_field_type::errors.bit();
        }

        let mut buf = Printbuf::new();
        buf.set_human_readable(true);
        crate::wrappers::sb_display::sb_to_text_with_names(&mut buf, Some(&fs), sb, cli.layout, fields, field_only);
        print!("{}", buf);
    }

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("show-super", "Print superblock information", ShowSuperCli, cmd_show_super);
