// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_header_validate, bkey_val_validate, AllTypes, BkeyI, BkeyP, BkeySC, FamilyVal,
                         ValidateFlags, BKEY_U64S};
use crate::btree::{compat::__bch2_bkey_compat, node::{BtreeNode, NodeContents}, read::BsetHolder};
use crate::c::{self, Errcode as e};
use crate::data::ptrs::BkeyVRead;
use crate::errcode::BchError;
use crate::format::{BkeyType, Btree};
use crate::fs::{Dev, Fs};
use crate::init::{error::{id, BkeyValidate}, passes::RecoveryPass};
use crate::sb::{errors::{FsckFlags, SbErrorId}, io::version_compatible};
use crate::types::Le16;
use crate::util::vstructs::{Head, Unvalidated, VStructError};
use crate::util::{log::LogMsg, Printbuf, ToText};
use crate::{bkey_fsck_err_on, prt};
use core::cmp::Ordering;

/// Which way a node is going: on read, the device it's read from, what's
/// failed reading it so far - nothing to record, scanning for btree nodes -
/// and what the read reports.
pub enum NodeIo<'a> {
    Read {
        ca:      &'a Dev,
        failed:  Option<&'a mut c::bch_io_failures>,
        err_msg: &'a mut Printbuf,
    },
    Write,
}

pub struct NodeValidate<'a> {
    pub fs: &'a Fs,
    pub b:  &'a BtreeNode,
    pub io: NodeIo<'a>,
}

/// Where in the node an error is, for its message: a bset, by its u64s, and a
/// key in it, by its byte offset from the start of the bset's block.
#[derive(Clone, Copy)]
pub struct ErrAt {
    bset_u64s: Option<usize>,
    key:       Option<usize>,
}

impl ErrAt {
    pub const NODE: Self = Self { bset_u64s: None, key: None };

    pub fn bset(i: &Head<c::bset>) -> Self {
        Self { bset_u64s: Some(i.u64s()), key: None }
    }

    /// The key at byte @at of what holds @i.
    pub fn key(i: &Head<c::bset>, at: usize) -> Self {
        Self { bset_u64s: Some(i.u64s()), key: Some(at) }
    }
}

fn ptr_sectors_written(k: BkeySC<'_>) -> u32 {
    k.arm::<c::BtreePtrV2>().map_or(0, |v2| v2.v().sectors_written.get().into())
}

fn ptr_range_updated(k: &BkeyI) -> bool {
    BkeySC::from(k).arm::<c::BtreePtrV2>().is_some_and(|v2| v2.v().btree_ptr_range_updated())
}

fn err_msg(out: &mut Printbuf, ca: Option<&Dev>, b: &BtreeNode, at: ErrAt, msg: &dyn ToText) {
    if let Some(ca) = ca {
        prt!(out, "{ca} ");
    }

    prt!(out, "node offset {}/{}", b.written.get(),
         ptr_sectors_written(unsafe { b.contents() }.key_i().into()));
    if let Some(u64s) = at.bset_u64s {
        prt!(out, " bset u64s {u64s}");
    }
    if let Some(k) = at.key {
        prt!(out, " bset byte offset {k}");
    }
    prt!(out, ": ");

    msg.to_text(out);
    out.newline();
}

