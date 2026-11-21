// SPDX-License-Identifier: GPL-2.0

//! A write's replicas, written: a clone of the write's bio for each of its
//! key's pointers, from the device's set, given to its IO and awaited - and
//! what failed returned.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeySC};
use crate::c;
use crate::data::dev_bio::DevBio;
use crate::data::nocow_locking::NocowKind;
use crate::data::ptrs::Ptrs;
use crate::data_structures::array_vec::ArrayVec;
use crate::errcode::BchError;
use crate::format::DataType;
use crate::fs::{Dev, Fs};
use crate::locking::MemallocFlags;
use crate::os::kernel::WRITE;
use crate::sb::members::{BlockDev, DevIoRef, IoRefIdx};
use crate::util::alloc::Kmalloc;
use crate::util::bio::{Bio, BioBox, BioClone, BioRange, HasBio, Submitted};
use crate::os::workqueue::Queue;
use crate::util::task::{waiter, Resumable, Resume, ResumeAt};
use core::ptr;
use cstruct_macros::args_stored_once;

const PTRS: usize = c::BCH_BKEY_PTRS_MAX as usize;

/// What writes' replicas came to, by device - one key's, or each extent's of a
/// data write's pass: the devices that failed, with their errors, and those a
/// nocow write left with writes to flush.
///
/// Failures past a key's pointers' worth aren't kept: a key's - a btree
/// node's - can't have more, nor a cow pass's, whose devices are its open
/// buckets'; a nocow pass's can, but it fails the whole write on any.
#[derive(Default)]
pub struct ReplicaErrs {
    failed:        ArrayVec<(u8, c::blk_status_t), PTRS>,
    pub unflushed: c::bch_devs_mask,
}

impl ReplicaErrs {
    pub fn failed(&self) -> impl Iterator<Item = (u8, c::blk_status_t)> + '_ {
        self.failed.iter().copied()
    }

    pub fn failures(&self, fs: &Fs) -> c::bch_io_failures {
        let mut failed = c::bch_io_failures::default();
        for (dev, status) in self.failed() {
            failed.set_blk_status(fs, dev.into(), status);
        }
        failed
    }

    fn fail(&mut self, dev: u8, status: c::blk_status_t) {
        let _ = self.failed.try_push((dev, status));
    }
}

/// A replica in flight, and what its end does besides its DevBio's: a nocow
/// write's bucket to unlock, and whether it's to be flushed.
struct Replica<'a> {
    io:     Submitted<DevBio, BioClone<'a>, Kmalloc, waiter::Async>,
    dev:    u8,
    nocow:  Option<(u64, bool)>,
}

const IN_FLIGHT: usize = 2 * PTRS;

/// A write's replicas in flight, each a clone of what it writes - borrowed
/// for 'a - from its device's set, and what those done came to.
///
/// A write waits on a set only with nothing in flight: one that holds clones
/// takes the next only if the set has it now, and otherwise waits for those
/// it holds first. So no two writes wait for each other's clones.
pub(crate) struct InFlight<'a> {
    fs:   &'static Fs,
    bios: ArrayVec<Replica<'a>, IN_FLIGHT>,
    errs: ReplicaErrs,
}

impl<'a> InFlight<'a> {
    pub fn new(fs: &'static Fs) -> Self {
        InFlight { fs, bios: ArrayVec::new(), errs: ReplicaErrs::default() }
    }

