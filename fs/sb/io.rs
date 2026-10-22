/* DOC_LATEX(superblock)
 * \subsubsection{Layout and redundancy}
 *
 * The primary superblock is located at sector 8 (4\,KB from the start of the
 * device). A \texttt{bch\_sb\_layout} structure at sector 7 records the locations
 * of all superblock copies---typically three: the primary, one immediately
 * following it, and one at the end of the device. Up to 61 backup locations can
 * be recorded. The layout structure has its own magic number so that it can be
 * found independently.
 *
 * Only the primary superblock and the layout structure are at fixed,
 * known-without-context offsets. Backup superblock locations are recorded only
 * inside the layout (and inside each superblock's own embedded layout copy);
 * recovery from a lost layout therefore requires either falling back to a
 * second known offset, or scanning the device. The end-of-device backup is at
 * an offset computable from the device size, which gives a second escape valve
 * when sector 7 itself is unreadable.
 *
 * The layout structure is intentionally minimal---a magic number, a small
 * offset table, no checksum---and lives in its own 512-byte sector so that it
 * shares a physical block with as little else as possible. All checksumming
 * and validation logic lives in the superblock itself. The embedded
 * \texttt{bch\_sb\_layout} inside each superblock is what's used in the
 * common case (the standalone copy at sector 7 is consulted only when the
 * primary superblock cannot be read).
 *
 * The superblock is written with a monotonically increasing sequence number
 * (\texttt{seq}); on read, the copy with the highest valid sequence number is
 * authoritative. The \texttt{bcachefs recover-super} command can reconstruct a
 * device's superblock from backup copies or from another device in the same
 * filesystem.
 *
 * \subsubsection{Threat model}
 *
 * Redundancy exists to survive several distinct failure modes, in roughly
 * descending order of how often they're observed in practice:
 *
 * \begin{itemize}
 * \item \textbf{Torn writes during power loss}. The actual superblock size
 *   varies---a few kilobytes on a single-device laptop filesystem, larger on
 *   big multi-device filesystems---but is generally well beyond a single
 *   sector or physical block, and the on-disk reservation can grow to 32\,MB.
 *   That's beyond any device's atomic write granularity, so power loss
 *   mid-write can leave a torn superblock; the checksum catches it and a
 *   backup is consulted. This was the original driver for redundant
 *   superblocks.
 * \item \textbf{Media errors and bit rot}. Bad sectors, single-sector unrecoverable
 *   reads, slow long-term decay on backup copies that are written infrequently.
 *   Front-of-device and end-of-device backups are physically separated on
 *   rotating media, providing some independence.
 * \item \textbf{Legacy bootloader coexistence and large physical block sizes}.
 *   Sector 7 is adjacent to sector 0 (where ancient-style bootloaders live)
 *   and the start of the primary superblock (sector 8). On disks with logical
 *   sector size 512 and physical block size $\geq$\,4\,KB---increasingly
 *   common, especially on SSDs reporting 8\,KB or 16\,KB physical blocks---a
 *   sub-physical-block write triggers a read-modify-write of the whole
 *   physical block. A torn RMW on physical block 0 can take out the boot
 *   sector, the layout, and the start of the primary superblock together.
 *   The end-of-device backup superblock is the escape valve here, since it
 *   lives in its own physical block far away.
 * \item \textbf{Whole-device failure}. A single device dropping out: covered
 *   trivially by replication of the filesystem-wide state across all member
 *   devices' superblocks.
 * \end{itemize}
 *
 * \subsubsection{Fixed fields}
 *
 * The superblock header contains:
 *
 * \begin{itemize}
 * \item \textbf{Identity}: filesystem UUID (immutable), user-visible UUID
 *   (mutable), filesystem label (up to 32 bytes)
 * \item \textbf{Geometry}: block size, btree node size, number of devices
 * \item \textbf{Versioning}: current metadata version, minimum version of any
 *   data still on disk (see the Metadata versions section)
 * \item \textbf{State}: initialized and clean flags, sequence number, write
 *   timestamp
 * \item \textbf{Options}: all persistent filesystem options are encoded as
 *   bitfields in the superblock flags---replication counts, checksum and
 *   compression types, error handling policy, targets, quotas, journal
 *   parameters, and more. Mount options override these at runtime;
 *   \texttt{bcachefs set-fs-option} persists changes.
 * \end{itemize}
 *
 * \subsubsection{Variable-length fields}
 *
 * The superblock is extensible via type-tagged variable-length fields
 * (\texttt{BCH\_SB\_FIELD\_*}). Some are per-device (journal bucket lists);
 * most are shared across all devices (members, encryption, replicas, disk
 * groups, error log, recovery state). See the On disk format section for the
 * complete field list.
 *
 * Key fields for operators:
 *
 * \begin{description}
 * \item[\texttt{members\_v2}] Per-\hyperref[sec:devices]{device} metadata: UUID,
 *   bucket count and size, state (rw/ro/evacuating/spare), durability,
 *   data-type restrictions, error counters, performance measurements, and
 *   hardware identifiers.
 * \item[\texttt{disk\_groups}] Device label hierarchy and group definitions,
 *   used for target-based allocation (see
 *   \hyperref[sec:disk-groups]{Device labels and targets}).
 * \item[\texttt{replicas}] All unique replication configurations in the
 *   filesystem (see Replicas tracking).
 * \item[\texttt{clean}] Written on clean shutdown: contains btree roots and
 *   usage counters, allowing the next mount to skip
 *   \hyperref[sec:journal]{journal} replay entirely.
 * \item[\texttt{errors}] Persistent error log recording operational errors and
 *   fsck findings across mounts.
 * \item[\texttt{ext}] Extended metadata including required recovery passes and
 *   silenced errors.
 * \end{description}
 *
 * \subsubsection{Version upgrades}
 *
 * The superblock records both the current metadata version and the minimum
 * version of any data still on disk. This two-version scheme allows the
 * filesystem to upgrade incrementally: new data is written with the current
 * version while old data retains the format it was written with. The
 * \texttt{version\_upgrade} option controls upgrade behavior at mount time:
 * \texttt{compatible} (allow new features), \texttt{incompatible} (upgrade to
 * latest), or \texttt{none} (don't upgrade). Downgrade information is stored
 * separately so that a filesystem can be safely used by older tools after an
 * upgrade if no incompatible features have been used.
 *
 * \subsubsection{Consistency and self-healing}
 *
 * Every superblock copy is checksummed; reads validate the checksum and fall
 * back to alternative copies on failure. The sequence number provides
 * unambiguous ordering when copies disagree. The \texttt{recover-super} command
 * can reconstruct a completely overwritten superblock from the backup copies on
 * the same device or from any other device in the filesystem. Recovery passes
 * \texttt{check\_alloc\_info} and \texttt{check\_topology} verify that
 * superblock-recorded state matches the actual on-disk data.
 */

use crate::c;
use crate::c::{bch_sb, bch_sb_buf, bch_sb_field_crypt, bch_sb_handle, block_device, nonce};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::sb::members;
use crate::util::bitmap::LeBitmap;
use crate::util::projections::{AtomicWord, Volatile, VolatileWriter};
use crate::util::{Printbuf, ToText};
use crate::prt;
use crate::types::{bits_to_longs, ArmOf, Le16, Le32, Le64};
use crate::util::vstructs::{bytes_of_mut, cast_plain, cast_plain_mut, read_past, ArmRef, Head, InState, Plain, SelfSized, State, Unsized, Unvalidated, UnvalidatedEntries, VStruct, Validated, ValidatedEntries};
#[cfg(feature = "std")]
use crate::util::vstructs::VStructError;
use core::ffi::{c_int, c_uint, CStr};
use crate::util::alloc::KVVec;
use crate::util::bio::{Bio, BioOwner};
use crate::util::gather::Gather;
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::NSEC_PER_SEC;
use cstruct_macros::{c_const, c_export};
use core::mem::{size_of, size_of_val};
use arrayvec::ArrayVec;
use core::pin::Pin;
use pin_init::PinInit;

/// A superblock section's type: an arm of tagged_union! bch_sb_field, which
/// gives its field type - handed out as its ArmRef says, a short one, written
/// before its type grew, reading as zero past its end.
pub trait SbField: ArmOf<c::bch_sb_field, Tag = u32> + ArmRef {
    const FIELD_TYPE: c::bch_sb_field_type = c::bch_sb_field_type(Self::TAG);
    /// Each device has its own: copy_super() leaves it.
    const SINGLE_DEVICE: bool = c::BCH_SINGLE_DEVICE_SB_FIELDS & (1 << Self::TAG) != 0;
}

impl<F: ArmOf<c::bch_sb_field, Tag = u32> + ArmRef + ?Sized> SbField for F {}

/// How a section type is validated and printed - by the sections! table, as
/// its type says. Both get the section as read, Unvalidated, from a superblock
/// that's Unvalidated too: validating is what finds out whether it's valid,
/// and a printer prints one that isn't - the error saying it isn't does. One
/// too short to be read as its type fails validation before validate() is
/// called, and isn't printed.
///
/// And which way it's kept in step with memory: to_cpu() where the superblock
/// is the master copy, from_cpu() where memory is.
pub trait SbFieldOps: SbField + InState {
    fn validate(_sb: &bch_sb<Unvalidated>, _f: &Self::In<Unvalidated>, _flags: c::bch_validate_flags,
                _err: &mut Printbuf) -> Result<(), BchError> {
        Ok(())
    }

    fn to_text(_out: &mut Printbuf, _fs: Option<&Fs>, _sb: &bch_sb<Unvalidated>, _f: &Self::In<Unvalidated>) {}

    /// What memory has of the section, built from it.
    fn to_cpu(_fs: &Fs, _sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        Ok(())
    }

    /// The section written from memory, at a superblock write: counted there,
    /// too often to go through sb_lock, or regenerated from what this version
    /// knows.
    fn from_cpu(_fs: &Fs, _sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        Ok(())
    }

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

/// The entries @E of a section of type @F, as read - @b its bytes, from
/// @head on: those F::entries_end() finds, each of which fits.
pub fn entries_as_read<'a, F, E>(b: &'a [u8], head: usize) -> impl Iterator<Item = &'a E>
where
    F: SbFieldOps + ?Sized,
    E: Unsized<Head: Plain + SelfSized> + ?Sized + 'a,
{
    let end = F::entries_end(b).unwrap_or(head);
    UnvalidatedEntries::<E>::new(b.get(head..end).unwrap_or(&[])).map_while(Result::ok)
}