impl NodeValidate<'_> {
    pub fn writing(&self) -> bool {
        matches!(self.io, NodeIo::Write)
    }

    /*
     * Damage we were able to work around while parsing: the in-memory node is
     * usable, but what's on disk isn't, so the repair only means something once
     * it's been written back.
     */
    fn degraded(&self) {
        self.b.flags.set_need_rewrite(true);
        self.b.flags.set_need_rewrite_error(true);
        let _ = self.fs.err(e::BCH_ERR_fsck_fix);
    }

    pub fn err(&mut self, flags: FsckFlags, at: ErrAt, err: SbErrorId, msg: &dyn ToText) -> Result<bool, BchError> {
        let (fs, b) = (self.fs, self.b);

        if fs.recovery.runner.current_pass.get() == RecoveryPass::scan_for_btree_nodes {
            return if flags.contains(FsckFlags::CAN_FIX) {
                let _ = fs.err(e::BCH_ERR_fsck_fix);
                Ok(true)
            } else {
                Err(fs.err(e::BCH_ERR_btree_node_validate_err))
            };
        }

        fs.sb_error_count(err);

        match &mut self.io {
            NodeIo::Read { ca, failed, err_msg: out } => {
                err_msg(out, Some(ca), b, at, msg);

                /* not over keys_lost, from earlier in this replica: */
                if let Some(f) = failed.as_deref_mut() {
                    if !f.dev_err(ca.dev_idx.into()).is_some_and(|err| err.matches(e::BCH_ERR_btree_node_validate_err)) {
                        f.set_errcode(ca.dev_idx.into(), -fs.err(e::BCH_ERR_btree_node_validate_err).raw());
                    }
                }

                let have_retry = matches!(BkeySC::from(unsafe { b.contents() }.key_i())
                                              .pick_read_device(fs, failed.as_deref(), 0, c::ReadFlags::empty()),
                                          Ok(Some(_)));

                /*
                 * Reading is not repairing, so fix_errors has no say here: it
                 * decides whether a repair is persisted, not whether a node can
                 * be read. Consulting it meant a node that parses fine was
                 * reported unreadable whenever the run wasn't fixing errors -
                 * which is every ordinary mount, since fix_errors defaults to
                 * exit and bset_bad_csum isn't autofix. Topology repair then
                 * dropped a node that was sitting right there, leaving a gap
                 * that btree node scan filled with the same node, forever.
                 *
                 * The write is gated where it belongs: async_btree_op()
                 * only queues a rewrite once the fs can take a write ref.
                 */
                if !have_retry && flags.contains(FsckFlags::CAN_FIX) {
                    self.degraded();
                    Ok(true)
                } else {
                    Err(fs.err(e::BCH_ERR_btree_node_validate_err))
                }
            }
            NodeIo::Write => Err(self.write_err(at, msg)),
        }
    }

    fn write_err(&self, at: ErrAt, msg: &dyn ToText) -> BchError {
        let (fs, b) = (self.fs, self.b);
        let mut m = LogMsg::new(fs);

        prt!(&mut m, "corrupt btree node before write at btree {}\n", b.pos_to_text(fs));
        err_msg(&mut m, None, b, at, msg);

        fs.emergency_read_only(&mut m);

        fs.err(e::BCH_ERR_fsck_errors_not_fixed)
    }

    /*
     * The replica being read lost keys - dropped as invalid, a bset truncated or
     * missing: recorded against its device, as its error, so that if it's the
     * replica the read ends up using, the btree is flagged as having lost data.
     * No @failed when scanning for btree nodes.
     */
    pub fn keys_lost(&mut self) {
        let fs = self.fs;
        if let NodeIo::Read { ca, failed: Some(f), .. } = &mut self.io {
            f.set_errcode(ca.dev_idx.into(), -fs.err(e::BCH_ERR_btree_node_validate_err_keys_lost).raw());
        }
    }

    /*
     * A bset of the replica being read failed its checksum, and was kept: if it's
     * the replica the read ends up using, nothing vouches for what it holds, and
     * the read says so rather than reporting a repair. Lost keys say more.
     */
    pub fn csum_bad(&mut self) {
        let fs = self.fs;
        if let NodeIo::Read { ca, failed, .. } = &mut self.io {
            ca.io_error(c::MemberErrorType::checksum);

            if let Some(f) = failed.as_deref_mut() {
                if !f.dev_err(ca.dev_idx.into()).is_some_and(|err| err.matches(e::BCH_ERR_btree_node_validate_err_keys_lost)) {
                    f.set_errcode(ca.dev_idx.into(), -fs.err(e::BCH_ERR_btree_node_validate_err_csum).raw());
                }
            }
        }
    }
}

