pub mod alloc;
pub mod backref;
pub mod backtrace;
pub mod bio;
pub mod clock;
pub mod closure;
#[cfg(CONFIG_DEBUG_FS)]
pub mod debugfs;
pub mod demangle;
pub mod enumerated_ref;
pub mod ffi;
pub mod file;
#[cfg(kernel)]
pub mod fs_context;
#[cfg(kernel)]
pub mod fs_type;
pub mod futex;
pub mod gather;
pub mod hash;
pub mod ioctl;
#[cfg(kernel)]
pub mod iov_iter;
pub mod jump_label;
pub mod kernel;
pub mod kobject;
pub mod kthread;
pub mod late;
pub mod log;
pub mod maybe_rcu_alloc;
pub mod mean_and_variance;
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
pub mod rcu_pending;
pub mod refcount;
#[cfg(kernel)]
pub mod seq_file;
pub mod siphash;
pub mod sleep;
#[cfg(kernel)]
pub mod super_block;
pub mod sysfs;
pub mod task;
pub mod time_stats;
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
