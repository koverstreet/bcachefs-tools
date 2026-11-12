// SPDX-License-Identifier: GPL-2.0

//! struct bio, from Rust. The terms, used bare here and in iter.rs and
//! owned.rs:
//!
//! - A bio describes one block IO: device, op, sector, a position in its
//!   data, and its bvecs - (page, offset, len), in an array that may be
//!   inline, right after the bio in its allocation. Bio is a view of one; its
//!   c::bio is private, read through raw().
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
//!   bio's made, and the bio holds it (BioBuffersMapped); one whose owner
//!   puts nothing in its bvecs is remapped (BioOwner::remap_read()).
//! - In flight: submitted, until its endio. A gathered submit consumes the
//!   box, and its endio drops it (BioBox::submit_gathered()); or submit()
//!   borrows it for its BioFuture, which yields the status. Either way nothing
//!   reaches a bio in flight - but submit_gathered(), the superblock's, which
//!   is why it's unsafe. Those states are what make reading a bio's fields
//!   through &Bio safe.
//! - Pinned: inline bvecs make a bio self-referential, so it's never moved,
//!   and changed only through Pin<&mut Bio> - its own methods, given one by
//!   its owner's bio_mut().
//!
//! The generics, below, are those terms as traits. A bio is the last field
//! of a struct of its user's - an rbio, a write op - and HasBio is that
//! struct, found from its bio by the offset has_bio!() checks; BioField is
//! what the field may be. BioOwner is whatever holds a bio to build and
//! submit; Gathered, what an owner's T puts to the Gather it was submitted
//! to when its IO completes. The owner itself, BioBox, is owned.rs's,
//! generic over Buffers too (buffers.rs): what its bvecs point at - its own,
//! or a clone's source's - filled as the bio's made and released as it's
//! dropped.
//!
//! After Andreas Hindborg's kernel::block::bio, from his rnull series
//! (git.kernel.org a.hindborg/linux.git, rnull-v6.19-rc5, 41e04e1bb2e6): the
//! kernel crate vendored here hasn't got it, and couldn't serve userspace. So
//! it's over c::bio here, in both builds - the shim's bio_vec laid out as the
//! kernel's.

use core::ffi::{c_ulong, c_void};
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::sync::atomic::{AtomicI32, Ordering::Relaxed};
use core::task::{ready, Context, Poll};

use crate::c;
use crate::errcode::BchError;
use crate::util::backtrace::prt_symbol;
use crate::util::printbuf::prt_bdevname;
use crate::util::sleep::Sleep;
use crate::util::task::{waiter, Completion, Done, Waiter};
use crate::util::gather::GatherRef;
use crate::util::task::Resumable;
use crate::util::{Printbuf, ToText};
use cstruct_macros::{c_export, Init};
use pin_init::pin_data;

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
mod iter;
mod owned;
pub mod pages;
#[cfg(kernel)]
mod user_pages;

/// @f with the block layer's plug held: the bios it submits held back to be
/// merged, then issued together - blk_start_plug() and blk_finish_plug(). The
/// plug's the task's, so it can't move while it's held.
#[cfg(kernel)]
pub fn plugged<R>(f: impl FnOnce() -> R) -> R {
    let mut plug = core::mem::MaybeUninit::<c::blk_plug>::uninit();
    unsafe { c::blk_start_plug(plug.as_mut_ptr()) };
    let ret = f();
    unsafe { c::blk_finish_plug(plug.as_mut_ptr()) };
    ret
}

/* Userspace's block layer has no plug: */
#[cfg(not(kernel))]
pub fn plugged<R>(f: impl FnOnce() -> R) -> R {
    f()
}

#[cfg(kernel)]
pub use iter::{FolioVec, Folios};
pub use iter::{bvec_offset, reset_bvec_offset, BioRange, Chunk, Chunks, Segment, Segments, Walk};
#[cfg(kernel)]
pub(crate) use iter::nth_page;
pub use buffers::{BioBuffersMapped, BioBuffersOwned, BioBuffersPages, BioBuffersUnmanaged, BioClone,
                  Buffers, MapBuf};