macro_rules! btree_err {
    ($v:expr, $flags:expr, $at:expr, $err:expr, $($msg:tt)*) => {
        $v.err($flags, $at, $err, $crate::message!($($msg)*))
    };
}

macro_rules! btree_err_on {
    ($v:expr, $cond:expr, $flags:expr, $at:expr, $err:expr, $($msg:tt)*) => {
        if $crate::util::kernel::unlikely($cond) {
            $crate::btree::validate::btree_err!($v, $flags, $at, $err, $($msg)*)
        } else {
            Ok(false)
        }
    };
}
pub(crate) use {btree_err, btree_err_on};

/// @v's key inside @b's range.
pub fn bkey_in_btree_node(v: &BkeyValidate<'_, '_>, b: &BtreeNode) -> Result<(), BchError> {
    let data = unsafe { b.contents() }.data();

    bkey_fsck_err_on!(v, v.k.k.p < data.min_key, id::bkey_before_start_of_btree_node,
                      "key before start of btree node")?;
    bkey_fsck_err_on!(v, v.k.k.p > data.max_key, id::bkey_after_end_of_btree_node,
                      "key past end of btree node")
}

/* A node header as a machine of @big_endian wrote it, made this one's, or back */
fn compat_btree_node(big_endian: bool, bn: &mut c::btree_node<Unvalidated>) {
    if big_endian != cfg!(target_endian = "big") {
        bn.min_key.swab();
        bn.max_key.swab();
    }
}

pub(crate) fn node_btree(bn: &Head<c::btree_node>) -> u32 {
    (bn.id_lo() | bn.id_hi() << 4) as u32
}

