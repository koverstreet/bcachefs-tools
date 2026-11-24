// SPDX-License-Identifier: GPL-2.0

//! Scrub: a node's replica read back on its own, checked against its key, and
//! the node rewritten from another if it's bad. The journal's scrub and
//! check_data read replicas this way too.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{types_with, BkeySC};
use crate::btree::bkey_buf::BtreeKeyBuf;
use crate::btree::cache::{btree_pos_to_text, Bounce};
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, CommitFlags};
use crate::btree::read::{BsetHolder, NodePtr};
use crate::c;
use crate::data::checksum::csum_err_msg;
use crate::data::inline_data::InlineDataRef;
use crate::data::moving::{Moving, MovingIo};
use crate::data::ptrs::BkeyVRead;
use crate::data::update::dev_ptr_bit;
use crate::data_structures::array_vec::ArrayVec;
use crate::errcode::BchError;
use crate::format::{BkeyTypeFlags, Btree};
use crate::fs::{Dev, Fs, WriteRef};
use crate::os::workqueue::Queue;
use crate::sb::members::DevIoRef;
use crate::os::bio::{Bio, BioBox, BioSubmit, Buffer};
use crate::util::task::{spawn_static, waiter, Resumable, Resume};
use crate::util::vstructs::{cast_plain, Head, Unvalidated};
use crate::util::{backref::BackRef, refcount::TryRefcount, Printbuf};
use crate::prt;
use core::sync::atomic::Ordering::Relaxed;

fn replica_bset<H: BsetHolder + ?Sized>(fs: &Fs, buf: &[u8], written: usize, end: usize,
                                        err: &mut Printbuf) -> Option<usize> {
    let offset = written << 9;
    let sectors = H::sectors_of(&buf[offset..], fs.block_bits());

    if written + sectors > end {
        prt!(err, "bset past end of btree node (offset {written} len {sectors} but written {end})");
        return None;
    }

    let n = H::of(&buf[offset..(written + sectors) << 9]);
    let ty = n.bset().csum_type();
    if ty.valid(fs) {
        let (stored, csum) = (n.csum(), n.checksum(fs, offset as u32));
        if stored != csum {
            csum_err_msg(err, ty, stored, csum);
            return None;
        }
    }

    Some(sectors)
}

pub fn node_replica_good(fs: &Fs, k: BkeySC<'_>, buf: &[u8], err: &mut Printbuf) -> bool {
    let p = NodePtr::new(fs, k);
    // A corrupt key's sectors_written can't take the walk past the buffer
    let end = p.end.min(buf.len() >> 9);

    let bn = cast_plain::<Head<c::btree_node>>(buf).expect("a btree node's worth of buffer");
    if bn.magic.get() != fs.bset_magic() {
        prt!(err, "bad magic: want {:x}, got {:x}", fs.bset_magic(), bn.magic.get());
        return false;
    }

    if let Some(seq) = p.seq.filter(|&seq| bn.keys.seq.get() != seq) {
        prt!(err, "wrong btree node: seq {:x}, want {seq:x}", bn.keys.seq.get());
        return false;
    }

    let mut written = 0;
    while written < end {
        let sectors = if written == 0 {
            replica_bset::<c::btree_node<Unvalidated>>(fs, buf, written, end, err)
        } else if c::btree_node_entry::<Unvalidated>::head(&buf[written << 9..]).seq != bn.keys.seq {
            break;
        } else {
            replica_bset::<c::btree_node_entry<Unvalidated>>(fs, buf, written, end, err)
        };
        let Some(sectors) = sectors else { return false };

        written += sectors;
    }

    if p.written != 0 && written < p.written {
        prt!(err, "btree node data missing: expected {} sectors, found {written}", p.written);
        return false;
    }

    true
}

pub type Replicas<T> = ArrayVec<T, { c::BCH_BKEY_PTRS_MAX as usize }>;

pub struct ReplicaRead<'f> {
    fs:                &'f Fs,
    pub(super) ca:     DevIoRef<c::DevReadRefKind>,
    pub(super) sector: u64,
    pub(super) bio:    BioBox<'static, Bio<Buffer<Bounce<'f>>>>,
    pub ptr_bit:       u32,
}

impl<'f> ReplicaRead<'f> {
    pub fn new(fs: &'f Fs, k: BkeySC<'_>, dev: u32) -> Result<Option<Self>, BchError> {
        let pick = k.pick_read_device(fs, None, dev, c::ReadFlags::hard_require_read_device)?;
        let Some(pick) = pick else { return Ok(None) };
        let ptr_bit = dev_ptr_bit(fs, k, pick.ptr.dev().into());

        let ca = fs.dev_get_ioref::<c::DevReadRefKind>(pick.ptr.dev().into())
            .ok_or_else(|| fs.err(c::Errcode::device_offline))?;

        let bytes = fs.opts.btree_node_size.get() as usize;
        let buf = Bounce::new(fs, bytes);
        let bio = BioBox::<Bio<Buffer<_>>>::new::<BchError, _>(
            Buffer::nr_vecs(&buf), c::req_op_REQ_OP_READ, |b| b.init_from(|bio| Ok(Buffer::fill(bio, buf))))?;

        Ok(Some(Self { fs, ca, sector: pick.ptr.offset(), bio, ptr_bit }))
    }

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
            btree_pos_to_text(&mut msg, fs, key.btree, key.level, k);
            prt!(&mut msg, "\n{}", err.as_str());
            crate::bch_err_ratelimited!(fs, "{}", msg.as_str());
        }
        good
    }
}

/// All submitted before any is awaited: awaiting each in turn would read them
/// one at a time.
pub async fn read_replicas(reads: &mut [ReplicaRead<'_>], q: &Queue) -> Replicas<Result<(), BchError>> {
    let waiter = waiter::Async::current().await;
    let mut submitted = Replicas::new();
    for r in reads.iter_mut() {
        submitted.push(r.bio.submit(r.ca.bdev(), r.sector, waiter));
    }

    let mut ret = Replicas::new();
    for s in submitted {
        ret.push(s.resume_on(Resume::queue(q)).await);
    }
    ret
}

/// A node rewritten from a bset that failed its checksum gets good checksums
/// over whatever its keys held, on every replica: so the inline data is
/// checked against its own checksums, and what fails is lost.
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
    let read = read_replicas(core::slice::from_mut(&mut r), &fs.btree.read_complete_wq).await[0];
    let good = r.good(read, &key, ", rewriting node");
    drop(r);

    if key.level == 1 && key.btree.key_types() & types_with(BkeyTypeFlags::inline_data) != 0 {
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

pub fn btree_node_scrub(m: &Moving<'_>, btree: Btree, level: u32, k: BkeySC<'_>, dev: u32)
    -> Result<(), BchError>
{
    // A v1 pointer carries nothing to check a replica against
    if k.arm::<c::BtreePtrV2>().is_none() {
        return Ok(());
    }

    let fs = m.trans().fs();
    let write = fs.writes.tryget_live().ok_or_else(|| fs.err(c::Errcode::erofs_no_writes))?;
    // the task holds it, and its write ref: going read-only waits for that
    let fs_ref = unsafe { BackRef::new(fs) };
    let fs = unsafe { fs_ref.get() };
    let Some(r) = ReplicaRead::new(fs, k, dev)? else { return Ok(()) };
    let key = BtreeKeyBuf::new(btree, level, k);
    let io = m.io_start(false, fs.btree_sectors());

    spawn_static(&fs.btree.read_complete_wq, fs.tasks(), move || rewrite_if_bad(write, fs_ref, key, r, io))
}
