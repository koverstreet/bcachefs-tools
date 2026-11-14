// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_validate, BkeyS, BkeySC, ValidateFlags, BKEY_U64S};
use crate::btree::cache::Bounce;
use crate::btree::iter::{lockrestart_do, BtreeNodeIter, CommitFlags, CommitOpts, TransAttempt, Watermark};
use crate::btree::node::sort::{
    btree_node_compact, btree_node_set_needs_whiteout, btree_node_sort, build_aux_trees, drop_whiteouts,
    sort_whiteouts, CompactMode, WriteSortRuns,
};
use crate::btree::node::bset::MAX_BSETS;
use crate::btree::node::{BtreeNode, NodeContents};
use crate::btree::path::{LockedNode, Paths, PathsUpdates, Write};
use crate::btree::trans::BtreeTrans;
use crate::btree::read::BsetHolder;
use crate::btree::validate::{validate_bset, validate_bset_keys, NodeIo, NodeValidate};
use crate::btree_trans;
use crate::c;
use crate::cstructs::c as cs;
use crate::cstructs::c::{BtreeNodeFlags, BtreeWriteType};
use crate::data::keylist::PaddedKey;
use crate::data::ptrs::{Ptrs, PtrsMut};
use crate::data::write::{submit_wbio_replicas, to_wbio, wbio_init};
use crate::data_structures::list::Walk;
use crate::debug::async_objs;
use crate::debug::trace::events;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::format::{Btree, DataType};
use crate::init::error::BkeyValidate;
use crate::inode_opts::get_io_opts;
use crate::locking::six::{six_lock_counts, six_lock_downgrade, six_lock_tryupgrade, six_trylock_ip};
use crate::types::{Le16, Le64};
use crate::util::bio::{bch2_bio_map, buf_nr_bvecs, of_bio, MapBuf};
use crate::os::kernel::{dump_stack, local_clock, unlikely, READ};
use crate::util::log::LogMsg;
use crate::util::refcount::Refcount;
use crate::util::time_stats::TimeStats;
use crate::util::vstructs::{cast_plain_mut, cast_plain_slice_mut, plain_bytes, Head, Unvalidated};
use crate::util::workqueue::{run_raw, RawWork};
use crate::util::names::Names;
use crate::util::{rcu, HumanReadable, Printbuf};
use crate::{fs_fatal_error, fs_inconsistent, log_msg_ratelimited, prt};
use core::ffi::c_uint;
use core::mem::offset_of;
use core::ptr::{self, NonNull};
use core::sync::atomic::{fence, Ordering::{Acquire, Relaxed, SeqCst}};
use cstruct_macros::bch_enum;

#[repr(C)]
pub struct BtreeWriteBio {
    work: c::work_struct,
    key: PaddedKey<{ BKEY_U64S + c::BKEY_BTREE_PTR_VAL_U64s_MAX }>,
    bounce: Bounce<'static>,
    sector_offset: c_uint,
    shard_cpu: Option<u16>,
    start_time: u64,
    /* its slot on its async object list */
    list_idx: c_uint,
    failed: c::bch_io_failures,
    pub(crate) wbio: c::bch_write_bio,
}
crate::has_bio!(BtreeWriteBio, wbio.bio);

#[bch_enum(closed)]
#[prefix(__BTREE_WRITE_)]
#[mask(BtreeWriteFlags: u32, prefix = BTREE_WRITE_)]
#[repr(u32)]
pub enum BtreeWriteFlagBit {
    only_if_need = cs::BTREE_WRITE_TYPE_BITS,
    already_started,
}

const HEADER: &str = "a btree node or node entry header, in its write's buffer";

fn bset_at(buf: &mut [u8], keys_at: usize) -> &mut Head<c::bset> {
    cast_plain_mut(&mut buf[keys_at..]).expect("a bset header, in its write's buffer")
}

fn write_started(f: BtreeNodeFlags) -> BtreeNodeFlags {
    f.with_dirty(false)
        .with_need_write(false)
        .with_write_in_flight(true)
        .with_write_in_flight_inner(true)
        .with_just_written(true)
        .with_write_idx(!f.write_idx())
        .with_write_type(BtreeWriteType::initial)
}

fn write_done(trans: &BtreeTrans<'_>, b: &BtreeNode) {
    let fs = trans.fs();
    let w = b.prev_write();

    /* If this was the first write to a btree node, signal to the interior
     * update that the write is complete:
     */
    b.will_make_reachable.first_write_done();

    fs.journal.pin_drop(&w.journal);

    let rearm = |f: BtreeNodeFlags| f.dirty() && f.need_write() && !f.never_write() &&
        !f.write_blocked() && !f.will_make_reachable();
    let (Ok(old) | Err(old)) = b.flags.fetch_update(SeqCst, Relaxed, |f| Some(if rearm(f) {
        write_started(f)
    } else {
        f.with_write_in_flight(false).with_write_in_flight_inner(false)
    }));

    if rearm(old) {
        /* Re-arm: bit stays set across the new write, no counter change. */
        node_write(trans, b, old.write_type(), BtreeWriteFlags::already_started);
    } else {
        fs.btree.cache.nr_in_flight.fetch_sub(1, Relaxed);
        fs.btree_node_write_done_clean(b);
        fence(SeqCst);
        b.flags.wake();
    }
}