/// The section at @f in @sb, @len bytes, read with a partial entry past its
/// whole ones, which end at @end - C's readers ignored one: shrunk to them,
/// the rest of its last u64 zeroed as a writer leaves it, the sections after
/// it moved down, and a warning in @err. Repaired as it's read, as
/// validate() repairs version_incompat_allowed, and written whole
/// with the next superblock write; unrepaired, a reader would find no
/// section, or not all of it. The section itself doesn't move, so
/// validate()'s walk goes on from where the next one now is.
fn sb_field_drop_partial(sb: &mut bch_sb<Unvalidated>, f: usize, len: usize, end: usize, err: &mut Printbuf) {
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

/// The header of the section at @off of @sb, as read: its extent checked.
fn field_head(sb: &bch_sb<Unvalidated>, off: usize) -> &Head<c::bch_sb_field> {
    cast_plain(&sb.as_bytes()[off..]).expect("a section's header, its extent checked")
}

/// The section at @off of @sb validated as its type says: a partial entry at
/// its end repaired at read, invalid at write; then its type's own checks.
/// Nothing to do for a type this version doesn't know.
fn validate_at(sb: &mut bch_sb<Unvalidated>, off: usize, flags: c::bch_validate_flags, err: &mut Printbuf)
    -> Result<(), BchError>
{
    let ty = field_head(sb, off).type_.get();
    let Some(&min_len) = SECTION_MIN_LEN.get(ty as usize) else { return Ok(()) };

    let invalid = || Err(bch_errcode::BCH_ERR_invalid_sb_field_size.into());
    let too_short = |err: &mut Printbuf, len| {
        write!(err, "too short ({len} < {min_len} bytes)");
        invalid()
    };

    let len = field_head(sb, off).bytes();
    let f = c::bch_sb_field::<Unvalidated>::from_bytes(&sb.as_bytes()[off..])
        .expect("a section, its extent checked");
    let Some(end) = f.entries_end() else {
        return too_short(err, len);
    };
    if end.next_multiple_of(size_of::<u64>()) < len {
        if flags.contains(c::bch_validate_flags::BCH_VALIDATE_write) {
            write!(err, "{} bytes past its last whole entry", len - end);
            return invalid();
        }
        sb_field_drop_partial(sb, off, len, end, err);
    }

    let sb = &*sb;
    let f = c::bch_sb_field::<Unvalidated>::from_bytes(&sb.as_bytes()[off..])
        .expect("a section, its extent checked");
    f.validate(sb, flags, err).unwrap_or_else(|| too_short(err, f.as_bytes().len()))
}

// ── The sections, by type ────────────────────────────────────────────────

const FIELD_NR: usize = c::bch_sb_field_type::BCH_SB_FIELD_NR.0 as usize;
const _: () = assert!(FIELD_NR <= 64, "bch_sb_buf.dirty: a bit per section type");

macro_rules! sb_fields {
    ($(($name:ident, $nr:literal, $($desc:literal)*)),* $(,)?) => {
        /// By type.
        const FIELD_NAMES: [&str; FIELD_NR] = [$(stringify!($name)),*];

        // by type, as C's table is by position: in order, from 0
        const _: () = {
            let mut i = 0;
            $(assert!($nr == i, "BCH_SB_FIELDS(): types out of order"); i += 1;)*
            let _ = i;
        };
    };
}
c::BCH_SB_FIELDS!(sb_fields);

/// A section's to_cpu() or from_cpu(), as the union hands it out.
pub type SbSyncFn = fn(&Fs, &mut SbLockGuard<'_>) -> Result<(), BchError>;

/// The type a section's SbFieldOps are of: its arm - ext's, a grown type's
/// family, not its C struct.
macro_rules! section_ops {
    (ext)          => { c::bch_sb_field_ext<Validated> };
    ($name:ident)  => { ::paste::paste! { c::[<bch_sb_field_ $name>] } };
}

/// The section union's ops: each arm's SbFieldOps, by its tag - typed().
macro_rules! sb_field_ops {
    ($(($name:ident, $nr:literal, $($desc:literal)*)),* $(,)?) => { ::paste::paste! {
        /// Each type's shortest: what a section is too short to be read as
        /// one under, to say so.
        const SECTION_MIN_LEN: [usize; FIELD_NR] = [$(<section_ops!($name) as ArmRef>::MIN_LEN),*];

        /// Each type's entries_end(): by the tag, not typed() - it's what
        /// finds a partial entry, which typed() can't read the arm past.
        const SECTION_ENTRIES_END: [fn(&[u8]) -> Option<usize>; FIELD_NR] =
            [$(<section_ops!($name) as SbFieldOps>::entries_end),*];

        impl c::bch_sb_field<Unvalidated> {
            /// Where its whole entries end, as its type says: None if it's
            /// too short to be one, or of a type this version doesn't know.
            fn entries_end(&self) -> Option<usize> {
                SECTION_ENTRIES_END.get(self.type_.get() as usize)?(self.as_bytes())
            }

            /// Its type's validate(): None if it can't be read as one.
            fn validate(&self, sb: &bch_sb<Unvalidated>, flags: c::bch_validate_flags, err: &mut Printbuf)
                -> Option<Result<(), BchError>>
            {
                match self.typed() {
                    $(Ok(c::BchSbFieldRef::$name(f)) =>
                        Some(<section_ops!($name) as SbFieldOps>::validate(sb, f, flags, err)),)*
                    Err(_) => None,
                }
            }

            /// Its type's to_text(): nothing, if it's too short to be one, or
            /// of a type this version doesn't know.
            fn to_text(&self, out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb<Unvalidated>) {
                match self.typed() {
                    $(Ok(c::BchSbFieldRef::$name(f)) =>
                        <section_ops!($name) as SbFieldOps>::to_text(out, fs, sb, f),)*
                    Err(_) => {}
                }
            }
        }

        impl c::bch_sb_field<Validated> {
            /// Its type's to_cpu(): None for a type this version doesn't know.
            fn to_cpu_fn(&self) -> Option<SbSyncFn> {
                match self.typed().ok()? {
                    $(c::BchSbFieldRef::$name(_) => Some(<section_ops!($name) as SbFieldOps>::to_cpu),)*
                }
            }
        }
    } };
}
c::BCH_SB_FIELDS!(sb_field_ops);

impl c::bch_sb_field_type {
    /// Its name: None for a type this version doesn't know.
    pub fn name(self) -> Option<&'static str> {
        FIELD_NAMES.get(self.0 as usize).copied()
    }

    /// The type named @name.
    pub fn from_name(name: &str) -> Option<Self> {
        FIELD_NAMES.iter().position(|&n| n == name).map(|t| Self(t as u32))
    }
}

/// The section at @off validated by its type's ops - and if it's invalid,
/// said so in @err, with what it holds. A repair made as it's read is said
/// too.
fn field_validate(sb: &mut bch_sb<Unvalidated>, off: usize,
                  flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
    let ty = field_head(sb, off).type_.get();
    let name = c::bch_sb_field_type(ty).name().unwrap_or("(unknown)");
    let mut field_err = Printbuf::new();

    let ret = validate_at(sb, off, flags, &mut field_err);

    if ret.is_err() {
        write!(err, "Invalid superblock section {name}: {}", field_err.as_str());
        err.newline();
        let f = c::bch_sb_field::<Unvalidated>::from_bytes(&sb.as_bytes()[off..])
            .expect("a section, its extent checked");
        field_to_text(err, None, sb, f);
    } else if !field_err.as_str().is_empty() {
        // repaired as read: bch2_read_super() prints it
        write!(err, "Superblock section {name}: {}", field_err.as_str());
        err.newline();
    }
    ret
}

/// Section @f as its type's ops print it.
pub fn field_ops_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb<Unvalidated>,
                         f: &c::bch_sb_field<Unvalidated>) {
    if out.nr_tabstops() == 0 {
        out.tabstop_push(32);
    }

    f.to_text(out, fs, sb);
}

/// Section @f: its type, its size, and what it holds.
pub fn field_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb<Unvalidated>,
                     f: &c::bch_sb_field<Unvalidated>) {
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

macro_rules! feature_names {
    ($(($name:ident, $nr:literal)),* $(,)?) => {
        /// The features' names, by bit.
        const FEATURE_NAMES: [&str; c::BCH_FEATURE_NR as usize] = [$(stringify!($name)),*];

        // by bit, as C's table is by position: in order, from 0
        const _: () = {
            let mut i = 0;
            $(assert!($nr == i, "BCH_SB_FEATURES(): bits out of order"); i += 1;)*
            let _ = i;
        };
    };
}
c::BCH_SB_FEATURES!(feature_names);

/// Superblock @sb checked - its header, its layout, every section - before
/// anything trusts it: as read from @read_offset, or about to be written,
/// BCH_VALIDATE_write. Read, a few things are repaired in place: an
/// incompat_allowed older versions didn't downgrade, a partial entry at a
/// section's end. What passes is Validated.
pub fn validate<'a>(sb: &'a mut bch_sb<Unvalidated>, opts: &c::bch_opts, read_offset: u64,
                    flags: c::bch_validate_flags, out: &mut Printbuf) -> Result<&'a mut bch_sb, BchError> {
    let write = flags.contains(c::bch_validate_flags::BCH_VALIDATE_write);

    sb_compatible(sb, out)?;

    if opts.no_version_check == 0 {
        let incompat = sb.features[0].get() & (!0u64 << c::BCH_FEATURE_NR);
        let incompat_bit = if incompat != 0 {
            incompat.trailing_zeros()
        } else if sb.features[1].get() != 0 {
            64 + sb.features[1].get().trailing_zeros()
        } else {
            0
        };

        if incompat_bit != 0 {
            write!(out, "Filesystem has incompatible feature bit {incompat_bit}, highest supported {} ({})",
                   FEATURE_NAMES[c::BCH_FEATURE_NR as usize - 1], c::BCH_FEATURE_NR - 1);
            return Err(bch_errcode::BCH_ERR_invalid_sb_features.into());
        }

        let current = c::bcachefs_metadata_version_current as u64;
        if c::BCH_VERSION_MAJOR(sb.version.get().into()) > c::BCH_VERSION_MAJOR(current) ||
           sb.version_incompat() > current {
            prt!(out, "Filesystem has incompatible version {}, current version {}",
                 version(sb.version.get()), version(current as u32));
            return Err(bch_errcode::BCH_ERR_invalid_sb_features.into());
        }
    }

    if sb.user_uuid.b == [0; 16] {
        write!(out, "Bad user UUID (got zeroes)");
        return Err(bch_errcode::BCH_ERR_invalid_sb_uuid.into());
    }

    if sb.uuid.b == [0; 16] {
        write!(out, "Bad internal UUID (got zeroes)");
        return Err(bch_errcode::BCH_ERR_invalid_sb_uuid.into());
    }

    if !write && sb.offset.get() != read_offset {
        write!(out, "Bad sb offset (got {}, read from {read_offset})", sb.offset.get());
        return Err(bch_errcode::BCH_ERR_invalid_sb_offset.into());
    }

    if sb.nr_devices == 0 || u32::from(sb.nr_devices) > c::BCH_SB_MEMBERS_MAX {
        write!(out, "Bad number of member devices {} (max {})", sb.nr_devices, c::BCH_SB_MEMBERS_MAX);
        return Err(bch_errcode::BCH_ERR_invalid_sb_too_many_members.into());
    }

    if sb.dev_idx >= sb.nr_devices {
        write!(out, "Bad dev_idx (got {}, nr_devices {})", sb.dev_idx, sb.nr_devices);
        return Err(bch_errcode::BCH_ERR_invalid_sb_dev_idx.into());
    }

    let precision = sb.time_precision.get();
    if precision == 0 || u64::from(precision) > NSEC_PER_SEC {
        write!(out, "Invalid time precision: {precision} (min 1, max {NSEC_PER_SEC})");
        return Err(bch_errcode::BCH_ERR_invalid_sb_time_precision.into());
    }

    // old versions didn't know to downgrade this field
    if sb.version_incompat_allowed() > sb.version.get().into() {
        let v = sb.version.get().into();
        sb.set_version_incompat_allowed(v);
    }

    if sb.version_incompat() > sb.version_incompat_allowed() {
        prt!(out, "Invalid version_incompat {} > incompat_allowed {}",
             version(sb.version_incompat() as u32), version(sb.version_incompat_allowed() as u32));
        if write {
            return Err(bch_errcode::BCH_ERR_invalid_sb_version.into());
        }
        let v = sb.version_incompat();
        sb.set_version_incompat_allowed(v);
    }

    if sb.nr_devices > 1 {
        sb.set_multi_device(true);
    }

    #[cfg(kernel)]
    if sb.shard_inums_nbits() == 0 {
        let btree_node_bytes = sb.btree_node_size() << 9;
        // the members, as C reads them - not yet validated
        let fs_size = (0..sb.number_of_devices()).map(|i| {
            let m = crate::sb::members::sb_member(sb, i).unwrap_or_default();
            m.nbuckets.get() * u64::from(m.bucket_size.get())
        }).sum::<u64>();
        let bits = crate::inode::shard_bits_default(
            crate::util::kernel::num_online_cpus(), fs_size << 9, btree_node_bytes);
        sb.set_shard_inums_nbits(bits.into());
    }

    validate_layout(&sb.layout, out)?;

    // every section within the superblock, before any is read
    let b = sb.as_bytes();
    let mut off = size_of::<Head<bch_sb>>();
    while off < b.len() {
        let Some(f) = cast_plain::<Head<c::bch_sb_field>>(&b[off..]) else {
            write!(out, "Invalid superblock: optional field extends past end of superblock");
            return Err(bch_errcode::BCH_ERR_invalid_sb_field_size.into());
        };
        if f.u64s.get() == 0 {
            write!(out, "Invalid superblock: optional field with size 0 (type {})", f.type_.get());
            return Err(bch_errcode::BCH_ERR_invalid_sb_field_size.into());
        }
        off += f.u64s.get() as usize * size_of::<u64>();
        if off > b.len() {
            write!(out, "Invalid superblock: optional field extends past end of superblock (type {})",
                   f.type_.get());
            return Err(bch_errcode::BCH_ERR_invalid_sb_field_size.into());
        }
    }

    // members must be validated first
    let Some(mi) = field_offset(sb, c::bch_sb_field_type::BCH_SB_FIELD_members_v2)
        .or_else(|| field_offset(sb, c::bch_sb_field_type::BCH_SB_FIELD_members_v1)) else {
        write!(out, "Invalid superblock: member info area missing");
        return Err(bch_errcode::BCH_ERR_invalid_sb_members_missing.into());
    };
    field_validate(sb, mi, flags, out)?;

    // each section, members_v2 again: a section's validate may shrink it, the
    // sections after moved down, so its length is read after
    let mut off = size_of::<Head<bch_sb>>();
    while off < sb.as_bytes().len() {
        if field_head(sb, off).type_.get() != c::bch_sb_field_type::BCH_SB_FIELD_members_v1.0 {
            field_validate(sb, off, flags, out)?;
        }
        off += field_head(sb, off).bytes();
    }

    if write {
        let m = crate::sb::members::sb_member(sb, sb.dev_idx.into()).unwrap_or_default();
        if m.seq != sb.seq {
            write!(out, "Invalid superblock: member seq {} != sb seq {}", m.seq.get(), sb.seq.get());
            return Err(bch_errcode::BCH_ERR_invalid_sb_members_missing.into());
        }
    }

    // SAFETY: what Validated says of it is true - every check above passed
    Ok(unsafe { sb.assume_validated_mut() })
}