pub use owned::{inline_bio, inline_vecs, BioBox, BioInPlace, BioLend, BioSet, Either, FrontPad, Gathered, Lent};

fn req_op(opf: u32) -> u32 {
    opf & ((1 << c::REQ_OP_BITS) - 1)
}

/// How many bvecs mapping @len bytes at @p takes: a page each for vmalloc
/// memory, one for the direct map.
pub fn buf_nr_bvecs(p: *const u8, len: usize) -> usize {
    use crate::util::kernel::{is_vmalloc_addr, PAGE_SIZE};
    if is_vmalloc_addr(p) {
        return (len + (p as usize & (PAGE_SIZE - 1))).div_ceil(PAGE_SIZE);
    }
    1
}

#[pin_data]
#[repr(C)]
#[derive(Init)]
pub struct Bio {
    bio:  c::bio,
    #[pin]
    data: BioData,
    #[pin]
    _pin: PhantomData<PhantomPinned>,
}

// SAFETY: a bio is no thread's - submitted on one, completed on another. Not
// Sync: two holders of a &Bio could write its data at once.
unsafe impl Send for Bio {}

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
    /// @ptr is a bio, valid for 'a, and not in flight.
    pub unsafe fn from_raw<'a>(ptr: *const c::bio) -> &'a Self {
        // SAFETY: the caller's; Bio is a transparent c::bio
        unsafe { &*ptr.cast() }
    }

    /// # Safety
    /// As from_raw()'s, and nothing else has it for 'a.
    pub unsafe fn from_raw_mut<'a>(ptr: *mut c::bio) -> Pin<&'a mut Self> {
        // SAFETY: the caller's; a bio never moves
        unsafe { Pin::new_unchecked(&mut *ptr.cast()) }
    }

    pub fn raw(&self) -> &c::bio {
        &self.bio
    }

    /// For its own methods, and C's: never moved out of.
    fn raw_mut(self: Pin<&mut Self>) -> &mut c::bio {
        self.project().bio
    }

    /// bio_uninit(): an inline_bio()'s, as what holds it drops.
    ///
    /// # Safety
    /// Not used after.
    pub unsafe fn uninit(self: Pin<&mut Self>) {
        unsafe { c::bio_uninit(self.raw_mut()) }
    }

    pub fn range(&self) -> BioRange<'_> {
        BioRange { data: &self.data, iter: self.bio.bi_iter }
    }

    /// Its data from @pos: a position saved from it, its bi_iter before the IO
    /// advanced it or a range's pos().
    ///
    /// # Safety
    /// @pos is a position of this bio: within its bvecs, as many bytes of
    /// them after it as bi_size says.
    // XXX: unsafe, as a position carries nothing saying which bio it's of. A
    // lifetime tying them together may make this safe, once bcachefs walks its
    // bios this way throughout.
    pub unsafe fn range_at(&self, pos: c::bvec_iter) -> BioRange<'_> {
        BioRange { data: &self.data, iter: pos }
    }

    /// All its bvecs, wherever its position is: its owner's walk -
    /// BioOwner::range_all() - or, for C, a bio's whose ref we were given.
    /// Panics on a clone.
    fn range_all(&self) -> BioRange<'_> {
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

    fn op(&self) -> u32 {
        req_op(self.bio.bi_opf)
    }

    /// @len bytes of @buf mapped. Panics if they don't fit, where
    /// __bio_add_page() would write past the bvecs. For a buffer kept by other
    /// means than the bio - a stripe buffer's blocks, kept by the IO's round;
    /// BioBuffersMapped holds it.
    ///
    /// # Safety
    /// @buf is the allocators' - in the direct map, or vmalloc - and outlives
    /// its place in the bvecs; nothing has the bio in flight.
    pub unsafe fn map(self: Pin<&mut Self>, buf: *mut u8, len: usize) {
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
        unsafe { c::bio_add_virt_nofail(bio, buf.cast(), len) };
    }

    /// Panics if the bio's bvecs are full.
    ///
    /// # Safety
    /// @folio stays locked till the bio's done with it.
    #[cfg(kernel)]
    pub unsafe fn add_folio(self: Pin<&mut Self>, folio: crate::vfs::folio::Folio) {
        let bio = self.raw_mut();
        assert!(bio.bi_vcnt < bio.bi_max_vecs, "adding a folio: the bio's bvecs are full");
        unsafe { c::bio_add_folio_nofail(bio, folio.as_ptr(), folio.size(), 0) }
    }

    /// For another IO: bio_reset(), its bvecs kept - the buffers it was made
    /// with - and its position all of them again, from the table: after an IO
    /// bi_iter is the block layer's. Panics on a clone, whose bvecs aren't its
    /// own. BioOwner::reset(), which checks the buffers serve @opf.
    pub(super) fn reset(mut self: Pin<&mut Self>, bdev: *mut c::block_device, opf: u32) {
        let size = self.as_ref().range_all().len();
        let bio = self.as_mut().raw_mut();
        let vcnt = bio.bi_vcnt;
        // SAFETY: had by &mut, so not in flight
        unsafe { c::bio_reset(bio, bdev, opf as _) };
        bio.bi_vcnt = vcnt;
        bio.bi_iter.bi_size = size;
    }

    /// Panics if @range isn't this bio's.
    pub fn set_range(self: Pin<&mut Self>, range: BioRange<'_>) {
        assert!(range.is_of(&self), "a bio set to another bio's range");
        self.raw_mut().bi_iter = range.pos();
    }

    /// Panics past its end.
    pub fn shrink(self: Pin<&mut Self>, bytes: u32) {
        let iter = &mut self.raw_mut().bi_iter;
        assert!(bytes <= iter.bi_size, "shrinking a bio of {} bytes to {bytes}", iter.bi_size);
        iter.bi_size = bytes;
    }

    /// A random u64 of its data overwritten: the debug corrupt ratios'.
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub fn corrupt(self: Pin<&mut Self>) {
        use crate::util::kernel::{random_u64, random_u64_below};
        let u64s = self.bio.bi_iter.bi_size / 8;
        if u64s == 0 {
            return;
        }
        let mut r = self.range();
        r.advance(random_u64_below(u64s.into()) as u32 * 8);
        r.take(8).copy_from(&random_u64().to_ne_bytes());
    }

    pub fn set_dev(self: Pin<&mut Self>, bdev: *mut c::block_device) {
        #[cfg(kernel)]
        unsafe { c::bio_set_dev(self.raw_mut(), bdev) }
        #[cfg(not(kernel))]
        { self.raw_mut().bi_bdev = bdev }
    }

    /// # Safety
    /// @iter is a position of its bvecs.
    pub unsafe fn set_iter(self: Pin<&mut Self>, iter: c::bvec_iter) {
        self.raw_mut().bi_iter = iter;
    }

    pub fn set_opf(self: Pin<&mut Self>, opf: u32) {
        self.raw_mut().bi_opf = opf as _;
    }

    pub fn set_ioprio(self: Pin<&mut Self>, prio: u16) {
        self.raw_mut().bi_ioprio = prio;
    }

    pub fn set_status(self: Pin<&mut Self>, status: u8) {
        self.raw_mut().bi_status = status;
    }

    pub fn set_sector(self: Pin<&mut Self>, sector: u64) {
        self.raw_mut().bi_iter.bi_sector = sector;
    }

    /// Its size, for a bio with no data to size it by: a discard's.
    pub fn set_size(self: Pin<&mut Self>, bytes: u32) {
        self.raw_mut().bi_iter.bi_size = bytes;
    }

    /// Its bvecs after the first @nr dropped, and its position with them -
    /// their pages given back by whoever has them.
    pub fn truncate_vecs(self: Pin<&mut Self>, nr: u16) {
        let bio = self.raw_mut();
        assert!(nr <= bio.bi_vcnt, "truncating a bio to more bvecs than it has");
        bio.bi_vcnt = nr;
        bio.bi_iter.bi_size = 0;
    }

    /// Its completed IO's result: a BCH_ERR_BLK_STS_* errcode on failure.
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
        unsafe { *bi_io_vec!(self.bio).add(idx as usize) }
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
        prt_bdevname(out, unsafe { b.bi_bdev.as_ref() });
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
/// BIO_OFFSET is the offset of a Bio field that ends the struct, #[repr(C)]. If the
/// struct projects its pinning - #[pin_data] - that field is #[pin]: so
/// BioBox::as_mut()'s Pin<&mut Self> never hands it out unpinned, to be
/// moved off its bvecs.
pub unsafe trait HasBio: Sized {
    const BIO_OFFSET: usize;

