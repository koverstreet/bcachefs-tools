//! The bkey methods of the key types with pointers, over ptrs.rs's walk:
//! validate, to_text and swab - each reading entries validate may not have
//! seen, or rejected - and btree_ptr(_v2)'s own on top of them.

use super::*;
use crate::{bkey_fsck_err, bkey_fsck_err_on};
use crate::btree::bkey::{bkey_eq, bpos_ge, POS_MIN};
use crate::errcode::ret_to_result_void;
use crate::init::error::{id, BkeyValidate};
use crate::opts::prt_compression_type;
use crate::util::rcu::ReadGuard;
use crate::util::Printbuf;

// ── validate ─────────────────────────────────────────────────────────────

/// A pointer, against the rest of the key and its device: extent_ptr_validate().
/// One to a device that isn't there passes - check_fix_ptrs() repairs it.
fn extent_ptr_validate(v: &BkeyValidate<'_, '_>, p: BkeySC<'_, Ptrs>, ptr: &c::bch_extent_ptr,
                       size_ondisk: u32) -> Result<(), BchError> {
    if u32::from(ptr.dev()) == c::BCH_SB_MEMBER_INVALID {
        return Ok(());
    }

    for ptr2 in p.ptrs(v.fs) {
        bkey_fsck_err_on!(v, !core::ptr::eq(ptr, ptr2) && ptr.dev() == ptr2.dev(),
                          id::ptr_to_duplicate_device,
                          "multiple pointers to same device ({})", ptr.dev())?;
    }

    let dev = {
        let rcu = read_lock();
        v.fs.dev_rcu(ptr.dev().into(), &rcu).map(|ca| {
            let (bucket, bucket_offset) = ca.sector_to_bucket_and_offset(ptr.offset());
            (bucket, bucket_offset, ca.mi.first_bucket, ca.mi.nbuckets, ca.mi.bucket_size)
        })
    };
    let Some((bucket, bucket_offset, first_bucket, nbuckets, bucket_size)) = dev else {
        return Ok(());
    };

    bkey_fsck_err_on!(v, bucket >= nbuckets, id::ptr_after_last_bucket,
                      "pointer past last bucket ({} > {})", bucket, nbuckets)?;
    bkey_fsck_err_on!(v, bucket < u64::from(first_bucket), id::ptr_before_first_bucket,
                      "pointer before first bucket ({} < {})", bucket, first_bucket)?;
    // u32, wrapping, as C's unsigned arithmetic
    bkey_fsck_err_on!(v, bucket_offset.wrapping_add(size_ondisk) > u32::from(bucket_size),
                      id::ptr_spans_multiple_buckets,
                      "pointer spans multiple buckets ({} + {} > {})", bucket_offset, size_ondisk, bucket_size)
}

/// The entry types a btree node pointer may have.
fn btree_ptr_entry_type_allowed(ty: u32) -> bool {
    use c::bch_extent_entry_type as t;
    [t::BCH_EXTENT_ENTRY_ptr, t::BCH_EXTENT_ENTRY_reconcile, t::BCH_EXTENT_ENTRY_reconcile_bp]
        .iter().any(|a| a.0 == ty)
}

