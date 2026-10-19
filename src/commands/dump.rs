use bcachefs_kernel::btree::bkey::prelude::*;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use bcachefs_kernel::btree;
use bcachefs_kernel::btree::bkey::{BkeyI, BkeyS};
use bcachefs_kernel::data::ptrs::Ptrs;
use bcachefs_kernel::c;
use bcachefs_kernel::errcode::{ret_to_result_void, BchError};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::journal::{entry_type, journal_entry_type};
use bcachefs_kernel::util::vstructs::{assume_validated, cast_plain, Head, SelfSized, Unvalidated};
use bcachefs_kernel::opt_set;
use bcachefs_kernel::POS_MIN;

use crate::qcow2::{self, Qcow2Image, Ranges, range_add, ranges_sort};

// Crypto for the sanitize path. These drive the wrapped bch2_encrypt /
// bch2_checksum (and bset_encrypt) over the same byte ranges the kernel's
// bset_encrypt() and csum_vstruct() macro cover, so the metadata dump can
// decrypt, edit, and re-checksum bsets and journal entries without a C shim.
// The checksum covers everything after the leading csum field, to the end of
// the vstruct - a bset's @vstruct_bytes, a jset's reference's - and journal
// encryption everything after the encrypted_start marker. Encryption is
// symmetric, so the same call both decrypts and re-encrypts.

fn jset_encrypt(fs: &Fs, j: &mut c::jset<Unvalidated>, csum_type: u32) -> Result<(), BchError> {
    let off = std::mem::offset_of!(Head<c::jset>, encrypted_start);
    let (nonce, len) = (j.nonce(), size_of_val(j) - off);
    // SAFETY: the jset's bytes, past encrypted_start: its pointer reaches all of them
    ret_to_result_void(unsafe {
        c::bch2_encrypt(fs.as_ptr(), csum_type, nonce, j.as_ffi_mut().cast::<u8>().add(off).cast(), len)
    })
}

fn jset_csum_set(fs: &Fs, j: &mut c::jset<Unvalidated>, csum_type: u32) {
    let off = std::mem::size_of::<c::bch_csum>();
    // SAFETY: the jset's bytes, past its csum: its pointer reaches all of them
    j.csum = unsafe {
        c::bch2_checksum(fs.as_ptr(), csum_type, j.nonce(), j.as_ffi().cast::<u8>().add(off).cast(),
                         size_of_val(j) - off)
    };
}

/// @node points at the btree_node (first bset) or btree_node_entry, whose first
/// field is the csum; @i is the bset within it, at byte @offset from the node.
fn bset_csum_set(fs: &Fs, node: *mut u8, i: *mut Head<c::bset>, offset: u32,
                 csum_type: u32, vstruct_bytes: usize) {
    let off = std::mem::size_of::<c::bch_csum>();
    let csum = unsafe {
        c::bch2_checksum(fs.as_ptr(), csum_type, (*i).nonce(offset),
                         node.add(off) as *const core::ffi::c_void,
                         vstruct_bytes - off)
    };
    unsafe { *(node as *mut c::bch_csum) = csum };
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

    /// Dump only the lowest-device-index replica of each btree node, rewriting
    /// the other replica pointers to an invalid device. Produces a much smaller
    /// metadata image that still reads back cleanly (the surviving replica is
    /// the only one the read path will consider).
    #[arg(long)]
    single_replica: bool,

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

fn cmd_undump(cli: UndumpCli) -> Result<()> {
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

        qcow2::qcow2_to_raw(infile.as_fd(), outfile.as_fd())?;
    }

    Ok(())
}

// ---- Sanitize implementation ----

/// Sanitize a bkey value region in-place. Returns true if modified.
///
/// On-disk value layout (bch_val = 0 bytes):
///  - inline_data:          data at offset 0 — zero all
///  - indirect_inline_data: refcount(8), data — zero data
///  - dirent:               d_inum(8), d_type(1), d_name — fill with 'X'
///
/// What the write path should do to each buffer before dumping it.
#[derive(Clone, Copy)]
struct SanitizeOpts {
    /// Zero inline data extents, and (with `sanitize_filenames`) scramble dirent
    /// names.
    sanitize:           bool,
    sanitize_filenames: bool,
    /// Keep only the lowest-device-index replica of each btree_ptr, rewriting
    /// the rest to an invalid device.
    single_replica:     bool,
}