/// validate(), of the superblock at the start of @buf - read into it whole:
/// its count saying it's longer than @buf is an error too.
pub fn validate_bytes<'a>(buf: &'a mut [u8], opts: &c::bch_opts, read_offset: u64,
                          flags: c::bch_validate_flags, out: &mut Printbuf) -> Result<&'a mut bch_sb, BchError> {
    let sb = bch_sb::<Unvalidated>::from_bytes_mut(buf).map_err(|e| {
        write!(out, "Invalid superblock: {e}");
        BchError::from(bch_errcode::BCH_ERR_invalid_sb_too_big)
    })?;
    validate(sb, opts, read_offset, flags, out)
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
        version.0 <= self.sb.version_incompat.get().into() ||
            (self.feature(c::bch_sb_feature::BCH_FEATURE_incompat_version_field) &&
             version.0 <= self.sb.version_incompat_allowed.get().into())
    }

    /// Make the filesystem incompatible with versions before @version, if
    /// that's allowed, writing the superblock: bch2_set_version_incompat().
    /// If it isn't, the first request for each version says so.
    pub fn set_version_incompat(&self, version: c::bcachefs_metadata_version) -> Result<(), BchError> {
        if self.incompat_feature_allowed(version) {
            let mut sb = self.sb_lock();
            if version.0 > self.sb.version_incompat.get().into() {
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
                               version, self::version(self.sb.version_incompat_allowed.get()));
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
        let (v, allowed) = (self.sb.version.get(), self.sb.version_incompat_allowed.get());
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
fn check_version_downgrade(fs: &Fs, sb: &mut SbLockGuard<'_>) -> bool {
    let current = c::bcachefs_metadata_version_current;
    let s = sb.sb_mut();

    if s.version_upgrade_complete() > current.into() {
        s.set_version_upgrade_complete(current.into());
    }
    if s.version_incompat_allowed() > current.into() {
        s.set_version_incompat_allowed(current.into());
    }
    if u32::from(fs.sb.version.get()) > current {
        s.version = Le16::new(current as u16);
    }
    if u32::from(fs.sb.version_min.get()) > current {
        s.version_min = Le16::new(current as u16);
    }
    s.compat[0] = Le64::new(s.compat[0].get() & ((1 << c::bch_sb_compat::BCH_COMPAT_NR.0) - 1));

    current < fs.sb.version.get().into()
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
fn sb_upgrade(fs: &Fs, sb: &mut SbLockGuard<'_>, new: u32, incompat: bool) {
    if c::BCH_VERSION_MAJOR(new.into()) > c::BCH_VERSION_MAJOR(sb.sb().version.get().into()) {
        fs.sb_resize(sb).field_resize::<c::bch_sb_field_downgrade>(0);
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
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    sb_upgrade(fs, &mut unsafe { fs.sb_lock_held() }, new_version, incompat)
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
pub fn sb_to_text(out: &mut Printbuf, fs: Option<&Fs>, sb: &bch_sb<Unvalidated>, print_layout: bool, fields: u32) {
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
    write!(out, "Devices:\t{}\n", sb.number_of_devices());

    write!(out, "Sections:\t");
    let have = sb.entries().map_while(Result::ok).fold(0u64, |m, f| m | 1u64.checked_shl(f.type_.get()).unwrap_or(0));
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

    let printed = |f: &&c::bch_sb_field<Unvalidated>| 1u32.checked_shl(f.type_.get()).is_some_and(|b| fields & b != 0);
    for f in sb.entries().map_while(Result::ok).filter(printed) {
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
    /// Its section of type @F, if it has one, over the bytes its header says
    /// are its - one written before F grew reads as zero past them.
    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        field_get_id(self, F::FIELD_TYPE)?.arm::<F>()
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

impl bch_sb_buf {
    pub fn sb(&self) -> &bch_sb {
        // SAFETY: C read and validated it, and owns its buffer while we're
        // borrowed - resize and realloc take &mut
        unsafe { bch_sb::<Validated>::from_ffi(self.sb) }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        // SAFETY: as sb()'s
        unsafe { bch_sb::<Validated>::from_ffi_mut(self.sb) }
    }

    pub fn field<F: SbField + ?Sized>(&self) -> Option<&F> {
        self.sb().field()
    }

    /// To edit: a field past the section's end is no write it will carry -
    /// whole_mut() for all of it, if it's all there.
    pub fn field_mut<F: SbField + ?Sized>(&mut self) -> Option<&mut F> {
        self.mark_dirty(F::FIELD_TYPE);
        // SAFETY: the handle's superblock, borrowed mutably with it
        unsafe { sb_field_get_raw_mut(self.sb) }
    }

    /// Section @ty edited: to_cpu() it, after the next write, if this is the
    /// filesystem's.
    fn mark_dirty(&mut self, ty: c::bch_sb_field_type) {
        self.dirty |= 1 << ty.0;
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

impl c::bch_sb_bdev {
    pub fn bdev(&self) -> &block_device {
        unsafe { &*self.bdev }
    }

    /// The device closed, and what names it freed.
    fn free(&mut self) {
        use crate::util::alloc::kfree;
        use core::alloc::Layout;
        use core::ptr::NonNull;

        if !self.s_bdev_file.is_null() {
            // SAFETY: the file the device was opened with
            unsafe { c::bdev_fput(self.s_bdev_file) };
        }
        // SAFETY: read_super_and_backups()'s kzalloc()s, or null
        unsafe {
            if let Some(p) = NonNull::new(self.holder) {
                kfree(p.cast(), Layout::new::<c::bch_sb_handle_holder>());
            }
            if let Some(p) = NonNull::new(self.sb_name) {
                let len = CStr::from_ptr(p.as_ptr().cast()).to_bytes_with_nul().len();
                kfree(p.cast(), Layout::array::<u8>(len).expect("the name's layout, as allocated"));
            }
        }
    }
}

impl c::bch_sb_io {
    /// Room for a superblock of @u64s - 0, the minimum - in a buffer a power
    /// of two, at least @dev's logical block; past what its layout allows,
    /// -ENOSPC. And with it, a bio of as many bvecs as that maps to: made
    /// apart from the buffer, so one that failed is made again next time.
    fn realloc(&mut self, dev: &c::bch_sb_bdev, u64s: u32) -> Result<(), BchError> {
        let mut bytes = size_of::<Head<bch_sb>>() + u64s as usize * size_of::<u64>();
        if !dev.bdev.is_null() {
            bytes = bytes.max(crate::util::kernel::bdev_logical_block_size(dev.bdev()) as usize);
        }
        let size = bytes.next_power_of_two();

        if self.buf.sb.is_null() || self.buf.buffer_size < size {
            self.grow(dev, bytes, size)?;
        }

        if self.bits.have_bio() != 0 {
            use crate::util::bio::{Bio, BioBuf, RawBio};

            let nr = crate::util::bio::buf_nr_bvecs(self.buf.sb.cast(), self.buf.buffer_size);
            // SAFETY: ours, from the last realloc(), idle
            if !self.bio.is_null() && usize::from(unsafe { (*self.bio).bi_max_vecs }) >= nr {
                return Ok(());
            }

            let bio = BioBuf::<Bio>::new::<BchError, _>(nr.try_into().expect("a superblock's worth of bvecs"),
                                                        core::ptr::null_mut(), 0, |b| b.init())
                .map_err(|_| BchError::from(c::bch_errcode::BCH_ERR_ENOMEM_sb_bio_realloc))?;
            if !self.bio.is_null() {
                // SAFETY: the bio's ours, from the last realloc()
                drop(unsafe { BioBuf::<Bio>::from_raw(self.bio) });
            }
            self.bio = bio.into_raw();
        }
        Ok(())
    }

    /// The buffer grown to @size, for a superblock of @bytes: past what its
    /// layout allows, -ENOSPC.
    fn grow(&mut self, dev: &c::bch_sb_bdev, bytes: usize, size: usize) -> Result<(), BchError> {
        if !self.buf.sb.is_null() && self.bits.have_layout() != 0 {
            let max = 512u64 << self.buf.sb().layout.sb_max_size_bits;
            if bytes as u64 > max {
                let bdev = dev.bdev();
                crate::util::log::log_no_fs(crate::util::log::LogLevel::ERR, &crate::util::printbuf::Msg(|out: &mut Printbuf| {
                    crate::util::printbuf::prt_bdevname(out, Some(bdev));
                    write!(out, ": superblock too big: want {bytes} but have {max}");
                }));
                return Err(c::bch_errcode::BCH_ERR_ENOSPC_sb.into());
            }
        }

        self.buf.grow(size).map_err(|_| BchError::from(c::bch_errcode::BCH_ERR_ENOMEM_sb_buf_realloc))
    }

    /// The buffer and bio freed.
    fn free(&mut self) {
        use crate::util::bio::{Bio, BioBuf, RawBio};

        if !self.bio.is_null() {
            // SAFETY: realloc()'s
            drop(unsafe { BioBuf::<Bio>::from_raw(self.bio) });
        }
        self.buf.free();
    }
}

impl bch_sb_handle {
    pub fn sb_realloc(&mut self, u64s: u32) -> Result<(), BchError> {
        self.io.realloc(&self.dev, u64s)
    }

    /// What it holds released, and it zeroed: bch2_free_super().
    pub fn free(&mut self) {
        self.io.free();
        self.dev.free();
        // SAFETY: all zeroes is a handle that holds nothing
        unsafe { core::ptr::write_bytes(core::ptr::from_mut(self), 0, 1) };
    }
}

impl core::ops::Deref for bch_sb_handle {
    type Target = bch_sb_buf;

    fn deref(&self) -> &bch_sb_buf {
        &self.io.buf
    }
}

impl core::ops::DerefMut for bch_sb_handle {
    fn deref_mut(&mut self) -> &mut bch_sb_buf {
        &mut self.io.buf
    }
}

impl Drop for bch_sb_handle {
    fn drop(&mut self) {
        self.free();
    }
}

#[c_export]
pub fn bch2_free_super(sb: &mut c::bch_sb_handle) {
    sb.free();
}

// ── Reading a device's superblock ────────────────────────────────────────

/// A handle's bio, borrowed apart from the buffer it reads into and writes
/// from.
struct HandleBio<'a>(&'a Bio);

// SAFETY: the handle's, from realloc(): nothing else has it while the
// handle's borrowed
unsafe impl BioOwner for HandleBio<'_> {
    fn bio(&self) -> &Bio {
        self.0
    }
}

impl c::bch_sb_io {
    /// The bio, set up for an IO by @opf at @sector of @dev - with the
    /// buffer it maps.
    fn bio(&mut self, dev: &c::bch_sb_bdev, opf: u32, sector: u64) -> (HandleBio<'_>, &mut [u8]) {
        assert!(!self.bio.is_null(), "superblock IO without a bio: have_bio wasn't set");
        // SAFETY: realloc()'s, borrowed with us
        let mut bio = HandleBio(unsafe { Bio::from_raw(self.bio) });
        bio.reset(dev.bdev, opf);
        bio.set_sector(sector);
        (bio, self.buf.buffer_mut())
    }

    /// @bytes from @sector of @dev into the buffer.
    fn read(&mut self, dev: &c::bch_sb_bdev, sector: u64, bytes: usize) -> Result<(), BchError> {
        let (mut bio, buf) = self.bio(dev, c::req_op_REQ_OP_READ | c::REQ_SYNC | c::REQ_META, sector);
        // bound, so the Mapped drops before the bio it borrows
        let ret = bio.map_read(&mut buf[..bytes]).submit_wait();
        ret
    }

    /// The buffer's first @bytes to @sector of @dev, FUA.
    fn write_fua(&mut self, dev: &c::bch_sb_bdev, sector: u64, bytes: usize) -> Result<(), BchError> {
        let (mut bio, buf) = self.bio(dev, c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_META | c::REQ_FUA,
                                      sector);
        // as read()'s
        let ret = bio.map_write(&buf[..bytes]).submit_wait();
        ret
    }
}

/// A superblock's checksum, of @bytes - all of it past the csum, its first
/// field: csum_vstruct().
fn sb_csum(fs: Option<&Fs>, ty: c::bch_csum_type, bytes: &[u8]) -> c::bch_csum {
    let start = size_of::<c::bch_csum>();
    let c = fs.map_or(core::ptr::null_mut(), Fs::as_ptr);
    // SAFETY: @bytes past the csum; no filesystem, no key: then @ty isn't an
    // encryption type
    unsafe { c::bch2_checksum(c, ty.0, nonce::default(), bytes[start..].as_ptr().cast(), bytes.len() - start) }
}

/// The superblock at @offset read into @sb's buffer - grown as it needs - and
/// checked as far as that takes: that it's one, of a version we read, whole,
/// and its checksum. Not validated.
fn read_one_super(sb: &mut bch_sb_handle, offset: u64, err: &mut Printbuf) -> Result<(), BchError> {
    loop {
        let len = sb.io.buf.buffer_size;
        sb.io.read(&sb.dev, offset, len).inspect_err(|e| prt!(err, "IO error: {}", -e.raw()))?;

        let buf: &[u8] = sb.io.buf.buffer_mut();
        let h = cast_plain::<Head<bch_sb>>(buf).expect("a superblock buffer, holding a header");

        if h.magic.b != BCACHE_MAGIC && h.magic.b != BCHFS_MAGIC {
            prt!(err, "Not a bcachefs superblock (got magic ");
            err.uuid(&h.magic);
            prt!(err, ")");
            return Err(bch_errcode::BCH_ERR_invalid_sb_magic.into());
        }

        sb_compatible(h, err)?;

        let bytes = size_of::<Head<bch_sb>>() + h.u64s() as usize * size_of::<u64>();
        let max = 512u64 << u32::from(h.layout.sb_max_size_bits).min(c::BCH_SB_LAYOUT_SIZE_BITS_MAX);
        if bytes as u64 > max {
            prt!(err, "Invalid superblock: too big (got {bytes} bytes, layout max {max})");
            return Err(bch_errcode::BCH_ERR_invalid_sb_too_big.into());
        }

        if bytes > buf.len() {
            let u64s = h.u64s() as u32;
            sb.sb_realloc(u64s)?;
            continue;
        }

        let ty = c::bch_csum_type(h.csum_type() as u32);
        if ty.0 >= c::bch_csum_type::BCH_CSUM_NR.0 || ty.is_encryption() {
            prt!(err, "unknown checksum type {}", h.csum_type());
            return Err(bch_errcode::BCH_ERR_invalid_sb_csum_type.into());
        }

        // XXX: verify MACs
        let csum = sb_csum(None, ty, &buf[..bytes]);
        if csum != h.csum {
            crate::data::checksum::csum_err_msg(err, ty, h.csum, csum);
            return Err(bch_errcode::BCH_ERR_invalid_sb_csum.into());
        }

        sb.io.seq = h.seq.get();
        return Ok(());
    }
}

/// The layout, from its own sector - read through @sb's buffer, aligned for
/// IO as one on the stack isn't - and validated.
fn read_layout_sector(sb: &mut bch_sb_handle, err: &mut Printbuf) -> Result<c::bch_sb_layout, BchError> {
    sb.io.read(&sb.dev, c::BCH_SB_LAYOUT_SECTOR.into(), size_of::<c::bch_sb_layout>())?;
    let layout = read_past::<c::bch_sb_layout>(sb.io.buf.buffer_mut(), 0);
    validate_layout(&layout, err)?;
    Ok(layout)
}

/// @layout's backups read - the primary's tried already, and valid if
/// @primary_valid, in @sb's buffer - for the offset of the copy with the
/// highest seq, of all of them: in the buffer, after.
///
/// Reading every copy defends against several failure modes:
///   - a torn write to the primary: its csum fails; a backup at the same seq
///     is taken
///   - a stale primary, from a write that reached slot 1+ but not it: an
///     older seq in slot 0; the backup with the higher one is taken
///   - bit rot in a backup not read since its last write: noticed when the
///     primary is down too
///
/// The last read wins a tie, so the common case - every slot at the same
/// seq - reads nothing again.
fn read_backup_supers(sb: &mut bch_sb_handle, layout: &c::bch_sb_layout, primary_valid: bool,
                      err: &mut Printbuf) -> Result<u64, BchError> {
    let primary = layout.sb_offset[0].get();
    let mut best_seq = if primary_valid { sb.io.seq } else { 0 };
    let mut best = primary;
    let mut last_read = if primary_valid { primary } else { 0 };
    let mut any_valid = primary_valid;

    for o in &layout.sb_offset[1..usize::from(layout.nr_superblocks)] {
        let offset = o.get();

        let mut slot_err = Printbuf::new();
        let ret = read_one_super(sb, offset, &mut slot_err);
        // read into the buffer, valid or not
        last_read = offset;
        if ret.is_err() {
            prt!(err, "  sb @ {offset}: ");
            err.write_bytes(slot_err.as_bytes());
            prt!(err, "\n");
            continue;
        }

        any_valid = true;
        if sb.io.seq >= best_seq {
            best_seq = sb.io.seq;
            best = offset;
        }
    }

    if !any_valid {
        return Err(bch_errcode::BCH_ERR_invalid.into());
    }

    if last_read != best {
        read_one_super(sb, best, err)?;
    }
    Ok(best)
}

/// The primary was unreadable, failed validation, or was older than a
/// backup. The good copy goes back in slot 0 now, rather than leaving it for
/// the next superblock write - which reads back slot 0 first, and would take
/// a torn primary for another writer's.
fn rewrite_primary_super(sb: &mut bch_sb_handle) -> Result<(), BchError> {
    let s = sb.io.buf.sb_mut();
    s.offset = s.layout.sb_offset[0];
    let (ty, sector) = (c::bch_csum_type(s.csum_type() as u32), s.offset.get());
    let csum = sb_csum(None, ty, sb.io.buf.sb_bytes());
    sb.io.buf.sb_mut().csum = csum;

    let bytes = sb.io.buf.sb_bytes().len()
        .next_multiple_of(crate::util::kernel::bdev_logical_block_size(sb.dev.bdev()) as usize);
    sb.io.write_fua(&sb.dev, sector, bytes)
}

/// The shim's buffered open, without direct_io: nothing, in the kernel.
fn open_buffered(opts: &c::bch_opts) -> c::blk_mode_t {
    #[cfg(not(kernel))]
    return if crate::opt_get!(opts, direct_io) == 0 { c::BLK_OPEN_BUFFERED } else { 0 };
    #[cfg(kernel)]
    {
        let _ = opts;
        0
    }
}

/// For a device whose logical block is bigger than the filesystem's: in
/// userspace, buffered IO instead - the read's to be retried, direct_io off.
/// Whether it is.
fn buffered_instead(opts: &mut c::bch_opts) -> bool {
    #[cfg(not(kernel))]
    {
        crate::opt_set!(opts, direct_io, 0);
        true
    }
    #[cfg(kernel)]
    {
        let _ = opts;
        false
    }
}

/// The header of what read_one_super() read, in @sb's buffer.
fn header(sb: &mut bch_sb_handle) -> &Head<bch_sb> {
    cast_plain(sb.io.buf.buffer_mut()).expect("a superblock buffer, holding a header")
}

fn read_super_and_backups(path: &CStr, opts: &mut c::bch_opts, err: &mut Printbuf)
    -> Result<bch_sb_handle, BchError>
{
    use crate::util::alloc::kzalloc;
    use core::alloc::Layout;

    let mut sb = bch_sb_handle::default();
    sb.dev.mode = c::BLK_OPEN_READ;
    sb.io.bits.set_have_bio(1);

    let enomem = |_| BchError::from(c::ENOMEM);
    sb.dev.holder = kzalloc(Layout::new::<c::bch_sb_handle_holder>()).map_err(enomem)?.as_ptr().cast();
    let name = path.to_bytes_with_nul();
    let p = kzalloc(Layout::array::<u8>(name.len()).map_err(|_| BchError::from(c::ENOMEM))?).map_err(enomem)?;
    // SAFETY: name.len() bytes, just allocated
    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), p.as_ptr(), name.len()) };
    sb.dev.sb_name = p.as_ptr().cast();

    sb.dev.mode |= open_buffered(opts);
    if crate::opt_get!(opts, noexcl) == 0 {
        sb.dev.mode |= c::BLK_OPEN_EXCL;
    }
    if crate::opt_get!(opts, nochanges) == 0 {
        sb.dev.mode |= c::BLK_OPEN_WRITE;
    }

    let open = |mode: c::blk_mode_t, holder: *mut c::bch_sb_handle_holder| {
        // SAFETY: a C string, and our holder; the ops are static
        let f = unsafe { c::bdev_file_open_by_path(path.as_ptr().cast(), mode, holder.cast(),
                                                   &raw const c::bch2_sb_handle_bdev_ops) };
        crate::errcode::errptr_to_result(core::ptr::NonNull::new(f).expect("bdev_file_open_by_path(): an ERR_PTR, not NULL"))
    };

    let mut file = open(sb.dev.mode, sb.dev.holder);
    if matches!(&file, Err(e) if e.raw() == c::EACCES as i32) && crate::opt_get!(opts, read_only) != 0 {
        sb.dev.mode &= !c::BLK_OPEN_WRITE;
        file = open(sb.dev.mode, sb.dev.holder);
        if file.is_ok() {
            crate::opt_set!(opts, nochanges, 1);
        }
    }

    let file = file.inspect_err(|e| {
        // Detail only - bch2_read_super() has already printed the path and
        // the errno, and repeating them here just gets the same words
        // twice. The errno alone is not actionable: what the user needs is
        // what we asked the block layer for, since that - not the device -
        // is usually what's wrong.
        prt!(err, "  requested {}{}\n",
             if sb.dev.mode & c::BLK_OPEN_WRITE != 0 { "read-write" } else { "read-only" },
             if sb.dev.mode & c::BLK_OPEN_EXCL != 0 { ", exclusive" } else { "" });

        if e.raw() == c::EACCES as i32 {
            prt!(err, "  insufficient privilege (try root), or a write protected device\n  (check blockdev --getro)\n");
        } else if e.raw() == c::EBUSY as i32 {
            prt!(err, "  device is in use: already mounted, or held by another process\n");
        }
    })?;

    sb.dev.s_bdev_file = file.as_ptr();
    // SAFETY: the file just opened
    sb.dev.bdev = unsafe { c::file_bdev(file.as_ptr()) };

    sb.sb_realloc(0)?;

    let (sb_offset, primary_bad) = if crate::opt_defined!(opts, sb) {
        // a superblock asked for, for recovery or debugging: that one, no
        // scan or fallback
        let offset = crate::opt_get!(opts, sb);
        read_one_super(&mut sb, offset, err)?;
        (offset, false)
    } else {
        // the primary first, for its layout; the layout's own sector if it
        // can't be read
        let mut primary_err = Printbuf::new();
        match read_one_super(&mut sb, c::BCH_SB_SECTOR.into(), &mut primary_err) {
            Ok(()) => {
                let primary_seq = sb.io.seq;
                let layout = header(&mut sb).layout;
                validate_layout(&layout, err)?;
                let offset = read_backup_supers(&mut sb, &layout, true, err)?;
                (offset, primary_seq < sb.io.seq)
            }
            Err(_) => {
                prt!(err, "primary superblock unreadable: ");
                err.write_bytes(primary_err.as_bytes());
                prt!(err, "\n");
                let layout = read_layout_sector(&mut sb, err)?;
                (read_backup_supers(&mut sb, &layout, false, err)?, true)
            }
        }
    };

    let block_bytes = u32::from(header(&mut sb).block_size.get()) << 9;
    let dev_block = crate::util::kernel::bdev_logical_block_size(sb.dev.bdev());
    if block_bytes < dev_block && crate::opt_get!(opts, direct_io) != 0 {
        if buffered_instead(opts) {
            return Err(c::EINTR.into());
        }
        prt!(err, "block size ({block_bytes}) smaller than device block size ({dev_block})");
        return Err(bch_errcode::BCH_ERR_block_size_too_small.into());
    }

    sb.io.bits.set_have_layout(1);
    validate_bytes(sb.buffer_mut(), opts, sb_offset, c::bch_validate_flags::empty(), err)?;

    if primary_bad && sb.dev.mode & c::BLK_OPEN_WRITE != 0 {
        match rewrite_primary_super(&mut sb) {
            Ok(())  => prt!(err, "rewrote primary superblock from backup at {sb_offset}\n"),
            Err(e)  => prt!(err, "error rewriting primary superblock from backup at {sb_offset}: {e}\n"),
        }
    }

    Ok(sb)
}

/// read_super_and_backups(), again with buffered IO if direct IO can't.
fn read_super_err(path: &CStr, opts: &mut c::bch_opts, err: &mut Printbuf) -> Result<bch_sb_handle, BchError> {
    loop {
        match read_super_and_backups(path, opts, err) {
            Err(e) if e.raw() == c::EINTR as i32 => err.reset(),
            r => return r,
        }
    }
}

/// The superblock of the device at @path, opened as @opts says - why not
/// printed, and what reading a backup instead took: bch2_read_super().
pub fn read_super(path: &CStr, opts: &mut c::bch_opts) -> Result<bch_sb_handle, BchError> {
    use crate::util::log::{log_opts, LogLevel};
    use crate::util::printbuf::Msg;

    let mut err = Printbuf::new();
    let ret = read_super_err(path, opts, &mut err);

    // @err goes mid-message: a reason that came back unterminated would run
    // into whatever's printed next
    err.ensure_trailing_newline();

    let name = path.to_bytes();
    match &ret {
        Err(e) => log_opts(opts, LogLevel::ERR, &Msg(|out: &mut Printbuf| {
            prt!(out, "bcachefs (");
            out.write_bytes(name);
            prt!(out, "): error reading superblock: {e}\n");
            out.write_bytes(err.as_bytes());
        })),
        Ok(_) if !err.as_bytes().is_empty() => log_opts(opts, LogLevel::NOTICE, &Msg(|out: &mut Printbuf| {
            prt!(out, "bcachefs (");
            out.write_bytes(name);
            prt!(out, "): ");
            out.write_bytes(err.as_bytes());
            prt!(out, "successful read from backup\n");
        })),
        Ok(_) => {}
    }
    ret
}

/// read_super(), printing nothing: for mount.bcachefs.
pub fn read_super_silent(path: &CStr, opts: &mut c::bch_opts) -> Result<bch_sb_handle, BchError> {
    read_super_err(path, opts, &mut Printbuf::new())
}

/// # Safety
/// @path is a C string; @sb holds nothing, and is written over.
#[c_export]
pub unsafe fn bch2_read_super(path: *const c_char, opts: &mut c::bch_opts, sb: *mut c::bch_sb_handle) -> c_int {
    // SAFETY: the caller's
    ret_to_c(read_super(unsafe { CStr::from_ptr(path.cast()) }, opts).map(|h| unsafe { sb.write(h) }))
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

    /// fsck error @err silenced - expected, not damage - in the ext section,
    /// recorded: whether it wasn't already. sb_set_err_silent().
    pub fn set_err_silent(&mut self, err: c::bch_sb_error_id) -> bool {
        let ext = self.field_mut::<c::bch_sb_field_ext<Validated>>()
            .expect("an ext section: a mounted filesystem's superblock has one");
        let (word, bit) = (err.0 as usize / 64, 1u64 << (err.0 % 64));
        let v = ext.errors_silent[word].get();
        ext.errors_silent[word].set(v | bit);
        self.record(v & bit == 0)
    }

    /// Write now, if dirtied, for an error to report; after, nothing is to
    /// write until something is dirtied again.
    pub fn flush(&mut self) -> Result<(), BchError> {
        let w = self.state();
        if !core::mem::take(&mut w.dirty) {
            return Ok(());
        }
        let (replicas, flags) = (w.replicas, w.flags);
        if replicas {
            self.fs.write_super_replicas_ret(&mut self.sb)
        } else {
            self.fs.write_super_flags(&mut self.sb, flags)
        }
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

/// A superblock's state, for what reads one in either - validate reading
/// members, a printer, as the rest reads them: its header, and a section by
/// type, as read - Unvalidated, whatever the superblock's state, for what
/// reads one that may not be valid. Validated, the walk trusts the sections'
/// counts; Unvalidated, each is checked, and the walk ends at one that
/// doesn't fit - which validate reports.
pub trait SbState: State + Sized {
    fn head(sb: &bch_sb<Self>) -> &Head<bch_sb>;

    fn field(sb: &bch_sb<Self>, ty: c::bch_sb_field_type) -> Option<&c::bch_sb_field<Unvalidated>>;
}

impl SbState for Validated {
    fn head(sb: &bch_sb) -> &Head<bch_sb> {
        sb
    }

    fn field(sb: &bch_sb, ty: c::bch_sb_field_type) -> Option<&c::bch_sb_field<Unvalidated>> {
        field_get_id(sb, ty).map(|f| f.as_unvalidated())
    }
}

impl SbState for Unvalidated {
    fn head(sb: &bch_sb<Unvalidated>) -> &Head<bch_sb> {
        sb
    }

    fn field(sb: &bch_sb<Unvalidated>, ty: c::bch_sb_field_type) -> Option<&c::bch_sb_field<Unvalidated>> {
        sb.entries().map_while(Result::ok).find(|f| f.type_.get() == ty.0)
    }
}

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
fn field_offset<S: SbState>(sb: &bch_sb<S>, ty: c::bch_sb_field_type) -> Option<usize> {
    let base = core::ptr::from_ref(sb).cast::<u8>() as usize;
    S::field(sb, ty).map(|f| f.as_bytes().as_ptr() as usize - base)
}

/// The section at @off in the superblock at @sb.
///
/// # Safety
/// @sb is validated, @off a section's offset in it: field_offset()'s.
unsafe fn field_at(sb: *mut Head<bch_sb>, off: usize) -> *mut Head<c::bch_sb_field> {
    unsafe { sb.byte_add(off).cast() }
}

/// bch_sb_handle::field_mut(), for a superblock C hands Rust without its
/// handle.
///
/// # Safety
/// @sb is validated, and nothing else uses it for 'a.
pub unsafe fn sb_field_get_raw_mut<'a, F: SbField + ?Sized>(sb: *mut Head<bch_sb>) -> Option<&'a mut F> {
    // SAFETY: the caller's
    let off = field_offset(unsafe { bch_sb::<Validated>::from_ffi(sb) }, F::FIELD_TYPE)?;
    // SAFETY: a section of it, borrowed as it is
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

impl bch_sb_buf {
    /// The buffer grown to @size bytes, the superblock kept and zeroes after.
    fn grow(&mut self, size: usize) -> Result<(), crate::util::alloc::AllocError> {
        let p = crate::util::alloc::kvrealloc_zeroed(self.sb.cast(), self.buffer_size, size)?;
        self.sb = p.as_ptr().cast();
        self.buffer_size = size;
        Ok(())
    }

    /// The buffer freed, and the superblock with it.
    fn free(&mut self) {
        if let Some(p) = core::ptr::NonNull::new(self.sb.cast::<u8>()) {
            // SAFETY: grow()'s, buffer_size bytes
            unsafe { crate::util::alloc::kvfree_sized(p, self.buffer_size) };
        }
        self.sb = core::ptr::null_mut();
        self.buffer_size = 0;
    }

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

    /// The section at @off, as an F: whole_mut() if that's all of F.
    fn arm_mut_at<F: SbField + ?Sized>(&mut self, off: usize) -> Option<&mut F> {
        // SAFETY: a section of the handle's superblock, borrowed with it
        unsafe { c::bch_sb_field::<Validated>::from_ffi_mut(field_at(self.sb, off)) }.arm_mut::<F>()
    }

    /// The section's pointer, for C.
    fn field_ptr(&mut self, off: Option<usize>) -> *mut Head<c::bch_sb_field> {
        // SAFETY: a section of the handle's superblock
        off.map_or(core::ptr::null_mut(), |off| unsafe { field_at(self.sb, off) })
    }
}

/// A superblock whose sections are resized: its buffer, how it makes room in
/// it, and what else must make the same room - nothing, but for the
/// filesystem's, whose devices' superblocks are copies of it. They're grown
/// with it so that a resize fails, -ENOSPC or -ENOMEM, before anything
/// depends on the section, not the superblock write after.
pub trait SbResize: core::ops::DerefMut<Target = bch_sb_buf> {
    /// Room for a superblock of @u64s - 0, the minimum.
    fn sb_realloc(&mut self, u64s: u32) -> Result<(), BchError>;

    /// Room in what else holds this superblock, grown by @d u64s.
    fn grow_with(&mut self, _d: isize) -> Option<()> {
        Some(())
    }

    /// The section of type @ty resized to @u64s - made if there's none - with
    /// the superblock grown to hold it, and what's grown with it: its
    /// offset, None if there's no room.
    fn field_resize_id(&mut self, ty: c::bch_sb_field_type, u64s: u32) -> Option<usize> {
        self.mark_dirty(ty);
        let d = u64s as isize - (field_bytes(self.sb(), ty) / size_of::<u64>()) as isize;
        let new = (self.sb().u64s() as isize + d) as u32;

        self.sb_realloc(new).ok()?;
        self.grow_with(d)?;

        let at = field_offset(self.sb(), ty);
        let off = self.field_resize_at(at, u64s)?;
        // SAFETY: the section just resized
        unsafe { (*field_at(self.sb, off)).type_ = Le32::new(ty.0) };
        Some(off)
    }

    /// The section of type @ty, made or grown to at least @u64s.
    fn field_get_minsize_id(&mut self, ty: c::bch_sb_field_type, u64s: u32) -> Option<usize> {
        self.mark_dirty(ty);
        if field_get_id(self.sb(), ty).is_some_and(|f| f.as_bytes().len() >= u64s as usize * size_of::<u64>()) {
            return field_offset(self.sb(), ty);
        }
        self.field_resize_id(ty, u64s)
    }

    /// Resized to @u64s, made if there's none: at its (possibly new) place.
    fn field_resize<F: SbField + ?Sized>(&mut self, u64s: u32) -> Option<&mut F> {
        let off = self.field_resize_id(F::FIELD_TYPE, u64s)?;
        self.arm_mut_at(off)
    }

    /// Made or grown to at least @min_u64s.
    fn field_get_minsize<F: SbField + ?Sized>(&mut self, min_u64s: u32) -> Option<&mut F> {
        let off = self.field_get_minsize_id(F::FIELD_TYPE, min_u64s)?;
        self.arm_mut_at(off)
    }

    fn field_delete(&mut self, ty: c::bch_sb_field_type) {
        self.mark_dirty(ty);
        if let Some(off) = field_offset(self.sb(), ty) {
            self.field_resize_at(Some(off), 0);
        }
    }
}

/// A device's: its own limits - the logical block, the layout - and its bio.
impl SbResize for bch_sb_handle {
    fn sb_realloc(&mut self, u64s: u32) -> Result<(), BchError> {
        bch_sb_handle::sb_realloc(self, u64s)
    }
}

/// The filesystem's superblock, to resize: Fs::sb_resize()'s.
pub struct FsSbResize<'a> {
    fs:      &'a Fs,
    disk_sb: &'a mut c::bch_fs_sb,
}

impl Fs {
    /// The superblock @sb holds, to resize - and each online device's grown
    /// with it, under the same sb_lock.
    pub fn sb_resize<'a>(&'a self, sb: &'a mut SbLockGuard<'_>) -> FsSbResize<'a> {
        assert!(core::ptr::eq(sb.lock(), &self.sb), "sb_resize(): another filesystem's sb_lock");
        FsSbResize { fs: self, disk_sb: sb }
    }
}

impl core::ops::Deref for FsSbResize<'_> {
    type Target = bch_sb_buf;

    fn deref(&self) -> &bch_sb_buf {
        &**self.disk_sb
    }
}

impl core::ops::DerefMut for FsSbResize<'_> {
    fn deref_mut(&mut self) -> &mut bch_sb_buf {
        &mut **self.disk_sb
    }
}

impl SbResize for FsSbResize<'_> {
    fn sb_realloc(&mut self, u64s: u32) -> Result<(), BchError> {
        let size = (size_of::<Head<bch_sb>>() + u64s as usize * size_of::<u64>()).next_power_of_two();
        if !self.sb.is_null() && self.buffer_size >= size {
            return Ok(());
        }
        self.grow(size).map_err(|_| BchError::from(c::bch_errcode::BCH_ERR_ENOMEM_sb_buf_realloc))
    }

    fn grow_with(&mut self, d: isize) -> Option<()> {
        // XXX: we're not checking that offline device have enough space
        for ca in self.fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_sb_field_resize) {
            // SAFETY: our devices', under the sb_lock we hold - mutably, and
            // nothing else of it is borrowed meanwhile
            let io = unsafe { ca.disk_sb.protected_mut() };
            io.realloc(&ca.disk_sb, (io.buf.sb().u64s() as isize + d) as u32).ok()?;
        }
        Some(())
    }
}

