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
use crate::os::bio::{submit_gathered_borrowed, BioBox, BioInPlace, BvecMempool, Gathered, HasBio};
use crate::util::gather::{Fold, Gather};
use crate::os::module::{Quiesced, ThisModule};
use crate::util::mempool::Mempool;
use crate::util::task::{waiter, Resume};
use core::alloc::Layout;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering::Relaxed};
use cstruct_macros::args_stored_once;
use pin_init::{stack_pin_init, PinInit};

/// The replicas' clones: a write has each only while it's in flight, and
/// allocates the next once it's submitted, so a pool of a few makes progress.
static REPLICA_CLONES: Mempool<Kmalloc> = ::pin_init::zeroed();
static REPLICA_CLONE_BVECS: BvecMempool = ::pin_init::zeroed();

pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    Mempool::init_static(&REPLICA_CLONES, Mempool::kmalloc(4, size_of::<c::bch_write_bio>()), m)?;
    BvecMempool::init_static(&REPLICA_CLONE_BVECS, 1, m)
}

pub fn pools_exit(q: &Quiesced<'_>) {
    REPLICA_CLONE_BVECS.exit(q);
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

const MAX: usize = c::BCH_BKEY_PTRS_MAX as usize;

/// The replicas of a write that failed: each one's device and block status.
#[derive(Default)]
pub struct ReplicaErrs(pub ArrayVec<(u8, c::blk_status_t), MAX>);

impl ReplicaErrs {
    pub fn failures(&self, fs: &Fs) -> c::bch_io_failures {
        let mut failed = c::bch_io_failures::default();
        for &(dev, status) in self.0.iter() {
            failed.set_blk_status(fs, dev.into(), status);
        }
        failed
    }
}

impl Fold for ReplicaErrs {
    type Item = Option<(u8, c::blk_status_t)>;
    /// How many have failed, and each one's device and status, packed
    type Acc = (AtomicUsize, [AtomicU16; MAX]);

    fn acc() -> impl PinInit<Self::Acc> {
        (AtomicUsize::new(0), core::array::from_fn(|_| AtomicU16::new(0)))
    }

    fn fold(acc: &Self::Acc, v: Self::Item) {
        if let Some((dev, status)) = v {
            let i = acc.0.fetch_add(1, Relaxed);
            acc.1[i].store(u16::from(dev) << 8 | u16::from(status), Relaxed);
        }
    }

    fn result(acc: &Self::Acc) -> Self {
        let n = acc.0.load(Relaxed);
        ReplicaErrs(acc.1[..n].iter().map(|e| e.load(Relaxed)).map(|e| ((e >> 8) as u8, e as u8)).collect())
    }
}

/// A replica's bio done, in its endio's context: its IO accounted against its
/// device, its nocow bucket unlocked, its io ref put.
impl<R: IoRefIdx> Gathered<ReplicaErrs> for c::bch_write_bio<R> {
    fn done(mut self: Pin<&mut Self>, _: Result<(), BchError>) -> Option<(u8, c::blk_status_t)> {
        let ca = self.as_mut().project().ca.take();
        let w = &*self;
        let (dev, status) = (w.wbio.dev, w.bio.raw().bi_status);

        if let Some(ca) = &ca {
            ca.account_io_completion(c::MemberErrorType::write, w.wbio.submit_time, status == 0);
            if w.bio.raw().bi_opf & c::REQ_FUA == 0 {
                ca.write_unflushed();
            }
            if w.wbio.bits.nocow() != 0 {
                w.fs().nocow_locks.unlock_bucket(pos(ca.dev_idx.into(), w.wbio.nocow_bucket), NocowKind::Update);
            }
        }

        (status != 0).then_some((dev, status))
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
    assert_eq!(wbio.bio.nr_vecs(), 0, "write_replicas() of a bio with buffers mapped already");
    /* mapped while @buf's borrowed: emptied once every replica's done */
    unsafe {
        wbio.as_mut().bio_mut().map(buf.as_ptr().cast_mut(), buf.len())
            .expect("a write's buffer, into a bio with room for its bvecs")
    };

    stack_pin_init!(let g = Gather::<ReplicaErrs, waiter::Async>::new());
    submit_replicas(fs, wbio.as_mut(), ty, k, cas, &g);
    let errs = g.wait(at).await;

    wbio.bio_mut().truncate_vecs(0);
    errs
}

/// write_replicas()'s bios, each submitted into @g.
fn submit_replicas<R: IoRefIdx>(fs: &'static Fs, mut wbio: Pin<&mut c::bch_write_bio<R>>, ty: DataType, k: &BkeyI,
                                mut cas: Option<&mut [Option<DevIoRef<R>>]>, g: &Gather<ReplicaErrs, waiter::Async>) {
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
            n.bio.set_sector(ptr.offset());

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
                BioBox::new_in(ReplicaClones, REPLICA_CLONE_BVECS.alloc(), ptr::null_mut(), 0, opf,
                               |b| c::bch_write_bio::<R>::clone_of(b, HasBio::bio(&*wbio).range()))
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
