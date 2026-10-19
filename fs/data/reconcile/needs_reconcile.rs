// SPDX-License-Identifier: GPL-2.0

//! What a key's reconcile state should be, and setting it - trigger.c's
//! needs_reconcile family: the reconcile entry its pointers' work and the io
//! options call for, padding pointers for replicas it's short, a stripe's
//! needs_reconcile bit; and whether new work is allowed, or an fsck error.

use crate::fsck_err;
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{BkeyS, BkeySC, BkeyV};
use crate::btree::bkey_methods::TriggerOp;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeNodeIter, BtreeTrans, CommitFlags, TransAttempt, TransBkey,
                         UpdateTriggerFlags};
use crate::c;
use crate::data::ec::trigger::stripe_header_mut;
use crate::data::ptrs::{BkeyVPtrs, BkeyVPtrsMut, ExtentEntry, ExtentEntryMut, Ptrs, PtrsMut};
use crate::data::reconcile::trigger::{extent_reconcile_to_text, BkeyVReconcile};
use crate::errcode::{ret_to_c, BchError};
use crate::init::error::id;
use crate::util::printbuf::Msg;
use crate::util::rcu::read_lock;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use core::mem::{size_of, MaybeUninit};
use cstruct_macros::c_export;

/// Option @o's need_rb bit.
fn rb_bit(o: c::bch_reconcile_opts) -> u8 {
    1 << o.0
}

/// Whether a key should carry @r: while it has work - and a reflink_v while
/// it has any option its inode set, as it has no inode to take them from
/// after. bkey_should_have_rb_opts().
fn should_have_rb_opts(k: &c::bkey, r: &c::bch_extent_reconcile) -> bool {
    if k.key_type() == c::bch_bkey_type::KEY_TYPE_reflink_v {
        macro_rules! any_from_inode {
            ($(($name:tt)),* $(,)?) => { crate::paste! { false $(|| r.[<$name _from_inode>]() != 0)* } };
        }
        if c::BCH_RECONCILE_OPTS!(any_from_inode) {
            return true;
        }
    }
    r.need_rb() != 0
}

/// What a key's reconcile state should be: needs_reconcile()'s.
struct NeedsReconcile {
    r:            c::bch_extent_reconcile,
    /// Padding pointers to BCH_SB_MEMBER_INVALID to drop, or - negative - to
    /// add, standing in for replicas the key is short.
    invalid_devs: i32,
}

