pub mod alloc;
pub mod bio;
pub mod bitmap;
pub mod closure;
pub mod darray;
#[cfg(CONFIG_DEBUG_FS)]
pub mod debugfs;
pub mod eytzinger;
pub mod ffi;
pub mod futex;
pub mod gather;
pub mod genradix;
pub mod inline_vec;
#[cfg(not(kernel))]
pub mod ioctl;
pub mod jump_label;
pub mod kernel;
pub mod kthread;
pub mod list;
pub mod log;
pub mod module_param;
pub mod os_str;
pub mod par;
pub mod percpu;
pub mod printbuf;
pub mod projections;
pub mod rcu;
pub mod rhashtable;
pub mod siphash;
pub mod sleep;
pub mod strided;
pub mod sysfs;
pub mod task;
pub mod varint;
pub mod vstructs;
pub mod workqueue;

pub use printbuf::{HumanReadable, Printbuf, ToText};

/// The largest value @_x's type holds: a field's, without naming its type -
/// `max_of(&k.size)`, C's KEY_SIZE_MAX.
pub fn max_of<T: num_traits::Bounded>(_x: &T) -> T {
    T::max_value()
}