fn write_update_key(t: &TransAttempt<'_, '_>, wbio: &BtreeWriteBio, b: &BtreeNode) -> Result<(), BchError> {
    let fs = t.fs();

    let mut iter = match unsafe { BtreeNodeIter::at_node(t, b) } {
        Err(e) if e == BchError::from(c::Errcode::BCH_ERR_btree_node_dying) => return Ok(()),
        r => r?,
    };
    let node = iter.iter().node_at_iter_level(t).expect("the node the iterator was got for, at its level");

    let mut n = t.bkey_reassemble_room(BkeySC::from(node.key()), c::BKEY_BTREE_PTR_U64s_MAX as usize * size_of::<u64>())?;

    const V2: &str = "a written node's key a btree_ptr_v2: v1 nodes are need_rewrite from their read, never dirtied";
    let w = BkeySC::from(wbio.key.key()).arm::<c::BtreePtrV2>().expect(V2);
    n.reborrow().arm::<c::BtreePtrV2>().expect(V2).v_mut().sectors_written = w.v().sectors_written;

    n.reborrow().arm::<PtrsMut>().expect("a btree node pointer, with pointers")
        .drop_ptrs_noerror(fs, |p| wbio.failed.dev(p.p.ptr.dev().into()).is_some());

    if n.as_sc().durability_safe(fs).nr_ptrs == 0 {
        return Err(fs.err(c::Errcode::BCH_ERR_btree_node_write_all_failed));
    }

    if !wbio.failed.is_empty() {
        let opts = get_io_opts(t, None, n.as_sc())?;
        n.set_needs_reconcile(t, None, &opts, c::SetNeedsReconcileCtx::SET_NEEDS_RECONCILE_opt_change, 0)?;
    }

    t.btree_node_update_key(iter.iter(), node, n,
                            CommitOpts::new()
                                .watermark(Watermark::interior_updates)
                                .flags(CommitFlags::JOURNAL_RECLAIM | CommitFlags::NO_ENOSPC | CommitFlags::NO_CHECK_RW),
                            wbio.failed.is_empty())
}

impl RawWork for BtreeWriteBio {
    const WORK: usize = offset_of!(Self, work);

    unsafe fn run(wbio: NonNull<Self>) {
        unsafe { write_work(wbio) }
    }
}

/// # Safety
/// @wbio's replicas are done, its endio queued it: it's put here.
unsafe fn write_work(wbio: NonNull<BtreeWriteBio>) {
    let wbio = unsafe { &mut *wbio.as_ptr() };
    let fs = wbio.wbio.fs();
    let b = unsafe { &*wbio.wbio.bio.bi_private.cast::<BtreeNode>() };

    let trans = btree_trans!(fs);
    if let Some(cpu) = wbio.shard_cpu {
        trans.shard_cpu.set(cpu as i16);
    }

    /*
     * btree_node_write_update_key commits through the journal; on a dead
     * journal that commit fails with journal_shutdown and there's no
     * point burning lockrestart_do cycles trying. Skip the update — the
     * read lock + __btree_node_write_done step below still runs so
     * BTREE_NODE_write_in_flight clears and flushers/cancellers drain.
     */
    if fs.journal.error().is_ok() &&
        (wbio.wbio.wbio.bits.first_btree_write() == 0 || !wbio.failed.is_empty())
    {
        let ret = lockrestart_do(&trans, |t| write_update_key(t, wbio, b));
        if ret.is_err() {
            b.flags.set_noevict(true);
        }

        if ret.is_err_and(|e| !e.matches(c::EROFS)) || !wbio.failed.is_empty() {
            /*
             * Writing degraded because a device was removed is not
             * news: the removal was reported when it happened, and
             * reconcile restores the replicas. Decided before
             * bch2_ratelimit(), which spends this call site's burst
             * budget - the noise must not crowd out a real error
             * arriving in the same window.
             */
            let mut msg = if ret.is_ok() && wbio.failed.all_dev_removed() {
                let mut msg = LogMsg::new(fs);
                msg.set_suppressed(true);
                msg
            } else {
                /* Separate ratelimit_states for hard and soft errors */
                log_msg_ratelimited!(fs, ret.is_err() as usize, 2)
            };

            prt!(&mut *msg, "error writing btree node at {}", b.pos_to_text(fs));
            msg.newline();

            wbio.failed.to_text(&mut msg, fs);

            match ret {
                Ok(()) => {
                    prt!(&mut *msg, "wrote degraded to ");
                    let key = BkeySC::from(unsafe { b.contents() }.key_i());
                    let d = key.arm::<Ptrs>().expect("a btree node pointer, with pointers").devs(fs);
                    fs.devs_list_to_text(&mut msg, &d);
                    msg.newline();
                }
                Err(e) => {
                    prt!(&mut *msg, "error {}\n", e.msg());
                    fs.emergency_read_only(&mut msg);
                }
            }
        }
    }

    fs.times[c::TimeStat::btree_node_write as usize].update(wbio.start_time);
    unsafe {
        async_objs::list_del(fs, c::AsyncObjKind::btree_write_bio, &mut wbio.list_idx);
        c::bio_put(&raw mut wbio.wbio.bio);
    }

    lockrestart_do(&trans, |_| {
        /* identity pinned: write_in_flight (cleared below) blocks reclaim */
        let idx = trans.node_lock_with_path(&mut trans.paths(), &b.c, c::SixLockType::SIX_LOCK_read, 0)?;
        write_done(&trans, b);
        let mut paths = trans.paths();
        trans.node_unlock(&mut paths, idx, b.c.level as usize);
        trans.path_put(&mut paths, idx, true);
        Ok(())
    }).expect("the write completion's node lock fails only with restarts");
}

