// SPDX-License-Identifier: GPL-2.0

//! The module's entry points, into init/module.rs's bcachefs_init() and
//! bcachefs_exit(). Ours, not the kernel crate's module!()'s, as
//! module_param.rs's parameters are: what it generates differs from
//! kernel to kernel, and is edition 2021's. In a module, init_module() and
//! cleanup_module(), as C's module_init() and module_exit() alias them; built
//! in, an initcall.

#[path = "../module.rs"]
mod common;
pub use common::*;

use crate::init::module::{bcachefs_exit, bcachefs_init};
use core::ffi::c_int;

crate::init_fn! {
    /// A failed init is exited, and is ENOMEM, as C's.
    fn init() -> c_int {
        match bcachefs_init(&ThisModule::new()) {
            Ok(()) => 0,
            Err(_) => {
                bcachefs_exit(&ModuleExit::new());
                -(crate::c::ENOMEM as c_int)
            }
        }
    }
}

#[cfg(MODULE)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".init.text")]
pub extern "C" fn init_module() -> c_int {
    init()
}

#[cfg(MODULE)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".exit.text")]
pub extern "C" fn cleanup_module() {
    bcachefs_exit(&ModuleExit::new())
}

// ___ADDRESSABLE(), as module_init() and module_exit() have: what's called
// only through its address keeps its ENDBR, which objtool seals off otherwise.
#[cfg(MODULE)]
#[used(compiler)]
#[unsafe(link_section = ".init.data")]
static INIT_MODULE_ADDRESSABLE: extern "C" fn() -> c_int = init_module;

#[cfg(MODULE)]
#[used(compiler)]
#[unsafe(link_section = ".exit.data")]
static CLEANUP_MODULE_ADDRESSABLE: extern "C" fn() = cleanup_module;

#[cfg(not(MODULE))]
#[unsafe(link_section = ".init.text")]
extern "C" fn initcall() -> c_int {
    init()
}

#[cfg(all(not(MODULE), not(CONFIG_HAVE_ARCH_PREL32_RELOCATIONS)))]
#[used(compiler)]
#[unsafe(link_section = ".initcall6.init")]
static INITCALL: extern "C" fn() -> c_int = initcall;

#[cfg(all(not(MODULE), CONFIG_HAVE_ARCH_PREL32_RELOCATIONS))]
core::arch::global_asm!(
    ".section \".initcall6.init\", \"a\"",
    ".long {initcall} - .",
    ".previous",
    initcall = sym initcall,
);
