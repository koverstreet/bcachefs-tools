// SPDX-License-Identifier: GPL-2.0

//! The module, in a process: init/module.rs's bcachefs_init() run as a
//! constructor, at the shim's module_init() priority - after its percpu and
//! scheduler setup. There's no exit but a failed init's.

#[path = "../module.rs"]
mod common;
pub use common::*;

use crate::init::module::{bcachefs_exit, bcachefs_init};

#[used]
#[unsafe(link_section = ".init_array.00120")]
static INIT: extern "C" fn() = {
    extern "C" fn init() {
        if let Err(e) = bcachefs_init(&ThisModule::new()) {
            bcachefs_exit(&ModuleExit::new());
            panic!("bcachefs_init: {e}");
        }
    }
    init
};
