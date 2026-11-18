// SPDX-License-Identifier: GPL-2.0

//! A regular file as an open file: its file_operations, FILE_OPS, and
//! the ops of its mappings.
//!
//! A regular file has no private data: its state is its inode's, so its ops
//! are InodeInfo's, &self the file's inode - FileVTable over InodeFile,
//! whose ops_of() is f_inode. Open checks and release flushes, making and
//! dropping nothing. llseek, fsync, fallocate and remap_file_range are
//! vfs/io.rs's, read_iter direct.rs's and write_iter buffered_write.rs's. A
//! mapping's ops are InodeInfo's VmOps.
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
use crate::errcode::{Errcode, BchError};
use crate::snapshots::subvolume;
use crate::types::CStatic;
use crate::os::file::{File, FileOps, FileVTable, InodeFileOps, Opening, PosLocked};
use crate::os::iov_iter::IovIter;
use crate::os::kiocb::Kiocb;
use crate::os::kernel::{current_id, PAGE_SIZE};
use crate::os::super_block::IsInode;
use crate::os::vm::{VmArea, VmFault, VmFaultRet, VmOps, VmVTable};
use core::cmp::{max, min};
use kernel::page::PAGE_SHIFT;
use cstruct_macros::vtable;

use super::inode::InodeInfo;
use super::io;
use super::folio::FolioSpan;
use super::folio_reservation::FolioReservation;
use super::ioctl::fs_file_ioctl;

static VM_OPS: CStatic<c::vm_operations_struct, InodeInfo> = VmVTable::<InodeInfo>::STATIC;

#[vtable]
impl VmOps for InodeInfo {
    type Data = InodeFile;

    fn fault(vmf: &VmFault<InodeFile>) -> VmFaultRet {
        let mapping = vmf.file().mapping();
        let inode = vmf.file().inode();
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

    fn page_mkwrite(vmf: &VmFault<InodeFile>) -> VmFaultRet {
        let file = vmf.file();
        let inode = file.inode();
        let mapping = file.mapping();
        let fs = inode.fs();
        let block_bytes = fs.block_bytes();
        let folio = vmf.folio();

        let file_offset = (vmf.pgoff() << PAGE_SHIFT) / block_bytes * block_bytes;
        let mut len = max(PAGE_SIZE, block_bytes as usize);

        let mut res = FolioReservation::new(inode);

        let _create_lock = fs.snapshots.create_lock.read();
        let _write = inode.sb().start_pagefault();
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

        let offset = (file_offset - locked.pos()) as usize;
        assert!(offset + len <= locked.size());

        len = min(len as u64, isize - file_offset) as usize;
        let span = FolioSpan { folio: &locked, offset, len };

        if locked.update_state(inode).and_then(|()| res.add(span)).is_err() {
            return VmFaultRet::SIGBUS;
        }

        inode.ei_last_dirtied.set(current_id() as _);

        res.dirty(span);
        drop(res);

        locked.wait_stable();
        locked.keep_locked();

        inode.queue_writeback();

        VmFaultRet::LOCKED
    }
}

pub type InodeFile = crate::os::file::InodeFile<InodeInfo>;

impl InodeFileOps for InodeInfo {
    /// Opened for writing, the subvolume must be writable.
    fn check_open(f: Opening<'_, InodeFile>) -> Result<(), BchError> {
        let inode = f.file().inode();
        if f.file().flags() & (c::O_WRONLY | c::O_RDWR) != 0 {
            subvolume::is_ro(inode.fs(), inode.inum().subvol as u32)?;
        }
        f.set_can_odirect();
        f.generic_open()
    }

    /// Writeback started, if a truncate to zero asked for it: see
    /// DOC(replace-in-place).
    fn released(&self, _file: &File<InodeFile>) {
        if self.ei_flags.swap(c::InodeInfoFlag::EI_INODE_FLUSH_ON_CLOSE, false) {
            let _ = self.vfs().mapping().flush();
        }
    }
}

/// A shared writable mapping is refused once the filesystem's gone emergency
/// read-only.
fn mmap(mut vma: VmArea<'_, InodeFile>) -> Result<(), BchError> {
    let file = vma.file();
    let fs = file.inode().fs();
    let shared_writable = c::VM_SHARED | c::VM_WRITE;
    if vma.flags() & shared_writable == shared_writable && fs.flags.get(c::FsFlag::emergency_ro) {
        return fs.throw(Errcode::mmap_emergency_ro);
    }
    file.accessed();
    vma.set_vm_ops(&VM_OPS);
    Ok(())
}

#[vtable]
impl FileOps for InodeInfo {
    type Data = InodeFile;
    type Inode = InodeInfo;

    // The mount holds the module, and an open of a regular file is hot
    const MODULE_REF: bool = false;
    const PAGECACHE: bool = true;

    fn read_iter(req: &mut Kiocb<'_, InodeFile>, iter: &mut IovIter<'_>) -> isize {
        super::direct::read_iter(req, iter)
    }

    fn write_iter(req: &mut Kiocb<'_, InodeFile>, iter: &mut IovIter<'_>) -> isize {
        super::buffered_write::write_iter(req, iter)
    }

    // #[vtable] sees mmap() and mmap_prepare() whichever one is built
    #[cfg(linux_ge = "6.17")]
    const HAS_MMAP_PREPARE: bool = true;
    #[cfg(not(linux_ge = "6.17"))]
    const HAS_MMAP: bool = true;

    #[cfg(linux_ge = "6.17")]
    fn mmap_prepare(vma: VmArea<'_, InodeFile>) -> Result<(), BchError> {
        mmap(vma)
    }

    #[cfg(not(linux_ge = "6.17"))]
    fn mmap(&self, _file: &File<InodeFile>, vma: VmArea<'_, InodeFile>) -> Result<(), BchError> {
        mmap(vma)
    }

    fn llseek(&self, file: &File<InodeFile>, _fpos: &mut PosLocked<'_, Self>, offset: i64, whence: c_int)
        -> Result<u64, BchError>
    {
        io::llseek(self, file, offset, whence)
    }

    fn fsync(&self, file: &File<InodeFile>, start: i64, end: i64, datasync: c_int) -> Result<(), BchError> {
        io::fsync(self, file, start, end, datasync)
    }

    fn fallocate(&self, file: &File<InodeFile>, mode: c_int, offset: i64, len: i64) -> Result<(), BchError> {
        io::fallocate(self, file, mode, offset, len)
    }

    fn remap_file_range(&self, file_in: &File<InodeFile>, pos_in: i64, file_out: &File<InodeFile>,
                        pos_out: i64, len: i64, remap_flags: c_uint) -> Result<u64, BchError> {
        io::remap_file_range(self, file_in, pos_in, file_out, pos_out, len, remap_flags)
    }

    fn unlocked_ioctl(&self, file: &File<InodeFile>, cmd: c_uint, arg: c_ulong) -> c_long {
        fs_file_ioctl(file, self, Some(file), cmd, arg)
    }

    #[cfg(CONFIG_COMPAT)]
    fn compat_ioctl(&self, file: &File<InodeFile>, cmd: c_uint, arg: c_ulong) -> c_long {
        super::ioctl::fs_file_compat_ioctl(file, self, Some(file), cmd, arg)
    }
}

/// i_fop, as NewInode::init() sets it for a regular file.
pub static FILE_OPS: CStatic<c::file_operations, InodeFile> = FileVTable::<InodeFile>::STATIC;
