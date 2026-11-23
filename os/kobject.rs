// SPDX-License-Identifier: GPL-2.0

//! What both ports' kobject.rs have: a kobject's refs, as a Refcount - placed
//! in what it's the kobject of, a ref keeps that - and a kobject in a static.

use core::ptr::NonNull;

use crate::util::refcount::{Refcount, TryRefcount};

// SAFETY: its release - KobjOwner's - frees what it's in only once the last
// ref is put
unsafe impl Refcount for Kobject {
    type Target = Kobject;

    fn target(&self) -> &Kobject {
        self
    }

    unsafe fn inc(&self) {
        self.get()
    }

    unsafe fn dec(&self) {
        // SAFETY: the caller's ref
        unsafe { Kobject::put(self) }
    }
}

// SAFETY: get_unless_zero() takes a ref when it succeeds
unsafe impl TryRefcount for Kobject {
    fn try_inc(&self) -> bool {
        self.get_unless_zero()
    }
}

use super::{GroupList, Groups, KobjOwner, KobjPlace, Kobject};
use crate::os::sysfs::Dir;
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::refcount::{ARef, AlwaysRefCounted};

/// @b's T, owned from here by its kobject's refs: the ref Kobject::new() made
/// is the ARef's, and the last put frees it as @b's drop would have.
pub(crate) fn into_aref<T: KobjOwner + AlwaysRefCounted>(b: MaybeRcuAlloc<T>) -> ARef<T> {
    unsafe { ARef::from_raw(b.into_raw()) }
}

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