/// Each entry against the ones before it, then all of them together:
/// bch2_bkey_ptrs_validate().
pub fn bkey_ptrs_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    use c::bch_bkey_type as kt;

    let (fs, k) = (v.fs, v.k);
    let p = k.arm::<Ptrs>().expect("a key with pointers");
    let is_btree_ptr = matches!(k.k.key_type(), kt::KEY_TYPE_btree_ptr | kt::KEY_TYPE_btree_ptr_v2);
    let range = Ptrs::entry_range(p);
    let entries_start = k.val_bytes().as_ptr() as usize + range.start;
    let entries_u64s = range.len() / 8;

    // btree_sectors(): a node's on-disk size, where a pointer has no crc
    let mut size_ondisk = if is_btree_ptr { fs.opts().btree_node_size >> 9 } else { k.k.size };
    let mut nonce = None;
    let mut nr_ptrs = 0u32;
    let (mut have_written, mut have_unwritten, mut have_ec, mut crc_since_last_ptr) =
        (false, false, false, false);
    let (mut have_non_inval_dev_ptrs, mut have_non_inval_dev_ptrs_dirty) = (false, false);

    for (i, e) in p.extent_entries(fs).enumerate() {
        let ty = e.ty();

        bkey_fsck_err_on!(v, matches!(e, ExtentEntry::Invalid { .. }), id::extent_ptrs_invalid_entry,
                          "invalid extent entry type (got {}, max {})", ty, fs.sb.extent_types_known.get())?;

        if let ExtentEntry::PastEnd { bytes, .. } = e {
            return Err(bkey_fsck_err!(v, id::extent_ptrs_entry_past_end,
                "extent entry at u64 {} runs past the end of the value, {} u64s",
                (bytes.as_ptr() as usize - entries_start) / 8, entries_u64s));
        }

        bkey_fsck_err_on!(v, is_btree_ptr && ty < c::BCH_EXTENT_ENTRY_MAX && !btree_ptr_entry_type_allowed(ty),
                          id::btree_ptr_has_non_ptr, "has non allowed field")?;

        match e {
            ExtentEntry::ptr(ptr) => {
                extent_ptr_validate(v, p, ptr, size_ondisk)?;

                bkey_fsck_err_on!(v, ptr.cached() != 0 && have_ec, id::ptr_cached_and_erasure_coded,
                                  "cached, erasure coded ptr")?;

                let real_dev = u32::from(ptr.dev()) != c::BCH_SB_MEMBER_INVALID;
                if real_dev {
                    if ptr.unwritten() == 0 {
                        have_written = true;
                    } else {
                        have_unwritten = true;
                    }
                }

                if real_dev || have_ec {
                    if ptr.cached() == 0 {
                        have_non_inval_dev_ptrs_dirty = true;
                    }
                    have_non_inval_dev_ptrs = true;
                }

                have_ec = false;
                crc_since_last_ptr = false;
                nr_ptrs += 1;
            }
            ExtentEntry::crc32(_) | ExtentEntry::crc64(_) | ExtentEntry::crc128(_) => {
                let crc = crc_unpack(k.k, e).expect("a crc");
                let csum_type = c::bch_csum_type(crc.csum_type.into());

                bkey_fsck_err_on!(v, !csum_type.valid(fs), id::ptr_crc_csum_type_unknown,
                                  "invalid checksum type")?;
                bkey_fsck_err_on!(v, u32::from(crc.compression_type) >=
                                     c::bch_compression_type::BCH_COMPRESSION_TYPE_NR.0,
                                  id::ptr_crc_compression_type_unknown, "invalid compression type")?;

                bkey_fsck_err_on!(v, u32::from(crc.offset).wrapping_add(crc.live_size) > crc.uncompressed_size,
                                  id::ptr_crc_uncompressed_size_too_small,
                                  "checksum offset + key size > uncompressed size")?;
                bkey_fsck_err_on!(v, crc_is_encoded(&crc) &&
                                     crc.uncompressed_size > fs.opts().encoded_extent_max >> 9 &&
                                     v.from.flags().intersects(c::bch_validate_flags::BCH_VALIDATE_write |
                                                               c::bch_validate_flags::BCH_VALIDATE_commit),
                                  id::ptr_crc_uncompressed_size_too_big, "too large encoded extent")?;
                bkey_fsck_err_on!(v, !crc_is_compressed(&crc) && crc.compressed_size != crc.uncompressed_size,
                                  id::ptr_crc_uncompressed_size_mismatch,
                                  "not compressed but compressed != uncompressed size")?;

                // an encrypted extent's crcs all point to one nonce, offset in
                if csum_type.is_encryption() {
                    let n = u32::from(crc.offset) + u32::from(crc.nonce);
                    match nonce {
                        None => nonce = Some(n),
                        Some(first) if first != n =>
                            return Err(bkey_fsck_err!(v, id::ptr_crc_nonce_mismatch, "incorrect nonce")),
                        Some(_) => {}
                    }
                }

                bkey_fsck_err_on!(v, crc_since_last_ptr, id::ptr_crc_redundant, "redundant crc entry")?;
                crc_since_last_ptr = true;

                size_ondisk = crc.compressed_size;
            }
            ExtentEntry::stripe_ptr(_) => {
                bkey_fsck_err_on!(v, have_ec, id::ptr_stripe_redundant, "redundant stripe entry")?;
                have_ec = true;
            }
            ExtentEntry::reconcile(r) => ret_to_result_void(unsafe {
                c::bch2_extent_reconcile_validate(fs.as_ptr(), k.into(), v.from, r)
            })?,
            ExtentEntry::flags(_) =>
                bkey_fsck_err_on!(v, i != 0, id::extent_flags_not_at_start,
                                  "extent flags entry not at start")?,
            _ => {}
        }
    }

    bkey_fsck_err_on!(v, nr_ptrs == 0, id::extent_ptrs_no_ptrs, "no ptrs")?;
    bkey_fsck_err_on!(v, nr_ptrs > c::BCH_BKEY_PTRS_MAX, id::extent_ptrs_too_many_ptrs,
                      "too many ptrs: {} > {}", nr_ptrs, c::BCH_BKEY_PTRS_MAX)?;
    bkey_fsck_err_on!(v, have_written && have_unwritten, id::extent_ptrs_written_and_unwritten,
                      "extent with unwritten and written ptrs")?;
    bkey_fsck_err_on!(v, k.k.key_type() != kt::KEY_TYPE_extent && have_unwritten,
                      id::extent_ptrs_unwritten, "has unwritten ptrs")?;
    bkey_fsck_err_on!(v, crc_since_last_ptr, id::extent_ptrs_redundant_crc, "redundant crc entry")?;
    bkey_fsck_err_on!(v, have_ec, id::extent_ptrs_redundant_stripe, "redundant stripe entry")?;

    // A dead btree node keeps its pointer - not KEY_TYPE_error - for
    // bch_btree_ptr_v2's other fields
    if !is_btree_ptr {
        bkey_fsck_err_on!(v, !have_non_inval_dev_ptrs, id::extent_ptrs_all_invalid,
                          "extent without valid pointers")?;
        bkey_fsck_err_on!(v, v.from.from() == c::bkey_validate_from::BKEY_VALIDATE_commit &&
                             !have_non_inval_dev_ptrs_dirty,
                          id::extent_ptrs_all_invalid, "extent without valid dirty pointers")?;
    }
    Ok(())
}

