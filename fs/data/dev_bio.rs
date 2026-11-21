// SPDX-License-Identifier: GPL-2.0

//! A bio to a device, holding the device's io ref for its IO: given to its IO
//! by BioBox::submit_owned(), and given back. Its latency is accounted in its
//! endio, where its completion time is its own; the rest of its end - a
//! failure, a write the device may hold in its cache till a flush - is its
//! waiter's, by finish().

use core::pin::Pin;

use pin_init::{pin_data, pin_init, PinInit};

use crate::c;
use crate::os::kernel::{local_clock, READ, WRITE};
use crate::sb::members::BlockDev;
use crate::util::bio::{AtEnd, Bio, BioBox, BioInPlace, Buffers};

#[pin_data]
#[repr(C)]
pub struct DevBio {
    /* the bio's, from for_dev() till its end */
    ca:          Option<BlockDev>,
    submit_time: u64,
    /* a write with data, not FUA, which the device may hold in its cache */
    unflushed:   bool,

    #[pin]
    pub bio:     Bio,
}

crate::has_bio!(DevBio, bio);

impl DevBio {
    /// Made where its bioset or its owner has its bio, for no device yet.
    pub fn new<E>(bio: BioInPlace) -> impl PinInit<Self, E> {
        pin_init!(Self { ca: None, submit_time: 0, unflushed: false, bio <- bio.init::<E, _>() }? E)
    }

    /// For an IO to @ca, from now - its bvecs and position set: the bio's
    /// for @ca's device, and holds its io ref till it's done.
    pub fn for_dev(self: Pin<&mut Self>, ca: impl Into<BlockDev>) {
        let ca = ca.into();
        let mut this = self.project();
        this.bio.as_mut().set_dev(ca.disk_sb.bdev);
        *this.unflushed = this.bio.op() == c::req_op_REQ_OP_WRITE && this.bio.raw().bi_iter.bi_size != 0 &&
            this.bio.raw().bi_opf & c::REQ_FUA == 0;
        *this.submit_time = local_clock();
        *this.ca = Some(ca);
    }

    /// Its IO done - submit_owned() and awaited - the rest of its end: its
    /// device and status, if it failed. The box goes before the io ref, which
    /// keeps the device's bioset its bio is put back to.
    pub fn finish<B: Buffers>(mut b: BioBox<Self, B>) -> Option<(u8, c::blk_status_t)> {
        let (ca, status) = b.as_mut().take_end();
        drop(b);
        (status != 0).then_some((ca.dev_idx, status))
    }

    /// Its end in its endio, for a bio gathered, whose endio is all its end
    /// has: all of it accounted, and its device and status, for what else its
    /// owner looks at there.
    pub fn end(self: Pin<&mut Self>) -> (BlockDev, c::blk_status_t) {
        self.at_end();
        self.take_end()
    }

    /// Its result accounted against its device: the device, and its status.
    fn take_end(self: Pin<&mut Self>) -> (BlockDev, c::blk_status_t) {
        let this = self.project();
        let ca = this.ca.take().expect("a DevBio submitted for no device: for_dev() first");
        let status = this.bio.raw().bi_status;
        // by the op, not the ref's kind: a btree node write holds a read ref
        let ty = if this.bio.op() == c::req_op_REQ_OP_WRITE { c::MemberErrorType::write } else { c::MemberErrorType::read };
        ca.account_io_result(ty, status == 0);
        if *this.unflushed {
            ca.write_unflushed();
        }
        (ca, status)
    }
}

impl AtEnd for DevBio {
    fn at_end(&self) {
        if let Some(ca) = &self.ca {
            let rw = if self.bio.op() == c::req_op_REQ_OP_WRITE { WRITE } else { READ };
            ca.io_latency_acct(self.submit_time, rw);
        }
    }
}
