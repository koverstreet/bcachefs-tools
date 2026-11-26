// SPDX-License-Identifier: GPL-2.0

use core::mem::{offset_of, size_of};

use crate::alloc::{replicas::SbEntry, replicas_format::ReplicasEntryV1};
use crate::errcode::{Errcode, BchError};
use crate::format::{Btree, DataType, MetadataVersion, BkeyFormatBits, BkeyPacked,
    BTREE_MAX_DEPTH, FsUsageType, Jset, JsetEntry, JsetEntryBlacklist, JsetEntryBlacklistV2,
    JsetEntryClock, JsetEntryDataUsage, JsetEntryDatetime, JsetEntryDevUsage,
    JsetEntryLog, JsetEntryPrioPtrs, JsetEntryRewind, JsetEntryRewindLimit,
    JsetEntryUsage, KEY_FORMAT_CURRENT, JsetEntry_by_tag, JsetEntry_match,
    JsetEntryDataUsageWhole, JsetEntryType, jset_entry_btree_keys, jset_entry_btree_root,
    jset_entry_overwrite, jset_entry_write_buffer_keys, jset_entry_log_bkey};
use crate::fs::{Dev, Fs};
use crate::btree::bkey::{AnyKey, AnyKeySC, BkeyP, BkeySC, ValidateFlags, BkeyValidateContext,
    BkeyValidateFrom};
use crate::init::error::{fs_inconsistent, id, mustfix_fsck_err, sb_error_count, BkeyValidate};
use crate::init::recovery::btree_lost_data;
use crate::sb::{errors::SbErrorId, members::DevName};
use crate::util::log::LogMsg;
use crate::util::vstructs::{cast_plain, cast_plain_mut, cast_plain_slice_mut, ArmRef,
                            Head, Unvalidated, VStructError, from_bytes};
use crate::util::{DateTime, Printbuf, ToText};
use crate::{message, prt};

const HEAD: usize = size_of::<Head<JsetEntry>>();
const JSET_HEAD: usize = size_of::<Head<Jset>>();

pub(crate) enum JsetCheck {
    Ok,
    None,
    Bad,
}

#[derive(Clone, Copy)]
struct JsetPos {
    seq:  u64,
    u64s: u32,
}

#[derive(Clone, Copy)]
struct EntryPos {
    type_:  u8,
    offset: usize,
}

fn err_msg(out: &mut Printbuf, version: MetadataVersion, jset: Option<JsetPos>, entry: Option<EntryPos>) {
    prt!(out, "invalid journal entry, version={version}");

    if let Some(e) = entry {
        prt!(out, " type=");
        JsetEntryType(e.type_.into()).to_text(out);
    }

    match jset {
        None => prt!(out, " in superblock"),
        Some(j) => {
            prt!(out, " seq={}", j.seq);

            if let Some(e) = entry {
                prt!(out, " offset={}/{}", e.offset, j.u64s);
            }
        }
    }

    prt!(out, ": ");
}

/// What an error in an entry says of where it is, and whether it's being read
/// or written.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    fs:         &'a Fs,
    version:    MetadataVersion,
    big_endian: bool,
    jset:       Option<JsetPos>,
    entry:      Option<EntryPos>,
    from:       BkeyValidateContext,
}

impl<'a> Ctx<'a> {
    fn jset(fs: &'a Fs, h: &Head<Jset>, flags: ValidateFlags) -> Self {
        let mut from = BkeyValidateContext::default();
        from.bits.set_flags(flags);
        from.bits.set_from(BkeyValidateFrom::journal);
        from.journal_seq = h.seq.get();

        Ctx {
            fs,
            version:    h.version.get(),
            big_endian: h.big_endian(),
            jset:       Some(JsetPos { seq: h.seq.get(), u64s: h.u64s() as u32 }),
            entry:      None,
            from,
        }
    }

    fn write(&self) -> bool {
        self.from.bits.flags().contains(ValidateFlags::write)
    }

    fn err(&self, err: SbErrorId, msg: &dyn ToText) -> Result<(), BchError> {
        let mut buf = Printbuf::new();
        err_msg(&mut buf, self.version, self.jset, self.entry);
        msg.to_text(&mut buf);

        if !self.write() {
            mustfix_fsck_err(self.fs, err, &message!("{}", buf))?;
        } else {
            sb_error_count(self.fs, err);
            if fs_inconsistent(self.fs, &message!("corrupt metadata before write: {}\n", buf)) {
                return self.fs.throw(Errcode::fsck_errors_not_fixed);
            }
        }
        Ok(())
    }
}