/// A member index that names no device (`BCH_SB_MEMBER_INVALID`); the read
/// path skips ptrs pointing at it.
const MEMBER_INVALID: u8 = c::BCH_SB_MEMBER_INVALID as u8;

/// De-replicate a btree pointer key in place: keep the pointer on the lowest
/// device index and set every other pointer's device to `BCH_SB_MEMBER_INVALID`.
/// Returns whether it changed anything. The surviving replica is the one the
/// dump actually wrote, and the only one the read path will consider.
fn derep(fs: &Fs, k: BkeyS) -> bool {
    let Some(mut k) = k.arm::<Ptrs>() else { return false };
    let min_dev = k.ptrs_mut(fs)
        .map(|p| p.dev())
        .filter(|&d| d != MEMBER_INVALID)
        .min();
    let Some(min_dev) = min_dev else { return false };

    let mut modified = false;
    for p in k.ptrs_mut(fs) {
        if p.dev() != min_dev {
            p.set_dev(MEMBER_INVALID);
            modified = true;
        }
    }
    modified
}

/// Per-value transform, dispatched by key type. Handles both `--single-replica`
/// de-replication (btree pointers) and `--sanitize` scrubbing (inline data,
/// filenames). Called for every key in both btree nodes and journal entries, so
/// de-replication reaches the btree_root pointers carried in the journal as well
/// as the interior/leaf pointers in btree nodes. Returns whether it modified.
fn sanitize_val(fs: &Fs, mut k: BkeyS, opts: SanitizeOpts) -> bool {
    const BTREE_PTR:            c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_btree_ptr;
    const BTREE_PTR_V2:         c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_btree_ptr_v2;
    const INLINE_DATA:          c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_inline_data;
    const INDIRECT_INLINE_DATA: c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_indirect_inline_data;
    const DIRENT:               c::bch_bkey_type = c::bch_bkey_type::KEY_TYPE_dirent;

    match k.k.key_type() {
        BTREE_PTR | BTREE_PTR_V2 if opts.single_replica => derep(fs, k),
        INLINE_DATA if opts.sanitize => {
            k.val_bytes_mut().fill(0);
            true
        }
        INDIRECT_INLINE_DATA if opts.sanitize => {
            let val = k.val_bytes_mut();
            if val.len() > 8 {
                val[8..].fill(0);
            }
            true
        }
        DIRENT if opts.sanitize && opts.sanitize_filenames => {
            let val = k.val_bytes_mut();
            if val.len() > 9 {
                val[9..].fill(b'X');
            }
            true
        }
        _ => false,
    }
}

