// SPDX-License-Identifier: GPL-2.0

//! BCH_SB_FIELD_clean: btree roots, and a few other things, are recovered
//! from the journal after an unclean shutdown - but after a clean shutdown, to
//! avoid having to read the journal, we can store them in the superblock.
//!
//! The section is a list of journal entries, stored exactly as they would be
//! in the journal, after a header saying which journal seq they're as of.

use core::mem::{size_of, size_of_val};
use core::sync::atomic::Ordering::Relaxed;

use zerocopy::IntoBytes;

use crate::btree::bkey::{AnyKey, BkeySCToText, ValidateFlags, BkeyValidateContext,
    BkeyValidateFrom};
use crate::btree::interior::btree_roots_to_journal_entries;
use crate::errcode::{Errcode, BchError};
use crate::fs::{Fs, SbLockGuard, btree_id_is_alloc};
use crate::journal::entries::{entry_u64s, EntryWriter, NoRoom};
use crate::journal::validate::{journal_entry_to_text, journal_entry_validate};
use crate::sb::io::{entries_end, SbFieldOps, SbResize};
use crate::types::{Le32, Le64};
use crate::util::alloc::Vec;
use crate::util::vstructs::{bytes_of_mut, cast_plain_slice_mut, Head, Unsized, Unvalidated, Validated,
                            ValidatedEntriesMut};
use crate::util::{Printbuf, ToText};
use crate::{bch_err, fsck_err, mustfix_fsck_err_on, prt};
use crate::format::{Btree, MetadataVersion, SbFeatures, Sb, SbFieldClean, FsUsageType,
    Jset, JsetEntry, JsetEntryClock, JsetEntryUsage, JsetEntryRef, JsetEntryType, SbCompats,
    SbFieldType};

type Section = SbFieldClean<Validated>;

const SECTION_HEAD: usize = size_of::<Head<Section>>();

/// Nothing of its own to validate: its header whole, and an entry overrunning
/// its end, are sb_field_validate()'s, and the entries themselves are
/// validated late, with the filesystem - validate_late().
impl SbFieldOps for Section {
    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, _sb: &Sb<Unvalidated>,
                f: &SbFieldClean<Unvalidated>) {
        write!(out, "flags:          {:x}\n", f.flags.get());
        write!(out, "journal_seq:    {}\n", f.journal_seq.get());

        for e in f.entries().map_while(Result::ok) {
            if e.type_() == JsetEntryType::btree_keys && e.u64s() == 0 {
                continue;
            }
            journal_entry_to_text(out, fs, e);
            out.newline();
        }
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<JsetEntry<Unvalidated>>(b, SECTION_HEAD, |_| true)
    }
}

// ── Recovery's: the section read, and checked against the journal ──────────

/// Its journal entries validated as the journal's are - which takes the
/// filesystem, as a superblock section's validate can't: of a superblock of
/// @version, @big_endian; @write for one about to be written. In place, as C
/// may fix an entry up as it validates it.
fn validate_late(fs: &Fs, version: MetadataVersion, big_endian: bool, f: &mut Section, write: bool)
    -> Result<(), BchError>
{
    let mut from = BkeyValidateContext::default();
    from.set_from(BkeyValidateFrom::superblock);
    from.set_flags(if write { ValidateFlags::write } else { ValidateFlags::empty() });

    for e in ValidatedEntriesMut::of(f) {
        journal_entry_validate(fs, bytes_of_mut(e), version, big_endian, from)?;
    }
    Ok(())
}

/// A copy of the clean section, validated: recovery's, which uses it after
/// sb_lock is dropped and the superblock may have changed.
pub(crate) struct CleanSection(Vec<u64>);

impl core::ops::Deref for CleanSection {
    type Target = Section;

    fn deref(&self) -> &Section {
        let b = self.0.as_bytes();
        unsafe { &*Section::from_raw(b.as_ptr(), b.len()) }
    }
}

