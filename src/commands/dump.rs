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
    fn rust_jset_decrypt(c: *mut c::bch_fs, j: *mut u8) -> i32;
    fn rust_bset_decrypt(c: *mut c::bch_fs, i: *mut u8, offset: u32) -> i32;
}

/// First 8 bytes of the superblock UUID interpreted as a little-endian u64.
fn sb_magic(sb: &c::bch_sb) -> u64 {
    u64::from_le_bytes(sb.uuid.b[..8].try_into().unwrap())
}

/// Journal set magic: sb_magic XOR JSET_MAGIC constant.
fn jset_magic(sb: &c::bch_sb) -> u64 {
    sb_magic(sb) ^ 0x245235c1a3625032
}

/// Btree set magic: sb_magic XOR BSET_MAGIC constant.
fn bset_magic(sb: &c::bch_sb) -> u64 {
    sb_magic(sb) ^ 0x90135c78b99e07f5
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

// ---- Sanitize implementation ----

// On-disk struct sizes (all __packed, little-endian x86_64)
const JSET_HDR: usize = 56;            // offsetof(jset, _data)
const JSET_ENTRY_HDR: usize = 8;       // offsetof(jset_entry, start)
const BSET_HDR: usize = 24;            // offsetof(bset, _data)
const BTREE_NODE_KEYS: usize = 136;    // offsetof(btree_node, keys)
const BNE_KEYS: usize = 16;            // offsetof(btree_node_entry, keys)
const BKEY_U64S: usize = 5;            // sizeof(bkey) / 8

fn read_le64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn read_le32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn read_le16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn csum_type_is_encryption(csum_type: u32) -> bool {
    csum_type == 3 || csum_type == 4 // chacha20_poly1305_{80,128}
}

/// Zero the csum field (16 bytes at `csum_off`) and clear bits 0-3 of
/// the le32 flags field at `flags_off`.
fn clear_csum(buf: &mut [u8], csum_off: usize, flags_off: usize) {
    buf[csum_off..csum_off + 16].fill(0);
    let flags = read_le32(buf, flags_off);
    buf[flags_off..flags_off + 4].copy_from_slice(&(flags & !0xf).to_le_bytes());
}

/// Round `bytes` up to the block alignment determined by `block_bits`,
/// matching C `vstruct_sectors(s, block_bits) << 9`.
fn vstruct_aligned_bytes(bytes: usize, block_bits: usize) -> usize {
    let align = 512 << block_bits;
    bytes.div_ceil(align) * align
}

/// Sanitize a bkey value region in-place. Returns true if modified.
///
/// On-disk value layout (bch_val = 0 bytes):
///  - inline_data:          data at offset 0 — zero all
///  - indirect_inline_data: refcount(8), data — zero data
///  - dirent:               d_inum(8), d_type(1), d_name — fill with 'X'
fn sanitize_val(val: &mut [u8], key_type: u8, sanitize_filenames: bool) -> bool {
    use c::bch_bkey_type::*;
    let t = key_type as u32;

    if t == KEY_TYPE_inline_data as u32 {
        val.fill(0);
        true
    } else if t == KEY_TYPE_indirect_inline_data as u32 {
        if val.len() > 8 {
            val[8..].fill(0);
        }
        true
    } else if t == KEY_TYPE_dirent as u32 && sanitize_filenames {
        if val.len() > 9 {
            val[9..].fill(b'X');
        }
        true
    } else {
        false
    }
}

/// Walk unpacked bkey_i entries in a jset_entry data region and sanitize.
fn sanitize_journal_keys(
    buf: &mut [u8],
    start: usize,
    end: usize,
    sanitize_filenames: bool,
) -> bool {
    let mut modified = false;
    let mut pos = start;

    while pos + 3 <= end {
        let key_u64s = buf[pos] as usize;
        if key_u64s == 0 {
            break;
        }

        let key_bytes = key_u64s * 8;
        if pos + key_bytes > end {
            break;
        }

        let key_type = buf[pos + 2];
        let val_off = BKEY_U64S * 8;
        if val_off < key_bytes
            && sanitize_val(&mut buf[pos + val_off..pos + key_bytes],
                            key_type, sanitize_filenames) {
            modified = true;
        }

        pos += key_bytes;
    }

    modified
}

/// Sanitize a journal buffer in-place: walk jset entries, handle encryption,
/// zero inline data, optionally scramble filenames, clear checksums.
fn sanitize_journal(fs_raw: *mut c::bch_fs, buf: &mut [u8], sanitize_filenames: bool) {
    let jset_magic = jset_magic(unsafe { &*(*fs_raw).disk_sb.sb });
    let block_bits = unsafe { (*fs_raw).block_bits } as usize;

    let mut pos = 0;
    while pos + JSET_HDR <= buf.len() {
        if read_le64(buf, pos + 16) != jset_magic {
            break;
        }

        let u64s = read_le32(buf, pos + 40) as usize;
        let vstruct_bytes = JSET_HDR + u64s * 8;
        if vstruct_bytes > buf.len() - pos {
            break;
        }

        let csum_type = read_le32(buf, pos + 36) & 0xf;
        let mut modified = false;

        if csum_type_is_encryption(csum_type) {
            if !unsafe { (*fs_raw).chacha20_key_set } {
                eprintln!("found encrypted journal entry on non-encrypted filesystem");
                return;
            }

            let ret = unsafe { rust_jset_decrypt(fs_raw, buf.as_mut_ptr().add(pos)) };
            if ret != 0 {
                eprintln!("error decrypting journal entry: {}", ret);
                return;
            }
            modified = true;
        }

        // Walk jset entries: each is 8-byte header + u64s * 8 data
        let data_end = (pos + vstruct_bytes).min(buf.len());
        let mut entry_pos = pos + JSET_HDR;

        while entry_pos + JSET_ENTRY_HDR <= data_end {
            let entry_u64s = read_le16(buf, entry_pos) as usize;
            let entry_end = entry_pos + JSET_ENTRY_HDR + entry_u64s * 8;
            if entry_end > data_end {
                break;
            }

            // jset_entry_is_key: btree_keys(0), btree_root(1), write_buffer_keys(11)
            let entry_type = buf[entry_pos + 4];
            if (entry_type == 0 || entry_type == 1 || entry_type == 11)
                && sanitize_journal_keys(buf, entry_pos + JSET_ENTRY_HDR,
                                         entry_end, sanitize_filenames) {
                modified = true;
            }

            entry_pos = entry_end;
        }

        if modified {
            clear_csum(buf, pos, pos + 36);
        }

        pos += vstruct_aligned_bytes(vstruct_bytes, block_bits)
            .min(buf.len() - pos);
    }
}

/// Sanitize a btree node buffer in-place: walk bset entries, handle
/// encryption, zero inline data, optionally scramble filenames.
fn sanitize_btree(fs_raw: *mut c::bch_fs, buf: &mut [u8], sanitize_filenames: bool) {
    let bset_magic = bset_magic(unsafe { &*(*fs_raw).disk_sb.sb });
    let block_bits = unsafe { (*fs_raw).block_bits } as usize;

    let mut first = true;
    let mut seq: u64 = 0;
    let mut format_key_u64s: usize = BKEY_U64S;
    let mut pos = 0;
    let mut bset_byte_offset: usize = 0;

    while pos < buf.len() {
        let (bset_off, data_off, vstruct_bytes);

        if first {
            if pos + BTREE_NODE_KEYS + BSET_HDR > buf.len() {
                break;
            }
            if read_le64(buf, pos + 16) != bset_magic {
                break;
            }

            bset_off = pos + BTREE_NODE_KEYS;
            data_off = pos + BTREE_NODE_KEYS + BSET_HDR;
            format_key_u64s = buf[pos + 80] as usize; // btree_node.format.key_u64s
            seq = read_le64(buf, bset_off);            // bset.seq
            let u64s = read_le16(buf, bset_off + 22) as usize;
            vstruct_bytes = BTREE_NODE_KEYS + BSET_HDR + u64s * 8;
        } else {
            if pos + BNE_KEYS + BSET_HDR > buf.len() {
                break;
            }

            bset_off = pos + BNE_KEYS;
            data_off = pos + BNE_KEYS + BSET_HDR;
            if read_le64(buf, bset_off) != seq {
                break;
            }
            let u64s = read_le16(buf, bset_off + 22) as usize;
            vstruct_bytes = BNE_KEYS + BSET_HDR + u64s * 8;
        }

        if pos + vstruct_bytes > buf.len() {
            break;
        }

        let csum_type = read_le32(buf, bset_off + 16) & 0xf;
        let mut modified = false;

        if csum_type_is_encryption(csum_type) {
            if !unsafe { (*fs_raw).chacha20_key_set } {
                eprintln!("found encrypted btree node on non-encrypted filesystem");
                return;
            }

            let ret = unsafe {
                rust_bset_decrypt(fs_raw, buf.as_mut_ptr().add(bset_off),
                                  bset_byte_offset as u32)
            };
            if ret != 0 {
                eprintln!("error decrypting btree node: {}", ret);
                return;
            }
            modified = true;
        }

        // Walk packed keys in bset data region
        let u64s = read_le16(buf, bset_off + 22) as usize;
        let key_end = (data_off + u64s * 8).min(buf.len());

        let mut key_pos = data_off;
        while key_pos + 3 <= key_end {
            let key_u64s = buf[key_pos] as usize;
            if key_u64s == 0 {
                break;
            }

            let key_bytes = key_u64s * 8;
            if key_pos + key_bytes > key_end {
                break;
            }

            let key_type = buf[key_pos + 2];
            let key_format = buf[key_pos + 1] & 0x7f;
            let key_hdr_u64s = if key_format != 0 { format_key_u64s } else { BKEY_U64S };
            let val_off = key_hdr_u64s * 8;

            if val_off < key_bytes
                && sanitize_val(&mut buf[key_pos + val_off..key_pos + key_bytes],
                                key_type, sanitize_filenames) {
                modified = true;
            }

            key_pos += key_bytes;
        }

        if modified {
            // Zero csum at start of btree_node or btree_node_entry
            // Clear BSET_CSUM_TYPE in bset.flags
            clear_csum(buf, pos, bset_off + 16);
        }

        first = false;
        let advance = vstruct_aligned_bytes(vstruct_bytes, block_bits)
            .min(buf.len() - pos);
        bset_byte_offset += advance;
        pos += advance;
    }
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
    sanitize_fn: fn(*mut c::bch_fs, &mut [u8], bool),
) -> Result<()> {
    ranges_sort(ranges);
    let mut buf = vec![0u8; bucket_bytes as usize];

    for r in ranges.iter() {
        let len = (r.end - r.start) as usize;
        assert!(len <= bucket_bytes as usize);

        qcow2::pread_exact(img.infd(), &mut buf[..len], r.start)?;
        sanitize_fn(fs_raw, &mut buf[..len], sanitize_filenames);
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
            sanitize_filenames, sanitize_journal,
        )?;
        write_sanitized_ranges(
            &mut img, fs.raw, &mut d.btree, bucket_bytes,
            sanitize_filenames, sanitize_btree,
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