unsafe extern "C" fn write_endio(bio: *mut c::bio) {
    let wbio = unsafe { to_wbio(bio) };
    let parent = (wbio.wbio.bits.split() != 0).then_some(wbio.wbio.parent);
    let fs = wbio.fs();
    let b = unsafe { &*wbio.bio.bi_private.cast::<BtreeNode>() };
    let (ca, dev, status) = (wbio.ca(), wbio.wbio.dev, wbio.bio.bi_status);

    if let Some(ca) = ca {
        ca.account_io_completion(c::MemberErrorType::write, wbio.wbio.submit_time, status == 0);
        ca.write_unflushed();
    }

    let wb = unsafe { &mut *of_bio::<BtreeWriteBio>(parent.map_or(bio, |p| &raw mut (*p).bio)) };

    if status != 0 {
        let _g = fs.write_error_lock.lock();
        wb.failed.set_blk_status(fs, dev.into(), status);
    }

    /*
     * XXX: we should be using io_ref[WRITE], but we aren't retrying failed
     * btree writes yet (due to device removal/ro):
     */
    if let Some(ca) = ca {
        unsafe { ca.io_ref[READ].dec() };
    }

    if let Some(parent) = parent {
        unsafe {
            c::bio_put(bio);
            c::bio_endio(&raw mut (*parent).bio);
        }
        return;
    }

    drop(unsafe { (&raw const wb.bounce).read() });

    let bc = &fs.btree.cache;
    bc.nr_in_flight_inner.fetch_sub(1, Relaxed);
    bc.update_throttle();
    crate::util::closure::closure_wake_up(&bc.nr_in_flight_wait);

    b.flags.set_write_in_flight_inner(false);
    fence(SeqCst);
    b.flags.wake();

    let work = &raw mut wb.work;
    let Ok(()) = unsafe { crate::util::alloc::init_at(crate::new_work!(run_raw::<BtreeWriteBio>, "&wb->work"), work) };
    let wq = &*fs.btree.write_complete_wq;
    unsafe {
        match wb.shard_cpu {
            Some(cpu) => wq.queue_work_on(cpu.into(), work),
            None      => wq.queue_work(work),
        }
    };
}

/// Encrypted, then checksummed, at byte @offset of the node.
fn seal_bset<H: BsetHolder + ?Sized>(fs: &Fs, n: &mut H, offset: u32) -> Result<(), BchError> {
    n.encrypt(fs, offset)?;
    n.set_csum(n.checksum(fs, offset));
    Ok(())
}

fn validate_bset_for_write<H: BsetHolder + ?Sized>(fs: &Fs, b: &BtreeNode, n: &mut H) -> Result<(), BchError> {
    let mut from = c::bkey_validate_context {
        level: b.c.level + 1,
        btree: Btree(b.c.btree_id.into()),
        ..Default::default()
    };
    from.bits.set_from(c::BkeyValidateFrom::btree_node);
    from.bits.set_flags(ValidateFlags::write);

    let k = BkeySC::from(unsafe { b.contents() }.key_i());
    if let Err(e) = bkey_validate(&BkeyValidate { fs, k, from: &from }) {
        fs_inconsistent!(fs, "invalid btree node key before write");
        return Err(e);
    }

    let mut v = NodeValidate { fs, b, io: NodeIo::Write };
    let ret = validate_bset_keys(&mut v, n)
        .and_then(|()| validate_bset(&mut v, n, b.written.get().into()));
    if ret.is_err() {
        fs.inconsistent();
        dump_stack();
    }

    ret
}