/// What @k's reconcile entry should be for options @opts - the work its
/// pointers need, and which are moving - and the padding pointers it should
/// gain or lose; None if it has them already: bch2_bkey_needs_reconcile().
/// A stripe goes by its needs_reconcile bit: data_replicas work, or none.
fn needs_reconcile(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, opts: &c::bch_inode_opts)
    -> Result<Option<NeedsReconcile>, BchError>
{
    use c::bch_reconcile_opts as o;
    let fs = t.fs();
    let btree = k.k.is_btree_ptr();

    if btree && fs.request_incompat_feature(c::bcachefs_metadata_version::reconcile).is_err() {
        return Ok(None);
    }

    let kp = k.arm::<Ptrs>().expect("direct data or a stripe: a key with pointers");
    let mut r = c::bch_extent_reconcile::from_io_opts(opts);
    let need = |r: &mut c::bch_extent_reconcile, opt| r.update_need_rb(|n| n | rb_bit(opt));

    let poisoned = kp.extent_flags(fs) & 1 << c::bch_extent_flags_e::BCH_EXTENT_FLAG_poisoned.0 != 0;
    let compression_type = c::bch_compression_opt::to_type(r.background_compression(), fs);
    let csum_type = if opts.nocow == 0 { fs.data_checksum_type_rb(&r) } else { c::bch_csum_type::BCH_CSUM_none };
    let stripe = k.k.key_type() == c::bch_bkey_type::KEY_TYPE_stripe;

    let (mut incompressible, mut unwritten, mut ec) = (false, false, false);
    let (mut invalid, mut ec_redundancy) = (0u32, 0u32);
    {
        let rcu = read_lock();
        for (i, d) in kp.ptrs_decoded(fs).enumerate() {
            let p = &d.p;
            let dev = u32::from(p.ptr.dev());

            incompressible |= p.crc.compression_type == c::bch_compression_type::BCH_COMPRESSION_TYPE_incompressible.0 as u8;
            unwritten      |= p.ptr.unwritten() != 0;

            // A stripe block counts for nothing on a device of durability 0 -
            // missing, evacuating, or set to 0: bch2_stripe_block_dev_bad().
            // An extent's pointer there is legitimate - a cache device - but a
            // stripe block there leaves every extent in the stripe short.
            let evacuating = if stripe {
                fs.dev_durability(dev, &rcu) == 0
            } else {
                fs.dev_bad_or_evacuating(dev, &rcu) && !p.has_ec
            };

            if !poisoned && !btree && p.ptr.cached() == 0 {
                if p.crc.csum_type != csum_type.0 as u8 {
                    need(&mut r, o::BCH_RECONCILE_data_checksum);
                }
                if p.crc.compression_type != compression_type.0 as u8 {
                    need(&mut r, o::BCH_RECONCILE_background_compression);
                }
            }

            if !poisoned && !evacuating && p.ptr.cached() == 0 &&
                r.background_target() != 0 &&
                !fs.dev_in_target(dev, Target::decode(r.background_target().into()), &rcu) {
                need(&mut r, o::BCH_RECONCILE_background_target);
                if dev != c::BCH_SB_MEMBER_INVALID {
                    r.set_ptr_moving(i);
                }
            }

            if evacuating {
                need(&mut r, o::BCH_RECONCILE_data_replicas);
                r.set_hipri(1);
                if dev != c::BCH_SB_MEMBER_INVALID {
                    r.set_ptr_moving(i);
                }
            }

            if p.has_ec && r.erasure_code() != 0 {
                ec_redundancy = ec_redundancy.max(p.ec.redundancy().into());
            }
            ec |= p.has_ec;
            invalid += u32::from(dev == c::BCH_SB_MEMBER_INVALID);
        }
    }

    if let Some(s) = k.arm::<c::Stripe>() {
        // both ways: a device that stops evacuating leaves the bit set on
        // stripes that no longer need anything (#929)
        let needs = r.need_rb() & rb_bit(o::BCH_RECONCILE_data_replicas) != 0;
        return Ok(if needs != (s.v().needs_reconcile() != 0) {
            Some(NeedsReconcile { r, invalid_devs: 0 })
        } else {
            None
        });
    }

    if unwritten || incompressible {
        r.update_need_rb(|n| n & !rb_bit(o::BCH_RECONCILE_background_compression));
    }
    if unwritten {
        r.update_need_rb(|n| n & !rb_bit(o::BCH_RECONCILE_data_checksum));
    }

    // whole-key durability: a device shared between the extent's stripes
    // counted once, which a per-pointer sum can't see
    let dur = k.durability(t)?;
    let (durability, acct, min) = (u32::from(dur.total), u32::from(dur.acct), u32::from(dur.min_durability));
    let replicas = u32::from(r.data_replicas());

    if durability.max(ec_redundancy + 1) < replicas {
        need(&mut r, o::BCH_RECONCILE_data_replicas);
        r.set_hipri(1);
    }
    if durability >= replicas + min {
        need(&mut r, o::BCH_RECONCILE_data_replicas);
    }
    if !unwritten && (r.erasure_code() != 0) != ec {
        need(&mut r, o::BCH_RECONCILE_erasure_code);
    }

    // The EC contribution floored at what the stripe can repair: a removed
    // stripe device reads as durability_desired 0, dropping acct below
    // ec_redundancy + 1, but stripe repair restores the block - so no padding
    // for durability the stripe gets back on its own.
    let mut invalid_devs = (acct.max(ec_redundancy + 1) as i32 + invalid as i32 - replicas as i32)
        .min(invalid as i32);

    // more than one pointer to BCH_SB_MEMBER_INVALID is an incompat feature
    if invalid_devs < 0 && fs.request_incompat_feature(c::bcachefs_metadata_version::reconcile).is_err() {
        invalid_devs = 0;
    }

    let old = kp.reconcile_opts(fs);
    if let Some(old) = old.filter(|old| old.need_rb() & !r.need_rb() == 0) {
        r.set_pending(old.pending());
        if r.hipri() != 0 && old.hipri() == 0 {
            r.set_pending(0);
        }
    }

    let should_have = should_have_rb_opts(k.k, &r);
    let changed = invalid_devs != 0 || match old {
        Some(old) => !should_have || old.into_bits() != r.into_bits(),
        None      => should_have,
    };

    Ok((changed && fs.request_incompat_feature(c::bcachefs_metadata_version::sb_field_extent_type_u64s).is_ok())
        .then_some(NeedsReconcile { r, invalid_devs }))
}

