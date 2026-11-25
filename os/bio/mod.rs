// SPDX-License-Identifier: GPL-2.0

//! struct bio, from Rust. The terms, used bare here and in iter.rs and
//! owned.rs:
//!
//! - A bio describes one block IO: device, op, sector, a position in its
//!   data, and its bvecs - (page, offset, len), its own, or a clone's
//!   source's. Its c::bio is private, read through raw(). The device and
//!   sector are given only as it's submitted.
//! - Its data is what the bvecs point at. A position is a bvec_iter: a bvec,
//!   an offset into it, and the bytes left. The bio's own is bi_iter, which
//!   the block layer advances as the IO completes. A BioRange is a position
//!   and its bio; walking one gives chunks, virtually contiguous pieces of the
//!   data, or segments, the bvecs as they are, or folios (iter.rs).
//! - Its owner is what allocated it and holds it (owned.rs). Only the owner
//!   builds a bio, submits it, and walks all of its bvecs wherever its
//!   position is - range_all(), C's _all walks. Anything with a &Bio may read
//!   it, and walk its data from a position.
//! - Mapping a buffer adds it to the bvecs, until they're emptied: reset()
//!   keeps them, for another IO of the same buffers. A buffer's mapped as the
//!   bio's made, and the bio holds it (Buffer); one whose owner puts nothing
//!   in its bvecs is remapped (BioOwner::remap_read()).
//! - In flight: submitted, until its endio. A gathered submit consumes the
//!   box, and its endio drops it (BioBox::submit_gathered()); submit()
//!   borrows it for its BioFuture, which yields the status; submit_owned()
//!   consumes it for its Submitted, which gives it back; a remapped bio's
//!   gathered submit borrows its owner for the Gather's lifetime. Either way
//!   nothing reaches a bio in flight. Those states are what make reading a
//!   bio's fields through &Bio safe.
//! - A value: it moves as one, its bvecs with it, and is changed through
//!   &mut Bio - its own methods, given one by its owner's bio_mut(). Its C
//!   view, bi_io_vec, is pointed at its bvecs as it goes out to C.
//! - A Bio<B> owns what its bvecs point at as B says (BioOwns), and is lent
//!   as the plain Bio, Bio<Data>, which can't add or free them.
//!
//! The generics, below, are those terms as traits. A bio is a field of a
//! struct of its user's - an rbio, a write op - and HasBio is that
//! struct, found from its bio by the offset has_bio!() checks; BioField is
//! what the field may be. BioOwner is whatever holds a bio to build and
//! submit; Gathered, what an owner's T puts to the Gather it was submitted
//! to when its IO completes. The owner itself, BioBox, is owned.rs's.
//!
//! After Andreas Hindborg's kernel::block::bio, from his rnull series
//! (git.kernel.org a.hindborg/linux.git, rnull-v6.19-rc5, 41e04e1bb2e6): the
//! kernel crate vendored here hasn't got it, and couldn't serve userspace. So
//! it's over c::bio here, in both builds - the shim's bio_vec laid out as the
//! kernel's.

use core::ffi::c_ulong;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, Ordering::Relaxed};
use core::task::{ready, Context, Poll};

use crate::c;
use crate::data_structures::inline_vec::InlineVec;
use crate::errcode::BchError;
use crate::os::blkdev::BlockDevice;
use crate::util::backtrace::prt_fn_name;
use crate::util::sleep::Sleep;
use crate::util::task::{waiter, Completion, Done, WaitSync, Waiter};
use crate::util::gather::GatherRef;
use crate::util::task::Resumable;
use crate::util::{Printbuf, ToText};
use pin_init::{pin_data, pinned_drop};

// A bvec_iter's offset into its bvec, as a place: bi_offset - bi_bvec_done
// before 7.3, and in the shim.
#[cfg(kernel)]
#[macro_use]
mod compat {
    #[cfg(linux_ge = "7.3")]
    macro_rules! bi_offset { (@name) => { "bi_offset" }; ($i:expr) => { $i.bi_offset } }
    #[cfg(not(linux_ge = "7.3"))]
    macro_rules! bi_offset { (@name) => { "bi_bvec_done" }; ($i:expr) => { $i.bi_bvec_done } }

