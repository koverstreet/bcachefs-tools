// SPDX-License-Identifier: GPL-2.0

//! Reading a btree node: its replicas tried in turn, and what's read checked,
//! decrypted and sorted down to one bset - read_done().
//!
//! A read owns its node's contents while read_in_flight is set: the cache's
//! fill, or btree_root_read(), sets it with the node write locked - making a
//! ReadInFlightNode - and whoever locks the node after waits on the bit before
//! touching its contents (btree_node_wait_on_read()). Dropping the guard
//! clears it and wakes them; a sync read waits on it as they do.
//!
//! node_read() picks a replica and starts the read's task, read_task(), in a
//! box from the module's pool, NODE_READS, its bio's bvecs inline: freed a
//! grace period after the task's done, when the filesystem may be gone - and a
//! pool, so node reads make progress under memory pressure. With a device to
//! read from, the task starts inline, its first IO submitted by the caller;
//! with none, on read_complete_wq, where the failure is reported.
//!
//! The task's state is a NodeRead. Each attempt maps the bio onto the node's
//! buffer as it is then - read_done() may have swapped it for its sort's -
//! submits, and resumes on read_complete_wq for read_done(); a failure marks
//! the replica failed, and the next is picked. What every attempt found is
//! reported once, after the IO, in one message. The node goes last, its drop
//! the wake: btree_flush_all_reads() waits on read_in_flight, and after it the
//! filesystem may go - the task comes off its list after that, which the
//! filesystem's teardown waits for too.
//!
//! read_done() is node scan's as well, for the nodes it finds: the node its
//! own, and no failures to record.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_val_to_text, AllTypes, BkeyI, BkeySC, FamilyVal, ValidateFlags, POS_MIN, SPOS_MAX};
use crate::btree::async_ops::AsyncBtreeOp;
use crate::btree::cache::{btree_id_level_to_text, Bounce, BtreeNodeCacheState};
use crate::btree::node::sort::{build_aux_trees, SortRuns};
use crate::btree::node::{BtreeNode, NodeContents, BtreeNodeFlags};
use crate::btree::path::Paths;
use crate::btree::scrub::ReplicaRead;
use crate::btree::trans::BtreeTrans;
use crate::btree::validate::{btree_err_on, btree_node_bkey_val_validate, validate_bset,
                             validate_bset_keys, ErrAt, NodeIo, NodeValidate};
use crate::c;
use crate::data::extents::{ExtentPtrDecoded, ReadFlags};
use crate::data::ptrs::IoFailures;
use crate::data::extents_format::BtreePtrV2;
use crate::errcode::{status::BLK_STS_REMOVED, blk_status_to_bch_err, BchError, Errcode};
use crate::sb::members::DevReadRefKind;
use crate::sb::members_format::MemberErrorType;
use crate::util::time_stats::TimeStat;
use crate::data::checksum::{csum_err_msg, csum_vstruct, NONCE_BTREE, CHACHA_BLOCK_SIZE};
use crate::data::ptrs::BkeyVRead;
use crate::debug::trace::events;
use crate::format::{BkeyType, Btree, BtreeFlags, DataType, Nonce, Csum, Bkey, Bset, btree_node,
    BtreeNodeEntry, Bversion, BsetHead};
use crate::fs::{Dev, Fs};
use crate::init::{damage::note_lost_extents, error::id, passes::RecoveryPass, recovery::btree_lost_data};
use crate::sb::{errors::FsckFlags, members::DevIoRef};
use crate::types::{Le32, Le64};
use crate::locking::MemallocFlags;
use crate::os::bio::{Bio, BioOwner, BioSubmit, BvecMempool};
#[cfg(CONFIG_BCACHEFS_DEBUG)]
use crate::os::kernel::{random_u64, random_u64_below};
use crate::os::kernel::{local_clock, mem_alloc_profiling_enabled, PAGE_SIZE, READ};
use crate::util::log::{LogLevel, LogMsg};
use crate::os::module::{Quiesced, ThisModule};
use crate::util::mempool::Mempool;
use crate::util::task::{exclusive_size, waiter, Resumable, Resume, Spawn, TaskSlot};
use crate::util::vstructs::{bytes_of_mut, cast_plain, cast_plain_mut, from_bytes, from_bytes_mut, Head,
                            Unsized, Unvalidated, VStruct};
use crate::util::{backref::BackRef, printbuf::Msg};
use crate::util::Printbuf;
use crate::{fs_fatal_error, log_msg_ratelimited, prt};
use core::mem::offset_of;
use core::ops::{ControlFlow, Deref, Range};
use core::pin::Pin;
use core::sync::atomic::{fence, Ordering::SeqCst};
use cstruct_macros::in_place;
use pin_init::{pin_data, pin_init};

pub struct ReadInFlightNode {
    node: BackRef<BtreeNode>,
}

