// SPDX-License-Identifier: GPL-2.0

//! A regular file as an open file: its file_operations, bch2_file_ops, and
//! the ops of its mappings.
//!
//! A regular file has no private data: its state is its inode's, so its ops
//! are InodeInfo's, &self the file's inode - FileVTable over InodeFile,
//! whose ops_of() is f_inode. Open checks and release flushes, making and
//! dropping nothing. llseek, fsync, fallocate and remap_file_range are
//! vfs/io.rs's; read and write are C's still. A mapping's ops are InodeInfo's
//! VmOps, its map_pages the kernel's generic, set in the table as it is.
//!
//! A shared writable mapping of a filesystem gone emergency read-only is
//! refused: its first write fault couldn't get a disk reservation, and
//! page_mkwrite would SIGBUS. EIO, as ext4 and f2fs return from a shut-down
//! filesystem. A read-only mapping of a file open for writing - xfs_io's
//! default - never faults for write, and still maps: generic/743 maps one
//! while its error injection takes the filesystem down. mprotect() making a
//! mapping writable later still SIGBUSes, as it always did.

/* DOC(replace-in-place)
 *
 * Applications commonly replace a file's contents in one of two ways, often
 * without fsync. POSIX promises nothing for either, but both get special
 * handling, as they do on ext4, XFS and btrfs - in one case more strongly.
 *
 * **Write a temporary file, then rename it over the original.** When a
 * rename overwrites an existing file, bcachefs first completes writeback of
 * the source file's data, so the data's extents are journalled before the
 * rename is. Combined with prefix consistency this makes the replacement
 * atomic and complete without any fsync: after a crash, either the old file
 * is intact, or the rename happened and all of the new file's data is
 * present; the empty-or-truncated-file outcome cannot occur. This orders the
 * rename behind the data but does not make the rename itself durable (no
 * journal flush is issued); an application that must know the replacement has
 * happened still needs fsync. Plain renames that don't overwrite anything get
 * no implicit writeback.
 *
 * This is stronger than the other filesystems: ext4 (`auto_da_alloc`) and
 * btrfs only start writeback of the source on an overwriting rename without
 * waiting for it, so a crash shortly afterwards can still recover an empty
 * file, and XFS does nothing here. The cost is that each overwriting rename
 * waits for its source's data to be written, which shows up in workloads that
 * replace many files this way, such as package managers.
 *
 * **Truncate and rewrite** (`O_TRUNC`). Some applications replace a file by
 * opening it with `O_TRUNC`, writing the new contents and closing it, without
 * fsync. The truncate is journalled within `journal_flush_delay` (default one
 * second), but the new data can stay in the page cache for the dirty writeback
 * delay (default thirty seconds). A crash in between recovers an empty file:
 * both the old and the new contents are lost.
 *
 * As a workaround, as on ext4, XFS and btrfs, truncating a nonempty file to
 * zero length makes the next close start writeback of the file's data.
 *
 * This is an implicit fsync on close, minus waiting for the writeback and
 * minus the journal commit, and it does not make the pattern safe. The file is
 * empty on disk from the moment the truncate is journalled until the new data
 * is written back, and a crash anywhere in that interval still loses both
 * versions; no flush can prevent that, it can only shorten the interval.
 *
 * Applications that do this and need the old or the new contents to survive a
 * crash are buggy, on every filesystem, and need to be fixed. The correct way
 * to replace a file is to write the new contents to a new file - a temporary
 * file, or an `O_TMPFILE` given a name with `linkat` - and rename it over the
 * old one, fsyncing the new file first if the new contents must be durable.
 */

use core::ffi::{c_int, c_uint};
use kernel::ffi::{c_long, c_ulong};

