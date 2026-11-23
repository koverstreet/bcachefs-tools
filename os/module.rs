// SPDX-License-Identifier: GPL-2.0

//! The module's lifetime, as proof: what may run only at its init, or its
//! exit, takes one of these - made only by the port's module.rs, around
//! init/module.rs's bcachefs_init() and bcachefs_exit().

use core::marker::PhantomData;

/// The module's init is running: nothing of ours runs but it.
pub struct ThisModule(());

/// The module's exit is running, or a failed init's: no filesystem is left, or
/// will be.
pub struct ModuleExit(());

/// Module exit, past maybe_rcu_alloc::barrier(): every free begun - after a
/// grace period, or on the free queue - has run, its memory back where it
/// came from, and with no filesystem, none's to come.
pub struct Quiesced<'m>(PhantomData<&'m ModuleExit>);

impl ThisModule {
    pub(super) const fn new() -> Self {
        ThisModule(())
    }
}

/// A ref on the module, which its unload waits out: __module_get(), put as it
/// drops. Userspace has no modules, so it's nothing.
pub struct ModuleRef(());

impl ModuleRef {
    pub fn get() -> Self {
        #[cfg(kernel)]
        unsafe { crate::c::__module_get(&raw mut kernel::bindings::__this_module) };
        ModuleRef(())
    }
}

impl Drop for ModuleRef {
    fn drop(&mut self) {
        #[cfg(kernel)]
        unsafe { crate::c::module_put(&raw mut kernel::bindings::__this_module) };
    }
}

impl ModuleExit {
    pub(super) const fn new() -> Self {
        ModuleExit(())
    }

    pub fn quiesce(&self) -> Quiesced<'_> {
        crate::util::maybe_rcu_alloc::barrier();
        Quiesced(PhantomData)
    }
}
