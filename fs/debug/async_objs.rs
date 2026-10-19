// SPDX-License-Identifier: GPL-2.0

//! debugfs's async_objs/: a file per list of in-flight bios and write ops.
//! The lists are async_objs.c's; kernel only, as they are.
//!
//! XXX: use after free, C's too: the walk is under RCU, but objects are freed
//! at completion - bio_put(), end_io() - with no grace period. Needs
//! async_objs.c in Rust, so fast_list can be typed (its XXX) with an unsafe
//! "freed after a grace period" assertion, as RcuPtr::assign(); then each
//! list's objects RCU-freed, or refcounted against the walker, or removal
//! excluded.

use core::ffi::CStr;
use core::mem::{offset_of, size_of};
use core::ptr::NonNull;

use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::debugfs::{self, DebugFile, DebugOut};
use crate::util::ffi::Opaque;
use crate::util::rcu;

impl c::async_obj_list {
    /// The filesystem it's one of the async_objs[] of.
    fn fs(&self) -> &Fs {
        let offset = offset_of!(Fs, async_objs) + self.idx as usize * size_of::<Self>();
        // SAFETY: a bch_fs's async_objs[idx], which bch2_fs_async_obj_init() numbered
        unsafe { &*core::ptr::from_ref(self).byte_sub(offset).cast::<Fs>() }
    }
}

/// The cursor is the next slot.
#[derive(Default)]
struct AsyncObjList {
    next: usize,
}

impl DebugFile for AsyncObjList {
    type Obj = c::async_obj_list;

    // bch2_async_obj_list_open() didn't set may_vmalloc
    const MAY_VMALLOC: bool = false;

    fn read(&mut self, list: &c::async_obj_list, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let fs = list.fs();
        let to_text = list.obj_to_text.expect("async_obj_list without its obj_to_text");

        out.print_each(|out| {
            let rcu = rcu::read_lock();
            let Some((idx, obj)) = list.list.items_from(self.next, &rcu).next() else {
                return Ok(false);
            };

            // atomic: under RCU, the printbuf mustn't sleep to grow. SAFETY:
            // an object of the list's kind - but see the XXX
            unsafe { to_text(out.atomic().as_raw(), core::ptr::from_ref(fs).cast_mut(), obj.as_ptr()) };
            self.next = idx + 1;
            Ok(true)
        })
    }
}

/// Indexed as async_objs[].
macro_rules! list_names {
    ($(($name:ident)),* $(,)?) => {
        const LIST_NAMES: [&CStr; c::BCH_ASYNC_OBJ_NR as usize] = [$(
            match CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()) {
                Ok(name) => name,
                Err(_)   => panic!(),
            },
        )*];
    };
}
c::BCH_ASYNC_OBJ_LISTS!(list_names);

/// bch2_fs_debug_init()'s, in its directory @parent.
pub(crate) fn debugfs_init(c: &Opaque<c::bch_fs>, parent: NonNull<c::dentry>) {
    let dir = debugfs::create_dir(c"async_objs", Some(parent));
    // SAFETY: filesystem init, before anything reads it
    unsafe { (*c.as_ptr()).async_obj_dir = dir.map_or(core::ptr::null_mut(), NonNull::as_ptr) };

    let Some(dir) = dir else { return };
    let fs = Fs::from_c(c);
    for (list, name) in fs.async_objs.iter().zip(LIST_NAMES) {
        // SAFETY: bch2_fs_debug_exit() removes them first
        unsafe { debugfs::create_file::<AsyncObjList>(name, dir, list) };
    }
}