impl ReadInFlightNode {
    /// # Safety
    /// Nothing else touches @node's contents while this lives: it's write
    /// locked now, and whoever locks it after waits on read_in_flight first.
    /// And @node outlives this, as BackRef::new()'s.
    pub unsafe fn new(node: &BtreeNode) -> Self {
        node.flags.set_read_in_flight(true);
        ReadInFlightNode { node: unsafe { BackRef::new(node) } }
    }

    pub fn contents(&self) -> &NodeContents {
        unsafe { self.node.contents.protected() }
    }

    pub fn key(&self) -> &BkeyI {
        self.node.key(self.contents())
    }

    pub fn parts(&mut self) -> (&BtreeNode, &mut NodeContents) {
        let b = &*self.node;
        (b, unsafe { b.contents.protected_mut() })
    }
}

impl Deref for ReadInFlightNode {
    type Target = BtreeNode;

    fn deref(&self) -> &BtreeNode {
        &self.node
    }
}

impl Drop for ReadInFlightNode {
    fn drop(&mut self) {
        let b = &*self.node;
        b.flags.set_read_in_flight(false);
        fence(SeqCst);
        b.flags.wake();
    }
}

#[pin_data]
#[repr(C)]
struct NodeReadBio {
    #[pin]
    bio: Bio,
}
crate::has_bio!(NodeReadBio, bio);

/// The module's, not the filesystem's: a box is freed a grace period after its
/// read, when the filesystem may be gone. A mempool: node reads must make
/// progress under memory pressure.
static NODE_READS: Mempool = ::pin_init::zeroed();
static NODE_READ_BVECS: BvecMempool = ::pin_init::zeroed();

pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    let size = exclusive_size::<NodeReadBio, NodeReadFut<'static>, &'static Mempool>();
    Mempool::init_static(&NODE_READS, Mempool::new(1, size), m)?;
    BvecMempool::init_static(&NODE_READ_BVECS, 1, m)
}

pub fn pools_exit(q: &Quiesced<'_>) {
    NODE_READ_BVECS.exit(q);
    NODE_READS.exit(q)
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    pub(crate) static btree_read_corrupt_ratio: u32 = 0;
    perm = 0o644, desc = "";
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    pub(crate) static btree_read_corrupt_device: i32 = 0;
    perm = 0o644, desc = "";
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
fn maybe_corrupt(buf: &mut [u8], dev: i32) {
    let (corrupt_dev, ratio) = (btree_read_corrupt_device.get(), btree_read_corrupt_ratio.get());
    if (corrupt_dev == dev || corrupt_dev < 0) && ratio != 0 && random_u64_below(ratio.into()) == 0 {
        let at = random_u64_below((buf.len() / 8) as u64) as usize * 8;
        buf[at..at + 8].copy_from_slice(&random_u64().to_ne_bytes());
    }
}

struct NodeRead<'r> {
    fs:         &'r Fs,
    io:         Pin<&'r mut NodeReadBio>,
    ca:         Option<DevIoRef<DevReadRefKind>>,
    pick:       ExtentPtrDecoded,
    failed:     IoFailures,
    start_time: u64,
    /* dropped last: that's the wake, after which the filesystem may go */
    node:       ReadInFlightNode,
}

