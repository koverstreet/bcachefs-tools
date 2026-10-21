// SPDX-License-Identifier: GPL-2.0

//! BCH_SB_FIELD_clean: btree roots, and a few other things, are recovered
//! from the journal after an unclean shutdown - but after a clean shutdown, to
//! avoid having to read the journal, we can store them in the superblock.
//!
//! The section is a list of journal entries, stored exactly as they would be
//! in the journal, after a header saying which journal seq they're as of.

use core::ffi::c_int;
use core::mem::size_of;
use core::sync::atomic::{AtomicI64, Ordering::Relaxed};

use cstruct_macros::c_export;
use zerocopy::IntoBytes;

use crate::btree::bkey::BkeySCToText;
use crate::btree::interior::btree_roots_to_journal_entries;
use crate::c;
use crate::errcode::{bch_errcode, err_ptr, ret_to_c, ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::journal::entries::{EntryWriter, NoRoom};
use crate::sb::io::{entries_end, SbFieldOps};
use crate::types::{Le32, Le64};
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::ffi::Opaque;
use crate::util::vstructs::{bytes_of_mut, Head, Unvalidated, UnvalidatedEntriesMut, Validated, VstructRef};
use crate::util::{Printbuf, ToText};
use crate::{bch_err, fsck_err, mustfix_fsck_err_on, prt};

type Section = c::bch_sb_field_clean;

const SECTION_HEAD: usize = size_of::<Head<Section>>();

/// Nothing of its own to validate: its header whole, and an entry overrunning
/// its end, are sb_field_validate()'s, and the entries themselves are
/// validated late, with the filesystem - bch2_sb_clean_validate_late().
impl SbFieldOps for Section {
    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_clean<Unvalidated>) {
        write!(out, "flags:          {:x}\n", f.flags.get());
        write!(out, "journal_seq:    {}\n", f.journal_seq.get());

        let fs = fs.map_or(core::ptr::null_mut(), Fs::as_ptr);
        for e in f.entries().map_while(Result::ok) {
            if e.type_ == c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_keys.0 as u8 && e.u64s() == 0 {
                continue;
            }
            // SAFETY: a whole entry; C takes NULL for no filesystem
            unsafe { c::bch2_journal_entry_to_text(out.as_raw(), fs, e.as_ffi().cast_mut()) };
            out.newline();
        }
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<c::jset_entry<Validated>>(b, SECTION_HEAD, |_| true)
    }
}

// ── Recovery's: the section read, and checked against the journal ──────────

/// Its journal entries validated as the journal's are - which takes the
/// filesystem, as a superblock section's validate can't: of a superblock of
/// @version, @big_endian; @write for one about to be written. In place, as C
/// may fix an entry up as it validates it.
fn validate_late(fs: &Fs, version: u16, big_endian: bool, f: &mut Section, write: bool)
    -> Result<(), BchError>
{
    let mut from = c::bkey_validate_context::default();
    from.set_from(c::bkey_validate_from::BKEY_VALIDATE_superblock);
    from.set_flags(if write { c::bch_validate_flags::BCH_VALIDATE_write } else { c::bch_validate_flags::empty() });

    for e in UnvalidatedEntriesMut::<c::jset_entry<Unvalidated>>::new(&mut bytes_of_mut(f)[SECTION_HEAD..]) {
        // whole: sb_field_validate() has repaired or refused a section with
        // one overrunning its end
        let e = e.map_err(|_| BchError::from(bch_errcode::BCH_ERR_invalid_sb_clean))?;
        // SAFETY: a whole entry, ours to change
        ret_to_result(unsafe {
            c::bch2_journal_entry_validate(fs.as_ptr(), core::ptr::null_mut(), e.as_ffi_mut(),
                                           version.into(), big_endian.into(), from)
        })?;
    }
    Ok(())
}

impl Fs {
    /// A copy of the clean section, validated - for recovery, which uses it
    /// after sb_lock is dropped and the superblock may have changed, and
    /// frees it with kvfree().
    fn read_superblock_clean(&self) -> Result<*mut Head<c::bch_sb_field_clean>, BchError> {
        let mut sb = self.sb_lock();
        let (version, big_endian) = (sb.sb().version.get(), sb.sb().big_endian());

        let Some(f) = sb.field::<Section>() else {
            if fsck_err!(self, c::bch_sb_error_id::BCH_FSCK_ERR_sb_clean_missing,
                         "superblock marked clean but clean section not present")? {
                sb.sb_mut().set_clean(false);
                sb.published().clean.set(0);
            }
            return Err(bch_errcode::BCH_ERR_invalid_sb_clean.into());
        };

        let bytes = f.as_bytes();
        let mut copy = KVVec::from_elem(0u64, bytes.len() / size_of::<u64>(), GFP_KERNEL)
            .map_err(|_| BchError::from(bch_errcode::BCH_ERR_ENOMEM_read_superblock_clean))?;
        copy.as_mut_bytes().copy_from_slice(bytes);

        // SAFETY: a copy of a validated section, as long as its count says
        let clean = unsafe { Section::from_ffi_mut(copy.as_mut_ptr().cast()) };
        validate_late(self, version, big_endian, clean, false)?;

        let (p, _, _) = copy.into_raw_parts();
        Ok(p.cast())
    }
}

