// SPDX-License-Identifier: GPL-2.0

//! Kobjects, after Wedson Almeida Filho's rust/kernel/kobject.rs (2021,
//! never merged): a kobject's kobj_type made at compile time from its
//! owner's KobjOwner impl - release, sysfs_ops and groups - and its files as
//! attribute groups, a named group a subdirectory of the kobject's with no
//! kobject, and no lifetime, of its own.
//!
//! Wedson's Object<T> allocates the kobject with its data; ours is a field of
//! what it's the kobject of, where C has it: Kobject::new() makes it with its
//! owner, and the last put is the owner's release.

use core::cell::UnsafeCell;
use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::place::{container_of_raw, Place, Placed};
use crate::util::sysfs::{self, Dir, Files};

/// A sysfs file read and written in pages at an offset: a bin_attribute,
/// for a static.
#[repr(transparent)]
pub struct BinFile(pub c::bin_attribute);

unsafe impl Sync for BinFile {}

/// A group's bin files: NULL terminated, N files plus one.
#[repr(transparent)]
pub struct BinFiles<const N: usize>([*const c::bin_attribute; N]);

unsafe impl<const N: usize> Sync for BinFiles<N> {}

impl<const N: usize> BinFiles<N> {
    pub const fn new<const M: usize>(files: [&'static BinFile; M]) -> Self {
        assert!(M + 1 == N);

        let mut ptrs = [core::ptr::null(); N];
        let mut i = 0;
        while i < M {
            ptrs[i] = &files[i].0;
            i += 1;
        }
        BinFiles(ptrs)
    }
}

/// An attribute group: a kobject's files, or with a name a subdirectory of
/// them.
#[repr(transparent)]
pub struct Group(c::attribute_group);

unsafe impl Sync for Group {}

impl Group {
    pub const fn new<const N: usize>(name: Option<&'static CStr>, files: &'static Files<N>) -> Self {
        Self::make(name, files.as_ptr(), core::ptr::null())
    }

    pub const fn with_bin<const N: usize, const B: usize>(name: Option<&'static CStr>, files: &'static Files<N>,
                                                          bin: &'static BinFiles<B>) -> Self {
        Self::make(name, files.as_ptr(), bin.0.as_ptr())
    }

    const fn make(name: Option<&'static CStr>, attrs: *mut *mut c::attribute,
                  bin_attrs: *const *const c::bin_attribute) -> Self {
        let mut g: c::attribute_group = unsafe { core::mem::zeroed() };
        if let Some(name) = name {
            g.name = name.as_ptr().cast();
        }

        // where the kernel has them: a union with attrs_const from 6.19, and
        // bin_attrs one with bin_attrs_new before 6.18
        #[cfg(kernel)]
        #[cfg(linux_ge = "6.19")]
        {
            g.__bindgen_anon_2.attrs = attrs;
            g.bin_attrs = bin_attrs;
        }
        #[cfg(kernel)]
        #[cfg(linux_ge = "6.18")]
        #[cfg(not(linux_ge = "6.19"))]
        {
            g.attrs = attrs;
            g.bin_attrs = bin_attrs;
        }
        #[cfg(kernel)]
        #[cfg(not(linux_ge = "6.18"))]
        {
            g.attrs = attrs;
            g.__bindgen_anon_1.bin_attrs_new = bin_attrs;
        }
        #[cfg(not(kernel))]
        {
            g.attrs = attrs;
            g.bin_attrs = bin_attrs;
        }
        Group(g)
    }
}

/// A kobject's groups, for its kobj_type: NULL terminated, N groups plus one.
#[repr(transparent)]
pub struct Groups<const N: usize>([*const c::attribute_group; N]);

unsafe impl<const N: usize> Sync for Groups<N> {}

impl<const N: usize> Groups<N> {
    pub const fn new<const M: usize>(groups: [&'static Group; M]) -> Self {
        assert!(M + 1 == N);

        let mut ptrs = [core::ptr::null(); N];
        let mut i = 0;
        while i < M {
            ptrs[i] = &groups[i].0;
            i += 1;
        }
        Groups(ptrs)
    }

    pub const fn list(&'static self) -> GroupList {
        GroupList(self.0.as_ptr())
    }
}

/// Groups<N>, its N forgotten: what a kobj_type points to.
#[derive(Clone, Copy)]
pub struct GroupList(*const *const c::attribute_group);

/// Where a kobject is, as a type: what it's the kobject of.
///
/// # Safety
/// owner() of a kobject at this place is the Owner it's in.
pub(crate) unsafe trait KobjPlace {
    type Owner;

