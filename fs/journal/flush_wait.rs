// SPDX-License-Identifier: GPL-2.0

//! A journal flush's waiters: lockless singly linked lists of FlushEntry, a
//! next link and a completion each - on a buf's wait word, which is its
//! write's flush decision too, or on flush_wait, a plain list, with no live
//! buf to wait on.

use crate::journal::journal::{Journal, JournalBuf};
use crate::util::alloc::{kbox_zeroed_nofail, KBox};
use crate::util::task::{waiter, Armable, Completion, DynDone, Unarmed, Waiter};
use core::cell::UnsafeCell;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering::{Relaxed, SeqCst}};
use pin_init::pin_data;

/*
 * buf->wait is the write's flush decision, and its flush waiters: NULL is
 * undecided; NOFLUSH decided noflush; FLUSH, FLUSH_NO_WAIT or a list of
 * waiters decided flush. It's decided by cmpxchg on this word, so the writer
 * and noflush_seq() don't both win - but for the first write after the
 * filesystem was clean, see do_writes_locked(). JSET_NO_FLUSH records the
 * outcome in the jset, as write_prep() writes it from the word.
 */
pub const JOURNAL_BUF_NOT_IN_FLIGHT: *mut FlushEntry = 1 as _;
pub const JOURNAL_BUF_NOFLUSH: *mut FlushEntry = 2 as _;
/*
 * Stamped on the first entry opened after the journal was clean (the
 * clean->dirty transition): this entry must be a flush write - its write
 * completion marks the fs dirty (the superblock write in journal_write_done())
 * - but flushers must not wait on it, since that mark happens after the early
 * flush-completion signal. Flushers skip it (like NOFLUSH) and attach to a
 * later entry, which by construction completes only after this one's sb write.
 */
pub const JOURNAL_BUF_FLUSH_NO_WAIT: *mut FlushEntry = 3 as _;
/*
 * A flush with no waiters, which flushers may attach to: the writer's pick, or
 * what the early flush-completion signal leaves once it has woken the waiters.
 * A waiter attached to it ends the list at NULL, not at the sentinel.
 */
pub const JOURNAL_BUF_FLUSH: *mut FlushEntry = 4 as _;

/* Real waiters, not one of the sentinels: */
pub fn buf_waiters(first: *mut FlushEntry) -> bool {
    first > JOURNAL_BUF_FLUSH
}

/// A journal flush's waiter: linked on a buf's word or flush_wait, from the
/// flush it asks for till that flush is done, when its completion is
/// completed.
pub struct FlushEntry {
    next: AtomicPtr<FlushEntry>,
    done: UnsafeCell<Option<DynDone<'static>>>,
}

// its Done is set before it's on a list, and taken by the wake of the chain
// that holds it, which nothing else reaches
unsafe impl Send for FlushEntry {}
unsafe impl Sync for FlushEntry {}

impl FlushEntry {
    fn of(done: Option<DynDone<'static>>) -> Self {
        FlushEntry { next: AtomicPtr::new(ptr::null_mut()), done: UnsafeCell::new(done) }
    }

    /// A flush asked for, waited on by no one: an entry of its own, freed once
    /// it's completed.
    pub fn detached() -> &'static FlushEntry {
        unsafe fn free(e: NonNull<()>) {
            drop(unsafe { KBox::from_raw(e.cast::<MaybeUninit<FlushEntry>>().as_ptr()) });
        }
        let e = KBox::into_raw(kbox_zeroed_nofail::<MaybeUninit<FlushEntry>>());
        let done = unsafe { DynDone::from_fn(NonNull::new_unchecked(e).cast(), free) };
        unsafe { (*e).write(FlushEntry::of(Some(done))) }
    }

    /// # Safety
    /// @e is on a chain swapped off a word: ours.
    unsafe fn next(e: NonNull<FlushEntry>) -> *mut FlushEntry {
        unsafe { e.as_ref() }.next.load(Relaxed)
    }

    /// Woken: its completion, which may free it.
    ///
    /// # Safety
    /// As next()'s.
    unsafe fn wake(e: NonNull<FlushEntry>) {
        unsafe { (*e.as_ref().done.get()).take() }.expect("a journal flush waiter completed twice").complete();
    }

    /// Pushed on @list, a word with no sentinels - flush_wait's.
    pub fn push(&self, list: &AtomicPtr<FlushEntry>) {
        let mut first = list.load(Relaxed);
        loop {
            self.next.store(first, Relaxed);
            match list.compare_exchange(first, ptr::from_ref(self).cast_mut(), SeqCst, Relaxed) {
                Ok(_)  => return,
                Err(o) => first = o,
            }
        }
    }
}