/// A device's superblock, to edit: its shared contents only by sync(), from
/// the filesystem's, its own directly - Fs::dev_sb_mut()'s.
pub struct DevSbMut<'a> {
    fs_sb: &'a bch_sb,
    io:    &'a mut c::bch_sb_io,
    ca:    &'a c::bch_dev,
}

impl Fs {
    /// That @ca's superblock is the one @sb locks: sb_lock is ours, @ca ours.
    fn dev_sb_check(&self, sb: &SbLockGuard<'_>, ca: &c::bch_dev) {
        assert!(core::ptr::eq(sb.lock(), &self.sb), "dev_sb(): another filesystem's sb_lock");
        assert!(core::ptr::eq(ca.fs, self), "dev_sb(): another filesystem's device {}", ca.dev_idx);
    }

    /// @ca's superblock, under sb_lock: None if it has none, not attached.
    pub fn dev_sb<'a>(&self, sb: &'a SbLockGuard<'_>, ca: &'a c::bch_dev) -> Option<&'a bch_sb_buf> {
        self.dev_sb_check(sb, ca);
        // SAFETY: @sb holds sb_lock, which protects it - and every &mut of
        // it is had through a guard borrowed mutably
        let io = unsafe { ca.disk_sb.protected() };
        (!io.buf.sb.is_null()).then_some(&io.buf)
    }

    /// @ca's superblock, to edit: None if it has none, not attached.
    pub fn dev_sb_mut<'a>(&self, sb: &'a mut SbLockGuard<'_>, ca: &'a c::bch_dev) -> Option<DevSbMut<'a>> {
        let sb: &'a SbLockGuard<'_> = sb;
        self.dev_sb_check(sb, ca);
        // SAFETY: @sb holds sb_lock, which protects it - mutably borrowed, so
        // nothing else of it is
        let io = unsafe { ca.disk_sb.protected_mut() };
        (!io.buf.sb.is_null()).then(|| DevSbMut { fs_sb: sb.sb(), io, ca })
    }
}

