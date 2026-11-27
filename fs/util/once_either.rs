// SPDX-License-Identifier: GPL-2.0

//! OnceEither: an owning pointer to an L or an R, empty until it's set - once,
//! by a CAS from null, from any thread through &self. Which it holds is the
//! pointer's low bit. What it points to doesn't move while it's borrowed, and
//! is freed with it.

use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering::{Acquire, Relaxed, Release}};
use crate::util::alloc::{stateless, Alloc, Box, Global};
use crate::util::rcu::rcu_dereference;
use pin_init::Zeroable;

pub enum Either<L, R> {
    Left(L),
    Right(R),
}

const RIGHT: usize = 1;

pub struct OnceEither<L, R, A: Alloc = Global> {
    p:  AtomicPtr<u8>,
    _t: PhantomData<(Box<L, A>, Box<R, A>, *mut ())>,
}

// What it holds is shared between threads, and may be freed on another than
// set it
unsafe impl<L: Send, R: Send, A: Alloc> Send for OnceEither<L, R, A> {}
unsafe impl<L: Send + Sync, R: Send + Sync, A: Alloc> Sync for OnceEither<L, R, A> {}

// null: empty
unsafe impl<L, R, A: Alloc> Zeroable for OnceEither<L, R, A> {}

impl<L, R, A: Alloc> OnceEither<L, R, A> {
    const TAGGABLE: () = assert!(align_of::<L>() > 1 && align_of::<R>() > 1,
                                 "OnceEither: no low bit to say which it holds");

    pub const fn new() -> Self {
        OnceEither { p: AtomicPtr::new(ptr::null_mut()), _t: PhantomData }
    }

    fn decode(p: *mut u8) -> Option<Either<*mut L, *mut R>> {
        if p.is_null() {
            None
        } else if p.addr() & RIGHT == 0 {
            Some(Either::Left(p.cast()))
        } else {
            Some(Either::Right(p.map_addr(|a| a & !RIGHT).cast()))
        }
    }

    /// # Safety
    /// @e is what it holds, not freed while the borrow lives.
    unsafe fn borrow<'a>(e: Either<*mut L, *mut R>) -> Either<&'a L, &'a R> {
        match e {
            Either::Left(p)  => Either::Left(unsafe { &*p }),
            Either::Right(p) => Either::Right(unsafe { &*p }),
        }
    }

    pub fn get(&self) -> Option<Either<&L, &R>> {
        Self::decode(rcu_dereference(&self.p)).map(|e| unsafe { Self::borrow(e) })
    }

    pub fn get_mut(&mut self) -> Option<Either<&mut L, &mut R>> {
        Self::decode(*self.p.get_mut()).map(|e| match e {
            Either::Left(p)  => Either::Left(unsafe { &mut *p }),
            Either::Right(p) => Either::Right(unsafe { &mut *p }),
        })
    }

    /// @p, a tagged Box, in if it's empty: what it holds after, and whether
    /// that's @p - if not, @p is still the caller's.
    fn set(&self, p: *mut u8) -> (Either<&L, &R>, bool) {
        let () = Self::TAGGABLE;
        match self.p.compare_exchange(ptr::null_mut(), p, Release, Relaxed) {
            Ok(_)    => (unsafe { Self::borrow(Self::decode(p).expect("a box")) }, true),
            Err(cur) => (unsafe { Self::borrow(Self::decode(cur).expect("set")) }, false),
        }
    }

    /// @l in, if it's empty: what it holds after, and @l back if another got
    /// there first.
    pub fn get_or_set_left(&self, l: Box<L, A>) -> (Either<&L, &R>, Option<Box<L, A>>) {
        let (p, _) = Box::into_raw_with_allocator(l);
        match self.set(p.cast()) {
            (e, true)  => (e, None),
            (e, false) => (e, Some(unsafe { Box::from_raw_in(p, stateless()) })),
        }
    }

    pub fn get_or_set_right(&self, r: Box<R, A>) -> (Either<&L, &R>, Option<Box<R, A>>) {
        let (p, _) = Box::into_raw_with_allocator(r);
        match self.set(p.cast::<u8>().map_addr(|a| a | RIGHT)) {
            (e, true)  => (e, None),
            (e, false) => (e, Some(unsafe { Box::from_raw_in(p, stateless()) })),
        }
    }

    /// What it holds, by &mut - an L from @l if it's empty.
    pub fn get_mut_or_set_left<E>(&mut self, l: impl FnOnce() -> Result<Box<L, A>, E>)
        -> Result<Either<&mut L, &mut R>, E>
    {
        if self.p.get_mut().is_null() {
            let () = Self::TAGGABLE;
            *self.p.get_mut() = Box::into_raw_with_allocator(l()?).0.cast();
        }
        Ok(self.get_mut().expect("set"))
    }

    pub fn get_mut_or_set_right<E>(&mut self, r: impl FnOnce() -> Result<Box<R, A>, E>)
        -> Result<Either<&mut L, &mut R>, E>
    {
        if self.p.get_mut().is_null() {
            let () = Self::TAGGABLE;
            *self.p.get_mut() = Box::into_raw_with_allocator(r()?).0.cast::<u8>().map_addr(|a| a | RIGHT);
        }
        Ok(self.get_mut().expect("set"))
    }

    /// # Safety
    /// @p was what it held, and nothing else has it now.
    unsafe fn unbox(p: *mut u8) -> Option<Either<Box<L, A>, Box<R, A>>> {
        Self::decode(p).map(|e| match e {
            Either::Left(p)  => Either::Left(unsafe { Box::from_raw_in(p, stateless()) }),
            Either::Right(p) => Either::Right(unsafe { Box::from_raw_in(p, stateless()) }),
        })
    }

    /// What it holds, out: empty after.
    pub fn take(&mut self) -> Option<Either<Box<L, A>, Box<R, A>>> {
        unsafe { Self::unbox(core::mem::replace(self.p.get_mut(), ptr::null_mut())) }
    }

    /// take(), by &self.
    ///
    /// # Safety
    /// Nothing borrows what it holds, nor will.
    pub unsafe fn take_shared(&self) -> Option<Either<Box<L, A>, Box<R, A>>> {
        unsafe { Self::unbox(self.p.swap(ptr::null_mut(), Acquire)) }
    }
}

impl<L, R, A: Alloc> Drop for OnceEither<L, R, A> {
    fn drop(&mut self) {
        drop(self.take());
    }
}