impl NodeRead<'_> {
    async fn attempt(&mut self, waiter: waiter::Async) -> Result<(), BchError> {
        let fs = self.fs;
        let Some(ca) = &self.ca else { return Err(fs.err(blk_status_to_bch_err(BLK_STS_REMOVED))) };

        self.io.reset(c::req_op_REQ_OP_READ | c::REQ_SYNC | c::REQ_META);
        /* read_done() may have swapped the node's buffer for its sort's */
        let buf = unsafe { node_buf(&self.node, self.node.contents()) };
        let resume = Resume::queue(&fs.btree.read_complete_wq);
        let status = self.io.remap_read(buf).expect("a node's buffer, into a bio with a bvec for each of its pages")
            .submit(ca.bdev(), self.pick.ptr.offset(), waiter).resume_on(resume).await;
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        if status.is_ok() {
            maybe_corrupt(buf, self.pick.ptr.dev() as i32);
        }
        ca.account_io_completion(MemberErrorType::read, self.start_time, status.is_ok());
        status.map_err(|e| fs.err(e))
    }

    fn next_replica(&mut self) -> bool {
        let fs = self.fs;
        let k = BkeySC::from(self.node.key());
        let Ok(Some(pick)) = k.pick_read_device(fs, Some(&self.failed), 0, ReadFlags::empty()) else {
            return false;
        };
        self.pick = pick;
        self.ca = fs.dev_get_ioref::<DevReadRefKind>(pick.ptr.dev().into());
        true
    }

    async fn read(&mut self, err: &mut Printbuf) -> Result<(), BchError> {
        let waiter = waiter::Async::current().await;
        loop {
            self.start_time = local_clock();
            let ret = match self.attempt(waiter).await {
                Ok(()) => {
                    let ca = self.ca.as_ref().expect("a read that completed, from a device it had a ref on");
                    read_done(self.fs, ca, &mut self.node, Some(&mut self.failed), err)
                }
                Err(e) => {
                    self.failed.mark(&self.pick, e);
                    Err(e)
                }
            };
            if ret.is_ok() || !self.next_replica() {
                return ret;
            }
        }
    }

    fn report(&self, ret: Result<(), BchError>, err: &Printbuf) {
        let (fs, b) = (self.fs, &*self.node);
        let btree = Btree(b.c.btree_id.get().into());

        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "btree node read error at btree {}\n", b.pos_to_text(fs));
        msg.write_bytes_indented(err.as_bytes());
        self.failed.to_text(&mut msg, fs);

        /*
         * only print retry success if we read from a replica with no errors
         */
        if let Err(err) = ret {
            /*
             * Initialize buf.suppress before btree_lost_data(); that will
             * clear it if it did any work (scheduling recovery passes,
             * marking superblock
             */
            msg.set_suppressed(fs.ratelimit_suppress(&fs.btree.read_errors_hard, &crate::here_str!()));

            b.flags.set_read_error(true);
            let _ = btree_lost_data(fs, &mut msg, btree);
            note_lost_extents(fs, btree, self.node.key().into());
            prt!(&mut msg, "error {}\n", err.msg());
        } else if !self.failed.is_empty() {
            /* Separate ratelimit states for soft vs. hard errors */
            msg.set_suppressed(fs.ratelimit_suppress(&fs.btree.read_errors_soft, &crate::here_str!()));

            let dev = self.pick.ptr.dev().into();
            let used = self.failed.dev(dev).is_some();
            let used_err = |code| self.failed.dev_err(dev).is_some_and(|err| err.matches(code));

            if !used {
                prt!(&mut msg, "retry success");
            } else if used_err(Errcode::btree_node_validate_err_csum) {
                prt!(&mut msg, "checksum error, no good replica: read as is, contents unverified");
            } else {
                prt!(&mut msg, "repair success");
            }

            if (!self.failed.is_empty() || b.flags.need_rewrite()) &&
               fs.recovery.runner.current_pass.get() != RecoveryPass::scan_for_btree_nodes {
                prt!(&mut msg, " (rewriting node)");
                fs.async_btree_op(b, self.node.contents(), AsyncBtreeOp::Rewrite);
            }

            msg.newline();

            /* the replica we're using lost keys: the btree lost data */
            if used && used_err(Errcode::btree_node_validate_err_keys_lost) {
                let _ = btree_lost_data(fs, &mut msg, btree);
            }
        } else {
            msg.set_suppressed(true);
        }

        msg.set_level(if ret.is_err() { LogLevel::ERR } else { LogLevel::NOTICE });
    }

    fn maybe_merge(&self) {
        let (fs, contents) = (self.fs, self.node.contents());
        if !crate::debug::params::btree_node_merging_disabled() &&
           (contents.data().min_key != POS_MIN || self.node.key().k.p != SPOS_MAX) &&
           contents.nr.live_u64s < fs.btree.foreground_merge_threshold / 2 {
            /* Ensure very empty btree nodes get merged, in case they're
             * being read but never written to - but with a very low
             * threshold, or __bch2_foreground_maybe_merge() will
             * recursively kick off more merge attempts as it reads sibling
             * nodes:
             */
            fs.async_btree_op(&self.node, contents, AsyncBtreeOp::MergeNoRead);
        }
    }
}

pub(super) struct NodePtr {
    /// 0: a v1 pointer doesn't say
    pub(super) written: usize,
    pub(super) end:     usize,
    pub(super) seq:     Option<u64>,
    range_updated:      bool,
}

impl NodePtr {
    pub(super) fn new(fs: &Fs, k: BkeySC<'_>) -> Self {
        let v2 = k.arm::<BtreePtrV2>();
        let v2 = v2.as_ref().map(|v2| v2.v());
        let written = v2.map_or(0, |v2| v2.sectors_written.get() as usize);
        Self {
            written,
            end:           if written != 0 { written } else { fs.btree_sectors() as usize },
            seq:           v2.map(|v2| v2.seq.get()),
            range_updated: v2.is_some_and(|v2| v2.range_updated()),
        }
    }
}

enum BsetRead {
    End,
    Blacklisted,
    Keys { at: Range<usize>, journal_seq: u64 },
}

const NODE_BYTES: usize = size_of::<Head<btree_node>>();
const FITS: &str = "a bset inside the node, checked against its end";

impl BsetHead {
    pub fn nonce(&self, offset: u32) -> Nonce {
        let seq = self.seq.get();
        Nonce {
            d: [
                Le32::new(offset),
                Le32::new(seq as u32),
                Le32::new((seq >> 32) as u32),
                Le32::new(self.journal_seq.get() as u32 ^ NONCE_BTREE),
            ],
        }
    }
}

pub trait BsetHolder: Unsized + VStruct {
    const BSET: usize;
    const KEYS: usize = Self::BSET + size_of::<Head<Bset>>();

    fn node(&mut self) -> Option<&mut btree_node<Unvalidated>> {
        None
    }