/// Sanitize a journal buffer in-place: walk jset entries, handle encryption,
/// zero inline data, optionally scramble filenames, clear checksums.
fn sanitize_journal(fs_raw: *mut c::bch_fs, buf: &mut [u8], opts: SanitizeOpts) -> Result<()> {
    let fs = unsafe { Fs::borrow_raw(fs_raw) };
    let jset_magic = fs.sb_lock().sb().jset_magic();
    let block_bits = fs.block_bits();

    // A jset that overruns what holds it - or an entry or key one - is an
    // error: the walk can't scrub it, and skipping it would dump it as it is
    let mut pos = 0;
    while cast_plain::<Head<c::jset>>(&buf[pos..]).is_some_and(|j| j.magic.get() == jset_magic) {
        let j = c::jset::<Unvalidated>::from_bytes_mut(&mut buf[pos..])
            .map_err(|e| anyhow!("journal entry at {pos:#x} overruns its bucket: {e}"))?;
        let csum_type = j.csum_type() as u32;
        let mut modified = false;

        if c::bch_csum_type(csum_type).is_encryption() {
            if !fs.chacha20_key_set() {
                bail!("journal entry at {pos:#x} is encrypted, and the filesystem has no key to decrypt it");
            }

            jset_encrypt(&fs, j, csum_type)
                .map_err(|e| anyhow!("decrypting journal entry at {pos:#x}: {e}"))?;
            modified = true;
        }

        for e in j.entries_mut() {
            let e = e.map_err(|e| anyhow!("journal entry at {pos:#x}: {e}"))?;
            if matches!(entry_type(e), journal_entry_type::btree_keys
                                     | journal_entry_type::btree_root
                                     | journal_entry_type::write_buffer_keys) {
                for k in e.entries_mut() {
                    let k = k.map_err(|e| anyhow!("journal entry at {pos:#x}: {e}"))?;
                    // SAFETY: XXX: off disk, not validated - sanitize_val()
                    // should take an Unvalidated key
                    let mut k = unsafe { assume_validated(k) };
                    modified |= sanitize_val(&fs, BkeyS::from(k.key_mut()), opts);
                }
            }
        }

        if modified {
            // Re-encrypt (symmetric) if encrypted, then recompute the csum so
            // the entry stays checksum-valid instead of csum-cleared.
            if c::bch_csum_type(csum_type).is_encryption() {
                jset_encrypt(&fs, j, csum_type)
                    .map_err(|e| anyhow!("re-encrypting journal entry at {pos:#x}: {e}"))?;
            }
            jset_csum_set(&fs, j, csum_type);
        }

        pos += (j.sectors(block_bits) << 9).min(buf.len() - pos);
    }

    Ok(())
}