    fn bio(&self) -> &Bio {
        unsafe { Bio::from_raw(bio_of(core::ptr::from_ref(self).cast_mut())) }
    }

    fn bio_mut(self: Pin<&mut Self>) -> Pin<&mut Bio> {
        unsafe { Bio::from_raw_mut(bio_of(self.get_unchecked_mut())) }
    }
}

// SAFETY: a bio, at its own start
unsafe impl HasBio for Bio {
    const BIO_OFFSET: usize = 0;
}

/// A struct's bio field: a Bio, or a C-shared struct's c::bio - the same
/// memory, Bio being transparent - or a BioLend, its bio AT into it.
///
/// # Safety
/// A bio is AT bytes into the field, and ends it.
pub unsafe trait BioField {
    const AT: usize = 0;
}
unsafe impl BioField for Bio {}
unsafe impl BioField for c::bio {}

#[doc(hidden)]
pub const fn bio_field_at<B: BioField>(_: *const B) -> usize {
    B::AT
}

/// HasBio for $t, by its bio field $f: checked to be a BioField, and its bio
/// last - not that it's #[pin], which HasBio asks of a struct with
/// #[pin_data].
#[macro_export]
macro_rules! has_bio {
    ($t:ty, $($f:ident).+) => {
        // SAFETY: a bio field, below, its bio ending the struct
        unsafe impl $crate::util::bio::HasBio for $t {
            const BIO_OFFSET: usize = {
                let t = ::core::mem::MaybeUninit::<$t>::uninit();
                let field = unsafe { &raw const (*t.as_ptr()).$($f).+ };
                let off = ::core::mem::offset_of!($t, $($f).+) + $crate::util::bio::bio_field_at(field);
                assert!(off + ::core::mem::size_of::<$crate::util::bio::Bio>() == ::core::mem::size_of::<$t>(),
                        concat!(stringify!($t), ": its bio isn't last"));
                off
            };
        }
    };
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
    type Buffers: Buffers;

    fn bio(&self) -> &Bio;

    fn bio_mut(&mut self) -> Pin<&mut Bio>;

    /// For another IO, of @opf, to or from the same buffers: Bio::reset().
    /// Panics if they can't serve it - a buffer borrowed shared, for a read.
    fn reset(&mut self, bdev: *mut c::block_device, opf: u32) {
        assert!(Self::Buffers::allows_op(req_op(opf)), "resetting a bio for an op its buffers can't serve");
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
        self.submit(waiter::Sync::current()).wait(Sleep::UNINTERRUPTIBLE.io())
    }

    /// Submitted, its IO's Completion made here for @waiter: the endio's end in
    /// bi_private, the waiter's the BioFuture - awaited or waited on, now or
    /// later, for the IO's status.
    fn submit<W: Waiter>(&mut self, waiter: W) -> BioFuture<'_, W> {
        let bio: *mut c::bio = self.bio_mut().raw_mut();
        unsafe {
            let c = Completion::init_at((&raw mut (*bio).bi_private).cast(), waiter);
            (*bio).bi_end_io = Some(done_endio::<W>);
            c::submit_bio(bio);
            BioFuture { c, bio: &*bio.cast::<Bio>() }
        }
    }

    /// The bio mapped to @buf for one IO, in place of what it was: for a bio
    /// whose buffer isn't one for its life - the superblock's, which serves its
    /// buffer and the read-back's; node scan's, whose node's buffer read_done
    /// swaps. A bio's buffer is otherwise the one it's made with,
    /// BioBuffersMapped. The bvecs are the owner's to replace: its Buffers put
    /// nothing there. Panics if the bio isn't a read, or hasn't the bvecs for
    /// @buf.
    fn remap_read<'a>(&'a mut self, buf: &'a mut [u8]) -> Remapped<'a, Self, &'a mut [u8]>
    where
        Self: Sized + BioOwner<Buffers = BioBuffersUnmanaged>,
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
        Self: Sized + BioOwner<Buffers = BioBuffersUnmanaged>,
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
        let bio = this.owner.bio_mut().raw_mut();
        bio.bi_private = r.into_raw().cast_mut().cast();
        bio.bi_end_io = Some(mapped_gathered_endio);
        // SAFETY: the owner's, built for this IO; the caller keeps it, and
        // the buffer, from being touched until it's done
        unsafe { c::submit_bio(bio) };
    }
}

