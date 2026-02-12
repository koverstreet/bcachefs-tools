use std::ffi::CStr;
use std::ops::ControlFlow;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Parser;

use bch_bindgen::bkey::BkeySC;
use bch_bindgen::btree::*;
use bch_bindgen::accounting;
use bch_bindgen::c;
use bch_bindgen::data::extents::bkey_ptrs_sc;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use bch_bindgen::POS_MIN;

use crate::qcow2::{self, Qcow2Image, Ranges, range_add, ranges_sort};
use crate::wrappers::super_io::vstruct_bytes_sb;

extern "C" {
    fn rust_sanitize_journal(c: *mut c::bch_fs, buf: *mut u8, len: usize,
                             sanitize_filenames: bool);
    fn rust_sanitize_btree(c: *mut c::bch_fs, buf: *mut u8, len: usize,
                           sanitize_filenames: bool);
}

// ---- Dump CLI ----

/// Dump filesystem metadata to a qcow2 image
#[derive(Parser, Debug)]
#[command(about = "Dump filesystem metadata to a qcow2 image")]
pub struct DumpCli {
    /// Output filename (without .qcow2 extension)
    #[arg(short = 'o')]
    output: String,

    /// Force; overwrite existing files
    #[arg(short = 'f', long)]
    force: bool,

    /// Sanitize inline data and optionally filenames (data or filenames)
    #[arg(short = 's', long, num_args = 0..=1, default_missing_value = "data")]
    sanitize: Option<String>,

    /// Don't dump entire journal, just dirty entries
    #[arg(long)]
    nojournal: bool,

    /// Open devices without O_EXCL
    #[arg(long)]
    noexcl: bool,

    /// Verbose output
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Devices to dump
    #[arg(required = true)]
    devices: Vec<String>,
}

// ---- Undump CLI ----

/// Convert a qcow2 image back to a raw device image
#[derive(Parser, Debug)]
#[command(about = "Convert qcow2 dump files back to raw device images")]
pub struct UndumpCli {
    /// Overwrite existing output files
    #[arg(short = 'f', long = "force")]
    force: bool,

    /// qcow2 files to convert
    #[arg(required = true)]
    files: Vec<String>,
}

pub fn cmd_undump(argv: Vec<String>) -> Result<()> {
    let cli = UndumpCli::parse_from(argv);
    let suffix = ".qcow2";

    struct FileEntry {
        input: String,
        output: String,
    }

    let mut entries = Vec::new();

    for f in &cli.files {
        if !f.ends_with(suffix) {
            return Err(anyhow!("{} not a qcow2 image?", f));
        }

        let output = f[..f.len() - suffix.len()].to_string();

        if !cli.force && Path::new(&output).exists() {
            return Err(anyhow!("{} already exists", output));
        }

        entries.push(FileEntry { input: f.clone(), output });
    }

    for e in &entries {
        let infile = std::fs::File::open(&e.input)
            .map_err(|err| anyhow!("{}: {}", e.input, err))?;

        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create(true);
        if !cli.force {
            open_opts.create_new(true);
        } else {
            open_opts.truncate(false);
        }

        let outfile = open_opts.open(&e.output)
            .map_err(|err| anyhow!("{}: {}", e.output, err))?;

        qcow2::qcow2_to_raw(infile.as_raw_fd(), outfile.as_raw_fd())?;
    }

    Ok(())
}

// ---- Dump implementation ----

struct DumpDev {
    sb:      Ranges,
    journal: Ranges,
    btree:   Ranges,
}

impl DumpDev {
    fn new() -> Self {
        DumpDev {
            sb:      Vec::new(),
            journal: Vec::new(),
            btree:   Vec::new(),
        }
    }
}

fn dump_node(fs: &Fs, devs: &mut [DumpDev], k: BkeySC<'_>, btree_node_size: u64) {
    let val = k.v();
    for ptr in bkey_ptrs_sc(&val) {
        let dev = ptr.dev() as usize;
        if dev < devs.len() && fs.dev_exists(dev as u32) {
            range_add(&mut devs[dev].btree, ptr.offset() << 9, btree_node_size);
        }
    }
}