pub fn validate_bset<H: BsetHolder + ?Sized>(v: &mut NodeValidate<'_>, n: &mut H, offset: u32) -> Result<(), BchError> {
    let (fs, b) = (v.fs, v.b);
    let contents = unsafe { b.contents() };
    let write = v.writing();
    let i = n.bset_mut();
    let version = i.version.get();

    btree_err_on!(v, !version_compatible(version),
                  FsckFlags::empty(),
                  ErrAt::bset(i),
                  id::btree_node_unsupported_version,
                  "unsupported bset version {}.{}",
                  c::BCH_VERSION_MAJOR(version.into()),
                  c::BCH_VERSION_MINOR(version.into()))?;

    if fs.recovery.runner.current_pass.get() != RecoveryPass::scan_for_btree_nodes &&
       btree_err_on!(v, version < fs.sb.version_min.get(),
                     FsckFlags::CAN_FIX,
                     ErrAt::bset(i),
                     id::btree_node_bset_older_than_sb_min,
                     "bset version {} older than superblock version_min {}",
                     version, fs.sb.version_min.get())? {
        if version_compatible(version) {
            let mut sb = fs.sb_lock();
            sb.sb_mut().version_min = Le16::new(version);
            fs.write_super(&mut sb);
        } else {
            /* We have no idea what's going on: */
            i.version = Le16::new(fs.sb.version.get());
        }
    }

    if btree_err_on!(v, c::BCH_VERSION_MAJOR(version.into()) >
                        c::BCH_VERSION_MAJOR(fs.sb.version.get().into()),
                     FsckFlags::CAN_FIX,
                     ErrAt::bset(i),
                     id::btree_node_bset_newer_than_sb,
                     "bset version {} newer than superblock version {}",
                     version, fs.sb.version.get())? {
        let mut sb = fs.sb_lock();
        sb.sb_mut().version = Le16::new(version);
        fs.write_super(&mut sb);
    }

    btree_err_on!(v, i.separate_whiteouts(),
                  FsckFlags::empty(),
                  ErrAt::bset(i),
                  id::btree_node_unsupported_version,
                  "BSET_SEPARATE_WHITEOUTS no longer supported")?;

    btree_err_on!(v, offset != 0 && i.u64s() == 0,
                  FsckFlags::CAN_FIX,
                  ErrAt::bset(i),
                  id::bset_empty,
                  "empty bset")?;

    let bset_offset = i.offset() as u32;
    btree_err_on!(v, bset_offset != 0 && bset_offset != offset,
                  FsckFlags::CAN_FIX,
                  ErrAt::bset(i),
                  id::bset_wrong_sector_offset,
                  "bset at wrong sector offset")?;

    if let Some(bn) = n.node() {
        let big_endian = bn.keys.big_endian();
        let v2 = BkeySC::from(contents.key_i()).arm::<c::BtreePtrV2>();

        /* These indicate that we read the wrong btree node: */

        if let Some(bp) = &v2 {
            /* XXX endianness */
            btree_err_on!(v, bp.v().seq != bn.keys.seq,
                          FsckFlags::empty(),
                          ErrAt::NODE,
                          id::bset_bad_seq,
                          "incorrect sequence number (wrong btree node)")?;
        }

        btree_err_on!(v, node_btree(bn) != b.c.btree_id.into(),
                      FsckFlags::empty(),
                      ErrAt::bset(&bn.keys),
                      id::btree_node_bad_btree,
                      "incorrect btree id")?;

        btree_err_on!(v, bn.level() as u32 != b.c.level.into(),
                      FsckFlags::empty(),
                      ErrAt::bset(&bn.keys),
                      id::btree_node_bad_level,
                      "incorrect level")?;

        if !write {
            compat_btree_node(big_endian, bn);
        }

        if let Some(bp) = &v2 {
            let bp = bp.v();

            if bp.btree_ptr_range_updated() {
                if write {
                    return Err(v.write_err(ErrAt::NODE,
                        crate::message!("pointer has range updated, but the node has never been written")));
                }

                bn.min_key = bp.min_key;
                bn.max_key = contents.key.k.p;
            }

            btree_err_on!(v, bn.min_key != bp.min_key,
                          FsckFlags::empty(),
                          ErrAt::NODE,
                          id::btree_node_bad_min_key,
                          "incorrect min_key: got {} should be {}",
                          bn.min_key, bp.min_key)?;
        }

        btree_err_on!(v, bn.max_key != contents.key.k.p,
                      FsckFlags::empty(),
                      ErrAt::bset(&bn.keys),
                      id::btree_node_bad_max_key,
                      "incorrect max key {}",
                      bn.max_key)?;

        if write {
            compat_btree_node(big_endian, bn);
        }

        let mut buf1 = Printbuf::new();
        btree_err_on!(v, bn.format.validate(&mut buf1).is_err(),
                      FsckFlags::empty(),
                      ErrAt::bset(&bn.keys),
                      id::btree_node_bad_format,
                      "invalid bkey format: {}\n{}", buf1.as_str(), bn.format)?;
    }

    Ok(())
}

pub(crate) fn validate_from(b: &BtreeNode, flags: ValidateFlags) -> c::bkey_validate_context {
    let mut from = c::bkey_validate_context {
        level: b.c.level,
        btree: Btree(b.c.btree_id.into()),
        ..Default::default()
    };
    from.bits.set_from(c::BkeyValidateFrom::btree_node);
    from.bits.set_flags(flags);
    from
}

pub fn btree_node_bkey_val_validate(fs: &Fs, b: &BtreeNode, k: BkeySC<'_>,
                                    flags: ValidateFlags) -> Result<(), BchError> {
    bkey_val_validate(&BkeyValidate { fs, k, from: &validate_from(b, flags) })
}

