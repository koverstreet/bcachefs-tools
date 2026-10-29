pub mod alloc;
pub mod backref;
pub mod backtrace;
pub mod bio;
pub mod bitmap;
pub mod clock;
pub mod closure;
pub mod cuckoo;
pub mod darray;
#[cfg(CONFIG_DEBUG_FS)]
pub mod debugfs;
pub mod demangle;
pub mod enumerated_ref;
pub mod eytzinger;
pub mod ffi;
pub mod fifo;
pub mod file;
pub mod futex;
pub mod gather;
pub mod genradix;
pub mod hash;
pub mod inline_vec;
pub mod ioctl;
pub mod jump_label;
pub mod kernel;
pub mod kthread;
pub mod list;
pub mod log;
pub mod mempool;
pub mod module_param;
pub mod names;
pub mod ops;
pub mod os_str;
pub mod par;
pub mod percpu;
pub mod place;
pub mod printbuf;
pub mod projections;
pub mod raid;
pub mod rcu;
pub mod refcount;
pub mod rhashtable;
#[cfg(kernel)]
pub mod seq_file;
pub mod siphash;
pub mod sleep;
pub mod sort;
pub mod strided;
pub mod sysfs;
pub mod task;
#[cfg(kernel)]
pub mod thread_with_file;
pub mod varint;
pub mod volatile;
pub mod vstructs;
pub mod workqueue;

pub use printbuf::{Bytes, DateTime, HumanReadable, Join, Nsecs, Printbuf, Sectors, ToText};

/// The largest value @_x's type holds: a field's, without naming its type -
/// `max_of(&k.size)`, C's KEY_SIZE_MAX.
pub fn max_of<T: num_traits::Bounded>(_x: &T) -> T {
    T::max_value()
}