    /// @src - @k's data, with @opf - written to each device of @k's pointers,
    /// each holding an io ref of R's: a btree node write's a read ref, a data
    /// write's a write ref. With @cas, a nocow write's io refs, one per
    /// pointer, which the clones are given. What waiting it does - for its
    /// own clones, then a set - is resumed @at: not inline, as a set's wait
    /// may follow.
    pub async fn submit<R: IoRefIdx>(&mut self, src: BioRange<'a>, opf: u32, ty: DataType, k: &BkeyI,
                                     mut cas: Option<&mut [Option<DevIoRef<R>>]>, at: Resume<'_>)
    where
        DevIoRef<R>: Into<BlockDev>,
    {
        let fs = self.fs;
        assert!(fs.opts().nochanges.get() == 0, "writing with nochanges");
        assert!(!matches!(at.get(), ResumeAt::Inline), "a write's replicas submitted to resume inline, in an endio");

        /* For nocow, bkey_get_dev_iorefs would have bailed if it hit an
         * invalid ptr, so cas_idx stays in sync with the iterator. */
        let devs: ArrayVec<c::bch_extent_ptr, PTRS> = {
            let ptrs = BkeySC::from(k).arm::<Ptrs>().expect("writing a key with no pointers");
            ptrs.ptrs(fs).filter(|p| u32::from(p.dev()) != c::BCH_SB_MEMBER_INVALID).copied().collect()
        };
        assert!(!devs.is_empty(), "writing with no devices");

        let nocow = cas.is_some();
        let fua = opf & c::REQ_FUA != 0;
        let waiter = waiter::Async::current().await;

        for (i, ptr) in devs.iter().enumerate() {
            let ca = match &mut cas {
                Some(cas) => Some(cas[i].take().expect("a nocow write without its io refs")),
                None      => fs.dev_get_ioref::<R>(ptr.dev().into()),
            };
            let Some(ca) = ca else {
                self.errs.fail(ptr.dev(), c::BLK_STS_REMOVED);
                continue;
            };
            ca.io_done_add(WRITE, ty, (src.len() >> 9).into());
            let nocow = nocow.then(|| (ca.ptr_bucket(ptr), !fua));
            if ty != DataType::btree && fs.opts().no_data_io.get() != 0 {
                if let Some((bucket, _)) = nocow {
                    fs.nocow_locks.unlock_bucket(pos(ptr.dev().into(), bucket), NocowKind::Update);
                }
                continue;
            }

            if self.bios.is_full() {
                self.drain(at).await;
            }
            let mut n = match self.clone_of(&ca, src, opf) {
                Some(n) => n,
                None => {
                    self.drain(at).await;
                    self.clone_of(&ca, src, opf).expect("a replica's clone, waited for with none held")
                }
            };
            n.as_mut().bio_mut().set_sector(ptr.offset());
            n.as_mut().for_dev(ca);
            self.bios.push(Replica { io: n.submit_owned(waiter), dev: ptr.dev(), nocow });
        }
    }

    /// A clone of @src for @ca - waited for if nothing's in flight, else only
    /// if its set has one now.
    fn clone_of(&self, ca: &Dev, src: BioRange<'a>, opf: u32) -> Option<BioBox<DevBio, BioClone<'a>>> {
        let _noio = MemallocFlags::noio();
        let _nowait = (!self.bios.is_empty()).then(MemallocFlags::noreclaim);
        ca.write_clones.alloc_with::<BioClone<'a>, BchError, _>(ptr::null_mut(), opf, src, DevBio::new).ok()
    }

    /// Every replica in flight done, and its end: resumed @at.
    async fn drain(&mut self, at: Resume<'_>) {
        let Self { fs, bios, errs } = self;
        for r in bios.drain(..) {
            let b = r.io.resume_on(at).await;
            if let Some((dev, status)) = DevBio::finish(b) {
                errs.fail(dev, status);
            }
            if let Some((bucket, unflushed)) = r.nocow {
                fs.nocow_locks.unlock_bucket(pos(r.dev.into(), bucket), NocowKind::Update);
                if unflushed {
                    errs.unflushed.d.set(r.dev.into());
                }
            }
        }
    }

    /// Every replica done, resumed @at: what they came to.
    pub async fn wait(mut self, at: Resume<'_>) -> ReplicaErrs {
        self.drain(at).await;
        self.errs
    }
}

/// @buf, by @bio - a write, mapped to @buf for the IO - written to each device
/// of @k's pointers: the replicas that failed, once all are done, resumed @at.
#[args_stored_once]
pub async fn write_replicas(fs: &'static Fs, bio: &mut Bio, buf: &[u8], ty: DataType, k: &BkeyI,
                            at: Resume<'_>) -> ReplicaErrs {
    assert_eq!(bio.op(), c::req_op_REQ_OP_WRITE, "write_replicas() of a bio for another op");
    assert_eq!(bio.nr_vecs(), 0, "write_replicas() of a bio with buffers mapped already");
    /* mapped while @buf's borrowed: emptied once every replica's done */
    unsafe { bio.map(buf.as_ptr().cast_mut(), buf.len()) };

    let errs = {
        let mut io = InFlight::new(fs);
        /*
         * XXX: btree writes should be using io_ref[WRITE], but we aren't retrying
         * failed btree writes yet (due to device removal/ro):
         */
        io.submit::<c::DevReadRefKind>(bio.range(), bio.raw().bi_opf, ty, k, None, Resume::queue(Queue::system_dfl()))
            .await;
        io.wait(at).await
    };

    bio.truncate_vecs(0);
    errs
}