impl DevSbMut<'_> {
    pub fn sb(&self) -> &bch_sb {
        self.io.buf.sb()
    }

    /// The filesystem's superblock copied in, all but each device's own:
    /// bch2_sb_from_fs().
    pub fn sync(&mut self) -> Result<(), BchError> {
        let src = self.fs_sb;
        copy_super(&mut self.resize(), src)
    }

    /// Section @F, one each device has its own of, resized to @u64s - made if
    /// there's none: None if there's no room.
    pub fn field_resize<F: SbField + ?Sized>(&mut self, u64s: u32) -> Option<&mut F> {
        const { assert!(F::SINGLE_DEVICE, "a section every device's superblock shares: the filesystem's to change") };
        let off = self.resize().field_resize_id(F::FIELD_TYPE, u64s)?;
        self.io.buf.arm_mut_at(off)
    }

    pub fn field_delete<F: SbField + ?Sized>(&mut self) {
        const { assert!(F::SINGLE_DEVICE, "a section every device's superblock shares: the filesystem's to change") };
        self.resize().field_delete(F::FIELD_TYPE)
    }

    pub fn layout_mut(&mut self) -> &mut c::bch_sb_layout {
        &mut self.io.buf.sb_mut().layout
    }

    /// bch_sb_buf::edit_header(), for the tools: of what's shared, the next
    /// sync() keeps nothing.
    pub fn edit_header<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, &'static str> {
        self.io.buf.edit_header(f)
    }

    fn resize(&mut self) -> DevSbResize<'_> {
        DevSbResize { io: self.io, dev: &self.ca.disk_sb }
    }
}

/// A device's superblock, resized as its own: only DevSbMut's.
struct DevSbResize<'a> {
    io:  &'a mut c::bch_sb_io,
    dev: &'a c::bch_sb_bdev,
}

impl core::ops::Deref for DevSbResize<'_> {
    type Target = bch_sb_buf;

    fn deref(&self) -> &bch_sb_buf {
        &self.io.buf
    }
}

impl core::ops::DerefMut for DevSbResize<'_> {
    fn deref_mut(&mut self) -> &mut bch_sb_buf {
        &mut self.io.buf
    }
}

impl SbResize for DevSbResize<'_> {
    fn sb_realloc(&mut self, u64s: u32) -> Result<(), BchError> {
        self.io.realloc(self.dev, u64s)
    }
}

impl Fs {
    /// @devs' superblocks, to edit together - as dev_sb_mut() gives each, and
    /// None for one with none, not attached. Panics if one's twice in @devs.
    pub fn dev_sbs_mut<'a, D: core::ops::Deref<Target = c::bch_dev>>(&self, sb: &'a mut SbLockGuard<'_>, devs: &'a [D])
        -> impl Iterator<Item = Option<DevSbMut<'a>>>
    {
        let sb: &'a SbLockGuard<'_> = sb;
        let mut seen = crate::c::DevsMask::default();
        for ca in devs {
            self.dev_sb_check(sb, ca);
            assert!(!seen.test(ca.dev_idx.into()), "dev_sbs_mut(): device {} twice", ca.dev_idx);
            seen.set(ca.dev_idx.into());
        }

        let fs_sb = sb.sb();
        devs.iter().map(move |ca| {
            // SAFETY: as dev_sb_mut()'s - each device's once, by @seen
            let io = unsafe { ca.disk_sb.protected_mut() };
            (!io.buf.sb.is_null()).then(|| DevSbMut { fs_sb, io, ca })
        })
    }
}