/// # Safety
/// @wbio is a btree node write, queued.
unsafe fn write_do_submit(wbio: &mut BtreeWriteBio) {
    let fs = wbio.wbio.fs();
    let mut tmp = wbio.key;

    let mut k = BkeyS::from(tmp.key_mut()).arm::<PtrsMut>().expect("a btree node pointer, with pointers");
    for ptr in k.ptrs_mut(fs) {
        ptr.set_offset(ptr.offset() + wbio.sector_offset as u64);
    }

    unsafe { submit_wbio_replicas(&mut wbio.wbio, fs, DataType::btree, tmp.key(), None) };
}

/// C's one word: the write type, its flags above.
fn unpack(flags: c_uint) -> (BtreeWriteType, BtreeWriteFlags) {
    (BtreeWriteType::from_bits(flags & cs::BTREE_WRITE_TYPE_MASK as c_uint), BtreeWriteFlags::from_bits_truncate(flags))
}

pub fn __bch2_btree_node_write(trans: &BtreeTrans<'_>, b: &BtreeNode, flags: c_uint) {
    let (ty, flags) = unpack(flags);
    node_write(trans, b, ty, flags)
}

pub(crate) fn node_write(trans: &BtreeTrans<'_>, b: &BtreeNode, mut ty: BtreeWriteType, flags: BtreeWriteFlags) {
    let fs = trans.fs();
    let start_time = local_clock();

    if !flags.contains(BtreeWriteFlags::already_started) {
        let only_if_need = flags.contains(BtreeWriteFlags::only_if_need);
        let Some(old) = claim(b, only_if_need) else { return };

        if only_if_need {
            ty = old.write_type();
        }

        fs.btree.cache.nr_in_flight.fetch_add(1, Relaxed);
    }

    assert!((ty == BtreeWriteType::initial) == (b.written.get() == 0));
    assert!(!b.is_fake());

    if unlikely(b.will_make_reachable.update().is_some() != (b.written.get() == 0)) {
        lifecycle_violated(fs, b, ty, flags);
    }

    let contents = unsafe { b.contents() };
    assert!(b.written.get() < fs.btree_sectors() as u16);
    assert!(b.written.get() & (fs.block_sectors() as u16 - 1) == 0);
    assert!(!b.bset_written(contents.last_set()));
    assert!(contents.data().magic.get() == fs.bset_magic());
    assert!(plain_bytes(&contents.data().format) == plain_bytes(&contents.format));

    sort_whiteouts(fs, b, unsafe { b.contents_mut() });

    let contents = unsafe { b.contents() };
    let Some(mut w) = BsetWrite::build(unsafe { Fs::borrow_raw(fs.as_ptr()) }, b, contents) else {
        write_done(trans, b);
        return;
    };

    let sealed = w.seal(b);

    /*
     * We handle btree write errors by immediately halting the journal -
     * after we've done that, we can't issue any subsequent btree writes
     * because they might have pointers to new nodes that failed to write.
     *
     * Furthermore, there's no point in doing any more btree writes because
     * with the journal stopped, we're never going to update the journal to
     * reflect that those writes were done and the data flushed from the
     * journal:
     *
     * Also on journal error, the pending write may have updates that were
     * never journalled (interior nodes, see btree_update_nodes_written()) -
     * it's critical that we don't do the write in that case otherwise we
     * will have updates visible that weren't in the journal:
     *
     * Make sure to update b->written so bch2_btree_init_next() doesn't
     * break:
     */
    if sealed.is_err() || fs.journal.error().is_err() || fs.opts().nochanges.get() != 0 {
        b.flags.set_noevict(true);
        b.written.set(b.written.get() + w.sectors);
        drop(w);
        write_done(trans, b);
        return;
    }

    w.queue(trans, b, contents, ty, start_time);
}

/*
 * We may only have a read lock on the btree node - the dirty bit is our
 * "lock" against racing with other threads that may be trying to start
 * a write, we do a write iff we clear the dirty bit. Since setting the
 * dirty bit requires a write lock, we can't race with other threads
 * redirtying it:
 */
fn claim(b: &BtreeNode, only_if_need: bool) -> Option<BtreeNodeFlags> {
    b.flags.fetch_update(Acquire, Relaxed, |f| {
        (f.dirty() &&
         (!only_if_need || f.need_write()) &&
         !f.never_write() &&
         !f.write_blocked() &&
         !(b.written.get() != 0 && f.will_make_reachable()) &&
         !f.write_in_flight())
            .then(|| write_started(f))
    }).ok()
}

