// SPDX-License-Identifier: GPL-2.0

//! The ioctl handlers' side of util/ioctl.rs's numbers. A handler never sees
//! the user's pointer for an argument of fixed size: input(), output() and
//! inout() copy it as its number's direction says - in, out, or in and back
//! out on success - no_arg() is _IO's, and a direction they don't take
//! doesn't compile. A Head<T> argument is the head of a variable-length one,
//! and there the number's direction is the head's alone - an array after it
//! may go either way, as BCH_IOCTL_QUERY_COUNTERS's does: vstruct() hands the
//! handler a UserVstruct, which reads and writes the head, and the array as
//! the head counts it - where it starts, what it holds and how many, by the
//! type's #[counted] field - no further. An argument whose array the type
//! doesn't count - entries of varying size, the head giving their bytes - is
//! arg()'s: a UserArg, read and written whole, and the bytes at its array's
//! offset. What a handler returns, IoctlRet, is the syscall's value; its
//! errors are returned by the dispatch, as their class.

use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::KVVec;
use crate::util::ioctl::{dir, Ioctl};
use crate::util::vstructs::{FlexArray, Head, Unsized};
use core::ffi::CStr;
use core::marker::PhantomData;
use core::mem::size_of;
use kernel::ffi::{c_int, c_long};
use kernel::transmute::{AsBytes, FromBytes};
use kernel::uaccess::{UserPtr, UserSlice};

pub trait IoctlRet {
    fn ret(self) -> c_long;
}

impl IoctlRet for () {
    fn ret(self) -> c_long {
        0
    }
}

impl IoctlRet for c_int {
    fn ret(self) -> c_long {
        self as c_long
    }
}

impl IoctlRet for c_long {
    fn ret(self) -> c_long {
        self
    }
}

pub fn input<I, R>(user: UserPtr, f: impl FnOnce(I::Arg) -> Result<R, BchError>)
    -> Result<c_long, BchError>
where
    I: Ioctl<Dir = dir::W>,
    I::Arg: FromBytes,
    R: IoctlRet,
{
    let arg = UserSlice::new(user, size_of::<I::Arg>()).reader().read()?;
    Ok(f(arg)?.ret())
}

pub fn no_arg<I, R>(f: impl FnOnce() -> Result<R, BchError>) -> Result<c_long, BchError>
where
    I: Ioctl<Dir = dir::IO>,
    R: IoctlRet,
{
    Ok(f()?.ret())
}

pub fn output<I>(user: UserPtr, f: impl FnOnce() -> Result<I::Arg, BchError>)
    -> Result<c_long, BchError>
where
    I: Ioctl<Dir = dir::R>,
    I::Arg: AsBytes,
{
    let arg = f()?;
    UserSlice::new(user, size_of::<I::Arg>()).writer().write(&arg)?;
    Ok(0)
}

pub fn inout<I, R>(user: UserPtr, f: impl FnOnce(&mut I::Arg) -> Result<R, BchError>)
    -> Result<c_long, BchError>
where
    I: Ioctl<Dir = dir::WR>,
    I::Arg: FromBytes + AsBytes,
    R: IoctlRet,
{
    let (mut r, mut w) = UserSlice::new(user, size_of::<I::Arg>()).reader_writer();
    let mut arg = r.read()?;
    let ret = f(&mut arg)?.ret();
    w.write(&arg)?;
    Ok(ret)
}

pub fn vstruct<I, T, R>(user: UserPtr, f: impl FnOnce(UserVstruct<T>) -> Result<R, BchError>)
    -> Result<c_long, BchError>
where
    I: Ioctl<Arg = Head<T>>,
    T: ?Sized + Unsized,
    R: IoctlRet,
{
    Ok(f(UserVstruct { at: user, _t: PhantomData })?.ret())
}

/// A string from userspace, shorter than its limit: EINVAL if it isn't,
/// as strndup_user().
pub struct UserStr(KVVec<u8>);

