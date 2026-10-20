use crate::c;
use crate::c::{bch_sb, bch_sb_field_crypt, bch_sb_handle, block_device, nonce};
use crate::errcode::{bch_errcode, ret_to_c, ret_to_result_void, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::sb::members;
use crate::util::{Printbuf, ToText};
use crate::prt;
use crate::types::{ArmOf, Le16, Le32, Le64};
use crate::util::vstructs::{bytes_of_mut, cast_plain_mut, read_past, ArmRef, Head, Plain, SelfSized, Unsized, VStruct, Validated, ValidatedEntries};
use core::ffi::{c_int, c_uint, CStr};
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::NSEC_PER_SEC;
use cstruct_macros::c_export;
use core::mem::{size_of, size_of_val};

/// A superblock section's type: an arm of tagged_union! bch_sb_field, which
/// gives its field type - handed out as its ArmRef says, a short one, written
/// before its type grew, reading as zero past its end.
pub trait SbField: ArmOf<c::bch_sb_field, Tag = u32> + ArmRef {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type(Self::TAG);
}

impl<F: ArmOf<c::bch_sb_field, Tag = u32> + ArmRef + ?Sized> SbField for F {}

/// How a section type is validated and printed - by the sections! table, as
/// its type says. Both get the section as Self; one too short to be read as
/// Self fails validation before validate() is called, and isn't printed.
pub trait SbFieldOps: SbField {
    fn validate(_sb: &bch_sb, _f: &Self, _flags: c::bch_validate_flags,
                _err: &mut Printbuf) -> Result<(), BchError> {
        Ok(())
    }

    fn to_text(_out: &mut Printbuf, _fs: Option<&Fs>, _sb: &bch_sb, _f: &Self) {}

    /// Where the whole entries of the section in @b - its bytes, its header's
    /// included - end: past that, rounded up to a u64, is a partial entry,
    /// dropped at read and invalid at write. None if it's too short to be
    /// one. A section of fixed-size entries is as its arm says; one of
    /// variable-size entries walks them, entries_end().
    fn entries_end(b: &[u8]) -> Option<usize> {
        <Self as ArmRef>::whole_len(b.len())
    }
}

/// entries_end() for a section of variable-size entries @E, from @head on:
/// up to the first that doesn't fit, or that @is_entry says ends the walk -
/// a terminator, zeroes.
pub fn entries_end<E>(b: &[u8], head: usize, is_entry: impl Fn(&E) -> bool) -> Option<usize>
where
    E: Unsized<Head: Plain + SelfSized> + ?Sized,
{
    let entries = ValidatedEntries::<E>::new(b.get(head..)?);
    Some(head + entries.take_while(|e| is_entry(e)).map(size_of_val).sum::<usize>())
}

/// The section at @f in @sb, @len bytes, read with a partial entry past its
/// whole ones, which end at @end - C's readers ignored one: shrunk to them,
/// the rest of its last u64 zeroed as a writer leaves it, the sections after
/// it moved down, and a warning in @err. Repaired as it's read, as
/// bch2_sb_validate() repairs version_incompat_allowed, and written whole
/// with the next superblock write; unrepaired, a reader would find no
/// section, or not all of it. The section itself doesn't move, so
/// bch2_sb_validate()'s walk goes on from where the next one now is.
fn sb_field_drop_partial(sb: &mut bch_sb, f: usize, len: usize, end: usize, err: &mut Printbuf) {
    let whole = end.next_multiple_of(size_of::<u64>());
    let dropped = len - whole;

    let bytes = bytes_of_mut(sb);
    bytes[f + end..f + whole].fill(0);
    bytes.copy_within(f + len.., f + whole);
    let sb_end = bytes.len();
    bytes[sb_end - dropped..].fill(0);
    cast_plain_mut::<Head<c::bch_sb_field>>(&mut bytes[f..]).expect("a section's header").u64s =
        Le32::new((whole / size_of::<u64>()) as u32);
    sb.sub_u64s((dropped / size_of::<u64>()) as u32);

    write!(err, "{dropped} bytes past its last whole entry, dropped");
}

/// Section @f of @sb validated as an F: a partial entry at its end repaired
/// at read, invalid at write; then its type's own checks.
///
/// # Safety
/// @sb's sections' extents are checked, and @f is one of them.
pub(crate) unsafe fn validate_as<F: SbFieldOps + ?Sized>(
    sb:    *mut Head<bch_sb>,
    f:     *mut Head<c::bch_sb_field>,
    flags: c::bch_validate_flags,
    err:   &mut Printbuf,
) -> Result<(), BchError> {
    let invalid = || Err(bch_errcode::BCH_ERR_invalid_sb_field_size.into());
    let too_short = |err: &mut Printbuf, len| {
        write!(err, "too short ({len} < {} bytes)", <F as ArmRef>::MIN_LEN);
        invalid()
    };

    // SAFETY: the caller's
    let b = unsafe { c::bch_sb_field::<Validated>::from_ffi(f) }.as_bytes();
    let len = b.len();
    let Some(end) = F::entries_end(b) else {
        return too_short(err, len);
    };
    if end.next_multiple_of(size_of::<u64>()) < len {
        if flags.contains(c::bch_validate_flags::BCH_VALIDATE_write) {
            write!(err, "{} bytes past its last whole entry", len - end);
            return invalid();
        }
        // SAFETY: the caller's - all of the superblock, which its walk holds
        // no reference into; @f is a section of it
        let sb_mut = unsafe { bch_sb::<Validated>::from_ffi_mut(sb) };
        sb_field_drop_partial(sb_mut, f as usize - sb as usize, len, end, err);
    }

    // SAFETY: the caller's
    let (sb, f) = unsafe {
        (bch_sb::<Validated>::from_ffi(sb), c::bch_sb_field::<Validated>::from_ffi(f))
    };
    match f.arm::<F>() {
        Some(f) => F::validate(sb, f, flags, err),
        None => too_short(err, f.as_bytes().len()),
    }
}

/// Section @f printed as an F - nothing, if it's too short to be one.
fn to_text_as<F: SbFieldOps + ?Sized>(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb, f: &c::bch_sb_field) {
    if let Some(f) = f.arm::<F>() {
        F::to_text(out, fs, sb, f);
    }
}

// ── The sections, by type ────────────────────────────────────────────────

const FIELD_NR: usize = c::bch_sb_field_type::BCH_SB_FIELD_NR.0 as usize;

macro_rules! sb_fields {
    ($(($name:ident, $nr:literal, $($desc:literal)*)),* $(,)?) => { ::paste::paste! {
        /// By type.
        const FIELD_NAMES: [&str; FIELD_NR] = [$(stringify!($name)),*];

        /// The names, by type, NULL terminated: for C's flag parsing.
        #[c_export]
        #[c("const char * const bch2_sb_fields[]")]
        pub static bch2_sb_fields: [*const c_char; FIELD_NR + 1] =
            [$(concat!(stringify!($name), "\0").as_ptr().cast(),)* core::ptr::null()];

        // by type, as C's table is by position: in order, from 0
        const _: () = {
            let mut i = 0;
            $(assert!($nr == i, "BCH_SB_FIELDS(): types out of order"); i += 1;)*
            let _ = i;
        };
    } };
}
c::BCH_SB_FIELDS!(sb_fields);

/// Each section type's Rust type, whose SbFieldOps validate and print it.
macro_rules! sections {
    ($($name:ident => $t:ty),* $(,)?) => { ::paste::paste! {
        // each type the arm of its section, and every section a type
        const _: () = {
            $(assert!(<$t as SbField>::FIELD_TYPE.0 == c::bch_sb_field_type::[<BCH_SB_FIELD_ $name>].0,
                      concat!(stringify!($t), ": not section ", stringify!($name)));)*
            assert!([$(stringify!($name)),*].len() == FIELD_NR, "sections!: a section type missing");
        };

        /// Section @f validated as its type - nothing to do for a type this
        /// version doesn't know.
        ///
        /// # Safety
        /// As validate_as()'s.
        unsafe fn validate_typed(sb: *mut Head<bch_sb>, f: *mut Head<c::bch_sb_field>,
                                 flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
            let ty = unsafe { (*f).type_.get() };
            $(if ty == c::bch_sb_field_type::[<BCH_SB_FIELD_ $name>].0 {
                return unsafe { validate_as::<$t>(sb, f, flags, err) };
            })*
            Ok(())
        }

        /// Section @f printed as its type - nothing, for a type this version
        /// doesn't know.
        fn to_text_typed(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb, f: &c::bch_sb_field) {
            let ty = f.type_.get();
            $(if ty == c::bch_sb_field_type::[<BCH_SB_FIELD_ $name>].0 {
                return to_text_as::<$t>(out, fs, sb, f);
            })*
        }
    } };
}

sections! {
    journal                 => c::bch_sb_field_journal,
    members_v1              => c::bch_sb_field_members_v1,
    crypt                   => c::bch_sb_field_crypt,
    replicas_v0             => c::bch_sb_field_replicas_v0,
    quota                   => c::bch_sb_field_quota,
    disk_groups             => c::bch_sb_field_disk_groups,
    clean                   => c::bch_sb_field_clean,
    replicas                => c::bch_sb_field_replicas,
    journal_seq_blacklist   => c::bch_sb_field_journal_seq_blacklist,
    journal_v2              => c::bch_sb_field_journal_v2,
    counters                => c::bch_sb_field_counters,
    members_v2              => c::bch_sb_field_members_v2,
    errors                  => c::bch_sb_field_errors,
    ext                     => c::bch_sb_field_ext<Validated>,
    downgrade               => c::bch_sb_field_downgrade,
    recovery_passes         => c::bch_sb_field_recovery_passes,
    extent_type_u64s        => c::bch_sb_field_extent_type_u64s,
    errors_v2               => c::bch_sb_field_errors_v2,
}

impl c::bch_sb_field_type {
    /// Its name: None for a type this version doesn't know.
    pub fn name(self) -> Option<&'static str> {
        FIELD_NAMES.get(self.0 as usize).copied()
    }
}

