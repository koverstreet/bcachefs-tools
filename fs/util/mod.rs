pub mod alloc;
pub mod async_exec;
pub mod bitmap;
pub mod darray;
pub mod eytzinger;
pub mod ffi;
pub mod genradix;
#[cfg(not(kernel))]
pub mod ioctl;
pub mod kernel;
pub mod locking;
pub mod log;
pub mod module_param;
pub mod os_str;
pub mod printbuf;
pub mod rcu;
pub mod sync;
pub mod varint;
pub mod vstructs;

pub use printbuf::Printbuf;

/// The largest value @_x's type holds: a field's, without naming its type -
/// `max_of(&k.size)`, C's KEY_SIZE_MAX.
pub fn max_of<T: num_traits::Bounded>(_x: &T) -> T {
    T::max_value()
}