    // A bio's bvecs: from 7.4 in an anonymous union, with a dma-buf map; a
    // clone copies the union, as __bio_clone() copies one or the other
    #[cfg(linux_ge = "7.4")]
    macro_rules! bi_io_vec {
        ($b:expr) => { $b.__bindgen_anon_1.bi_io_vec };
        (clone $b:expr, $s:expr) => { $b.__bindgen_anon_1 = $s.__bindgen_anon_1 };
    }
    #[cfg(not(linux_ge = "7.4"))]
    macro_rules! bi_io_vec {
        ($b:expr) => { $b.bi_io_vec };
        (clone $b:expr, $s:expr) => { $b.bi_io_vec = $s.bi_io_vec };
    }
}
#[cfg(not(kernel))]
macro_rules! bi_offset { (@name) => { "bi_bvec_done" }; ($i:expr) => { $i.bi_bvec_done } }
#[cfg(not(kernel))]
macro_rules! bi_io_vec {
    ($b:expr) => { $b.bi_io_vec };
    (clone $b:expr, $s:expr) => { $b.bi_io_vec = $s.bi_io_vec };
}

mod buffers;
#[cfg(kernel)]
mod bvec;
mod iter;
mod owned;
pub mod pages;

#[cfg(kernel)]
pub use iter::{FolioVec, Folios};
pub use iter::{BioRange, Chunk, Chunks, Mapped, Segment, Segments, Walk};
pub use buffers::{io_buf, Buffer, IoBuf, MapBuf, MappedPages, Pages, IO_ALIGN};
#[cfg(kernel)]
pub use bvec::{Bvec, FolioHold, Holds};
pub use owned::{bio_put, BioBox, BioInPlace, BioLend, BioPool, BioSet, BvecAlloc, BvecMempool, Gathered, HoldsDev,
                Lent, Submitted};

fn req_op(opf: u32) -> u32 {
    opf & ((1 << c::REQ_OP_BITS) - 1)
}

/* the OS's half: bio_init() and the rest of C's, and a buffer's bvecs */
#[cfg_attr(kernel, path = "../linux_kernel/bio.rs")]
#[cfg_attr(not(kernel), path = "../linux_user/bio.rs")]
pub(super) mod sys;

pub use sys::{buf_bio_bytes, buf_nr_bvecs};

pub const BIO_INLINE_VECS: usize = 4;

/// What a bio owns of what its bvecs point at - pages, a mapped buffer,
/// folios: made with the bio, by BioInPlace::init_from(), and living as long
/// as it does.
pub trait BioOwns: Sized {
    type Bvec;

    /// As the bio's dropped, its bvecs still there - done with, and not in
    /// flight: what dropping them doesn't give back.
    fn release(&mut self, _vecs: &mut BioVecs<Self>) {}

    /// Whether an IO of @op may go to or from them: they last the bio's life,
    /// through each reset() for another IO.
    fn allows_op(_op: u32) -> bool {
        true
    }
}

/// Plain data buffers, which the bio doesn't own: it reads and writes them.
#[derive(Default)]
pub struct Data;

impl BioOwns for Data {
    type Bvec = c::bio_vec;
}

pub(crate) type BioVecs<B = Data> = InlineVec<<B as BioOwns>::Bvec, BIO_INLINE_VECS, u16, BvecAlloc>;

/// Room made on @vecs for @additional more bvecs, which can then be pushed
/// without failing: past BIO_MAX_VECS there's none, as bio_add_page() fails.
fn reserve_bvecs<V>(vecs: &mut InlineVec<V, BIO_INLINE_VECS, u16, BvecAlloc>, additional: usize)
    -> Result<(), BchError>
{
    if vecs.len() + additional > c::BIO_MAX_VECS as usize {
        return Err(c::Errcode::bio_full.into());
    }
    Ok(vecs.reserve(additional)?)
}

/// Where a bio's bvec table is: its own BioVecs, or a clone's source's - read
/// where they are now, as they move or grow. bi_io_vec is only ever where
/// they were when the bio last went out to C: raw_mut() writes it, and
/// nothing reads it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum VecsAt {
    Own,
    /// A clone's: the bio whose bvecs they are, which outlives it
    Borrowed(NonNull<Bio>),
}

// XXX: a Bio moves, and is swapped, as any Rust value - but a clone's bvecs
// are its source's, borrowed for its box's 'a, not by the Bio: swap two boxes'
// bios and a clone outlives its source.
#[pin_data(PinnedDrop)]
#[repr(C)]
#[derive(cstruct_macros::Fields)]
pub struct Bio<B: BioOwns = Data> {
    bio:     c::bio,
    vecs:    BioVecs<B>,
    vecs_at: VecsAt,
    #[pin]
    data:    BioData,
    /* last: the plain Bio is every Bio<B>'s first part */
    owns:    B,
}