impl UserStr {
    pub fn read(addr: u64, max: usize) -> Result<Self, BchError> {
        let mut buf = KVVec::from_elem(0u8, max + 1)?;
        let len = UserSlice::new(UserPtr::from_addr(addr as usize), max + 1).reader()
            .strcpy_into_buf(&mut buf)?.to_bytes().len();
        if len >= max {
            return Err(c::EINVAL.into());
        }
        buf.truncate(len + 1);
        Ok(UserStr(buf))
    }

    pub fn as_cstr(&self) -> &CStr {
        CStr::from_bytes_with_nul(&self.0).expect("read up to its NUL")
    }
}

pub fn arg<I, R>(user: UserPtr, f: impl FnOnce(UserArg<I::Arg>) -> Result<R, BchError>)
    -> Result<c_long, BchError>
where
    I: Ioctl,
    R: IoctlRet,
{
    Ok(f(UserArg { at: user, _t: PhantomData })?.ret())
}

pub struct UserArg<T> {
    at: UserPtr,
    _t: PhantomData<T>,
}

impl<T> UserArg<T> {
    /// @len bytes from offset @off: its array's, from offset_of!().
    pub fn bytes_at(&self, off: usize, len: usize) -> UserSlice {
        UserSlice::new(self.at.wrapping_byte_add(off), len)
    }
}

impl<T: FromBytes> UserArg<T> {
    pub fn read(&self) -> Result<T, BchError> {
        Ok(UserSlice::new(self.at, size_of::<T>()).reader().read()?)
    }
}

impl<T: AsBytes> UserArg<T> {
    pub fn write(&self, v: &T) -> Result<(), BchError> {
        Ok(UserSlice::new(self.at, size_of::<T>()).writer().write(v)?)
    }
}

impl<T> UserArg<T> {
    /// Write one field, at offset @off - offset_of!()'s: put_user(), for
    /// what an ioctl returns of an argument it doesn't write back whole.
    pub fn write_at<V: AsBytes>(&self, off: usize, v: &V) -> Result<(), BchError> {
        Ok(self.bytes_at(off, size_of::<V>()).writer().write(v)?)
    }
}

pub struct UserVstruct<T: ?Sized> {
    at: UserPtr,
    _t: PhantomData<T>,
}

impl<T: ?Sized + Unsized> UserVstruct<T> where Head<T>: FlexArray {
    /// The array after the head, as long as @head says.
    pub fn tail(&self, head: &Head<T>) -> UserArray<<Head<T> as FlexArray>::Elem> {
        UserArray {
            at: self.at.wrapping_byte_add(<Head<T> as FlexArray>::TAIL),
            nr: head.nr(),
            _e: PhantomData,
        }
    }
}

impl<T: ?Sized + Unsized> UserVstruct<T> where Head<T>: FromBytes {
    pub fn head(&self) -> Result<Head<T>, BchError> {
        Ok(UserSlice::new(self.at, size_of::<Head<T>>()).reader().read()?)
    }
}

impl<T: ?Sized + Unsized> UserVstruct<T> where Head<T>: AsBytes {
    pub fn set_head(&self, head: &Head<T>) -> Result<(), BchError> {
        Ok(UserSlice::new(self.at, size_of::<Head<T>>()).writer().write(head)?)
    }
}

pub struct UserArray<E> {
    at: UserPtr,
    nr: usize,
    _e: PhantomData<E>,
}

impl<E> UserArray<E> {
    pub fn len(&self) -> usize {
        self.nr
    }

    fn slot(&self, i: usize) -> UserSlice {
        assert!(i < self.nr, "past the {} entries the head gives", self.nr);
        UserSlice::new(self.at.wrapping_byte_add(i * size_of::<E>()), size_of::<E>())
    }
}

impl<E: FromBytes> UserArray<E> {
    pub fn get(&self, i: usize) -> Result<E, BchError> {
        Ok(self.slot(i).reader().read()?)
    }
}

impl<E: AsBytes> UserArray<E> {
    pub fn set(&self, i: usize, v: &E) -> Result<(), BchError> {
        Ok(self.slot(i).writer().write(v)?)
    }
}
