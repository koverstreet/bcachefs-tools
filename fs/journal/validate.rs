// SPDX-License-Identifier: GPL-2.0

use core::ffi::{c_int, c_uint};
use core::mem::{offset_of, size_of};

use crate::alloc::replicas::SbEntry;
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::format::{Btree, DataType};
use crate::fs::{Dev, Fs};
use crate::btree::bkey::{BkeySC, ValidateFlags};
use crate::init::error::{fs_inconsistent, id, mustfix_fsck_err, sb_error_count, BkeyValidate};
use crate::init::recovery::btree_lost_data;
use crate::sb::errors::SbErrorId;
use crate::util::ffi::Opaque;
use crate::util::log::LogMsg;
use crate::util::vstructs::{bytes_of, cast_plain, cast_plain_mut, ArmRef, Head, SelfSized,
                            Unvalidated, VStruct};
use crate::util::{DateTime, Printbuf, ToText};
use crate::{message, prt};
use cstruct_macros::c_export;

const HEAD: usize = size_of::<Head<c::jset_entry>>();
const JSET_HEAD: usize = size_of::<Head<c::jset>>();

pub(crate) enum Jset {
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

fn err_msg(out: &mut Printbuf, version: u32, jset: Option<JsetPos>, entry: Option<EntryPos>) {
    prt!(out, "invalid journal entry, version=");
    crate::sb::io::version(version).to_text(out);

    if let Some(e) = entry {
        prt!(out, " type=");
        c::bch_jset_entry_type(e.type_.into()).to_text(out);
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
    version:    u32,
    big_endian: bool,
    jset:       Option<JsetPos>,
    entry:      Option<EntryPos>,
    from:       c::bkey_validate_context,
}

impl<'a> Ctx<'a> {
    fn jset(fs: &'a Fs, h: &Head<c::jset>, flags: ValidateFlags) -> Self {
        let mut from = c::bkey_validate_context::default();
        from.bits.set_flags(flags);
        from.bits.set_from(c::bkey_validate_from::journal);
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
                return self.fs.throw(bch_errcode::BCH_ERR_fsck_errors_not_fixed);
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

fn head(e: &[u8]) -> &Head<c::jset_entry> {
    cast_plain(e).expect("an entry's header")
}

fn head_mut(e: &mut [u8]) -> &mut Head<c::jset_entry> {
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
fn journal_key_lost(fs: &Fs, from: &c::bkey_validate_context) {
    if from.bits.flags().contains(ValidateFlags::write) ||
       from.bits.from() != c::bkey_validate_from::journal {
        return;
    }

    let mut msg = LogMsg::new(fs);
    /* printed only if bch2_btree_lost_data() did something: */
    msg.set_suppressed(true);

    prt!(&mut msg, "journal seq {} offset {}: dropped key\n", from.journal_seq, from.journal_offset);
    let _ = btree_lost_data(fs, &mut msg, from.btree);
}

fn key_at(e: &[u8], at: usize) -> c::bkey_s_c {
    let k = e[at..].as_ptr();
    c::bkey_s_c { k: k.cast(), v: k.wrapping_add(size_of::<c::bkey>()).cast() }
}

#[derive(PartialEq, Eq)]
enum Key {
    Kept,
    Deleted,
}

fn bkey_compat(ctx: &Ctx<'_>, k: &mut [u8]) {
    if ctx.version < c::bcachefs_metadata_version_current as u32 ||
       ctx.big_endian != cfg!(target_endian = "big") ||
       cfg!(CONFIG_BCACHEFS_DEBUG) {
        unsafe {
            crate::btree::compat::__bch2_bkey_compat(Some(ctx.fs), ctx.from.level.into(), ctx.from.btree, ctx.version,
                                                     ctx.big_endian.into(), ctx.write().into(), None,
                                                     k.as_mut_ptr().cast());
        }
    }
}

fn journal_validate_key(ctx: &Ctx<'_>, e: &mut [u8], at: usize) -> Result<Key, BchError> {
    let write = ctx.write();
    let next = HEAD + head(e).u64s() * 8;

    let cut = |e: &mut [u8]| {
        head_mut(e).set_val_u64s((at - HEAD) / 8);
        e[at..next].fill(0);
        journal_key_lost(ctx.fs, &ctx.from);
        Ok(Key::Deleted)
    };
    let drop = |e: &mut [u8], bytes: usize| {
        let u64s = head(e).u64s();
        head_mut(e).set_val_u64s(u64s - bytes / 8);
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
    let format = c::bkey_format_bits::from_bits(e[at + offset_of!(c::bkey_packed, format_bits)]).format();
    if journal_entry_err_on!(ctx, format != c::KEY_FORMAT_CURRENT as u8,
                             journal_entry_bkey_bad_format, "bad format {}", format) {
        return drop(e, bytes);
    }

    if !write {
        bkey_compat(ctx, &mut e[at..at + bytes]);
    }

    let k = key_at(e, at);
    let ret = crate::btree::bkey::bkey_validate(
        &BkeyValidate { fs: ctx.fs, k: unsafe { BkeySC::from_c(&k) }, from: &ctx.from });
    if let Err(err) = ret {
        if journal_entry_err_on!(ctx, true, journal_entry_bkey_bad_format,
                                 "bkey validate error {}", err.msg()) {
            return drop(e, bytes);
        }
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
            c::bch_jset_entry_type(h.type_.into()).to_text(out);
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
        let k = key_at(e, at);
        crate::btree::bkey::bkey_val_to_text(out, fs, unsafe { BkeySC::from_c(&k) });
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

    fn to_text(&self, out: &mut Printbuf, fs: Option<&Fs>);

    /// Entry @e as its type, its size checked.
    fn arm(e: &[u8]) -> &Self {
        Self::arm_ref(e).expect("its size, checked")
    }
}

fn entry_validate<A: EntryOps + ?Sized>(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
    let bad = match A::SIZE {
        Size::Any          => None,
        Size::Exactly(err) => (e.len() != A::MIN_LEN).then_some((err, "")),
        Size::AtLeast(err) => (e.len() < A::MIN_LEN).then_some((err, "at least ")),
    };
    if let Some((err, at_least)) = bad {
        ctx.err(err, &message!("bad size {}, expected {at_least}{}", e.len(), A::MIN_LEN))?;
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
       journal_entry_err_on!(ctx, u32::from(h.level) >= c::BTREE_MAX_DEPTH, journal_entry_btree_root_bad_level,
                             "invalid btree root journal entry: level {} >= max depth {}",
                             h.level, c::BTREE_MAX_DEPTH) {
        /*
         * we don't want to null out this jset_entry,
         * just the contents, so that later we can tell
         * we were _supposed_ to have a btree root
         */
        head_mut(e).set_val_u64s(0);
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
        impl EntryOps for c::$t<Unvalidated> {
            fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
                $validate(ctx, e)
            }

            fn to_text(&self, out: &mut Printbuf, fs: Option<&Fs>) {
                journal_entry_btree_keys_to_text(out, fs, bytes_of(self))
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

impl EntryOps for c::jset_entry_prio_ptrs<Unvalidated> {
    /* obsolete, don't care: */
    fn to_text(&self, _out: &mut Printbuf, _fs: Option<&Fs>) {}
}

impl EntryOps for c::jset_entry_blacklist<Unvalidated> {
    const SIZE: Size = Size::Exactly(id::journal_entry_blacklist_bad_size);

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "seq={}", self.seq.get());
    }
}

impl EntryOps for c::jset_entry_blacklist_v2<Unvalidated> {
    const SIZE: Size = Size::Exactly(id::journal_entry_blacklist_v2_bad_size);

    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        let bl = Self::arm(e);
        if journal_entry_err_on!(ctx, bl.start.get() > bl.end.get(), journal_entry_blacklist_v2_start_past_end,
                                 "invalid journal seq blacklist entry: start > end") {
            e.fill(0);
        }
        Ok(())
    }

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "start={} end={}", self.start.get(), self.end.get());
    }
}

impl EntryOps for c::jset_entry_usage<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_usage_bad_size);

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "type=");
        c::bch_fs_usage_type(self.entry.btree_id.into()).to_text(out);
        prt!(out, " v={}", self.v.get());
    }
}

fn data_usage_replicas(e: &[u8]) -> Option<SbEntry<'_>> {
    e.get(offset_of!(Head<c::jset_entry_data_usage>, r)..)
        .and_then(|r| c::bch_replicas_entry_v1::<Unvalidated>::from_bytes(r).ok())
        .map(SbEntry::from)
}

impl EntryOps for c::jset_entry_data_usage<Unvalidated> {
    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        let Some(r) = data_usage_replicas(e) else {
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

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        match data_usage_replicas(bytes_of(self)) {
            Some(r) => r.to_text(out),
            None    => prt!(out, "(invalid, replicas entry overruns jset_entry)"),
        }
        prt!(out, "={}", self.v.get());
    }
}

impl EntryOps for c::jset_entry_clock<Unvalidated> {
    const SIZE: Size = Size::Exactly(id::journal_entry_clock_bad_size);

    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        if journal_entry_err_on!(ctx, Self::arm(e).rw > 1, journal_entry_clock_bad_rw, "bad rw") {
            e.fill(0);
        }
        Ok(())
    }

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "{}={}", if self.rw != 0 { "write" } else { "read" }, self.time.get());
    }
}

