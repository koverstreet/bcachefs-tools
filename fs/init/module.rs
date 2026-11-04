// SPDX-License-Identifier: GPL-2.0

//! The module: its init and exit, and /sys/fs/bcachefs, where each
//! filesystem's directory is.
//!
//! The entry points are ours, not the kernel crate's module!()'s, as
//! util/module_param.rs's parameters are: what it generates differs from
//! kernel to kernel, and is edition 2021's. In a module, init_module() and
//! cleanup_module(), as C's module_init() and module_exit() alias them; built
//! in, an initcall. Userspace runs init as a constructor, at the shim's
//! module_init() priority: after its percpu and scheduler setup.

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::kobject::{GroupList, Groups, KobjOwner, KobjPlace, Kobject};
use crate::util::sysfs::Dir;
use core::ffi::c_int;
use core::ptr::NonNull;
use cstruct_macros::c_extern;

c_extern! {
    fn bch2_dirent_init();
    fn bch2_lock_graph_init() -> c_int;
    fn bch2_lock_graph_exit();
    fn bch2_btree_key_cache_init() -> c_int;
    fn bch2_btree_key_cache_exit();
}

#[cfg(not(NO_BCACHEFS_FS))]
c_extern! {
    fn bch2_vfs_init() -> c_int;
    fn bch2_vfs_exit();
}

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
    static version: u32 = c::bcachefs_metadata_version_current as u32;
    perm = 0o444
}

/// /sys/fs/bcachefs.
#[repr(transparent)]
pub(crate) struct Root(pub(crate) Kobject);

pub(crate) static BCACHEFS_KOBJ: Root = Root(unsafe { core::mem::zeroed() });

static NO_GROUPS: Groups<1> = Groups::new([]);

unsafe impl Dir for Root {
    type Obj = Root;
    type Kobj = Root;
}

unsafe impl KobjPlace for Root {
    type Owner = Root;

    unsafe fn owner(kobj: *mut c::kobject) -> *mut Root {
        kobj.cast()
    }
}

unsafe impl KobjOwner for Root {
    const GROUPS: GroupList = NO_GROUPS.list();
    type Dir = Root;

    unsafe fn release(_: NonNull<Self>) {}
}

/* Global interfaces/init */

fn bcachefs_exit() {
    #[cfg(CONFIG_DEBUG_FS)]
    crate::debug::debug::bch2_debug_exit();
    #[cfg(not(NO_BCACHEFS_FS))]
    unsafe { bch2_vfs_exit() };
    #[cfg(kernel)]
    crate::util::bio::dirty_pages_exit();
    #[cfg(kernel)]
    crate::init::chardev::bch2_chardev_exit();
    unsafe {
        bch2_btree_key_cache_exit();
        bch2_lock_graph_exit();
        Kobject::put(&BCACHEFS_KOBJ.0);
    }
    // what's freed a grace period on runs our code
    crate::util::maybe_rcu_alloc::barrier();
    crate::util::maybe_rcu_alloc::free_exit();
}

#[cfg_attr(kernel, unsafe(link_section = ".init.text"))]
fn bcachefs_init() -> c_int {
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    crate::btree::bkey::pack_test();

    #[cfg(kernel)]
    crate::util::bio::dirty_pages_init();
    crate::util::futex::bch2_futex_init();
    unsafe { bch2_dirent_init() };
    crate::init::registry::bch2_fs_registry_init();

    unsafe { BCACHEFS_KOBJ.0.init::<Root>() };

    if init_interfaces().is_err() {
        bcachefs_exit();
        return -(c::ENOMEM as c_int);
    }
    0
}

#[cfg_attr(kernel, unsafe(link_section = ".init.text"))]
fn init_interfaces() -> Result<(), BchError> {
    #[cfg(kernel)]
    let parent = Some(unsafe { Kobject::from_raw(c::fs_kobj) });
    #[cfg(not(kernel))]
    let parent = None;

    crate::util::maybe_rcu_alloc::free_init()?;
    BCACHEFS_KOBJ.0.add(parent, c"bcachefs")?;
    ret_to_result_void(unsafe { bch2_lock_graph_init() })?;
    ret_to_result_void(unsafe { bch2_btree_key_cache_init() })?;
    #[cfg(kernel)]
    ret_to_result_void(crate::init::chardev::bch2_chardev_init())?;
    #[cfg(not(NO_BCACHEFS_FS))]
    ret_to_result_void(unsafe { bch2_vfs_init() })?;
    #[cfg(CONFIG_DEBUG_FS)]
    ret_to_result_void(crate::debug::debug::bch2_debug_init())?;
    Ok(())
}

#[cfg(all(kernel, MODULE))]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".init.text")]
pub extern "C" fn init_module() -> c_int {
    bcachefs_init()
}

#[cfg(all(kernel, MODULE))]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".exit.text")]
pub extern "C" fn cleanup_module() {
    bcachefs_exit()
}

// ___ADDRESSABLE(), as module_init() and module_exit() have: what's called
// only through its address keeps its ENDBR, which objtool seals off otherwise.
#[cfg(all(kernel, MODULE))]
#[used(compiler)]
#[unsafe(link_section = ".init.data")]
static INIT_MODULE_ADDRESSABLE: extern "C" fn() -> c_int = init_module;

#[cfg(all(kernel, MODULE))]
#[used(compiler)]
#[unsafe(link_section = ".exit.data")]
static CLEANUP_MODULE_ADDRESSABLE: extern "C" fn() = cleanup_module;

#[cfg(all(kernel, not(MODULE)))]
#[unsafe(link_section = ".init.text")]
extern "C" fn initcall() -> c_int {
    bcachefs_init()
}

#[cfg(all(kernel, not(MODULE), not(CONFIG_HAVE_ARCH_PREL32_RELOCATIONS)))]
#[used(compiler)]
#[unsafe(link_section = ".initcall6.init")]
static INITCALL: extern "C" fn() -> c_int = initcall;

#[cfg(all(kernel, not(MODULE), CONFIG_HAVE_ARCH_PREL32_RELOCATIONS))]
core::arch::global_asm!(
    ".section \".initcall6.init\", \"a\"",
    ".long {initcall} - .",
    ".previous",
    initcall = sym initcall,
);

#[cfg(not(kernel))]
#[used]
#[unsafe(link_section = ".init_array.00120")]
static INIT: extern "C" fn() = {
    extern "C" fn init() {
        assert!(bcachefs_init() == 0, "bcachefs_init");
    }
    init
};
