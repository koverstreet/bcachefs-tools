// SPDX-License-Identifier: GPL-2.0

//! fsck's repairs of a key's pointers.
//!
//! check_fix_ptrs() is the check_repair of every key type with pointers: gc
//! runs it on each key before marking it, checking each pointer against the
//! gc bucket it points into - as gc has marked it so far - and the stripe it
//! claims. What can be repaired is: the bucket's, in gc's state; or the
//! pointer's, done to a copy of the key that replaces it. gc owns alloc info
//! and accounting while it runs, so the replacement is written without
//! triggers; gc marks it when it restarts. A key left with nothing to read
//! is an error key, or for a btree node pointer, its node lost - or if it's
//! cached, kept there until it can be rewritten, once we're rw.
//!
//! For check_backpointers, two keys claiming the same space: reading one's
//! replica to see whether it's the bad one, and dropping a device's pointer
//! from a key and writing it back - for a btree node pointer, an update of
//! the node's key, a rewrite from memory, or the pointer gone and its
//! btree's lost data flagged.
//!
//! A gc bucket's state is read and written under its lock; between a read and
//! the write it decides on there's the fsck question, so the two are atomic
//! only because gc's walk is single threaded today.

use crate::accounting::{bucket_data_type_mismatch, data_type_from_u8};
use crate::alloc::buckets::gen_cmp;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeySC, POS_MIN};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeNodeIter, BtreeNodeRef, BtreeTrans, CommitOpts,
                         TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::c::bch_data_type as dt;
