use std::{marker::PhantomData, mem::ManuallyDrop};

/// A bindgen-generated bcachefs darray type.
///
/// # Safety
///
/// Implementations must expose the standard bcachefs darray layout:
/// `nr`, `size`, and `data`, where `data` points to `size` elements of `T`.
pub unsafe trait Darray<T>: Default {
    fn nr(&self) -> usize;
    fn data(&mut self) -> *mut T;
    fn set_vec_storage(&mut self, data: *mut T, nr: usize, size: usize);
}

/// Owns a Rust `Vec<T>` while presenting it to C as a bcachefs darray.
///
/// This is only for C functions that compact/remove entries by changing `nr`;
/// they must not reallocate `data` with `darray_make_room()`.
pub struct DarrayVec<D, T>
where
    D: Darray<T>,
{
    darray: D,
    vec:    ManuallyDrop<Vec<T>>,
    _ty:    PhantomData<T>,
}

impl<D, T> DarrayVec<D, T>
where
    D: Darray<T>,
{
    pub fn from_vec(vec: Vec<T>) -> Self {
        let mut vec = ManuallyDrop::new(vec);
        let mut darray = D::default();

        darray.set_vec_storage(vec.as_mut_ptr(), vec.len(), vec.capacity());

        Self {
            darray,
            vec,
            _ty: PhantomData,
        }
    }

    pub fn as_mut(&mut self) -> &mut D {
        &mut self.darray
    }

    pub fn into_vec(self) -> Vec<T> {
        let mut this = ManuallyDrop::new(self);

        unsafe {
            Vec::from_raw_parts(
                this.darray.data(),
                this.darray.nr(),
                this.vec.capacity(),
            )
        }
    }
}

impl<D, T> Drop for DarrayVec<D, T>
where
    D: Darray<T>,
{
    fn drop(&mut self) {
        unsafe {
            drop(Vec::from_raw_parts(
                self.darray.data(),
                self.darray.nr(),
                self.vec.capacity(),
            ));
        }
    }
}

#[macro_export]
macro_rules! impl_darray {
    ($darray:ty, $item:ty) => {
        unsafe impl $crate::darray::Darray<$item> for $darray {
            fn nr(&self) -> usize {
                self.nr
            }

            fn data(&mut self) -> *mut $item {
                self.data
            }

            fn set_vec_storage(&mut self, data: *mut $item, nr: usize, size: usize) {
                self.data = data;
                self.nr = nr;
                self.size = size;
            }
        }
    };
}