/// Its bvecs emptied as they're freed: a bioset's bio is dropped again by
/// bio_put() when what it's in fails to init. What it owns drops last: a
/// mapped buffer outlives the bio.
#[pinned_drop]
impl<B: BioOwns> PinnedDrop for Bio<B> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.get_mut();
        this.owns.release(&mut this.vecs);
        unsafe { crate::os::bio::sys::uninit(&mut this.bio) };
        drop(core::mem::replace(&mut this.vecs, BioVecs::<B>::new_in(BvecAlloc::NONE)));
    }
}

// SAFETY: a bio is no thread's - submitted on one, completed on another. Not
// Sync: two holders of a &Bio could write its data at once.
unsafe impl<B: BioOwns + Send> Send for Bio<B> {}

/// A B other than Data: its bio is lent as the plain Bio, which can't add or
/// free its bvecs.
pub trait Typed: BioOwns {}

impl<B: Typed> Bio<B> {
    pub(crate) const ERASES: () = assert!(
        size_of::<B::Bvec>() == size_of::<c::bio_vec>() && align_of::<B::Bvec>() == align_of::<c::bio_vec>() &&
            (size_of::<B>() == 0 || size_of::<Bio>() <= core::mem::offset_of!(Self, owns)),
        "a bio not laid out as the plain one, then what it owns");
}

/// The same bytes, as far as the plain Bio goes: its bvecs are bio_vecs, and
/// what it owns comes after.
impl<B: Typed> Deref for Bio<B> {
    type Target = Bio;

    fn deref(&self) -> &Bio {
        let () = Self::ERASES;
        unsafe { &*core::ptr::from_ref(self).cast() }
    }
}

// XXX: through this, its bvecs can be swapped with another bio's - mem::swap()
// of two &mut Bio - and its drop then releases what B never held.
impl<B: Typed> DerefMut for Bio<B> {
    fn deref_mut(&mut self) -> &mut Bio {
        let () = Self::ERASES;
        unsafe { &mut *core::ptr::from_mut(self).cast() }
    }
}

impl<B: BioOwns> Bio<B> {
    pub fn owns(&self) -> &B {
        &self.owns
    }

    pub fn raw(&self) -> &c::bio {
        &self.bio
    }

    /// For its own methods, and C's: bi_io_vec pointed at its bvecs, where
    /// they are now.
    pub(crate) fn raw_mut(&mut self) -> &mut c::bio {
        bi_io_vec!(self.bio) = match self.vecs_at {
            VecsAt::Own         => self.vecs.as_mut_ptr().cast(),
            VecsAt::Borrowed(s) => unsafe { s.as_ref() }.vecs.as_ptr().cast_mut(),
        };
        &mut self.bio
    }

    pub fn opf(&self) -> u32 {
        self.bio.bi_opf
    }

    pub(crate) fn op(&self) -> u32 {
        req_op(self.bio.bi_opf)
    }

    /// For its IO, at @sector of @bdev: each submit's, as it goes out, from
    /// what has the device open for as long as the IO.
    fn aim(&mut self, bdev: *const BlockDevice, sector: u64) -> *mut c::bio {
        let bio = self.raw_mut();
        bio.bi_iter.bi_sector = sector;
        crate::os::blkdev::bio_set_dev(bio, bdev.cast_mut());
        bio
    }

    /// After its bvecs change: C's view of them, and its size.
    fn sync(&mut self, size: u32) {
        assert!(matches!(self.vecs_at, VecsAt::Own), "changing the bvecs of a bio whose table isn't its own");
        let (nr, max) = (self.vecs.len() as u16, self.vecs.capacity().min(c::BIO_MAX_VECS as usize) as u16);
        bi_io_vec!(self.bio) = self.vecs.as_mut_ptr().cast();
        self.bio.bi_vcnt = nr;
        self.bio.bi_max_vecs = max;
        self.bio.bi_iter.bi_size = size;
    }
}

// Unnameable outside the bio code: crate-visible for the place machinery's
// Field, in a module nothing else can reach
mod data {
    use super::Bio;
    use cstruct_macros::{placed, Init};
    use pin_init::pin_data;

    /// Sync, as Bio isn't, for a BioRange to go to another thread: what a range
    /// reads of its bio, the op and the bvecs, doesn't change while they're
    /// mapped.
    #[placed(Bio.data)]
    #[pin_data]
    #[repr(C)]
    #[derive(Init)]
    pub(crate) struct BioData {}
}
use data::BioData;

impl Bio {
    /// # Safety
    /// @ptr is a Bio's c::bio, its first field, valid for 'a, and not in flight.
    pub unsafe fn from_raw<'a>(ptr: *const c::bio) -> &'a Self {
        unsafe { &*ptr.cast() }
    }