// ── Superblock IOs in flight together ────────────────────────────────────

/// A device's superblock with an IO in flight on it - its bio, mapped to its
/// buffer or to @buf: nothing of either reachable until SbIos::wait() hands
/// it back.
struct Frozen<'a> {
    sb:  DevSbMut<'a>,
    buf: Option<&'a mut [u8]>,
}

/// Superblock IOs to devices, submitted together and waited on together:
/// what C's c->sb_write closure was for, the superblock write's fan-outs -
/// the read-backs, then each round of writes, a SbIos each. Submitting
/// freezes the device's superblock; wait() hands each back, with its IO's
/// result.
#[pin_init::pin_data]
pub struct SbIos<'a> {
    // first: it waits for the IOs before what they're on is handed back
    #[pin]
    gather: Gather<()>,
    frozen: KVVec<Frozen<'a>>,
}

impl<'a> SbIos<'a> {
    pub fn new() -> impl PinInit<Self> {
        pin_init::pin_init!(Self { gather <- Gather::new(), frozen: KVVec::new() })
    }

    /// @sb's first @bytes written to @sector of its device, FUA: a copy of it.
    pub fn write(self: Pin<&mut Self>, sb: DevSbMut<'a>, sector: u64, bytes: usize) -> Result<(), BchError> {
        // blk-wbt throttles all writes but those with REQ_SYNC and REQ_IDLE
        let opf = c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE | c::REQ_META | c::REQ_FUA;
        self.submit(Frozen { sb, buf: None }, opf, sector, bytes)
    }

    /// @buf read from @sector of @sb's device.
    pub fn read(self: Pin<&mut Self>, sb: DevSbMut<'a>, sector: u64, buf: &'a mut [u8]) -> Result<(), BchError> {
        let opf = c::req_op_REQ_OP_READ | c::REQ_SYNC | c::REQ_META;
        let bytes = buf.len();
        self.submit(Frozen { sb, buf: Some(buf) }, opf, sector, bytes)
    }

    fn submit(self: Pin<&mut Self>, f: Frozen<'a>, opf: u32, sector: u64, bytes: usize) -> Result<(), BchError> {
        let this = self.project();
        this.frozen.push(f, crate::util::alloc::flags::GFP_KERNEL)
            .map_err(|_| BchError::from(c::ENOMEM))?;
        let f = this.frozen.last_mut().expect("just pushed");
        let r = this.gather.get();

        let (mut bio, sb_buf) = f.sb.io.bio(&f.sb.ca.disk_sb, opf, sector);
        // SAFETY: frozen: nothing reaches its bio or buffer, or @buf, until
        // wait() has waited on the gather
        unsafe {
            match &mut f.buf {
                Some(buf) => bio.map_read(buf).submit_gathered(r),
                None      => bio.map_write(&sb_buf[..bytes]).submit_gathered(r),
            }
        }
        Ok(())
    }

    /// Every IO complete: each superblock back, with its IO's result.
    pub fn wait(self: Pin<&mut Self>) -> impl Iterator<Item = (DevSbMut<'a>, Result<(), BchError>)> {
        let this = self.project();
        this.gather.wait_sync();
        core::mem::take(this.frozen).into_iter().map(|f| {
            // SAFETY: realloc()'s, the superblock's, idle
            let ret = unsafe { Bio::from_raw(f.sb.io.bio) }.status();
            (f.sb, ret)
        })
    }
}

// ── Writing the superblock ───────────────────────────────────────────────

c_const! {
    /// ca->sb_read_scratch's size: what a write reads back, the primary's
    /// header and then some.
    #[c_int]
    pub const BCH_SB_READ_SCRATCH_BUF_SIZE: u32 = 4096;
}

impl DevSbMut<'_> {
    /// Valid to be written, as validate() checks one about to be.
    fn validate_write(&mut self, err: &mut Printbuf) -> Result<(), BchError> {
        let opts = c::bch_opts::default();
        validate_bytes(self.io.buf.buffer_mut(), &opts, 0, c::bch_validate_flags::BCH_VALIDATE_write, err)
            .map(drop)
    }

    /// Made its copy for layout slot @idx - its offset, and its checksum, of
    /// the type metadata_checksum is: that copy's sector and bytes, for
    /// SbIos::write(). write_one_super().
    fn copy_for(&mut self, fs: &Fs, idx: usize) -> (u64, usize) {
        let s = self.io.buf.sb_mut();
        s.offset = s.layout.sb_offset[idx];
        let ty = c::bch_csum_opt(fs.opts().metadata_checksum.into()).to_type(fs, false);
        s.set_csum_type(ty.0.into());
        let sector = s.offset.get();

        let csum = sb_csum(Some(fs), ty, self.io.buf.sb_bytes());
        self.io.buf.sb_mut().csum = csum;

        let block = crate::util::kernel::bdev_logical_block_size(self.ca.disk_sb.bdev()) as usize;
        (sector, self.io.buf.sb_bytes().len().next_multiple_of(block))
    }
}

impl<'a> DevSbMut<'a> {
    /// ca->sb_read_scratch, what's read back into.
    ///
    /// # Safety
    /// Not had twice at once.
    unsafe fn scratch(&self) -> &'a mut [u8] {
        // SAFETY: the device's, BCH_SB_READ_SCRATCH_BUF_SIZE bytes, kmalloc()'d
        // at its alloc - used only by a superblock write, under the sb_lock
        // we hold; once at a time, the caller's
        unsafe {
            core::slice::from_raw_parts_mut(self.ca.sb_read_scratch.cast::<u8>(), BCH_SB_READ_SCRATCH_BUF_SIZE as usize)
        }
    }
}