#[cold]
fn lifecycle_violated(fs: &Fs, b: &BtreeNode, ty: BtreeWriteType, flags: BtreeWriteFlags) -> ! {
    /* Not a bch_log_msg: that prints when it goes out of scope, which we don't */
    let mut buf = Printbuf::new();
    prt!(&mut buf, "btree node write lifecycle invariant violated at {}", b.pos_to_text(fs));
    buf.newline();
    prt!(&mut buf, "  written: {}\n", b.written.get());
    prt!(&mut buf, "  will_make_reachable: {:x}\n", b.will_make_reachable.raw());
    prt!(&mut buf, "  flags: {}\n", b.flags);
    prt!(&mut buf, "  write_type: {}\n", ty as u32);
    prt!(&mut buf, "  already_started: {}\n", flags.contains(BtreeWriteFlags::already_started) as u32);
    panic!("{}", buf.as_str());
}

/// A node's unwritten bsets and whiteouts, sorted into one bset in a bounce
/// buffer: in a btree_node if it's the node's first, else a btree_node_entry.
struct BsetWrite {
    fs:      &'static Fs,
    bounce:  Bounce<'static>,
    first:   bool,
    keys_at: usize,
    bytes:   usize,
    sectors: u16,
}

impl BsetWrite {
    /// None if there's nothing to write.
    fn build(fs: &'static Fs, b: &BtreeNode, contents: &NodeContents) -> Option<Self> {
        let keys = b.keys(contents);
        let mut runs = WriteSortRuns::default();

        let first = b.written.get() == 0;
        let (mut bytes, keys_at) = if first {
            (size_of::<Head<c::btree_node>>(), offset_of!(Head<c::btree_node>, keys))
        } else {
            (size_of::<Head<c::btree_node_entry>>(), offset_of!(Head<c::btree_node_entry>, keys))
        };

        bytes += b.whiteout_u64s.get() as usize * size_of::<u64>();

        let mut seq = 0;
        for t in contents.sets() {
            if b.bset_written(t) {
                continue;
            }

            let i = keys.bset(t);
            bytes += i.u64s() * size_of::<u64>();
            runs.add(t.first_offset() as usize..t.end_offset as usize);
            seq = seq.max(i.journal_seq.get());
        }

        assert!(first || seq != 0);

        /* Slack for varint::decode_fast(), which reads up to 7 bytes past the end: */
        bytes += 8;

        /* buffer must be a multiple of the block size */
        bytes = bytes.next_multiple_of(fs.block_bytes() as usize);

        let mut bounce = Bounce::new(fs, bytes);
        let buf = bounce.as_bytes_mut();

        if first {
            *cast_plain_mut::<Head<c::btree_node>>(buf).expect(HEADER) = *contents.data();
        } else {
            cast_plain_mut::<Head<c::btree_node_entry>>(buf).expect(HEADER).keys = contents.data().keys;
        }
        let i = bset_at(buf, keys_at);

        i.journal_seq = Le64::new(seq);
        i.set_u64s(0);

        runs.add(b.unwritten_whiteouts());
        i.set_separate_whiteouts(false);

        let dst = cast_plain_slice_mut(&mut buf[keys_at + size_of::<Head<c::bset>>()..])
            .expect("a bset's keys, in its write's buffer");
        let u64s = runs.sort_keys_keep_unwritten_whiteouts(&keys, dst);
        let i = bset_at(buf, keys_at);
        i.set_u64s(i.u64s() + u64s);

        b.whiteout_u64s.set(0);

        assert!(!first || i.u64s() == contents.data().keys.u64s());

        /* do we have data to write? */
        if !first && i.u64s() == 0 {
            return None;
        }

        let bytes_to_write = keys_at + size_of::<Head<c::bset>>() + i.u64s() * size_of::<u64>();
        let sectors_to_write = (bytes_to_write.next_multiple_of(fs.block_bytes() as usize) >> 9) as u16;

        if let Some(v2) = BkeySC::from(contents.key_i()).arm::<c::BtreePtrV2>().filter(|_| first) {
            assert!(v2.v().sectors_written.get() == sectors_to_write);
        }

        buf[bytes_to_write..(sectors_to_write as usize) << 9].fill(0);

        let i = bset_at(buf, keys_at);
        assert!(b.written.get() + sectors_to_write <= fs.btree_sectors() as u16);
        assert!(i.big_endian() == cfg!(target_endian = "big"));
        assert!(i.seq == contents.data().keys.seq);

        i.version.set(fs.sb.version.get());
        i.set_offset(b.written.get());
        i.set_csum_type(fs.meta_checksum_type());

        Some(Self { fs, bounce, first, keys_at, bytes: bytes_to_write, sectors: sectors_to_write })
    }

    fn bset(&mut self) -> &mut Head<c::bset> {
        bset_at(&mut self.bounce, self.keys_at)
    }