macro_rules! journal_entry_err_on {
    ($ctx:expr, $cond:expr, $err:ident, $($msg:tt)*) => {
        if $cond {
            $ctx.err(id::$err, &crate::message!($($msg)*))?;
            true
        } else {
            false
        }
    };
}

fn head(e: &[u8]) -> &Head<JsetEntry> {
    cast_plain(e).expect("an entry's header")
}

fn head_mut(e: &mut [u8]) -> &mut Head<JsetEntry> {
    cast_plain_mut(e).expect("an entry's header")
}

/*
 * A key dropped from a journal entry as it was read: what it said never
 * reaches its btree, as for a btree node that couldn't be read - flag the
 * btree, so the passes that rebuild it run. Not on write or commit, where
 * the key is ours and never got out; nor for the superblock's clean section,
 * validated under sb_lock, which bch2_btree_lost_data() takes - its entries
 * are roots, and a missing root recovery handles itself.
 */
fn journal_key_lost(fs: &Fs, from: &BkeyValidateContext) {
    if from.bits.flags().contains(ValidateFlags::write) ||
       from.bits.from() != BkeyValidateFrom::journal {
        return;
    }

    let mut msg = LogMsg::new(fs);
    /* printed only if bch2_btree_lost_data() did something: */
    msg.set_suppressed(true);

    prt!(&mut msg, "journal seq {} offset {}: dropped key\n", from.journal_seq, from.journal_offset);
    let _ = btree_lost_data(fs, &mut msg, from.btree);
}

/// The key at @at, if it's one there - a header, as long as its u64s, within
/// the entry.
fn key_at(e: &[u8], at: usize) -> Result<AnyKeySC<'_, Unvalidated>, VStructError> {
    Ok(BkeySC::from(from_bytes::<AnyKey<Unvalidated>>(&e[at..])?))
}

#[derive(PartialEq, Eq)]
enum Key {
    Kept,
    Deleted,
}

fn bkey_compat(ctx: &Ctx<'_>, k: &mut [u8]) {
    if ctx.version < MetadataVersion::CURRENT ||
       ctx.big_endian != cfg!(target_endian = "big") ||
       cfg!(CONFIG_BCACHEFS_DEBUG) {
        let k = cast_plain_slice_mut(k).and_then(BkeyP::from_slice_mut)
            .expect("a journal key: whole u64s, its u64s checked");
        unsafe {
            crate::btree::compat::bkey_compat(Some(ctx.fs), ctx.from.btree, ctx.version, ctx.big_endian,
                                              ctx.write(), None, k);
        }
    }
}

fn journal_validate_key(ctx: &Ctx<'_>, e: &mut [u8], at: usize) -> Result<Key, BchError> {
    let write = ctx.write();
    let next = HEAD + head(e).u64s() * 8;

    let cut = |e: &mut [u8]| {
        head_mut(e).set_u64s((at - HEAD) / 8);
        e[at..next].fill(0);
        journal_key_lost(ctx.fs, &ctx.from);
        Ok(Key::Deleted)
    };
    let drop = |e: &mut [u8], bytes: usize| {
        let u64s = head(e).u64s();
        head_mut(e).set_u64s(u64s - bytes / 8);
        e.copy_within(at + bytes..next, at);
        e[next - bytes..next].fill(0);
        journal_key_lost(ctx.fs, &ctx.from);
        Ok(Key::Deleted)
    };

    let u64s = e[at] as usize;

    if journal_entry_err_on!(ctx, u64s == 0, journal_entry_bkey_u64s_0, "k->u64s 0") {
        return cut(e);
    }

    if journal_entry_err_on!(ctx, at + u64s * 8 > next, journal_entry_bkey_past_end,
                             "extends past end of journal entry") {
        return cut(e);
    }

    let bytes = u64s * 8;
    let format = BkeyFormatBits::from_bits(e[at + offset_of!(BkeyPacked, format_bits)]).format();
    if journal_entry_err_on!(ctx, format != KEY_FORMAT_CURRENT as u8,
                             journal_entry_bkey_bad_format, "bad format {}", format) {
        return drop(e, bytes);
    }

    if !write {
        bkey_compat(ctx, &mut e[at..at + bytes]);
    }

    let ret = key_at(e, at).map(|k| crate::btree::bkey::bkey_validate(
        &BkeyValidate::new(ctx.fs, k, &ctx.from)));
    match ret {
        Ok(Err(err)) if journal_entry_err_on!(ctx, true, journal_entry_bkey_bad_format,
                                              "bkey validate error {}", err.msg()) => return drop(e, bytes),
        Err(err) if journal_entry_err_on!(ctx, true, journal_entry_bkey_bad_format,
                                          "bkey validate error {err}") => return drop(e, bytes),
        _ => {}
    }

    if write {
        bkey_compat(ctx, &mut e[at..at + bytes]);
    }
    Ok(Key::Kept)
}

