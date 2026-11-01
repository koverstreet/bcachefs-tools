// SPDX-License-Identifier: GPL-2.0

//! A Placed count is its value's: the refcount traits, forwarded.

use super::{Refcount, TryRefcount};
use crate::util::place::Placed;

unsafe impl<C: Refcount, A> Refcount for Placed<C, A> {
    unsafe fn inc(&self) {
        unsafe { (**self).inc() }
    }

    unsafe fn dec(&self) {
        unsafe { (**self).dec() }
    }
}

unsafe impl<C: TryRefcount, A> TryRefcount for Placed<C, A> {
    fn try_inc(&self) -> bool {
        (**self).try_inc()
    }

    fn try_inc_live(&self) -> bool {
        (**self).try_inc_live()
    }
}
