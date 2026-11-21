// SPDX-License-Identifier: GPL-2.0

//! A write's replicas, written: a bio for each of its key's pointers - the
//! write's own for the last, a clone of it for each of the others - all
//! completing into one Gather, awaited, and what failed returned.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeySC};
use crate::c;
use crate::data::nocow_locking::NocowKind;
use crate::data::ptrs::Ptrs;
use crate::data_structures::array_vec::ArrayVec;
use crate::errcode::BchError;
use crate::format::DataType;
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::os::kernel::{local_clock, WRITE};
use crate::sb::members::{DevIoRef, IoRefIdx};
use crate::util::alloc::{Alloc, AllocError, Allocator, Kmalloc};
use crate::util::bio::{bio_box_size, submit_gathered_borrowed, BioBox, BioClone, BioInPlace, Gathered, HasBio};
use crate::util::gather::{Fold, Gather};
use crate::os::module::{Quiesced, ThisModule};
use crate::util::mempool::Mempool;
use crate::util::task::{waiter, Resume};
use core::alloc::Layout;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU8, Ordering::Relaxed};
use cstruct_macros::args_stored_once;
use pin_init::{stack_pin_init, PinInit};

/// The replicas' clones: a write has each only while it's in flight, and
/// allocates the next once it's submitted, so a pool of a few makes progress.
static REPLICA_CLONES: Mempool<Kmalloc> = ::pin_init::zeroed();

pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    Mempool::init_static(&REPLICA_CLONES, Mempool::kmalloc(4, bio_box_size::<c::bch_write_bio>(0)), m)
}

pub fn pools_exit(q: &Quiesced<'_>) {
    REPLICA_CLONES.exit(q)
}

/// REPLICA_CLONES with no state: a clone's endio frees it to the pool with
/// nothing but its bio.
#[derive(Clone, Copy)]
struct ReplicaClones;

unsafe impl Allocator for ReplicaClones {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        REPLICA_CLONES.allocate(layout)
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        REPLICA_CLONES.allocate_zeroed(layout)
    }

    unsafe fn deallocate(&self, p: NonNull<u8>, layout: Layout) {
        unsafe { REPLICA_CLONES.deallocate(p, layout) }
    }
}

unsafe impl Alloc for ReplicaClones {}

const MEMBERS: usize = c::BCH_SB_MEMBERS_MAX as usize;

/// What writes' replicas came to, by device - one key's, or each extent's of a
/// data write's pass, into one Gather: the devices that failed, with their
/// errors, and those a nocow write left with writes to flush. Small, as each
/// wait for it has a copy in its poll's frame.
///
/// Failures past a key's pointers' worth aren't kept: a key's - a btree
/// node's - can't have more, nor a cow pass's, whose devices are its open
/// buckets'; a nocow pass's can, but it fails the whole write on any.
pub struct ReplicaErrs {
    failed:        ArrayVec<(u8, c::blk_status_t), { c::BCH_BKEY_PTRS_MAX as usize }>,
    pub unflushed: c::bch_devs_mask,
}

/// A replica's: its device, its status, and whether it's a nocow write not
/// FUA.
pub type ReplicaDone = (u8, c::blk_status_t, bool);

impl ReplicaErrs {
    /// Each device that failed, and its status.
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
}

impl Fold for ReplicaErrs {
    type Item = ReplicaDone;
    /// Each device's last error - in the Gather, not copied - and the
    /// unflushed devices.
    type Acc = ([AtomicU8; MEMBERS], c::AtomicDevsMask);

    fn acc() -> impl PinInit<Self::Acc> {
        (core::array::from_fn(|_| AtomicU8::new(0)), c::AtomicDevsMask::default())
    }

    fn fold(acc: &Self::Acc, (dev, status, unflushed): ReplicaDone) {
        if status != 0 {
            acc.0[usize::from(dev)].store(status, Relaxed);
        }
        if unflushed {
            acc.1.set(dev.into());
        }
    }

    fn result(acc: &Self::Acc) -> Self {
        let mut failed = ArrayVec::new();
        for (dev, status) in acc.0.iter().enumerate() {
            let status = status.load(Relaxed);
            if status != 0 && failed.try_push((dev as u8, status)).is_err() {
                break;
            }
        }
        ReplicaErrs { failed, unflushed: c::bch_devs_mask { d: acc.1.load() } }
    }
}

/// A replica's bio done, in its endio's context: its IO accounted against its
/// device, its nocow bucket unlocked, its io ref put.
impl<R: IoRefIdx> Gathered<ReplicaErrs> for c::bch_write_bio<R> {
    fn done(mut self: Pin<&mut Self>, _: Result<(), BchError>) -> ReplicaDone {
        let ca = self.as_mut().project().ca.take();
        let w = &*self;
        let (dev, status) = (w.wbio.dev, w.bio.raw().bi_status);
        let fua = w.bio.raw().bi_opf & c::REQ_FUA != 0;
        let nocow = w.wbio.bits.nocow() != 0;

        if let Some(ca) = &ca {
            ca.account_io_completion(c::MemberErrorType::write, w.wbio.submit_time, status == 0);
            if !fua {
                ca.write_unflushed();
            }
            if nocow {
                w.fs().nocow_locks.unlock_bucket(pos(ca.dev_idx.into(), w.wbio.nocow_bucket), NocowKind::Update);
            }
        }

        (dev, status, nocow && !fua)
    }
}

