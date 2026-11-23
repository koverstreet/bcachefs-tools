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
use crate::errcode::BchError;
use crate::os::bio::{AtEnd, Bio, BioBox, BioOwns, Data, Gathered, HoldsDev};
use crate::os::blkdev::BlockDevice;

#[pin_data]
#[repr(C)]
pub struct DevBio<B: BioOwns = Data> {
    /* the bio's, from its submit till its end */
    ca:          Option<BlockDev>,
    submit_time: u64,
    /* a write with data, not FUA, which the device may hold in its cache */
    unflushed:   bool,

    #[pin]
    pub bio:     Bio<B>,
}

crate::has_bio!(DevBio<B>, bio, owns B where B: BioOwns);

impl<B: BioOwns> DevBio<B> {
    /// Made around @bio, where its bioset or its owner has it, for no device
    /// yet.
    pub fn new<E>(bio: impl PinInit<Bio<B>, E>) -> impl PinInit<Self, E> {
        pin_init!(Self { ca: None, submit_time: 0, unflushed: false, bio <- bio }? E)
    }

    /// Its IO done - submit_owned() and awaited - the rest of its end: its
    /// device and status, if it failed. The box goes before the io ref, which
    /// keeps the device's bioset its bio is put back to.
    pub fn finish(mut b: BioBox<Self>) -> Option<(u8, c::blk_status_t)> {
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
        let ca = this.ca.take().expect("a DevBio's end, with no device: it wasn't submitted");
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

/// Its submit's io ref, from now - its bvecs and position set: held till the
/// bio's done.
impl<B: BioOwns> HoldsDev for DevBio<B> {
    type Dev = BlockDev;

    fn hold(self: Pin<&mut Self>, ca: BlockDev) {
        let this = self.project();
        *this.unflushed = this.bio.op() == c::req_op_REQ_OP_WRITE && this.bio.raw().bi_iter.bi_size != 0 &&
            this.bio.raw().bi_opf & c::REQ_FUA == 0;
        *this.submit_time = local_clock();
        *this.ca = Some(ca);
    }

    fn bdev(&self) -> &BlockDevice {
        self.ca.as_ref().expect("a DevBio's device, before it's submitted").bdev()
    }
}

/// Gathered alone - a flush's: its end is all of it.
impl<B: BioOwns> Gathered<()> for DevBio<B> {
    fn done(self: Pin<&mut Self>, _: Result<(), BchError>) {
        self.end();
    }
}

impl<B: BioOwns> AtEnd for DevBio<B> {
    fn at_end(&self) {
        if let Some(ca) = &self.ca {
            let rw = if self.bio.op() == c::req_op_REQ_OP_WRITE { WRITE } else { READ };
            ca.io_latency_acct(self.submit_time, rw);
        }
    }
}