/// The section at @f validated by its type's ops - and if it's invalid, said
/// so in @err, with what it holds. A repair made as it's read is said too.
///
/// # Safety
/// @sb is a superblock whose sections' extents are checked, @f one of them.
unsafe fn field_validate(sb: *mut Head<bch_sb>, f: *mut Head<c::bch_sb_field>,
                         flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
    let ty = unsafe { (*f).type_.get() };
    let name = c::bch_sb_field_type(ty).name().unwrap_or("(unknown)");
    let mut field_err = Printbuf::new();

    // SAFETY: the caller's
    let ret = unsafe { validate_typed(sb, f, flags, &mut field_err) };

    if ret.is_err() {
        write!(err, "Invalid superblock section {name}: {}", field_err.as_str());
        err.newline();
        // SAFETY: the caller's
        field_to_text(err, None, unsafe { bch_sb::<Validated>::from_ffi(sb) },
                      unsafe { c::bch_sb_field::<Validated>::from_ffi(f) });
    } else if !field_err.as_str().is_empty() {
        // repaired as read: bch2_read_super() prints it
        write!(err, "Superblock section {name}: {}", field_err.as_str());
        err.newline();
    }
    ret
}

/// Section @f as its type's ops print it.
pub fn field_ops_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb, f: &c::bch_sb_field) {
    if out.nr_tabstops() == 0 {
        out.tabstop_push(32);
    }

    to_text_typed(out, fs, sb, f);
}

/// Section @f: its type, its size, and what it holds.
pub fn field_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb, f: &c::bch_sb_field) {
    let ty = c::bch_sb_field_type(f.type_.get());
    match ty.name() {
        Some(name) => write!(out, "{name}"),
        None => write!(out, "(unknown field {})", ty.0),
    }
    write!(out, " (size {}):", f.as_bytes().len());
    out.newline();

    field_ops_to_text(out, fs, sb, f);
}

// ── Validating a superblock ──────────────────────────────────────────────

fn validate_layout(layout: &c::bch_sb_layout, out: &mut Printbuf) -> Result<(), BchError> {
    const _: () = assert!(size_of::<c::bch_sb_layout>() == 512);

    if layout.magic.b != BCACHE_MAGIC && layout.magic.b != BCHFS_MAGIC {
        write!(out, "Not a bcachefs superblock layout");
        return Err(bch_errcode::BCH_ERR_invalid_sb_layout.into());
    }

    if layout.layout_type != 0 {
        write!(out, "Invalid superblock layout type {}", layout.layout_type);
        return Err(bch_errcode::BCH_ERR_invalid_sb_layout_type.into());
    }

    if layout.nr_superblocks == 0 {
        write!(out, "Invalid superblock layout: no superblocks");
        return Err(bch_errcode::BCH_ERR_invalid_sb_layout_nr_superblocks.into());
    }

    if usize::from(layout.nr_superblocks) > layout.sb_offset.len() {
        write!(out, "Invalid superblock layout: too many superblocks");
        return Err(bch_errcode::BCH_ERR_invalid_sb_layout_nr_superblocks.into());
    }

    if u32::from(layout.sb_max_size_bits) > c::BCH_SB_LAYOUT_SIZE_BITS_MAX {
        write!(out, "Invalid superblock layout: max_size_bits too high");
        return Err(bch_errcode::BCH_ERR_invalid_sb_layout_sb_max_size_bits.into());
    }

    let max_sectors = 1u64 << layout.sb_max_size_bits;
    let offsets = &layout.sb_offset[..usize::from(layout.nr_superblocks)];
    for (i, w) in offsets.windows(2).enumerate() {
        let (prev, offset) = (w[0].get(), w[1].get());
        if offset < prev + max_sectors {
            write!(out, "Invalid superblock layout: superblocks overlap\n  (sb {i} ends at {} next starts at {offset}",
                   prev + max_sectors);
            return Err(bch_errcode::BCH_ERR_invalid_sb_layout_superblocks_overlap.into());
        }
    }
    Ok(())
}

/// Whether this version reads @version: its major, and no older than the
/// oldest it knows - bch2_version_compatible().
fn version_compatible(version: u16) -> bool {
    let current = c::bcachefs_metadata_version_current as u64;
    c::BCH_VERSION_MAJOR(version.into()) <= c::BCH_VERSION_MAJOR(current) &&
        u32::from(version) >= c::bcachefs_metadata_version_min as u32
}

fn version(v: impl Into<u32>) -> c::bcachefs_metadata_version {
    c::bcachefs_metadata_version(v.into())
}

fn sb_compatible(sb: &Head<bch_sb>, out: &mut Printbuf) -> Result<(), BchError> {
    let (v, v_min) = (sb.version.get(), sb.version_min.get());
    let (min, current) = (version(c::bcachefs_metadata_version_min as u32),
                          version(c::bcachefs_metadata_version_current as u32));

    for (what, v) in [("version", v), ("version_min", v_min)] {
        if !version_compatible(v) {
            prt!(out, "Unsupported superblock {what} {} (min {min}, max {current})", version(v));
            return Err(bch_errcode::BCH_ERR_invalid_sb_version.into());
        }
    }

    if v_min > v {
        prt!(out, "Bad minimum version {}, greater than version field {}", version(v_min), version(v));
        return Err(bch_errcode::BCH_ERR_invalid_sb_version.into());
    }
    Ok(())
}

/// For C's read path, which checks a layout read from its own sector, and a
/// superblock's version before it's read whole.
#[c_export]
pub fn bch2_sb_layout_validate(layout: &c::bch_sb_layout, out: &mut Printbuf) -> c_int {
    ret_to_c(validate_layout(layout, out))
}

#[c_export]
pub fn bch2_sb_compatible(sb: &Head<c::bch_sb>, out: &mut Printbuf) -> c_int {
    ret_to_c(sb_compatible(sb, out))
}