fn bset_key_validate(fs: &Fs, b: &BtreeNode, k: BkeySC<'_>, updated_range: bool,
                     from: &c::bkey_validate_context) -> Result<(), BchError> {
    let v = BkeyValidate { fs, k, from };

    bkey_header_validate(&v)?;
    if !updated_range {
        bkey_in_btree_node(&v, b)?;
    }
    if from.bits.flags().contains(ValidateFlags::write) {
        bkey_val_validate(&v)?;
    }
    Ok(())
}

fn bkey_packed_valid(fs: &Fs, b: &BtreeNode, contents: &NodeContents, i: &c::bset<Unvalidated>,
                     at: usize) -> bool {
    let Some(Ok(k)) = i.key_at(at) else { return false };
    let k = k.key();

    if k.format() as u32 > c::KEY_FORMAT_CURRENT {
        return false;
    }

    if !k.u64s_valid(&contents.format) {
        return false;
    }

    let mut tmp = c::bkey::default();
    let u = k.cast::<FamilyVal<AllTypes>>().disassemble(contents, &mut tmp);
    bkey_header_validate(&BkeyValidate { fs, k: u, from: &validate_from(b, ValidateFlags::silent) }).is_ok()
}

fn btree_node_read_bkey_cmp(contents: &NodeContents, l: &BkeyP, r: &BkeyP) -> Ordering {
    let deleted = |k: &BkeyP| (k.key_type() == BkeyType::deleted) as i32;

    contents.cmp_packed_inlined(l, r)
        .then((deleted(r) - deleted(l)).cmp(&0))
}

fn bkey_compat(v: &NodeValidate<'_>, version: u16, big_endian: bool, k: &mut BkeyP<FamilyVal<AllTypes, Unvalidated>>) {
    let b = v.b;

    if version < c::bcachefs_metadata_version_current as u16 ||
       big_endian != cfg!(target_endian = "big") ||
       cfg!(CONFIG_BCACHEFS_DEBUG) {
        unsafe {
            __bch2_bkey_compat(Some(v.fs), b.c.level.into(), Btree(b.c.btree_id.into()), version.into(),
                               big_endian.into(), v.writing().into(), Some(&b.contents().format),
                               k.as_ffi_mut())
        }
    }
}

struct BsetKeys<'a> {
    contents:      &'a NodeContents,
    keys_at:       usize,
    version:       u16,
    big_endian:    bool,
    updated_range: bool,
    from:          c::bkey_validate_context,
}

enum Key {
    Keep(usize),
    Drop(usize),
}

/// The key at byte @at of @i checked, made this machine's - or the disk's, to
/// write - and against the key before it, at @prev.
fn bset_key_check(v: &mut NodeValidate<'_>, ks: &BsetKeys<'_>, i: &mut c::bset<Unvalidated>,
                  at: usize, prev: Option<usize>) -> Result<Key, BchError> {
    let (fs, b, contents) = (v.fs, v.b, ks.contents);
    let h = i.key_header(at).expect("a key's first word, before the end of its bset");
    let (u64s, format, key_u64s) = (h.u64s(), h.format(), h.key_u64s(&contents.format));
    let here = ErrAt::key(i, ks.keys_at + at);

    if btree_err_on!(v, format as u32 > c::KEY_FORMAT_CURRENT,
                     FsckFlags::CAN_FIX,
                     here,
                     id::btree_node_bkey_bad_format,
                     "invalid bkey format {}", format)? {
        return Ok(Key::Drop(u64s));
    }

    if btree_err_on!(v, !h.u64s_valid(&contents.format),
                     FsckFlags::CAN_FIX,
                     here,
                     id::btree_node_bkey_bad_u64s,
                     "bad k->u64s {} (min {} max {})", u64s,
                     key_u64s,
                     u8::MAX as usize - BKEY_U64S + key_u64s)? {
        return Ok(Key::Drop(u64s));
    }

    let k = i.key_at_mut(at)
        .expect("a key, before the end of its bset")
        .expect("a key, its u64s checked against the bset's end")
        .into_key_mut();

    if !v.writing() {
        bkey_compat(v, ks.version, ks.big_endian, k);
    }

    let mut tmp = c::bkey::default();
    let u = k.cast::<FamilyVal<AllTypes>>().disassemble(contents, &mut tmp);

    match bset_key_validate(fs, b, u, ks.updated_range, &ks.from) {
        Err(err) if err == BchError::from(e::BCH_ERR_fsck_delete_bkey) => return Ok(Key::Drop(u64s)),
        r => r?,
    }

    if v.writing() {
        bkey_compat(v, ks.version, ks.big_endian, k);
    }

    if let Some(p) = prev {
        let key = |at| i.key_at(at).and_then(Result::ok).expect("a key validated already").key()
            .cast::<FamilyVal<AllTypes>>();
        let (pk, kk) = (key(p), key(at));

        if btree_node_read_bkey_cmp(contents, pk, kk) != Ordering::Less {
            let (up, uk) = (pk.unpack(contents), kk.unpack(contents));

            btree_err!(v, FsckFlags::CAN_FIX,
                       here,
                       id::btree_node_bkey_out_of_order,
                       "keys out of order: {} > {}", up, uk)?;
            return Ok(Key::Drop(u64s));
        }
    }

    Ok(Key::Keep(u64s))
}