impl Fs {
    pub(crate) fn read_superblock_clean(&self) -> Result<CleanSection, BchError> {
        let mut sb = self.sb_lock();
        let (version, big_endian) = (sb.sb().version.get(), sb.sb().big_endian());

        let Some(f) = sb.field::<Section>() else {
            if fsck_err!(self, sb_clean_missing,
                         "superblock marked clean but clean section not present")? {
                sb.sb_mut().set_clean(false);
                sb.published().clean.set(0);
            }
            return Err(Errcode::invalid_sb_clean.into());
        };

        let bytes = f.as_bytes();
        let mut copy = Vec::from_elem(0u64, bytes.len() / size_of::<u64>())
            .map_err(|_| BchError::from(Errcode::ENOMEM_read_superblock_clean))?;
        copy.as_mut_bytes().copy_from_slice(bytes);

        // SAFETY: a copy of a validated section, all of it
        let b = copy.as_mut_bytes();
        let clean = unsafe { &mut *Section::from_raw_mut(b.as_mut_ptr(), b.len()) };
        validate_late(self, version, big_endian, clean, false)?;

        Ok(CleanSection(copy))
    }
}

type Key<'a> = &'a AnyKey<Validated>;

/// A btree root's entry in @entries, if there is one: its level, and its key
/// - None for an entry with no key.
fn btree_root<'a>(mut entries: impl Iterator<Item = &'a JsetEntry>, id: u32)
    -> Option<(u8, Option<Key<'a>>)>
{
    let (e, r) = entries.find_map(|e| match e.typed() {
        Ok(JsetEntryRef::btree_root(r)) if u32::from(e.btree_id) == id => Some((e, r)),
        _ => None,
    })?;
    Some((e.level, r.entries().next()))
}

/// A btree root's key in an error message.
struct RootText<'a>(&'a Fs, &'a Option<(u8, Option<Key<'a>>)>);

impl ToText for RootText<'_> {
    fn to_text(&self, out: &mut Printbuf) {
        match self.1 {
            None               => write!(out, "(none)"),
            Some((l, None))    => write!(out, "l={l} (no key)"),
            Some((l, Some(k))) => prt!(out, "l={l} {}", BkeySCToText { k: (*k).into(), fs: self.0 }),
        }
    }
}

impl Fs {
    /// Whether the clean section @clean agrees with @j, the journal's last
    /// entry, after a clean shutdown: its seq, and every btree root. False if
    /// its seq doesn't - it's of no use then - and an error if a root doesn't
    /// and that can't be fixed.
    pub(crate) fn verify_superblock_clean(&self, clean: &Section, j: &Jset) -> Result<bool, BchError> {
        if mustfix_fsck_err_on!(self, j.seq != clean.journal_seq,
                sb_clean_journal_seq_mismatch,
                "superblock journal seq ({}) doesn't match journal ({}) after clean shutdown",
                clean.journal_seq.get(), j.seq.get())? {
            return Ok(false);
        }

        for id in 0..Btree::NR {
            let sb = btree_root(clean.entries(), id);
            let journal = btree_root(j.entries(), id);
            if sb.is_none() && journal.is_none() {
                continue;
            }

            let same = match (&sb, &journal) {
                (Some((l1, Some(k1))), Some((l2, Some(k2)))) => l1 == l2 && k1.as_bytes() == k2.as_bytes(),
                _ => false,
            };
            mustfix_fsck_err_on!(self, !same, sb_clean_btree_root_mismatch,
                "superblock btree root {id} doesn't match journal after clean shutdown\n\
                 sb:      {}\n\
                 journal: {}\n", RootText(self, &sb), RootText(self, &journal))?;
        }
        Ok(true)
    }
}

// ── Writing it, at a clean shutdown ──────────────────────────────────────────

/// The u64s super_entries_add_common() writes.
pub(crate) const COMMON_U64S: usize = entry_u64s::<JsetEntryUsage>() + 2 * entry_u64s::<JsetEntryClock>();