/// A bio submitted: the Completion of its IO, and a ref on the bio for its
/// status once that's done. Dropped, it waits for the IO.
#[must_use = "a bio's IO is waited on, or its drop waits for it"]
pub struct BioFuture<'a, W: Waiter> {
    c:   Completion<'a, W>,
    bio: &'a Bio,
}

// the Bio's Send, and read only once it's done
unsafe impl Send for BioFuture<'_, waiter::Async> {}

impl BioFuture<'_, waiter::Sync> {
    pub fn wait(self, how: Sleep) -> Result<(), BchError> {
        let bio = self.bio;
        self.c.wait(how);
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
    unsafe { Done::<W>::from_ptr((&raw mut (*bio).bi_private).cast()) }.complete();
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

// ── For C ────────────────────────────────────────────────────────────────

#[cfg(CONFIG_BCACHEFS_DEBUG)]
#[c_export]
pub unsafe fn bch2_corrupt_bio(bio: *mut c::bio) {
    unsafe { Bio::from_raw_mut(bio) }.corrupt()
}

#[c_export]
pub unsafe fn bch2_bio_to_text(out: &mut Printbuf, bio: *mut c::bio) {
    unsafe { Bio::from_raw(bio) }.to_text(out)
}

#[c_export]
pub fn bch2_buf_nr_bvecs(p: *const c_void, len: usize) -> usize {
    buf_nr_bvecs(p.cast(), len)
}

#[c_export]
pub unsafe fn bch2_bio_map(bio: *mut c::bio, base: *mut c_void, size: usize) {
    unsafe { Bio::from_raw_mut(bio).map(base.cast(), size) }
}

#[c_export]
pub unsafe fn memcpy_to_bio(dst: *mut c::bio, dst_iter: c::bvec_iter, src: *const c_void) {
    let dst = unsafe { Bio::from_raw(dst).range_at(dst_iter) };
    dst.copy_from(unsafe { core::slice::from_raw_parts(src.cast(), dst.len() as usize) });
}

/// As much as both have, copied, both iters advanced past it.
#[c_export]
pub unsafe fn bch2_bio_copy_data_iter(dst: *mut c::bio, dst_iter: &mut c::bvec_iter,
                                      src: *mut c::bio, src_iter: &mut c::bvec_iter) {
    let mut d = unsafe { Bio::from_raw(dst).range_at(*dst_iter) };
    let mut s = unsafe { Bio::from_raw(src).range_at(*src_iter) };
    let n = d.copy_from_range(s);
    d.advance(n);
    s.advance(n);
    *dst_iter = d.pos();
    *src_iter = s.pos();
}

#[c_export]
pub unsafe fn bch2_zero_fill_bio_iter(bio: *mut c::bio, start: c::bvec_iter) {
    unsafe { Bio::from_raw(bio).range_at(start) }.zero()
}