// ── to_text ──────────────────────────────────────────────────────────────

/// A pointer: its device, bucket and offset, if the device is there, and how
/// stale it is once bucket gens are valid - on a new filesystem, or after
/// alloc_read; before, the table is zeroes.
pub(crate) fn extent_ptr_to_text(out: &mut Printbuf, fs: Option<&Fs>, ptr: &c::bch_extent_ptr,
                                 rcu: &ReadGuard) {
    match fs.and_then(|fs| Some((fs, fs.dev_rcu(ptr.dev().into(), rcu)?))) {
        None => write!(out, "{}:{} gen {}", ptr.dev(), ptr.offset(), ptr.generation()),
        Some((fs, ca)) => {
            let (b, offset) = ca.sector_to_bucket_and_offset(ptr.offset());
            write!(out, "{:>6} {}:{}:{} gen {}", ca.name().to_str().unwrap_or("(invalid name)"),
                   ptr.dev(), b, offset, ptr.generation());
            if ca.mi.durability != 1 {
                write!(out, " d={}", ca.mi.durability);
            }

            let gens_valid = fs.flag(c::bch_fs_flags::BCH_FS_new_fs) ||
                fs.recovery.runner.passes_complete.get() & (1 << c::bch_recovery_pass::BCH_RECOVERY_PASS_alloc_read as u32) != 0;
            // dev_ptr_stale_rcu()'s -1 for a bucket the device doesn't have
            let stale = if gens_valid { ca.ptr_stale(ptr, rcu).map_or(-1, i32::from) } else { 0 };
            if stale != 0 {
                write!(out, " stale={stale}");
            }
        }
    }

    if ptr.cached() != 0 {
        write!(out, " cached");
    }
    if ptr.unwritten() != 0 {
        write!(out, " unwritten");
    }
}

/// A checksum's word as C prints it: its bytes as a u64 of this machine's.
fn csum_word_raw(w: Le64) -> u64 {
    u64::from_ne_bytes(w.get().to_le_bytes())
}

fn crc_unpacked_to_text(out: &mut Printbuf, crc: &c::bch_extent_crc_unpacked) {
    write!(out, "c_size {} size {} offset {} nonce {} csum {} {:x}:{:x}  compress ",
           crc.compressed_size, crc.uncompressed_size, crc.offset, crc.nonce,
           c::bch_csum_type(crc.csum_type.into()),
           csum_word_raw(crc.csum.hi), csum_word_raw(crc.csum.lo));
    prt_compression_type(out, c::bch_compression_type(crc.compression_type.into()));
}

