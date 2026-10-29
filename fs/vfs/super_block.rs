// SPDX-License-Identifier: GPL-2.0

//! The VFS's struct super_block, for Rust: the filesystem's, once it's
//! mounted - Fs::vfs_super(), or an inode's, VfsInode::sb(). A SuperBlock is
//! always one of ours: its s_fs_info is its Fs, set before it has an inode
//! and live until the last is evicted.
//!
//! Its operations are a trait, SuperOps, declared by vfs_ops! (ops.rs): Self
//! is what s_fs_info points to - Fs - and each op gets it as &self, but
//! free_inode(), which can run after the filesystem's freed.
//!
//! bch2_super_ops is ours, the table mount sets s_op to. evict_inode() and
//! free_inode() are still fs.c's, with the inode cache.

use core::ffi::{c_int, CStr};
use core::ptr::NonNull;

use crate::accounting;
use crate::alloc::capacity::avail_factor;
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::inode_opts;
use crate::locking::Lock;
use crate::prt;
use crate::sb::counters::events;
use crate::util::kernel::ktime_get_ns;
use crate::util::{Nsecs, Printbuf};
use crate::util::rcu::read_lock;
use crate::util::seq_file::SeqFile;
use cstruct_macros::{c_export, c_extern, vtable};

use super::dentry::Dentry;
use super::inode::{InodeInfo, ATTR_ATIME, ATTR_CTIME, ATTR_MTIME};

c_extern! {
    // fs.c's, with the inode cache:
    fn bch2_evict_inode(vinode: *mut c::inode);
    fn bch2_free_inode(vinode: *mut c::inode);
}

/// One of ours: s_fs_info is its Fs. The type is util/super_block.rs's.
pub type SuperBlock = crate::util::super_block::SuperBlock<Fs>;

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

// ── The operations ───────────────────────────────────────────────────────

crate::vfs_ops! {
    pub(crate) trait SuperOps for c::super_operations, SuperVTable<T: SuperOps> = T;

    fn alloc_inode(&self = sb, sb: &SuperBlock) -> Result<NonNull<c::inode>, BchError>;
    /// The inode's memory, the VFS done with it - an RCU callback after
    /// eviction, which can run after the filesystem's freed: no &self.
    fn free_inode(inode: NonNull<InodeInfo>);
    /// Not always referenced - iput_final() writes one back with i_count 0
    /// - but I_SYNC or I_WILL_FREE keeps it from eviction meanwhile.
    fn write_inode(&self = inode, inode: &InodeInfo, wbc: *mut c::writeback_control)
        -> Result<(), BchError>;
    /// I_FREEING: ours to evict, the superblock still up.
    fn evict_inode(&self = inode, inode: &InodeInfo);
    fn sync_fs(&self = sb, sb: &SuperBlock, wait: c_int) -> Result<(), BchError>;
    fn statfs(&self = dentry, dentry: Dentry<'_>, buf: &mut c::kstatfs) -> Result<(), BchError>;
    fn show_devname(&self = root, m: &SeqFile, root: Dentry<'_>) -> Result<(), BchError>;
    fn show_options(&self = root, m: &SeqFile, root: Dentry<'_>) -> Result<(), BchError>;
    /// The VFS's last op on the superblock.
    fn put_super(&self = sb, sb: &SuperBlock);
    fn freeze_fs(&self = sb, sb: &SuperBlock) -> Result<(), BchError>;
    fn unfreeze_fs(&self = sb, sb: &SuperBlock) -> Result<(), BchError>;
}

// ── bcachefs's ───────────────────────────────────────────────────────────

#[vtable]
impl SuperOps for Fs {
    /// Never the VFS's to call: our inodes are made by bch2_new_inode().
    fn alloc_inode(&self, _sb: &SuperBlock) -> Result<NonNull<c::inode>, BchError> {
        unreachable!("bcachefs inodes are allocated by bch2_new_inode()")
    }

    fn free_inode(inode: NonNull<InodeInfo>) {
        unsafe { bch2_free_inode(inode.as_ptr().cast()) }
    }

    fn write_inode(&self, inode: &InodeInfo, _wbc: *mut c::writeback_control) -> Result<(), BchError> {
        inode.update_lock().write_inode(ATTR_ATIME | ATTR_MTIME | ATTR_CTIME, |_, _| Ok(()))
    }

    fn evict_inode(&self, inode: &InodeInfo) {
        unsafe { bch2_evict_inode(inode.vfs().as_ptr()) }
    }

    fn sync_fs(&self, _sb: &SuperBlock, wait: c_int) -> Result<(), BchError> {
        let start = ktime_get_ns();
        let disabled = self.opts().journal_flush_disabled != 0;

        let ret = if disabled {
            Ok(())
        } else if wait == 0 {
            unsafe { c::bch2_journal_flush_async(&raw mut (*self.as_ptr()).journal, core::ptr::null_mut()) };
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
        let mut uuid = self.sb.user_uuid.each_ref().map(|b| b.get());
        buf.f_fsid    = unsafe { c::uuid_to_fsid(uuid.as_mut_ptr()) };
        buf.f_namelen = c::BCH_NAME_MAX as _;
        Ok(())
    }

    fn show_devname(&self, m: &SeqFile, _root: Dentry<'_>) -> Result<(), BchError> {
        let rcu = read_lock();
        for (i, ca) in self.online_devs_rcu(&rcu).enumerate() {
            if i > 0 {
                m.write(b":");
            }
            // SAFETY: an online device's, set while it's open
            m.write(unsafe { CStr::from_ptr(ca.disk_sb.sb_name.cast()) }.to_bytes());
        }
        Ok(())
    }

    fn show_options(&self, m: &SeqFile, _root: Dentry<'_>) -> Result<(), BchError> {
        let mut out = Printbuf::new();
        // No superblock: with the filesystem, every option's to_text reads it
        crate::opts::opts_to_text(&mut out, self.opts(), Some(self), None, Some(&self.mount_opts),
                                  c::opt_flags::OPT_MOUNT.bits(), c::opt_flags::OPT_HIDDEN.bits(),
                                  c::OPT_SHOW_MOUNT_STYLE);
        if out.allocation_failure() {
            return Err(BchError::from(c::ENOMEM));
        }
        m.write(b",");
        m.write(out.as_bytes());
        Ok(())
    }

    fn put_super(&self, _sb: &SuperBlock) {
        unsafe { c::bch2_fs_stop(self.as_ptr()) };
    }

    fn freeze_fs(&self, _sb: &SuperBlock) -> Result<(), BchError> {
        let _state = Lock::from_c(&self.state_lock).write();
        self.read_only();
        Ok(())
    }

    fn unfreeze_fs(&self, _sb: &SuperBlock) -> Result<(), BchError> {
        if self.flag(c::bch_fs_flags::BCH_FS_emergency_ro) {
            return Ok(());
        }
        let _state = Lock::from_c(&self.state_lock).write();
        ret_to_result_void(unsafe { c::bch2_fs_read_write(self.as_ptr()) })
    }
}

#[c_export]
pub static bch2_super_ops: c::super_operations = SuperVTable::<Fs>::OPS;
