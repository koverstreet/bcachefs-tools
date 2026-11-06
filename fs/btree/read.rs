// SPDX-License-Identifier: GPL-2.0

//! Rust's part of reading btree nodes (btree/read.c): so far, a bset's
//! checksum and encryption - its nonce, and a node's and a node entry's
//! checksum() and encrypt() - and scrub, a node's replica read back and
//! checked.

#![allow(non_camel_case_types)]

use crate::fs::Dev;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{types_with, BkeyI, BkeySC};
use crate::btree::bkey_buf::BtreeKeyBuf;
use crate::btree::cache::{__bch2_btree_pos_to_text, Bounce};
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, CommitFlags};
use crate::btree::node::BtreeNode;
use crate::c;
use crate::data::checksum::{csum_err_msg, csum_vstruct, BCH_NONCE_BTREE, CHACHA_BLOCK_SIZE};
use crate::data::ptrs::BkeyVRead;
use crate::data::update::dev_ptr_bit;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::fs::WriteRef;
use crate::prt;
use crate::sb::members::DevIoRef;
use crate::types::{Le32, c_default};
use crate::util::bio::{Bio, BioBox, BioBuffersMapped, BioOwner};
use crate::util::refcount::TryRefcount;
use crate::util::backref::BackRef;
use crate::util::task::{spawn_static, waiter, Resumable, Resume, Spawn};
use crate::util::vstructs::{cast_plain, Head, SelfSized, Unvalidated};
use crate::data::inline_data::InlineDataRef;
use crate::util::Printbuf;
use crate::util::sleep::Sleep;
use crate::data::moving::{Moving, MovingIo};
use core::sync::atomic::Ordering::Relaxed;
use crate::data_structures::array_vec::ArrayVec;
use core::mem::offset_of;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{CStruct, bitfield, c_export, c_extern};
use nestify::nest;
use crate::format::{Btree, Nonce};

nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct btree_read_bio {
        pub c: *mut Fs,
        pub ca: *mut Dev, /* stashed at submit; see bch_write_bio */
        pub b: *mut BtreeNode,
        pub start_time: u64,
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
        pub bits: pub struct btree_read_bio_bits {
            idx:   u7,
            __pad: u1,
        },
        /* its slot on its async object list */
        pub list_idx: c_uint,
        pub pick: c::extent_ptr_decoded,
        pub work: c::work_struct,
        pub bio: c::bio,
    }
}
c_default!(btree_read_bio);
crate::has_bio!(btree_read_bio, bio);

c_extern! {
    pub fn bch2_btree_flush_all_reads(c: *mut Fs) -> bool;
    pub fn bch2_btree_node_read_done(c: *mut Fs, ca: *mut Dev, b: *mut BtreeNode,
                                     failed: *mut c::bch_io_failures, err: *mut Printbuf) -> c_int;
    pub fn bch2_btree_root_read(c: *mut Fs, btree: Btree, k: *const c::bkey_i, level: c_uint) -> c_int;
}

impl Fs {
    pub fn btree_root_read(&self, btree: Btree, k: &BkeyI, level: u32) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_btree_root_read(self.as_ptr(), btree, k.as_ffi(), level) })
    }
}

impl c::bset__head {
    /// The nonce of this bset, at byte @offset of its btree node: the offset,
    /// the bset's seq, and the low half of its journal seq, in the btree
    /// domain.
    pub fn nonce(&self, offset: u32) -> Nonce {
        let seq = self.seq.get();
        Nonce {
            d: [
                Le32::new(offset),
                Le32::new(seq as u32),
                Le32::new((seq >> 32) as u32),
                Le32::new(self.journal_seq.get() as u32 ^ BCH_NONCE_BTREE),
            ],
        }
    }

    fn checksum_type(&self) -> c::bch_csum_type {
        c::bch_csum_type(self.csum_type() as u32)
    }
}

/// Where a bset's keys start in a node, or a node entry: its tail, past its
/// header.
const fn keys_offset(bset: usize) -> usize {
    bset + size_of::<Head<c::bset>>()
}

/// A node's first bset - its checksum covers the node from past its csum,
/// and its encryption the node's header from flags on too, ahead of the keys,
/// their nonce starting at the next ChaCha20 block after it.
impl c::btree_node<Unvalidated> {
    pub fn checksum(&self, fs: &Fs) -> c::bch_csum {
        csum_vstruct(Some(fs), self.keys.checksum_type(), self.keys.nonce(0), self)
    }

