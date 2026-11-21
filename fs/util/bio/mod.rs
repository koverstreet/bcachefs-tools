// SPDX-License-Identifier: GPL-2.0

//! struct bio, from Rust. The terms, used bare here and in iter.rs and
//! owned.rs:
//!
//! - A bio describes one block IO: device, op, sector, a position in its
//!   data, and its bvecs - (page, offset, len), its own, or a clone's
//!   source's. Its c::bio is private, read through raw().
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
//!   bio's made, and the bio holds it (BioBuffersMapped); one whose owner
//!   puts nothing in its bvecs is remapped (BioOwner::remap_read()).
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
//!
//! The generics, below, are those terms as traits. A bio is a field of a
//! struct of its user's - an rbio, a write op - and HasBio is that
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
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
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
use cstruct_macros::Init;
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
mod iter;
mod owned;
pub mod pages;

#[cfg(kernel)]
pub use iter::{FolioVec, Folios};
pub use iter::{bvec_offset, reset_bvec_offset, BioRange, Chunk, Chunks, Mapped, Segment, Segments, Walk};
pub use buffers::{io_buf, BioBuffersMapped, BioBuffersOwned, BioBuffersPages, BioBuffersUnmanaged, BioClone,
                  Buffers, IoBuf, MapBuf};
pub use owned::{bio_put, submit_gathered_borrowed, BioBox, BioInPlace, BioLend, BioPool, BioSet, BvecAlloc,
                BvecMempool, Gathered, Lent, Submitted};

fn req_op(opf: u32) -> u32 {
    opf & ((1 << c::REQ_OP_BITS) - 1)
}

pub use crate::os::bio::{buf_bio_bytes, buf_nr_bvecs};

pub const BIO_INLINE_VECS: usize = 4;

pub(crate) type BioVecs = InlineVec<c::bio_vec, BIO_INLINE_VECS, u16, BvecAlloc>;

/// Where a bio's bvec table is: its own BioVecs, or another bio's - a clone's
/// source's, init_borrowed()'s table's - read where they are now, as they
/// move or grow. bi_io_vec is only ever where they were when the bio last
/// went out to C: raw_mut() writes it, and nothing reads it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VecsAt {
    Own,
    /// A clone's, or a table its owner keeps - init_borrowed()'s: the bio
    /// whose bvecs they are, which outlives it
    Borrowed(NonNull<Bio>),
}

// XXX: a Bio moves, and is swapped, as any Rust value - but what its bvecs
// point at is its box's Buffers, not its own: swap two boxes' bios and each
// box gives back the other's pages, or a clone outlives the parent its
// BioClone<'a> borrowed. Sound once a bio is generic over what it owns.
#[pin_data(PinnedDrop)]
#[repr(C)]
#[derive(Init)]
pub struct Bio {
    bio:     c::bio,
    #[init(BioVecs::new_in(BvecAlloc::NONE))]
    vecs:    BioVecs,
    #[init(VecsAt::Own)]
    vecs_at: VecsAt,
    #[pin]
    data:    BioData,
}

/// bio_uninit(), and its bvecs freed: emptied first, as a bioset's bio is
/// dropped again by bio_put() when what it's in fails to init - which frees
/// nothing twice.
#[pinned_drop]
impl PinnedDrop for Bio {
    fn drop(self: Pin<&mut Self>) {
        let this = self.get_mut();
        unsafe { crate::os::bio::uninit(&mut this.bio) };
        drop(core::mem::replace(&mut this.vecs, BioVecs::new_in(BvecAlloc::NONE)));
    }
}

// SAFETY: a bio is no thread's - submitted on one, completed on another. Not
// Sync: two holders of a &Bio could write its data at once.
unsafe impl Send for Bio {}