    fn crypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError>;

    fn bset(&self) -> &Head<Bset> {
        Self::head(self.as_bytes())
    }

    fn bset_mut(&mut self) -> &mut Bset<Unvalidated> {
        from_bytes_mut(&mut bytes_of_mut(self)[Self::BSET..]).expect("a bset, its holder's tail")
    }

    fn csum(&self) -> Csum {
        *cast_plain(self.as_bytes()).expect("a csum, at the start of what holds a bset")
    }

    fn set_csum(&mut self, csum: Csum) {
        *cast_plain_mut(bytes_of_mut(self)).expect("a csum, at the start of what holds a bset") = csum;
    }

    fn checksum(&self, fs: &Fs, offset: u32) -> Csum {
        let i = self.bset();
        csum_vstruct(Some(fs), i.csum_type(), i.nonce(offset), self)
    }

    fn head(bytes: &[u8]) -> &Head<Bset> {
        cast_plain(&bytes[Self::BSET..]).expect("a bset header, in its node's buffer")
    }

    fn head_mut(bytes: &mut [u8]) -> &mut Head<Bset> {
        cast_plain_mut(&mut bytes[Self::BSET..]).expect("a bset header, in its node's buffer")
    }

    /// As its bset's header says, before the bytes are known to hold it: its
    /// keys, and all of it before them.
    fn sectors_of(bytes: &[u8], block_bits: u32) -> usize {
        (Self::KEYS + Self::head(bytes).u64s() * size_of::<u64>()).next_multiple_of(512 << block_bits) >> 9
    }

    fn of(bytes: &[u8]) -> &Self {
        from_bytes(bytes).expect(FITS)
    }

    fn of_mut(bytes: &mut [u8]) -> &mut Self {
        from_bytes_mut(bytes).expect(FITS)
    }
}

impl BsetHolder for btree_node<Unvalidated> {
    const BSET: usize = offset_of!(Head<btree_node>, keys);

    fn node(&mut self) -> Option<&mut btree_node<Unvalidated>> {
        Some(self)
    }

    fn crypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.csum_type(), self.keys.nonce(offset));
        let (flags, keys) = (offset_of!(Head<btree_node>, flags), Self::BSET);
        let bytes = self.as_bytes_mut();

        fs.encrypt(ty, nonce, &mut bytes[flags..keys])?;
        let nonce = nonce + ((keys - flags) as u32).next_multiple_of(CHACHA_BLOCK_SIZE);
        fs.encrypt(ty, nonce, &mut bytes[Self::KEYS..])
    }
}

impl BsetHolder for BtreeNodeEntry<Unvalidated> {
    const BSET: usize = offset_of!(Head<BtreeNodeEntry>, keys);

    fn crypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.csum_type(), self.keys.nonce(offset));
        fs.encrypt(ty, nonce, &mut self.as_bytes_mut()[Self::KEYS..])
    }
}

/// # Safety
/// The read has the node - read_in_flight - and nothing else has its buffer.
unsafe fn node_buf<'a>(b: &BtreeNode, contents: &NodeContents) -> &'a mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(contents.bufs.data.cast::<u8>(), b.buf_bytes()) }
}

fn node_header(buf: &[u8]) -> Head<btree_node> {
    *cast_plain(buf).expect(FITS)
}

fn node_header_to_text(out: &mut Printbuf, bn: &Head<btree_node>) {
    btree_id_level_to_text(out, bn.btree_id(), bn.level() as u32);
    prt!(out, " seq {:x} {}\n", bn.keys.seq.get(), bn.seq());
    prt!(out, "min: {}\n", bn.min_key);
    prt!(out, "max: {}", bn.max_key);
}

fn bset_ondisk_to_text<H: BsetHolder + ?Sized>(out: &mut Printbuf, fs: &Fs, contents: &NodeContents,
                                               bytes: &mut [u8], offset: usize) -> Option<usize> {
    let ty = H::head(bytes).csum_type();
    if !ty.valid(fs) {
        prt!(out, "unknown checksum type at offset {}: {}\n", offset, ty);
        return None;
    }

    let sectors = H::sectors_of(bytes, fs.block_bits());
    let Some(bytes) = bytes.get_mut(..sectors << 9) else {
        prt!(out, "bset at offset {} past the end of the node\n", offset);
        return None;
    };
    let n = H::of_mut(bytes);

    if n.csum() != n.checksum(fs, (offset << 9) as u32) {
        prt!(out, "invalid checksum\n");
        return None;
    }

    let _ = n.crypt(fs, (offset << 9) as u32);

    let i = n.bset_mut();
    prt!(out, "  offset {} version {}, journal seq {}\n", offset, i.version.get(), i.journal_seq.get());

    let mut out = out.indent(2);
    let mut at = 0;
    while let Some(k) = i.key_at(at) {
        if k.u64s() == 0 {
            prt!(out, "zero length key at byte {at}\n");
            break;
        }

        let mut u = Bkey::default();
        bkey_val_to_text(&mut out, Some(fs), k.cast::<FamilyVal<AllTypes>>().disassemble(contents, &mut u));
        out.newline();
        at += k.u64s() * size_of::<u64>();
    }

    Some(sectors)
}