/// Superblock @sb checked - its header, its layout, every section - before
/// anything trusts it: as read from @read_offset, or about to be written,
/// BCH_VALIDATE_write. Read, a few things are repaired in place: an
/// incompat_allowed older versions didn't downgrade, a partial entry at a
/// section's end.
///
/// # Safety
/// @sb is a superblock read whole into its buffer: its count's bytes are
/// there, and nothing else uses it.
pub unsafe fn validate(sb: *mut Head<bch_sb>, opts: &c::bch_opts, read_offset: u64,
                       flags: c::bch_validate_flags, out: &mut Printbuf) -> Result<(), BchError> {
    let write = flags.contains(c::bch_validate_flags::BCH_VALIDATE_write);
    // SAFETY: the caller's - the header, which nothing else borrows
    let h = unsafe { &mut *sb };

    sb_compatible(h, out)?;

    if opts.no_version_check == 0 {
        let incompat = h.features[0].get() & (!0u64 << c::BCH_FEATURE_NR);
        let incompat_bit = if incompat != 0 {
            incompat.trailing_zeros()
        } else if h.features[1].get() != 0 {
            64 + h.features[1].get().trailing_zeros()
        } else {
            0
        };

        if incompat_bit != 0 {
            // SAFETY: C's table, BCH_FEATURE_NR names
            let highest = unsafe { CStr::from_ptr(*c::bch2_sb_features.as_ptr().add(c::BCH_FEATURE_NR as usize - 1)) };
            write!(out, "Filesystem has incompatible feature bit {incompat_bit}, highest supported {} ({})",
                   highest.to_str().unwrap_or("?"), c::BCH_FEATURE_NR - 1);
            return Err(bch_errcode::BCH_ERR_invalid_sb_features.into());
        }

        let current = c::bcachefs_metadata_version_current as u64;
        if c::BCH_VERSION_MAJOR(h.version.get().into()) > c::BCH_VERSION_MAJOR(current) ||
           h.version_incompat() > current {
            prt!(out, "Filesystem has incompatible version {}, current version {}",
                 version(h.version.get()), version(current as u32));
            return Err(bch_errcode::BCH_ERR_invalid_sb_features.into());
        }
    }

    if h.user_uuid.b == [0; 16] {
        write!(out, "Bad user UUID (got zeroes)");
        return Err(bch_errcode::BCH_ERR_invalid_sb_uuid.into());
    }

    if h.uuid.b == [0; 16] {
        write!(out, "Bad internal UUID (got zeroes)");
        return Err(bch_errcode::BCH_ERR_invalid_sb_uuid.into());
    }

    if !write && h.offset.get() != read_offset {
        write!(out, "Bad sb offset (got {}, read from {read_offset})", h.offset.get());
        return Err(bch_errcode::BCH_ERR_invalid_sb_offset.into());
    }

    if h.nr_devices == 0 || u32::from(h.nr_devices) > c::BCH_SB_MEMBERS_MAX {
        write!(out, "Bad number of member devices {} (max {})", h.nr_devices, c::BCH_SB_MEMBERS_MAX);
        return Err(bch_errcode::BCH_ERR_invalid_sb_too_many_members.into());
    }

    if h.dev_idx >= h.nr_devices {
        write!(out, "Bad dev_idx (got {}, nr_devices {})", h.dev_idx, h.nr_devices);
        return Err(bch_errcode::BCH_ERR_invalid_sb_dev_idx.into());
    }

    let precision = h.time_precision.get();
    if precision == 0 || u64::from(precision) > NSEC_PER_SEC {
        write!(out, "Invalid time precision: {precision} (min 1, max {NSEC_PER_SEC})");
        return Err(bch_errcode::BCH_ERR_invalid_sb_time_precision.into());
    }

    // old versions didn't know to downgrade this field
    if h.version_incompat_allowed() > h.version.get().into() {
        h.set_version_incompat_allowed(h.version.get().into());
    }

    if h.version_incompat() > h.version_incompat_allowed() {
        prt!(out, "Invalid version_incompat {} > incompat_allowed {}",
             version(h.version_incompat() as u32), version(h.version_incompat_allowed() as u32));
        if write {
            return Err(bch_errcode::BCH_ERR_invalid_sb_version.into());
        }
        h.set_version_incompat_allowed(h.version_incompat());
    }

    if h.nr_devices > 1 {
        h.set_multi_device(true);
    }

    #[cfg(kernel)]
    if h.shard_inums_nbits() == 0 {
        let btree_node_bytes = h.btree_node_size() << 9;
        // the members read through @sb, as C reads them - not yet validated
        let nr = unsafe { crate::sb::members::bch2_sb_nr_devices(sb) };
        let fs_size = (0..nr as i32).map(|i| {
            // SAFETY: a member of the superblock's, i < its nr_devices
            let m = unsafe { crate::sb::members::bch2_sb_member_get(sb, i) };
            m.nbuckets.get() * u64::from(m.bucket_size.get())
        }).sum::<u64>();
        let bits = crate::inode::bch2_shard_inode_numbers_bits_default(
            crate::util::kernel::num_online_cpus(), fs_size << 9, btree_node_bytes);
        // SAFETY: the header again, after the members' reads
        unsafe { &mut *sb }.set_shard_inums_nbits(bits.into());
    }
    // SAFETY: as above
    let h = unsafe { &*sb };

    validate_layout(&h.layout, out)?;

    // every section within the superblock, before any is read
    // SAFETY: the caller's
    let end = unsafe { bch_sb::<Validated>::from_ffi(sb) }.as_bytes().len();
    let mut off = size_of::<Head<bch_sb>>();
    while off < end {
        // SAFETY: within the superblock, which the caller says is there
        let f = unsafe { &*field_at(sb, off) };
        if f.u64s.get() == 0 {
            write!(out, "Invalid superblock: optional field with size 0 (type {})", f.type_.get());
            return Err(bch_errcode::BCH_ERR_invalid_sb_field_size.into());
        }
        off += f.u64s.get() as usize * size_of::<u64>();
        if off > end {
            write!(out, "Invalid superblock: optional field extends past end of superblock (type {})",
                   f.type_.get());
            return Err(bch_errcode::BCH_ERR_invalid_sb_field_size.into());
        }
    }

    // members must be validated first
    // SAFETY: its sections' extents checked, just now
    let s = unsafe { bch_sb::<Validated>::from_ffi(sb) };
    let Some(mi) = field_offset(s, c::bch_sb_field_type::BCH_SB_FIELD_members_v2)
        .or_else(|| field_offset(s, c::bch_sb_field_type::BCH_SB_FIELD_members_v1)) else {
        write!(out, "Invalid superblock: member info area missing");
        return Err(bch_errcode::BCH_ERR_invalid_sb_members_missing.into());
    };
    // SAFETY: a section, extents checked
    unsafe { field_validate(sb, field_at(sb, mi), flags, out)? };

    // each section, members_v2 again: a section's validate may shrink it, the
    // sections after moved down, so its length is read after
    let mut off = size_of::<Head<bch_sb>>();
    // SAFETY: the superblock, its count as validates leave it
    while off < unsafe { bch_sb::<Validated>::from_ffi(sb) }.as_bytes().len() {
        // SAFETY: a section, extents checked
        let f = unsafe { field_at(sb, off) };
        if unsafe { (*f).type_.get() } != c::bch_sb_field_type::BCH_SB_FIELD_members_v1.0 {
            unsafe { field_validate(sb, f, flags, out)? };
        }
        off += unsafe { (*f).u64s.get() } as usize * size_of::<u64>();
    }

    if write {
        // SAFETY: the header again - the sections' validates may have
        // changed its count - and the members, validated
        let h = unsafe { &*sb };
        let m = unsafe { crate::sb::members::bch2_sb_member_get(sb, i32::from(h.dev_idx)) };
        if m.seq != h.seq {
            write!(out, "Invalid superblock: member seq {} != sb seq {}", m.seq.get(), h.seq.get());
            return Err(bch_errcode::BCH_ERR_invalid_sb_members_missing.into());
        }
    }
    Ok(())
}

/// # Safety
/// As validate()'s.
#[c_export]
pub unsafe fn bch2_sb_validate(sb: *mut Head<c::bch_sb>, opts: &c::bch_opts, read_offset: u64,
                               flags: c::bch_validate_flags, out: &mut Printbuf) -> c_int {
    ret_to_c(unsafe { validate(sb, opts, read_offset, flags, out) })
}

// ── Versions ─────────────────────────────────────────────────────────────

macro_rules! metadata_versions {
    ($(($name:tt, $v:expr, $($rest:tt)*)),* $(,)?) => {
        /// The versions this one knows, oldest first, and their names.
        const VERSIONS: &[(u32, &str)] = {
            use crate::c::BCH_VERSION;
            &[$(($v as u32, stringify!($name))),*]
        };
    };
}
c::BCH_METADATA_VERSIONS!(metadata_versions);

impl c::bcachefs_metadata_version {
    /// Its name, if it's a version this one knows.
    pub fn name(self) -> Option<&'static str> {
        VERSIONS.iter().find(|&&(v, _)| v == self.0).map(|&(_, name)| name)
    }

    /// The newest version this one knows of the same major - itself at
    /// major 0: bch2_latest_compatible_version().
    pub fn latest_compatible(self) -> Self {
        let major = |v: u32| c::BCH_VERSION_MAJOR(v.into());
        if major(self.0) == 0 {
            return self;
        }
        Self(VERSIONS.iter().map(|&(v, _)| v).filter(|&v| major(v) == major(self.0)).fold(self.0, u32::max))
    }
}

/// The version's name and number.
impl ToText for c::bcachefs_metadata_version {
    fn to_text(&self, out: &mut Printbuf) {
        let v = u64::from(self.0);
        prt!(out, "{} ({}.{})", self.name().unwrap_or("(unknown version)"),
             c::BCH_VERSION_MAJOR(v), c::BCH_VERSION_MINOR(v));
    }
}
crate::display_via_to_text!(c::bcachefs_metadata_version);

#[c_export]
pub fn bch2_version_to_text(out: &mut Printbuf, v: c::bcachefs_metadata_version) {
    v.to_text(out)
}

#[c_export]
pub fn bch2_latest_compatible_version(v: c::bcachefs_metadata_version) -> c::bcachefs_metadata_version {
    v.latest_compatible()
}

impl Fs {
    /// Whether features of @version may be used, without asking:
    /// bch2_incompat_feature_allowed().
    fn incompat_feature_allowed(&self, version: c::bcachefs_metadata_version) -> bool {
        version.0 <= self.sb.version_incompat.into() ||
            (self.feature(c::bch_sb_feature::BCH_FEATURE_incompat_version_field) &&
             version.0 <= self.sb.version_incompat_allowed.into())
    }

    /// Make the filesystem incompatible with versions before @version, if
    /// that's allowed, writing the superblock: bch2_set_version_incompat().
    /// If it isn't, the first request for each version says so.
    pub fn set_version_incompat(&self, version: c::bcachefs_metadata_version) -> Result<(), BchError> {
        if self.incompat_feature_allowed(version) {
            let mut sb = self.sb_lock();
            if version.0 > self.sb.version_incompat.into() {
                let s = sb.sb_mut();
                let max = s.version_incompat().max(version.0.into());
                s.set_version_incompat(max);
                self.write_super(&mut sb);
            }
            return Ok(());
        }

        // a bit per minor: one major
        const _: () = assert!(c::BCH_VERSION_MAJOR(c::bcachefs_metadata_version_current as u64) == 1);
        let minor = c::BCH_VERSION_MINOR(version.0.into()) as usize;
        let requested = &self.incompat_versions_requested;
        if !requested.test(minor) && !requested.test_and_set(minor) {
            crate::bch_notice!(self, "requested incompat feature {} currently not enabled, allowed up to {}\n  set version_upgrade=incompatible to enable",
                               version, self::version(self.sb.version_incompat_allowed));
        }

        self.throw(bch_errcode::BCH_ERR_may_not_use_incompat_feature)
    }