fn get_sb_journal(fs: &Fs, ca: &c::bch_dev, entire_journal: bool, d: &mut DumpDev) {
    let sb = unsafe { &*ca.disk_sb.sb };
    let sb_bytes = vstruct_bytes_sb(sb) as u64;
    let bucket_bytes = (ca.mi.bucket_size as u64) << 9;

    // Superblock layout
    range_add(&mut d.sb,
              (c::BCH_SB_LAYOUT_SECTOR as u64) << 9,
              std::mem::size_of::<c::bch_sb_layout>() as u64);

    // All superblock copies
    for i in 0..sb.layout.nr_superblocks as usize {
        let offset = u64::from_le(sb.layout.sb_offset[i]);
        range_add(&mut d.sb, offset << 9, sb_bytes);
    }

    // Journal buckets
    let last_seq_ondisk = unsafe { (*fs.raw).journal.last_seq_ondisk };
    for i in 0..ca.journal.nr as usize {
        let seq = unsafe { *ca.journal.bucket_seq.add(i) };
        if entire_journal || seq >= last_seq_ondisk {
            let bucket = unsafe { *ca.journal.buckets.add(i) };
            range_add(&mut d.journal, bucket_bytes * bucket, bucket_bytes);
        }
    }
}

fn write_sanitized_ranges(
    img: &mut Qcow2Image,
    fs_raw: *mut c::bch_fs,
    ranges: &mut Ranges,
    bucket_bytes: u64,
    sanitize_filenames: bool,
    sanitize_fn: unsafe extern "C" fn(*mut c::bch_fs, *mut u8, usize, bool),
) -> Result<()> {
    ranges_sort(ranges);
    let mut buf = vec![0u8; bucket_bytes as usize];

    for r in ranges.iter() {
        let len = (r.end - r.start) as usize;
        assert!(len <= bucket_bytes as usize);

        qcow2::pread_exact(img.infd(), &mut buf[..len], r.start)?;
        unsafe { sanitize_fn(fs_raw, buf.as_mut_ptr(), len, sanitize_filenames) };
        img.write_buf(&buf[..len], r.start)?;
    }

    Ok(())
}

fn write_dev_image(
    fs: &Fs,
    ca: &c::bch_dev,
    path: &str,
    force: bool,
    sanitize: bool,
    sanitize_filenames: bool,
    block_size: u32,
    d: &mut DumpDev,
) -> Result<()> {
    let mut open_opts = std::fs::OpenOptions::new();
    open_opts.write(true).create(true);
    if !force {
        open_opts.create_new(true);
    } else {
        open_opts.truncate(true);
    }

    let outfile = open_opts.open(path)
        .map_err(|e| anyhow!("{}: {}", path, e))?;

    let infd = unsafe { (*ca.disk_sb.bdev).bd_fd };
    let mut img = Qcow2Image::new(infd, outfile.as_raw_fd(), block_size)?;

    img.write_ranges(&mut d.sb)?;

    if !sanitize {
        img.write_ranges(&mut d.journal)?;
        img.write_ranges(&mut d.btree)?;
    } else {
        let bucket_bytes = (ca.mi.bucket_size as u64) << 9;
        write_sanitized_ranges(
            &mut img, fs.raw, &mut d.journal, bucket_bytes,
            sanitize_filenames, rust_sanitize_journal,
        )?;
        write_sanitized_ranges(
            &mut img, fs.raw, &mut d.btree, bucket_bytes,
            sanitize_filenames, rust_sanitize_btree,
        )?;
    }

    img.finish()
}

