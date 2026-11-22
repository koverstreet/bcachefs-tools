// SPDX-License-Identifier: GPL-2.0

//! struct bio, from Rust. The terms, used bare here and in iter.rs and
//! owned.rs:
//!
//! - A bio describes one block IO: device, op, sector, a position in its
//!   data, and its bvecs - (page, offset, len), in storage of its own: its
//!   first few inline, more from a BvecMempool - or borrowed, a clone's
//!   parent's. Bio is a view of one; its c::bio is private, read through
//!   raw().
//! - Its data is what the bvecs point at. A position is a bvec_iter: a bvec,
//!   an offset into it, and the bytes left. The bio's own is bi_iter, which
//!   the block layer advances as the IO completes. A BioRange is a position
//!   and its bio; walking one gives chunks, virtually contiguous pieces of the
//!   data, or segments, the bvecs as they are, or folios (iter.rs).
//! - Its owner is what allocated it and holds it (owned.rs): a BioBox, from
//!   a BioSet or a struct and its inline bvecs in one allocation. Only the
//!   owner builds a bio, submits it, and walks all of its
//!   bvecs wherever its position is - range_all(), C's _all walks. Anything
//!   with a &Bio may read it, and walk its data from a position.
//! - Mapping a buffer adds it to the bvecs, until they're emptied: reset()
//!   keeps them, for another IO of the same buffers. A buffer's mapped as the
//!   bio's made, and the bio holds it (Buffer); one whose owner puts nothing
//!   in its bvecs is remapped (BioOwner::remap_read()).
//! - In flight: submitted, until its endio. A gathered submit consumes the
//!   box, and its endio drops it (BioBox::submit_gathered()); submit()
//!   borrows it for its BioFuture, which yields the status; submit_owned()
//!   consumes it for its Submitted, which gives it back. Either way nothing
//!   reaches a bio in flight - but submit_gathered(), the superblock's, which
//!   is why it's unsafe. Those states are what make reading a bio's fields
//!   through &Bio safe.
//! - A value: it moves as one, its bvecs with it, and is changed through
//!   &mut Bio - its own methods, given one by its owner's bio_mut(). Its C
//!   view, bi_io_vec, is pointed at its bvecs as it goes out to C.
//! - A Bio<B> owns what its bvecs point at as B says (BioOwns): nothing,
//!   Data - a clone's are its source's (BioInPlace::clone_of()); pages of its
//!   own, a mapped buffer (buffers.rs), filled as the bio's made and released
//!   as it's dropped; or folios (bvec.rs), Holds<H>, its bvecs Bvec<H>s, a
//!   range of a folio and what it holds of it, pushed and dropped as a Vec's
//!   elements are. It's lent as the plain Bio, Bio<Data>, which can't add or
//!   free them.
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
use core::sync::atomic::{AtomicI32, Ordering::Relaxed};
use core::task::{ready, Context, Poll};

use crate::c;
use crate::data_structures::inline_vec::InlineVec;
use crate::errcode::BchError;
use crate::util::backtrace::prt_symbol;
use crate::util::printbuf::prt_bdevname;
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
pub use iter::{bvec_offset, reset_bvec_offset, BioRange, Chunk, Chunks, Mapped, Segment, Segments, Walk};
pub use buffers::{io_buf, Buffer, IoBuf, MapBuf, Pages};
#[cfg(kernel)]
pub use bvec::{Bvec, FolioHold, Holds};
pub use owned::{bio_put, BioBox, BioInPlace, BioLend, BioPool, BioSet, BvecAlloc, BvecMempool, Gathered, Lent,
                Submitted};

fn req_op(opf: u32) -> u32 {
    opf & ((1 << c::REQ_OP_BITS) - 1)
}

/// How many bvecs mapping @len bytes at @p takes: a page each for vmalloc
/// memory, one for the direct map.
pub fn buf_nr_bvecs(p: *const u8, len: usize) -> usize {
    use crate::os::kernel::{is_vmalloc_addr, PAGE_SIZE};
    if is_vmalloc_addr(p) {
        return (len + (p as usize & (PAGE_SIZE - 1))).div_ceil(PAGE_SIZE);
    }
    1
}

/// How much of @len bytes at @p one bio maps, in at most BIO_MAX_VECS bvecs:
/// all of them, but vmalloc memory spanning more pages - where @p is in its
/// first page says how many fit.
pub fn buf_bio_bytes(p: *const u8, len: usize) -> usize {
    use crate::os::kernel::{is_vmalloc_addr, PAGE_SIZE};
    if is_vmalloc_addr(p) {
        return len.min(c::BIO_MAX_VECS as usize * PAGE_SIZE - (p as usize & (PAGE_SIZE - 1)));
    }
    len
}