    /// Mark the filesystem as using @feature, writing the superblock if it
    /// didn't - checked again under sb_lock: __bch2_check_set_feature().
    pub(crate) fn set_feature(&self, feature: c::bch_sb_feature) {
        let mut sb = self.sb_lock();
        if !self.feature(feature) {
            sb.sb_mut().features[0] |= 1 << feature.0;
            self.write_super(&mut sb);
        }
    }

    /// Allow incompatible features up to the filesystem's version, as an
    /// upgrade to it would have: version_upgrade=incompatible, set at
    /// runtime - bch2_sb_upgrade_incompat().
    fn upgrade_incompat(&self) {
        let mut sb = self.sb_lock();
        let (v, allowed) = (self.sb.version, self.sb.version_incompat_allowed);
        if v == allowed {
            return;
        }

        crate::bch_notice!(self, "Now allowing incompatible features up to {}, previously allowed up to {}\n",
                           version(v), version(allowed));

        let s = sb.sb_mut();
        s.features[0] |= c::BCH_SB_FEATURES_ALL;
        let max = s.version_incompat_allowed().max(v.into());
        s.set_version_incompat_allowed(max);

        crate::sb::downgrade::set_upgrade_incompat(&mut sb, allowed.into(), v.into());
        self.write_super(&mut sb);

        crate::init::passes::run_async(self);
    }
}

#[c_export]
pub fn bch2_set_version_incompat(c: &Opaque<c::bch_fs>, version: c::bcachefs_metadata_version) -> c_int {
    ret_to_c(Fs::from_c(c).set_version_incompat(version))
}

#[c_export]
pub fn __bch2_check_set_feature(c: &Opaque<c::bch_fs>, feat: c_uint) {
    Fs::from_c(c).set_feature(c::bch_sb_feature(feat))
}

#[c_export]
pub fn bch2_sb_upgrade_incompat(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).upgrade_incompat()
}

/// A superblock newer than this version brought down to it - what it's at
/// and what it allows - and the compat bits this version doesn't know
/// cleared: whether it was newer. bch2_check_version_downgrade().
///
/// The superblock only: c->sb keeps the version it's coming from, which
/// bch2_sb_set_downgrade() wants next, until the write's sb_update() brings
/// it up to date.
fn check_version_downgrade(fs: &Fs, sb: &mut SbLockGuard) -> bool {
    let current = c::bcachefs_metadata_version_current;
    let s = sb.sb_mut();

    if s.version_upgrade_complete() > current.into() {
        s.set_version_upgrade_complete(current.into());
    }
    if s.version_incompat_allowed() > current.into() {
        s.set_version_incompat_allowed(current.into());
    }
    if u32::from(fs.sb.version) > current {
        s.version = Le16::new(current as u16);
    }
    if u32::from(fs.sb.version_min) > current {
        s.version_min = Le16::new(current as u16);
    }
    s.compat[0] = Le64::new(s.compat[0].get() & ((1 << c::bch_sb_compat::BCH_COMPAT_NR.0) - 1));

    current < fs.sb.version.into()
}

#[c_export]
pub fn bch2_check_version_downgrade(c: &Opaque<c::bch_fs>) -> bool {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    check_version_downgrade(fs, &mut unsafe { fs.sb_lock_held() })
}

/// The superblock at @new, and allowing incompatible features up to it if
/// @incompat; a new major drops the downgrade section, which is for
/// downgrades within one. bch2_sb_upgrade().
fn sb_upgrade(sb: &mut SbLockGuard, new: u32, incompat: bool) {
    if c::BCH_VERSION_MAJOR(new.into()) > c::BCH_VERSION_MAJOR(sb.sb().version.get().into()) {
        sb.field_resize::<c::bch_sb_field_downgrade>(0);
    }

    let s = sb.sb_mut();
    s.version = Le16::new(new as u16);
    if incompat {
        s.features[0] |= c::BCH_SB_FEATURES_ALL;
        let max = s.version_incompat_allowed().max(new.into());
        s.set_version_incompat_allowed(max);
    }
}

#[c_export]
pub fn bch2_sb_upgrade(c: &Opaque<c::bch_fs>, new_version: c_uint, incompat: bool) {
    // SAFETY: C calls it under sb_lock
    sb_upgrade(&mut unsafe { Fs::from_c(c).sb_lock_held() }, new_version, incompat)
}

// ── Printing ─────────────────────────────────────────────────────────────

pub fn layout_to_text(out: &mut Printbuf, l: &c::bch_sb_layout) {
    write!(out, "Type:                    {}", l.layout_type);
    out.newline();

    write!(out, "Superblock max size:     ");
    out.units_u64(512 << l.sb_max_size_bits);
    out.newline();

    write!(out, "Nr superblocks:          {}", l.nr_superblocks);
    out.newline();

    write!(out, "Offsets:                 ");
    for (i, o) in l.sb_offset[..usize::from(l.nr_superblocks)].iter().enumerate() {
        if i != 0 {
            write!(out, ", ");
        }
        write!(out, "{}", o.get());
    }
    out.newline();
}

/// The superblock: its header, options, and - if @print_layout - its
/// layout; then the sections whose types are in @fields.
#[cold]
pub fn sb_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb, print_layout: bool, fields: u32) {
    if out.nr_tabstops() == 0 {
        out.tabstop_push(44);
    }

    let uuid = |b: [u8; 16]| uuid::Uuid::from_bytes(b);
    write!(out, "External UUID:\t{}\n", uuid(sb.user_uuid.b));
    write!(out, "Internal UUID:\t{}\n", uuid(sb.uuid.b));
    write!(out, "Magic number:\t{}\n", uuid(sb.magic.b));
    write!(out, "Device index:\t{}\n", sb.dev_idx);

    write!(out, "Label:\t");
    let label = sb.label();
    if label.is_empty() {
        write!(out, "(none)");
    } else {
        out.write_bytes(label);
    }
    out.newline();

    for (what, v) in [("Version", u32::from(sb.version.get())),
                      ("Incompatible features allowed", sb.version_incompat_allowed() as u32),
                      ("Incompatible features in use", sb.version_incompat() as u32),
                      ("Version upgrade complete", sb.version_upgrade_complete() as u32),
                      ("Oldest version on disk", u32::from(sb.version_min.get()))] {
        prt!(out, "{what}:\t{}\n", version(v));
    }

    write!(out, "Created:\t");
    match sb.time_base_lo.get() {
        0 => write!(out, "(not set)"),
        t => out.datetime((t / NSEC_PER_SEC) as i64),
    }
    out.newline();

    write!(out, "Sequence number:\t{}\n", sb.seq.get());

    write!(out, "Time of last write:\t");
    out.datetime(sb.write_time.get() as i64);
    out.newline();

    write!(out, "Superblock size:\t");
    out.units_u64(sb.as_bytes().len() as u64);
    write!(out, "/");
    out.units_u64(512u64 << sb.layout.sb_max_size_bits);
    out.newline();

    write!(out, "Clean:\t{}\n", u8::from(sb.clean()));
    if sb.dirents_sanitized() {
        write!(out, "Dirents sanitized:\t1\n");
    }
    // SAFETY: a validated superblock's
    write!(out, "Devices:\t{}\n", unsafe { crate::sb::members::bch2_sb_nr_devices(sb.as_ffi()) });

    write!(out, "Sections:\t");
    let have = sb.entries().fold(0u64, |m, f| m | 1u64.checked_shl(f.type_.get()).unwrap_or(0));
    out.bitflags(&FIELD_NAMES, have);
    out.newline();

    // SAFETY: C's tables, NULL terminated
    unsafe {
        write!(out, "Features:\t");
        out.prt_bitflags(c::bch2_sb_features.as_ptr(), sb.features[0].get());
        out.newline();

        write!(out, "Compat features:\t");
        out.prt_bitflags(c::bch2_sb_compat.as_ptr(), sb.compat[0].get());
        out.newline();
    }

    out.newline();
    write!(out, "Options:");
    out.newline();
    {
        let mut out = out.indent(2);
        let sb_ptr = sb.as_ffi() as *mut _;
        for (id, opt) in crate::opts::opt_table().iter().enumerate() {
            if opt.get_sb.is_none() && opt.get_ext.is_none() {
                continue;
            }
            // SAFETY: an option of the table, of a validated superblock
            unsafe {
                let v = c::bch2_opt_from_sb(sb_ptr, c::bch_opt_id(id as u32), -1);
                write!(out, "{}:\t", CStr::from_ptr(opt.attr.name.cast()).to_str().unwrap_or("?"));
                c::bch2_opt_to_text(out.as_raw(), core::ptr::null_mut(), sb_ptr, opt, v,
                                    c::OPT_HUMAN_READABLE | c::OPT_SHOW_FULL_LIST);
            }
            out.newline();
        }
    }

    if print_layout {
        out.newline();
        write!(out, "layout:");
        out.newline();
        layout_to_text(&mut out.indent(2), &sb.layout);
    }

    for f in sb.entries().filter(|f| 1u32.checked_shl(f.type_.get()).is_some_and(|b| fields & b != 0)) {
        out.newline();
        field_to_text(out, fs, sb, f);
    }
}
impl PartialEq for c::bch_sb__head {
    fn eq(&self, other: &Self) -> bool {
        self.magic.b == other.magic.b
            && self.user_uuid.b == other.user_uuid.b
            && self.block_size == other.block_size
            && self.version == other.version
            && self.uuid.b == other.uuid.b
            && self.seq == other.seq
    }
}