fn journal_entry_btree_keys_validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
    let h = head(e);
    let mut ctx = *ctx;
    ctx.from.level = h.level;
    ctx.from.btree = Btree(h.btree_id.into());

    let mut at = HEAD;
    while at < HEAD + head(e).u64s() * 8 {
        if journal_validate_key(&ctx, e, at)? == Key::Deleted {
            continue;
        }

        at += e[at] as usize * 8;
    }

    Ok(())
}

fn journal_entry_btree_keys_to_text(out: &mut Printbuf, fs: Option<&Fs>, e: &[u8]) {
    let h = head(e);
    let last = e.len();
    let mut first = true;

    let mut at = HEAD;
    while at < last {
        if !first {
            out.newline();
            JsetEntryType(h.type_.into()).to_text(out);
            prt!(out, ": ");
        }
        /* We may be called on entries that haven't been validated: */
        let u64s = e[at] as usize;
        if u64s == 0 {
            prt!(out, "(invalid, k->u64s 0)");
            break;
        }

        if at + u64s * 8 > last {
            prt!(out, "(invalid, bkey overruns jset_entry)");
            break;
        }

        prt!(out, "btree={} level={} ", Btree(h.btree_id.into()), h.level);
        match key_at(e, at) {
            Ok(k) => crate::btree::bkey::bkey_val_to_text(out, fs, k),
            Err(err) => {
                prt!(out, "(invalid, {err})");
                break;
            }
        }
        first = false;
        at += u64s * 8;
    }
}

/// An entry's size against its type's, MIN_LEN, and the error a bad one counts.
enum Size {
    Any,
    Exactly(SbErrorId),
    AtLeast(SbErrorId),
}

/// A journal entry type's checks, made on its bytes as read and repaired
/// there, and how it prints.
trait EntryOps: ArmRef {
    /// Checked before validate(): one that's bad is nulled. Any, for a type
    /// whose validate() checks it.
    const SIZE: Size = Size::Any;

    fn validate(_ctx: &Ctx<'_>, _e: &mut [u8]) -> Result<(), BchError> {
        Ok(())
    }

    /// @self, the arm of entry @e.
    fn to_text(&self, e: &JsetEntry<Unvalidated>, out: &mut Printbuf, fs: Option<&Fs>);

    /// Entry @e's arm, its type, its size checked.
    fn arm(e: &[u8]) -> &Self {
        Self::arm_ref(&e[HEAD..]).expect("its size, checked")
    }
}

fn entry_validate<A: EntryOps + ?Sized>(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
    let len = e.len() - HEAD;
    let bad = match A::SIZE {
        Size::Any          => None,
        Size::Exactly(err) => (len != A::MIN_LEN).then_some((err, "")),
        Size::AtLeast(err) => (len < A::MIN_LEN).then_some((err, "at least ")),
    };
    if let Some((err, at_least)) = bad {
        ctx.err(err, &message!("bad size {len}, expected {at_least}{}", A::MIN_LEN))?;
        e.fill(0);
        return Ok(());
    }
    A::validate(ctx, e)
}

