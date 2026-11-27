pub mod alloc;
pub mod atomic;
pub mod backref;
pub mod backtrace;
pub mod clock;
pub mod demangle;
pub mod ffi;
pub mod futex;
pub mod gather;
pub mod hash;
pub mod ioctl;
pub mod late;
pub mod loan;
pub mod log;
pub mod maybe_rcu_alloc;
pub mod mean_and_variance;
pub mod mempool;
pub mod names;
pub mod once_either;
pub mod os_str;
pub mod par;
pub mod place;
#[cfg(not(kernel))]
pub mod plymouth;
pub mod printbuf;
pub mod projections;
pub mod prompt;
pub mod raid;
pub mod ratelimit;
pub mod rcu;
pub mod rcu_pending;
pub mod refcount;
pub mod sharing;
pub mod siphash;
pub mod sleep;
pub mod task;
pub mod thin;
#[cfg(kernel)]
pub mod thread_with_file;
pub mod time_stats;
pub mod timer;
pub mod varint;
pub mod volatile;
pub mod vstructs;

pub use printbuf::{Bytes, DateTime, HumanReadable, Join, Nsecs, Printbuf, Sectors, ToText};

/// The largest value @_x's type holds: a field's, without naming its type -
/// `max_of(&k.size)`, C's KEY_SIZE_MAX.
pub fn max_of<T: num_traits::Bounded>(_x: &T) -> T {
    T::max_value()
}

// memzero_explicit(): volatile writes + a compiler fence, so the store
// can't be optimized away as dead - the wipe is the point (the technique
// the `zeroize` crate uses internally). The keys' Drop in-kernel, which
// has no `zeroize`; and in both builds, key material that isn't a Key.
#[cfg(not(feature = "std"))]
pub(crate) fn wipe<T: ?Sized>(v: &mut T) {
    let len = core::mem::size_of_val(v);
    let p = (v as *mut T).cast::<u8>();
    for i in 0..len {
        unsafe { core::ptr::write_volatile(p.add(i), 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
