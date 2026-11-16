// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct super_block, for Rust: the filesystem's, once it's
//! mounted - Fs::vfs_super(), or an inode's, VfsInode::sb(). A SuperBlock is
//! always one of ours: its s_fs_info is its Fs, set before it has an inode
//! and live until the last is evicted.
//!
//! Its operations, os::super_block's SuperOps, are Fs's: SUPER_OPS is the
//! table mount (vfs/fs_type.rs) sets s_op to.

use core::ffi::c_int;
use core::ptr::NonNull;

use crate::accounting;
use crate::alloc::capacity::avail_factor;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::inode_opts;
use crate::prt;
use crate::debug::trace::events;
use crate::os::kernel::{ktime_get_ns, uuid_to_fsid};
use crate::util::{Nsecs, Printbuf};
use crate::util::rcu::read_lock;
use crate::os::seq_file::SeqFile;
use cstruct_macros::vtable;

use crate::os::dentry::Dentry;
use super::inode::InodeInfo;
use crate::os::super_block::{SuperOps, SuperVTable};
use crate::os::vfs_inode::VfsInode;
use crate::os::iattr::{ATTR_ATIME, ATTR_CTIME, ATTR_MTIME};

/// One of ours: s_fs_info is its Fs. The type is os/linux_kernel/super_block.rs's.
pub type SuperBlock = crate::os::super_block::SuperBlock<Fs>;

impl Fs {
    /// The VFS superblock it's mounted as: None while it isn't.
    pub fn vfs_super(&self) -> Option<&SuperBlock> {
        // SAFETY: set at mount, live until unmount clears it
        unsafe { self.vfs_sb.cast::<SuperBlock>().as_ref() }
    }
}

impl SuperBlock {
    pub fn fs(&self) -> &Fs {
        self.data()
    }
}

impl VfsInode {
    pub fn sb(&self) -> &SuperBlock {
        // SAFETY: set at allocation, to our superblock, which outlives it
        unsafe { &*(*self.as_ptr()).i_sb.cast::<SuperBlock>() }
    }
}

// ── The operations ───────────────────────────────────────────────────────

#[vtable]
impl SuperOps for Fs {
    type Inode = InodeInfo;

    /// Never the VFS's to call: our inodes are made by NewInode::new().
    fn alloc_inode(&self, _sb: &SuperBlock) -> Result<NonNull<c::inode>, BchError> {
        unreachable!("bcachefs inodes are allocated by NewInode::new()")
    }

    fn free_inode(inode: NonNull<InodeInfo>) {
        super::new_inode::free(inode)
    }

    fn write_inode(&self, inode: &InodeInfo, _wbc: *mut c::writeback_control) -> Result<(), BchError> {
        inode.update_lock().write_inode(ATTR_ATIME | ATTR_MTIME | ATTR_CTIME, |_, _| Ok(()))
    }

    fn evict_inode(&self, inode: &InodeInfo) {
        inode.evict()
    }

    fn sync_fs(&self, _sb: &SuperBlock, wait: c_int) -> Result<(), BchError> {
        let start = ktime_get_ns();
        let disabled = self.opts().journal_flush_disabled.get() != 0;

        let ret = if disabled {
            Ok(())
        } else if wait == 0 {
            self.journal.flush_async(None);
            Ok(())
        } else {
            self.journal_flush()
        };

        self.event_inc_trace(events::sync_fs, |out| {
            prt!(out, "journal_flush_disabled: {}\n", disabled as u32);
            prt!(out, "blocking: {}\n", wait);
            prt!(out, "duration: {}\n", Nsecs(ktime_get_ns() - start));
        });
        ret
    }

    fn statfs(&self, dentry: Dentry<'_>, buf: &mut c::kstatfs) -> Result<(), BchError> {
        let inode = dentry.inode();
        let usage = self.usage_read_short();
        let block_bytes = self.block_bytes();
        let shift = block_bytes.ilog2() - 9;

        // Inodes as 256 bytes each - 144 to 160 is typical, since bi_dir and
        // bi_dir_offset, which shouldn't have been varints
        let avail_inodes = (usage.capacity - usage.used) << 1;

        // A reservation at the inode's replica count, from placeable space -
        // whole free buckets - which applications size fallocate() from; and
        // never more than capacity less used
        let opts = inode_opts::get_inode(self, &inode.inode());
        let nr_replicas = (opts.data_replicas as usize).clamp(1, c::BCH_REPLICAS_MAX as usize);
        let placeable = self.sectors_placeable().eventual[nr_replicas - 1];
        let avail = avail_factor(usage.free).min(placeable) / nr_replicas as u64;

        buf.f_type    = c::BCACHEFS_STATFS_MAGIC as _;
        buf.f_bsize   = block_bytes as _;
        buf.f_blocks  = usage.capacity >> shift;
        buf.f_bfree   = usage.free >> shift;
        buf.f_bavail  = avail >> shift;
        buf.f_files   = accounting::nr_inodes(self) + avail_inodes;
        buf.f_ffree   = avail_inodes;
        buf.f_fsid    = uuid_to_fsid(&self.sb.user_uuid.each_ref().map(|b| b.get()));
        buf.f_namelen = c::BCH_NAME_MAX as _;
        Ok(())
    }

    fn show_devname(&self, m: &SeqFile, _root: Dentry<'_>) -> Result<(), BchError> {
        let rcu = read_lock();
        for (i, ca) in self.online_devs_rcu(&rcu).enumerate() {
            if i > 0 {
                m.write(b":");
            }
            m.write(ca.disk_sb.name().to_bytes());
        }
        Ok(())
    }

    fn show_options(&self, m: &SeqFile, _root: Dentry<'_>) -> Result<(), BchError> {
        let mut out = Printbuf::new();
        // No superblock: with the filesystem, every option's to_text reads it
        crate::opts::opts_to_text(&mut out, &self.opts().load(), Some(self), None, Some(&c::bch_opts_mask::load(&self.mount_opts)),
                                  c::OptFlags::mount.bits(), c::OptFlags::hidden.bits(),
                                  c::OPT_SHOW_MOUNT_STYLE);
        if out.allocation_failure() {
            return Err(BchError::from(c::ENOMEM));
        }
        m.write(b",");
        m.write(out.as_bytes());
        Ok(())
    }

    fn put_super(&self, _sb: &SuperBlock) {
        let _ = self.stop();
    }

    /// bcachefs doesn't integrate intwrite freeze protection, but the
    /// internal write references serve the same purpose: so this reuses the
    /// read-only transition for the quiesce. The caveat is there's no blocking
    /// tasks that want a write reference while the superblock is frozen. Fine
    /// for now; blocking support, or sb_start_intwrite() and friends, would
    /// close it.
    fn freeze_fs(&self, _sb: &SuperBlock) -> Result<(), BchError> {
        self.state_lock_write().read_only();
        Ok(())
    }

    fn unfreeze_fs(&self, _sb: &SuperBlock) -> Result<(), BchError> {
        if self.flags.get(c::FsFlag::emergency_ro) {
            return Ok(());
        }
        self.state_lock_write().read_write()
    }
}

pub(crate) static SUPER_OPS: c::super_operations = SuperVTable::<Fs>::OPS;