/// Sanitize a btree node buffer in-place: walk bset entries, handle
/// encryption, zero inline data, optionally scramble filenames.
fn sanitize_btree(fs_raw: *mut c::bch_fs, buf: &mut [u8], opts: SanitizeOpts) -> Result<()> {
    let fs = unsafe { Fs::borrow_raw(fs_raw) };
    let bset_magic = fs.sb_lock().sb().bset_magic();
    let block_bits = fs.block_bits();

    // The node's packed-key format lives in the btree_node header at the start
    // of the buffer; packed keys are unpacked against it.
    let format_ptr = unsafe {
        buf.as_ptr().add(std::mem::offset_of!(Head<c::btree_node>, format)) as *const c::bkey_format
    };

    let mut first = true;
    let mut seq: u64 = 0;
    let mut format_key_u64s = c::BKEY_U64s;
    let mut pos = 0;
    let mut bset_byte_offset: usize = 0;

    // The node, then its entries - each as long as its bset - until one isn't
    // this node's. A bset of this node's that overruns the buffer, or a key
    // that overruns its bset, is an error, as in sanitize_journal()
    let overrun = |pos: usize, e| anyhow!("btree node bset at {pos:#x} overruns the node: {e}");

    while pos < buf.len() {
        // where the bset is, and the whole's bytes and sectors
        let (bset_off, vstruct_bytes, sectors) = if first {
            let Some(n) = cast_plain::<Head<c::btree_node>>(&buf[pos..]) else { break };
            if n.magic.get() != bset_magic {
                break;
            }
            let n = c::btree_node::<Unvalidated>::from_bytes(&buf[pos..]).map_err(|e| overrun(pos, e))?;
            format_key_u64s = n.format.key_u64s as usize;
            seq = n.keys.seq.get();
            (pos + std::mem::offset_of!(Head<c::btree_node>, keys), n.bytes(), n.sectors(block_bits))
        } else {
            let Some(n) = cast_plain::<Head<c::btree_node_entry>>(&buf[pos..]) else { break };
            if n.keys.seq.get() != seq {
                break;
            }
            let n = c::btree_node_entry::<Unvalidated>::from_bytes(&buf[pos..]).map_err(|e| overrun(pos, e))?;
            (pos + std::mem::offset_of!(Head<c::btree_node_entry>, keys), n.bytes(), n.sectors(block_bits))
        };
        // SAFETY: checked whole, above
        let csum_type = unsafe { &*buf.as_ptr().add(bset_off).cast::<Head<c::bset>>() }.csum_type() as u32;
        let mut modified = false;

        if c::bch_csum_type(csum_type).is_encryption() {
            if !fs.chacha20_key_set() {
                bail!("btree node bset at {pos:#x} is encrypted, and the filesystem has no key to decrypt it");
            }

            let ret = unsafe {
                btree::read::bset_encrypt(fs.as_ptr(), buf.as_mut_ptr().add(bset_off) as *mut Head<c::bset>,
                                          bset_byte_offset as u32)
            };
            ret.map_err(|e| anyhow!("decrypting btree node bset at {pos:#x}: {e}"))?;
            modified = true;
        }

        // Walk packed keys in bset data region
        let key_end = pos + vstruct_bytes;
        let mut key_pos = bset_off + std::mem::size_of::<Head<c::bset>>();
        while key_pos + 3 <= key_end {
            let key_u64s = buf[key_pos] as usize;
            if key_u64s == 0 {
                break;
            }

            let key_bytes = key_u64s * 8;
            if key_pos + key_bytes > key_end {
                bail!("btree node bset at {pos:#x}: key at {key_pos:#x}, {key_bytes} bytes, overruns the bset's end at {key_end:#x}");
            }

            let key_format = buf[key_pos + 1] & 0x7f;
            // KEY_FORMAT_LOCAL_BTREE (0) uses the btree node's packed format;
            // anything else (KEY_FORMAT_CURRENT = 1) is the canonical
            // unpacked layout.
            let key_hdr_u64s = if key_format == 0 { format_key_u64s } else { c::BKEY_U64s };
            let val_off = key_hdr_u64s * 8;

            if val_off < key_bytes {
                let vs = key_pos + val_off;

                // An unpacked key (KEY_FORMAT_CURRENT) is a bkey_i in place; a
                // packed key is unpacked into a local bkey using the node format
                // only -- no struct btree, so none of btree_node_read_done's
                // repair runs. Either way the value is pointed at in place, so
                // sanitize_val's edits land on the buffer.
                let mut u: c::bkey;
                let k = if key_format == 0 {
                    u = unsafe { std::mem::zeroed() };
                    unsafe {
                        c::__bch2_bkey_unpack_key(
                            format_ptr,
                            &mut u,
                            buf.as_ptr().add(key_pos) as *const c::bkey_packed,
                        );
                    }
                    let v = unsafe { buf.as_mut_ptr().add(vs) as *mut c::bch_val };
                    // SAFETY: the value after the packed key, in the node's
                    // buffer: as long as the unpacked key's u64s says
                    unsafe { BkeyS::from_parts(&mut u, v) }
                } else {
                    // SAFETY: an unpacked key in the node's buffer, as long
                    // as its u64s say
                    let ki = unsafe { &mut *BkeyI::from_raw_mut(buf.as_mut_ptr().add(key_pos).cast()) };
                    BkeyS::from(ki)
                };

                if sanitize_val(&fs, k, opts) {
                    modified = true;
                }
            }

            key_pos += key_bytes;
        }

        if modified {
            // Re-encrypt (symmetric) if encrypted, then recompute the bset csum
            // over the btree_node (first bset) or btree_node_entry.
            let bset = unsafe { buf.as_mut_ptr().add(bset_off) } as *mut Head<c::bset>;
            if c::bch_csum_type(csum_type).is_encryption() {
                unsafe { btree::read::bset_encrypt(fs.as_ptr(), bset, bset_byte_offset as u32) }
                    .map_err(|e| anyhow!("re-encrypting btree node bset at {pos:#x}: {e}"))?;
            }
            let node = unsafe { buf.as_mut_ptr().add(pos) };
            bset_csum_set(&fs, node, bset, bset_byte_offset as u32, csum_type, vstruct_bytes);
        }

        first = false;
        let advance = (sectors << 9).min(buf.len() - pos);
        bset_byte_offset += advance;
        pos += advance;
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

fn dump_node(fs: &Fs, devs: &mut [DumpDev], k: btree::BkeySC<'_>, btree_node_size: u64,
             single_replica: bool) {
    // Capture the length by value so the filter closure doesn't borrow `devs`,
    // which we mutate via range_add below.
    let ndevs = devs.len();
    let Some(k) = k.arm::<Ptrs>() else { return };
    let exists = |p: &&c::bch_extent_ptr| {
        let dev = p.dev() as usize;
        dev < ndevs && fs.dev_exists(dev as u32)
    };

    if single_replica {
        // Dump only the replica on the lowest device index; the other ptrs are
        // rewritten to an invalid device in the btree write path.
        if let Some(ptr) = k.ptrs(fs).filter(exists).min_by_key(|p| p.dev()) {
            range_add(&mut devs[ptr.dev() as usize].btree, ptr.offset() << 9, btree_node_size);
        }
    } else {
        for ptr in k.ptrs(fs).filter(exists) {
            range_add(&mut devs[ptr.dev() as usize].btree, ptr.offset() << 9, btree_node_size);
        }
    }
}

fn get_sb_journal(fs: &Fs, ca: &c::bch_dev, entire_journal: bool, d: &mut DumpDev) {
    let sb = unsafe { &*ca.disk_sb.sb };
    let sb_bytes = sb.bytes() as u64;
    let bucket_bytes = (ca.mi.bucket_size as u64) << 9;

    // Superblock layout
    range_add(&mut d.sb,
              (c::BCH_SB_LAYOUT_SECTOR as u64) << 9,
              std::mem::size_of::<c::bch_sb_layout>() as u64);

    // All superblock copies
    for i in 0..sb.layout.nr_superblocks as usize {
        let offset = sb.layout.sb_offset[i].get();
        range_add(&mut d.sb, offset << 9, sb_bytes);
    }

    // Journal buckets
    let last_seq_ondisk = unsafe { (*fs.as_ptr()).journal.last_seq_ondisk };
    for i in 0..ca.journal.nr as usize {
        let seq = unsafe { *ca.journal.bucket_seq.add(i) };
        if entire_journal || seq >= last_seq_ondisk {
            let bucket = unsafe { *ca.journal.buckets.add(i) };
            range_add(&mut d.journal, bucket_bytes * bucket, bucket_bytes);
        }
    }
}

fn write_sanitized_ranges(
    img: &mut Qcow2Image<'_>,
    fs_raw: *mut c::bch_fs,
    ranges: &mut Ranges,
    bucket_bytes: u64,
    opts: SanitizeOpts,
    sanitize_fn: fn(*mut c::bch_fs, &mut [u8], SanitizeOpts) -> Result<()>,
) -> Result<()> {
    ranges_sort(ranges);
    let mut words = vec![0u64; bucket_bytes as usize / 8];
    // SAFETY: u64s, which any bytes are - 8 byte aligned, as the jsets and
    // btree nodes read through it need
    let buf = unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 8) };

    for r in ranges.iter() {
        let len = (r.end - r.start) as usize;
        assert!(len <= bucket_bytes as usize);

        qcow2::pread_exact(img.infd(), &mut buf[..len], r.start)?;
        sanitize_fn(fs_raw, &mut buf[..len], opts)
            .with_context(|| format!("sanitizing the {len} bytes at device offset {:#x}; \
                                      not written unsanitized - dump failed", r.start))?;
        img.write_buf(&buf[..len], r.start)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_dev_image(
    fs: &Fs,
    ca: &c::bch_dev,
    path: &str,
    force: bool,
    sanitize: bool,
    sanitize_filenames: bool,
    single_replica: bool,
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

    let infd = unsafe { BorrowedFd::borrow_raw((*ca.disk_sb.bdev).bd_fd) };
    let mut img = Qcow2Image::new(infd, outfile.as_fd(), block_size)?;

    img.write_ranges(&mut d.sb)?;

    let opts = SanitizeOpts { sanitize, sanitize_filenames, single_replica };
    let bucket_bytes = (ca.mi.bucket_size as u64) << 9;

    // The journal carries the btree_root pointers, so it goes through the
    // modify path for --single-replica (to de-replicate them) as well as
    // --sanitize.
    if sanitize || single_replica {
        write_sanitized_ranges(
            &mut img, fs.as_ptr(), &mut d.journal, bucket_bytes, opts, sanitize_journal,
        )?;
    } else {
        img.write_ranges(&mut d.journal)?;
    }

    // Btree nodes go through the modify path for --sanitize or --single-replica.
    if sanitize || single_replica {
        write_sanitized_ranges(
            &mut img, fs.as_ptr(), &mut d.btree, bucket_bytes, opts, sanitize_btree,
        )?;
    } else {
        img.write_ranges(&mut d.btree)?;
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
    let btree_node_size = fs.opts().btree_node_size as u64;
    let block_size = fs.opts().block_size as u32;

    let mut nr_online = 0u32;
    for ca in fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_bch2_online_devs) {
        if (sanitize || cli.single_replica) && (ca.mi.bucket_size as u32) % (block_size >> 9) != 0 {
            return Err(anyhow!("{} has unaligned buckets, cannot sanitize or de-replicate",
                               ca.name().to_string_lossy()));
        }

        get_sb_journal(fs, &ca, entire_journal, &mut devs[ca.dev_idx as usize]);
        nr_online += 1;
    }

    // Walk all btree types (including dynamic) to collect metadata locations.
    // The node iterator is per-level, so loop every level and dump each node's
    // own location (b.key) -- this reaches the root and every interior level,
    // no special-casing. Walking via the iterator (not a raw DFS) applies the
    // journal overlay, so nodes reachable only through not-yet-replayed journal
    // entries are captured too.
    for id in 0..fs.btree_id_nr_alive() {
        let trans = bcachefs_kernel::btree_trans!(fs);

        for level in 0..c::BTREE_MAX_DEPTH {
            let mut node_iter = btree::BtreeNodeIter::new(
                &trans,
                id,
                POS_MIN,
                0, // locks_want
                level,
                btree::BtreeIterFlags::PREFETCH,
            );

            node_iter.for_each(&trans, |b| {
                dump_node(fs, &mut devs, btree::BkeySC::from(b.key_i()), btree_node_size,
                          cli.single_replica);
                Ok(())
            }).map_err(|e| anyhow!("error walking btree {}: {}",
                btree::types::btree_id_str(id), e))?;
        }
    }

    // Write qcow2 image(s)
    for ca in fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_bch2_online_devs) {
        let dev_idx = ca.dev_idx;
        let path = if nr_online > 1 {
            format!("{}.{}.qcow2", cli.output, dev_idx)
        } else {
            format!("{}.qcow2", cli.output)
        };

        write_dev_image(fs, &ca, &path, cli.force, sanitize, sanitize_filenames,
                        cli.single_replica, block_size, &mut devs[dev_idx as usize])?;
    }

    Ok(())
}

fn cmd_dump(cli: DumpCli) -> Result<()> {

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
    opt_set!(opts, degraded, c::bch_degraded_actions::BCH_DEGRADED_very.0 as u8);
    opt_set!(opts, errors, c::bch_error_actions::BCH_ON_ERROR_continue.0 as u8);
    opt_set!(opts, fix_errors, c::fsck_err_opts::FSCK_FIX_no as u8);

    if cli.noexcl {
        opt_set!(opts, noexcl, 1);
    }
    if cli.verbose {
        opt_set!(opts, verbose, 1);
    }

    let fs = crate::device_scan::open_scan(&devs, opts)?;
    dump_fs(&fs, &cli, sanitize, sanitize_filenames)
}

pub const CMD_DUMP: super::CmdDef = typed_cmd!("dump", "Dump filesystem metadata", DumpCli, cmd_dump);
pub const CMD_UNDUMP: super::CmdDef = typed_cmd!("undump", "Restore dumped metadata", UndumpCli, cmd_undump);