/// A write bio, made where its box has it - a clone's, or a btree node write
/// task's: its bio there already.
pub(crate) fn wbio_in_place<R: IoRefIdx>(_: BioInPlace) -> impl PinInit<c::bch_write_bio<R>, BchError> {
    unsafe {
        ::pin_init::pin_init_from_closure(|w: *mut c::bch_write_bio<R>| {
            (&raw mut (*w).wbio).write(::pin_init::zeroed());
            (&raw mut (*w).ca).write(None);
            Ok(())
        })
    }
}

/// @buf, by @wbio - a write, its bio mapped to @buf for the IO - written to
/// each device of @k's pointers, by itself to the last and by clones of it
/// to the others: the replicas that failed, once all are done, resumed @at.
/// With @cas, a nocow write's io refs, one per pointer, which the bios are
/// given.
#[args_stored_once]
pub async fn write_replicas<R: IoRefIdx>(fs: &'static Fs, mut wbio: Pin<&mut c::bch_write_bio<R>>, buf: &[u8],
                                         ty: DataType, k: &BkeyI, cas: Option<&mut [Option<DevIoRef<R>>]>,
                                         at: Resume<'_>)
    -> ReplicaErrs
{
    assert_eq!(wbio.bio.op(), c::req_op_REQ_OP_WRITE, "write_replicas() of a bio for another op");
    assert_eq!(wbio.bio.raw().bi_vcnt, 0, "write_replicas() of a bio with buffers mapped already");
    /* mapped while @buf's borrowed: emptied once every replica's done */
    unsafe { wbio.as_mut().bio_mut().map(buf.as_ptr().cast_mut(), buf.len()) };

    stack_pin_init!(let g = Gather::<ReplicaErrs, waiter::Async>::new());
    unsafe { submit_replicas(fs, wbio.as_mut(), ty, k, cas, &g) };
    let errs = g.wait(at).await;

    wbio.bio_mut().truncate_vecs(0);
    errs
}

/*
 * @cas is the caller's bch_dev pointer array (parallel to @k's ptrs)
 * for the nocow path — those io_refs were already taken by the caller
 * via bkey_get_dev_iorefs and we just transfer them to the per-bio
 * wbios here.  Non-nocow callers pass NULL and we take fresh io_refs
 * via bch2_dev_get_ioref atomically.
 */
/// write_replicas()'s bios, each submitted into @g: @wbio to the last
/// device, borrowed, and an independent clone of it to each of the others.
///
/// # Safety
/// @wbio isn't moved, used or dropped till @g's wait has returned.
pub(crate) unsafe fn submit_replicas<R: IoRefIdx>(fs: &'static Fs, mut wbio: Pin<&mut c::bch_write_bio<R>>,
                                                  ty: DataType, k: &BkeyI,
                                                  mut cas: Option<&mut [Option<DevIoRef<R>>]>,
                                                  g: &Gather<ReplicaErrs, waiter::Async>) {
    let ptrs = BkeySC::from(k).arm::<Ptrs>().expect("writing a key with no pointers");
    let nocow = cas.is_some();

    assert!(fs.opts().nochanges.get() == 0, "writing with nochanges");

    /* For nocow, bkey_get_dev_iorefs would have bailed if it hit an
     * invalid ptr, so cas_idx stays in sync with the iterator. */
    let devs = || ptrs.ptrs(fs).filter(|p| u32::from(p.dev()) != c::BCH_SB_MEMBER_INVALID);
    let last = devs().count();
    assert!(last != 0, "writing with no devices");

    for (i, ptr) in devs().enumerate() {
        let ca = match &mut cas {
            Some(cas) => Some(cas[i].take().expect("a nocow write without its io refs")),
            None      => fs.dev_get_ioref::<R>(ptr.dev().into()),
        };

        let set_up = |n: Pin<&mut c::bch_write_bio<R>>| {
            let mut n = n.project();
            n.wbio.c            = Some(fs);
            n.wbio.dev          = ptr.dev();
            n.wbio.bits.set_nocow(nocow.into());
            n.wbio.submit_time  = local_clock();
            n.wbio.inode_offset = k.k().start_offset();
            n.bio.as_mut().set_sector(ptr.offset());

            let bdev = ca.as_deref().map(|ca| {
                if nocow {
                    n.wbio.nocow_bucket = ca.ptr_bucket(ptr);
                }
                ca.io_done_add(WRITE, ty, (n.bio.range().len() >> 9).into());
                ca.disk_sb.bdev
            });
            /* the bio's, till its endio takes it back */
            *n.ca = ca;

            match bdev {
                Some(bdev) => {
                    n.bio.set_dev(bdev);
                    !(ty != DataType::btree && fs.opts().no_data_io.get() != 0)
                }
                None => {
                    n.bio.set_status(c::BLK_STS_REMOVED);
                    false
                }
            }
        };

        if i + 1 != last {
            let opf = wbio.bio.raw().bi_opf;
            let mut n = {
                let _noio = MemallocFlags::noio();
                BioBox::<c::bch_write_bio<R>, BioClone<'_>, ReplicaClones>::new_with_in(
                    ReplicaClones, ptr::null_mut(), opf, HasBio::bio(&*wbio).range(), wbio_in_place)
            }.expect("a replica's clone: its pool waits for an element");

            if set_up(n.as_mut()) {
                unsafe { n.submit_gathered_unchecked(g.get()) };
            } else {
                g.get().put(n.as_mut().done(Ok(())));
            }
        } else if set_up(wbio.as_mut()) {
            unsafe { submit_gathered_borrowed(wbio.as_mut(), g.get()) };
        } else {
            g.get().put(wbio.as_mut().done(Ok(())));
        }
    }
}