    /// Or decrypted: ChaCha20 is its own inverse.
    pub fn encrypt(&mut self, fs: &Fs) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.checksum_type(), self.keys.nonce(0));
        let (flags, keys) = (offset_of!(Head<c::btree_node>, flags), offset_of!(Head<c::btree_node>, keys));
        let bytes = self.as_bytes_mut();

        fs.encrypt(ty, nonce, &mut bytes[flags..keys])?;
        let nonce = nonce + ((keys - flags) as u32).next_multiple_of(CHACHA_BLOCK_SIZE);
        fs.encrypt(ty, nonce, &mut bytes[keys_offset(keys)..])
    }
}

/// A later bset, at byte @offset of its node.
impl c::btree_node_entry<Unvalidated> {
    pub fn checksum(&self, fs: &Fs, offset: u32) -> c::bch_csum {
        csum_vstruct(Some(fs), self.keys.checksum_type(), self.keys.nonce(offset), self)
    }

    /// Or decrypted, as a node's.
    pub fn encrypt(&mut self, fs: &Fs, offset: u32) -> Result<(), BchError> {
        let (ty, nonce) = (self.keys.checksum_type(), self.keys.nonce(offset));
        let keys = keys_offset(offset_of!(Head<c::btree_node_entry>, keys));
        fs.encrypt(ty, nonce, &mut self.as_bytes_mut()[keys..])
    }
}

/// Whether @buf, read from a replica of the node @k points to, is that node:
/// its magic, its seq if @k has one, and every bset - the first and those
/// appended since - checksummed, as far as @k says was written; if it doesn't
/// say, till the first bset of another node. If not, what's wrong, to @err.
pub fn node_replica_good(fs: &Fs, k: BkeySC<'_>, buf: &[u8], err: &mut Printbuf) -> bool {
    const FITS: &str = "a bset inside the node, checked against its end";

    let v2 = k.arm::<c::BtreePtrV2>();
    let ptr_written = v2.as_ref().map_or(0, |v2| v2.v().sectors_written.get() as usize);
    // A corrupt key's sectors_written can't take the walk past the buffer
    let end = if ptr_written != 0 { ptr_written } else { fs.btree_sectors() as usize }
        .min(buf.len() >> 9);
    let block_bits = fs.block_bits();

    let bn = cast_plain::<Head<c::btree_node>>(buf).expect("a btree node's worth of buffer");
    if bn.magic.get() != fs.bset_magic() {
        prt!(err, "bad magic: want {:x}, got {:x}", fs.bset_magic(), bn.magic.get());
        return false;
    }

    if let Some(v2) = v2.as_ref().filter(|v2| bn.keys.seq.get() != v2.v().seq.get()) {
        prt!(err, "wrong btree node: seq {:x}, want {:x}", bn.keys.seq.get(), v2.v().seq.get());
        return false;
    }

    let mut written = 0;
    while written < end {
        let offset = written << 9;
        let (keys, sectors) = if written == 0 {
            (&bn.keys, bn.sectors(block_bits))
        } else {
            let bne = cast_plain::<Head<c::btree_node_entry>>(&buf[offset..]).expect(FITS);
            if bne.keys.seq != bn.keys.seq {
                break;
            }
            (&bne.keys, bne.sectors(block_bits))
        };

        if written + sectors > end {
            prt!(err, "bset past end of btree node (offset {written} len {sectors} but written {end})");
            return false;
        }

        let ty = keys.checksum_type();
        if ty.valid(fs) {
            let bytes = &buf[offset..(written + sectors) << 9];
            let (stored, csum) = if written == 0 {
                let n = c::btree_node::<Unvalidated>::from_bytes(bytes).expect(FITS);
                (n.csum, n.checksum(fs))
            } else {
                let n = c::btree_node_entry::<Unvalidated>::from_bytes(bytes).expect(FITS);
                (n.csum, n.checksum(fs, offset as u32))
            };
            if stored != csum {
                csum_err_msg(err, ty, stored, csum);
                return false;
            }
        }

        written += sectors;
    }

    if ptr_written != 0 && written < ptr_written {
        prt!(err, "btree node data missing: expected {ptr_written} sectors, found {written}");
        return false;
    }

    true
}

// ── Scrub ────────────────────────────────────────────────────────────────