/*
 * bcachefs_format.h's JSET_MAGIC and BSET_MAGIC. C keeps its own: they're
 * __cpu_to_le64()s, XORed with the uuid's bytes as they are, which a
 * c_const! - a plain integer - can't say.
 */
const JSET_MAGIC: u64 = 0x245235c1a3625032;
const BSET_MAGIC: u64 = 0x90135c78b99e07f5;

/// What walks its sections - C's walk, which trusts their counts: a
/// validated superblock's.
impl c::bch_sb {
    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self)
    }

    /// Its crypt section - whole, or none: validate rejects a shorter one.
    pub fn crypt(&self) -> Option<&bch_sb_field_crypt> {
        self.field::<bch_sb_field_crypt>()
    }
}

/// Its header's: every state's, which each derefs to.
impl c::bch_sb__head {
    pub fn uuid(&self) -> uuid::Uuid {
        uuid::Uuid::from_bytes(self.user_uuid.b)
    }

    /// The magic this filesystem's jsets carry.
    pub fn jset_magic(&self) -> u64 {
        self.magic() ^ JSET_MAGIC
    }

    /// The magic this filesystem's btree nodes carry.
    pub fn bset_magic(&self) -> u64 {
        self.magic() ^ BSET_MAGIC
    }

    /// The first 8 bytes of its internal uuid.
    fn magic(&self) -> u64 {
        u64::from_le_bytes(self.uuid.b[..8].try_into().unwrap())
    }

    /// Up to the first NUL.
    pub fn label(&self) -> &[u8] {
        let len = self.label.iter().position(|b| *b == 0).unwrap_or(self.label.len());
        &self.label[..len]
    }

    /// The nonce the superblock is encrypted with.
    pub fn nonce(&self) -> nonce {
        let [a, b, c, d, e, f, g, h, _rest @ ..] = self.uuid.b;
        // nonce.d is __le32: the raw bytes, as they are.
        use crate::types::Le32;
        let raw = |b| Le32::new(u32::from_le_bytes(b));
        nonce {
            d: [Le32::new(0), Le32::new(0), raw([a, b, c, d]), raw([e, f, g, h])],
        }
    }
}

impl bch_sb_handle {
    pub fn sb(&self) -> &bch_sb {
        // SAFETY: C read and validated it, and owns its buffer while we're
        // borrowed - resize and realloc take &mut
        unsafe { bch_sb::<Validated>::from_ffi(self.sb) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        // SAFETY: as sb()'s
        unsafe { bch_sb::<Validated>::from_ffi_mut(self.sb) }
    }

    pub fn bdev(&self) -> &block_device {
        unsafe { &*self.bdev }
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self.sb())
    }

    pub fn field_mut<F: SbField + ?Sized>(&mut self) -> Option<&mut F> {
        sb_field_get_mut::<F>(self)
    }

    pub fn field_resize<F: SbField + ?Sized>(&mut self, u64s: u32) -> Option<&mut F> {
        sb_field_resize::<F>(self, u64s)
    }

    pub fn field_get_minsize<F: SbField + ?Sized>(&mut self, min_u64s: u32) -> Option<&mut F> {
        sb_field_get_minsize::<F>(self, min_u64s)
    }

    pub fn field_delete(&mut self, ty: c::bch_sb_field_type) {
        bch2_sb_field_delete(self, ty)
    }

    /// @u64s 0 for the minimum.
    pub fn sb_realloc(&mut self, u64s: u32) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::bch2_sb_realloc(self, u64s) })
    }

    /// The superblock's full vstruct extent as bytes.
    pub fn sb_bytes(&self) -> &[u8] {
        self.sb().as_bytes()
    }

    /// Its header - C's struct bch_sb, every field at its offset - edited as
    /// bytes by @f: kvdb's sb set. Not its count, u64s, which changes only
    /// with what it counts: an error if @f changed it, the header left as it
    /// was.
    pub fn edit_header<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, &'static str> {
        const N: usize = core::mem::size_of::<Head<bch_sb>>();
        let mut h: [u8; N] = self.sb_bytes()[..N].try_into().unwrap();
        let r = f(&mut h);
        if read_past::<Head<bch_sb>>(&h, 0).u64s() != self.sb().u64s() {
            return Err("u64s is the superblock's count: it changes with the fields it counts");
        }
        crate::util::vstructs::bytes_of_mut(self.sb_mut())[..N].copy_from_slice(&h);
        Ok(r)
    }

    /// Read-only, bounds-checked access to members_v2.
    pub fn members_v2(&self) -> Option<members::MembersV2<'_>> {
        members::members_v2(self.sb())
    }

    /// Read-only, bounds-checked access to members_v1.
    pub fn members_v1(&self) -> Option<members::MembersV1<'_>> {
        members::members_v1(self.sb())
    }
}

impl Drop for bch_sb_handle {
    fn drop(&mut self) {
        unsafe { c::bch2_free_super(&mut *self); }
    }
}

pub struct CounterInfo {
    pub name: &'static str,
    pub stable_id: u16,
    pub is_sectors: bool,
}

// From BCH_PERSISTENT_COUNTERS(): its entries are (name, stable id, type,
// description).
macro_rules! counter_is_sectors {
    (TYPE_SECTORS) => { true };
    ($other:tt) => { false };
}
macro_rules! counter_table {
    ($(($name:tt, $id:tt, $ty:tt $(, $($rest:tt)*)?)),* $(,)?) => {
        pub const COUNTERS: &[CounterInfo] = &[$(
            CounterInfo { name: stringify!($name), stable_id: $id, is_sectors: counter_is_sectors!($ty) },
        )*];
    };
}
c::BCH_PERSISTENT_COUNTERS!(counter_table);

// ── Writing it back ──────────────────────────────────────────────────────

/// Permission to change the superblock, and what writes it back: C's
/// CLASS(sb_write). It holds sb_lock, so changes go through it; dropped, it
/// writes - if anything was dirtied - before the lock goes, which in C was
/// the order of two declarations.
///
/// Or a C caller's, from_c(): its CLASS(sb_write), under its sb_lock -
/// dirtied here, and written when C's guard goes.
pub struct SbWrite<'a> {
    fs: &'a Fs,
    sb: SbLockGuard<'a>,
    w:  SbWriteState<'a>,
}

enum SbWriteState<'a> {
    Own(c::sb_write),
    C(&'a mut c::sb_write),
}

impl Fs {
    pub fn sb_write(&self) -> SbWrite<'_> {
        let w = c::sb_write { c: self.as_ptr(), ..Default::default() };
        SbWrite { fs: self, sb: self.sb_lock(), w: SbWriteState::Own(w) }
    }

    /// sb_write(), for a write whose only reason is a replicas entry coming
    /// or going: it may leave the slow disks behind.
    pub fn sb_write_replicas(&self) -> SbWrite<'_> {
        let w = c::sb_write { c: self.as_ptr(), replicas: true, ..Default::default() };
        SbWrite { fs: self, sb: self.sb_lock(), w: SbWriteState::Own(w) }
    }
}

impl<'a> SbWrite<'a> {
    /// C's @w, for @fs.
    ///
    /// # Safety
    /// C holds sb_lock for 'a, and @w is its CLASS(sb_write) under it.
    pub unsafe fn from_c(fs: &'a Fs, w: &'a mut c::sb_write) -> Self {
        SbWrite { fs, sb: unsafe { fs.sb_lock_held() }, w: SbWriteState::C(w) }
    }

    fn state(&mut self) -> &mut c::sb_write {
        match &mut self.w {
            SbWriteState::Own(w) => w,
            SbWriteState::C(w) => w,
        }
    }

    /// The caller changed the superblock: write it.
    pub fn dirty(&mut self) {
        self.state().dirty = true;
    }

    /// @was_new from a test-and-set: the superblock changed if it was, and
    /// whether it was - a different question from whether to write - is
    /// returned.
    pub fn record(&mut self, was_new: bool) -> bool {
        if was_new {
            self.dirty();
        }
        was_new
    }

    /// Write now, if dirtied, for an error to report; after, nothing is to
    /// write until something is dirtied again.
    pub fn flush(&mut self) -> Result<(), BchError> {
        let w = self.state();
        if !core::mem::take(&mut w.dirty) {
            return Ok(());
        }
        let (replicas, flags) = (w.replicas, w.flags);
        // SAFETY: under sb_lock, which the write - and Rust it calls into,
        // taking it as held - expects; nothing of the superblock is borrowed
        ret_to_result_void(unsafe {
            if replicas {
                c::bch2_write_super_replicas(self.fs.as_ptr())
            } else {
                c::bch2_write_super_flags(self.fs.as_ptr(), flags)
            }
        })
    }
}

impl<'a> core::ops::Deref for SbWrite<'a> {
    type Target = SbLockGuard<'a>;

    fn deref(&self) -> &Self::Target {
        &self.sb
    }
}

impl core::ops::DerefMut for SbWrite<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sb
    }
}

/// An error writing is the write's to report; as C's guard, nothing here to
/// return it to.
impl Drop for SbWrite<'_> {
    fn drop(&mut self) {
        if matches!(self.w, SbWriteState::Own(_)) {
            let _ = self.flush();
        }
    }
}

