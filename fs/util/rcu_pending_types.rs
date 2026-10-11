// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{c_opaque, c_default};
use cstruct_macros::{c_typedef, CStruct};

c_typedef! {
    pub type rcu_pending_process_fn = Option<unsafe extern "C" fn(*mut c::rcu_pending, *mut c::rcu_head)>;
}

c_opaque!(rcu_pending_pcpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct rcu_pending {
    #[c("struct rcu_pending_pcpu __percpu *p")]
    pub p: *mut c::rcu_pending_pcpu,
    pub srcu: *mut c::srcu_struct,
    pub process: c::rcu_pending_process_fn,
}
c_default!(rcu_pending);