/// An extent's flags, by name, as prt_bitflags() prints them: from the
/// lowest, stopping at the first one without a name.
fn extent_flags_to_text(out: &mut Printbuf, mut flags: u64) {
    macro_rules! extent_flag_names {
        ($(($name:tt, $n:expr)),* $(,)?) => {
            const NAMES: &[&str] = &[$(stringify!($name),)*];
        };
    }
    c::BCH_EXTENT_FLAGS!(extent_flag_names);

    let mut first = true;
    while flags != 0 {
        let Some(name) = NAMES.get(flags.trailing_zeros() as usize) else { break };
        if !first {
            write!(out, ",");
        }
        first = false;
        write!(out, "{name}");
        flags &= flags - 1;
    }
}

/// A key with pointers' entries, a line each: bch2_bkey_ptrs_to_text(). It
/// prints keys validate rejected - that's what it's for - so an entry past
/// the end of the value is reported, not read, and one of a type this code
/// doesn't know ends it.
pub fn bkey_ptrs_to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
    let Some(fs) = fs else {
        write!(out, "no filesystem object available");
        return;
    };
    let p = k.arm::<Ptrs>().expect("a key with pointers");
    let entries_start = k.val_bytes().as_ptr() as usize + Ptrs::entry_range(p).start;

    let past_end = p.extent_entries(fs).find_map(|e| match e {
        ExtentEntry::PastEnd { bytes, .. } => Some((bytes.as_ptr() as usize - entries_start) / 8),
        _ => None,
    });

    if past_end.is_none() {
        write!(out, "durability: {} ", k.durability_safe(fs).total);
    }

    let mut out = out.atomic();
    let rcu = read_lock();

    for e in p.extent_entries(fs) {
        if let ExtentEntry::PastEnd { .. } = e {
            break;
        }

        out.newline();
        if let Some(name) = extent_entry_type_name(e.ty()) {
            write!(out, "{name}: ");
        }

        match e {
            ExtentEntry::ptr(ptr) => extent_ptr_to_text(&mut out, Some(fs), ptr, &rcu),
            ExtentEntry::crc32(_) | ExtentEntry::crc64(_) | ExtentEntry::crc128(_) =>
                crc_unpacked_to_text(&mut out, &crc_unpack(k.k, e).expect("a crc")),
            ExtentEntry::stripe_ptr(ec) => write!(out, "idx {} block {}", ec.idx(), ec.block()),
            ExtentEntry::rebalance_v1(r) =>
                unsafe { c::bch2_extent_rebalance_v1_to_text(out.as_raw(), fs.as_ptr(), r) },
            ExtentEntry::reconcile(r) =>
                unsafe { c::bch2_extent_reconcile_to_text(out.as_raw(), fs.as_ptr(), r) },
            ExtentEntry::flags(f) => extent_flags_to_text(&mut out, f.flags()),
            ExtentEntry::reconcile_bp(r) => write!(out, "idx {}", r.idx()),
            ExtentEntry::Unknown { bytes, .. } | ExtentEntry::Invalid { bytes, .. } |
            ExtentEntry::PastEnd { bytes, .. } => {
                let w = bytes.get(..8).map_or(0, |w| u64::from_ne_bytes(w.try_into().unwrap()));
                write!(out, "(unknown extent entry {w:016x})");
                return;
            }
        }
    }

    if let Some(at) = past_end {
        out.newline();
        write!(out, "(extent entry at u64 {at} runs past the end of the value)");
    }
}

// ── btree node pointers ──────────────────────────────────────────────────

pub(super) fn btree_ptr_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let val_u64s = v.k.val_bytes().len() / 8;

    bkey_fsck_err_on!(v, val_u64s > c::BCH_REPLICAS_MAX as usize, id::btree_ptr_val_too_big,
                      "value too big ({} > {})", val_u64s, c::BCH_REPLICAS_MAX)?;

    bkey_ptrs_validate(v)
}