/// One of something per pointer of a key.
pub type Replicas<T> = ArrayVec<T, { c::BCH_BKEY_PTRS_MAX as usize }>;

/// A replica of a btree node, set up to be read back and checked against its
/// pointer: its buffer and bio, and a ref on its device.
pub struct ReplicaRead<'f> {
    fs:      &'f Fs,
    ca:      DevIoRef<c::bch_dev_read_ref>,
    bio:     BioBox<Bio, BioBuffersMapped<Bounce<'f>>>,
    /// Its pointer's, by position in the key, as ptrs_io_error's bits are
    pub ptr_bit: u32,
}

impl<'f> ReplicaRead<'f> {
    /// The replica on @dev of the node @k points to - None if there's none
    /// there to read.
    pub fn new(fs: &'f Fs, k: BkeySC<'_>, dev: u32) -> Result<Option<Self>, BchError> {
        let pick = k.pick_read_device(fs, None, dev, c::bch_read_flags::BCH_READ_hard_require_read_device)?;
        let Some(pick) = pick else { return Ok(None) };
        let ptr_bit = dev_ptr_bit(fs, k, pick.ptr.dev().into());

        let ca = fs.dev_get_ioref::<c::bch_dev_read_ref>(pick.ptr.dev().into())
            .ok_or_else(|| fs.err(c::bch_errcode::BCH_ERR_device_offline))?;

        let bytes = fs.opts.btree_node_size.get() as usize;
        let mut bio = BioBox::<Bio, BioBuffersMapped<_>>::new_with::<BchError, _>(
            ca.disk_sb.bdev, c::req_op_REQ_OP_READ, Bounce::new(fs, bytes), |b| b.init())?;
        bio.bio_mut().set_sector(pick.ptr.offset());

        Ok(Some(Self { fs, ca, bio, ptr_bit }))
    }

    /// Whether the replica, read by read_replicas() - @read, how that went - is
    /// the node @key points to. If not it's said, with @action - what's done
    /// about it.
    pub fn good(&self, read: Result<(), BchError>, key: &BtreeKeyBuf, action: &str) -> bool {
        let (fs, k) = (self.fs, key.key());
        let mut err = Printbuf::new();
        let good = match read {
            Ok(()) => node_replica_good(fs, k, self.bio.buf(), &mut err),
            Err(e) => {
                prt!(&mut err, "read error: {e}");
                false
            }
        };

        if !good {
            let ca: &Dev = &self.ca;
            let mut msg = Printbuf::new();
            prt!(&mut msg, "btree node scrub: bad replica on {ca}{action}:\n");
            unsafe { __bch2_btree_pos_to_text(msg.as_raw(), fs.as_ptr(), key.btree, key.level, k.into()) };
            prt!(&mut msg, "\n{}", err.as_str());
            crate::bch_err_ratelimited!(fs, "{}", msg.as_str());
        }
        good
    }
}

