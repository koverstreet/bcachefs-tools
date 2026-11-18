// SPDX-License-Identifier: GPL-2.0

//! A mapping's operations, vm_operations_struct, as a trait - VmOps,
//! declared by vfs_ops! (ops.rs), as the inode ops are - and a page fault,
//! VmFault, as they're passed it.

use core::marker::PhantomData;

use crate::c;
use crate::util::ffi::Opaque;
use crate::os::file::{Any, File, FileData, FsFile};
use crate::os::ops::{from_raw, into_raw};
use crate::os::folio::{FolioRef, FsMapping};

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

    /// The page cache's fault: filemap_fault().
    pub fn filemap_fault(&self) -> VmFaultRet {
        VmFaultRet(unsafe { c::filemap_fault(self.as_ptr()) })
    }
}

impl<D: FsFile> VmFault<D> where D::Inode: FsMapping {
    /// The folio of the page a write faulted on, page_mkwrite()'s - its
    /// file's inode's, unless it's since been truncated: unlocked, the
    /// fault's ref keeping it.
    pub fn folio(&self) -> FolioRef<'_, D::Inode> {
        unsafe { FolioRef::from_raw(c::rust_page_folio((*self.as_ptr()).page)) }
    }
}

#[cfg(linux_ge = "6.17")]
type VmAreaRaw = c::vm_area_desc;
#[cfg(not(linux_ge = "6.17"))]
type VmAreaRaw = c::vm_area_struct;

/// A mapping of a file of D's about to be made: mmap_prepare()'s
/// vm_area_desc, or before 6.17 mmap()'s vm_area_struct.
pub struct VmArea<'a, D>(&'a mut VmAreaRaw, PhantomData<&'a File<D>>);

impl<'a, D> VmArea<'a, D> {
    pub fn file(&self) -> &'a File<D> {
        #[cfg(linux_ge = "6.17")]
        let file = self.0.file;
        #[cfg(not(linux_ge = "6.17"))]
        let file = self.0.vm_file;
        unsafe { &*file.cast::<File<D>>() }
    }

    /// VM_*.
    pub fn flags(&self) -> kernel::ffi::c_ulong {
        #[cfg(linux_ge = "7.0")]
        let flags = self.0.vma_flags.__vma_flags[0];
        #[cfg(all(linux_ge = "6.17", not(linux_ge = "7.0")))]
        let flags = self.0.vm_flags;
        // vm_flags and __vm_flags are the one word
        #[cfg(not(linux_ge = "6.17"))]
        let flags = unsafe { self.0.__bindgen_anon_2.vm_flags };
        flags
    }

    pub fn set_vm_ops(&mut self, ops: &'static c::vm_operations_struct) {
        self.0.vm_ops = ops;
    }
}

from_raw! {
    /// mmap_prepare()'s, or mmap()'s: of the file of D's being mapped.
    VmArea<'a, D> [D: FileData]   = *mut VmAreaRaw   => |r| VmArea(&mut *r, PhantomData);
    /// A fault on a mapping VmOps' table is installed on: of its Data's file.
    &'a VmFault<D> [D: FileData]  = *mut c::vm_fault => |r| &*r.cast::<VmFault<D>>();
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
    // Faults around through the page cache: filemap_map_pages()
    pub(crate) trait VmOps for c::vm_operations_struct, VmVTable<T: VmOps> = T, |ops| {
        ops.map_pages = Some(c::filemap_map_pages);
    };
    // The files whose mappings the table is installed on: mmap's to keep
    type Data: FileData;

    fn fault(vmf: &VmFault<Self::Data>) -> VmFaultRet;
    /// A write to a page mapped read-only: made writable, its folio dirtied.
    fn page_mkwrite(vmf: &VmFault<Self::Data>) -> VmFaultRet;
}