#[cold]
pub fn node_ondisk_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode, contents: &NodeContents) {
    let k = BkeySC::from(b.key(contents));

    let dev = match k.pick_read_device(fs, None, 0, ReadFlags::empty()) {
        Ok(Some(pick)) => pick.ptr.dev().into(),
        _ => {
            prt!(out, "error getting device to read from: invalid device\n");
            return;
        }
    };
    let Ok(Some(mut r)) = ReplicaRead::new(fs, k, dev) else {
        prt!(out, "error getting device to read from: not online\n");
        return;
    };

    if let Err(e) = r.bio.submit_wait(r.ca.bdev(), r.sector) {
        prt!(out, "IO error reading btree node: {e}\n");
        return;
    }

    let buf = r.bio.as_mut().get_mut().buf_mut();
    let node_seq = node_header(buf).keys.seq;
    let mut offset = 0;

    while offset < fs.btree_sectors() as usize {
        let bytes = &mut buf[offset << 9..];
        let sectors = if offset == 0 {
            bset_ondisk_to_text::<btree_node<Unvalidated>>(out, fs, contents, bytes, offset)
        } else if BtreeNodeEntry::<Unvalidated>::head(bytes).seq != node_seq {
            break;
        } else {
            bset_ondisk_to_text::<BtreeNodeEntry<Unvalidated>>(out, fs, contents, bytes, offset)
        };
        let Some(sectors) = sectors else { return };

        offset += sectors;
    }
}

/*
 * When btree topology repair changes the start or end of a node, that might
 * mean we have to drop keys that are no longer inside the node:
 */
#[cold]
pub fn drop_keys_outside_node(b: &BtreeNode, contents: &mut NodeContents) {
    let (min_key, max_key) = (contents.data().min_key, contents.data().max_key);

    let mut keys = b.keys_mut(contents);
    keys.drop_keys_outside(min_key, max_key);

    /*
     * Always rebuild search trees: eytzinger search tree nodes directly
     * depend on the values of min/max key:
     */
    keys.contents.set_no_aux_tree(0);
    build_aux_trees(b, contents);
    contents.nr = b.keys(contents).btree_node_count_keys();

    let _ = b.for_each_key(contents, |k| {
        assert!(k.k.p >= min_key, "key {} before its node's min {min_key}", k.k.p);
        assert!(k.k.p <= max_key, "key {} past its node's max {max_key}", k.k.p);
        ControlFlow::Continue(())
    });
}