// ── Sections, through a bch_sb_handle ────────────────────────────────────
//
// Resizing takes &mut bch_sb_handle - it can reallocate the superblock - so
// no section borrowed from the handle outlives it.

/// The section of type @ty in @sb: the walk trusts the sections' counts, a
/// validated superblock's.
fn field_get_id(sb: &bch_sb, ty: c::bch_sb_field_type) -> Option<&c::bch_sb_field<Validated>> {
    sb.entries().find(|f| f.type_.get() == ty.0)
}

/// Its length in bytes, header and all - 0 if there's none.
fn field_bytes(sb: &bch_sb, ty: c::bch_sb_field_type) -> usize {
    field_get_id(sb, ty).map_or(0, |f| f.as_bytes().len())
}

/// Where it is, in bytes from the superblock's start.
fn field_offset(sb: &bch_sb, ty: c::bch_sb_field_type) -> Option<usize> {
    let base = core::ptr::from_ref(sb).cast::<u8>() as usize;
    field_get_id(sb, ty).map(|f| core::ptr::from_ref(f).cast::<u8>() as usize - base)
}

/// The section at @off in the superblock at @sb.
///
/// # Safety
/// @sb is validated, @off a section's offset in it: field_offset()'s.
unsafe fn field_at(sb: *mut Head<bch_sb>, off: usize) -> *mut Head<c::bch_sb_field> {
    unsafe { sb.byte_add(off).cast() }
}

/// The superblock's section of type @F, if it has one, over the bytes its
/// header says are its - one written before F grew reads as zero past them.
pub fn sb_field_get<F: SbField + ?Sized>(sb: &c::bch_sb) -> Option<&F> {
    field_get_id(sb, F::FIELD_TYPE)?.arm::<F>()
}

/// To edit: a field past the section's end is no write it will carry -
/// whole_mut() for all of it, if it's all there.
pub fn sb_field_get_mut<F: SbField + ?Sized>(disk_sb: &mut c::bch_sb_handle) -> Option<&mut F> {
    // SAFETY: the handle's superblock, borrowed mutably with it
    unsafe { sb_field_get_raw_mut(disk_sb.sb) }
}

/// sb_field_get_mut(), for a superblock C hands Rust without its handle.
///
/// # Safety
/// @sb is validated, and nothing else uses it for 'a.
pub unsafe fn sb_field_get_raw_mut<'a, F: SbField + ?Sized>(sb: *mut Head<bch_sb>) -> Option<&'a mut F> {
    // SAFETY: the caller's
    let off = field_offset(unsafe { bch_sb::<Validated>::from_ffi(sb) }, F::FIELD_TYPE)?;
    // SAFETY: as sb_field_get()'s
    unsafe { c::bch_sb_field::<Validated>::from_ffi_mut(field_at(sb, off)) }.arm_mut::<F>()
}

/// # Safety
/// @sb's sections' counts are validated.
#[c_export]
pub unsafe fn bch2_sb_field_get_id(sb: *mut Head<c::bch_sb>, ty: c::bch_sb_field_type) -> *mut Head<c::bch_sb_field> {
    // SAFETY: the caller's
    match field_offset(unsafe { bch_sb::<Validated>::from_ffi(sb) }, ty) {
        Some(off) => unsafe { field_at(sb, off) },
        None => core::ptr::null_mut(),
    }
}

impl bch_sb_handle {
    /// The superblock's buffer, all of it: what the superblock can grow into.
    fn buffer_mut(&mut self) -> &mut [u8] {
        // SAFETY: the handle's, buffer_size bytes, borrowed with it
        unsafe { core::slice::from_raw_parts_mut(self.sb.cast(), self.buffer_size) }
    }

    /// The section at @off - or a new one at the end, None - resized in place
    /// to @u64s, those after it moved: grown, zeroed; 0, deleted. Its offset,
    /// if it's still there. The buffer must have room.
    fn field_resize_at(&mut self, off: Option<usize>, u64s: u32) -> Option<usize> {
        let new = u64s as usize * size_of::<u64>();
        let buf = self.buffer_mut();
        let sb = cast_plain_mut::<Head<bch_sb>>(buf).expect("a superblock's head");
        let (end, sb_u64s) = (sb.bytes(), sb.u64s());

        let old = off.map_or(0, |off| {
            cast_plain_mut::<Head<c::bch_sb_field>>(&mut buf[off..]).expect("a section's head").bytes()
        });
        let new_end = end + new - old;
        assert!(new_end <= buf.len(), "superblock resized past its buffer ({new_end} > {})", buf.len());

        let ret = match off {
            None if u64s == 0 => None,
            None => {
                buf[end..new_end].fill(0);
                cast_plain_mut::<Head<c::bch_sb_field>>(&mut buf[end..]).expect("a section's head")
                    .set_val_u64s(u64s as usize);
                Some(end)
            }
            Some(off) => {
                let (src, dst) = (off + old, off + new);
                buf.copy_within(src..end, if u64s != 0 { dst } else { off });
                if u64s != 0 {
                    if dst > src {
                        buf[src..dst].fill(0);
                    }
                    cast_plain_mut::<Head<c::bch_sb_field>>(&mut buf[off..]).expect("a section's head")
                        .set_val_u64s(u64s as usize);
                }
                (u64s != 0).then_some(off)
            }
        };

        cast_plain_mut::<Head<bch_sb>>(buf).expect("a superblock's head")
            .set_val_u64s(sb_u64s + new / size_of::<u64>() - old / size_of::<u64>());
        ret
    }

    /// The section of type @ty resized to @u64s - made if there's none - with
    /// the superblock grown to hold it, and if this is the filesystem's, each
    /// online device's too: its offset, None if there's no room.
    fn field_resize_id(&mut self, ty: c::bch_sb_field_type, u64s: u32) -> Option<usize> {
        let d = u64s as isize - (field_bytes(self.sb(), ty) / size_of::<u64>()) as isize;
        // each superblock grown by the section's change: a device's is its own size
        let grow = |sb: &mut bch_sb_handle| sb.sb_realloc((sb.sb().u64s() as isize + d) as u32).ok();

        grow(self)?;

        if self.bits.fs_sb() != 0 {
            // SAFETY: the filesystem's handle, its disk_sb, under sb_lock -
            // the filesystem only for its devices, whose handles are theirs
            let fs = unsafe { Fs::from_disk_sb(self) };
            fs.disk_sb.assert_held();

            // XXX: we're not checking that offline device have enough space
            for ca in fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_sb_field_resize) {
                // SAFETY: changed only under sb_lock, which we hold
                grow(unsafe { &mut *(&raw const ca.disk_sb).cast_mut() })?;
            }
        }

        let off = self.field_resize_at(field_offset(self.sb(), ty), u64s)?;
        // SAFETY: the section just resized
        unsafe { (*field_at(self.sb, off)).type_ = Le32::new(ty.0) };
        Some(off)
    }

    /// The section of type @ty, made or grown to at least @u64s.
    fn field_get_minsize_id(&mut self, ty: c::bch_sb_field_type, u64s: u32) -> Option<usize> {
        if field_get_id(self.sb(), ty).is_some_and(|f| f.as_bytes().len() >= u64s as usize * size_of::<u64>()) {
            return field_offset(self.sb(), ty);
        }
        self.field_resize_id(ty, u64s)
    }

    /// The section's pointer, C's or a typed arm's to make.
    fn field_ptr(&mut self, off: Option<usize>) -> *mut Head<c::bch_sb_field> {
        // SAFETY: a section of the handle's superblock
        off.map_or(core::ptr::null_mut(), |off| unsafe { field_at(self.sb, off) })
    }
}

#[c_export]
pub fn bch2_sb_field_resize_id(sb: &mut c::bch_sb_handle, ty: c::bch_sb_field_type, u64s: c_uint)
    -> *mut Head<c::bch_sb_field>
{
    let off = sb.field_resize_id(ty, u64s);
    sb.field_ptr(off)
}

#[c_export]
pub fn bch2_sb_field_get_minsize_id(sb: &mut c::bch_sb_handle, ty: c::bch_sb_field_type, u64s: c_uint)
    -> *mut Head<c::bch_sb_field>
{
    let off = sb.field_get_minsize_id(ty, u64s);
    sb.field_ptr(off)
}

/// The section at @f - NULL for a new one - resized in place, the buffer
/// already big enough: for C's __copy_super(), which reallocates first.
///
/// # Safety
/// @f is NULL, or a section of @sb's superblock.
#[c_export]
pub unsafe fn __bch2_sb_field_resize(sb: &mut c::bch_sb_handle, f: *mut Head<c::bch_sb_field>, u64s: c_uint)
    -> *mut Head<c::bch_sb_field>
{
    let off = (!f.is_null()).then(|| f as usize - sb.sb as usize);
    let off = sb.field_resize_at(off, u64s);
    sb.field_ptr(off)
}

#[c_export]
pub fn bch2_sb_field_delete(sb: &mut c::bch_sb_handle, ty: c::bch_sb_field_type) {
    if let Some(off) = field_offset(sb.sb(), ty) {
        sb.field_resize_at(Some(off), 0);
    }
}

