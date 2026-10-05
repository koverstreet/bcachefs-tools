// SPDX-License-Identifier: GPL-2.0

//! Progress indicators for recovery passes that walk btrees (init/progress.h):
//! printed to dmesg as the walk goes, for work that has nowhere better to
//! report progress.

use crate::btree::iter::{BtreeIter, TransAttempt, TransError};
use crate::c;
use crate::fs::Fs;
use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr;

/// The filesystem's recovery pass progress indicator, c->recovery.progress -
/// one for the whole filesystem, as passes run one at a time.
pub struct Progress<'f> {
    raw: *mut c::progress_indicator,
    fs:  PhantomData<&'f Fs>,
}

impl<'f> Progress<'f> {
    /// Start reporting a walk of @leaf_btrees' keys (and @inner_btrees'
    /// interior nodes), as @msg: as bch2_progress_init() on
    /// c->recovery.progress. C passes __func__ as @msg.
    pub fn recovery(
        fs:           &'f Fs,
        msg:          &'static CStr,
        leaf_btrees:  &[c::btree_id],
        inner_btrees: &[c::btree_id],
    ) -> Self {
        let mask = |ids: &[c::btree_id]| ids.iter().fold(0u64, |m, &id| m | 1 << id as u64);
        let raw = unsafe { ptr::addr_of_mut!((*fs.raw).recovery.progress) };

        unsafe { c::bch2_progress_init(raw, msg.as_ptr(), fs.raw, mask(leaf_btrees), mask(inner_btrees)) };
        Progress { raw, fs: PhantomData }
    }

    /// Report the walk reaching @iter's position: as bch2_progress_update_iter().
    pub fn update<'a, 't>(
        &self,
        t:    TransAttempt<'a, 't>,
        iter: &mut BtreeIter<'t>,
    ) -> Result<TransAttempt<'a, 't>, TransError> {
        let ret = unsafe { c::bch2_progress_update_iter(t.raw(), self.raw, iter.raw_mut()) };
        t.result(ret)
    }
}
