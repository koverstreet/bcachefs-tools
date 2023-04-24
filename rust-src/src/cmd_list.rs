use atty::Stream;
use log::{error};
use bch_bindgen::bcachefs;
use bch_bindgen::opt_set;
use bch_bindgen::fs::Fs;
use bch_bindgen::bkey::BkeySC;
use bch_bindgen::btree::BtreeTrans;
use bch_bindgen::btree::BtreeIter;
use bch_bindgen::btree::BtreeNodeIter;
use bch_bindgen::btree::BtreeIterFlags;
use clap::Parser;
use std::ffi::{CStr, OsStr, c_int, c_char};
use std::os::unix::ffi::OsStrExt;

fn list_keys(fs: &Fs, opt: Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeIter::new(&trans, opt.btree, opt.start,
        BtreeIterFlags::ALL_SNAPSHOTS|
        BtreeIterFlags::PREFETCH);

    while let Some(k) = iter.peek_and_restart()? {
        if k.k.p > opt.end {
            break;
        }

        println!("{}", k.to_text(fs));
        iter.advance();
    }

    Ok(())
}

fn list_btree_formats(fs: &Fs, opt: Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeNodeIter::new(&trans, opt.btree, opt.start,
        0, opt.level,
        BtreeIterFlags::PREFETCH);

    while let Some(b) = iter.peek_and_restart()? {
        if b.key.k.p > opt.end {
            break;
        }

        println!("{}", b.to_text(fs));
        iter.advance();
    }

    Ok(())
}

fn list_btree_nodes(fs: &Fs, opt: Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeNodeIter::new(&trans, opt.btree, opt.start,
        0, opt.level,
        BtreeIterFlags::PREFETCH);

    while let Some(b) = iter.peek_and_restart()? {
        if b.key.k.p > opt.end {
            break;
        }

        println!("{}", BkeySC::from(&b.key).to_text(fs));
        iter.advance();
    }

    Ok(())
}

fn list_nodes_ondisk(fs: &Fs, opt: Cli) -> anyhow::Result<()> {
    let trans = BtreeTrans::new(fs);
    let mut iter = BtreeNodeIter::new(&trans, opt.btree, opt.start,
        0, opt.level,
        BtreeIterFlags::PREFETCH);

    while let Some(b) = iter.peek_and_restart()? {
        if b.key.k.p > opt.end {
            break;
        }

        println!("{}", b.ondisk_to_text(fs));
        iter.advance();
    }

    Ok(())
}

#[derive(Clone, clap::ValueEnum)]
enum Mode {
    Keys,
    Formats,
    Nodes,
    NodesOndisk,
}

#[derive(Parser)]
struct Cli {
    /// Btree to list from
    #[arg(short, long, default_value_t=bcachefs::btree_id::BTREE_ID_extents)]
    btree:      bcachefs::btree_id,

    /// Btree depth to descend to (0 == leaves)
    #[arg(short, long, default_value_t=0)]
    level:      u32,

    /// Start position to list from
    #[arg(short, long, default_value="POS_MIN")]
    start:      bcachefs::bpos,

    /// End position
    #[arg(short, long, default_value="SPOS_MAX")]
    end:        bcachefs::bpos,

    #[arg(short, long, default_value="keys")]
    mode:       Mode,

    /// Check (fsck) the filesystem first
    #[arg(short, long, default_value_t=false)]
    fsck:       bool,

    /// Force color on/off. Default: autodetect tty
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=atty::is(Stream::Stdout))]
    colorize:   bool,
   
    /// Verbose mode
    #[arg(short, long)]
    verbose:    bool,

    #[arg(required(true))]
    devices:    Vec<std::path::PathBuf>,
}

fn cmd_list_inner(opt: Cli) -> anyhow::Result<()> {
    let mut fs_opts: bcachefs::bch_opts = Default::default();

    opt_set!(fs_opts, nochanges,        1);
    opt_set!(fs_opts, norecovery,       1);
    opt_set!(fs_opts, degraded,         1);
    opt_set!(fs_opts, errors,           bcachefs::bch_error_actions::BCH_ON_ERROR_continue as u8);

    if opt.fsck {
        opt_set!(fs_opts, fix_errors,   bcachefs::fsck_err_opts::FSCK_OPT_YES as u8);
        opt_set!(fs_opts, norecovery,   0);
    }

    if opt.verbose {
        opt_set!(fs_opts, verbose,      1);
    }

    let fs = Fs::open(&opt.devices, fs_opts)?;

    match opt.mode {
        Mode::Keys          => list_keys(&fs, opt),
        Mode::Formats       => list_btree_formats(&fs, opt),
        Mode::Nodes         => list_btree_nodes(&fs, opt),
        Mode::NodesOndisk   => list_nodes_ondisk(&fs, opt),
    }
}

#[no_mangle]
pub extern "C" fn cmd_list(argc: c_int, argv: *const *const c_char) {
    let argv: Vec<_> = (0..argc)
        .map(|i| unsafe { CStr::from_ptr(*argv.add(i as usize)) })
        .map(|i| OsStr::from_bytes(i.to_bytes()))
        .collect();

    let opt = Cli::parse_from(argv);
    colored::control::set_override(opt.colorize);
    if let Err(e) = cmd_list_inner(opt) {
        error!("Fatal error: {}", e);
    }
}
