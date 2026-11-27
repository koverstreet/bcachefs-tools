// SPDX-License-Identifier: GPL-2.0

//! A write's replicas, written: a clone of the write's bio for each of its
//! key's pointers, from the device's set, given to its IO and awaited - and
//! what failed returned.

use crate::btree::{bkey::prelude::*, bkey::{pos, BkeyI, BkeySC}};
use crate::c;
use crate::data::ptrs::IoFailures;
use crate::data::extents_format::ExtentPtr;
use crate::errcode::{status::BLK_STS_REMOVED, BchError};
use crate::init::dev::DevsMask;
use crate::sb::members_format::MEMBER_INVALID;
use crate::data::dev_bio::DevBio;
use crate::data::nocow_locking::NocowKind;
use crate::data::ptrs::Ptrs;
use crate::data_structures::array_vec::ArrayVec;
use crate::format::{DataType, BKEY_PTRS_MAX};
use crate::fs::{Dev, Fs};
use crate::locking::MemallocFlags;
use crate::os::kernel::WRITE;
use crate::sb::members::{BlockDev, DevIoRef, IoRefIdx, DevReadRefKind};
use crate::os::bio::{BioBox, BioOwner, BioRange, BioSet, Data, HasBio, Remapped, BioAlloc, Submitted};
use crate::os::workqueue::Queue;
use crate::util::task::{waiter, Resumable, Resume, ResumeAt};
use cstruct_macros::args_stored_once;

const PTRS: usize = BKEY_PTRS_MAX as usize;

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
    pub unflushed: DevsMask,
}

impl ReplicaErrs {
    pub fn failed(&self) -> impl Iterator<Item = (u8, c::blk_status_t)> + '_ {
        self.failed.iter().copied()
    }

    pub fn failures(&self, fs: &Fs) -> IoFailures {
        let mut failed = IoFailures::default();
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
    io:     Submitted<'a, DevBio, waiter::Async, BioAlloc>,
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
    fs:     &'a Fs,
    /* the set a device's clones are from, and their priority */
    clones: fn(&Dev) -> &BioSet<DevBio>,
    ioprio: u16,
    bios:   ArrayVec<Replica<'a>, IN_FLIGHT>,
    errs:   ReplicaErrs,
}