impl EntryOps for c::jset_entry_dev_usage<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_dev_usage_bad_size);

    fn validate(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
        if journal_entry_err_on!(ctx, Self::arm(e).pad != 0, journal_entry_dev_usage_bad_pad, "bad pad") {
            e.fill(0);
        }
        Ok(())
    }

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
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

impl EntryOps for c::jset_entry_log<Unvalidated> {
    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        let len = self.d.iter().position(|&b| b == 0).unwrap_or(self.d.len());
        out.write_bytes(&self.d[..len]);
    }
}

impl EntryOps for c::jset_entry_datetime<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_datetime_bad_size);

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        DateTime(self.seconds.get() as i64).to_text(out);
    }
}

impl EntryOps for c::jset_entry_rewind_limit<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_rewind_limit_bad_size);

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "seq {}", self.seq.get());
    }
}

impl EntryOps for c::jset_entry_rewind<Unvalidated> {
    const SIZE: Size = Size::AtLeast(id::journal_entry_rewind_bad_size);

    fn to_text(&self, out: &mut Printbuf, _fs: Option<&Fs>) {
        prt!(out, "from {} to {}", self.from.get(), self.to.get());
    }
}

fn entry_validate_typed(ctx: &Ctx<'_>, e: &mut [u8]) -> Result<(), BchError> {
    c::jset_entry_by_tag!(head(e).type_, A => entry_validate::<A<Unvalidated>>(ctx, e), _ => Ok(()))
}