    /// # Safety
    /// As from_raw()'s, and nothing else has it for 'a.
    pub unsafe fn from_raw_mut<'a>(ptr: *mut c::bio) -> &'a mut Self {
        unsafe { &mut *ptr.cast() }
    }

    /// Its own bvecs: none, a clone's.
    pub fn nr_vecs(&self) -> u16 {
        self.vecs.len() as u16
    }

    pub fn max_vecs(&self) -> u16 {
        self.vecs.capacity().min(c::BIO_MAX_VECS as usize) as u16
    }

    fn vecs_owner(&self) -> &Bio {
        match self.vecs_at {
            VecsAt::Own         => self,
            VecsAt::Borrowed(s) => unsafe { s.as_ref() },
        }
    }

    pub fn range(&self) -> BioRange<'_> {
        BioRange { data: &self.data, iter: self.bio.bi_iter }
    }

    /// All its bvecs, wherever its position is: its owner's walk -
    /// BioOwner::range_all() - or, for C, a bio's whose ref we were given.
    /// Panics on a clone.
    pub(crate) fn range_all(&self) -> BioRange<'_> {
        assert!(!self.cloned(), "the _all walk of a cloned bio: its bvecs are its parent's");
        let size = self.vecs.iter().map(|bv| bv.bv_len).sum();
        // INVARIANT: from its first bvec, all of them
        BioRange {
            data: &self.data,
            iter: c::bvec_iter { bi_size: size, ..Default::default() },
        }
    }

    pub fn has_data(&self) -> bool {
        self.bio.bi_iter.bi_size != 0 && !self.no_advance_iter()
    }

    /// A discard, or a write of zeroes: a size, and no bvecs.
    fn no_advance_iter(&self) -> bool {
        [c::req_op_REQ_OP_DISCARD, c::req_op_REQ_OP_SECURE_ERASE, c::req_op_REQ_OP_WRITE_ZEROES]
            .contains(&self.op())
    }

    /// @len bytes of @buf mapped, pushed onto its bvecs - or with no room for
    /// them, as reserve_bvecs() says, none of it. The bio code's alone: a
    /// bio's buffers are what owns them - a Buffer, as it's made - or, for an
    /// owner whose bio owns none, a remap_*(), for as long as it borrows them.
    ///
    /// # Safety
    /// @buf is the allocators' - in the direct map, or vmalloc - and outlives
    /// its place in the bvecs; nothing has the bio in flight.
    unsafe fn map(&mut self, buf: *mut u8, len: usize) -> Result<(), BchError> {
        let len32 = u32::try_from(len).expect("mapping a buffer of 4G or more into a bio");
        reserve_bvecs(&mut self.vecs, buf_nr_bvecs(buf, len))?;
        for bv in unsafe { crate::os::bio::sys::bvecs(buf, len) } {
            self.vecs.push(bv).expect("a buffer's bvecs, reserved");
        }
        self.sync(self.bio.bi_iter.bi_size + len32);
        Ok(())
    }

    /// For another IO: bio_reset(), its bvecs kept - the buffers it was made
    /// with - and its position all of them again, from the table: after an IO
    /// bi_iter is the block layer's. Panics on a clone, whose bvecs aren't its
    /// own. BioOwner::reset(), which checks the buffers serve @opf.
    pub(super) fn reset(&mut self, opf: u32) {
        let size = self.range_all().len();
        // SAFETY: had by &mut, so not in flight
        unsafe { crate::os::bio::sys::reset(self.raw_mut(), core::ptr::null_mut(), opf) };
        self.sync(size);
    }

    /// Panics if @range isn't this bio's.
    pub fn set_range(&mut self, range: BioRange<'_>) {
        assert!(range.is_of(self), "a bio set to another bio's range");
        self.raw_mut().bi_iter = range.pos();
    }

    /// Panics past its end.
    pub fn shrink(&mut self, bytes: u32) {
        let iter = &mut self.raw_mut().bi_iter;
        assert!(bytes <= iter.bi_size, "shrinking a bio of {} bytes to {bytes}", iter.bi_size);
        iter.bi_size = bytes;
    }

    /// At @iter: a position of its bvecs - its own, or a clone's source's.
    /// Panics if it isn't.
    pub fn set_iter(&mut self, iter: c::bvec_iter) {
        let vecs = &self.vecs_owner().vecs;
        let (idx, done) = (iter.bi_idx as usize, u64::from(bi_offset!(iter)));
        let left = vecs.get(idx..).map_or(0, |v| v.iter().map(|bv| u64::from(bv.bv_len)).sum::<u64>());
        assert!(self.no_advance_iter() ||
                (iter.bi_size == 0 && idx <= vecs.len()) ||
                (vecs.get(idx).is_some_and(|bv| done < u64::from(bv.bv_len)) &&
                 done + u64::from(iter.bi_size) <= left),
                "a bio's position at bvec {idx} + {done}, {} bytes: it has {} bvecs, {left} bytes from there",
                iter.bi_size, vecs.len());
        self.raw_mut().bi_iter = iter;
    }

    pub fn set_opf(&mut self, opf: u32) {
        self.raw_mut().bi_opf = opf as _;
    }

    pub fn set_ioprio(&mut self, prio: u16) {
        self.raw_mut().bi_ioprio = prio;
    }

    pub fn set_status(&mut self, status: u8) {
        self.raw_mut().bi_status = status;
    }

    /// A read's request position, in the file; a device's sector is its
    /// submit's.
    pub fn set_sector(&mut self, sector: u64) {
        self.raw_mut().bi_iter.bi_sector = sector;
    }

    /// Its size, for a bio with no data to size it by: a discard's.
    pub fn set_size(&mut self, bytes: u32) {
        self.raw_mut().bi_iter.bi_size = bytes;
    }

    /// Its bvecs after the first @nr dropped, and its position with them -
    /// their pages given back by whoever has them. The bio code's alone, as
    /// map() is.
    fn truncate_vecs(&mut self, nr: u16) {
        assert!(usize::from(nr) <= self.vecs.len(), "truncating a bio to more bvecs than it has");
        self.vecs.truncate(nr.into());
        self.sync(0);
    }

    /// Its completed IO's result: a BLK_STS_* errcode on failure.
    pub fn status(&self) -> Result<(), BchError> {
        match self.bio.bi_status {
            0  => Ok(()),
            // SAFETY: a status
            st => Err(crate::errcode::blk_status_to_bch_err(st).into()),
        }
    }

    /// A clone's bvecs are its parent's, and its bi_vcnt 0.
    pub fn cloned(&self) -> bool {
        self.bio.bi_flags & (1 << c::BIO_CLONED as u32) != 0
    }

    /// A clone's borrow of its source ended, its IO done: a bio of its own,
    /// with no bvecs, so nothing reaches the source through it - which may
    /// change after. Nothing, on a bio that isn't a clone.
    pub fn end_clone(&mut self) {
        if let VecsAt::Borrowed(_) = self.vecs_at {
            self.vecs_at = VecsAt::Own;
            self.bio.bi_flags &= !(1 << c::BIO_CLONED);
            self.bio.bi_iter = Default::default();
            self.sync(0);
        }
    }

    /// Its bvec @idx: its own, or a clone's source's as they are now. Panics
    /// past them - a clone's source emptied under it.
    fn bvec(&self, idx: u32) -> c::bio_vec {
        let vecs = &self.vecs_owner().vecs;
        *vecs.get(idx as usize).unwrap_or_else(|| {
            let whose = if self.cloned() { "a clone, of its source's" } else { "a bio" };
            panic!("reading bvec {idx} of {whose} {}", vecs.len())
        })
    }
}