    fn validate(&mut self, b: &BtreeNode) -> Result<(), BchError> {
        if self.first {
            validate_bset_for_write(self.fs, b, c::btree_node::<Unvalidated>::of_mut(&mut self.bounce))
        } else {
            validate_bset_for_write(self.fs, b, c::btree_node_entry::<Unvalidated>::of_mut(&mut self.bounce))
        }
    }

    fn seal(&mut self, b: &BtreeNode) -> Result<(), BchError> {
        let fs = self.fs;
        let i = self.bset();

        /* bch2_validate_bset will be modifying: */
        let validate_before_checksum = i.csum_type().is_encryption() ||
            i.version.get() < c::MetadataVersion::CURRENT;

        /* if we're going to be encrypting, check metadata validity first: */
        if validate_before_checksum {
            self.validate(b)?;
        }

        let offset = (b.written.get() as u32) << 9;
        let ret = if self.first {
            seal_bset(fs, c::btree_node::<Unvalidated>::of_mut(&mut self.bounce), offset)
        } else {
            seal_bset(fs, c::btree_node_entry::<Unvalidated>::of_mut(&mut self.bounce), offset)
        };
        if let Err(e) = ret {
            fs_fatal_error!(fs, "encrypting btree node: {}", e.msg());
            return Err(e);
        }

        /* if we're not encrypting, check metadata after checksumming: */
        if !validate_before_checksum {
            self.validate(b)?;
        }
        Ok(())
    }

    fn queue(self, trans: &BtreeTrans<'_>, b: &BtreeNode, contents: &NodeContents,
             ty: BtreeWriteType, start_time: u64) {
        let Self { fs, bounce, first, bytes, sectors, .. } = self;

        fs.event_inc_trace(events::btree_node_write, |out| {
            prt!(out, "offset {} sectors {} bytes {}\n{}", b.written.get(), sectors, bytes, b.pos_to_text(fs));
        });

        /*
         * blk-wbt.c throttles all writes except those that have both REQ_SYNC
         * and REQ_IDLE set...
         */

        let (data, len) = (bounce.as_raw().0, (sectors as usize) << 9);
        let bio = unsafe {
            c::bio_alloc_bioset(ptr::null_mut(),
                                buf_nr_bvecs(data, len) as _,
                                c::req_op_REQ_OP_WRITE | c::REQ_META | c::REQ_SYNC | c::REQ_IDLE,
                                c::GFP_NOIO,
                                fs.btree.bio.as_ptr())
        };
        let wbio = unsafe { &mut *of_bio::<BtreeWriteBio>(bio) };
        unsafe { wbio_init(bio) };
        unsafe { (&raw mut wbio.bounce).write(bounce) };
        wbio.sector_offset  = b.written.get().into();
        wbio.shard_cpu      = b.shard(contents, fs).map(|shard| fs.inode_shard_cpu[shard].load(Relaxed));
        wbio.start_time     = start_time;
        unsafe { (&raw mut wbio.failed).write(Default::default()) };
        wbio.wbio.wbio.c    = Some(fs);
        wbio.wbio.wbio.bits.set_first_btree_write(first.into());
        wbio.wbio.bio.bi_end_io  = Some(write_endio);
        wbio.wbio.bio.bi_private = ptr::from_ref(b).cast_mut().cast();

        unsafe { bch2_bio_map(bio, data.cast(), len) };

        wbio.key.set_key(contents.key_i());

        b.written.set(b.written.get() + sectors);

        if let Some(mut v2) = BkeyS::from(wbio.key.key_mut()).arm::<c::BtreePtrV2>() {
            v2.v_mut().sectors_written = Le16::new(b.written.get());
        }

        let stats = &fs.btree.write_stats[ty as usize];
        stats.nr.fetch_add(1, Relaxed);
        stats.bytes.fetch_add(bytes as i64, Relaxed);

        let list = &fs.async_objs[c::AsyncObjKind::btree_write_bio as usize].list;
        wbio.list_idx = list.add(ptr::NonNull::from(&mut *wbio).cast()).unwrap_or(0);

        fs.btree.cache.nr_in_flight_inner.fetch_add(1, Relaxed);
        fs.btree.cache.update_throttle();

        /*
         * Queue the bio on the trans — no block layer work while we hold
         * btree node locks. Submitted when the trans unlocks, or before
         * waiting on btree node IO.
         */
        wbio.wbio.bio.bi_next = trans.queued_write_bios.get();
        trans.queued_write_bios.set(&raw mut wbio.wbio.bio);
    }
}