/// Entry @e - its bytes as read, as long as its u64s says - checked, and
/// repaired in place: one not in a jset, the superblock's clean section's or a
/// transaction's.
pub(crate) fn journal_entry_validate(fs: &Fs, e: &mut [u8], version: u32, big_endian: bool,
                                     from: c::bkey_validate_context) -> Result<(), BchError>
{
    let ctx = Ctx { fs, version, big_endian, jset: None, entry: Some(EntryPos { type_: head(e).type_, offset: 0 }), from };
    entry_validate_typed(&ctx, e)
}

pub fn journal_entry_to_text(out: &mut Printbuf, fs: Option<&Fs>, e: &c::jset_entry<Unvalidated>) {
    c::bch_jset_entry_type(e.type_.into()).to_text(out);

    if u32::from(e.type_) < c::bch_jset_entry_type::NR {
        prt!(out, ": ");
        if let Ok(r) = e.typed() {
            c::jset_entry_match!(r, a => a.to_text(out, fs))
        }
    }
}

fn jset_head(j: &[u8]) -> &Head<c::jset> {
    cast_plain(j).expect("a jset's header")
}

fn jset_head_mut(j: &mut [u8]) -> &mut Head<c::jset> {
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
        let t = rest.get(offset_of!(Head<c::jset_entry>, type_)).copied();
        let len = if rest.len() >= HEAD { HEAD + head(rest).u64s() * 8 } else { usize::MAX };

        ctx.from.journal_offset = (off / 8) as c_uint;
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
        jset_head_mut(j).set_val_u64s(off / 8);
    }
    Ok(())
}

fn dev_or_fs_name<'a>(fs: &'a Fs, ca: Option<&'a Dev>) -> &'a str {
    ca.map_or_else(|| fs.name(), Dev::name).to_str().unwrap_or("?")
}