/// The io refs a write's replicas are submitted with, each of R's: a btree
/// node write's a read ref, a data write's or the journal's a write ref.
pub(crate) enum Refs<'r, R: IoRefIdx> {
    /// Each device's, got for the write
    Get,
    /// The caller's, one per pointer
    Given(&'r mut [Option<DevIoRef<R>>]),
    /// A nocow write's, one per pointer: its buckets, locked, unlocked as each
    /// replica's done
    Nocow(&'r mut [Option<DevIoRef<R>>]),
}

impl<'a> InFlight<'a> {
    pub fn new(fs: &'a Fs) -> Self {
        Self::with(fs, |ca| &ca.write_clones, 0)
    }

    /// Its clones from the set @clones picks, at @ioprio: a journal write's.
    pub fn with(fs: &'a Fs, clones: fn(&Dev) -> &BioSet<DevBio>, ioprio: u16) -> Self {
        InFlight { fs, clones, ioprio, bios: ArrayVec::new(), errs: ReplicaErrs::default() }
    }

    /// @src - @k's data, with @opf - written to each device of @k's pointers,
    /// with @refs. What waiting it does - for its own clones, then a set - is
    /// resumed @at: not inline, as a set's wait may follow.
    pub async fn submit<R: IoRefIdx>(&mut self, src: BioRange<'a>, opf: u32, ty: DataType, k: &BkeyI,
                                     mut refs: Refs<'_, R>, at: Resume<'_>)
    where
        DevIoRef<R>: Into<BlockDev>,
    {
        let fs = self.fs;
        assert!(fs.opts().nochanges.get() == 0, "writing with nochanges");
        assert!(!matches!(at.get(), ResumeAt::Inline), "a write's replicas submitted to resume inline, in an endio");

        /* For nocow, bkey_get_dev_iorefs would have bailed if it hit an
         * invalid ptr, so cas_idx stays in sync with the iterator. */
        let devs: ArrayVec<ExtentPtr, PTRS> = {
            let ptrs = BkeySC::from(k).arm::<Ptrs>().expect("writing a key with no pointers");
            ptrs.ptrs(fs).filter(|p| u32::from(p.dev()) != MEMBER_INVALID).copied().collect()
        };
        assert!(!devs.is_empty(), "writing with no devices");

        let nocow = matches!(refs, Refs::Nocow(_));
        let fua = opf & c::REQ_FUA != 0;
        let waiter = waiter::Async::current().await;

        for (i, ptr) in devs.iter().enumerate() {
            let ca = match &mut refs {
                Refs::Get => fs.dev_get_ioref::<R>(ptr.dev().into()),
                Refs::Given(cas) | Refs::Nocow(cas) =>
                    Some(cas[i].take().expect("a write's io refs, one per pointer")),
            };
            let Some(ca) = ca else {
                self.errs.fail(ptr.dev(), BLK_STS_REMOVED);
                continue;
            };
            ca.io_done_add(WRITE, ty, (src.len() >> 9).into());
            let nocow = nocow.then(|| (ca.ptr_bucket(ptr), !fua));
            if !matches!(ty, DataType::btree | DataType::journal) && fs.opts().no_data_io.get() != 0 {
                if let Some((bucket, _)) = nocow {
                    fs.nocow_locks.unlock_bucket(pos(ptr.dev().into(), bucket), NocowKind::Update);
                }
                continue;
            }

            if self.bios.is_full() {
                self.drain(at).await;
            }
            let n = match self.clone_of(&ca, src, opf) {
                Some(n) => n,
                None => {
                    self.drain(at).await;
                    self.clone_of(&ca, src, opf).expect("a replica's clone, waited for with none held")
                }
            };
            self.bios.push(Replica { io: n.submit_owned(ca.into(), ptr.offset(), waiter), dev: ptr.dev(), nocow });
        }
    }

    /// A clone of @src for @ca - waited for if nothing's in flight, else only
    /// if its set has one now.
    fn clone_of(&self, ca: &Dev, src: BioRange<'a>, opf: u32) -> Option<BioBox<'a, DevBio, BioAlloc>> {
        let _noio = MemallocFlags::noio();
        let _nowait = (!self.bios.is_empty()).then(MemallocFlags::noreclaim);
        let mut n = (self.clones)(ca).alloc::<BchError, _>(0, opf, |b| DevBio::new(b.clone_of(src))).ok()?;
        if self.ioprio != 0 {
            n.as_mut().bio_mut().set_ioprio(self.ioprio);
        }
        Some(n)
    }

    /// Every replica in flight done, and its end: resumed @at.
    async fn drain(&mut self, at: Resume<'_>) {
        let Self { fs, bios, errs, .. } = self;
        for r in bios.drain(..) {
            let b = r.io.resume_on(at).await;
            if let Some((dev, status)) = DevBio::finish(b) {
                errs.fail(dev, status);
            }
            if let Some((bucket, unflushed)) = r.nocow {
                fs.nocow_locks.unlock_bucket(pos(r.dev.into(), bucket), NocowKind::Update);
                if unflushed {
                    errs.unflushed.set(r.dev.into());
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

/// @bio's buffer - an owner's write bio, remapped to it for the IO - written
/// to each device of @k's pointers: the replicas that failed, once all are
/// done, resumed @at.
#[args_stored_once]
pub async fn write_replicas<O: BioOwner<Owns = Data>>(fs: &Fs, bio: Remapped<'_, O, &[u8]>, ty: DataType,
                                                     k: &BkeyI, at: Resume<'_>) -> ReplicaErrs {
    /* the replicas' clones borrow the mapping, and are done with it here */
    let mut io = InFlight::new(fs);
    /*
     * XXX: btree writes should be using io_ref[WRITE], but we aren't retrying
     * failed btree writes yet (due to device removal/ro):
     */
    io.submit::<DevReadRefKind>(bio.bio().range(), bio.bio().opf(), ty, k, Refs::Get,
                                Resume::queue(Queue::system_dfl()))
        .await;
    io.wait(at).await
}
