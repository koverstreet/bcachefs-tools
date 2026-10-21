// SPDX-License-Identifier: GPL-2.0

//! SeqLock: ours in both builds - the kernel crate has none - and only
//! atomics, with no C lock under it.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::mem::MaybeUninit;
use core::sync::atomic::{fence, AtomicU32, Ordering};

/// A sequence lock over a T: readers copy the value out without taking
/// anything, and retry a copy a write overlapped; writers have it alone.
/// For small state read rarely and written often - a progress indicator
/// sysfs shows while its work updates it.
///
/// The sequence is odd while a write is in progress: taking it from even
/// to odd is the writers' lock, so there's no lock besides it, and a
/// second writer spins. A write nested in a write deadlocks.
///
/// The reader's copy races with the writer's stores, as C's seqcount
/// readers' do: it's volatile - the kernel's READ_ONCE() - into a
/// MaybeUninit, and becomes a T only once the sequence says no write
/// overlapped it, so a torn copy (half a bbpos, a bool mid-store) is
/// never a T at all. Rust's memory model would want atomic word-by-word
/// copies for that race; the kernel's, which this follows, doesn't.
#[repr(C)]
pub struct SeqLock<T: Copy> {
    seq:  AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: a reader gets a copy of the T, on any thread - T: Send; the T
// itself is only ever mutated by the one writer.
unsafe impl<T: Copy + Send> Sync for SeqLock<T> {}

// SAFETY: a zero sequence is even - unlocked - and a zeroed T is a T.
unsafe impl<T: Copy + ::pin_init::Zeroable> ::pin_init::Zeroable for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    pub const fn new(v: T) -> Self {
        Self { seq: AtomicU32::new(0), data: UnsafeCell::new(v) }
    }

    /// The value, as of the last write to finish.
    pub fn read(&self) -> T {
        loop {
            let seq = self.seq.load(Ordering::Acquire);
            if seq & 1 != 0 {
                spin_loop();
                continue;
            }

            // SAFETY: valid for reads; any bytes are a MaybeUninit
            let v = unsafe { self.data.get().cast::<MaybeUninit<T>>().read_volatile() };

            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == seq {
                // SAFETY: no write overlapped the copy - it's a T, as the
                // last write left it
                return unsafe { v.assume_init() };
            }
        }
    }

    /// @f on the value, readers held off - shown what @f leaves, once it
    /// returns. A panic in @f leaves the sequence odd: write-held, so
    /// readers spin on it for good.
    pub fn write<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut seq = self.seq.load(Ordering::Relaxed);
        loop {
            if seq & 1 != 0 {
                spin_loop();
                seq = self.seq.load(Ordering::Relaxed);
                continue;
            }
            match self.seq.compare_exchange_weak(seq, seq.wrapping_add(1),
                                                 Ordering::Acquire, Ordering::Relaxed) {
                Ok(_)    => break,
                Err(now) => seq = now,
            }
        }
        // The odd sequence, before any of the value's stores:
        fence(Ordering::Release);

        // SAFETY: the odd sequence is ours - no other writer; readers
        // discard what they copy meanwhile
        let ret = f(unsafe { &mut *self.data.get() });

        self.seq.store(seq.wrapping_add(2), Ordering::Release);
        ret
    }
}

// A torn read is silent - a copy that's half one write and half another -
// so: a writer keeps both halves equal, readers check every copy.
#[cfg(test)]
mod tests {
    use super::SeqLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn no_torn_reads() {
        let l = SeqLock::new([0u64; 4]);
        let done = AtomicBool::new(false);

        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    let mut reads = 0u64;
                    while !done.load(Ordering::Relaxed) || reads < 1000 {
                        let v = l.read();
                        assert!(v.iter().all(|&x| x == v[0]), "torn read: {v:?}");
                        reads += 1;
                    }
                });
            }
            let writers: Vec<_> = (0..2).map(|_| s.spawn(|| {
                for i in 0..200_000u64 {
                    l.write(|v| *v = [i; 4]);
                }
            })).collect();
            writers.into_iter().for_each(|w| w.join().unwrap());
            done.store(true, Ordering::Relaxed);
        });
    }
}
