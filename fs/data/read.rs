// SPDX-License-Identifier: GPL-2.0

//! A read, awaited by its caller's task: C's bch2_read(), which ends the read
//! with its bio's endio once all of it is done.

use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::util::alloc::init_at;
use crate::util::bio::BioInPlace;
use crate::util::task::{waiter, Completion, Done, Resumable, Resume};
use core::convert::Infallible;
use core::ffi::c_int;
use core::mem::offset_of;
use core::ptr::null_mut;
use cstruct_macros::c_extern;
use pin_init::PinInit;

type F = c::bch_read_flags;

c_extern! {
    fn bch2_read(trans: *mut BtreeTrans<'_>, rbio: *mut c::bch_read_bio, iter: c::bvec_iter, inum: c::subvol_inum,
                 failed: *mut c::bch_io_failures, prev_read: *mut c::bkey_buf, flags: F) -> c_int;
}

unsafe impl Send for c::bch_read_bio {}
unsafe impl Sync for c::bch_read_err_report {}

impl c::bch_read_bio {
    /// A read bio around @bio, inline_bio()'s.
    pub fn new<'a>(fs: &'a Fs, opts: c::bch_inode_opts, subvol: u32, err_report: Option<&'a c::bch_read_err_report>,
                   bio: BioInPlace) -> impl PinInit<Self, BchError> + 'a {
        unsafe {
            ::pin_init::pin_init_from_closure(move |rbio: *mut Self| {
                let Ok(()) = init_at(bio.init::<Infallible>(), (&raw mut (*rbio).bio).cast());
                rbio.cast::<u8>().write_bytes(0, offset_of!(Self, bio));
                let r = &mut *rbio;
                r.init(fs, opts, None);
                r.subvol = subvol;
                r.err_report = err_report.map_or(null_mut(), |e| core::ptr::from_ref(e).cast_mut());
                Ok(())
            })
        }
    }
}

pub async fn read(rbio: &mut c::bch_read_bio, inum: c::subvol_inum, flags: F) -> Result<(), BchError> {
    let fs = unsafe { Fs::borrow_raw(rbio.c) };
    let waiter = waiter::Async::current().await;
    let bio = &raw mut rbio.bio;
    let done = unsafe {
        (*bio).bi_end_io = Some(read_endio);
        Completion::init_at((&raw mut (*bio).bi_private).cast(), waiter)
    };
    {
        let trans = crate::btree_trans!(fs);
        unsafe { bch2_read(trans.raw(), rbio, (*bio).bi_iter, inum, null_mut(), null_mut(), flags) };
    }
    done.resume_on(Resume::SPAWNED).await;
    ret_to_result_void(rbio.ret.into())
}

unsafe extern "C" fn read_endio(bio: *mut c::bio) {
    unsafe { Done::<waiter::Async>::from_ptr((&raw mut (*bio).bi_private).cast()) }.complete();
}