pub fn trans_submit_write_bios(trans: &BtreeTrans<'_>) {
    let mut bio = trans.queued_write_bios.replace(ptr::null_mut());

    while !bio.is_null() {
        unsafe {
            let next = (*bio).bi_next;

            (*bio).bi_next = ptr::null_mut();
            write_do_submit(&mut *of_bio::<BtreeWriteBio>(bio));
            bio = next;
        }
    }
}

/*
 * Work that must be done with write lock held:
 */
pub fn bch2_btree_post_write_cleanup(fs: &Fs, b: &mut LockedNode<'_, BtreeNode, Write>) -> bool {
    if !b.flags.just_written() {
        return false;
    }

    assert!(b.whiteout_u64s.get() == 0);

    b.flags.set_just_written(false);

    /*
     * Note: immediately after write, bset_written() doesn't work - the
     * amount of data we had to write after compaction might have been
     * smaller than the offset of the last bset.
     *
     * However, we know that all bsets have been written here, as long as
     * we're still holding the write lock:
     */

    /*
     * XXX: decide if we really want to unconditionally sort down to a
     * single bset:
     */
    let nsets = b.contents().nsets;
    let invalidated_iter = if nsets > 1 {
        btree_node_sort(fs, b.node(), b.contents_mut(), 0, nsets.into());
        true
    } else {
        drop_whiteouts(b.node(), b.contents_mut(), CompactMode::COMPACT_ALL)
    };

    btree_node_set_needs_whiteout(b);

    /*
     * If later we don't unconditionally sort down to a single bset, we have
     * to ensure this is still true:
     */
    let (b, contents) = (b.node(), b.contents_mut());
    assert!(contents.last_set().end_offset as usize <= b.written_end());

    if let Some(bne) = b.want_new_bset(contents, fs) {
        b.keys_mut(contents).bset_init_next(bne);
    }

    build_aux_trees(b, contents);

    invalidated_iter
}

/// btree_node_write_if_need()
pub(crate) fn node_write_if_need(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: &BtreeNode,
                                 lock_type_held: c::SixLockType) {
    let (ty, flags) = unpack(BtreeWriteFlags::only_if_need.bits());
    node_write_trans(trans, paths, b, lock_type_held, ty, flags)
}

pub(crate) fn node_write_trans(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: &BtreeNode,
                               lock_type_held: c::SixLockType, ty: BtreeWriteType, flags: BtreeWriteFlags) {
    use c::SixLockType::*;

    let fs = trans.fs();
    let lock = b.c.lock.six();

    if lock_type_held == SIX_LOCK_intent ||
        (lock_type_held == SIX_LOCK_read && unsafe { six_lock_tryupgrade(lock) })
    {
        node_write(trans, b, ty, flags);

        /* don't cycle lock unnecessarily: */
        if b.flags.just_written() && six_trylock_ip(lock, SIX_LOCK_write, crate::ret_ip!()) {
            bch2_btree_post_write_cleanup(fs, &mut unsafe { LockedNode::new_write(b) });
            trans.node_unlock_write_pathless(paths, b);
        }

        if lock_type_held == SIX_LOCK_read {
            unsafe { six_lock_downgrade(lock) };
        }
    } else {
        assert!(lock_type_held != SIX_LOCK_write, "node_write_trans() of a node held for write: node_write_locked()");
        node_write(trans, b, ty, flags);
    }
}

/// node_write_trans(), of a node held for write.
pub(crate) fn node_write_locked(trans: &BtreeTrans<'_>, b: &mut LockedNode<'_, BtreeNode, Write>,
                                ty: BtreeWriteType, flags: BtreeWriteFlags) {
    node_write(trans, b, ty, flags);
    if b.flags.just_written() {
        bch2_btree_post_write_cleanup(trans.fs(), b);
    }
}

/*
 * @bch_btree_init_next - initialize a new (unwritten) bset that can then be
 * inserted into
 *
 * Safe to call if there already is an unwritten bset - will only add a new bset
 * if @b doesn't already have one.
 *
 * Returns true if we sorted (i.e. invalidated iterators
 */
pub(in crate::btree) fn bch2_btree_init_next(t: &TransAttempt<'_, '_>, pu: &mut PathsUpdates<'_, '_>,
                                             b: &mut LockedNode<'_, BtreeNode, Write>) {
    let trans = t.trans();
    let fs = trans.fs();
    let mut reinit_iter = false;

    ebug_assert!(six_lock_counts(b.c.lock.six()).n[c::SixLockType::SIX_LOCK_write as usize] != 0);
    let contents = b.contents();
    assert!(!b.bset_written(&contents.set[1]));
    assert!(!b.flags.just_written());

    if contents.nsets as u32 == MAX_BSETS &&
        !b.flags.write_in_flight() &&
        contents.should_compact_all(fs)
    {
        node_write_locked(trans, b, BtreeWriteType::init_next_bset, BtreeWriteFlags::empty());
        reinit_iter = true;
    }

    if b.contents().nsets as u32 == MAX_BSETS &&
        btree_node_compact(fs, b)
    {
        reinit_iter = true;
    }

    let (n, contents) = (b.node(), b.contents_mut());
    assert!((contents.nsets as u32) < MAX_BSETS);

    if let Some(bne) = n.want_new_bset(contents, fs) {
        n.keys_mut(contents).bset_init_next(bne);
    }

    build_aux_trees(n, contents);

    if reinit_iter {
        trans.node_reinit_iter(t, pu, b.held());
    }
}