fn read_bset<H: BsetHolder + ?Sized>(v: &mut NodeValidate<'_>, contents: &mut NodeContents, p: &NodePtr,
                                     buf: &mut [u8]) -> Result<BsetRead, BchError>
{
    let (fs, b) = (v.fs, v.b);
    let written = b.written.get() as usize;
    let first = written == 0;
    let offset = written << 9;
    let node_seq = node_header(buf).keys.seq;
    let bytes = &mut buf[offset..];
    let i = *H::head(bytes);

    if !first && i.seq != node_seq {
        return Ok(BsetRead::End);
    }

    let ty = i.csum_type();
    let good_csum_type = ty.valid(fs);

    btree_err_on!(v, !good_csum_type,
                  if ty.is_encryption() { FsckFlags::empty() } else { FsckFlags::CAN_FIX },
                  ErrAt::bset(&i),
                  id::bset_unknown_csum,
                  "unknown checksum type {}", i.csum_type())?;

    let sectors = H::sectors_of(bytes, fs.block_bits());

    if btree_err_on!(v, written + sectors > p.end,
                     FsckFlags::CAN_FIX,
                     ErrAt::bset(&i),
                     id::bset_past_end_of_btree_node,
                     "bset past end of btree node (offset {} len {} but written {})",
                     written, sectors, p.end)? {
        H::head_mut(bytes).set_u64s(0);
        v.keys_lost();
    }

    let mut n = H::of_mut(bytes);

    if good_csum_type {
        let (stored, csum) = (n.csum(), n.checksum(fs, offset as u32));
        let csum_bad = stored != csum;
        if csum_bad {
            v.csum_bad();
        }

        btree_err_on!(v, csum_bad,
                      FsckFlags::CAN_FIX,
                      ErrAt::bset(n.bset()),
                      id::bset_bad_csum,
                      "{}", Msg(|out: &mut Printbuf| csum_err_msg(out, ty, stored, csum)))?;

        if let Err(err) = n.crypt(fs, offset as u32) {
            fs_fatal_error!(fs, "decrypting btree node: {}", err.msg());
            return Err(err);
        }
    }

    if let Some(bn) = n.node() {
        let bn = node_header(bn.as_bytes());

        match p.seq {
            Some(seq) => btree_err_on!(v, bn.keys.seq.get() != seq,
                                       FsckFlags::empty(),
                                       ErrAt::NODE,
                                       id::btree_node_bad_seq,
                                       "got wrong btree node: got\n{}",
                                       Msg(|out: &mut Printbuf| node_header_to_text(&mut out.indent(2), &bn)))?,
            None      => btree_err_on!(v, bn.keys.seq.get() == 0,
                                       FsckFlags::empty(),
                                       ErrAt::NODE,
                                       id::btree_node_bad_seq,
                                       "bad btree header: seq 0\n{}",
                                       Msg(|out: &mut Printbuf| node_header_to_text(out, &bn)))?,
        };

        btree_err_on!(v, b.c.level.get() == 0 &&
                         Btree(b.c.btree_id.get().into()).flags().contains(BtreeFlags::extents) &&
                         !bn.new_extent_overwrite(),
                      FsckFlags::empty(),
                      ErrAt::NODE,
                      id::btree_node_unsupported_version,
                      "btree node does not have NEW_EXTENT_OVERWRITE set")?;
    }

    /*
     * The bset code addresses keys by u16 offsets from b->data, and a
     * 512k node's end is one past the last: no bset may end there
     */
    let keys_at = (offset + H::KEYS) / 8;

    if btree_err_on!(v, keys_at + n.bset().u64s() == u16::MAX as usize + 1,
                     FsckFlags::CAN_FIX,
                     ErrAt::bset(n.bset()),
                     id::bset_past_end_of_btree_node,
                     "bset ends at the end of the btree node, u64 {}: past the last u16 offset",
                     u16::MAX as u32 + 1)? {
        n.bset_mut().truncate(0);
        n = H::of_mut(bytes);
        v.keys_lost();
    }

    validate_bset(v, contents, n, written as u32)?;

    if let Some(bn) = n.node() {
        contents.set_format(bn.format);
    }

    validate_bset_keys(v, contents, n)?;

    let i = n.bset_mut();
    i.set_big_endian(cfg!(target_endian = "big"));

    let seq = i.journal_seq.get();
    let blacklisted = fs.journal_seq_is_blacklisted(seq, true);

    btree_err_on!(v, blacklisted && first,
                  FsckFlags::CAN_FIX,
                  ErrAt::bset(i),
                  id::first_bset_blacklisted_journal_seq,
                  "first btree node bset has blacklisted journal seq ({})",
                  seq)?;

    btree_err_on!(v, blacklisted && p.written != 0,
                  FsckFlags::CAN_FIX,
                  ErrAt::bset(i),
                  id::bset_blacklisted_journal_seq,
                  "found blacklisted bset (journal seq {}) in btree node at offset {}-{}/{}",
                  seq, written, written + sectors, p.written)?;

    b.written.set((written + sectors).min(fs.btree_sectors() as usize) as u16);

    if blacklisted && !first {
        return Ok(BsetRead::Blacklisted);
    }

    Ok(BsetRead::Keys { at: keys_at..keys_at + i.u64s(), journal_seq: seq })
}

fn read_done_end(v: &mut NodeValidate<'_>, p: &NodePtr, buf: &[u8]) -> Result<(), BchError> {
    let (fs, b) = (v.fs, v.b);

    if p.written != 0 {
        if btree_err_on!(v, (b.written.get() as usize) < p.written,
                         FsckFlags::CAN_FIX,
                         ErrAt::NODE,
                         id::btree_node_data_missing,
                         "btree node data missing: expected {} sectors, found {}",
                         p.written, b.written.get())? {
            v.keys_lost();
        }
    } else {
        let data_seq = node_header(buf).keys.seq;
        let mut bne = (b.written.get() as usize) << 9;

        while bne < b.buf_bytes() {
            let e = *cast_plain::<Head<BtreeNodeEntry>>(&buf[bne..]).expect(FITS);

            if btree_err_on!(v, e.keys.seq == data_seq &&
                                !fs.journal_seq_is_blacklisted(e.keys.journal_seq.get(), true),
                             FsckFlags::CAN_FIX,
                             ErrAt::NODE,
                             id::btree_node_bset_after_end,
                             "found bset signature after last bset")? {
                v.keys_lost();
            }
            bne += fs.block_bytes() as usize;
        }
    }

    Ok(())
}