/// A wait for a journal flush, made before anything asks for one: owed only
/// if something does - a write's commit, or a flush_seq(), that attaches it.
/// Pinned: once attached, its entry's on a list.
#[pin_data]
pub struct FlushWait<'a, W: Waiter = waiter::Async> {
    /* first: dropped, it waits for the flush, if it was attached, while the
     * entry it's woken through is still there */
    #[pin]
    done:  Option<Armable<'a, W>>,
    entry: FlushEntry,
}

/// A FlushWait's right to be attached, lent to what may ask for its flush -
/// a write's commits: the one that does consumes it. Dropped, nothing did,
/// and the wait's done.
pub struct Flush<'a> {
    entry: &'a FlushEntry,
    done:  Unarmed<'a>,
}

impl<'a, W: Waiter> FlushWait<'a, W> {
    /// One in @slot, for @waiter - what was there dropped, as it waits for
    /// its flush - and its Flush.
    pub fn new(mut slot: Pin<&mut Option<Self>>, waiter: W) -> Flush<'_> {
        slot.set(Some(FlushWait { done: None, entry: FlushEntry::of(None) }));
        let w = slot.as_pin_mut().expect("just set").project();
        Flush { entry: w.entry, done: Armable::new(w.done, waiter) }
    }

    /// new(), its flush completing @first - a Select's input - before its
    /// own wait, which its drop finishes.
    pub fn chained<'s>(mut slot: Pin<&'s mut Option<Self>>, waiter: W, first: DynDone<'a>) -> Flush<'s> {
        slot.set(Some(FlushWait { done: None, entry: FlushEntry::of(None) }));
        let w = slot.as_pin_mut().expect("just set").project();
        Flush { entry: w.entry, done: Armable::chained(w.done, waiter, first) }
    }

    /// The wait on its flush: done at once, if no commit attached it.
    pub fn completion(self: Pin<&mut Self>) -> Completion<'_, W> {
        self.project().done.as_pin_mut().expect("a FlushWait made by new()").completion()
    }

    pub fn armed(self: Pin<&Self>) -> bool {
        self.get_ref().done.as_ref().is_some_and(Armable::armed)
    }
}

impl Flush<'_> {
    /// Attached to @seq's flush, in @j: its wait armed, and its entry on a
    /// list - the word returned.
    pub fn attach(self, j: &Journal, seq: u64) -> &AtomicPtr<FlushEntry> {
        // on no list yet: the entry's ours
        unsafe { *self.entry.done.get() = Some(self.done.arm()) };
        j.__flush_seq_async(seq, self.entry)
    }
}

/// Flush waiters swapped off a word: a chain, ended by NULL, that nothing
/// else reaches until it's woken - when it drops - or added to another buf's.
pub struct Waiters {
    first: NonNull<FlushEntry>,
    /* found as it's taken: once its tail's linked ahead of another chain, a
     * walk from @first runs on into that one */
    last:  NonNull<FlushEntry>,
}

impl Waiters {
    /// @list's waiters, if it has any, the word left @with.
    pub fn take(list: &AtomicPtr<FlushEntry>, with: *mut FlushEntry) -> Option<Self> {
        let first = NonNull::new(list.swap(with, SeqCst)).filter(|f| buf_waiters(f.as_ptr()))?;
        let mut last = first;
        while let Some(e) = NonNull::new(unsafe { FlushEntry::next(last) }) {
            last = e;
        }
        Some(Waiters { first, last })
    }

    /// Its last entry's next: what it's put ahead of.
    fn set_tail(&mut self, next: *mut FlushEntry) {
        unsafe { self.last.as_ref() }.next.store(next, Relaxed);
    }

    fn into_raw(self) -> *mut FlushEntry {
        ManuallyDrop::new(self).first.as_ptr()
    }

    /// Its drop's the wake.
    pub fn wake(self) {}
}

impl Drop for Waiters {
    fn drop(&mut self) {
        let mut e = Some(self.first);
        while let Some(this) = e {
            // read before it's completed, which may free it
            e = NonNull::new(unsafe { FlushEntry::next(this) });
            unsafe { FlushEntry::wake(this) };
        }
    }
}