impl Fs {
    /*
     * Force-complete in-flight btree writes when a clean shutdown can't finish
     * (journal in error / emergency RO).
     *
     * The clean-shutdown loop in __bch2_fs_read_only relies on dirty nodes being
     * written out through the normal journal-flush path; when the journal is dead
     * those writes can't make progress and dirty nodes are stuck. Walk live[].dirty
     * (the only place dirty nodes can live — see the invariant assertion in
     * bch2_btree_node_transition_state_locked) and:
     *
     *   - clear BTREE_NODE_dirty so __btree_node_write_done's cmpxchg, when an
     *     in-flight write of one of these nodes finishes, won't re-arm the write
     *   - settle cache_state from the bits via btree_node_live_state(): nodes
     *     with write_in_flight still set stay DIRTY (the transition is a no-op),
     *     nodes with neither bit set move to CLEAN
     *
     * Then wait for any actual in-flight writes to drain. btree_node_write_work
     * fast-paths on bch2_journal_error() so even on a dead journal write_in_flight
     * gets cleared (synchronously via the !rearm branch of __btree_node_write_done),
     * which then calls bch2_btree_node_write_done_clean to complete the
     * DIRTY → CLEAN transition.
     *
     * Caller must guarantee bch2_journal_error() is set so the work fast-paths
     * and so no new writes are submitted concurrently.
     */
    pub fn btree_cancel_all_writes(&self) {
        {
            let mut lists = self.btree.cache.lock.lock();
            for i in 0..lists.live.len() {
                let mut walk = unsafe { Walk::new(&mut lists, |l| &mut l.live[i].dirty) };
                while let Some((lists, b)) = walk.next() {
                    b.flags.set_dirty(false);
                    let _ = self.btree_node_transition_state_locked(lists, b, b.live_state());
                }
            }
        }

        self.btree_flush_all_writes();
    }

    /// Every node's @flag - a read or a write in flight - waited out: whether
    /// there were any.
    fn btree_flush_all(&self, flag: fn(&BtreeNodeFlags) -> bool) -> bool {
        let nodes = &self.btree.cache.nodes;
        let mut ret = false;
        let mut idx = 0;

        /*
         * Every node, whatever its state: FREEABLE nodes too, as the cache
         * state machine permits them to have a write in flight -
         * bch2_btree_node_write_done_clean only transitions DIRTY → CLEAN when
         * the node is currently DIRTY, so when reclaim moves a node to
         * FREEABLE while its write is still in flight the node legitimately
         * stays on bc->freeable until the write finishes. Going-RO needs
         * drained writes for the BUG_ONs in bch2_fs_btree_cache_exit's
         * freeable walk.
         *
         * One pass: a flagged node is waited on with RCU dropped - we don't
         * want to hold it across an arbitrarily-long wait - and the walk goes
         * on from the next slot.
         */
        loop {
            let waiting = {
                let rcu = rcu::read_lock();
                nodes.items_from(idx, &rcu)
                    .find(|(_, b)| flag(&unsafe { b.cast::<BtreeNode>().as_ref() }.flags.load()))
            };
            let Some((i, b)) = waiting else { return ret };

            // a node isn't freed while the fs runs
            unsafe { b.cast::<BtreeNode>().as_ref() }.wait_on_flag(flag);
            ret = true;
            idx = i + 1;
        }
    }

    pub fn btree_flush_all_reads(&self) -> bool {
        self.btree_flush_all(BtreeNodeFlags::read_in_flight)
    }

    pub fn btree_flush_all_writes(&self) -> bool {
        self.btree_flush_all(BtreeNodeFlags::write_in_flight)
    }

    #[cold]
    pub fn btree_write_stats_to_text(&self, out: &mut Printbuf) {
        out.tabstop_push(20);
        out.tabstop_push(10);

        prt!(out, "\tnr\tsize\n");

        for &(name, i) in <BtreeWriteType as Names>::ENTRIES {
            let s = &self.btree.write_stats[i as usize];
            let nr = s.nr.load(Relaxed) as u64;
            let bytes = s.bytes.load(Relaxed) as u64;

            prt!(out, "{name}:\t{nr}\t{}\n", HumanReadable(if nr != 0 { bytes / nr } else { 0 }));
        }
    }
}
