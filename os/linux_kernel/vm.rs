// SPDX-License-Identifier: GPL-2.0

//! A mapping's operations, vm_operations_struct, as a trait - VmOps,
//! declared by vfs_ops! (ops.rs), as the inode ops are - and a page fault,
//! VmFault, as they're passed it.

use core::marker::PhantomData;

use crate::c;
use crate::util::ffi::Opaque;
use crate::os::file::{Any, File, FileData};
use crate::os::ops::{into_raw, FromRaw};
use crate::os::folio::FolioRef;

/// A page fault on a mapping of a file of D's: the kernel's vm_fault.
#[repr(transparent)]
pub struct VmFault<D = Any>(Opaque<c::vm_fault>, PhantomData<D>);

impl<D> VmFault<D> {
    pub fn as_ptr(&self) -> *mut c::vm_fault {
        self.0.as_ptr()
    }

    /// The file mapped.
    pub fn file(&self) -> &File<D> {
        unsafe { &*(*(*self.as_ptr()).__bindgen_anon_1.vma).vm_file.cast::<File<D>>() }
    }

    /// Where in the file it faulted, in pages.
    pub fn pgoff(&self) -> u64 {
        unsafe { (*self.as_ptr()).__bindgen_anon_1.pgoff as u64 }
    }

    /// The folio of the page a write faulted on, page_mkwrite()'s: unlocked,
    /// the fault's ref keeping it.
    pub fn folio(&self) -> FolioRef<'_> {
        unsafe { FolioRef::from_raw(c::rust_page_folio((*self.as_ptr()).page)) }
    }

    /// The page cache's fault: filemap_fault().
    pub fn filemap_fault(&self) -> VmFaultRet {
        VmFaultRet(unsafe { c::filemap_fault(self.as_ptr()) })
    }
}

/// A fault on a mapping VmOps' table is installed on: of its Data's file.
impl<'a, D: FileData> FromRaw for &'a VmFault<D> {
    type Raw = *mut c::vm_fault;
    unsafe fn from_raw(r: *mut c::vm_fault) -> Self { unsafe { &*r.cast::<VmFault<D>>() } }
}

/// What a fault returns: vm_fault_t, a VM_FAULT_* or what filemap_fault()
/// returned.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct VmFaultRet(c::vm_fault_t);

impl VmFaultRet {
    pub const SIGBUS: Self = VmFaultRet(c::vm_fault_reason_VM_FAULT_SIGBUS);
    pub const NOPAGE: Self = VmFaultRet(c::vm_fault_reason_VM_FAULT_NOPAGE);
    /// The folio is returned locked.
    pub const LOCKED: Self = VmFaultRet(c::vm_fault_reason_VM_FAULT_LOCKED);
}

into_raw! {
    VmFaultRet => c::vm_fault_t: |r| r.0;
}

crate::vfs_ops! {
    pub(crate) trait VmOps for c::vm_operations_struct, VmVTable<T: VmOps> = T;
    // The files whose mappings the table is installed on: mmap's to keep
    type Data: FileData;

    fn fault(vmf: &VmFault<Self::Data>) -> VmFaultRet;
    /// A write to a page mapped read-only: made writable, its folio dirtied.
    fn page_mkwrite(vmf: &VmFault<Self::Data>) -> VmFaultRet;
}