fn journal_entry_btree_root_validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
    let h = head(e);
    let u64s = h.u64s();
    if journal_entry_err_on!(ctx, u64s == 0 || u64s != e[HEAD] as usize, journal_entry_btree_root_bad_size,
                             "invalid btree root journal entry: wrong number of keys") ||
       journal_entry_err_on!(ctx, u32::from(h.level) >= BTREE_MAX_DEPTH, journal_entry_btree_root_bad_level,
                             "invalid btree root journal entry: level {} >= max depth {}",
                             h.level, BTREE_MAX_DEPTH) {
        /*
         * we don't want to null out this JsetEntry,
         * just the contents, so that later we can tell
         * we were _supposed_ to have a btree root
         */
        head_mut(e).set_u64s(0);
        e[HEAD..].fill(0);
        return Ok(());
    }

    let mut ctx = *ctx;
    ctx.from.root_bits.set_root(true);
    ctx.from.level = h.level + 1;
    ctx.from.btree = Btree(h.btree_id.into());

    journal_validate_key(&ctx, e, HEAD)?;
    Ok(())
}

fn journal_entry_overwrite_validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
    let mut ctx = *ctx;
    ctx.from.bits.set_flags(ValidateFlags::empty());
    journal_entry_btree_keys_validate(&ctx, e)
}

macro_rules! keys_entry_ops {
    ($($t:ident: $validate:ident),* $(,)?) => { $(
        impl EntryOps for $t<Unvalidated> {
            fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
                $validate(ctx, e)
            }

            fn to_text(&self, e: &crate::format::JsetEntry<Unvalidated>, out: &mut Printbuf, fs: Option<&Fs>) {
                journal_entry_btree_keys_to_text(out, fs, e.as_bytes())
            }
        }
    )* };
}
keys_entry_ops!(
    jset_entry_btree_keys:        journal_entry_btree_keys_validate,
    jset_entry_btree_root:        journal_entry_btree_root_validate,
    jset_entry_overwrite:         journal_entry_overwrite_validate,
    jset_entry_write_buffer_keys: journal_entry_btree_keys_validate,
    jset_entry_log_bkey:          journal_entry_overwrite_validate,
);

impl EntryOps for JsetEntryPrioPtrs<Unvalidated> {
    /* obsolete, don't care: */
    fn to_text(&self, _e: &JsetEntry<Unvalidated>, _out: &mut Printbuf, _fs: Option<&Fs>) {}
}

impl EntryOps for JsetEntryBlacklist<Unvalidated> {
    const SIZE: Size = Size::Exactly(id::journal_entry_blacklist_bad_size);

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "seq={}", self.seq.get());
    }
}

impl EntryOps for JsetEntryBlacklistV2<Unvalidated> {
    const SIZE: Size = Size::Exactly(id::journal_entry_blacklist_v2_bad_size);

    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        let bl = Self::arm(e);
        if journal_entry_err_on!(ctx, bl.start.get() > bl.end.get(), journal_entry_blacklist_v2_start_past_end,
                                 "invalid journal seq blacklist entry: start > end") {
            e.fill(0);
        }
        Ok(())
    }

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "start={} end={}", self.start.get(), self.end.get());
    }
}

impl EntryOps for JsetEntryUsage<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_usage_bad_size);

    fn to_text(&self, e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "type=");
        FsUsageType(e.btree_id.into()).to_text(out);
        prt!(out, " v={}", self.v.get());
    }
}

/// The replicas entry in data_usage arm @v's bytes.
fn data_usage_replicas(v: &[u8]) -> Option<SbEntry<'_>> {
    v.get(offset_of!(JsetEntryDataUsageWhole, r)..)
        .and_then(|r| ReplicasEntryV1::<Unvalidated>::from_bytes(r).ok())
        .map(SbEntry::from)
}

impl EntryOps for JsetEntryDataUsage<Unvalidated> {
    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        let Some(r) = data_usage_replicas(&e[HEAD..]) else {
            ctx.err(id::journal_entry_data_usage_bad_size, &message!("invalid journal entry usage: bad size"))?;
            e.fill(0);
            return Ok(());
        };

        let mut err = Printbuf::new();
        let bad = r.validate(|d| ctx.fs.dev_exists(d.into()), &mut err).is_err();
        if journal_entry_err_on!(ctx, bad, journal_entry_data_usage_bad_size,
                                 "invalid journal entry usage: {}", err) {
            e.fill(0);
        }
        Ok(())
    }

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        match data_usage_replicas(self.as_bytes()) {
            Some(r) => r.to_text(out),
            None    => prt!(out, "(invalid, replicas entry overruns jset_entry)"),
        }
        prt!(out, "={}", self.v.get());
    }
}