/// Where journal_buf::wait() left its entry.
pub enum BufWait {
    /// Not in flight: there's nothing from here on to wait for.
    NotInFlight,
    /// Decided noflush, or the clean->dirty FLUSH_NO_WAIT entry: not a flush
    /// to wait on.
    NotWaitable,
    /// Waiting on its flush - the first, or behind others.
    Waiting { first: bool },
}

impl JournalBuf {
    pub(crate) fn wait_word(&self) -> &AtomicPtr<FlushEntry> {
        &self.wait
    }

    pub fn must_flush(&self) -> bool {
        /*
         * "must be written as a flush" - true for entries with real flush
         * waiters AND for the FLUSH_NO_WAIT transition entry (which has no
         * waiters but must still flush, to mark the fs dirty). The close/cycle
         * logic keys on this, so FLUSH_NO_WAIT must count here or its entry
         * never gets closed and written.
         */
        self.wait_word().load(Relaxed) > JOURNAL_BUF_NOFLUSH
    }

    pub fn try_noflush(&self) -> bool {
        let w = self.wait_word();
        let mut old = w.load(Relaxed);
        loop {
            if old == JOURNAL_BUF_NOFLUSH {
                return true;
            }
            if !old.is_null() {
                return false;
            }
            match w.compare_exchange(old, JOURNAL_BUF_NOFLUSH, SeqCst, Relaxed) {
                Ok(_) => return true,
                Err(o) => old = o,
            }
        }
    }

    pub fn take_waiters(&self, with: *mut FlushEntry) -> Option<Waiters> {
        Waiters::take(self.wait_word(), with)
    }

    /// @waiters on its word, ahead of any it has - back, if it isn't taking
    /// flush waiters: decided noflush, not in flight, or FLUSH_NO_WAIT's.
    pub fn add_waiters(&self, mut waiters: Waiters) -> Result<(), Waiters> {
        let w = self.wait_word();
        let mut old = w.load(Relaxed);
        loop {
            if !old.is_null() && old <= JOURNAL_BUF_FLUSH_NO_WAIT {
                waiters.set_tail(ptr::null_mut());
                return Err(waiters);
            }

            waiters.set_tail(if buf_waiters(old) { old } else { ptr::null_mut() });

            match w.compare_exchange(old, waiters.first.as_ptr(), SeqCst, Relaxed) {
                Ok(_) => {
                    waiters.into_raw();
                    return Ok(());
                }
                Err(o) => old = o,
            }
        }
    }

    /*
     * Attach a flush waiter, respecting the journal_buf sentinel states: if
     * the journal_buf is not waitable, no waiters can attach. Otherwise the
     * entry is attached to buf->wait; the act of attaching transitions buf
     * out of the "may be noflush" state into "must flush" (via the wait-list
     * head becoming a real pointer).
     */
    /// Refused, @e is on no list.
    pub fn wait(&self, e: &FlushEntry) -> BufWait {
        let w = self.wait_word();
        let refused = |first: *mut FlushEntry| (!first.is_null() && first <= JOURNAL_BUF_FLUSH_NO_WAIT)
            .then(|| if first == JOURNAL_BUF_NOT_IN_FLIGHT { BufWait::NotInFlight } else { BufWait::NotWaitable });

        /*
         * cmpxchg-loop attach, bailing on race-to-poison. Standard llist_add
         * would happily chain through the NOFLUSH sentinel, which would
         * corrupt the list — so we open-code the attach to check first each
         * iteration.
         */
        let mut first = w.load(Relaxed);
        loop {
            if let Some(r) = refused(first) {
                return r;
            }
            e.next.store(if buf_waiters(first) { first } else { ptr::null_mut() }, Relaxed);

            match w.compare_exchange(first, ptr::from_ref(e).cast_mut(), SeqCst, Relaxed) {
                Ok(_) => return BufWait::Waiting { first: !buf_waiters(first) },
                Err(o) => first = o,
            }
        }
    }

    /// @waiters back, as its only ones: its word as it was before
    /// take_waiters() left @with.
    pub fn put_back_waiters(&self, waiters: Waiters, with: *mut FlushEntry) {
        let was = self.wait_word().swap(waiters.into_raw(), SeqCst);
        assert!(was == with, "journal buf's flush word changed while its waiters were away");
    }
}
