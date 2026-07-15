// RAII wrapper for bch2_moving_ctxt_init / bch2_moving_ctxt_exit.
//
// The moving_context struct contains embedded list_heads that become
// self-referential after init — it must be pinned (allocated on the heap
// and never moved). We Box it first, then init in place.

use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;

use crate::c;
use crate::errcode::{self, BchError};
use crate::fs::Fs;

fn ret_to_result(ret: i32) -> Result<(), BchError> {
    errcode::ret_to_result(ret).map(|_| ())
}

/// RAII wrapper around `moving_context`. Calls `bch2_moving_ctxt_exit` on drop.
///
/// Heap-allocated and pinned because the C struct contains self-referential
/// list_head pointers after initialization.
pub struct MovingContext {
    raw: Pin<Box<c::moving_context>>,
    _pin: PhantomPinned,
}

impl MovingContext {
    pub fn new(fs: &Fs, wp: c::write_point_specifier, wait_on_copygc: bool) -> Self {
        let mut raw = Box::pin(unsafe { std::mem::zeroed::<c::moving_context>() });
        unsafe {
            c::bch2_moving_ctxt_init(
                &mut *raw.as_mut().get_unchecked_mut(),
                fs.raw,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                wp,
                wait_on_copygc,
            );
        }
        MovingContext { raw, _pin: PhantomPinned }
    }

    /// # Safety
    /// `arg` must be valid for the predicate function `pred`.
    pub unsafe fn move_data_btree(
        &mut self,
        start: c::bpos,
        end: c::bpos,
        pred: c::move_pred_fn,
        arg: *mut c_void,
        btree: c::btree_id,
        level: u32,
    ) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_move_data_btree(
                &mut *self.raw.as_mut().get_unchecked_mut(),
                start,
                end,
                pred,
                arg,
                btree,
                level,
            )
        })
    }
}

impl Drop for MovingContext {
    fn drop(&mut self) {
        unsafe { c::bch2_moving_ctxt_exit(&mut *self.raw.as_mut().get_unchecked_mut()) }
    }
}
