// SPDX-License-Identifier: GPL-2.0

//! Several completions, waited on until each is done, one or more at a time:
//! a Gather's dual, the sum to its product. A Select's inputs are completers
//! of it - DynDones borrowing it, made with it - each recording that it's
//! done, then completing the Select's Done if it's owed: the wake, which
//! the waiter's next wait re-arms. A wait returns what's done since the
//! last, as bits by input: nothing completed is lost, nor waited for twice.
//!
//! What holds an input cancels it as it drops - a Timer deletes its timer, a
//! futex wait takes its entry off, waiting out a completion under way - or,
//! on a waitlist with no cancel, finishes the wait: a journal flush's chained
//! Armable. So borrowing the Select, every input is done with it before it
//! goes:
//!
//! ```ignore
//! let mut slot = pin!(None);
//! let (mut wait, [woken, timed_out]) = Select::new(slot.as_mut(), waiter::Async::current().await);
//! stack_pin_init!(let t = Timer::new(timed_out, timeout));
//! ...                                     // @woken to another completer
//! let done = wait.next().resume_on(at).await;     // a thread: wait.wait_sync(how)
//! ```
//!
//! The record and the Done are a Dekker pair: an input sets its bit, then
//! looks at the Done; the waiter re-arms the Done, then looks at the bits.
//! One sees the other's: the waiter what was done, or the input the Done to
//! complete. An input that sees a Done owed after its bit was had completes
//! it spuriously - the next wait finds nothing new, and waits again.

use super::{park_on, waiter, Done, DynDone, Resumable, WaitSync, Waiter};
use crate::util::sleep::{sleep_until, Sleep};
use core::marker::PhantomPinned;
use core::mem::offset_of;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::{Context, Poll};

pub struct Select<W: Waiter, const N: usize> {
    done:      Done<W>,
    waiter:    W,
    completed: AtomicU32,
    /// Where each input points: its index, at its own address
    ids:       [u8; N],
    _pin:      PhantomPinned,
}

pub struct SelectWait<'a, W: Waiter, const N: usize> {
    s: &'a Select<W, N>,
}

impl<W: Waiter, const N: usize> Select<W, N> {
    pub fn new(mut slot: Pin<&mut Option<Self>>, waiter: W) -> (SelectWait<'_, W, N>, [DynDone<'_>; N]) {
        const { assert!(N <= 32, "a Select's inputs are bits of a u32") };

        slot.set(Some(Select {
            done:      Done::new(waiter),
            waiter,
            completed: AtomicU32::new(0),
            ids:       core::array::from_fn(|i| i as u8),
            _pin:      PhantomPinned,
        }));
        let s = slot.into_ref().get_ref().as_ref().expect("just set");
        // from the whole Select: input_done() goes back to it
        let inputs = core::array::from_fn(|i| unsafe {
            DynDone::from_fn(NonNull::from(s).byte_add(offset_of!(Self, ids) + i).cast(), Self::input_done)
        });
        (SelectWait { s }, inputs)
    }

    unsafe fn input_done(id: NonNull<()>) {
        let i = unsafe { *id.cast::<u8>().as_ptr() } as usize;
        let s = unsafe { &*id.as_ptr().byte_sub(offset_of!(Self, ids) + i).cast::<Self>() };
        s.completed.fetch_or(1 << i, Ordering::SeqCst);
        unsafe { s.done.complete_if_owed() };
    }
}

impl<W: Waiter, const N: usize> SelectWait<'_, W, N> {
    /// What's done since the last wait, if anything - or the Done owed again,
    /// to wait on.
    fn poll_next(&mut self) -> Option<u32> {
        loop {
            let new = self.s.completed.swap(0, Ordering::SeqCst);
            if new != 0 {
                return Some(new);
            }
            if !self.s.done.done() {
                return None;
            }
            self.s.done.rearm(self.s.waiter);
        }
    }
}

/// The inputs done since the last wait, by bit: one at least, sleeping as
/// @how says until there is.
impl<const N: usize> WaitSync for &mut SelectWait<'_, waiter::Sync, N> {
    type Output = u32;

    fn wait_sync(self, how: Sleep) -> u32 {
        loop {
            if let Some(done) = self.poll_next() {
                return done;
            }
            let Ok(()) = sleep_until(how, || self.s.done.done().then_some(()));
        }
    }
}

impl<'a, const N: usize> SelectWait<'a, waiter::Async, N> {
    /// wait_sync(), awaited.
    pub fn next(&mut self) -> Next<'_, 'a, N> {
        Next(self)
    }
}

#[must_use = "a Select's next() is awaited"]
pub struct Next<'w, 'a, const N: usize>(&'w mut SelectWait<'a, waiter::Async, N>);

impl<const N: usize> Resumable for Next<'_, '_, N> {
    type Output = u32;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        let w = &mut *self.get_mut().0;
        match w.poll_next() {
            Some(done) => Poll::Ready(done),
            None       => {
                park_on(cx, &w.s.done);
                Poll::Pending
            }
        }
    }
}