use crate::c::bch_errcode;
use crate::data::ec::trigger::ptr_matches_stripe_m;
use crate::data::ptrs::{extent_nonce, ptr_data_type, BkeyVPtrs, BkeyVPtrsMut, DecodedPtr, Ptrs, PtrsMut};
use crate::errcode::{ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::inode_opts::get_io_opts;
use crate::opts::data_type_str;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::bio::{buf_nr_bvecs, Bio, BioBuf, BioOwner};
use crate::util::log::LogMsg;
use crate::util::rcu::read_lock;
use crate::util::vstructs::{cast_plain, Head, SelfSized};
use crate::util::Printbuf;
use crate::{bch_err, fsck_err, fsck_err_on, inode_fsck_err, prt};
use arrayvec::ArrayVec;
use bitflags::bitflags;
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use cstruct_macros::c_export;

/// What a key with pointers is copied with room for: as large as one can get,
/// for the reconcile entry and padding pointers set_needs_reconcile() may add.
const KEY_ROOM: usize = c::BKEY_EXTENT_U64s_MAX * size_of::<u64>();

/// Btree node pointer @n, at @level of @btree, with nothing left to read. A
/// node still cached is the only copy, for @cached to save - true; else the
/// node is lost: the pointer's deletion queued, and the btree flagged as
/// having lost data, for topology repair to fill the hole.
fn btree_ptr_unreadable<'a>(t: &TransAttempt<'a, '_>, btree: c::btree_id, level: u32, n: BkeySC<'_>,
                            cached: impl FnOnce(&mut BtreeIter<'a>, BtreeNodeRef) -> Result<(), BchError>)
    -> Result<bool, BchError>
{
    let fs = t.fs();

    let mut node_iter = BtreeNodeIter::new(t, btree, n.k.p, level + 1, level - 1, BtreeIterFlags::NOFILL);
    match node_iter.peek_ref() {
        Ok(b) => {
            cached(node_iter.iter(), b.expect("the node a btree node pointer points to"))?;
            return Ok(true);
        }
        Err(e) if e.matches(bch_errcode::BCH_ERR_no_btree_node_nofill) => {}
        Err(e) => return Err(e),
    }

    let mut del_iter = BtreeNodeIter::new(t, btree, n.k.p, 0, level, BtreeIterFlags::INTENT);
    t.iter_traverse(del_iter.iter())?;
    t.delete_at(del_iter.iter(), UpdateTriggerFlags::empty())?;

    let mut msg = LogMsg::new(fs);
    prt!(&mut msg, "btree node with no readable replicas, dropping pointer:\n{}", n.to_text(fs));
    // SAFETY: @n and @msg, borrowed for the calls
    ret_to_result_void(unsafe {
        c::bch2_damage_note_lost_extents(fs.as_ptr(), btree, n.into());
        c::bch2_btree_lost_data(fs.as_ptr(), msg.as_raw(), btree)
    })?;
    Ok(false)
}

/// @dev's pointers dropped from @k, a key at @level of @btree, and the key
/// written back. One left with nothing to read is, for a btree node pointer,
/// the node rewritten from memory if it's cached, or the pointer deleted and
/// the btree flagged as having lost data; for any other key, an error key of
/// type @error, the loss counted and recorded against its file.
pub fn bkey_drop_device_and_update(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u32, k: BkeySC<'_>,
                                   dev: u32, error: c::bch_key_type_errors) -> Result<(), BchError> {
    let fs = t.fs();
    let mut n = t.bkey_make_mut_noupdate(k)?;
    n.reborrow().arm::<Ptrs>().expect("a key with pointers").drop_device(fs, dev);

    if n.k().is_btree_ptr() {
        if n.as_sc().can_read(fs) {
            return t.btree_node_update_key_at_pos(btree, level, n);
        }
        // rw, so a cached node is written out now
        return btree_ptr_unreadable(t, btree, level, n.as_sc(), |iter, b| {
            t.btree_node_rewrite(iter, b, 0, CommitOpts::new(), c::bch_write_flags::empty())
        }).map(|_| ());
    }

    if !n.as_sc().can_read(fs) {
        n.set_error(fs, error);
        // SAFETY: our transaction
        ret_to_result_void(unsafe {
            c::bch2_damage_record_data_loss(t.raw(), btree, n.k().p, id::data_lost_double_allocation)
        })?;
    }

    let mut iter = BtreeNodeIter::new(t, btree, n.k().p, 0, level, BtreeIterFlags::INTENT);
    t.iter_traverse(iter.iter())?;
    t.update(iter.iter(), &n, UpdateTriggerFlags::empty())
}

/// Whether btree node @buf isn't the node @k points to, read from one of its
/// replicas: not a btree node, a bad checksum, or another seq. A node whose
/// first bset says it's longer than @buf is bad.
fn btree_node_bad(fs: &Fs, buf: &[u8], k: BkeySC<'_>) -> bool {
    let bn = cast_plain::<Head<c::btree_node>>(buf).expect("a btree node's worth of buffer");

    if bn.magic.get() != fs.bset_magic() {
        return true;
    }

    let ty = c::bch_csum_type(bn.keys.csum_type() as u32);
    if ty.valid(fs) {
        let end = bn.bytes();
        if end > buf.len() {
            return true;
        }
        let start = size_of::<c::bch_csum>();
        // SAFETY: @buf, from past the csum to its first bset's end
        let csum = unsafe {
            c::bch2_checksum(fs.as_ptr(), ty.0, bn.keys.nonce(0), buf[start..].as_ptr().cast(), end - start)
        };
        if csum != bn.csum {
            return true;
        }
    }

    k.arm::<c::BtreePtrV2>().is_some_and(|v2| bn.keys.seq.get() != v2.v().seq.get())
}

/// Whether @k's replica on @dev was bad, and dropped: read back, its checksum
/// wrong - or for a btree node, not the node @k points to. @other_k, in
/// @other_btree, is the key with a replica in the same space, for the report.
/// Unchecksummed data, and a device that can't be read, can't say: false.
pub fn kill_replica_if_checksum_bad(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u32, k: BkeySC<'_>,
                                    other_btree: c::btree_id, other_k: BkeySC<'_>, dev: u32)
    -> Result<bool, BchError>
{
    let fs = t.fs();
    let p = k.arm::<Ptrs>()
        .and_then(|kp| kp.ptrs_decoded(fs).find(|d| u32::from(d.p.ptr.dev()) == dev).map(|d| d.p))
        .expect("a pointer to the device whose replica it is");
    let is_btree_ptr = k.k.is_btree_ptr();

    if !is_btree_ptr && p.crc.csum_type == 0 {
        return Ok(false);
    }

    let Some(ca) = fs.dev_get_ioref(dev, c::bch_dev_read_ref::BCH_DEV_READ_REF_check_extent_checksums) else {
        return Ok(false);
    };

    let bytes = if is_btree_ptr {
        fs.opts.btree_node_size as usize
    } else {
        (p.crc.compressed_size as usize) << 9
    };
    let mut buf = KVVec::from_elem(0u8, bytes, GFP_KERNEL).map_err(|_| BchError::from(c::ENOMEM))?;

    let nr_vecs = buf_nr_bvecs(buf.as_ptr(), bytes).try_into().expect("bvecs for a btree node or an extent");
    let mut bio = BioBuf::<Bio>::new::<BchError, _>(nr_vecs, ca.disk_sb.bdev, c::req_op_REQ_OP_READ as u32,
                                                     |b| b.init())?;
    bio.set_sector(p.ptr.offset());
    bio.map_read(&mut buf).submit_wait()?;

    let bad = if is_btree_ptr {
        btree_node_bad(fs, &buf, k)
    } else {
        // SAFETY: @buf, @bytes long
        let csum = unsafe {
            c::bch2_checksum(fs.as_ptr(), p.crc.csum_type.into(), extent_nonce(k.k.bversion, &p.crc),
                             buf.as_ptr().cast(), bytes)
        };
        csum != p.crc.csum
    };
    if !bad {
        return Ok(false);
    }

    let mut msg = Printbuf::new();
    prt!(&mut msg, "duplicate extents pointing to same space on dev {dev}, \
                    checksum bad or wrong btree node - dropping:\n{btree} {}\n{other_btree} {}",
         k.to_text(fs), other_k.to_text(fs));

    // only an extents position names an inum; elsewhere report without recording
    let damage_pos = if btree == c::btree_id::BTREE_ID_extents { k.k.p } else { POS_MIN };

    if inode_fsck_err!(t, damage_pos, id::dup_backpointer_to_bad_csum_extent, "{}", msg)? {
        bkey_drop_device_and_update(t, btree, level, k, dev,
                                    c::bch_key_type_errors::KEY_TYPE_ERROR_double_allocation)?;
        return Ok(true);
    }
    Ok(false)
}

// ── gc's check of a key's pointers ───────────────────────────────────────

bitflags! {
    /// What check_fix_ptr() found a pointer needs.
    #[derive(Clone, Copy)]
    struct PtrFix: u8 {
        const DROP        = 1 << 0;
        const DROP_STRIPE = 1 << 1;
        const RESET_GEN   = 1 << 2;
    }
}

// repair_ptrs()'s masks: a bit per pointer
const _: () = assert!(c::BCH_BKEY_PTRS_MAX as u32 <= u32::BITS);

/// Pointer @d of @k checked against the bucket it points into, and what it
/// needs. A bucket the alloc btree didn't have takes the pointer's gen; one of
/// another data type is moved over if it was journal, loses the pointer if
/// it's a superblock's, and is otherwise multiple.
fn check_fix_ptr(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, d: &DecodedPtr<'_>) -> Result<PtrFix, BchError> {
    let fs = t.fs();
    let p = &d.p;
    let dev = u32::from(p.ptr.dev());
    if dev == c::BCH_SB_MEMBER_INVALID {
        return Ok(PtrFix::empty());
    }

    let marking = k.to_text(fs);
    let drop_if = |fix: bool| if fix { PtrFix::DROP } else { PtrFix::empty() };

    let Some(ca) = fs.dev_get(dev) else {
        let (err, what) = if fs.devs_removed.test(dev as usize) {
            (id::ptr_to_removed_device, "removed")
        } else {
            (id::ptr_to_invalid_device, "missing")
        };
        return Ok(drop_if(fsck_err!(t, err, "pointer to {what} device {dev}\nwhile marking {marking}")?));
    };

    let b = ca.ptr_bucket(&p.ptr);
    let Some(g) = ca.gc_bucket(b) else {
        return Ok(drop_if(fsck_err!(t, id::ptr_to_invalid_device,
                                    "pointer to invalid bucket on device {dev}\nwhile marking {marking}")?));
    };

    let cached = p.ptr.cached() != 0;
    let ptr_gen = p.ptr.generation();
    let data_type = ptr_data_type(fs, k, d);
    // what the messages name it, as C's ptr_data_type(): a stripe's are user data
    let ptr_type = data_type_str(if k.k.is_btree_ptr() {
        dt::BCH_DATA_btree
    } else if cached {
        dt::BCH_DATA_cached
    } else {
        dt::BCH_DATA_user
    });
    let mut fix = PtrFix::empty();

    let gen_valid = g.lock.lock().gen_valid() != 0;
    if fsck_err_on!(t, !gen_valid, id::ptr_to_missing_alloc_key,
                    "bucket {dev}:{b} data type {ptr_type} ptr gen {ptr_gen} missing in alloc btree\n\
                     while marking {marking}")? {
        if cached {
            return Ok(PtrFix::DROP);
        }

        let mut g = g.lock.lock();
        g.set_gen_valid(1);
        g.generation = ptr_gen;
    }

    let gen = g.lock.lock().generation;

    if fsck_err_on!(t, gen_cmp(ptr_gen, gen) > 0, id::ptr_gen_newer_than_bucket_gen,
                    "bucket {dev}:{b} data type {ptr_type} ptr gen in the future: {ptr_gen} > {gen}\n\
                     while marking {marking}")? {
        if cached {
            return Ok(PtrFix::DROP);
        }

        // XXX: if it's a data pointer, read it and see if it's good
        fix |= PtrFix::RESET_GEN;
    }

    if !cached {
        if fsck_err_on!(t, gen_cmp(ptr_gen, gen) < 0, id::stale_dirty_ptr,
                        "bucket {dev}:{b} data type {ptr_type} stale dirty ptr: {ptr_gen} < {gen}\n\
                         while marking {marking}")? {
            // XXX: if it's a data pointer, read it and see if it's good
            fix |= PtrFix::RESET_GEN;
        }
    } else if fsck_err_on!(t, i32::from(gen_cmp(gen, ptr_gen)) > c::BUCKET_GC_GEN_MAX as i32,
                           id::ptr_gen_newer_than_bucket_gen,
                           "bucket {dev}:{b} gen {gen} data type {ptr_type}: ptr gen {ptr_gen} too stale\n\
                            while marking {marking}")? {
        return Ok(PtrFix::DROP);
    }

    if data_type != dt::BCH_DATA_btree && ptr_gen != gen {
        return Ok(fix);
    }

    let bucket_type = data_type_from_u8(g.lock.lock().data_type());
    if fsck_err_on!(t, bucket_data_type_mismatch(bucket_type, data_type), id::ptr_bucket_data_type_mismatch,
                    "bucket {dev}:{b} gen {gen} different types of data in same bucket: {}, {}\n\
                     while marking {marking}", data_type_str(bucket_type), data_type_str(data_type))? {
        if bucket_type == dt::BCH_DATA_journal {
            // SAFETY: a device we have a ref on
            ret_to_result_void(unsafe { c::bch2_dev_journal_bucket_delete(ca.as_mut_ptr(), b) })?;

            // gc already counted the bucket as journal when it marked it:
            // move it over, as marking does
            t.gc_bucket_mod(&ca, g, UpdateTriggerFlags::GC, |a| {
                a.data_type      = data_type.0 as u8;
                a.stripe_sectors = 0;
                a.dirty_sectors  = 0;
                a.cached_sectors = 0;
                Ok(())
            })?;
            return Ok(fix);
        }

        if !cached && data_type == dt::BCH_DATA_btree && bucket_type == dt::BCH_DATA_sb {
            bch_err!(fs, "btree and superblock in the same bucket - cannot repair");
            return fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented);
        }

        // Data in a superblock bucket: every superblock write lands on it, so
        // it has been or will be overwritten - drop the pointer. If it was the
        // last readable one, repair_ptrs() records the loss. Seen after a
        // resize freed the bucket holding a backup superblock (#785).
        if bucket_type == dt::BCH_DATA_sb {
            return Ok(fix | PtrFix::DROP);
        }

        g.lock.lock().set_data_type(dt::BCH_DATA_multiple.0 as u8);
    }

    if p.has_ec {
        let idx = p.ec.idx();
        let (alive, matches) = fs.ec.gc_stripe(idx).map_or((false, false), |m| {
            let m = m.lock.lock();
            let alive = m.alive() != 0;
            (alive, alive && ptr_matches_stripe_m(&m, p))
        });

        if fsck_err_on!(t, !alive, id::ptr_to_missing_stripe,
                        "pointer to nonexistent stripe {idx}\nwhile marking {marking}")? ||
           fsck_err_on!(t, alive && !matches, id::ptr_to_incorrect_stripe,
                        "pointer does not match stripe {idx}\nwhile marking {marking}")? {
            fix |= PtrFix::DROP_STRIPE;
        }
    }

    Ok(fix)
}

/// @k, a key in @btree with nothing to read, rewritten without its triggers -
/// no_valid_pointers_fix() - and returned, to check in @k's place.
fn no_valid_pointers_repair<'a, 't>(t: &TransAttempt<'a, 't>, btree: c::btree_id, k: BkeySC<'_>)
    -> Result<TransBkey<'a, 't>, BchError>
{
    let mut new = t.bkey_reassemble_room(k, KEY_ROOM)?;
    no_valid_pointers_fix(t, btree, &mut new)?;

    let mut iter = BtreeNodeIter::new(t, btree, new.k().p, 0, 0,
                                      BtreeIterFlags::INTENT | BtreeIterFlags::ALL_SNAPSHOTS);
    t.iter_traverse(iter.iter())?;
    t.update(iter.iter(), &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | UpdateTriggerFlags::NORUN)?;
    Ok(new)
}

/// @new, a key in @btree with nothing to read, repaired in place: a cached
/// pointer that's still good made dirty, or if there's none, @new made an
/// error key - the loss reported, and recorded against its file.
fn no_valid_pointers_fix(t: &TransAttempt<'_, '_>, btree: c::btree_id, new: &mut TransBkey<'_, '_>)
    -> Result<(), BchError>
{
    let fs = t.fs();

    // We can only flip a pointer from cached -> dirty without contortions
    // here, when we're also repairing alloc info - to do this at runtime we'd
    // have to pin the bucket with an open_bucket
    let found_good_cached_ptr = {
        let rcu = read_lock();
        let mut kp = new.reborrow().arm::<Ptrs>().expect("a key with pointers");
        let mut found = false;
        for ptr in kp.ptrs_mut(fs) {
            if ptr.cached() != 0 &&
               fs.dev_rcu(ptr.dev().into(), &rcu).is_some_and(|ca| ca.ptr_stale(ptr, &rcu) == Some(0)) {
                ptr.set_cached(0);
                found = true;
            }
        }
        found
    };

    // Inode-scoped when we can attribute it: an extents btree position names
    // an inum, so the error records damage against the file. An indirect
    // extent's doesn't - POS_MIN reports without recording
    let damage_pos = if btree == c::btree_id::BTREE_ID_extents { new.k().p } else { POS_MIN };

    if found_good_cached_ptr {
        inode_fsck_err!(t, damage_pos, id::extent_ptrs_all_invalid_but_cached,
                        "extent without valid dirty pointers\n{}", new.as_sc().to_text(fs))?;

        let opts = get_io_opts(t, None, new.as_sc())?;
        new.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_opt_change, 0)?;
    } else {
        inode_fsck_err!(t, damage_pos, id::extent_ptrs_all_invalid,
                        "extent without valid pointers\n{}", new.as_sc().to_text(fs))?;

        new.set_error(fs, c::bch_key_type_errors::KEY_TYPE_ERROR_no_valid_pointers_repair);
    }
    Ok(())
}

fn check_fix_ptrs(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, btree: c::btree_id, level: u32, k: BkeySC<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();

    // We don't yet do btree key updates correctly for when we're RW
    assert!(!fs.flag(c::bch_fs_flags::BCH_FS_rw), "checking a key's pointers with the filesystem rw");

    if !k.k.is_btree_ptr() && !k.can_read(fs) {
        let repaired = no_valid_pointers_repair(t, btree, k)?;
        return repair_ptrs(t, iter, btree, level, repaired.as_sc());
    }
    repair_ptrs(t, iter, btree, level, k)
}

/// check_fix_ptrs(), past a key with nothing to read - which may now be an
/// error key, with no pointers: each pointer checked, and @k replaced by a
/// copy with the fixes they need. A btree node pointer's replacement is an
/// update of the node's key, then a restart. Its own function, not a rebinding
/// of @k: BkeySC is invariant in its lifetime, so the repair's and @k's differ.
fn repair_ptrs(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, btree: c::btree_id, level: u32, k: BkeySC<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let Some(kp) = k.arm::<Ptrs>() else {
        return Ok(());
    };

    let fixes = kp.ptrs_decoded(fs)
        .map(|d| check_fix_ptr(t, k, &d))
        .collect::<Result<ArrayVec<PtrFix, { c::BCH_BKEY_PTRS_MAX as usize }>, _>>()?;
    if fixes.iter().all(|f| f.is_empty()) {
        return Ok(());
    }
    // the pointers that need @fix, by index among the pointers
    let mask = |fix: PtrFix| fixes.iter().enumerate()
        .filter(|(_, f)| f.contains(fix))
        .fold(0u32, |m, (i, _)| m | 1 << i);

    let mut new = t.bkey_reassemble_room(k, KEY_ROOM)?;

    {
        let rcu = read_lock();
        let mut kp = new.reborrow().arm::<Ptrs>().expect("a key with pointers");
        for (ptr, _) in kp.ptrs_mut(fs).zip(&fixes).filter(|(_, f)| f.contains(PtrFix::RESET_GEN)) {
            let ca = fs.dev_rcu(ptr.dev().into(), &rcu).expect("the device the pointer was checked against");
            let g = ca.gc_bucket(ca.ptr_bucket(ptr)).expect("the gc bucket the pointer was checked against");
            ptr.set_generation(g.lock.lock().generation);
        }
    }

    let drop_stripe = mask(PtrFix::DROP_STRIPE);
    if drop_stripe != 0 {
        new.reborrow().arm::<PtrsMut>().expect("a key whose pointers can have stripes")
            .drop_ec_mask(fs, drop_stripe);
    }

    let drop = mask(PtrFix::DROP);
    if drop != 0 {
        new.reborrow().arm::<Ptrs>().expect("a key with pointers").drop_ptrs_mask(fs, drop);
    }

    // The drops can leave nothing to read - and no pointers at all is no
    // valid key. An extent is repaired as one found that way would be. A
    // btree node pointer's node is lost unless it's cached: then it's pinned
    // there, its only copy, and rewritten once we're rw - a rewrite
    // allocates, which gc, before journal replay, can't
    if !new.as_sc().can_read(fs) {
        if !new.k().is_btree_ptr() {
            no_valid_pointers_fix(t, btree, &mut new)?;
        } else if btree_ptr_unreadable(t, btree, level, new.as_sc(), |_, b| {
            // SAFETY: a cached node, held by the iterator for the calls
            unsafe {
                c::bch2_node_pin(fs.as_ptr(), b.as_ptr());
                c::bch2_async_btree_op(fs.as_ptr(), b.as_ptr(), c::async_btree_op::ASYNC_BTREE_rewrite);
            }
            Ok(())
        })? {
            let mut msg = LogMsg::new(fs);
            prt!(&mut msg, "btree node with no readable replicas kept in cache, to be rewritten once rw:\n{}",
                 new.as_sc().to_text(fs));
        } else {
            // gc commits the pointer's queued deletion, then restarts
            return Ok(());
        }
    }

    let opts = get_io_opts(t, None, k)?;
    new.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_opt_change, 0)?;

    // A btree node pointer kept for its cached node, with no pointer left,
    // gets one to BCH_SB_MEMBER_INVALID - which nothing reads or marks -
    // until the node's rewrite replaces it. set_needs_reconcile() pads a key
    // short of its replicas with those, but not a key at one replica
    if new.as_sc().arm::<Ptrs>().is_some_and(|p| p.ptrs(fs).next().is_none()) {
        let ptr = c::bch_extent_ptr::new().with_dev(c::BCH_SB_MEMBER_INVALID as u8);
        let mut kp = new.reborrow().arm::<PtrsMut>().expect("a btree node pointer: entries to its end");
        let end = kp.entries_end();
        kp.insert_entry(end, &[ptr.into_bits().0]).expect("room for a pointer: the copy's made with it");
    }

    // No triggers, at either level: gc owns alloc info and accounting here
    // and marks the new key itself when we restart. Transactional triggers
    // would check it against the alloc btree gc is rebuilding (fatal on
    // insert, GH #910), and - since gc_pos is already at this key - also mark
    // it into gc state before gc has marked the old key.
    if level == 0 {
        return t.update(iter, &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | UpdateTriggerFlags::NORUN);
    }

    t.btree_node_update_key_at_pos_raw(btree, level, new)?;
    Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_commit))
}

#[c_export]
pub fn bch2_check_fix_ptrs<'t>(
    trans: &'t BtreeTrans<'t>,
    iter:  &mut BtreeIter<'t>,
    btree: c::btree_id,
    level: c_uint,
    k:     BkeySC<'_>,
) -> c_int {
    // SAFETY: C calls check_repair in an attempt
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(check_fix_ptrs(&t, iter, btree, level, k))
}