pub(super) fn btree_ptr_v2_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let bp = v.k.arm::<c::BtreePtrV2>().expect("a btree_ptr_v2");
    let val_u64s = v.k.val_bytes().len() / 8;

    bkey_fsck_err_on!(v, val_u64s > c::BKEY_BTREE_PTR_VAL_U64s_MAX, id::btree_ptr_v2_val_too_big,
                      "value too big ({} > {})", val_u64s, c::BKEY_BTREE_PTR_VAL_U64s_MAX)?;

    bkey_fsck_err_on!(v, bpos_ge(bp.v().min_key, bp.k.p), id::btree_ptr_v2_min_key_bad,
                      "min_key > key")?;

    if v.from.flags().contains(c::bch_validate_flags::BCH_VALIDATE_write) &&
       u32::from(v.fs.sb.version_min.get()) >= c::bcachefs_metadata_version::btree_ptr_sectors_written.0 {
        bkey_fsck_err_on!(v, bp.v().sectors_written.get() == 0, id::btree_ptr_v2_written_0,
                          "sectors_written == 0")?;
    }

    bkey_ptrs_validate(v)
}

/// A btree_ptr_v2: its node's seq, how much of it is written and its
/// min_key, then its pointers.
pub fn btree_ptr_v2_to_text(out: &mut Printbuf, fs: &Fs, k: BkeySC<'_>) {
    let bp = k.arm::<c::BtreePtrV2>().expect("a btree_ptr_v2").v();

    crate::prt!(out, "\nseq {:x} written {} min_key {}{} ",
                bp.seq.get(), bp.sectors_written.get(),
                if bp.btree_ptr_range_updated() { "R " } else { "" },
                { bp.min_key });
    bkey_ptrs_to_text(out, Some(fs), k);
}

/// The node's min_key, to or from how @version on a @big_endian machine wrote
/// it: compat_bpos(), and the extents btree min_key shift - whose POS_MIN test
/// is bkey_eq(), where compat_btree_node()'s is bpos_eq(). It runs before
/// validate: a value too short for min_key is left to it, where C writes past.
///
/// XXX: as ptr_swab() - this should take an Unvalidated key.
pub fn btree_ptr_v2_compat(btree: c::btree_id, version: c::bcachefs_metadata_version, big_endian: bool,
                           write: bool, k: BkeyS<'_>) {
    let Some(mut bp) = k.arm::<c::BtreePtrV2>() else { return };
    let Some(v) = <c::BtreePtrV2 as ArmRef>::arm_mut(bp.val_bytes_mut()) else { return };

    let mut min_key = v.min_key;
    min_key.compat(btree, version, big_endian);

    if version < c::bcachefs_metadata_version::inode_btree_change &&
       btree.is_extents() &&
       !bkey_eq(min_key, POS_MIN) {
        min_key = if write { min_key.nosnap_predecessor() } else { min_key.nosnap_successor() };
    }
    v.min_key = min_key;
}

// ── byte swapping ────────────────────────────────────────────────────────

/// The entries of a key written on a machine of the other endianness, made
/// this one's - its own inverse: bch2_ptr_swab(), every BKEY_TYPE_ptrs
/// type's ->swab. Every word swapped, then the checksums fixed up: a crc's
/// checksum is bytes, not a word, and a crc32's and crc64's high part are
/// fields of their first word.
///
/// This runs before validate. An entry past the end of the value - the walk's
/// last - is left for validate to report.
///
/// XXX: this should take an Unvalidated key, once Rust has them.
pub fn ptr_swab(fs: &Fs, k: BkeyS<'_>) {
    let sizes = EntrySizes::of(fs);
    let mut k = k.arm::<Ptrs>().expect("a key with pointers");
    let range = Ptrs::entry_range(k.as_sc());

    if let Some(b) = k.val_bytes_mut().get_mut(range) {
        entries_swab(sizes, b);
    }
}

pub(super) fn entries_swab(sizes: EntrySizes, b: &mut [u8]) {
    b.chunks_exact_mut(8).for_each(<[u8]>::reverse);

    for e in (EntriesMut { sizes, b }).map(|(ty, b, stop)| ExtentEntryMut::new(ty, b, stop)) {
        match e {
            ExtentEntryMut::crc32(e) => e.update_csum(|c| c.swap_bytes()),
            ExtentEntryMut::crc64(e) => {
                e.update_csum_hi(|c| c.swap_bytes());
                e.csum_lo = { e.csum_lo }.swap_bytes();
            }
            ExtentEntryMut::crc128(e) => {
                e.csum.hi = Le64::new(e.csum.hi.get().swap_bytes());
                e.csum.lo = Le64::new(e.csum.lo.get().swap_bytes());
            }
            _ => {}
        }
    }
}