/// @reads read, all of them in flight at once - submitted before any is
/// waited on, as a task waits on one thing at a time: how each went.
pub async fn read_replicas(reads: &mut [ReplicaRead<'_>]) -> Replicas<Result<(), BchError>> {
    let waiter = waiter::Async::current().await;
    let mut submitted = Replicas::new();
    for r in reads.iter_mut() {
        submitted.push(r.bio.submit(waiter));
    }

    let mut ret = Replicas::new();
    for s in submitted {
        ret.push(s.resume_on(Resume::Spawned).await);
    }
    ret
}

/// One replica of the node @k points to, at @level of @btree, on @dev: read
/// asynchronously, and if it's bad, the node rewritten from another. In
/// flight in @m till it's done, and counted in its stats: the node's sectors
/// corrected, or uncorrected if the rewrite fails.
pub fn btree_node_scrub(m: &Moving<'_>, btree: Btree, level: u32, k: BkeySC<'_>, dev: u32)
    -> Result<(), BchError>
{
    // A v1 pointer carries nothing to check a replica against
    if k.arm::<c::BtreePtrV2>().is_none() {
        return Ok(());
    }

    let fs = m.trans().fs();
    let write = fs.writes.tryget_live().ok_or_else(|| fs.err(c::bch_errcode::BCH_ERR_erofs_no_writes))?;
    // the task holds it, and its write ref: going read-only waits for that
    let fs_ref = unsafe { BackRef::new(fs) };
    let fs = unsafe { fs_ref.get() };
    let Some(r) = ReplicaRead::new(fs, k, dev)? else { return Ok(()) };
    let key = BtreeKeyBuf::new(btree, level, k);
    let io = m.io_start(false, fs.btree_sectors());

    spawn_static(Spawn::Queue(&fs.btree.read_complete_wq), rewrite_if_bad(write, fs_ref, key, r, io))
}

/// The inline data keys of the leaf @key points to checked against their own
/// checksums, through the btree: a node rewritten from a bset that failed its
/// checksum has good checksums after, whatever its keys hold - and every
/// replica the same keys. Each that doesn't match is said, and its sectors
/// are what's returned: data lost, with no copy to repair it from.
fn inline_data_bad(fs: &Fs, key: &BtreeKeyBuf) -> u64 {
    let k = key.key();
    let Some(min_key) = k.arm::<c::BtreePtrV2>().map(|v2| v2.v().min_key) else { return 0 };

    let trans = crate::btree_trans!(fs);
    let (mut bad, mut seen) = (0u64, None);
    let ret = BtreeIter::new(&trans, key.btree, min_key, BtreeIterFlags::ALL_SNAPSHOTS)
        .for_each_max(&trans, k.k.p, |_, _, k| {
            // a restart runs a key again
            if seen >= Some(k.k.p) {
                return Ok(());
            }
            seen = Some(k.k.p);

            let Some(Err(e)) = InlineDataRef::of(k).map(|d| d.verify()) else { return Ok(()) };
            bad += u64::from(k.k.size);

            let mut msg = Printbuf::new();
            prt!(&mut msg, "btree node scrub: inline data doesn't match its checksum, and there's no \
                            other copy of it:\n  ");
            e.to_text(&mut msg);
            prt!(&mut msg, "\n  {}", k.to_text(fs));
            crate::bch_err_ratelimited!(fs, "{}", msg.as_str());
            Ok(())
        });
    if let Err(e) = ret {
        crate::bch_err_ratelimited!(fs, "btree node scrub: checking inline data: {}", e.msg());
    }
    bad
}

async fn rewrite_if_bad(_write: WriteRef, fs: BackRef<Fs>, key: BtreeKeyBuf, mut r: ReplicaRead<'static>,
                        io: MovingIo) {
    let fs = &*fs;
    let read = read_replicas(core::slice::from_mut(&mut r)).await[0];
    let good = r.good(read, &key, ", rewriting node");
    drop(r);

    // the btree's keys, whichever replica they were read from
    if key.level == 1 && key.btree.key_types() & types_with(c::BKEY_TYPE_inline_data) != 0 {
        let bad = inline_data_bad(fs, &key);
        io.stats().sectors_error_uncorrected.fetch_add(bad as i64, Relaxed);
    }

    if good {
        return;
    }

    let trans = crate::btree_trans!(fs);
    let ret = commit_do(&trans, None, CommitFlags::empty(), |t| {
        t.btree_node_rewrite_key(key.btree, key.level - 1, key.key(), CommitFlags::empty())
    });

    // ENOENT: the node was rewritten since, its bad replica with it
    if !ret.as_ref().is_err_and(|e| e.matches(c::ENOENT)) {
        let stats = io.stats();
        let n = if ret.is_ok() { &stats.sectors_error_corrected } else { &stats.sectors_error_uncorrected };
        n.fetch_add(fs.btree_sectors().into(), Relaxed);
    }

    if let Some(e) = ret.err().filter(|e| !e.matches(c::ENOENT) && !e.matches(c::EROFS)) {
        crate::bch_err_ratelimited!(fs, "btree node scrub: error rewriting node: {}", e.msg());
    }
}

impl BtreeNode {
    /// The node's io lock: its write_in_flight bit, taken by whoever finds it
    /// clear - no write starts while it's held.
    pub fn io_lock(&self) {
        let Ok(()) = self.flags.lock_write_in_flight(Sleep::UNINTERRUPTIBLE.io());
    }

    pub fn io_unlock(&self) {
        ebug_assert!(self.flags.write_in_flight(), "io unlock of a node not io locked");
        self.flags.set_write_in_flight_inner(false);
        self.flags.unlock_write_in_flight();
    }
}

#[c_export]
pub fn bch2_btree_node_io_lock(b: &BtreeNode) {
    b.io_lock()
}

#[c_export]
pub fn bch2_btree_node_io_unlock(b: &BtreeNode) {
    b.io_unlock()
}