/// Whether scan cookie @inum is set - remembered in @cache once it is: a set
/// cookie stays set while a walk runs, an unset one can be set any time.
/// check_reconcile_scan_cookie().
fn scan_cookie_set(t: &TransAttempt<'_, '_>, inum: u64, cache: Option<&mut bool>) -> Result<bool, BchError> {
    if cache.as_deref() == Some(&true) {
        return Ok(true);
    }
    // SAFETY: the attempt's transaction
    let ret = unsafe { c::bch2_reconcile_scan_cookie_is_set(t.raw(), inum) };
    t.result(ret)?;
    if let Some(cache) = cache {
        *cache = ret != 0;
    }
    Ok(ret != 0)
}

/// Whether a scan cookie is set for a device @k points to - @cache, the ones
/// found set, checked first: check_dev_reconcile_scan_cookie().
fn dev_scan_cookie_set(t: &TransAttempt<'_, '_>, k: &impl BkeyVPtrs, cache: Option<&mut c::bch_devs_mask>)
    -> Result<bool, BchError>
{
    let fs = t.fs();
    let devs = || k.ptrs(fs).map(|p| u32::from(p.dev())).filter(|&dev| dev != c::BCH_SB_MEMBER_INVALID);

    if cache.as_deref().is_some_and(|m| devs().any(|dev| m.d.test(dev as usize))) {
        return Ok(true);
    }
    for dev in devs() {
        if scan_cookie_set(t, (c::RECONCILE_SCAN_COOKIE_device + dev).into(), None)? {
            if let Some(m) = cache {
                m.d.set(dev as usize);
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether new work @new_need_rb - pointers that don't match the options -
/// is allowed: propagating new options, always; a foreground write, except
/// what it can't match yet (background compression and target, erasure
/// coding before a stripe exists, replicas lost to a device going away), or
/// if options changed since it took them; anything with a scan pending, as
/// that propagates options. Otherwise it's an fsck error: new_needs_rb_allowed().
///
/// Checking for scan cookies is cached in @s - only their being set, so a
/// cookie set meanwhile isn't missed.
#[allow(clippy::too_many_arguments)]
fn new_needs_rb_allowed(t: &TransAttempt<'_, '_>, s: Option<&mut c::per_snapshot_io_opts>, k: &impl BkeyVPtrs,
                        ctx: c::set_needs_reconcile_ctx, opt_change_cookie: u32,
                        old: Option<&c::bch_extent_reconcile>, new: &c::bch_extent_reconcile,
                        mut new_need_rb: u8) -> Result<(), BchError> {
    use c::bch_reconcile_opts as o;
    use c::set_needs_reconcile_ctx as x;
    let fs = t.fs();

    if ctx == x::SET_NEEDS_RECONCILE_opt_change || ctx == x::SET_NEEDS_RECONCILE_opt_change_indirect {
        return Ok(());
    }

    // foreground writes aren't erasure coded at first, and we may crash
    // before a stripe is made
    if new_need_rb & rb_bit(o::BCH_RECONCILE_erasure_code) != 0 &&
        !k.extent_entries(fs).any(|e| matches!(e, ExtentEntry::stripe_ptr(_))) {
        new_need_rb &= !rb_bit(o::BCH_RECONCILE_erasure_code);
    }

    if ctx == x::SET_NEEDS_RECONCILE_foreground {
        // XXX: a foreground write can end up degraded when a device is
        // yanked - retries in the write path would fix that, carefully
        new_need_rb &= !(rb_bit(o::BCH_RECONCILE_background_compression) |
                         rb_bit(o::BCH_RECONCILE_background_target) |
                         rb_bit(o::BCH_RECONCILE_data_replicas));
        // the caller took @opt_change_cookie with its options: if they've
        // changed since, the write may rightly not match - and a scan for
        // the new ones is triggered on its own
        if new_need_rb == 0 || opt_change_cookie != fs.opt_change_cookie {
            return Ok(());
        }
    }

    let (fs_cookie, inum_cookie, dev_cookie) = match s {
        Some(s) => (Some(&mut s.fs_scan_cookie), Some(&mut s.inum_scan_cookie), Some(&mut s.dev_cookie)),
        None    => (None, None, None),
    };

    // btree node pointers follow the metadata options, so theirs is the
    // metadata cookie
    if scan_cookie_set(t, c::RECONCILE_SCAN_COOKIE_fs.into(), fs_cookie)? ||
        if k.k().is_btree_ptr() {
            scan_cookie_set(t, c::RECONCILE_SCAN_COOKIE_metadata.into(), None)?
        } else {
            scan_cookie_set(t, k.k().p.inode, inum_cookie)?
        } {
        return Ok(());
    }

    if new_need_rb == rb_bit(o::BCH_RECONCILE_data_replicas) && dev_scan_cookie_set(t, k, dev_cookie)? {
        return Ok(());
    }

    let old = old.copied().unwrap_or(c::bch_extent_reconcile::from_bits(0));
    let diff = Msg(|out: &mut Printbuf| {
        macro_rules! diff {
            ($(($name:tt)),* $(,)?) => { crate::paste! { $(
                if new_need_rb & rb_bit(o::[<BCH_RECONCILE_ $name>]) != 0 {
                    write!(out, "\n{} {} != {}", stringify!($name), old.$name(), new.$name());
                }
            )* } };
        }
        c::BCH_RECONCILE_OPTS!(diff);
    });
    fsck_err!(t, id::extent_io_opts_not_set, "extent with incorrect/missing reconcile opts:\n{}\nnew reconcile : {}{}",
              k.as_sc().untyped().to_text(fs), Msg(|out: &mut Printbuf| extent_reconcile_to_text(out, Some(fs), new)), diff)?;
    Ok(())
}

impl TransBkey<'_, '_> {
    /// Its reconcile state brought in line with options @opts: a stripe's
    /// needs_reconcile bit; a data key's reconcile entry set, or dropped if
    /// it needs none, and its padding pointers to BCH_SB_MEMBER_INVALID added
    /// or dropped. New work must be allowed - new_needs_rb_allowed().
    /// bch2_bkey_set_needs_reconcile(); its room is what it's grown into.
    pub fn set_needs_reconcile(&mut self, t: &TransAttempt<'_, '_>, s: Option<&mut c::per_snapshot_io_opts>,
                               opts: &c::bch_inode_opts, ctx: c::set_needs_reconcile_ctx,
                               opt_change_cookie: u32) -> Result<(), BchError> {
        let fs = t.fs();

        if !self.k().extent_is_direct_data() && self.k().key_type() != c::bch_bkey_type::KEY_TYPE_stripe {
            return Ok(());
        }

        let Some(NeedsReconcile { r: new, mut invalid_devs }) = needs_reconcile(t, self.as_sc(), opts)? else {
            return Ok(());
        };

        if self.k().key_type() == c::bch_bkey_type::KEY_TYPE_stripe {
            return set_needs_reconcile_stripe(t, s, self.reborrow(),
                new.need_rb() & rb_bit(c::bch_reconcile_opts::BCH_RECONCILE_data_replicas) != 0);
        }

        let mut k = self.reborrow().arm::<PtrsMut>().expect("direct data: a key whose entries can be edited");
        let old = k.reconcile_opts(fs).copied();
        let new_need_rb = new.need_rb() & !old.map_or(0, |old| old.need_rb());

        if new_need_rb != 0 {
            new_needs_rb_allowed(t, s, &k, ctx, opt_change_cookie, old.as_ref(), &new, new_need_rb)?;
        }

        let should_have = should_have_rb_opts(k.k(), &new);
        let r = k.extent_entries_mut_at(fs).find_map(|(at, e)| match e {
            ExtentEntryMut::reconcile(r) => Some((at, r)),
            _ => None,
        });
        match r {
            Some((_, r)) if should_have => *r = new,
            Some((at, _)) => k.drop_entry(fs, at),
            None if should_have => {
                let end = k.entries_end();
                k.insert_entry(end, &[new.into_bits()]).expect("room for a reconcile entry: its caller's");
            }
            None => {}
        }

        if invalid_devs > 0 {
            k.drop_ptrs_noerror(fs, |p| {
                let drop = p.p.ptr.dev() as u32 == c::BCH_SB_MEMBER_INVALID && !p.p.has_ec && invalid_devs > 0;
                invalid_devs -= i32::from(drop);
                drop
            });
        } else if invalid_devs < 0 {
            let n = invalid_devs.unsigned_abs();
            let sectors: u64 = if k.k().is_btree_ptr() { fs.btree_sectors().into() } else { k.k().size.into() };
            t.extra_disk_res_add(u64::from(n) * sectors, n);

            for _ in 0..n {
                let end = k.entries_end();
                let ptr = c::bch_extent_ptr::new().with_dev(c::BCH_SB_MEMBER_INVALID as u8);
                k.insert_entry(end, &[ptr.into_bits().0]).expect("room for padding pointers: its caller's");
            }
        }
        Ok(())
    }
}

/// A stripe's needs_reconcile bit made @needs - set where the stripe's
/// devices have no scan pending to explain it, an fsck error:
/// set_needs_reconcile_stripe().
fn set_needs_reconcile_stripe(t: &TransAttempt<'_, '_>, s: Option<&mut c::per_snapshot_io_opts>,
                              mut k: TransBkey<'_, '_>, needs: bool) -> Result<(), BchError> {
    {
        let stripe = k.reborrow().arm::<c::Stripe>().expect("a stripe");
        if needs && stripe.v().needs_reconcile() == 0 &&
            !dev_scan_cookie_set(t, &stripe, s.map(|s| &mut s.dev_cookie))? {
            fsck_err!(t, id::stripe_needs_reconcile_not_set, "stripe with needs_reconcile incorrectly unset\n{}",
                      k.as_sc().to_text(t.fs()))?;
        }
    }
    stripe_header_mut(&mut k).bits.set_needs_reconcile(needs.into());
    Ok(())
}

/// What set_needs_reconcile() can grow a key by: a reconcile entry, and a
/// padding pointer for each replica.
const SET_NEEDS_RECONCILE_ROOM: u32 = 1 + c::BCH_REPLICAS_MAX;

/// The extent trigger's: the reconcile entry filled in on the key going in,
/// so callers needn't - its room made here, as callers like extent split
/// fragments don't reserve it. Its io options are looked up each time:
/// bch2_extent_trigger_set_needs_reconcile().
fn extent_trigger_set_needs_reconcile(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    let mut opts = MaybeUninit::<c::bch_inode_opts>::uninit();
    // SAFETY: plain; @opts written on success
    t.result(unsafe {
        c::bch2_bkey_get_io_opts(t.raw(), core::ptr::null_mut(), BkeySC::from(&op.new).untyped().into(),
                                 opts.as_mut_ptr())
    })?;
    let opts = unsafe { opts.assume_init() };

    let needed = u32::from(op.new.k.u64s) + SET_NEEDS_RECONCILE_ROOM;
    op.new_with_room(t, needed)?
        .set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other, 0)
}

/// @k, at @iter, with its reconcile state brought in line with @opts, as an
/// update - or for a btree node pointer, @level the level it's at, the
/// pointer to the node below, updated in place: committed, and the
/// transaction restarted. Nothing if it's right already:
/// bch2_update_reconcile_opts().
pub fn update_reconcile_opts(t: &TransAttempt<'_, '_>, s: Option<&mut c::per_snapshot_io_opts>,
                             opts: &c::bch_inode_opts, iter: &mut BtreeIter<'_>, level: u32, k: BkeySC<'_>,
                             ctx: c::set_needs_reconcile_ctx) -> Result<(), BchError> {
    let flags = iter.iter_flags();
    assert!(!flags.contains(BtreeIterFlags::IS_EXTENTS), "update_reconcile_opts() on an extents iterator");
    assert!(!flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS),
            "update_reconcile_opts() on a snapshot filtering iterator");

    if !k.k.extent_is_direct_data() && k.k.key_type() != c::bch_bkey_type::KEY_TYPE_stripe {
        return Ok(());
    }
    if needs_reconcile(t, k, opts)?.is_none() {
        return Ok(());
    }

    if level == 0 {
        let room = u32::from(k.k.u64s) + SET_NEEDS_RECONCILE_ROOM;
        let mut n = t.bkey_reassemble_room(k, room as usize * size_of::<u64>())?;
        n.set_needs_reconcile(t, s, opts, ctx, 0)?;
        return t.update(iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE);
    }

    let mut node_iter = BtreeNodeIter::new(t, iter.btree(), iter.pos(), 0, level - 1, BtreeIterFlags::empty());
    let b = node_iter.peek_ref()?.expect("the node a btree node pointer points to");
    let mut n = t.bkey_reassemble_room(b.key_sc(), c::BKEY_BTREE_PTR_U64s_MAX as usize * size_of::<u64>())?;
    n.set_needs_reconcile(t, s, opts, ctx, 0)?;
    t.btree_node_update_key(node_iter.iter(), b, n, CommitFlags::NO_ENOSPC, false)?;
    Err(t.restart(c::bch_errcode::BCH_ERR_transaction_restart_commit))
}

// ── C's ──────────────────────────────────────────────────────────────────

/// # Safety
/// C has an attempt in progress on @trans, and @k is a key it can edit while
/// that runs, @buf_u64s u64s of it its.
#[c_export]
pub unsafe fn bch2_bkey_set_needs_reconcile(trans: &BtreeTrans<'_>, s: Option<&mut c::per_snapshot_io_opts>,
                                            opts: &c::bch_inode_opts, k: BkeyS<'_>, buf_u64s: c_uint,
                                            ctx: c::set_needs_reconcile_ctx, opt_change_cookie: u32) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    // SAFETY: the caller's - a struct bkey heads its struct bkey_i
    let mut k = unsafe { TransBkey::from_raw_room(&t, core::ptr::from_mut(k.k).cast(), buf_u64s) };
    ret_to_c(k.set_needs_reconcile(&t, s, opts, ctx, opt_change_cookie))
}

/// @op's @new and new_buf_u64s come back repointed, if the key was grown.
///
/// # Safety
/// @op is the update the extent trigger is running for, in its commit.
#[c_export]
pub unsafe fn bch2_extent_trigger_set_needs_reconcile(trans: &BtreeTrans<'_>, op: &mut c::btree_trigger_op) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    let mut top = unsafe { TriggerOp::from_raw(op) };
    let ret = extent_trigger_set_needs_reconcile(&t, &mut top);
    *op = top.to_raw();
    ret_to_c(ret)
}

/// # Safety
/// C has an attempt in progress on @trans, @iter one of its iterators.
#[c_export]
pub unsafe fn bch2_update_reconcile_opts(trans: &BtreeTrans<'_>, s: Option<&mut c::per_snapshot_io_opts>,
                                         opts: &c::bch_inode_opts, iter: &mut BtreeIter<'_>, level: c_uint,
                                         k: BkeySC<'_>, ctx: c::set_needs_reconcile_ctx) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(update_reconcile_opts(&t, s, opts, iter, level, k, ctx))
}
