// SPDX-License-Identifier: GPL-2.0

//! A mapping's operations, vm_operations_struct, as a trait - VmOps,
//! declared by vfs_ops! (ops.rs), as the inode ops are - and a page fault,
//! VmFault, as they're passed it.

use crate::c;
use crate::util::ffi::Opaque;
use crate::os::file::File;
use crate::os::ops::{from_raw, into_raw};
use crate::vfs::folio::FolioRef;

/// A page fault on a mapping: the kernel's vm_fault.
#[repr(transparent)]
pub struct VmFault(Opaque<c::vm_fault>);

impl VmFault {
    pub fn as_ptr(&self) -> *mut c::vm_fault {
        self.0.as_ptr()
    }

    /// The file mapped.
    pub fn file(&self) -> &File {
        unsafe { File::from_raw((*(*self.as_ptr()).__bindgen_anon_1.vma).vm_file) }
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

from_raw! {
    &'a VmFault = *mut c::vm_fault => |r| &*r.cast::<VmFault>();
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

    fn fault(vmf: &VmFault) -> VmFaultRet;
    /// A write to a page mapped read-only: made writable, its folio dirtied.
    fn page_mkwrite(vmf: &VmFault) -> VmFaultRet;
}