/// A device's result in a superblock write: its IO accounted, and what went
/// wrong thrown - as the IO's endio did, in C.
fn sb_io_result(fs: &Fs, ca: &c::bch_dev, rw: c::bch_member_error_type, ret: Result<(), BchError>)
    -> Result<(), BchError>
{
    match ret {
        Ok(()) => {
            if rw == c::bch_member_error_type::BCH_MEMBER_ERROR_write {
                ca.write_errors_start.store(0, core::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        }
        Err(e) => {
            // SAFETY: a device, live as long as @ca is
            unsafe { c::bch2_io_error(core::ptr::from_ref(ca).cast_mut(), rw) };
            Err(fs.err(c::bch_errcode(e.raw() as u32)))
        }
    }
}

/// Written to every online device - or just @devs - copy by copy: each
/// layout slot to all of them at once, and waited on before the next.
/// Read back first, to catch a write that was silently dropped, or another
/// writer. __bch2_write_super().
#[track_caller]
fn write_super_locked(fs: &Fs, sb: &mut SbLockGuard<'_>, devs: Option<&c::bch_devs_mask>,
                      flags: c::bch_sb_write_flags) -> Result<(), BchError> {
    use c::bch_fs_flags as f;
    use crate::sb::members::SbMembersMut;
    const READ: c::bch_member_error_type = c::bch_member_error_type::BCH_MEMBER_ERROR_read;
    const WRITE: c::bch_member_error_type = c::bch_member_error_type::BCH_MEMBER_ERROR_write;
    use crate::util::alloc::flags::GFP_KERNEL;
    use crate::util::log::LogMsg;

    if !fs.flag(f::BCH_FS_may_upgrade_downgrade) {
        return Ok(());
    }

    // Before a start has begun, the in-memory superblock holds what the open
    // decided - version upgrade/downgrade, last_mount - for a start to act
    // on; an open that never starts mustn't persist that. So a write before
    // start is either part of bringing the filesystem up (as recovery's own
    // are), or from an open that said it will never start, which skips
    // those decisions:
    if !fs.flag(f::BCH_FS_start_begun) &&
       fs.opts().will_not_start == 0 &&
       !flags.contains(c::bch_sb_write_flags::BCH_SB_WRITE_bringup) {
        return fs.throw(bch_errcode::BCH_ERR_erofs_sb_write_before_start);
    }

    fs.event_inc_trace(crate::sb::counters::events::write_super, |_| {});

    let mut degraded_flags = c::BCH_FORCE_IF_DEGRADED;
    if u32::from(fs.opts().degraded) == c::bch_degraded_actions::BCH_DEGRADED_very.0 {
        degraded_flags |= c::BCH_FORCE_IF_LOST;
    }

    if sb.sb().has_journal() {
        crate::sb::clean::fs_mark_dirty(sb);
    } else {
        crate::sb::clean::fs_mark_clean(fs, sb);
    }

    // Note: we do writes to RO devices here, and we might want to change that
    // in the future. For now, we expect to be able to call write_super() when
    // we're not yet RW.
    let mut cas = KVVec::new();
    let mut failures = KVVec::new();
    for ca in fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_write_super) {
        if devs.is_some_and(|d| !d.d.test(ca.dev_idx.into())) {
            continue;
        }
        if cas.push(ca, GFP_KERNEL).is_err() || failures.push(KVVec::<(u64, BchError)>::new(), GFP_KERNEL).is_err() {
            crate::fs_fatal_error!(fs, "error allocating online devices");
            return Err(c::ENOMEM.into());
        }
    }

    // Make sure we're using the new magic numbers:
    let s = sb.sb_mut();
    s.magic.b = BCHFS_MAGIC;
    s.layout.magic.b = BCHFS_MAGIC;
    s.seq = Le64::new(s.seq.get() + 1);
    let seq = s.seq;
    for ca in cas.iter() {
        sb.fs_member_mut(ca.dev_idx.into()).seq = seq;
    }

    let s = sb.sb_mut();
    s.write_time = Le64::new(crate::util::kernel::ktime_get_real_seconds() as u64);
    if fs.flag(f::BCH_FS_error) {
        s.set_has_errors(true);
    }
    if fs.flag(f::BCH_FS_topology_error) {
        s.set_has_topology_errors(true);
    }
    s.set_big_endian(cfg!(target_endian = "big"));

    c::bch_sb_field_counters::from_cpu(fs, sb)?;
    c::bch_sb_field_members_v2::from_cpu(fs, sb)?;
    // as C: neither failing stops the write
    let _ = fs.sb_resize(sb).members_cpy_v2_v1();
    c::bch_sb_field_errors_v2::from_cpu(fs, sb)?;
    let _ = c::bch_sb_field_downgrade::from_cpu(fs, sb);
    c::bch_sb_field_extent_type_u64s::from_cpu(fs, sb)?;

    for mut dev_sb in fs.dev_sbs_mut(sb, &cas).flatten() {
        let _ = dev_sb.sync();
    }

    for mut dev_sb in fs.dev_sbs_mut(sb, &cas).flatten() {
        let mut err = Printbuf::new();
        if dev_sb.validate_write(&mut err).is_err() {
            crate::fs_inconsistent!(fs, "sb invalid before write: {err}");
            return Ok(());
        }
    }

    if fs.opts().nochanges != 0 {
        return Ok(());
    }

    // Defer writing the superblock until filesystem initialization is
    // complete - don't write out a partly initialized superblock.
    //
    // An offline edit (will_not_start) on a filesystem that has never been
    // started has nothing to defer to: say so, rather than let it report
    // success having written nothing.
    if !sb.sb().initialized() {
        if fs.opts().will_not_start == 0 {
            return Ok(());
        }
        crate::bch_err!(fs, "filesystem has never been started, not writing superblock: mount it once first");
        return fs.throw(bch_errcode::BCH_ERR_erofs_sb_never_started);
    }

    let version = u32::from(sb.sb().version.get());
    if version > c::bcachefs_metadata_version_current {
        crate::fs_fatal_error!(fs, ": attempting to write superblock that wasn't version downgraded ({} > {})",
                               c::bcachefs_metadata_version(version),
                               c::bcachefs_metadata_version(c::bcachefs_metadata_version_current));
        return fs.throw(bch_errcode::BCH_ERR_sb_not_downgraded);
    }

    // Read back, to compare with what we last wrote
    {
        pin_init::stack_pin_init!(let ios = SbIos::new());
        for dev_sb in fs.dev_sbs_mut(sb, &cas).flatten() {
            let sector = dev_sb.io.buf.sb().layout.sb_offset[0].get();
            // SAFETY: once, until its read's waited on
            let scratch = unsafe { dev_sb.scratch() };
            scratch.fill(0);
            dev_sb.ca.io_done_add(crate::util::kernel::READ, c::bch_data_type::BCH_DATA_sb, (scratch.len() >> 9) as u64);
            ios.as_mut().read(dev_sb, sector, scratch)?;
        }

        for (dev_sb, ret) in ios.as_mut().wait() {
            let ca = dev_sb.ca;
            if sb_io_result(fs, ca, READ, ret).is_err() {
                continue;
            }

            // SAFETY: once - its read, which had it, waited on
            let read = cast_plain::<Head<bch_sb>>(unsafe { dev_sb.scratch() }).expect("a header's worth").seq.get();
            let expected = dev_sb.io.seq;

            if read != expected {
                let mut msg = LogMsg::new(fs);
                crate::util::printbuf::prt_bdevname(&mut msg, Some(ca.disk_sb.bdev()));
                if read < expected {
                    prt!(msg, ": Superblock write was silently dropped! (seq {read} expected {expected})");
                    let errors = u32::from(fs.opts().errors);
                    if errors == c::bch_error_actions::BCH_ON_ERROR_continue.0 ||
                       errors == c::bch_error_actions::BCH_ON_ERROR_fix_safe.0 {
                        continue;
                    }
                } else {
                    prt!(msg, ": Superblock modified by another process (seq {read} expected {expected})");
                }
                fs.emergency_read_only(&mut msg);
                return fs.throw(bch_errcode::BCH_ERR_erofs_sb_err);
            }
        }
    }

    let mut sb_written = c::bch_devs_mask::default();
    let mut have_errors = false;

    for idx in 0.. {
        pin_init::stack_pin_init!(let ios = SbIos::new());
        let mut wrote = false;

        for mut dev_sb in fs.dev_sbs_mut(sb, &cas).flatten() {
            if idx >= usize::from(dev_sb.io.buf.sb().layout.nr_superblocks) {
                continue;
            }
            let (sector, bytes) = dev_sb.copy_for(fs, idx);
            dev_sb.ca.io_done_add(crate::util::kernel::WRITE, c::bch_data_type::BCH_DATA_sb, (bytes >> 9) as u64);
            ios.as_mut().write(dev_sb, sector, bytes)?;
            wrote = true;
        }

        for (dev_sb, ret) in ios.as_mut().wait() {
            let ca = dev_sb.ca;
            match sb_io_result(fs, ca, WRITE, ret) {
                Ok(()) => {
                    sb_written.d.set(ca.dev_idx.into());
                    dev_sb.io.seq = dev_sb.io.buf.sb().seq.get();
                }
                Err(e) => {
                    let offset = dev_sb.io.buf.sb().layout.sb_offset[idx].get();
                    let i = cas.iter().position(|c| c.dev_idx == ca.dev_idx).expect("one of ours");
                    let _ = failures[i].push((offset, e), GFP_KERNEL);
                    have_errors = true;
                }
            }
        }

        if !wrote {
            break;
        }
    }

    let nr_wrote = sb_written.d.iter().count();
    let nr_members = match devs {
        Some(_) => cas.len(),
        None    => sb.sb().number_of_devices() as usize,
    };

    // A restricted write is not trying to be readable on its own - by
    // construction it skips devices that hold data. Asking
    // can_read_fs_with_devs() about it would say no every time and take us
    // read only. What matters is that the write landed somewhere: the
    // previous superblock is still on every device, so the failure here is a
    // missed update, not a lost one.
    let fatal = nr_wrote == 0 ||
        (devs.is_none() && !fs.can_read_fs_with_devs(&sb_written, degraded_flags, None));

    if !have_errors && !fatal {
        return Ok(());
    }

    let mut msg = LogMsg::new(fs);

    // A degraded write (wrote to fewer devices, but still enough to mount)
    // repeats on every superblock write while a device is failing, so
    // ratelimit it. A fatal failure goes emergency read-only below and always
    // prints.
    if !fatal {
        static RATELIMIT: crate::util::log::Ratelimit = crate::util::log::Ratelimit::new();
        msg.set_suppressed(RATELIMIT.suppress(fs, c"write_super_locked"));
    }

    prt!(msg, "Error writing superblock, wrote to {nr_wrote}/{nr_members} devices:\n");

    let mut sb_unwritten = c::bch_devs_mask::default();
    for dev in 0..c::BCH_SB_MEMBERS_MAX as usize {
        sb_unwritten.d.set(dev);
    }

    for (ca, failures) in cas.iter().zip(failures.iter()) {
        sb_unwritten.d.clear(ca.dev_idx.into());
        crate::sb::members::dev_to_text_short(&mut msg, sb.sb(), ca.dev_idx.into());

        if failures.is_empty() {
            prt!(msg, " (success)");
        } else {
            for (offset, e) in failures.iter() {
                prt!(msg, " {offset}={e}");
            }
            if sb_written.d.test(ca.dev_idx.into()) {
                prt!(msg, " (partial success)");
            } else {
                prt!(msg, " (all failed)");
            }
        }
        msg.newline();
    }

    // For a restricted write the leftovers are the devices we deliberately
    // skipped, not devices that went away - don't call them offline.
    prt!(msg, "{}", if devs.is_some() { "Not written:\n" } else { "Offline devices:\n" });
    msg.indent_add(2);
    crate::sb::members::devs_mask_to_text(&mut msg, fs, sb.sb(), &sb_unwritten);
    msg.indent_sub(2);

    if fatal {
        prt!(msg, "Unable to write superblock to sufficient devices (from {})\n", core::panic::Location::caller());
        prt!(msg, "Would not be able to mount with written devices\n");
        fs.can_read_fs_with_devs(&sb_written, degraded_flags, Some(&mut *msg));
        fs.emergency_read_only(&mut msg);
        return fs.throw(bch_errcode::BCH_ERR_erofs_sb_err);
    }

    // Not fatal: we wrote to fewer devices than we wanted, but enough that
    // the filesystem still mounts. The superblock is on disk, so the caller
    // got what it asked for - the message above is a warning, not a failure.
    Ok(())
}

/// write_super_locked(), then the new options made visible - after they're
/// persistent - and timed: bch2_write_super_devs(). And what memory has of
/// each section edited since the last write, by to_cpu() - not of what the
/// write's from_cpu()s edited, which is memory's already.
#[track_caller]
fn write_super_devs(fs: &Fs, sb: &mut SbLockGuard<'_>, devs: Option<&c::bch_devs_mask>,
                    flags: c::bch_sb_write_flags) -> Result<(), BchError> {
    let start = crate::util::kernel::local_clock();

    let dirty = core::mem::take(&mut sb.dirty);
    let ret = write_super_locked(fs, sb, devs, flags);
    sb.dirty = 0;
    sb_update(fs, sb);

    // XXX enable after all sb access in Rust: C edits sections through raw
    // pointers, which mark nothing - bch2_members_v2_get_mut(), all over -
    // so sb_update() reads members back after every write. Then the explicit
    // rebuilds go - sb_update()'s members, dev_group_set()'s disk groups,
    // journal_seq_blacklist_add()'s - and the replicas table's writes, and
    // the tools' strip_fs_alloc(), edit as from_cpu() does: marking nothing,
    // replicas' to_cpu() being only for before the table has refs.
    const TO_CPU_DIRTY: bool = false;
    let ret = if TO_CPU_DIRTY { ret.and(sections_to_cpu(fs, sb, dirty)) } else { ret };

    fs.times[c::bch_time_stats::BCH_TIME_sb_write as usize].update(start);
    ret
}

/// to_cpu() for each section the superblock has that @dirty names, by bit:
/// each, whichever fails. XXX: not one deleted since the last write.
fn sections_to_cpu(fs: &Fs, sb: &mut SbLockGuard<'_>, dirty: u64) -> Result<(), BchError> {
    let fns = section_fns(sb.sb(), |f| dirty & 1u64.checked_shl(f.type_.get()).unwrap_or(0) != 0,
                          c::bch_sb_field::to_cpu_fn);
    fns.into_iter().fold(Ok(()), |ret, f| ret.and(f(fs, sb)))
}

/// Of each section @sb has that @keep passes, its @f - collected, for the
/// walk borrows the superblock, and what it collects is called with it.
fn section_fns(sb: &bch_sb, keep: impl Fn(&c::bch_sb_field) -> bool,
               f: impl Fn(&c::bch_sb_field) -> Option<SbSyncFn>) -> ArrayVec<SbSyncFn, FIELD_NR> {
    // validated: one of each type at most
    sb.entries().filter(|s| keep(s)).filter_map(f).take(FIELD_NR).collect()
}

/// How much of the filesystem's metadata we're willing to leave on devices a
/// replicas superblock write skips. Tunable: lower is more conservative.
const SB_SKIP_META_PCT: u64 = 10;

impl Fs {
    #[track_caller]
    pub fn write_super_flags(&self, sb: &mut SbLockGuard<'_>, flags: c::bch_sb_write_flags) -> Result<(), BchError> {
        write_super_devs(self, sb, None, flags)
    }

    /// Superblock write for a new replicas entry - the one that happens
    /// constantly on a big multi device filesystem, because the journal
    /// wanders and every device set it lands on is a new entry, and every
    /// entry costs a barrier against the slowest disk in the fleet.
    ///
    /// Such a write may skip the devices that hold little metadata, because
    /// an entry that doesn't reach them isn't lost: the previous superblock is
    /// still on every device, the next fleet wide write catches them up, and a
    /// mount that has only them rebuilds the replicas section from accounting
    /// - see accounting_read_mem_fixups(), which does that on every mount
    /// already.
    ///
    /// Start from every online device and drop rotating disks while the
    /// metadata left behind stays under SB_SKIP_META_PCT. Dropping rather
    /// than picking keeps the default on the safe side, and only a rotating
    /// disk is ever dropped - an all-SSD filesystem keeps its whole fleet
    /// however the metadata is spread.
    ///
    /// Cheapest first, because the number of devices dropped is the whole
    /// point: with a budget of 10%, one disk holding 10% and ten holding 1%
    /// each gets us one device unsorted and ten sorted. Sorted also lets us
    /// stop at the first disk that doesn't fit - everything after it holds at
    /// least as much.
    ///
    /// Metadata is btree only. Not journal: that's buckets allocated to the
    /// journal, not the live entries in them, so it's both the wrong quantity
    /// and a constantly moving one. Not sb either: every device has one and
    /// they're all the same size, so it only dilutes the ranking - and on a
    /// filesystem with little btree it would dominate, leaving every device
    /// tied. Without it that case totals zero, and we write to everyone, which
    /// is the right answer.
    ///
    /// This is the *only* caller allowed to write to a subset, which is why
    /// it's named for the caller rather than for the mechanism: every other
    /// superblock section stays fleet wide, so nothing else can go stale
    /// behind us. bch2_write_super_replicas().
    #[track_caller]
    pub fn write_super_replicas_ret(&self, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        let mut m = KVVec::new();
        let mut devs = c::bch_devs_mask::default();
        let mut total = 0u64;

        for ca in self.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_write_super) {
            let sectors = self.dev_usage_full_read(ca.dev_idx.into()).d[c::bch_data_type::BCH_DATA_btree.0 as usize].sectors;
            if m.push((ca.dev_idx, sectors), crate::util::alloc::flags::GFP_KERNEL).is_err() {
                return self.write_super_flags(sb, c::bch_sb_write_flags::empty());
            }
            devs.d.set(ca.dev_idx.into());
            total += sectors;
        }

        if total == 0 {
            return self.write_super_flags(sb, c::bch_sb_write_flags::empty());
        }

        m.sort_unstable_by_key(|&(_, sectors)| sectors);

        let budget = total / 100 * SB_SKIP_META_PCT;
        let (mut skipped, mut nr_skipped) = (0, 0);
        for &(dev, sectors) in m.iter() {
            if !self.dev_rotational(dev.into()) {
                continue;
            }
            if skipped + sectors > budget {
                break;
            }
            skipped += sectors;
            devs.d.clear(dev.into());
            nr_skipped += 1;
        }

        if nr_skipped != 0 {
            write_super_devs(self, sb, Some(&devs), c::bch_sb_write_flags::empty())
        } else {
            self.write_super_flags(sb, c::bch_sb_write_flags::empty())
        }
    }
}

#[c_export]
pub fn bch2_write_super(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    ret_to_c(fs.write_super_flags(&mut unsafe { fs.sb_lock_held() }, c::bch_sb_write_flags::empty()))
}

#[c_export]
pub fn bch2_write_super_flags(c: &Opaque<c::bch_fs>, flags: c::bch_sb_write_flags) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    ret_to_c(fs.write_super_flags(&mut unsafe { fs.sb_lock_held() }, flags))
}

#[c_export]
pub fn bch2_write_super_replicas(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    ret_to_c(fs.write_super_replicas_ret(&mut unsafe { fs.sb_lock_held() }))
}

const _: () = assert!(size_of::<c::bch_sb_handle>() == size_of::<crate::locking::LockedBy<c::bch_sb_io, c::bch_sb_bdev>>(),
                      "ca->disk_sb is a struct bch_sb_handle to C");

/// # Safety
/// @sb is a live filesystem's c->disk_sb, its sb_lock held.
#[c_export]
pub unsafe fn bch2_sb_field_resize_id(sb: *mut c::bch_fs_sb, ty: c::bch_sb_field_type, u64s: c_uint)
    -> *mut Head<c::bch_sb_field>
{
    // SAFETY: the caller's
    let (fs, mut guard) = unsafe { Fs::from_disk_sb(sb) };
    let mut sb = fs.sb_resize(&mut guard);
    let off = sb.field_resize_id(ty, u64s);
    sb.field_ptr(off)
}

/// # Safety
/// @sb is a live filesystem's c->disk_sb, its sb_lock held.
#[c_export]
pub unsafe fn bch2_sb_field_get_minsize_id(sb: *mut c::bch_fs_sb, ty: c::bch_sb_field_type, u64s: c_uint)
    -> *mut Head<c::bch_sb_field>
{
    // SAFETY: the caller's
    let (fs, mut guard) = unsafe { Fs::from_disk_sb(sb) };
    let mut sb = fs.sb_resize(&mut guard);
    let off = sb.field_get_minsize_id(ty, u64s);
    sb.field_ptr(off)
}

/// The filesystem's superblock's buffer freed: bch2_free_super(), for it.
#[c_export]
pub fn bch2_free_fs_sb(sb: &mut c::bch_fs_sb) {
    sb.free();
}

/// The u64s section @F is with @nr of its tail's entries: for
/// field_resize() and field_get_minsize().
pub fn sb_field_u64s<F: SbField + Unsized + ?Sized>(nr: usize) -> u32 {
    (F::TAIL + nr * size_of::<F::Elem>()).div_ceil(size_of::<u64>()) as u32
}

// ── The filesystem's copies ──────────────────────────────────────────────

/// c->sb's words of @a, as they are.
fn get_all<T: AtomicWord, const N: usize>(a: &[Volatile<T>; N]) -> [T; N] {
    a.each_ref().map(Volatile::get)
}

/// c->sb's words of @a set to @v: @a as sb_lock's holder has it.
pub(crate) fn set_all<T: AtomicWord, const N: usize>(a: &[VolatileWriter<T>; N], v: [T; N]) {
    for (w, v) in a.iter().zip(v) {
        w.set(v);
    }
}