#[c_export]
pub fn bch2_read_superblock_clean(c: &Opaque<c::bch_fs>) -> *mut Head<c::bch_sb_field_clean> {
    Fs::from_c(c).read_superblock_clean().unwrap_or_else(err_ptr)
}

type Key<'a> = VstructRef<'a, c::bkey_i, Validated>;

/// A btree root's entry in @entries, if there is one: its level, and its key
/// - None for an entry with no key.
fn btree_root<'a>(mut entries: impl Iterator<Item = &'a c::jset_entry>, id: u32)
    -> Option<(u8, Option<Key<'a>>)>
{
    let e = entries.find(|e| e.type_ == c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_root.0 as u8 &&
                              u32::from(e.btree_id) == id)?;
    Some((e.level, e.entries().next()))
}

/// A btree root's key in an error message.
struct RootText<'a>(&'a Fs, &'a Option<(u8, Option<Key<'a>>)>);

impl ToText for RootText<'_> {
    fn to_text(&self, out: &mut Printbuf) {
        match self.1 {
            None               => write!(out, "(none)"),
            Some((l, None))    => write!(out, "l={l} (no key)"),
            Some((l, Some(k))) => prt!(out, "l={l} {}", BkeySCToText { k: k.key().into(), fs: self.0 }),
        }
    }
}

impl Fs {
    /// Whether the clean section @clean agrees with @j, the journal's last
    /// entry, after a clean shutdown: its seq, and every btree root. False if
    /// its seq doesn't - it's of no use then - and an error if a root doesn't
    /// and that can't be fixed.
    fn verify_superblock_clean(&self, clean: &Section, j: &c::jset) -> Result<bool, BchError> {
        if mustfix_fsck_err_on!(self, j.seq != clean.journal_seq,
                c::bch_sb_error_id::BCH_FSCK_ERR_sb_clean_journal_seq_mismatch,
                "superblock journal seq ({}) doesn't match journal ({}) after clean shutdown",
                clean.journal_seq.get(), j.seq.get())? {
            return Ok(false);
        }

        for id in 0..c::btree_id::BTREE_ID_NR.0 {
            let sb = btree_root(clean.entries(), id);
            let journal = btree_root(j.entries(), id);
            if sb.is_none() && journal.is_none() {
                continue;
            }

            let same = match (&sb, &journal) {
                (Some((l1, Some(k1))), Some((l2, Some(k2)))) => l1 == l2 && k1.as_bytes() == k2.as_bytes(),
                _ => false,
            };
            mustfix_fsck_err_on!(self, !same, c::bch_sb_error_id::BCH_FSCK_ERR_sb_clean_btree_root_mismatch,
                "superblock btree root {id} doesn't match journal after clean shutdown\n\
                 sb:      {}\n\
                 journal: {}\n", RootText(self, &sb), RootText(self, &journal))?;
        }
        Ok(true)
    }
}

/// # Safety
/// *@cleanp is from bch2_read_superblock_clean(), and @j the last journal
/// entry, validated.
#[c_export]
pub unsafe fn bch2_verify_superblock_clean(c: &Opaque<c::bch_fs>, cleanp: *mut *mut Head<c::bch_sb_field_clean>,
                                           j: *mut Head<c::jset>) -> c_int {
    // SAFETY: the caller's
    let (clean, j) = unsafe { (Section::from_ffi(*cleanp), c::jset::<Validated>::from_ffi(j)) };
    match Fs::from_c(c).verify_superblock_clean(clean, j) {
        Ok(true) => 0,
        Ok(false) => {
            // SAFETY: a KVVec's buffer, which nothing uses after
            unsafe {
                c::rust_kvfree((*cleanp).cast());
                *cleanp = core::ptr::null_mut();
            }
            0
        }
        Err(e) => ret_to_c(Err::<(), _>(e)),
    }
}

// ── Writing it, at a clean shutdown ──────────────────────────────────────────

/// C's atomic64_t, read: atomic64_read().
fn atomic64_read(a: &c::atomic64_t) -> u64 {
    // SAFETY: an atomic64_t is one s64, which C updates atomically
    unsafe { &*(&raw const *a).cast::<AtomicI64>() }.load(Relaxed) as u64
}