fn sort_bsets(fs: &Fs, b: &BtreeNode, contents: &mut NodeContents, runs: &mut SortRuns<'_>, journal_seq: u64) {
    let buf = unsafe { node_buf(b, contents) };
    let mut sorted = Bounce::new(fs, b.buf_bytes());

    let (u64s, nr) = runs.key_sort_fix_overlapping(&b.keys(contents), &mut sorted.u64s()[NODE_BYTES / 8..]);
    contents.nr = nr;
    sorted.u64s()[NODE_BYTES / 8 + nr.live_u64s as usize..].fill(0);

    cast_plain_mut::<Head<btree_node>>(&mut *buf).expect(FITS).keys.set_u64s(u64s);
    sorted.as_bytes_mut()[..NODE_BYTES].copy_from_slice(&buf[..NODE_BYTES]);

    if !mem_alloc_profiling_enabled() {
        sorted.swap(b, contents);
    } else {
        buf[..NODE_BYTES + u64s * 8].copy_from_slice(&sorted.as_bytes()[..NODE_BYTES + u64s * 8]);
    }

    contents.nsets = 1;
    b.flags.set_nr_written_bsets(1);
    b.keys_mut(contents).set_bset(0, (offset_of!(Head<btree_node>, keys) / 8) as u16);

    let n = btree_node::<Unvalidated>::from_bytes_mut(unsafe { node_buf(b, contents) }).expect(FITS);
    n.bset_mut().journal_seq = Le64::new(journal_seq);

    assert!(contents.nr.live_u64s as usize == n.keys.u64s(),
            "btree node read: {} live u64s sorted, its bset {}", contents.nr.live_u64s, n.keys.u64s());
}

fn validate_vals(v: &mut NodeValidate<'_>, contents: &mut NodeContents) -> Result<(), BchError> {
    let (fs, b) = (v.fs, v.b);
    let n = btree_node::<Unvalidated>::from_bytes_mut(unsafe { node_buf(b, contents) }).expect(FITS);
    let mut i = n.bset_mut();
    let mut at = 0;

    while let Some(k) = i.key_at_mut(at) {
        let k = k.cast_mut::<FamilyVal<AllTypes>>();

        let drop_key = {
            let mut tmp = Bkey::default();
            let u = k.disassemble(contents, &mut tmp);

            match btree_node_bkey_val_validate(fs, b, u, ValidateFlags::empty()) {
                Err(err) if err == BchError::from(Errcode::fsck_delete_bkey) => true,
                _ if crate::debug::params::inject_invalid_keys() && u.k.bversion == Bversion::MAX => true,
                Err(err) => {
                    v.replica_failed();
                    return Err(err);
                }
                Ok(()) => false,
            }
        };

        if drop_key {
            contents.nr.account_key_drop(0, k);

            let bytes = k.bytes();
            i = i.remove(at, bytes);
            b.flags.set_need_rewrite(true);
            b.flags.set_need_rewrite_error(true);
            v.keys_lost();
            continue;
        }

        if k.key_type() == BkeyType::btree_ptr_v2 {
            k.val_mut(&contents.format)[0] = 0;
        }

        at += k.bytes();
    }

    b.keys_mut(contents).set_bset_end(0);
    Ok(())
}

fn node_read_done(v: &mut NodeValidate<'_>, contents: &mut NodeContents) -> Result<(), BchError> {
    let (fs, b) = (v.fs, v.b);
    let p = NodePtr::new(fs, b.key(contents).into());

    /* We might get called multiple times on read retry: */
    b.written.set(0);

    let mut runs = SortRuns::new(fs);
    let buf = unsafe { node_buf(b, contents) };

    let magic = node_header(buf).magic.get();
    btree_err_on!(v, magic != fs.bset_magic(),
                  FsckFlags::empty(),
                  ErrAt::NODE,
                  id::btree_node_bad_magic,
                  "bad magic: want {:x}, got {:x}",
                  fs.bset_magic(), magic)?;

    let mut max_journal_seq = 0;
    while (b.written.get() as usize) < p.end {
        let read = if b.written.get() == 0 {
            read_bset::<btree_node<Unvalidated>>(v, contents, &p, buf)?
        } else {
            read_bset::<BtreeNodeEntry<Unvalidated>>(v, contents, &p, buf)?
        };
        match read {
            BsetRead::End                      => break,
            BsetRead::Blacklisted              => {}
            BsetRead::Keys { at, journal_seq } => {
                runs.add(at);
                max_journal_seq = max_journal_seq.max(journal_seq);
            }
        }
    }

    read_done_end(v, &p, buf)?;
    sort_bsets(fs, b, contents, &mut runs, max_journal_seq);
    validate_vals(v, contents)?;

    b.keys_mut(contents).bset_build_aux_tree(0, false);

    b.reset_sib_u64s(contents);

    if p.range_updated {
        drop_keys_outside_node(b, contents);
    }

    if p.written == 0 {
        b.flags.set_need_rewrite(true);
        b.flags.set_need_rewrite_ptr_written_zero(true);
    }

    Ok(())
}

pub fn read_done(fs: &Fs, ca: &Dev, node: &mut ReadInFlightNode, failed: Option<&mut IoFailures>,
                 err_msg: &mut Printbuf) -> Result<(), BchError> {
    let (b, contents) = node.parts();
    fs.times.get(TimeStat::btree_node_read_done as usize)
        .time(|| node_read_done(&mut NodeValidate { fs, b, io: NodeIo::Read { ca, failed, err_msg } }, contents))
}