fn dump_fs(fs: &Fs, cli: &DumpCli, sanitize: bool, sanitize_filenames: bool) -> Result<()> {
    if sanitize_filenames {
        println!("Sanitizing filenames and inline data extents");
    } else if sanitize {
        println!("Sanitizing inline data extents");
    }

    let nr_devices = fs.nr_devices() as usize;
    let mut devs: Vec<DumpDev> = (0..nr_devices).map(|_| DumpDev::new()).collect();

    let entire_journal = !cli.nojournal;
    let btree_node_size = unsafe { (*fs.raw).opts.btree_node_size as u64 };
    let block_size = unsafe { (*fs.raw).opts.block_size as u32 };

    let mut nr_online = 0u32;
    let mut bucket_err: Option<String> = None;
    let _ = fs.for_each_online_member(|ca| {
        if sanitize && (ca.mi.bucket_size as u32) % (block_size >> 9) != 0 {
            let name = unsafe { CStr::from_ptr(ca.name.as_ptr()) };
            bucket_err = Some(format!("{} has unaligned buckets, cannot sanitize",
                                      name.to_str().unwrap_or("?")));
            return ControlFlow::Break(());
        }

        get_sb_journal(fs, ca, entire_journal, &mut devs[ca.dev_idx as usize]);
        nr_online += 1;
        ControlFlow::Continue(())
    });
    if let Some(err) = bucket_err {
        return Err(anyhow!("{}", err));
    }

    // Walk all btree types (including dynamic) to collect metadata locations
    for id in 0..fs.btree_id_nr_alive() {
        let trans = BtreeTrans::new(fs);
        let mut node_iter = BtreeNodeIter::new(
            &trans,
            id,
            POS_MIN,
            0, // locks_want
            1, // depth
            BtreeIterFlags::PREFETCH,
        );

        node_iter.for_each(&trans, |b| {
            let _ = b.for_each_key(|k| {
                dump_node(fs, &mut devs, k, btree_node_size);
                ControlFlow::Continue(())
            });
            ControlFlow::Continue(())
        }).map_err(|e| anyhow!("error walking btree {}: {}",
            accounting::btree_id_str(id), e))?;

        // Also dump the root node itself
        if let Some(b) = fs.btree_id_root(id) {
            if !b.is_fake() {
                let k = BkeySC::from(&b.key);
                dump_node(fs, &mut devs, k, btree_node_size);
            }
        }
    }

    // Write qcow2 image(s)
    let mut write_err: Option<anyhow::Error> = None;
    let _ = fs.for_each_online_member(|ca| {
        let dev_idx = ca.dev_idx;
        let path = if nr_online > 1 {
            format!("{}.{}.qcow2", cli.output, dev_idx)
        } else {
            format!("{}.qcow2", cli.output)
        };

        match write_dev_image(fs, ca, &path, cli.force, sanitize, sanitize_filenames,
                              block_size, &mut devs[dev_idx as usize]) {
            Ok(()) => ControlFlow::Continue(()),
            Err(e) => {
                write_err = Some(e);
                ControlFlow::Break(())
            }
        }
    });
    if let Some(e) = write_err {
        return Err(e);
    }

    Ok(())
}

pub fn cmd_dump(argv: Vec<String>) -> Result<()> {
    let cli = DumpCli::parse_from(argv);

    let (sanitize, sanitize_filenames) = match cli.sanitize.as_deref() {
        None => (false, false),
        Some("data") => (true, false),
        Some("filenames") => (true, true),
        Some(other) => return Err(anyhow!("Bad sanitize option: {}", other)),
    };

    // Open filesystem in read-only, no-recovery mode
    let devs: Vec<PathBuf> = cli.devices.iter().map(PathBuf::from).collect();
    let mut opts: c::bch_opts = Default::default();
    opt_set!(opts, direct_io, 0);
    opt_set!(opts, read_only, 1);
    opt_set!(opts, nochanges, 1);
    opt_set!(opts, norecovery, 1);
    opt_set!(opts, degraded, c::bch_degraded_actions::BCH_DEGRADED_very as u8);
    opt_set!(opts, errors, c::bch_error_actions::BCH_ON_ERROR_continue as u8);
    opt_set!(opts, fix_errors, c::fsck_err_opts::FSCK_FIX_no as u8);

    if cli.noexcl {
        opt_set!(opts, noexcl, 1);
    }
    if cli.verbose {
        opt_set!(opts, verbose, 1);
    }

    let fs = Fs::open(&devs, opts)?;
    dump_fs(&fs, &cli, sanitize, sanitize_filenames)
}
