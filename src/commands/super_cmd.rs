use std::ffi::CString;

use anyhow::{anyhow, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::Parser;

use crate::wrappers::printbuf::Printbuf;

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

pub fn cmd_show_super(argv: Vec<String>) -> Result<()> {
    let cli = ShowSuperCli::parse_from(argv);

    let ext_bit = 1u32 << c::bch_sb_field_type::BCH_SB_FIELD_ext as u32;
    let mut fields = ext_bit;
    let mut field_only: i32 = -1;
    let mut print_default_fields = true;

    if let Some(ref f) = cli.fields {
        if f == "all" {
            fields = !0;
        } else {
            let c_str = CString::new(f.as_str())?;
            let v = unsafe {
                c::bch2_read_flag_list(c_str.as_ptr(), c::bch2_sb_fields.as_ptr())
            };
            if v == u64::MAX {
                return Err(anyhow!("invalid superblock field: {}", f));
            }
            fields = v as u32;
        }
        print_default_fields = false;
    }

    if let Some(ref f) = cli.field_only {
        let c_str = CString::new(f.as_str())?;
        let v = unsafe {
            c::match_string(c::bch2_sb_fields.as_ptr(), usize::MAX, c_str.as_ptr())
        };
        if v < 0 {
            return Err(anyhow!("invalid superblock field: {}", f));
        }
        field_only = v as i32;
        print_default_fields = false;
    }

    let mut fs_opts = bcachefs::bch_opts::default();
    opt_set!(fs_opts, noexcl, 1);
    opt_set!(fs_opts, nochanges, 1);
    opt_set!(fs_opts, no_version_check, 1);
    opt_set!(fs_opts, nostart, 1);

    let fs = Fs::open(&[std::path::PathBuf::from(&cli.device)], fs_opts)?;

    if print_default_fields {
        if fs.sb().field::<c::bch_sb_field_members_v2>().is_some() {
            fields |= 1 << c::bch_sb_field_type::BCH_SB_FIELD_members_v2 as u32;
        } else {
            fields |= 1 << c::bch_sb_field_type::BCH_SB_FIELD_members_v1 as u32;
        }
        fields |= 1 << c::bch_sb_field_type::BCH_SB_FIELD_errors as u32;
    }

    let mut buf = Printbuf::new();
    buf.set_human_readable(true);
    buf.sb_to_text_with_names(fs.raw, fs.sb(), cli.layout, fields, field_only);

    print!("{}", buf);

    Ok(())
}