/// The u64s section @F is with @nr of its tail's entries: for
/// sb_field_resize() and sb_field_get_minsize().
pub fn sb_field_u64s<F: SbField + Unsized + ?Sized>(nr: usize) -> u32 {
    (F::TAIL + nr * size_of::<F::Elem>()).div_ceil(size_of::<u64>()) as u32
}

/// Resize the section of type @F to @u64s, making it if there's none: the
/// section, at its (possibly new) location - whole_mut() if that's all of F.
pub fn sb_field_resize<F: SbField + ?Sized>(
    disk_sb: &mut c::bch_sb_handle,
    u64s: u32,
) -> Option<&mut F> {
    let off = disk_sb.field_resize_id(F::FIELD_TYPE, u64s)?;
    // SAFETY: the section just resized, borrowed with the handle
    unsafe { c::bch_sb_field::<Validated>::from_ffi_mut(field_at(disk_sb.sb, off)) }.arm_mut::<F>()
}

/// The section of type @F, made or grown to at least @min_u64s.
pub fn sb_field_get_minsize<F: SbField + ?Sized>(
    disk_sb: &mut c::bch_sb_handle,
    min_u64s: u32,
) -> Option<&mut F> {
    let off = disk_sb.field_get_minsize_id(F::FIELD_TYPE, min_u64s)?;
    // SAFETY: as sb_field_resize()'s
    unsafe { c::bch_sb_field::<Validated>::from_ffi_mut(field_at(disk_sb.sb, off)) }.arm_mut::<F>()
}

// ── Superblocks in byte buffers ──────────────────────────────────────────
//
// For device scans and recovery, without a bch_sb_handle. Construction checks
// every extent once - the header and vstruct within the buffer, each section
// within the superblock - so the C lookups after walk only what parse()
// checked. That's memory safety only: whether the contents make sense is
// bch2_sb_validate()'s. A buffer can't be reallocated, so resizing and
// deleting sections stay bch_sb_handle's.

pub const BCACHE_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];
pub const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

/// Why a buffer failed to parse as a superblock. Precise by design:
/// recovery reports these against scan candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbParseError {
    BufferTooSmall { need: usize, have: usize },
    Misaligned,
    BadMagic,
    ExtentBeyondBuffer { need: usize, have: usize },
    FieldBeyondSb { field_offset: usize },
    FieldBadU64s { field_offset: usize },
}

impl core::fmt::Display for SbParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SbParseError::BufferTooSmall { need, have } =>
                write!(f, "buffer too small for superblock header: need {need}, have {have}"),
            SbParseError::Misaligned =>
                write!(f, "buffer not sufficiently aligned for a superblock"),
            SbParseError::BadMagic =>
                write!(f, "not a bcachefs superblock (bad magic)"),
            SbParseError::ExtentBeyondBuffer { need, have } =>
                write!(f, "superblock u64s extends beyond buffer: need {need}, have {have}"),
            SbParseError::FieldBeyondSb { field_offset } =>
                write!(f, "superblock field at offset {field_offset} extends beyond superblock"),
            SbParseError::FieldBadU64s { field_offset } =>
                write!(f, "superblock field at offset {field_offset} with u64s 0"),
        }
    }
}

/// Validate the extents; returns the vstruct size in bytes.
fn sb_parse_checks(buf: &[u8]) -> Result<usize, SbParseError> {
    let hdr = core::mem::size_of::<Head<bch_sb>>();
    if buf.len() < hdr {
        return Err(SbParseError::BufferTooSmall { need: hdr, have: buf.len() });
    }
    if buf.as_ptr() as usize % core::mem::align_of::<Head<bch_sb>>() != 0 {
        return Err(SbParseError::Misaligned);
    }
    /* header extent and alignment just validated: */
    let sb = unsafe { &*(buf.as_ptr() as *const Head<bch_sb>) };

    if sb.magic.b != BCACHE_MAGIC && sb.magic.b != BCHFS_MAGIC {
        return Err(SbParseError::BadMagic);
    }

    let bytes = sb.bytes();
    if bytes > buf.len() {
        return Err(SbParseError::ExtentBeyondBuffer { need: bytes, have: buf.len() });
    }

    let mut off = hdr;
    while off < bytes {
        if bytes - off < core::mem::size_of::<Head<c::bch_sb_field>>() {
            return Err(SbParseError::FieldBeyondSb { field_offset: off });
        }
        /* field header extent just validated: */
        let f = unsafe { &*(buf.as_ptr().add(off) as *const Head<c::bch_sb_field>) };
        let f_u64s = f.u64s() as u64;
        if f_u64s == 0 {
            return Err(SbParseError::FieldBadU64s { field_offset: off });
        }
        let next = off as u64 + f_u64s * 8;
        if next > bytes as u64 {
            return Err(SbParseError::FieldBeyondSb { field_offset: off });
        }
        off = next as usize;
    }

    Ok(bytes)
}

/// A validated read-only superblock in a byte buffer.
pub struct SbRef<'a> {
    buf:   &'a [u8],
    bytes: usize,
}

impl<'a> SbRef<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, SbParseError> {
        let bytes = sb_parse_checks(buf)?;
        Ok(SbRef { buf, bytes })
    }

    pub fn sb(&self) -> &'a bch_sb {
        // SAFETY: parse() checked every count it has
        unsafe { bch_sb::<Validated>::from_ffi(self.buf.as_ptr().cast()) }
    }

    pub fn bytes(&self) -> &'a [u8] {
        &self.buf[..self.bytes]
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&'a F> {
        sb_field_get::<F>(self.sb())
    }
}

/// A validated superblock in a byte buffer, for in-place mutation.
pub struct SbMut<'a> {
    buf:   &'a mut [u8],
    bytes: usize,
}

impl<'a> SbMut<'a> {
    pub fn parse(buf: &'a mut [u8]) -> Result<Self, SbParseError> {
        let bytes = sb_parse_checks(buf)?;
        Ok(SbMut { buf, bytes })
    }

    pub fn sb(&self) -> &bch_sb {
        // SAFETY: parse() checked every count it has
        unsafe { bch_sb::<Validated>::from_ffi(self.buf.as_ptr().cast()) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        // SAFETY: as sb()'s
        unsafe { bch_sb::<Validated>::from_ffi_mut(self.buf.as_mut_ptr().cast()) }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.bytes]
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self.sb())
    }
}

/// Anything holding a superblock that can be viewed as bytes and
/// mutated in place: SbMut/SbBuf over raw buffers, bch_sb_handle for
/// C-owned superblocks. Superblock IO takes any of them.
pub trait SbAccess {
    fn sb(&self) -> &bch_sb;
    fn sb_mut(&mut self) -> &mut bch_sb;
    /// The superblock's full vstruct extent.
    fn sb_bytes(&self) -> &[u8];
}