    /// # Safety
    /// @kobj is at this place.
    unsafe fn owner(kobj: *mut c::kobject) -> *mut Self::Owner;
}

unsafe impl<A: Place<Type = Self>> KobjPlace for Placed<Kobject, A> {
    type Owner = A::Base;

    unsafe fn owner(kobj: *mut c::kobject) -> *mut A::Base {
        unsafe { container_of_raw(kobj.cast::<Self>()) }
    }
}

/// What a kobject is the kobject of: its kobj_type's.
///
/// # Safety
/// Its Dir's kobject was made with Kobject::new::<Self>(); release() frees
/// what it's given.
pub(crate) unsafe trait KobjOwner: Sized + 'static {
    const GROUPS: GroupList;
    type Dir: Dir<Obj = Self>;

    /// The last ref's been put.
    ///
    /// # Safety
    /// Nothing uses @this after.
    unsafe fn release(this: NonNull<Self>);
}

struct Ktype<T>(PhantomData<T>);

impl<T: KobjOwner> Ktype<T> {
    const OPS: c::sysfs_ops = sysfs::ops::<T::Dir>();

    const KTYPE: c::kobj_type = {
        let mut k: c::kobj_type = unsafe { core::mem::zeroed() };
        k.release = Some(Self::release);
        k.sysfs_ops = &Self::OPS;
        k.default_groups = T::GROUPS.0.cast_mut();
        k
    };

    unsafe extern "C" fn release(kobj: *mut c::kobject) {
        unsafe { T::release(NonNull::new_unchecked(<T::Dir as Dir>::Kobj::owner(kobj))) }
    }
}

/// A struct kobject, in what it's the kobject of.
#[repr(transparent)]
pub struct Kobject(UnsafeCell<c::kobject>);

unsafe impl Send for Kobject {}
unsafe impl Sync for Kobject {}

unsafe impl ::pin_init::Zeroable for Kobject {}

impl Kobject {
    /// kobject_init(), with T's kobj_type: holding the ref the owner's last
    /// put() drops.
    ///
    /// # Safety
    /// For T's kobject, its Dir's Kobj.
    pub(crate) unsafe fn new<T: KobjOwner>() -> impl ::pin_init::PinInit<Self> {
        unsafe {
            ::pin_init::pin_init_from_closure(|slot: *mut Self| {
                slot.write_bytes(0, 1);
                (*slot).init::<T>();
                Ok(())
            })
        }
    }

    /// new(), for one already in place, zeroed: a static's.
    ///
    /// # Safety
    /// As new()'s.
    pub(crate) unsafe fn init<T: KobjOwner>(&self) {
        unsafe { c::kobject_init(self.as_ptr(), &Ktype::<T>::KTYPE) }
    }

    pub fn as_ptr(&self) -> *mut c::kobject {
        self.0.get()
    }

    /// The kobject @kobj, as C's: a static one, or another's.
    ///
    /// # Safety
    /// @kobj is initialized, and lives as long as 'a.
    pub unsafe fn from_raw<'a>(kobj: *mut c::kobject) -> &'a Kobject {
        unsafe { &*kobj.cast() }
    }

    /// In sysfs: added, and not deleted since.
    pub fn in_sysfs(&self) -> bool {
        // a bool in userspace's kobject, an unsigned bitfield in the kernel's
        u32::from(unsafe { (*self.as_ptr()).state_in_sysfs() }) != 0
    }

    /// Into sysfs as @name, in @parent's directory - or at the top, without
    /// one: its groups' files with it.
    pub fn add(&self, parent: Option<&Kobject>, name: &CStr) -> Result<(), BchError> {
        let parent = parent.map_or(core::ptr::null_mut(), Kobject::as_ptr);
        ret_to_result_void(unsafe {
            c::kobject_add(self.as_ptr(), parent, c"%s".as_ptr().cast(), name.as_ptr())
        })
    }

    /// Out of sysfs, if it's there.
    pub fn del(&self) {
        if self.in_sysfs() {
            unsafe { c::kobject_del(self.as_ptr()) }
        }
    }

    /// new()'s ref put: the last, and the owner's released - freed.
    ///
    /// # Safety
    /// Nothing uses the owner after, nor holds a ref.
    pub unsafe fn put(this: *const Kobject) {
        unsafe { c::kobject_put(UnsafeCell::raw_get(this.cast())) }
    }
}