impl Bio {
    /// After its bvecs change: C's view of them.
    fn sync(&mut self) {
        assert!(matches!(self.vecs_at, VecsAt::Own), "changing the bvecs of a bio whose table isn't its own");
        let (nr, max) = (self.vecs.len() as u16, self.vecs.capacity().min(c::BIO_MAX_VECS as usize) as u16);
        bi_io_vec!(self.bio) = self.vecs.as_mut_ptr();
        self.bio.bi_vcnt = nr;
        self.bio.bi_max_vecs = max;
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

    /// bio_init() of a Bio at @bio whose bvecs are @table's, all of them,
    /// which it doesn't own: a data update's read and write bios share its
    /// pages' bio's.
    ///
    /// # Safety
    /// @bio is room for a Bio, not in flight; @table outlives the bio, and
    /// its bvecs don't change while the bio's IO is in flight.
    pub unsafe fn init_borrowed(bio: *mut Bio, bdev: *mut crate::os::blkdev::BlockDevice, table: &mut Bio,
                                opf: u32) {
        let nr = table.vecs.len() as u16;
        unsafe {
            (&raw mut (*bio).vecs).write(BioVecs::new_in(BvecAlloc::NONE));
            (&raw mut (*bio).vecs_at).write(VecsAt::Borrowed(NonNull::from(&*table)));
            crate::os::bio::init(bio.cast(), bdev, table.vecs.as_mut_ptr(), nr, opf);
            (*bio).bio.bi_vcnt = nr;
        }
    }

    pub fn raw(&self) -> &c::bio {
        &self.bio
    }

    /// A clone has none of its own: its source's are its, from its position.
    fn bvecs(&self) -> &[c::bio_vec] {
        if self.cloned() { &[] } else { &self.vecs_owner().vecs }
    }

    pub fn nr_vecs(&self) -> u16 {
        self.bvecs().len() as u16
    }

    pub fn max_vecs(&self) -> u16 {
        self.vecs.capacity().min(c::BIO_MAX_VECS as usize) as u16
    }

    /// For its own methods, and C's: bi_io_vec pointed at its bvecs, where
    /// they are now.
    pub(crate) fn raw_mut(&mut self) -> &mut c::bio {
        bi_io_vec!(self.bio) = match self.vecs_at {
            VecsAt::Own         => self.vecs.as_mut_ptr(),
            VecsAt::Borrowed(s) => unsafe { s.as_ref() }.bvecs().as_ptr().cast_mut(),
        };
        &mut self.bio
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
    pub(crate) fn range_all(&self) -> BioRange<'_> {
        assert!(!self.cloned(), "the _all walk of a cloned bio: its bvecs are its parent's");
        let size = self.bvecs().iter().map(|bv| bv.bv_len).sum();
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

    /// @len bytes of @buf mapped, pushed onto its bvecs. Panics if there's no
    /// room for them. For a buffer kept by other means than the bio - a stripe
    /// buffer's blocks, kept by the IO's round; BioBuffersMapped holds it.
    ///
    /// # Safety
    /// @buf is the allocators' - in the direct map, or vmalloc - and outlives
    /// its place in the bvecs; nothing has the bio in flight.
    pub unsafe fn map(&mut self, buf: *mut u8, len: usize) {
        let len32 = u32::try_from(len).expect("mapping a buffer of 4G or more into a bio");
        for bv in unsafe { crate::os::bio::bvecs(buf, len) } {
            self.vecs.push(bv).unwrap_or_else(|_| panic!("mapping {len} bytes into a bio: no room for its bvecs"));
        }
        self.sync();
        self.raw_mut().bi_iter.bi_size += len32;
    }

    /// @len bytes of @folio from @offset, its last bvec. Panics if the bio's
    /// bvecs are full.
    ///
    /// # Safety
    /// @folio stays held - locked, in writeback, pinned, or the caller's - till
    /// the bio's done with it.
    #[cfg(kernel)]
    pub(crate) unsafe fn add_folio_part<A>(&mut self, folio: &crate::os::folio::Folio<A>, offset: usize, len: usize) {
        assert!(offset + len <= folio.size(), "adding {len} bytes at {offset} of a folio of {}", folio.size());
        assert!(self.vecs.len() < usize::from(self.max_vecs()), "adding a folio: the bio's bvecs are full");
        self.vecs.push(c::bio_vec { bv_page: folio.as_ptr().cast(), bv_len: len as u32, bv_offset: offset as u32 })
            .expect("a bvec, into the room it has");
        self.sync();
        self.raw_mut().bi_iter.bi_size += len as u32;
    }

    /// Emptied, for another IO from scratch: bio_reset(), its bvecs and its
    /// position gone - a bio whose pages are new each IO, a dio's.
    pub fn clear(&mut self, opf: u32) {
        self.vecs.clear();
        unsafe { crate::os::bio::reset(self.raw_mut(), core::ptr::null_mut(), opf) };
        self.sync();
    }

    /// For another IO: bio_reset(), its bvecs kept - the buffers it was made
    /// with - and its position all of them again, from the table: after an IO
    /// bi_iter is the block layer's. Panics on a clone, whose bvecs aren't its
    /// own. BioOwner::reset(), which checks the buffers serve @opf.
    pub(super) fn reset(&mut self, bdev: *mut crate::os::blkdev::BlockDevice, opf: u32) {
        let size = self.range_all().len();
        // SAFETY: had by &mut, so not in flight
        unsafe { crate::os::bio::reset(self.raw_mut(), bdev, opf) };
        self.sync();
        self.raw_mut().bi_iter.bi_size = size;
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
        assert!(usize::from(nr) <= self.vecs.len(), "truncating a bio to more bvecs than it has");
        self.vecs.truncate(nr.into());
        self.sync();
        self.raw_mut().bi_iter.bi_size = 0;
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

    /// Its bvec @idx: its own, or a clone's source's as they are now. Panics
    /// past them - a clone's source emptied under it.
    fn bvec(&self, idx: u32) -> c::bio_vec {
        let vecs = self.vecs_owner().bvecs();
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

        write!(out, "bi_vcnt\t{}\n", self.nr_vecs());
        write!(out, "bi_max_vecs\t{}\n", self.max_vecs());
    }
}

/// # Safety
/// BIO_OFFSET is the offset of a Bio field.
pub unsafe trait HasBio: Sized {
    const BIO_OFFSET: usize;

    fn bio(&self) -> &Bio {
        unsafe { Bio::from_raw(bio_of(core::ptr::from_ref(self).cast_mut())) }
    }

    fn bio_mut(self: Pin<&mut Self>) -> &mut Bio {
        unsafe { Bio::from_raw_mut(bio_of(self.get_unchecked_mut())) }
    }
}

// SAFETY: a bio, at its own start
unsafe impl HasBio for Bio {
    const BIO_OFFSET: usize = 0;
}

/// A struct's bio field: a Bio, or a BioLend, its bio AT into it.
///
/// # Safety
/// A bio is AT bytes into the field, and ends it.
pub unsafe trait BioField {
    const AT: usize = 0;
}
unsafe impl BioField for Bio {}

#[doc(hidden)]
pub const fn bio_field_at<B: BioField>(_: *const B) -> usize {
    B::AT
}

/// HasBio for $t, by its bio field $f: checked to be a BioField - not that
/// it's #[pin], which HasBio asks of a struct with #[pin_data].
#[macro_export]
macro_rules! has_bio {
    ($t:ty, $($f:ident).+) => {
        unsafe impl $crate::util::bio::HasBio for $t {
            const BIO_OFFSET: usize = {
                let t = ::core::mem::MaybeUninit::<$t>::uninit();
                let field = unsafe { &raw const (*t.as_ptr()).$($f).+ };
                ::core::mem::offset_of!($t, $($f).+) + $crate::util::bio::bio_field_at(field)
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

    fn bio_mut(&mut self) -> &mut Bio;

    /// For another IO, of @opf, to or from the same buffers: Bio::reset().
    /// Panics if they can't serve it - a buffer borrowed shared, for a read.
    fn reset(&mut self, bdev: *mut crate::os::blkdev::BlockDevice, opf: u32) {
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
        self.submit(waiter::Sync::current()).wait_sync(Sleep::UNINTERRUPTIBLE.io())
    }

    /// Submitted, its IO's Completion made here for @waiter: the endio's end in
    /// bi_private, the waiter's the BioFuture - awaited or waited on, now or
    /// later, for the IO's status.
    fn submit<W: Waiter>(&mut self, waiter: W) -> BioFuture<'_, W> {
        let bio = self.bio_mut();
        let raw: *mut c::bio = bio.raw_mut();
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
        let bio: *mut c::bio = this.owner.bio_mut().raw_mut();
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

pub unsafe fn bch2_bio_map(bio: *mut c::bio, base: *mut c_void, size: usize) {
    unsafe { Bio::from_raw_mut(bio).map(base.cast(), size) }
}

pub unsafe fn memcpy_to_bio(dst: *mut c::bio, dst_iter: c::bvec_iter, src: *const c_void) {
    let dst = unsafe { Bio::from_raw(dst).range_at(dst_iter) };
    dst.copy_from(unsafe { core::slice::from_raw_parts(src.cast(), dst.len() as usize) });
}