/// The bytes super_entries_add_common() writes.
const COMMON_BYTES: usize = size_of::<c::jset_entry_usage>() + 2 * size_of::<c::jset_entry_clock>();

/// The entries every journal write carries, and the clean section: the key
/// version, and both io clocks' times.
fn super_entries_add_common(fs: &Fs, w: &mut EntryWriter<'_>) -> Result<(), NoRoom> {
    let mut u = c::jset_entry_usage::default();
    u.entry.type_ = c::bch_jset_entry_type::BCH_JSET_ENTRY_usage.0 as u8;
    u.entry.btree_id = c::bch_fs_usage_type::BCH_FS_USAGE_key_version.0 as u8;
    u.v = Le64::new(atomic64_read(&fs.key_version));
    w.push_entry(u)?;

    for (rw, clock) in fs.io_clock.iter().enumerate() {
        let mut e = c::jset_entry_clock::default();
        e.entry.type_ = c::bch_jset_entry_type::BCH_JSET_ENTRY_clock.0 as u8;
        e.rw = rw as u8;
        e.time = Le64::new(atomic64_read(&clock.now));
        w.push_entry(e)?;
    }
    Ok(())
}

/// super_entries_add_common(), for C's journal write: at *@end, which it
/// moves past them.
///
/// # Safety
/// There's room for them at *@end.
#[c_export]
pub unsafe fn bch2_journal_super_entries_add_common(c: &Opaque<c::bch_fs>, end: *mut *mut Head<c::jset_entry>,
                                                    _seq: u64) {
    // SAFETY: the caller's - room for these
    let buf = unsafe { core::slice::from_raw_parts_mut((*end).cast(), COMMON_BYTES) };
    super_entries_add_common(Fs::from_c(c), &mut EntryWriter::new(buf)).expect("sized for them");
    // SAFETY: as long as what was written
    unsafe { *end = (*end).byte_add(COMMON_BYTES) };
}

#[c_export]
pub fn bch2_fs_mark_dirty(c: &Opaque<c::bch_fs>) {
    // SAFETY: C calls it under sb_lock, from bch2_write_super()
    let mut sb = unsafe { Fs::from_c(c).sb_lock_held() };
    let sb = sb.sb_mut();
    sb.set_clean(false);
    sb.features[0] |= c::BCH_SB_FEATURES_ALWAYS;
}

/// Marked clean: the clean section written, so the next mount needn't read
/// the journal - its btree roots, the key version and io clocks, as of the
/// journal's seq now. The flag is set once the section is written and valid:
/// on an error, logged, the superblock stays dirty, and the next mount reads
/// the journal.
#[c_export]
pub fn bch2_fs_mark_clean(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock, from bch2_write_super()
    let mut sb = unsafe { fs.sb_lock_held() };
    if sb.sb().clean() {
        return;
    }

    let (version, big_endian) = (sb.sb().version.get(), sb.sb().big_endian());
    sb.sb_mut().compat[0] |= 1 << c::bch_sb_compat::BCH_COMPAT_alloc_info.0 |
                             1 << c::bch_sb_compat::BCH_COMPAT_alloc_metadata.0;

    let u64s = SECTION_HEAD / size_of::<u64>() + fs.journal.entry_u64s_reserved as usize;
    let Some(f) = sb.field_resize::<Section>(u64s as u32) else {
        bch_err!(fs, "error resizing superblock while setting filesystem clean");
        return;
    };

    f.flags = Le32::new(0);
    f.journal_seq = Le64::new(fs.journal.seq.load(Relaxed) as u64);
    // Trying to catch outstanding bug:
    assert!(f.journal_seq.get() <= i64::MAX as u64, "journal seq {} past S64_MAX", f.journal_seq.get());

    let entries = &mut bytes_of_mut(f)[SECTION_HEAD..];
    let mut w = EntryWriter::new(entries);
    if super_entries_add_common(fs, &mut w).and_then(|()| btree_roots_to_journal_entries(fs, &mut w, 0)).is_err() {
        bch_err!(fs, "error setting filesystem clean: its entries overran entry_u64s_reserved");
        return;
    }
    let written = w.len();
    entries[written..].fill(0);

    // this should be in the write path, and we should be validating every
    // superblock section:
    if validate_late(fs, version, big_endian, f, true).is_err() {
        bch_err!(fs, "error writing marking filesystem clean: validate error");
        return;
    }
    sb.sb_mut().set_clean(true);

    // SAFETY: under sb_lock, which it needs
    unsafe { c::bch2_journal_pos_from_member_info_set(fs.as_ptr()) };
}