impl SbAccess for bch_sb_handle {
    fn sb(&self) -> &bch_sb { bch_sb_handle::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { bch_sb_handle::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { bch_sb_handle::sb_bytes(self) }
}

impl SbAccess for SbMut<'_> {
    fn sb(&self) -> &bch_sb { SbMut::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { SbMut::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { SbMut::bytes(self) }
}

/// An owned superblock: aligned storage, extents validated at construction -
/// for recovery, which carries superblocks around as values.
#[cfg(feature = "std")]
pub struct SbBuf {
    buf:   Vec<u64>,
    bytes: usize,
}

#[cfg(feature = "std")]
impl SbBuf {
    /// @src needn't be aligned.
    pub fn from_bytes(src: &[u8]) -> Result<Self, SbParseError> {
        let mut buf = vec![0u64; src.len().div_ceil(8)];
        let dst = unsafe {
            core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, src.len())
        };
        dst.copy_from_slice(src);

        let bytes = sb_parse_checks(&dst[..])?;
        Ok(SbBuf { buf, bytes })
    }

    pub fn sb(&self) -> &bch_sb {
        // SAFETY: parse() checked every count it has
        unsafe { bch_sb::<Validated>::from_ffi(self.buf.as_ptr().cast()) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        // SAFETY: as sb()'s
        unsafe { bch_sb::<Validated>::from_ffi_mut(self.buf.as_mut_ptr().cast()) }
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.bytes) }
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        sb_field_get::<F>(self.sb())
    }
}

#[cfg(feature = "std")]
impl SbAccess for SbBuf {
    fn sb(&self) -> &bch_sb { SbBuf::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { SbBuf::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { SbBuf::bytes(self) }
}

#[cfg(test)]
mod tests {
    use super::{sb_field_u64s, validate_as, SbField};
    use crate::c;
    use crate::types::Le32;
    use crate::util::vstructs::{Head, Unvalidated, Validated, VStruct, VStructError};
    use crate::util::Printbuf;
    use core::ffi::c_int;

    /// @buf as a superblock field of type @ty, @u64s long - its header written
    /// - as a superblock's walk would give it: as many bytes as it says.
    fn field(buf: &mut [u64], ty: c::bch_sb_field_type, u64s: usize) -> &c::bch_sb_field {
        let f = buf.as_mut_ptr().cast::<Head<c::bch_sb_field>>();
        // SAFETY: u64s, which any bytes are, aligned for a bch_sb_field, and
        // as many as u64s says
        unsafe {
            (*f).set_val_u64s(u64s);
            (*f).type_ = Le32::new(ty.0);
            c::bch_sb_field::<Validated>::from_ffi(f)
        }
    }

    /// A superblock field is the section its type says - and one this code
    /// doesn't know is its type, as an error.
    #[test]
    fn sb_field_get() {
        type T = c::bch_sb_field_type;
        let mut buf = [0u64; 4];

        let f = field(&mut buf, T::BCH_SB_FIELD_journal, 3);
        assert_eq!(f.as_bytes().len(), 24);
        assert!(matches!(f.typed(), Ok(c::BchSbFieldRef::journal(j)) if j.field.u64s() == 3 && j.buckets.len() == 2));

        let f = field(&mut buf, T::BCH_SB_FIELD_NR, 3);
        assert!(matches!(f.typed(), Err(t) if t == T::BCH_SB_FIELD_NR));
    }

    /// A section too short to be its type fails validation, saying how short
    /// - against its whole size, or a grown type's floor - and isn't printed.
    #[test]
    fn sb_field_ops_short_section() {
        use crate::util::Printbuf;

        fn short<F: super::SbFieldOps + ?Sized>(ty: c::bch_sb_field_type, u64s: usize) -> String {
            let mut sb = [0u64; 32];
            let mut buf = [0u64; 16];
            field(&mut buf, ty, u64s);
            let (sb, f) = (sb.as_mut_ptr().cast(), buf.as_mut_ptr().cast());

            let mut err = Printbuf::new();
            let flags = c::bch_validate_flags::from_bits_retain(0);
            let ret = unsafe { validate_as::<F>(sb, f, flags, &mut err) };
            assert_eq!(crate::errcode::ret_to_c(ret), -(c::bch_errcode::BCH_ERR_invalid_sb_field_size.0 as i32));

            let mut out = Printbuf::new();
            // SAFETY: the superblock and section above
            let (sb, f) = unsafe { (c::bch_sb::<Validated>::from_ffi(sb), c::bch_sb_field::<Validated>::from_ffi(f)) };
            super::to_text_as::<F>(&mut out, None, sb, f);
            assert_eq!(out.as_str(), "");
            err.as_str().to_owned()
        }

        type T = c::bch_sb_field_type;
        // too short for its entries to start, and for its type
        assert_eq!(short::<c::bch_sb_field_clean>(T::BCH_SB_FIELD_clean, 2),
                   "too short (16 < 24 bytes)");
        // a grown type's floor
        assert_eq!(short::<c::bch_sb_field_ext<Validated>>(T::BCH_SB_FIELD_ext, 10),
                   "too short (80 < 88 bytes)");
        // a sized type's size
        assert_eq!(short::<c::bch_sb_field_crypt>(T::BCH_SB_FIELD_crypt, 7),
                   "too short (56 < 64 bytes)");
        assert_eq!(short::<c::bch_sb_field_quota>(T::BCH_SB_FIELD_quota, 2),
                   format!("too short (16 < {} bytes)", size_of::<c::bch_sb_field_quota>()));
    }

    /// An ext section from before btrees_lost_data, 88 bytes, is an ext
    /// section: the fields it has read as they are, the ones it doesn't as
    /// zero - not as whatever follows it, which here is all ones.
    #[test]
    fn sb_field_get_short_section() {
        let mut buf = [u64::MAX; 16];
        buf[..11].fill(0);
        buf[3] = 0x5a;                          // errors_silent[0]
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_ext, 11);

        let Ok(c::BchSbFieldRef::ext(e)) = f.typed() else { panic!("an ext section") };
        assert_eq!(e.errors_silent[0].get(), 0x5a);
        assert_eq!(e.btrees_lost_data().get(), 0);
        assert_eq!(e.btrees_clean().get(), 0);
        assert!(e.whole().is_none());
        assert_eq!((e.as_bytes().len(), size_of_val(e)), (88, 88));

        // and as the one arm sb_field_get() asks for
        let e = f.arm::<c::bch_sb_field_ext<Validated>>().expect("an ext section");
        assert_eq!((e.errors_silent[0].get(), e.btrees_lost_data().get()), (0x5a, 0));
        assert!(f.arm::<c::bch_sb_field_crypt>().is_none());
    }

    /// A clean section is its header, then journal entries to its end - each
    /// as long as it says.
    #[test]
    fn sb_field_clean_entries() {
        let mut buf = [0u64; 6];
        buf[2] = 42;                            // journal_seq
        buf[3] = 1 | 7 << 32;                   // an entry: u64s 1, type 7
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_clean, 6);

        let Ok(c::BchSbFieldRef::clean(clean)) = f.typed() else { panic!("a clean section") };
        assert_eq!((clean.journal_seq.get(), clean.as_bytes().len()), (42, 48));
        let es: Vec<_> = clean.entries().map(|e| (e.u64s(), e.type_)).collect();
        assert_eq!(es, [(1, 7), (0, 0)]);
    }

    /// A section is in its superblock field's state, and so are its members:
    /// the same clean section, Unvalidated, checks each entry and says which
    /// overruns it; Validated, trusting its counts as C does, just ends there.
    #[test]
    fn sb_field_state_reaches_its_members() {
        let mut buf = [0u64; 6];
        buf[2] = 42;                            // journal_seq
        buf[3] = 1;                             // an entry of 1 u64
        buf[5] = 5;                             // one of 5, past the section's end
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_clean, 6);
        let Ok(c::BchSbFieldRef::clean(clean)) = f.typed() else { panic!("a clean section") };
        assert_eq!(clean.entries().count(), 1);

        // SAFETY: u64s, which any bytes are
        let b = unsafe { core::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
        let f = c::bch_sb_field::<Unvalidated>::from_bytes(b).unwrap();
        let Ok(c::BchSbFieldRef::clean(clean)) = f.typed() else { panic!("a clean section") };
        let es: Vec<_> = clean.entries().collect();
        assert!(matches!(es[..], [Ok(_), Err(VStructError::Overrun { want: 48, have: 8, .. })]));
        assert_eq!(clean.journal_seq.get(), 42);

        // one arm, named in the field's state - c::bch_sb_field_clean alone,
        // the Validated one, isn't an arm of an Unvalidated field
        assert!(f.arm::<c::bch_sb_field_clean<Unvalidated>>().is_some());
    }

    /// A whole ext section is all of it, every field a field; one shorter
    /// than every valid one isn't one at all.
    #[test]
    fn sb_field_ext_whole_and_too_short() {
        let mut buf = [0u64; 16];
        buf[12] = 7;                            // flags0
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_ext, 15);
        let e = f.arm::<c::bch_sb_field_ext<Validated>>().expect("an ext section");
        let w = e.whole().expect("all of it");
        assert_eq!((w.flags0.get(), e.flags0().get(), size_of_val(e)), (7, 7, 120));

        let mut buf = [0u64; 16];
        let f = field(&mut buf, c::bch_sb_field_type::BCH_SB_FIELD_ext, 10);
        assert!(f.arm::<c::bch_sb_field_ext<Validated>>().is_none());
        assert!(f.typed().is_err());
    }

    /// A superblock with an errors_v2 section of two entries and a partial
    /// third - 8 bytes of one - then a counters section; and validate() of the
    /// errors_v2 section, as a read or a write would.
    fn validate_partial(flags: c::bch_validate_flags) -> (c::bch_sb_handle, c_int, Printbuf) {
        type E = c::bch_sb_field_errors_v2;
        type Ctrs = c::bch_sb_field_counters;

        let mut sb = c::bch_sb_handle::default();
        sb.sb_realloc(0).unwrap();
        for (i, e) in sb.field_resize::<E>(sb_field_u64s::<E>(2)).unwrap().entries.iter_mut().enumerate() {
            e.v.set_id(3 + i as u16);
            e.v.set_nr(1);
        }
        for (i, d) in sb.field_resize::<Ctrs>(sb_field_u64s::<Ctrs>(3)).unwrap().d.iter_mut().enumerate() {
            *d = (100 + i as u64).into();
        }
        sb.field_resize::<E>(sb_field_u64s::<E>(2) + 1);
        assert!(sb.field::<E>().is_none());

        let f = unsafe { crate::sb::io::bch2_sb_field_get_id(sb.sb, E::FIELD_TYPE) };
        let mut err = Printbuf::new();
        let ret = crate::errcode::ret_to_c(unsafe { validate_as::<E>(sb.sb, f, flags, &mut err) });
        (sb, ret, err)
    }

    /// A partial entry at a section's end, which C ignored, is dropped at read
    /// with a warning - the sections after it intact - and invalid at write.
    #[test]
    fn partial_entry() {
        let (sb, ret, err) = validate_partial(c::bch_validate_flags::empty());
        assert_eq!((ret, err.as_str()), (0, "8 bytes past its last whole entry, dropped"));
        let ids: Vec<u16> = sb.field::<c::bch_sb_field_errors_v2>().unwrap().entries.iter().map(|e| e.v.id()).collect();
        assert_eq!(ids, [3, 4]);
        let d: Vec<u64> = sb.field::<c::bch_sb_field_counters>().unwrap().d.iter().map(|d| d.get()).collect();
        assert_eq!(d, [100, 101, 102]);

        let (_, ret, err) = validate_partial(c::bch_validate_flags::BCH_VALIDATE_write);
        assert!(ret != 0);
        assert_eq!(err.as_str(), "8 bytes past its last whole entry");
    }
}
