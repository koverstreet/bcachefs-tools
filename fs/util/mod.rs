pub mod alloc;
pub mod backref;
pub mod backtrace;
pub mod bio;
pub mod clock;
pub mod closure;
#[cfg(CONFIG_DEBUG_FS)]
pub mod debugfs;
pub mod demangle;
pub mod ffi;
pub mod futex;
pub mod gather;
pub mod hash;
pub mod ioctl;
pub mod jump_label;
pub mod kmem_cache;
pub mod late;
pub mod log;
pub mod maybe_rcu_alloc;
pub mod mean_and_variance;
pub mod mempool;
pub mod module_param;
pub mod names;
pub mod os_str;
pub mod par;
pub mod percpu;
pub mod place;
#[cfg(not(kernel))]
pub mod plymouth;
pub mod printbuf;
pub mod projections;
pub mod prompt;
pub mod raid;
pub mod rcu;
pub mod rcu_pending;
pub mod refcount;
pub mod siphash;
pub mod sleep;
pub mod task;
pub mod time_stats;
pub mod varint;
pub mod volatile;
pub mod vstructs;

pub use printbuf::{Bytes, DateTime, HumanReadable, Join, Nsecs, Printbuf, Sectors, ToText};

/// The largest value @_x's type holds: a field's, without naming its type -
/// `max_of(&k.size)`, C's KEY_SIZE_MAX.
pub fn max_of<T: num_traits::Bounded>(_x: &T) -> T {
    T::max_value()
}
