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
use crate::util::place::{At, FieldOf, Here, Path, PathField, Placed};
use crate::util::refcount::RefGuard;

/// What holds a KobjOwner - a MaybeRcuAlloc, or the static it's in - given up
/// to its kobject's refs, and had back whole by their last put.
pub(crate) trait KobjAlloc<T>: Sized {
    /// # Safety
    /// @p is the T one held, and it's had back once.
    unsafe fn from_raw(p: NonNull<T>) -> Self;
}

impl<T> KobjAlloc<T> for MaybeRcuAlloc<T> {
    unsafe fn from_raw(p: NonNull<T>) -> Self {
        unsafe { MaybeRcuAlloc::from_raw(p) }
    }
}

impl<T: 'static> KobjAlloc<T> for &'static T {
    unsafe fn from_raw(p: NonNull<T>) -> Self {
        unsafe { p.as_ref() }
    }
}

/// @b's T, owned from here by its kobject's refs: the ref Kobject::new() made
/// is the one returned, and the last put gives @b back to its release.
pub(crate) fn into_ref<T, const N: u64>(b: MaybeRcuAlloc<T>) -> RefGuard<Placed<Kobject, At<T, N>>>
where
    T: KobjOwner<Alloc = MaybeRcuAlloc<T>> + PathField<N, Type = Placed<Kobject, At<T, N>>>,
    T::Dir: Dir<Kobj = Placed<Kobject, At<T, N>>>,
    Placed<Kobject, At<T, N>>: Refcount<Target = T>,
{
    let t = b.into_raw();
    unsafe { RefGuard::adopt(<FieldOf<Here, N> as Path<T>>::get(t.as_ref())) }
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
    type Alloc = &'static StaticKobject;
}