fn request_op_str(op: u32) -> &'static str {
    macro_rules! ops {
        ($($op:ident),*) => { ::paste::paste! {
            match op {
                $(c::[<req_op_ $op>] => stringify!($op),)*
                _ => "(null)",
            }
        } };
    }
    ops!(REQ_OP_READ, REQ_OP_WRITE, REQ_OP_FLUSH, REQ_OP_DISCARD, REQ_OP_SECURE_ERASE, REQ_OP_ZONE_APPEND,
         REQ_OP_WRITE_ZEROES, REQ_OP_ZONE_OPEN, REQ_OP_ZONE_CLOSE, REQ_OP_ZONE_FINISH, REQ_OP_ZONE_RESET,
         REQ_OP_ZONE_RESET_ALL, REQ_OP_DRV_IN, REQ_OP_DRV_OUT)
}

const REQUEST_FLAG_STRS: &[&str] = &[
    "REQ_FAILFAST_DEV", "REQ_FAILFAST_TRANSPORT", "REQ_FAILFAST_DRIVER", "REQ_SYNC", "REQ_META", "REQ_PRIO",
    "REQ_NOMERGE", "REQ_IDLE", "REQ_INTEGRITY", "REQ_FUA", "REQ_PREFLUSH", "REQ_RAHEAD", "REQ_BACKGROUND",
    "REQ_NOWAIT", "REQ_POLLED", "REQ_ALLOC_CACHE", "REQ_SWAP", "REQ_DRV", "REQ_FS_PRIVATE", "REQ_ATOMIC",
    "REQ_P2PDMA", "REQ_NOUNMAP",
];