impl EntryOps for JsetEntryClock<Unvalidated> {
    const SIZE: Size = Size::Exactly(id::journal_entry_clock_bad_size);

    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        if journal_entry_err_on!(ctx, Self::arm(e).rw > 1, journal_entry_clock_bad_rw, "bad rw") {
            e.fill(0);
        }
        Ok(())
    }

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "{}={}", if self.rw != 0 { "write" } else { "read" }, self.time.get());
    }
}

impl EntryOps for JsetEntryDevUsage<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_dev_usage_bad_size);

    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        if journal_entry_err_on!(ctx, Self::arm(e).pad != 0, journal_entry_dev_usage_bad_pad, "bad pad") {
            e.fill(0);
        }
        Ok(())
    }

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "dev={}", self.dev.get());
        let mut out = out.indent(2);

        for (i, d) in self.d.iter().enumerate() {
            out.newline();
            DataType(i as u32).to_text(&mut out);
            prt!(&mut out, ": buckets={} sectors={} fragmented={}",
                 d.buckets.get(), d.sectors.get(), d.fragmented.get());
        }
    }
}

impl EntryOps for JsetEntryLog<Unvalidated> {
    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        let len = self.d.iter().position(|&b| b == 0).unwrap_or(self.d.len());
        out.write_bytes(&self.d[..len]);
    }
}

impl EntryOps for JsetEntryDatetime<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_datetime_bad_size);

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        DateTime(self.seconds.get() as i64).to_text(out);
    }
}

impl EntryOps for JsetEntryRewindLimit<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_rewind_limit_bad_size);

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "seq {}", self.seq.get());
    }
}

impl EntryOps for JsetEntryRewind<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_rewind_bad_size);

    fn to_text(&self, _e: &JsetEntry<Unvalidated>, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "from {} to {}", self.from.get(), self.to.get());
    }
}

fn entry_validate_typed(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
    JsetEntry_by_tag!(head(e).type_, A => entry_validate::<A<Unvalidated>>(ctx, e), _ => Ok(()))
}

/// Entry @e - its bytes as read, as long as its u64s says - checked, and
/// repaired in place: one not in a jset, the superblock's clean section's or a
/// transaction's.
pub(crate) fn journal_entry_validate(fs: &Fs, e: &mut [u8], version: MetadataVersion, big_endian: bool,
                                     from: BkeyValidateContext) -> Result<(), BchError>
{
    let ctx = Ctx { fs, version, big_endian, jset: None, entry: Some(EntryPos { type_: head(e).type_, offset: 0 }), from };
    entry_validate_typed(&ctx, e)
}

pub fn journal_entry_to_text(out: &mut Printbuf, fs: Option<&Fs>, e: &JsetEntry<Unvalidated>) {
    JsetEntryType(e.type_.into()).to_text(out);

    if u32::from(e.type_) < JsetEntryType::NR {
        prt!(out, ": ");
        if let Ok(r) = e.typed() {
            JsetEntry_match!(r, a => a.to_text(e, out, fs))
        }
    }
}

fn jset_head(j: &[u8]) -> &Head<Jset> {
    cast_plain(j).expect("a jset's header")
}

pub(crate) fn jset_head_mut(j: &mut [u8]) -> &mut Head<Jset> {
    cast_plain_mut(j).expect("a jset's header")
}

pub(crate) fn jset_validate_entries(fs: &Fs, j: &mut [u8], flags: ValidateFlags) -> Result<(), BchError> {
    let mut ctx = Ctx::jset(fs, jset_head(j), flags);
    let u64s = jset_head(j).u64s();
    let data = &mut j[JSET_HEAD..JSET_HEAD + u64s * 8];
    let mut cut = None;

    let mut off = 0;
    while off < data.len() {
        let rest = &data[off..];
        let t = rest.get(offset_of!(Head<JsetEntry>, type_)).copied();
        let len = if rest.len() >= HEAD { HEAD + head(rest).u64s() * 8 } else { usize::MAX };

        ctx.from.journal_offset = (off / 8) as u32;
        ctx.entry = t.map(|type_| EntryPos { type_, offset: off / 8 });

        if journal_entry_err_on!(ctx, len > rest.len(), journal_entry_past_jset_end,
                                 "journal entry extends past end of jset") {
            cut = Some(off);
            break;
        }

        entry_validate_typed(&ctx, &mut data[off..off + len])?;
        off += len;
    }

    if let Some(off) = cut {
        jset_head_mut(j).set_u64s(off / 8);
    }
    Ok(())
}