/// The bad key at byte @at of @i dropped: its @u64s, or if they can't be
/// trusted, up to the next good key - or the rest of the bset.
fn bset_key_drop<'i>(v: &mut NodeValidate<'_>, ks: &BsetKeys<'_>, i: &'i mut c::bset<Unvalidated>,
                     at: usize, u64s: usize) -> &'i mut c::bset<Unvalidated> {
    let (fs, b) = (v.fs, v.b);
    let mut next_good_key = u64s;

    if next_good_key == 0 ||
       ks.big_endian == cfg!(target_endian = "big") {
        /*
         * only do scanning if bch2_bkey_compat() has nothing to
         * do
         */

        let keys: &c::bset<Unvalidated> = i;
        if !bkey_packed_valid(fs, b, ks.contents, keys, at + next_good_key * 8) {
            let rest = (keys.u64s() * 8 - at) / 8;

            next_good_key = (1..rest)
                .find(|&n| bkey_packed_valid(fs, b, ks.contents, keys, at + n * 8))
                /*
                 * didn't find a good key, have to truncate the rest of
                 * the bset
                 */
                .unwrap_or(rest);
        }
    }

    b.flags.set_need_rewrite(true);
    b.flags.set_need_rewrite_error(true);
    v.keys_lost();

    i.remove(at, next_good_key * 8)
}

pub fn validate_bset_keys<H: BsetHolder + ?Sized>(v: &mut NodeValidate<'_>, n: &mut H) -> Result<(), BchError> {
    let contents = unsafe { v.b.contents() };
    let mut i = n.bset_mut();
    let ks = BsetKeys {
        contents,
        keys_at:       H::KEYS,
        version:       i.version.get(),
        big_endian:    i.big_endian(),
        updated_range: ptr_range_updated(contents.key_i()),
        from:          validate_from(v.b, if v.writing() { ValidateFlags::write } else { ValidateFlags::empty() }),
    };
    let (mut at, mut prev) = (0, None);

    while let Some(walk) = i.key_at(at) {
        if btree_err_on!(v, matches!(walk, Err(VStructError::Overrun { .. })),
                         FsckFlags::CAN_FIX,
                         ErrAt::key(i, ks.keys_at + at),
                         id::btree_node_bkey_past_bset_end,
                         "key extends past end of bset")? {
            i.truncate(at);
            break;
        }

        match bset_key_check(v, &ks, i, at, prev)? {
            Key::Keep(u64s) => {
                prev = Some(at);
                at += u64s * 8;
            }
            Key::Drop(u64s) => i = bset_key_drop(v, &ks, i, at, u64s),
        }
    }

    Ok(())
}