const BIO_FLAG_STRS: &[&str] = &[
    "BIO_PAGE_PINNED", "BIO_CLONED", "BIO_QUIET", "BIO_CHAIN", "BIO_REFFED", "BIO_BPS_THROTTLED",
    "BIO_TRACE_COMPLETION", "BIO_CGROUP_ACCT", "BIO_QOS_THROTTLED", "BIO_QOS_MERGED", "BIO_REMAPPED",
    "BIO_ZONE_WRITE_PLUGGING", "BIO_EMULATES_ZONE_APPEND",
];

impl ToText for Bio {
    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        let b = &self.bio;
        if out.nr_tabstops() == 0 {
            out.tabstop_push(24);
        }

        write!(out, "bi_bdev:\t");
        unsafe { crate::os::blkdev::bio_bdev(b) }.to_text(out);
        out.newline();

        write!(out, "bi_opf:\t{} ", request_op_str(self.op()));
        out.bitflags(REQUEST_FLAG_STRS, (b.bi_opf >> c::REQ_OP_BITS).into());
        out.newline();

        write!(out, "bi_flags:\t");
        out.bitflags(BIO_FLAG_STRS, b.bi_flags.into());
        out.newline();

        write!(out, "bi_status:\t{}\n", b.bi_status);

        write!(out, "bi_iter.bi_sector\t{}\n", { b.bi_iter.bi_sector });
        write!(out, "bi_iter.bi_size\t{}\n", b.bi_iter.bi_size);
        write!(out, "bi_iter.bi_idx\t{}\n", b.bi_iter.bi_idx);
        write!(out, concat!("bi_iter.", bi_offset!(@name), "\t{}\n"), bi_offset!(b.bi_iter));

        let remaining = unsafe { AtomicI32::from_ptr((&raw const b.__bi_remaining.counter).cast_mut()) };
        write!(out, "bi_remaining:\t{}\n", remaining.load(Relaxed) as u32);
        write!(out, "bi_end_io:\t");
        prt_fn_name(out, b.bi_end_io.map_or(0, |f| f as usize as c_ulong));
        out.newline();

        write!(out, "bi_vcnt\t{}\n", self.nr_vecs());
        write!(out, "bi_max_vecs\t{}\n", self.max_vecs());
    }
}

/// # Safety
/// BIO_OFFSET is the offset of a Bio<Owns> field.
pub unsafe trait HasBio: Sized {
    type Owns: BioOwns;
    const BIO_OFFSET: usize;

    fn bio(&self) -> &Bio {
        unsafe { Bio::from_raw(bio_of(core::ptr::from_ref(self).cast_mut())) }
    }

    fn bio_mut(self: Pin<&mut Self>) -> &mut Bio {
        unsafe { Bio::from_raw_mut(bio_of(self.get_unchecked_mut())) }
    }
}

// SAFETY: a bio, at its own start
unsafe impl<B: BioOwns> HasBio for Bio<B> {
    type Owns = B;
    const BIO_OFFSET: usize = 0;
}

/// A struct's bio field: a Bio<Owns>, or a BioLend, its bio AT into it.
///
/// # Safety
/// A Bio<Owns> is AT bytes into the field, and ends it.
pub unsafe trait BioField {
    type Owns: BioOwns;
    const AT: usize = 0;
}
unsafe impl<B: BioOwns> BioField for Bio<B> {
    type Owns = B;
}

#[doc(hidden)]
pub const fn bio_field_at<F: BioField<Owns = O>, O: BioOwns>(_: *const F) -> usize {
    F::AT
}

/// HasBio for $t, by its bio field $f, which owns $o - Data, if it's not
/// given: checked to be a BioField that does - not that it's #[pin], which
/// HasBio asks of a struct with #[pin_data].
#[macro_export]
macro_rules! has_bio {
    ($t:ty, $($f:ident).+ $(, owns $o:ty)? $(where $($g:ident: $b:path),*)?) => {
        unsafe impl<$($($g: $b),*)?> $crate::os::bio::HasBio for $t {
            type Owns = $crate::has_bio!(@owns $($o)?);
            const BIO_OFFSET: usize = {
                let t = ::core::mem::MaybeUninit::<$t>::uninit();
                let field = unsafe { &raw const (*t.as_ptr()).$($f).+ };
                ::core::mem::offset_of!($t, $($f).+) + $crate::os::bio::bio_field_at::<_, Self::Owns>(field)
            };
        }
    };
    (@owns) => { $crate::os::bio::Data };
    (@owns $o:ty) => { $o };
}