/// The entries every journal write carries, and the clean section: the key
/// version, and both io clocks' times.
pub(crate) fn super_entries_add_common(fs: &Fs, w: &mut EntryWriter<'_>) -> Result<(), NoRoom> {
    let mut u = JsetEntryUsage::default();
    u.v = Le64::new(fs.key_version.load(Relaxed) as u64);
    w.push_entry(FsUsageType::key_version.0 as u8, 0, u)?;

    for (rw, clock) in fs.io_clock.iter().enumerate() {
        let mut e = JsetEntryClock::default();
        e.rw = rw as u8;
        e.time = Le64::new(clock.now.load(Relaxed) as u64);
        w.push_entry(0, 0, e)?;
    }
    Ok(())
}

/// Marked dirty, for a write with the journal in use.
pub(crate) fn fs_mark_dirty(sb: &mut SbLockGuard<'_>) {
    let sb = sb.sb_mut();
    sb.set_clean(false);
    sb.features.update(|f| f | SbFeatures::ALWAYS);
}

/// Marked clean: the clean section written, so the next mount needn't read
/// the journal - its btree roots, the key version and io clocks, as of the
/// journal's seq now. The flag is set once the section is written and valid:
/// on an error, logged, the superblock stays dirty, and the next mount reads
/// the journal.
pub(crate) fn fs_mark_clean(fs: &Fs, guard: &mut SbLockGuard<'_>) {
    let mut sb = fs.sb_resize(guard);
    if sb.sb().clean() {
        return;
    }

    let (version, big_endian) = (sb.sb().version.get(), sb.sb().big_endian());
    sb.sb_mut().compat.update(|f| f | SbCompats::alloc_info | SbCompats::alloc_metadata);

    let u64s = SECTION_HEAD / size_of::<u64>() + fs.journal.lock.entry_u64s_reserved.get() as usize;
    let Some(f) = sb.field_resize::<Section>(u64s as u32) else {
        bch_err!(fs, "error resizing superblock while setting filesystem clean");
        return;
    };

    f.flags = Le32::new(0);
    f.journal_seq = Le64::new(fs.journal.lock.seq.load(Relaxed) as u64);
    // Trying to catch outstanding bug:
    assert!(f.journal_seq.get() <= i64::MAX as u64, "journal seq {} past S64_MAX", f.journal_seq.get());

    let entries = cast_plain_slice_mut::<u64>(&mut bytes_of_mut(f)[SECTION_HEAD..])
        .expect("the clean section, u64s past its header");
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

    crate::journal::read::journal_pos_from_member_info_set(fs, guard);
}

/// Strip alloc info from a clean filesystem: removes alloc btree roots
/// from the clean section, replicas, and journal fields.
pub fn strip_alloc(fs: &Fs, guard: &mut SbLockGuard<'_>) {
    {
        let mut sb = fs.sb_resize(guard);

        let f = bytes_of_mut(sb.field_mut::<Section>().expect("a clean filesystem's clean section"));
        let (mut src, mut dst) = (SECTION_HEAD, SECTION_HEAD);
        while src < f.len() {
            let e = JsetEntry::<Unvalidated>::from_bytes(&f[src..]).expect("an entry that fits");
            let len = size_of_val(e);
            if !(e.type_() == JsetEntryType::btree_root && btree_id_is_alloc(e.btree_id.into())) {
                f.copy_within(src..src + len, dst);
                dst += len;
            }
            src += len;
        }
        sb.field_resize::<Section>((dst / size_of::<u64>()) as u32);

        fs.replicas_clear();

        sb.field_delete(SbFieldType::replicas_v0);
        sb.field_delete(SbFieldType::replicas);

        for _ca in fs.online_members() {
            sb.field_delete(SbFieldType::journal);
            sb.field_delete(SbFieldType::journal_v2);
        }
    }

    for ca in fs.members() {
        guard.fs_member_mut(ca.dev_idx.into()).set_freespace_initialized(false);
    }

    guard.sb_mut().features.update(|f| f | SbFeatures::no_alloc_info);
}
