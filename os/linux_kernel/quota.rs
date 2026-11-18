// SPDX-License-Identifier: GPL-2.0

//! Quota ids as the quota subsystem has them: a kqid, a type and an id in a
//! user namespace. On disk, an id is the initial namespace's. And
//! quotactl(2)'s ops, QuotactlOps.

use crate::c;
use crate::errcode::BchError;
use crate::os::super_block::SuperBlock;
use crate::util::ffi::c_char;

/// kqid is kernel::bindings', so its methods are a trait's.
pub trait Kqid: Sized {
    /// @ty's quota @id, in the initial user namespace.
    fn new(ty: c::quota_type, id: u32) -> Self;

    /// @ty's quota @id, in the calling task's user namespace.
    fn new_current(ty: c::quota_type, id: u32) -> Self;

    /// Its id in the initial user namespace.
    fn id(self) -> u32;

    /// Its owner warned of @msg, about device @dev: quota_send_warning().
    fn send_warning(self, dev: c::dev_t, msg: c_char);
}

impl Kqid for c::kqid {
    fn new(ty: c::quota_type, id: u32) -> Self {
        unsafe { c::make_kqid(&raw mut c::init_user_ns, ty, id) }
    }

    fn new_current(ty: c::quota_type, id: u32) -> Self {
        unsafe { c::make_kqid(c::current_user_ns(), ty, id) }
    }

    fn id(self) -> u32 {
        unsafe { c::from_kqid(&raw mut c::init_user_ns, self) }
    }

    fn send_warning(self, dev: c::dev_t, msg: c_char) {
        unsafe { c::quota_send_warning(self, dev, msg) }
    }
}

crate::vfs_ops! {
    /// quotactl(2)'s ops: sb->s_qcop, of a superblock whose s_fs_info is a
    /// Super.
    pub(crate) trait QuotactlOps for c::quotactl_ops, QuotactlVTable<T: QuotactlOps> = T;
    type Super;

    fn quota_enable(sb: &SuperBlock<Self::Super>, uflags: u32) -> Result<(), BchError>;
    fn quota_disable(sb: &SuperBlock<Self::Super>, uflags: u32) -> Result<(), BchError>;
    fn rm_xquota(sb: &SuperBlock<Self::Super>, uflags: u32) -> Result<(), BchError>;
    /// Which types are accounted, and their time and warning limits.
    fn get_state(sb: &SuperBlock<Self::Super>, state: &mut c::qc_state) -> Result<(), BchError>;
    fn set_info(sb: &SuperBlock<Self::Super>, ty: i32, info: &c::qc_info) -> Result<(), BchError>;
    fn get_dqblk(sb: &SuperBlock<Self::Super>, kqid: c::kqid, qdq: &mut c::qc_dqblk) -> Result<(), BchError>;
    fn get_nextdqblk(sb: &SuperBlock<Self::Super>, kqid: &mut c::kqid, qdq: &mut c::qc_dqblk)
        -> Result<(), BchError>;
    fn set_dqblk(sb: &SuperBlock<Self::Super>, kqid: c::kqid, qdq: &c::qc_dqblk) -> Result<(), BchError>;
}