fn bio_of<T: HasBio>(t: *mut T) -> *mut c::bio {
    t.cast::<u8>().wrapping_add(T::BIO_OFFSET).cast()
}

/// # Safety
/// @bio is a T's.
pub unsafe fn of_bio<T: HasBio>(bio: *mut c::bio) -> *mut T {
    // SAFETY: the caller's
    unsafe { bio.byte_sub(T::BIO_OFFSET).cast() }
}


/// What has a bio of its own to submit, as the erased Bio: a read's fragment,
/// whichever of its bios it has, and every BioOwner.
pub trait BioSubmit {
    fn bio(&self) -> &Bio;

    fn bio_mut(&mut self) -> &mut Bio;

    // TODO: once submitted, a bio's bi_iter is undefined - the block layer
    // advances and splits it - till its owner sets it again, and nothing
    // stops a read of it in between. A position after the IO is one saved
    // before it, or the owner's whole table; an iter is never walked back.
    // A typestate parameter on Bio, the owners generic over it, would make
    // it static - if one comes for something else.
    /// submit(), waited on by this thread, asleep as IO wait: its status.
    fn submit_wait(&mut self, bdev: &BlockDevice, sector: u64) -> Result<(), BchError> {
        self.submit(bdev, sector, waiter::Sync::current()).wait_sync(Sleep::UNINTERRUPTIBLE.io())
    }

    /// Submitted to @bdev at @sector, its IO's Completion made here for
    /// @waiter: the endio's end in bi_private, the waiter's the BioFuture -
    /// awaited or waited on, now or later, for the IO's status. The device is
    /// borrowed as long as the bio is.
    fn submit<'a, W: Waiter>(&'a mut self, bdev: &'a BlockDevice, sector: u64, waiter: W) -> BioFuture<'a, W> {
        let bio = self.bio_mut();
        let raw = bio.aim(bdev, sector);
        unsafe {
            let c = Completion::init_at((&raw mut (*raw).bi_private).cast(), waiter);
            (*raw).bi_end_io = Some(done_endio::<W>);
            crate::os::blkdev::submit_bio(raw);
            BioFuture { c, bio }
        }
    }
}

/// An owner of a bio, and of what it owns: its buffers, Owns, to reset for
/// another IO - or, if it owns none, to remap.
///
/// # Safety
/// Owns is what its bio owns.
pub unsafe trait BioOwner: BioSubmit {
    type Owns: BioOwns;

    /// For another IO, of @opf, to or from the same buffers: Bio::reset().
    /// Panics if they can't serve it - a buffer borrowed shared, for a read.
    fn reset(&mut self, opf: u32) {
        assert!(Self::Owns::allows_op(req_op(opf)), "resetting a bio for an op its buffers can't serve");
        self.bio_mut().reset(opf)
    }

    /// Panics on a clone.
    fn range_all(&self) -> BioRange<'_> {
        self.bio().range_all()
    }

    /// The bio mapped to @buf for one IO, in place of what it was: for a bio
    /// whose buffer isn't one for its life - the superblock's, which serves its
    /// buffer and the read-back's; node scan's, whose node's buffer read_done
    /// swaps. A bio's buffer is otherwise the one it's made with,
    /// a Buffer. The bvecs are the owner's to replace: its bio owns nothing
    /// there. Panics if the bio isn't a read; fails as map() does, the bio
    /// left with no bvecs.
    fn remap_read<'a>(&'a mut self, buf: &'a mut [u8]) -> Result<Remapped<'a, Self, &'a mut [u8]>, BchError>
    where
        Self: Sized + BioOwner<Owns = Data>,
    {
        assert_eq!(self.bio().op(), c::req_op_REQ_OP_READ, "remap_read() of a bio for another op");
        self.bio_mut().truncate_vecs(0);
        // SAFETY: @buf's borrowed as long as the mapping is
        unsafe { self.bio_mut().map(buf.as_mut_ptr(), buf.len())? };
        Ok(Remapped { owner: self, _buf: PhantomData })
    }

    /// Panics if the bio isn't a write; fails as remap_read() does.
    fn remap_write<'a>(&'a mut self, buf: &'a [u8]) -> Result<Remapped<'a, Self, &'a [u8]>, BchError>
    where
        Self: Sized + BioOwner<Owns = Data>,
    {
        assert_eq!(self.bio().op(), c::req_op_REQ_OP_WRITE, "remap_write() of a bio for another op");
        self.bio_mut().truncate_vecs(0);
        unsafe { self.bio_mut().map(buf.as_ptr().cast_mut(), buf.len())? };
        Ok(Remapped { owner: self, _buf: PhantomData })
    }
}