#[in_place(NodeReadFut)]
async fn read_task<'h>(home: Pin<&'h mut NodeReadBio>, fs: BackRef<Fs>, node: ReadInFlightNode,
                       pick: ExtentPtrDecoded, ca: Option<DevIoRef<DevReadRefKind>>) {
    let fs = &*fs;
    let mut r = NodeRead {
        fs,
        io: home,
        ca,
        pick,
        failed: Default::default(),
        start_time: 0,
        node,
    };

    let mut err = Printbuf::new();
    let ret = r.read(&mut err).await;
    r.report(ret, &err);

    fs.times.get(TimeStat::btree_node_read as usize).update(r.start_time);
    if ret.is_ok() {
        r.maybe_merge();
    }
}

pub fn node_read(trans: &BtreeTrans<'_>, node: ReadInFlightNode, sync: bool) {
    let fs = trans.fs();
    /* for a sync read's wait, after the read has the node */
    let b = unsafe { BackRef::new(&*node) };
    let btree = Btree(b.c.btree_id.get().into());
    let key = BkeySC::from(node.key());

    fs.event_inc_trace(events::btree_node_read, |out| prt!(out, "{}", b.pos_to_text(fs)));

    let Ok(Some(pick)) = key.pick_read_device(fs, None, 0, ReadFlags::empty()) else {
        let mut msg = log_msg_ratelimited!(fs);

        prt!(&mut msg, "btree node read error: no device to read from\n at {}\n", b.pos_to_text(fs));
        let _ = btree_lost_data(fs, &mut msg, btree);
        note_lost_extents(fs, btree, key);

        if fs.recovery.runner.passes_complete.get() & (1 << RecoveryPass::check_topology as u32) != 0 {
            fs.emergency_read_only(&mut msg);
        }

        b.flags.set_read_error(true);
        drop(node);
        return;
    };

    let ca = fs.dev_get_ioref::<DevReadRefKind>(pick.ptr.dev().into());
    let bytes = b.buf_bytes();

    if let Some(ca) = &ca {
        ca.io_done_add(READ, DataType::btree, (bytes >> 9) as u64);

        /*
         * Callers drop btree locks before reading a node but not srcu
         * (bch2_trans_unlock(), not unlock_long()), so the read happens
         * under the srcu read lock and submit_bio() can block for an
         * unbounded time on a congested device. Flag it so
         * bch2_trans_unlock_long() doesn't report legitimate IO as a
         * stuck codepath; cleared on the next relock/trans_begin().
         */
        trans.set_srcu_io_submitted(true);
    }

    let slot = {
        let _noio = MemallocFlags::noio();
        /*
         * Not buf_nr_bvecs(): a retry maps the bio onto the buffer read_done()
         * swapped in, which may be vmalloc where this one is direct mapped
         */
        TaskSlot::exclusive_bio_in(&NODE_READS, &NODE_READ_BVECS, bytes.div_ceil(PAGE_SIZE) as u16,
                                   c::req_op_REQ_OP_READ | c::REQ_SYNC | c::REQ_META,
                                   |bio| pin_init!(NodeReadBio { bio <- bio.init::<BchError, _>() }? BchError))
    }.expect("a node read's box: its pool waits for an element");

    let fs_ref = unsafe { BackRef::new(fs) };
    let (q, tasks) = (&fs.btree.read_complete_wq, fs.tasks());
    let at = if ca.is_some() { Spawn::Inline } else { Spawn::Queue(q) };
    unsafe { slot.start_unchecked(at, tasks, move |home| read_task(home, fs_ref, node, pick, ca)) };

    if sync {
        b.wait_on_flag(BtreeNodeFlags::read_in_flight);
    }
}

impl Fs {
    pub fn btree_root_read(&self, id: Btree, k: &BkeyI, level: u32) -> Result<(), BchError> {
        let trans = crate::btree_trans!(self);

        trans.btree_cache_cannibalize_lock();

        let b = trans.btree_node_mem_alloc(&mut trans.paths(), level != 0);
        trans.btree_cache_cannibalize_unlock();

        let mut n = b.unwrap_or_else(|e| panic!("allocating a btree root's node: {e}"));
        n.locked().set_key(k);
        n.set_id(id, level as usize);
        let (b, held) = n.hash(self, BtreeNodeCacheState::CLEAN)
            .unwrap_or_else(|e| panic!("hashing a btree root's node: {e}")).into_held(self);

        let read = unsafe { ReadInFlightNode::new(b) };

        /* we can't pass the trans to read_done() for fsck errors, so it must be unlocked */
        trans.unlock();
        node_read(&trans, read, true);

        let ret = if b.flags.read_error() {
            let _ = self.btree_node_transition_state(b, BtreeNodeCacheState::FREEABLE);
            Err(self.err(Errcode::btree_node_read_error))
        } else {
            self.btree_set_root_for_read(b);
            Ok(())
        };

        drop(held);
        ret
    }
}

pub fn btree_node_wait_on_read(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: &BtreeNode) {
    trans.wait_on_node_flag(paths, b, BtreeNodeFlags::read_in_flight);
}

pub fn btree_node_wait_on_write(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: &BtreeNode) {
    trans.wait_on_node_flag(paths, b, BtreeNodeFlags::write_in_flight);
}
