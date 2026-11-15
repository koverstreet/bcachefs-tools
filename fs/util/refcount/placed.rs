// SPDX-License-Identifier: GPL-2.0

//! A Placed count is its value's: the refcount traits, forwarded. What a ref
//! on it keeps is what it's placed in.

use super::{Refcount, TryRefcount};
use crate::util::place::{At, Contained, Placed};

unsafe impl<C: Refcount, B, const N: u64> Refcount for Placed<C, At<B, N>>
where
    Self: Contained<Parent = B>,
{
    type Target = B;

    fn target(&self) -> &B {
        self.container()
    }

    unsafe fn inc(&self) {
        unsafe { (**self).inc() }
    }

    unsafe fn dec(&self) {
        unsafe { (**self).dec() }
    }
}

unsafe impl<C: TryRefcount, B, const N: u64> TryRefcount for Placed<C, At<B, N>>
where
    Self: Contained<Parent = B>,
{
    fn try_inc(&self) -> bool {
        (**self).try_inc()
    }

    fn try_inc_live(&self) -> bool {
        (**self).try_inc_live()
    }
}