/// An owner's bio remapped to a buffer it borrows: B is the borrow, &mut [u8]
/// for a read and & for a write. Dropping it empties the bvecs, so none
/// outlives the borrow.
pub struct Remapped<'a, O: BioOwner, B> {
    owner: &'a mut O,
    _buf:  PhantomData<B>,
}

impl<O: BioOwner, B> Remapped<'_, O, B> {
    /// Its bio, mapped to the buffer: for clones of it, borrowing it no longer
    /// than the mapping.
    pub fn bio(&self) -> &Bio {
        self.owner.bio()
    }

    pub fn submit_wait(&mut self, bdev: &BlockDevice, sector: u64) -> Result<(), BchError> {
        self.owner.submit_wait(bdev, sector)
    }

    pub fn submit<'b, W: Waiter>(&'b mut self, bdev: &'b BlockDevice, sector: u64, waiter: W) -> BioFuture<'b, W> {
        self.owner.submit(bdev, sector, waiter)
    }

    /// Submitted to @bdev at @sector, @r put when it completes: the bio, its
    /// buffer and the device borrowed for @r's Gather, 'g, which is gone only
    /// once the IO is done. Its endio empties the bvecs, as a drop does.
    pub fn submit_gathered<'g>(self, bdev: &'g BlockDevice, sector: u64, r: GatherRef<'g, (), waiter::Sync>)
    where
        Self: 'g,
    {
        let mut this = core::mem::ManuallyDrop::new(self);
        let bio = this.owner.bio_mut().aim(bdev, sector);
        // SAFETY: the owner's, built for this IO, and borrowed by it till its
        // Gather's gone
        unsafe {
            (*bio).bi_private = r.into_raw().cast_mut().cast();
            (*bio).bi_end_io = Some(mapped_gathered_endio);
            crate::os::blkdev::submit_bio(bio);
        }
    }
}

/// A bio submitted: the Completion of its IO, and the bio, held exclusively -
/// the block layer and the endio write it till the IO's done - for its status
/// once it is. Dropped, it waits for the IO.
#[must_use = "a bio's IO is waited on, or awaited, to its end"]
pub struct BioFuture<'a, W: Waiter> {
    c:   Completion<'a, W>,
    bio: &'a mut Bio,
}

// the Bio's Send, and read only once it's done
unsafe impl Send for BioFuture<'_, waiter::Async> {}

impl WaitSync for BioFuture<'_, waiter::Sync> {
    type Output = Result<(), BchError>;

    fn wait_sync(self, how: Sleep) -> Result<(), BchError> {
        let BioFuture { c, bio } = self;
        c.wait_sync(how);
        bio.status()
    }
}

impl Resumable for BioFuture<'_, waiter::Async> {
    type Output = Result<(), BchError>;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.c).poll_resume(cx));
        Poll::Ready(this.bio.status())
    }
}

unsafe extern "C" fn done_endio<W: Waiter>(bio: *mut c::bio) {
    unsafe { Done::<W>::from_ptr((&raw mut (*bio).bi_private).cast()).complete() };
}

/// What a bio's T does as its IO completes, in its endio, before its waiter
/// has it: only what must see the completion as it happens - a device's
/// latency. The rest is its waiter's.
pub trait AtEnd: HasBio {
    fn at_end(&self);
}

unsafe extern "C" fn at_end_endio<T: AtEnd, W: Waiter>(bio: *mut c::bio) {
    unsafe {
        (*of_bio::<T>(bio)).at_end();
        Done::<W>::from_ptr((&raw mut (*bio).bi_private).cast()).complete()
    }
}

unsafe extern "C" fn mapped_gathered_endio(bio: *mut c::bio) {
    // SAFETY: submit_gathered()'s, whose ref is bi_private; the IO's, done
    unsafe {
        let r = GatherRef::<(), waiter::Sync>::from_raw((*bio).bi_private.cast());
        Bio::from_raw_mut(bio).truncate_vecs(0);
        drop(r)
    }
}

impl<O: BioOwner, B> Drop for Remapped<'_, O, B> {
    fn drop(&mut self) {
        self.owner.bio_mut().truncate_vecs(0);
    }
}