use crate::c;
use crate::errcode::{bch_errcode, ret_to_class, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::snapshots::subvolume;
use crate::types::CStatic;
use crate::util::file::{File, FileData, FileOps, FileVTable};
use crate::util::kernel::{current_id, PAGE_SIZE};
use crate::util::vm::{VmFault, VmFaultRet, VmOps, VmVTable};
use core::cmp::{max, min};
use kernel::page::PAGE_SHIFT;
use cstruct_macros::{c_export, c_extern, vtable};

use super::inode::InodeInfo;
use super::io;
use super::folio_reservation::FolioReservation;
use super::ioctl::fs_file_ioctl;

c_extern! {
    // The data path's, C's still:
    fn bch2_read_iter(iocb: *mut c::kiocb, iter: *mut c::iov_iter) -> isize;
    fn bch2_write_iter(iocb: *mut c::kiocb, iter: *mut c::iov_iter) -> isize;
}

// SAFETY, of the CStatic: immutable, its only parts that aren't Sync pointers
static VM_OPS: CStatic<c::vm_operations_struct> = unsafe {
    CStatic::new({
        let mut ops = VmVTable::<InodeInfo>::OPS;
        ops.map_pages = Some(c::filemap_map_pages);
        ops
    })
};

#[vtable]
impl VmOps for InodeInfo {
    fn fault(vmf: &VmFault) -> VmFaultRet {
        let mapping = vmf.file().mapping();
        let inode = unsafe { InodeInfo::of_file(vmf.file()) };
        let fs = inode.fs();
        let fdm = fs.fdm_table.get();

        if fdm == Some(mapping) {
            return VmFaultRet::SIGBUS;
        }

        /* Lock ordering: */
        let _add = match fdm {
            Some(fdm) if fdm > mapping => match inode.ei_pagecache_lock.try_lock::<c::PagecacheAdd>() {
                Some(add) => add,
                None => {
                    // the DIO that disabled faults on @fdm has its pagecache_block
                    unsafe {
                        fdm.inode().ei_pagecache_lock.unlocked_while::<c::PagecacheBlock, _>(|| {
                            drop(inode.ei_pagecache_lock.lock::<c::PagecacheAdd>())
                        })
                    };

                    /* Signal that lock has been dropped: */
                    fs.fdm_table.set_dropped_locks();
                    return VmFaultRet::SIGBUS;
                }
            },
            _ => inode.ei_pagecache_lock.lock::<c::PagecacheAdd>(),
        };

        vmf.filemap_fault()
    }

    fn page_mkwrite(vmf: &VmFault) -> VmFaultRet {
        let file = vmf.file();
        let inode = unsafe { InodeInfo::of_file(file) };
        let mapping = file.mapping();
        let fs = inode.fs();
        let block_bytes = fs.block_bytes();
        let folio = vmf.folio();

        let file_offset = (vmf.pgoff() << PAGE_SHIFT) / block_bytes * block_bytes;
        let offset = (file_offset - folio.pos()) as usize;
        let mut len = max(PAGE_SIZE, block_bytes as usize);

        assert!(offset + len <= folio.size());

        let mut res = FolioReservation::new(inode);

        let _create_lock = fs.snapshots.create_lock.read();
        let _write = inode.vfs().sb().start_pagefault();
        let _ = file.update_time();

        /*
         * Not strictly necessary, but helps avoid dio writes livelocking in
         * bch2_write_invalidate_inode_pages_range() - can drop this if/when we get
         * a bch2_write_invalidate_inode_pages_range() that works without dropping
         * page lock before invalidating page
         */
        let _add = inode.ei_pagecache_lock.lock::<c::PagecacheAdd>();

        let locked = folio.lock();
        let isize = inode.vfs().size();

        if locked.mapping() != Some(mapping) || file_offset >= isize {
            return VmFaultRet::NOPAGE;
        }

        len = min(len as u64, isize - file_offset) as usize;

        if locked.update_state(inode).and_then(|()| res.add(*locked, offset, len)).is_err() {
            return VmFaultRet::SIGBUS;
        }

        inode.ei_last_dirtied.set(current_id() as _);

        res.dirty(*locked, offset, len);
        drop(res);

        locked.keep_locked().wait_stable();

        inode.queue_writeback();

        VmFaultRet::LOCKED
    }
}

/// A regular file's state: its inode's. Never made: there's nothing to make.
/// FileVTable::<InodeInfo> would take the inode for private data - the
/// blanket FileData of an ops type - and drop it at release: this is the one.
pub(crate) enum InodeFile {}

impl FileData for InodeFile {
    type Ops = InodeInfo;

    fn ops(&self) -> &InodeInfo {
        match *self {}
    }

    unsafe fn ops_of<'a>(file: *mut c::file) -> &'a InodeInfo {
        // SAFETY: an open file of ours: its inode's referenced while it's open
        unsafe { InodeInfo::from_inode((*file).f_inode) }
    }

    /// Writeback started, if a truncate to zero asked for it: see
    /// DOC(replace-in-place).
    unsafe fn release(file: *mut c::file) {
        // SAFETY: as ops_of()'s
        let inode = unsafe { Self::ops_of(file) };
        if inode.ei_flags.swap(c::bch_inode_info_flags::EI_INODE_FLUSH_ON_CLOSE, false) {
            unsafe { c::rust_filemap_flush((*inode.vfs().as_ptr()).i_mapping) };
        }
    }
}

/// A shared writable mapping is refused once the filesystem's gone emergency
/// read-only: @flags the mapping's VM_*.
fn mmap_check(fs: &Fs, flags: c_ulong) -> Result<(), BchError> {
    let shared_writable = c::VM_SHARED | c::VM_WRITE;
    if flags & shared_writable == shared_writable && fs.flags.get(c::bch_fs_flags::emergency_ro) {
        return fs.throw(bch_errcode::BCH_ERR_mmap_emergency_ro);
    }
    Ok(())
}

#[vtable]
impl FileOps for InodeInfo {
    // #[vtable] sees mmap() and mmap_prepare() whichever one is built
    #[cfg(linux_ge = "6.17")]
    const HAS_MMAP_PREPARE: bool = true;
    #[cfg(not(linux_ge = "6.17"))]
    const HAS_MMAP: bool = true;

    #[cfg(linux_ge = "6.17")]
    fn mmap_prepare(desc: &mut c::vm_area_desc) -> Result<(), BchError> {
        #[cfg(linux_ge = "7.0")]
        let flags = desc.vma_flags.__vma_flags[0];
        #[cfg(not(linux_ge = "7.0"))]
        let flags = desc.vm_flags;
        // SAFETY: the file being mapped: one of ours, open
        mmap_check(unsafe { <InodeFile as FileData>::ops_of(desc.file) }.fs(), flags)?;
        unsafe { c::file_accessed(desc.file) };
        desc.vm_ops = &*VM_OPS;
        Ok(())
    }

    #[cfg(not(linux_ge = "6.17"))]
    fn mmap(&self, file: &File, vma: &mut c::vm_area_struct) -> Result<(), BchError> {
        // SAFETY: vm_flags and __vm_flags are the one word
        mmap_check(self.fs(), unsafe { vma.__bindgen_anon_2.vm_flags })?;
        unsafe { c::file_accessed(file.as_ptr()) };
        vma.vm_ops = &*VM_OPS;
        Ok(())
    }

    fn llseek(&self, file: &File, offset: i64, whence: c_int) -> Result<u64, BchError> {
        io::llseek(self, file, offset, whence)
    }

    fn fsync(&self, file: &File, start: i64, end: i64, datasync: c_int) -> Result<(), BchError> {
        io::fsync(self, file, start, end, datasync)
    }

    fn fallocate(&self, file: &File, mode: c_int, offset: i64, len: i64) -> Result<(), BchError> {
        io::fallocate(self, file, mode, offset, len)
    }

    fn remap_file_range(&self, file_in: &File, pos_in: i64, file_out: &File, pos_out: i64, len: i64,
                        remap_flags: c_uint) -> Result<u64, BchError> {
        io::remap_file_range(self, file_in, pos_in, file_out, pos_out, len, remap_flags)
    }

    fn unlocked_ioctl(&self, file: &File, cmd: c_uint, arg: c_ulong) -> c_long {
        fs_file_ioctl(file, cmd, arg)
    }

    #[cfg(CONFIG_COMPAT)]
    fn compat_ioctl(&self, file: &File, cmd: c_uint, arg: c_ulong) -> c_long {
        super::ioctl::fs_file_compat_ioctl(file, cmd, arg)
    }
}

/// Opened for writing, the subvolume must be writable. FileOps' open() makes
/// private data, and there's none: this is the table's own.
unsafe extern "C" fn open(vinode: *mut c::inode, file: *mut c::file) -> c_int {
    // SAFETY: a file of ours being opened, and its inode
    let (inode, f) = unsafe { (InodeInfo::from_inode(vinode), File::from_raw(file)) };
    ret_to_class((|| {
        if f.flags() & (c::O_WRONLY | c::O_RDWR) != 0 {
            subvolume::is_ro(inode.fs(), inode.inum().subvol as u32)?;
        }
        // SAFETY: not yet the file of anyone but the opener
        unsafe { (*file).f_mode |= c::FMODE_CAN_ODIRECT };
        ret_to_result_void(unsafe { c::generic_file_open(vinode, file) })
    })())
}

/// i_fop, as NewInode::init() sets it for a regular file.
#[c_export]
pub static bch2_file_ops: c::file_operations = {
    let mut ops = FileVTable::<InodeFile>::OPS;
    // No module reference taken by each open, as FileVTable has it: the
    // mount holds one, and an open of a regular file is hot
    ops.owner            = core::ptr::null_mut();
    ops.open             = Some(open);
    ops.read_iter        = Some(bch2_read_iter);
    ops.write_iter       = Some(bch2_write_iter);
    #[cfg(CONFIG_TRANSPARENT_HUGEPAGE)]
    {
        ops.get_unmapped_area = Some(c::thp_get_unmapped_area);
    }
    ops.splice_read      = Some(c::filemap_splice_read);
    ops.splice_write     = Some(c::iter_file_splice_write);
    ops
};