pub const BIO_INLINE_VECS: usize = 4;

/// What a bio owns of what its bvecs point at: its bvecs, B::Bvec, and itself
/// a value the bio holds - what giving them back needs besides them: the
/// bounce pool, a mapped buffer. Got as the bio's made, by
/// BioInPlace::init_from(): a bio's buffers are its contents, and live as
/// long as it does.
pub trait BioOwns: Sized {
    type Bvec;

    /// As the bio's dropped, its bvecs still there - done with, and not in
    /// flight: what dropping them doesn't give back.
    fn release(&mut self, _bio: &mut c::bio) {}

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

/// A bio's bvecs: the storage its bi_io_vec points into, its first few
/// inline. How many are in use is its bi_vcnt, which C's fillers keep - or a
/// Bio<Holds<H>>'s, its length.
type BioVecs<B = Data> = InlineVec<<B as BioOwns>::Bvec, BIO_INLINE_VECS, u16, BvecAlloc>;

/// Where a bio's bvec table is: its own BioVecs - which move with it, so
/// bi_io_vec is pointed at them again as the bio goes out to C - or not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VecsAt {
    Own,
    /// A clone's parent's, or a table its owner keeps - init_borrowed()'s
    Borrowed,
    /// An ITER_BVEC's, which pinning its pages made the bio's
    Iter,
}

// XXX: a Bio moves, and is swapped, as any Rust value - but a clone's bvecs
// are its source's, borrowed for its box's 'a, not by the Bio: swap two boxes'
// bios and a clone outlives its source.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Bio<B: BioOwns = Data> {
    bio:     c::bio,
    vecs:    BioVecs<B>,
    vecs_at: VecsAt,
    #[pin]
    data:    BioData,
    /* last: the plain Bio is every Bio<B>'s first part */
    owns:    B,
}

/// What it owns given back, bio_uninit(), and its bvecs freed: emptied first,
/// as a bioset's bio is dropped again by bio_put() when what it's in fails to
/// init - which frees nothing twice. What it owns goes after: a mapped buffer
/// outlives the bio.
#[pinned_drop]
impl<B: BioOwns> PinnedDrop for Bio<B> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.get_mut();
        if this.vecs_at == VecsAt::Own {
            bi_io_vec!(this.bio) = this.vecs.as_mut_ptr().cast();
        }
        this.owns.release(&mut this.bio);
        unsafe { crate::os::bio::uninit(&mut this.bio) };
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
// of two &mut Bio - and its drop then releases what B never held. And Bio
// still has map(), truncate_vecs() and pin_user_pages(), for the owners that
// aren't typed yet - each goes with its last; till then a Bio<B> could gain
// bvecs that aren't B's through them.
impl<B: Typed> DerefMut for Bio<B> {
    fn deref_mut(&mut self) -> &mut Bio {
        let () = Self::ERASES;
        unsafe { &mut *core::ptr::from_mut(self).cast() }
    }
}