/// c->sb: what's read of the superblock without sb_lock.
impl c::bch_sb_cpu {
    pub fn uuid(&self) -> c::__uuid_t {
        c::__uuid_t { b: get_all(&self.uuid) }
    }

    pub fn user_uuid(&self) -> c::__uuid_t {
        c::__uuid_t { b: get_all(&self.user_uuid) }
    }

    /// Each extent entry type's size, by type: extent_types_known of them.
    pub fn extent_type_u64s(&self) -> [u8; 31] {
        get_all(&self.extent_type_u64s)
    }
}

/// c->sb, from the superblock: bch2_sb_update(). And ca->mi, each member's.
pub(crate) fn sb_update(fs: &Fs, sb: &mut SbLockGuard<'_>) {
    let (handle, cpu) = sb.parts();
    let src = handle.sb();

    set_all(&cpu.uuid, src.uuid.b);
    set_all(&cpu.user_uuid, src.user_uuid.b);
    cpu.version                 .set(src.version.get());
    cpu.version_incompat        .set(src.version_incompat() as u16);
    cpu.version_incompat_allowed.set(src.version_incompat_allowed() as u16);
    cpu.version_min             .set(src.version_min.get());
    cpu.version_upgrade_complete.set(src.version_upgrade_complete() as u16);
    cpu.nr_devices              .set(src.nr_devices);
    cpu.clean                   .set(src.clean() as u8);
    cpu.encryption_type         .set(src.encryption_type() as u8);

    cpu.extent_bp_shift.set(match src.extent_bp_shift() {
        0 => c::BCH_SB_EXTENT_BP_SHIFT_DEFAULT as u8,
        shift => shift as u8,
    });

    let nsec_per_time_unit = src.time_precision.get();
    cpu.nsec_per_time_unit.set(nsec_per_time_unit);
    cpu.time_units_per_sec.set((NSEC_PER_SEC / u64::from(nsec_per_time_unit)) as u32);

    /* XXX this is wrong, we need a 96 or 128 bit integer type */
    cpu.time_base_lo.set(src.time_base_lo.get() / u64::from(nsec_per_time_unit));
    cpu.time_base_hi.set(src.time_base_hi.get());

    cpu.features         .set(src.features[0].get());
    cpu.compat           .set(src.compat[0].get());
    cpu.multi_device     .set(src.multi_device());
    cpu.dirents_sanitized.set(src.dirents_sanitized());

    match handle.field::<c::bch_sb_field_ext<Validated>>() {
        Some(e) => {
            cpu.recovery_passes_required.set(
                crate::init::passes::passes_from_stable(e.recovery_passes_required[0].get()));

            // the section's little-endian words as longs: le_bitvector_to_cpu()
            const _: () = assert!(size_of::<[core::ffi::c_ulong; bits_to_longs(c::BCH_FSCK_ERR_MAX as usize)]>()
                                  <= size_of::<[Le64; 8]>());
            let silent = LeBitmap::from_words(&e.errors_silent);
            let bits = core::ffi::c_ulong::BITS as usize;
            for (i, w) in cpu.errors_silent.iter().enumerate() {
                w.set((0..bits).filter(|&b| silent.test(i * bits + b)).fold(0, |w, b| w | 1 << b));
            }

            cpu.btrees_lost_data.set(e.btrees_lost_data().get());
            // grown since: 0 in an older version's section
            cpu.btrees_clean.set(e.btrees_clean().get());
        }
        None => cpu.errors_silent.iter().for_each(|w| w.set(0)),
    }

    let _ = c::bch_sb_field_members_v2::to_cpu(fs, sb);
}

/// @src's header and sections into @dst - all but those each device has its
/// own of, BCH_SINGLE_DEVICE_SB_FIELDS: __copy_super().
fn copy_super<T: SbResize + ?Sized>(dst: &mut T, src: &bch_sb) -> Result<(), BchError> {
    let d = dst.sb_mut();
    d.version        = src.version;
    d.version_min    = src.version_min;
    d.seq            = src.seq;
    d.uuid           = src.uuid;
    d.user_uuid      = src.user_uuid;
    d.label          = src.label;

    d.block_size     = src.block_size;
    d.nr_devices     = src.nr_devices;

    d.time_base_lo   = src.time_base_lo;
    d.time_base_hi   = src.time_base_hi;
    d.time_precision = src.time_precision;
    d.write_time     = src.write_time;

    d.flags          = src.flags;
    d.features       = src.features;
    d.compat         = src.compat;

    for ty in (0..FIELD_NR as u32).map(c::bch_sb_field_type) {
        if c::BCH_SINGLE_DEVICE_SB_FIELDS & (1 << ty.0) != 0 {
            continue;
        }

        let src_f = field_get_id(src, ty).map(|f| f.as_bytes());
        let u64s = src_f.map_or(0, |f| f.len() / size_of::<u64>());
        let grow = u64s.saturating_sub(field_bytes(dst.sb(), ty) / size_of::<u64>());
        if grow > 0 {
            dst.sb_realloc((dst.sb().u64s() + grow) as u32)?;
        }

        let at = field_offset(dst.sb(), ty);
        let off = dst.field_resize_at(at, u64s as u32);
        if let (Some(off), Some(f)) = (off, src_f) {
            dst.buffer_mut()[off..][..f.len()].copy_from_slice(f);
        }
    }
    Ok(())
}

/// The filesystem's superblock from @src, and what it derives from it:
/// bch2_sb_to_fs().
fn sb_to_fs(fs: &Fs, sb: &mut SbLockGuard<'_>, src: &bch_sb) -> Result<(), BchError> {
    let mut disk_sb = fs.sb_resize(sb);
    disk_sb.sb_realloc(0)?;
    copy_super(&mut disk_sb, src)?;
    c::bch_sb_field_replicas::to_cpu(fs, sb)?;
    c::bch_sb_field_disk_groups::to_cpu(fs, sb)?;
    c::bch_sb_field_extent_type_u64s::to_cpu(fs, sb)?;
    sb_update(fs, sb);
    Ok(())
}

/// # Safety
/// @src is a validated superblock, not the filesystem's.
#[c_export]
pub unsafe fn bch2_sb_to_fs(c: &Opaque<c::bch_fs>, src: *mut Head<c::bch_sb>) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock; @src is the caller's
    ret_to_c(sb_to_fs(fs, &mut unsafe { fs.sb_lock_held() }, unsafe { bch_sb::<Validated>::from_ffi(src) }))
}

/// @ca's superblock from the filesystem's: bch2_sb_from_fs().
#[c_export]
pub fn bch2_sb_from_fs(c: &Opaque<c::bch_fs>, ca: &c::bch_dev) -> c_int {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    let mut sb = unsafe { fs.sb_lock_held() };
    let mut dst = fs.dev_sb_mut(&mut sb, ca).expect("bch2_sb_from_fs(): a device with no superblock");
    ret_to_c(dst.sync())
}

// ── Superblocks in byte buffers ──────────────────────────────────────────
//
// For device scans and recovery, without a bch_sb_handle: a superblock in a
// buffer of its own - as read, Unvalidated, its count checked against its
// bytes, and Validated only once validate() passes. A buffer can't be
// reallocated, so resizing and deleting sections stay bch_sb_handle's.

pub const BCACHE_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x4e, 0x1a, 0x45, 0xca,
    0x82, 0x65, 0xf5, 0x7f, 0x48, 0xba, 0x6d, 0x81,
];
pub const BCHFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9,
    0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

/// Anything holding a valid superblock that can be viewed as bytes and
/// mutated in place: SbBuf, and bch_sb_handle for C-owned superblocks.
/// Superblock IO takes any of them.
pub trait SbAccess {
    fn sb(&self) -> &bch_sb;
    fn sb_mut(&mut self) -> &mut bch_sb;
    /// The superblock's full vstruct extent.
    fn sb_bytes(&self) -> &[u8];
}

impl SbAccess for bch_sb_buf {
    fn sb(&self) -> &bch_sb { bch_sb_buf::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { bch_sb_buf::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { bch_sb_buf::sb_bytes(self) }
}

impl SbAccess for bch_sb_handle {
    fn sb(&self) -> &bch_sb { self.io.buf.sb() }
    fn sb_mut(&mut self) -> &mut bch_sb { self.io.buf.sb_mut() }
    fn sb_bytes(&self) -> &[u8] { self.io.buf.sb_bytes() }
}

/// An owned superblock, in state @S: Unvalidated as read, from_bytes();
/// Validated once validate() passes, or copied from one that's valid.
#[cfg(feature = "std")]
pub struct SbBuf<S: State = Validated> {
    /// In u64s: a superblock's alignment. As long as its count says.
    buf:    Vec<u64>,
    _state: core::marker::PhantomData<S>,
}

#[cfg(feature = "std")]
impl<S: State> SbBuf<S> {
    /// @b, in a buffer of its own.
    fn copied(b: &[u8]) -> Self {
        let mut buf = vec![0u64; b.len().div_ceil(size_of::<u64>())];
        crate::util::vstructs::plain_bytes_mut(&mut buf[..])[..b.len()].copy_from_slice(b);
        SbBuf { buf, _state: core::marker::PhantomData }
    }

    fn raw(&self) -> &[u8] {
        crate::util::vstructs::plain_bytes(&self.buf[..])
    }

    fn raw_mut(&mut self) -> &mut [u8] {
        crate::util::vstructs::plain_bytes_mut(&mut self.buf[..])
    }
}

#[cfg(feature = "std")]
impl SbBuf<Unvalidated> {
    /// The superblock at the start of @src - which needn't be aligned - as
    /// read: an error if @src doesn't hold its header, or as many bytes as
    /// its count says.
    pub fn from_bytes(src: &[u8]) -> Result<Self, VStructError> {
        let mut sb = Self::copied(src);
        let len = bch_sb::<Unvalidated>::from_bytes(sb.raw())?.as_bytes().len();
        sb.buf.truncate(len.div_ceil(size_of::<u64>()));
        Ok(sb)
    }

    pub fn sb(&self) -> &bch_sb<Unvalidated> {
        bch_sb::<Unvalidated>::from_bytes(self.raw()).expect("its count, checked when it was made")
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb<Unvalidated> {
        bch_sb::<Unvalidated>::from_bytes_mut(self.raw_mut()).expect("its count, checked when it was made")
    }

    /// validate(), as read from @read_offset: Validated if it passes.
    pub fn validate(mut self, opts: &c::bch_opts, read_offset: u64, flags: c::bch_validate_flags,
                    out: &mut Printbuf) -> Result<SbBuf<Validated>, BchError> {
        validate(self.sb_mut(), opts, read_offset, flags, out)?;
        Ok(SbBuf { buf: self.buf, _state: core::marker::PhantomData })
    }
}

#[cfg(feature = "std")]
impl SbBuf<Validated> {
    /// A copy of @sb: as valid as it is.
    pub fn from_sb(sb: &bch_sb) -> Self {
        Self::copied(sb.as_bytes())
    }

    pub fn sb(&self) -> &bch_sb {
        let sb = bch_sb::<Unvalidated>::from_bytes(self.raw()).expect("its count, validated");
        // SAFETY: validate() passed, or it's a copy of one that did
        unsafe { sb.assume_validated() }
    }

    pub fn sb_mut(&mut self) -> &mut bch_sb {
        let sb = bch_sb::<Unvalidated>::from_bytes_mut(self.raw_mut()).expect("its count, validated");
        // SAFETY: as sb()'s
        unsafe { sb.assume_validated_mut() }
    }

    pub fn bytes(&self) -> &[u8] {
        self.sb().as_bytes()
    }
}

#[cfg(feature = "std")]
impl SbAccess for SbBuf {
    fn sb(&self) -> &bch_sb { SbBuf::<Validated>::sb(self) }
    fn sb_mut(&mut self) -> &mut bch_sb { SbBuf::<Validated>::sb_mut(self) }
    fn sb_bytes(&self) -> &[u8] { SbBuf::<Validated>::bytes(self) }
}

/// validate_at(), of @sb's section of type F, as a read or a write of it
/// would: for tests.
#[cfg(test)]
pub(crate) fn validate_section<F: SbField + ?Sized>(sb: &mut bch_sb_handle, flags: c::bch_validate_flags,
                                                    err: &mut Printbuf) -> Result<(), BchError> {
    let off = field_offset(sb.sb(), F::FIELD_TYPE).expect("the section");
    let s = bch_sb::<Unvalidated>::from_bytes_mut(sb.buffer_mut()).expect("a superblock within its buffer");
    validate_at(s, off, flags, err)
}

#[cfg(test)]
mod tests {
    use super::SbResize;
    use super::{sb_field_u64s, validate_section};
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

        fn short<F: super::SbField + ?Sized>(ty: c::bch_sb_field_type, u64s: usize) -> String {
            let mut sb = c::bch_sb_handle::default();
            sb.sb_realloc(0).unwrap();
            sb.field_resize_id(ty, u64s as u32).unwrap();

            let mut err = Printbuf::new();
            let ret = validate_section::<F>(&mut sb, c::bch_validate_flags::empty(), &mut err);
            assert_eq!(crate::errcode::ret_to_c(ret), -(c::bch_errcode::BCH_ERR_invalid_sb_field_size.0 as i32));

            let mut out = Printbuf::new();
            super::field_get_id(sb.sb(), ty).unwrap().as_unvalidated()
                .to_text(&mut out, None, sb.sb().as_unvalidated());
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

        // and as the one arm field() asks for
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

        let mut err = Printbuf::new();
        let ret = crate::errcode::ret_to_c(validate_section::<E>(&mut sb, flags, &mut err));
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