fn dev_or_fs_name<'a>(fs: &'a Fs, dev: &'a Option<DevName>) -> &'a str {
    dev.as_ref().map_or(fs.name(), DevName::as_cstr).to_str().unwrap_or("?")
}

/// JsetCheck @j - its bytes, as long as its u64s says - checked, its entries
/// repaired in place.
pub(crate) fn jset_validate(fs: &Fs, ca: Option<&Dev>, j: &mut [u8], sector: u64, flags: ValidateFlags)
    -> Result<JsetCheck, BchError>
{
    let h = jset_head(j);
    let (seq, version) = (h.seq.get(), h.version.get());

    if h.magic.get() != fs.jset_magic() {
        return Ok(JsetCheck::None);
    }

    let ctx = Ctx::jset(fs, h, flags);
    let dev = ca.map(Dev::name);
    let name = dev_or_fs_name(fs, &dev);

    if journal_entry_err_on!(ctx, !version.compatible(), jset_unsupported_version,
                             "{} sector {} seq {}: incompatible journal entry version {}.{}",
                             name, sector, seq, version.major(), version.minor()) {
        /* don't try to continue: */
        return fs.throw(Errcode::EINVAL_journal_entry_version_incompatible);
    }

    journal_entry_err_on!(ctx, !h.csum_type().valid(fs), jset_unknown_csum,
                          "{} sector {} seq {}: journal entry with unknown csum type {}",
                          name, sector, seq, h.csum_type());

    /* last_seq is ignored when JSET_NO_FLUSH is true */
    if !h.no_flush() && jset_last_seq_check(fs, jset_head_mut(j), flags)? {
        return Ok(JsetCheck::Bad);
    }

    jset_validate_entries(fs, j, flags)?;
    Ok(JsetCheck::Ok)
}

pub(crate) fn jset_last_seq_check(fs: &Fs, h: &mut Head<Jset>, flags: ValidateFlags) -> Result<bool, BchError> {
    let ctx = Ctx::jset(fs, h, flags);
    let seq = h.seq.get();

    if journal_entry_err_on!(ctx, h.last_seq.get() > seq, jset_last_seq_newer_than_seq,
                             "invalid journal entry: last_seq > seq ({} > {})", h.last_seq.get(), seq) {
        h.last_seq = crate::types::Le64::new(seq);
        return Ok(true);
    }
    Ok(false)
}

/// JsetCheck @h's header, as read from a bucket with @bucket_sectors_left: cut to
/// fit, if it says it's longer.
pub(crate) fn jset_validate_early(fs: &Fs, ca: Option<&Dev>, h: &mut Head<Jset>, sector: u64,
                                  bucket_sectors_left: u32) -> Result<JsetCheck, BchError>
{
    let (seq, version) = (h.seq.get(), h.version.get());

    if h.magic.get() != fs.jset_magic() {
        return Ok(JsetCheck::None);
    }

    let ctx = Ctx::jset(fs, h, ValidateFlags::empty());
    let dev = ca.map(Dev::name);
    let name = dev_or_fs_name(fs, &dev);

    if journal_entry_err_on!(ctx, !version.compatible(), jset_unsupported_version,
                             "{} sector {} seq {}: unknown journal entry version {}.{}",
                             name, sector, seq, version.major(), version.minor()) {
        /* don't try to continue: */
        return fs.throw(Errcode::EINVAL_journal_validate_version_incompatible);
    }

    let bytes = size_of::<Head<Jset>>() + h.u64s() * size_of::<u64>();
    let max = (bucket_sectors_left as usize) << 9;

    if journal_entry_err_on!(ctx, bytes > max, jset_past_bucket_end,
                             "{} sector {} seq {}: journal entry too big ({} bytes)", name, sector, seq, bytes) {
        h.set_u64s(h.u64s() - (bytes - max) / 8);
    }
    Ok(JsetCheck::Ok)
}

