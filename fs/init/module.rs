// SPDX-License-Identifier: GPL-2.0

//! The module: its init and exit, and /sys/fs/bcachefs, where each
//! filesystem's directory is. What runs them - the kernel's entry points, or
//! userspace's constructor - is the port's module.rs.

use crate::errcode::{ret_to_result_void, BchError};
use crate::os::kobject::{fs_kobj, StaticKobject};
use crate::os::module::{ModuleExit, ThisModule};

#[cfg(kernel)]
crate::module_info!("license" = "GPL");
#[cfg(kernel)]
crate::module_info!("author" = "Kent Overstreet <kent.overstreet@gmail.com>");
#[cfg(kernel)]
crate::module_info!("description" = "bcachefs filesystem");
#[cfg(all(kernel, not(MODULE)))]
crate::module_info!("file" = env!("RUST_MODFILE"));

crate::module_param! {
    #[cfg(kernel)]
    static version: u32 = crate::c::MetadataVersion::CURRENT.0;
    perm = 0o444
}

/// /sys/fs/bcachefs.
pub(crate) static BCACHEFS_KOBJ: StaticKobject = StaticKobject::new();

/* Global interfaces/init */

/// Undoes what of bcachefs_init() was done: each exit a no-op on what wasn't
/// made.
pub(crate) fn bcachefs_exit(m: &ModuleExit) {
    #[cfg(CONFIG_DEBUG_FS)]
    crate::debug::debug::bch2_debug_exit(m);
    #[cfg(kernel)]
    crate::init::chardev::bch2_chardev_exit(m);
    crate::btree::lock_graph::graphs_exit();
    BCACHEFS_KOBJ.put();

    // what's freed a grace period on runs our code, and goes back to our
    // caches and pools
    let q = m.quiesce();
    #[cfg(not(NO_BCACHEFS_FS))]
    crate::vfs::new_inode::vfs_exit(&q);
    crate::btree::key_cache::btree_key_cache_module_exit(&q);
    crate::util::maybe_rcu_alloc::free_exit(&q);
    #[cfg(not(NO_BCACHEFS_FS))]
    {
        crate::vfs::direct::pools_exit(&q);
        crate::vfs::direct_write::pools_exit(&q);
        crate::vfs::writeback::pools_exit(&q);
        crate::vfs::buffered::pools_exit(&q);
    }
    crate::data::read::pools_exit(&q);
    crate::data::write_replicas::pools_exit(&q);
    crate::data::write::pools_exit(&q);
    crate::btree::read::pools_exit(&q);
    crate::btree::write::pools_exit(&q);
    crate::util::task::TASKS.exit(&q);
}

/// Failed, bcachefs_exit() undoes it.
#[cfg_attr(kernel, unsafe(link_section = ".init.text"))]
pub(crate) fn bcachefs_init(m: &ThisModule) -> Result<(), BchError> {
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    crate::btree::bkey::pack_test();

    crate::util::futex::bch2_futex_init();

    BCACHEFS_KOBJ.init();

    crate::util::maybe_rcu_alloc::free_init()?;
    crate::util::task::TASKS.init(m)?;
    crate::data::read::pools_init(m)?;
    crate::data::write_replicas::pools_init(m)?;
    crate::data::write::pools_init(m)?;
    crate::btree::read::pools_init(m)?;
    crate::btree::write::pools_init(m)?;
    BCACHEFS_KOBJ.kobj().add(fs_kobj(), c"bcachefs")?;
    crate::btree::lock_graph::graphs_init()?;
    crate::btree::key_cache::btree_key_cache_module_init()?;
    #[cfg(kernel)]
    crate::init::chardev::bch2_chardev_init(m)?;
    #[cfg(not(NO_BCACHEFS_FS))]
    crate::vfs::new_inode::vfs_init(m)?;
    #[cfg(CONFIG_DEBUG_FS)]
    ret_to_result_void(crate::debug::debug::bch2_debug_init())?;
    Ok(())
}