impl<B: BioOwns> Bio<B> {
    /// What it owns besides its bvecs.
    pub fn owns(&self) -> &B {
        &self.owns
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

// BioData's place in the plain Bio, which is what a BioRange's bio is: what
// #[derive(Init)] would make, by hand - a Bio<B> is made in place by
// bio_init(), and the derive's init() would be generic over B.
impl crate::util::place::Field<{ crate::util::place::name("data") }> for Bio {
    type Type = BioData;
    const OFFSET: usize = core::mem::offset_of!(Bio, data);
}

impl crate::util::place::Enclosing for Bio {
    type Visible = Self;
    const VISIBLE_OFFSET: usize = 0;
}

impl Bio {
    /// # Safety
    /// @ptr is a bio, valid for 'a, and not in flight.
    pub unsafe fn from_raw<'a>(ptr: *const c::bio) -> &'a Self {
        // SAFETY: the caller's; Bio is a transparent c::bio
        unsafe { &*ptr.cast() }
    }

    /// # Safety
    /// As from_raw()'s, and nothing else has it for 'a.
    pub unsafe fn from_raw_mut<'a>(ptr: *mut c::bio) -> &'a mut Self {
        unsafe { &mut *ptr.cast() }
    }

    /// bio_init() of a Bio at @bio whose bvecs are @table's, which it doesn't
    /// own: a data update's read and write bios share theirs.
    ///
    /// # Safety
    /// @bio is room for a Bio, not in flight; @table has room for @max_vecs,
    /// and outlives the bio.
    pub unsafe fn init_borrowed(bio: *mut Bio, bdev: *mut crate::os::blkdev::BlockDevice, table: *mut c::bio_vec,
                                max_vecs: u16, opf: u32) {
        unsafe {
            (&raw mut (*bio).vecs).write(BioVecs::<Data>::new_in(BvecAlloc::NONE));
            (&raw mut (*bio).vecs_at).write(VecsAt::Borrowed);
            crate::os::bio::init(bio.cast(), bdev, table, max_vecs, opf);
        }
    }

    pub fn raw(&self) -> &c::bio {
        &self.bio
    }

    pub fn nr_vecs(&self) -> u16 {
        self.bio.bi_vcnt
    }

    pub fn max_vecs(&self) -> u16 {
        self.bio.bi_max_vecs
    }

    /// For its own methods, and C's: bi_io_vec pointed at its bvecs, where
    /// they are now.
    pub(crate) fn raw_mut(&mut self) -> &mut c::bio {
        if self.vecs_at == VecsAt::Own {
            bi_io_vec!(self.bio) = self.vecs.as_mut_ptr();
        }
        &mut self.bio
    }

    /// The bio as C has it, for its IO: a pointer derived from this borrow,
    /// which its holder keeps till the block layer's done with it.
    pub(crate) fn to_raw(&mut self) -> *mut c::bio {
        self.raw_mut()
    }

    fn vecs_ptr(&self) -> *const c::bio_vec {
        match self.vecs_at {
            VecsAt::Own => self.vecs.as_ptr(),
            _           => self.table(),
        }
    }

    /// bi_io_vec: from 7.4 a union's, so unsafe to read, for a bio no dma-buf
    /// has - none of ours.
    #[allow(unused_unsafe)]
    fn table(&self) -> *mut c::bio_vec {
        unsafe { bi_io_vec!(self.bio) }
    }

    /// After C has filled it: if C took a table of its own for it - an
    /// ITER_BVEC's, pinning its pages - that's its table till it's cleared.
    #[cfg(kernel)]
    pub(crate) fn raw_filled(&mut self) {
        if self.vecs_at == VecsAt::Own && self.table() != self.vecs.as_mut_ptr() {
            self.vecs_at = VecsAt::Iter;
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
        // SAFETY: its own bvecs, bi_vcnt of them
        let size = (0..u32::from(self.bio.bi_vcnt)).map(|i| unsafe { self.bvec(i) }.bv_len).sum();
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

    pub(crate) fn op(&self) -> u32 {
        req_op(self.bio.bi_opf)
    }

    /// @len bytes of @buf mapped. Panics if they don't fit, where
    /// __bio_add_page() would write past the bvecs. For a buffer kept by other
    /// means than the bio - a stripe buffer's blocks, kept by the IO's round;
    /// a Buffer holds it.
    ///
    /// # Safety
    /// @buf is the allocators' - in the direct map, or vmalloc - and outlives
    /// its place in the bvecs; nothing has the bio in flight.
    pub unsafe fn map(&mut self, buf: *mut u8, len: usize) {
        let len = u32::try_from(len).expect("mapping a buffer of 4G or more into a bio");
        let bio = self.raw_mut();
        #[cfg(kernel)]
        // SAFETY: the caller's
        if unsafe { c::is_vmalloc_addr(buf.cast()) } {
            assert!(unsafe { c::bio_add_vmalloc(bio, buf.cast(), len) },
                    "mapping {len} bytes of vmalloc memory: more pages than the bio's bvecs");
            return;
        }
        assert!(bio.bi_vcnt < bio.bi_max_vecs, "mapping {len} bytes: the bio's bvecs are full");
        // SAFETY: the caller's, and a bvec free
        unsafe { crate::os::bio::add_virt_nofail(bio, buf.cast(), len) };
    }

    /// For another IO: bio_reset(), its bvecs kept - the buffers it was made
    /// with - and its position all of them again, from the table: after an IO
    /// bi_iter is the block layer's. Panics on a clone, whose bvecs aren't its
    /// own. BioOwner::reset(), which checks the buffers serve @opf.
    pub(super) fn reset(&mut self, bdev: *mut crate::os::blkdev::BlockDevice, opf: u32) {
        let size = self.range_all().len();
        let bio = self.raw_mut();
        let vcnt = bio.bi_vcnt;
        // SAFETY: had by &mut, so not in flight
        unsafe { crate::os::bio::reset(bio, bdev, opf) };
        bio.bi_vcnt = vcnt;
        bio.bi_iter.bi_size = size;
    }

    /// Panics if @range isn't this bio's.
    pub fn set_range(&mut self, range: BioRange<'_>) {
        assert!(range.is_of(self), "a bio set to another bio's range");
        self.raw_mut().bi_iter = range.pos();
    }

    /// Its position @bytes on. Panics past its end.
    pub fn advance(&mut self, bytes: u32) {
        let mut r = self.range();
        r.advance(bytes);
        let pos = r.pos();
        self.raw_mut().bi_iter = pos;
    }

    /// Panics past its end.
    pub fn shrink(&mut self, bytes: u32) {
        let iter = &mut self.raw_mut().bi_iter;
        assert!(bytes <= iter.bi_size, "shrinking a bio of {} bytes to {bytes}", iter.bi_size);
        iter.bi_size = bytes;
    }

    /// A random u64 of its data overwritten: the debug corrupt ratios'.
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub fn corrupt(&mut self) {
        use crate::os::kernel::{random_u64, random_u64_below};
        let u64s = self.bio.bi_iter.bi_size / 8;
        if u64s == 0 {
            return;
        }
        let mut r = self.range();
        r.advance(random_u64_below(u64s.into()) as u32 * 8);
        r.take(8).copy_from(&random_u64().to_ne_bytes());
    }

    pub fn set_dev(&mut self, bdev: *mut crate::os::blkdev::BlockDevice) {
        crate::os::blkdev::bio_set_dev(self.raw_mut(), bdev)
    }

    /// # Safety
    /// @iter is a position of its bvecs.
    pub unsafe fn set_iter(&mut self, iter: c::bvec_iter) {
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

    pub fn set_sector(&mut self, sector: u64) {
        self.raw_mut().bi_iter.bi_sector = sector;
    }

    /// Its size, for a bio with no data to size it by: a discard's.
    pub fn set_size(&mut self, bytes: u32) {
        self.raw_mut().bi_iter.bi_size = bytes;
    }

    /// Its bvecs after the first @nr dropped, and its position with them -
    /// their pages given back by whoever has them.
    pub fn truncate_vecs(&mut self, nr: u16) {
        let bio = self.raw_mut();
        assert!(nr <= bio.bi_vcnt, "truncating a bio to more bvecs than it has");
        bio.bi_vcnt = nr;
        bio.bi_iter.bi_size = 0;
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

    /// # Safety
    /// @idx is one of its bvecs - bi_vcnt of them, or its parent's, a clone's.
    unsafe fn bvec(&self, idx: u32) -> c::bio_vec {
        // SAFETY: the caller's
        unsafe { *self.vecs_ptr().add(idx as usize) }
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
        prt_bdevname(out, unsafe { crate::os::blkdev::bio_bdev(b) });
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
        prt_symbol(out, b.bi_end_io.map_or(0, |f| f as usize as c_ulong));
        out.newline();

        write!(out, "bi_vcnt\t{}\n", b.bi_vcnt);
        write!(out, "bi_max_vecs\t{}\n", b.bi_max_vecs);
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

/// A struct's bio field: a Bio<Owns>, or a C-shared struct's c::bio - the same
/// memory, Bio being transparent - or a BioLend, its bio AT into it.
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
unsafe impl BioField for c::bio {
    type Owns = Data;
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
        unsafe impl<$($($g: $b),*)?> $crate::util::bio::HasBio for $t {
            type Owns = $crate::has_bio!(@owns $($o)?);
            const BIO_OFFSET: usize = {
                let t = ::core::mem::MaybeUninit::<$t>::uninit();
                let field = unsafe { &raw const (*t.as_ptr()).$($f).+ };
                ::core::mem::offset_of!($t, $($f).+) + $crate::util::bio::bio_field_at::<_, Self::Owns>(field)
            };
        }
    };
    (@owns) => { $crate::util::bio::Data };
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


/// # Safety
/// bio() is the implementor's own, which nothing else has - in flight, or
/// to build on - while it's had.
pub unsafe trait BioOwner {
    /// What its bio owns: its B.
    type Owns: BioOwns;

    fn bio(&self) -> &Bio;

    fn bio_mut(&mut self) -> &mut Bio;

    /// For another IO, of @opf, to or from the same buffers: Bio::reset().
    /// Panics if they can't serve it - a buffer borrowed shared, for a read.
    fn reset(&mut self, bdev: *mut crate::os::blkdev::BlockDevice, opf: u32) {
        assert!(Self::Owns::allows_op(req_op(opf)), "resetting a bio for an op its buffers can't serve");
        self.bio_mut().reset(bdev, opf)
    }

    /// Panics on a clone.
    fn range_all(&self) -> BioRange<'_> {
        self.bio().range_all()
    }

    // TODO: once submitted, a bio's bi_iter is undefined - the block layer
    // advances and splits it - till its owner sets it again, and nothing
    // stops a read of it in between. A position after the IO is one saved
    // before it, or the owner's whole table; an iter is never walked back.
    // A typestate parameter on Bio, the owners generic over it, would make
    // it static - if one comes for something else.
    /// submit(), waited on by this thread, asleep as IO wait: its status.
    fn submit_wait(&mut self) -> Result<(), BchError> {
        self.submit(waiter::Sync::current()).wait_sync(Sleep::UNINTERRUPTIBLE.io())
    }

    /// Submitted, its IO's Completion made here for @waiter: the endio's end in
    /// bi_private, the waiter's the BioFuture - awaited or waited on, now or
    /// later, for the IO's status.
    fn submit<W: Waiter>(&mut self, waiter: W) -> BioFuture<'_, W> {
        let bio = self.bio_mut();
        let raw = bio.to_raw();
        unsafe {
            let c = Completion::init_at((&raw mut (*raw).bi_private).cast(), waiter);
            (*raw).bi_end_io = Some(done_endio::<W>);
            crate::os::blkdev::submit_bio(raw);
            BioFuture { c, bio }
        }
    }

    /// The bio mapped to @buf for one IO, in place of what it was: for a bio
    /// whose buffer isn't one for its life - the superblock's, which serves its
    /// buffer and the read-back's; node scan's, whose node's buffer read_done
    /// swaps. A bio's buffer is otherwise the one it's made with,
    /// a Buffer. The bvecs are the owner's to replace: its bio owns nothing
    /// there. Panics if the bio isn't a read, or hasn't the bvecs for @buf.
    fn remap_read<'a>(&'a mut self, buf: &'a mut [u8]) -> Remapped<'a, Self, &'a mut [u8]>
    where
        Self: Sized + BioOwner<Owns = Data>,
    {
        assert_eq!(self.bio().op(), c::req_op_REQ_OP_READ, "remap_read() of a bio for another op");
        self.bio_mut().truncate_vecs(0);
        // SAFETY: @buf's borrowed as long as the mapping is
        unsafe { self.bio_mut().map(buf.as_mut_ptr(), buf.len()) };
        Remapped { owner: self, _buf: PhantomData }
    }

    /// Panics if the bio isn't a write, or hasn't the bvecs for @buf.
    fn remap_write<'a>(&'a mut self, buf: &'a [u8]) -> Remapped<'a, Self, &'a [u8]>
    where
        Self: Sized + BioOwner<Owns = Data>,
    {
        assert_eq!(self.bio().op(), c::req_op_REQ_OP_WRITE, "remap_write() of a bio for another op");
        self.bio_mut().truncate_vecs(0);
        unsafe { self.bio_mut().map(buf.as_ptr().cast_mut(), buf.len()) };
        Remapped { owner: self, _buf: PhantomData }
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
    pub fn submit_wait(&mut self) -> Result<(), BchError> {
        self.owner.submit_wait()
    }

    pub fn submit<W: Waiter>(&mut self, waiter: W) -> BioFuture<'_, W> {
        self.owner.submit(waiter)
    }

    /// Submitted, @r put when it completes. The mapping stays until the bio's
    /// next one.
    ///
    /// sb/io's alone: the bio isn't given up, as an async submit's is, so
    /// it's reachable in flight. It goes once the superblock's IO consumes
    /// its bio.
    ///
    /// # Safety
    /// Nothing touches the bio, or the buffer it's mapped to, until @r's
    /// Gather has been waited on; nor walks or submits the bio until its next
    /// reset().
    pub unsafe fn submit_gathered(self, r: GatherRef<(), waiter::Sync>) {
        let mut this = core::mem::ManuallyDrop::new(self);
        let bio = this.owner.bio_mut().to_raw();
        // SAFETY: the owner's, built for this IO; the caller keeps it, and
        // the buffer, from being touched until it's done
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
    // SAFETY: submit_gathered()'s, whose ref is bi_private
    drop(unsafe { GatherRef::<(), waiter::Sync>::from_raw((*bio).bi_private.cast()) })
}

impl<O: BioOwner, B> Drop for Remapped<'_, O, B> {
    fn drop(&mut self) {
        self.owner.bio_mut().truncate_vecs(0);
    }
}