/// Jset @j - its bytes, as long as its u64s says - checked, its entries
/// repaired in place.
pub(crate) fn jset_validate(fs: &Fs, ca: Option<&Dev>, j: &mut [u8], sector: u64, flags: ValidateFlags)
    -> Result<Jset, BchError>
{
    let h = jset_head(j);
    let (seq, version) = (h.seq.get(), h.version.get());

    if h.magic.get() != fs.jset_magic() {
        return Ok(Jset::None);
    }

    let ctx = Ctx::jset(fs, h, flags);
    let name = dev_or_fs_name(fs, ca);

    if journal_entry_err_on!(ctx, !crate::sb::io::version_compatible(version as u16), jset_unsupported_version,
                             "{} sector {} seq {}: incompatible journal entry version {}.{}",
                             name, sector, seq,
                             c::BCH_VERSION_MAJOR(version.into()), c::BCH_VERSION_MINOR(version.into())) {
        /* don't try to continue: */
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_journal_entry_version_incompatible);
    }

    journal_entry_err_on!(ctx, !crate::data::checksum::checksum_type_valid(fs, h.csum_type()), jset_unknown_csum,
                          "{} sector {} seq {}: journal entry with unknown csum type {}",
                          name, sector, seq, h.csum_type());

    /* last_seq is ignored when JSET_NO_FLUSH is true */
    if !h.no_flush() && jset_last_seq_check(fs, jset_head_mut(j), flags)? {
        return Ok(Jset::Bad);
    }

    jset_validate_entries(fs, j, flags)?;
    Ok(Jset::Ok)
}

pub(crate) fn jset_last_seq_check(fs: &Fs, h: &mut Head<c::jset>, flags: ValidateFlags) -> Result<bool, BchError> {
    let ctx = Ctx::jset(fs, h, flags);
    let seq = h.seq.get();

    if journal_entry_err_on!(ctx, h.last_seq.get() > seq, jset_last_seq_newer_than_seq,
                             "invalid journal entry: last_seq > seq ({} > {})", h.last_seq.get(), seq) {
        h.last_seq = crate::types::Le64::new(seq);
        return Ok(true);
    }
    Ok(false)
}

/// Jset @h's header, as read from a bucket with @bucket_sectors_left: cut to
/// fit, if it says it's longer.
pub(crate) fn jset_validate_early(fs: &Fs, ca: Option<&Dev>, h: &mut Head<c::jset>, sector: u64,
                                  bucket_sectors_left: u32) -> Result<Jset, BchError>
{
    let (seq, version) = (h.seq.get(), h.version.get());

    if h.magic.get() != fs.jset_magic() {
        return Ok(Jset::None);
    }

    let ctx = Ctx::jset(fs, h, ValidateFlags::empty());
    let name = dev_or_fs_name(fs, ca);

    if journal_entry_err_on!(ctx, !crate::sb::io::version_compatible(version as u16), jset_unsupported_version,
                             "{} sector {} seq {}: unknown journal entry version {}.{}",
                             name, sector, seq,
                             c::BCH_VERSION_MAJOR(version.into()), c::BCH_VERSION_MINOR(version.into())) {
        /* don't try to continue: */
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_journal_validate_version_incompatible);
    }

    let bytes = h.bytes();
    let max = (bucket_sectors_left as usize) << 9;

    if journal_entry_err_on!(ctx, bytes > max, jset_past_bucket_end,
                             "{} sector {} seq {}: journal entry too big ({} bytes)", name, sector, seq, bytes) {
        h.set_val_u64s(h.u64s() - (bytes - max) / 8);
    }
    Ok(Jset::Ok)
}

/// # Safety
/// @entry is an entry, as long as its u64s says.
#[c_export]
pub unsafe fn bch2_journal_entry_validate(c: &Opaque<Fs>, entry: *mut Head<c::jset_entry>, version: c_uint,
                                          big_endian: c_int, from: c::bkey_validate_context) -> c_int
{
    let bytes = unsafe { core::slice::from_raw_parts_mut(entry.cast::<u8>(), (*entry).bytes()) };
    ret_to_c(journal_entry_validate(Fs::from_c(c), bytes, version, big_endian != 0, from))
}
