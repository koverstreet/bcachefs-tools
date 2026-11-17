// SPDX-License-Identifier: GPL-2.0

//! A kobject in a static: the port's kobject.rs's, which has this module too.

use core::ptr::NonNull;

use super::{GroupList, Groups, KobjOwner, KobjPlace, Kobject};
use crate::os::sysfs::Dir;

/// A directory with no files of its own, in a static - /sys/fs/bcachefs. Its
/// release is nothing: there's nothing to free, and its children's refs on it
/// keep it till they're gone.
#[repr(transparent)]
pub struct StaticKobject(Kobject);

static NO_GROUPS: Groups<StaticKobject, 1> = Groups::new([]);

impl StaticKobject {
    pub const fn new() -> Self {
        StaticKobject(::pin_init::zeroed())
    }

    /// Made, holding the ref put() drops. Panics if it was made already: its
    /// refs would be reset under its children's.
    pub fn init(&'static self) {
        assert!(!self.0.initialized(), "a static kobject made twice");
        // SAFETY: StaticKobject's kobject, its Dir's Kobj
        unsafe { self.0.init::<StaticKobject>() }
    }

    pub fn kobj(&self) -> &Kobject {
        &self.0
    }

    /// init()'s ref dropped: out of sysfs, once it's the last.
    pub fn put(&'static self) {
        // SAFETY: release frees nothing
        unsafe { Kobject::put(&self.0) }
    }
}

impl Dir for StaticKobject {
    type Obj = StaticKobject;
    type Kobj = StaticKobject;
}

impl KobjPlace for StaticKobject {
    type Owner = StaticKobject;

    unsafe fn owner(kobj: *mut Kobject) -> *mut StaticKobject {
        kobj.cast()
    }
}

unsafe impl KobjOwner for StaticKobject {
    const GROUPS: GroupList<StaticKobject> = NO_GROUPS.list();
    type Dir = StaticKobject;

    unsafe fn release(_: NonNull<Self>) {}
}
