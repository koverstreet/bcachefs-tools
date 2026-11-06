// SPDX-License-Identifier: GPL-2.0

//! What keeps each dirty journal entry dirty: pins, each in its holder - a
//! btree node with updates in the entry, a key cache entry, the write buffer -
//! on its entry's list for its type until flushed; and the fifo of the live
//! entries, [front, back) by seq.
//!
//! The fifo's buffer is pin_resize_lock's: read for any entry, write to
//! replace it. front and back, and an entry's bytes, are j->lock's; an
//! entry's lists are its own lock's, two taken the older first; its devs are
//! the write path's.

use core::ffi::{c_int, c_uint, c_void};
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{fence, AtomicI32, Ordering::{Acquire, Relaxed, SeqCst}};

use cstruct_macros::c_export;
use pin_init::{pin_data, PinInit, Zeroable};

use crate::c;
use crate::c::journal_pin_type as PinType;
use crate::alloc::replicas::{bch2_devlist_to_replicas, bch2_replicas_entry_to_text};
use crate::data_structures::list::List;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::format::DataType;
use crate::fs::Fs;
use crate::locking::{ExclusiveGuard, Locked, LockedBy, MemallocFlags, SharedGuard};
use crate::sb::counters::events;
use crate::types::{rust_only, RustOnly};
use crate::util::alloc::{init_at, AllocError, KVVec};
use crate::util::closure::{closure_has_waiters, closure_wake_up};
use crate::util::ffi::Opaque;
use crate::util::projections::{TearSafe, Volatile};
use crate::util::Printbuf;
use crate::{bch_err, new_lock, prt, warn_once};

const JOURNAL_PIN: u64 = 32 * 1024;

/*
 * Ceiling on dirty journal entries. The pin fifo grows on demand up to this,
 * and never past it: what it bounds is how far the journal can run ahead of
 * reclaim, and so how many entries the next mount has to replay - small
 * fsync-heavy entries can otherwise pile up by the million, bounded only by
 * journal space.
 *
 * A filesystem from before the ceiling can come up with more than this dirty;
 * the fifo is allocated to fit, reclaim works it back down, and the fifo
 * shrinks to the ceiling once it can (resize()).
 */
const JOURNAL_PIN_MAX: u64 = 1024 * 1024;

pub type Pin = Opaque<c::journal_entry_pin>;

type PinListHead = List<Pin, { offset_of!(c::journal_entry_pin, list) }>;

impl Pin {
    /// The entry it holds, 0 for none.
    pub fn seq(&self) -> u64 {
        unsafe { (&raw const (*self.as_ptr()).seq).read_volatile() }
    }

    fn flush_fn(&self) -> c::journal_pin_flush_fn {
        unsafe { (*self.as_ptr()).flush }
    }

    fn pin_type(&self) -> PinType {
        unsafe { (*self.as_ptr()).pin_type }
    }

    fn set_seq(&self, seq: u64) {
        unsafe { (&raw mut (*self.as_ptr()).seq).write_volatile(seq) }
    }

    /* With the lists it's going on held */
    fn arm(&self, seq: u64, ty: PinType, flush: c::journal_pin_flush_fn) {
        self.set_seq(seq);
        unsafe {
            (*self.as_ptr()).flush    = flush;
            (*self.as_ptr()).pin_type = ty;
        }
    }
}

/* A flush function, as %ps prints it */
struct FlushFn(c::journal_pin_flush_fn);

impl core::fmt::Display for FlushFn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut out = Printbuf::new();
        let p = self.0.map_or(ptr::null(), |f| f as *const c_void);
        unsafe { c::bch2_prt_printf(out.as_raw(), c"%ps".as_ptr().cast(), p) };
        f.write_str(out.as_str())
    }
}

/* The devices an entry was written to: a word, so never seen torn */
#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
pub struct PinDevs {
    nr:   u8,
    devs: [u8; c::BCH_REPLICAS_MAX as usize],
}

unsafe impl TearSafe for PinDevs {}

impl PinDevs {
    /* @d is bounded by metadata_replicas <= BCH_REPLICAS_MAX (journal write) */
    fn new(d: &c::bch_devs_list) -> Self {
        let mut r = Self::default();
        for (dst, &dev) in r.devs.iter_mut().zip(&d.data[..d.nr as usize]) {
            *dst = dev;
            r.nr += 1;
        }
        r
    }

    fn list(self) -> c::bch_devs_list {
        let mut l = c::bch_devs_list::default();
        l.data[..self.nr as usize].copy_from_slice(&self.devs[..self.nr as usize]);
        l.nr = self.nr;
        l
    }

    fn has(self, dev: u8) -> bool {
        self.devs[..self.nr as usize].contains(&dev)
    }
}

struct PinLists {
    unflushed: [PinListHead; c::JOURNAL_PIN_TYPE_NR as usize],
    flushed:   PinListHead,
}

unsafe impl Send for PinLists {}

impl PinLists {
    /* Empty, in place: a list's head doesn't move */
    fn new() -> impl PinInit<Self> {
        unsafe {
            pin_init::pin_init_from_closure(|slot: *mut Self| {
                slot.write_bytes(0, 1);
                (*slot).init();
                Ok(())
            })
        }
    }

    /// # Safety
    /// They're empty, or what's on them is forgotten.
    unsafe fn init(&self) {
        for l in self.all() {
            unsafe { l.init() }
        }
    }

    fn all(&self) -> impl Iterator<Item = &PinListHead> {
        self.unflushed.iter().chain(core::iter::once(&self.flushed))
    }

    fn all_mut(&mut self) -> impl Iterator<Item = &mut PinListHead> {
        self.unflushed.iter_mut().chain(core::iter::once(&mut self.flushed))
    }
}

type ListsGuard<'a> = ExclusiveGuard<'a, c::spinlock_t, PinLists>;

/* One entry's: its pins, and what it takes to retire it */
#[pin_data]
pub struct PinList {
    #[pin]
    lists:      Locked<c::spinlock_t, PinLists>,
    /* the entry's own ref while it's open, one while it's unreplayed, one a pin */
    count:      AtomicI32,
    unreplayed: Volatile<bool>,
    devs:       Volatile<PinDevs>,
    bytes:      Volatile<u32>,
}

impl PinList {
    fn new() -> impl PinInit<Self> {
        pin_init::pin_init!(PinList {
            lists <- new_lock!(<- PinLists::new(), (), "&pin_list->lock"),
            count: AtomicI32::new(0),
            unreplayed: Zeroable::zeroed(),
            devs: Zeroable::zeroed(),
            bytes: Zeroable::zeroed(),
        })
    }

    /* Unlocked, as C's: it's under j->lock, and an entry being opened is no one else's */
    fn reset(&self, count: i32) {
        unsafe { (*self.lists.data_ptr()).init() };
        self.count.store(count, Relaxed);
        self.unreplayed.set(false);
        self.devs.set(PinDevs::default());
        self.bytes.set(0);
    }
}

/* Two entries' lists, held */
struct ListsPair<'b> {
    /*
     * Dropped in declaration order, the one taken second first: each guard
     * restores the irq flags it saved, so out of order leaves irqs off
     */
    second:    Option<ListsGuard<'b>>,
    first:     ListsGuard<'b>,
    new_first: bool,
}

impl ListsPair<'_> {
    fn new_lists(&mut self) -> &mut PinLists {
        match &mut self.second {
            Some(g) if !self.new_first => g.protected(),
            _ => self.first.protected(),
        }
    }
}

/* @new's lists, and @old's if they're another's: the older taken first */
fn lock_pair<'b>(new: &'b PinList, new_seq: u64, old: Option<&'b PinList>, old_seq: u64)
    -> ListsPair<'b>
{
    match old {
        Some(old) if !ptr::eq(old, new) => {
            let new_first = new_seq < old_seq;
            let (a, b) = if new_first { (new, old) } else { (old, new) };
            let first = a.lists.lock();
            ListsPair { second: Some(b.lists.lock_nested(1)), first, new_first }
        }
        _ => ListsPair { second: None, first: new.lists.lock(), new_first: true },
    }
}

/*
 * All of them, not just the live window: slot() masks without a range check,
 * so a stray pin for an out of window seq still lands on a valid, empty entry
 */
fn new_entries(nr: usize) -> Result<KVVec<PinList>, AllocError> {
    let mut v = KVVec::with_capacity(nr)?;
    for slot in &mut v.spare_capacity_mut()[..nr] {
        let Ok(()) = unsafe { init_at(PinList::new(), slot.as_mut_ptr()) };
    }
    unsafe { v.inc_len(nr) };
    Ok(v)
}

/* pin_resize_lock's */
pub struct PinBuf {
    /* a power of two of them */
    entries: KVVec<PinList>,
}

/* j->lock's, written under pin_resize_lock too */
pub struct PinRange {
    front: Volatile<u64>,
    back:  Volatile<u64>,
    /* how many may be live */
    size:  Volatile<u64>,
}

unsafe impl Zeroable for PinRange {}

pub type Pins = rust_only!(LockedBy<PinBuf, PinRange>);

pub fn new() -> impl PinInit<Pins, core::convert::Infallible> {
    RustOnly::new(LockedBy::new(PinBuf { entries: KVVec::new() }, Zeroable::zeroed()))
}

/* The live entries, with pin_resize_lock held for read */
pub struct PinsRead<'a> {
    j:   &'a c::journal,
    buf: &'a PinBuf,
    _g:  SharedGuard<'a, c::percpu_rw_semaphore>,
}

impl c::journal {
    pub fn fs(&self) -> &Fs {
        unsafe { &*ptr::from_ref(self).byte_sub(offset_of!(Fs, journal)).cast() }
    }

    pub fn pins(&self) -> PinsRead<'_> {
        let g = self.pin_resize_lock.read();
        PinsRead { j: self, buf: unsafe { self.pins.protected() }, _g: g }
    }

    /// # Safety
    /// The caller holds pin_resize_lock for read, for as long as these live.
    unsafe fn pins_held(&self) -> PinsRead<'_> {
        let g = unsafe { self.pin_resize_lock.read_held() };
        PinsRead { j: self, buf: unsafe { self.pins.protected() }, _g: g }
    }

    fn pin_range(&self) -> &PinRange {
        &self.pins
    }

    pub fn pin_set(&self, seq: u64, pin: &Pin, ty: PinType, flush: c::journal_pin_flush_fn) {
        self.pins().set(seq, pin, ty, flush)
    }

    pub fn pin_add(&self, seq: u64, pin: &Pin, ty: PinType, flush: c::journal_pin_flush_fn) {
        let held = pin.seq();
        if held == 0 || held > seq {
            self.pin_set(seq, pin, ty, flush)
        }
    }

    pub fn pin_update(&self, seq: u64, pin: &Pin, ty: PinType, flush: c::journal_pin_flush_fn) {
        let held = pin.seq();
        if held == 0 || held < seq {
            self.pin_set(seq, pin, ty, flush)
        }
    }

    pub fn pin_copy(&self, dst: &Pin, src: &Pin, ty: PinType, flush: c::journal_pin_flush_fn) {
        self.pins().copy(dst, src, ty, flush)
    }

    pub fn pin_drop(&self, pin: &Pin) {
        self.pins().drop_pin(pin)
    }
}

impl<'a> PinsRead<'a> {
    fn fs(&self) -> &'a Fs {
        self.j.fs()
    }

    pub fn front(&self) -> u64 {
        self.j.pin_range().front.get()
    }

    pub fn back(&self) -> u64 {
        self.j.pin_range().back.get()
    }

    fn last_seq(&self) -> u64 {
        self.j.lock.last_seq.get()
    }

    fn slot(&self, seq: u64) -> &'a PinList {
        let e = &self.buf.entries;
        &e[seq as usize & (e.len() - 1)]
    }

    /* A live entry's */
    fn get(&self, seq: u64) -> &'a PinList {
        debug_assert!(seq >= self.front() && seq < self.back(),
                      "journal seq {seq} outside live range [{}, {})", self.front(), self.back());
        self.slot(seq)
    }

    /*
     * The entry for @pin's @seq. A pin's seq is live by construction: say so
     * where it isn't - pinning, unpinning or flushing at a stale seq corrupts
     * whatever live entry shares its slot, silently, until some later pin
     * trips over the result.
     */
    fn pin_slot(&self, pin: &Pin, seq: u64) -> &'a PinList {
        warn_once!(seq < self.front() || seq >= self.back(), self.fs(),
                   "journal pin {} at seq {} outside live range [{}, {})",
                   FlushFn(pin.flush_fn()), seq, self.front(), self.back());
        self.slot(seq)
    }

    /* The fifo's ends, and an entry's counts: with j->lock where they change */

    pub fn push(&self, seq: u64) {
        let back = self.back();
        assert_eq!(back, seq, "journal entry opened out of order");
        self.slot(back).reset(1);
        self.j.pin_range().back.set(back + 1);
    }

    pub fn advance_front(&self, last_seq_ondisk: u64) {
        let new_front = self.back().min(last_seq_ondisk);
        for seq in self.front()..new_front {
            self.check_retired(self.slot(seq), seq);
        }
        self.j.pin_range().front.set(new_front);
    }

    /* The entry's own ref, from when it was opened: whether that was its last */
    pub fn seq_put(&self, seq: u64) -> bool {
        self.get(seq).count.fetch_sub(1, SeqCst) == 1
    }

    pub fn seq_unpinned(&self, seq: u64) -> bool {
        let l = self.get(seq);
        let unpinned = l.count.load(SeqCst) == 0;
        if unpinned {
            self.check_retired(l, seq);
        }
        unpinned
    }

    pub fn seq_replayed(&self, seq: u64) -> bool {
        let l = self.get(seq);
        assert!(l.unreplayed.get(), "journal seq {seq} replayed twice");
        l.unreplayed.set(false);
        l.count.fetch_sub(1, SeqCst) == 1
    }

    pub fn seq_set_bytes(&self, seq: u64, bytes: u32) {
        self.get(seq).bytes.set(bytes)
    }

    pub fn seq_take_bytes(&self, seq: u64) -> u32 {
        let l = self.get(seq);
        let bytes = l.bytes.get();
        l.bytes.set(0);
        bytes
    }

    pub fn seq_devs(&self, seq: u64) -> c::bch_devs_list {
        self.get(seq).devs.get().list()
    }

    pub fn seq_set_devs(&self, seq: u64, devs: &c::bch_devs_list) {
        self.get(seq).devs.set(PinDevs::new(devs))
    }

    fn wake_flushers(&self) {
        if closure_has_waiters(&self.j.reclaim_flush_wait) {
            closure_wake_up(&self.j.reclaim_flush_wait);
        }
    }

    fn maybe_update_last_seq(&self) {
        if self.slot(self.last_seq()).count.load(Relaxed) == 0 {
            let _j = self.j.lock.lock();
            unsafe { c::bch2_journal_update_last_seq(self.j.as_ptr()) }
        }
    }

    /* @pin off its list, on @l: whether that left the oldest entry unpinned */
    fn drop_locked(&self, l: &PinList, pin: &Pin) -> bool {
        if pin.seq() == 0 {
            return false;
        }

        /* active <=> on a list: */
        if !unsafe { PinListHead::unlink(pin) } {
            let _f = MemallocFlags::noreclaim();
            warn_once!(true, self.fs(), "journal pin {} active at seq {} but on no list",
                       FlushFn(pin.flush_fn()), pin.seq());
        }

        if self.j.flush_in_progress.load(Relaxed) == pin.as_ptr() {
            self.j.flush_in_progress_dropped.set(true);
        }
        /*
         * Don't clear pin->seq here; caller may be updating the pin, and we
         * don't want other threads to see pin->seq in an intermediate state
         */

        self.wake_flushers();

        /*
         * Unpinning a journal entry may make journal_next_bucket() succeed, if
         * writing a new last_seq will now make another bucket available:
         */
        l.count.fetch_sub(1, SeqCst) == 1 && ptr::eq(l, self.slot(self.last_seq()))
    }

    fn set_locked(&self, old: Option<&PinList>, new: &PinList, new_lists: &mut PinLists,
                  pin: &Pin, seq: u64, ty: PinType, flush: c::journal_pin_flush_fn) -> bool {
        let reclaim = old.is_some_and(|old| self.drop_locked(old, pin));

        /* inactive <=> on no list - or the push below corrupts the old one */
        if PinListHead::linked(pin) {
            let _f = MemallocFlags::noreclaim();
            warn_once!(true, self.fs(), "journal pin {} inactive but still on a list", FlushFn(pin.flush_fn()));
        }

        /*
         * flush_fn is how we identify journal pins in debugfs, so must always
         * exist, even if it doesn't do anything:
         */
        assert!(flush.is_some(), "journal pin set with no flush fn");

        new.count.fetch_add(1, Relaxed);
        pin.arm(seq, ty, flush);

        let list = &mut new_lists.unflushed[ty as usize];
        if list.is_empty() {
            self.wake_flushers();
        }
        unsafe { list.push_front(pin) };
        reclaim
    }

    pub fn set(&self, seq: u64, pin: &Pin, ty: PinType, flush: c::journal_pin_flush_fn) {
        /*
         * A pin outside the live window lands on an out-of-[front,back) slot:
         * that pin is never reclaimed (reclaim only walks the window) and is
         * left dangling in the old buffer when the fifo is resized. Catch the
         * offending caller here rather than debugging the eventual
         * use-after-free.
         */
        warn_once!(seq < self.front() || seq >= self.back(), self.fs(),
                   "journal pin set for seq {} outside live range [{}, {})", seq, self.front(), self.back());
        /*
         * Stronger than the above: front trails last_seq (it's last_seq_ondisk),
         * so a seq already retired - count reached zero - still passes it, and
         * the front then moves past a live pin. A legitimate caller holds
         * something that keeps its seq from retiring (the pin it's copying, a
         * key cache entry's own pin, a journal reservation): pinning below
         * last_seq means it didn't.
         */
        warn_once!(seq < self.last_seq(), self.fs(),
                   "journal pin {} set for seq {}, already retired (last_seq {})",
                   FlushFn(flush), seq, self.last_seq());

        loop {
            let old_seq = pin.seq();
            let new = self.slot(seq);
            let old = (old_seq != 0).then(|| self.pin_slot(pin, old_seq));

            let mut locked = lock_pair(new, seq, old, old_seq);
            if old_seq != pin.seq() {
                continue;
            }
            let reclaim = self.set_locked(old, new, locked.new_lists(), pin, seq, ty, flush);
            drop(locked);

            /* If the journal is currently full, we might want to call flush_fn immediately: */
            if seq == self.last_seq() {
                closure_wake_up(&self.j.async_wait);
            }
            if reclaim {
                self.maybe_update_last_seq();
            }
            return;
        }
    }

    pub fn copy(&self, dst: &Pin, src: &Pin, ty: PinType, flush: c::journal_pin_flush_fn) {
        loop {
            let (src_seq, dst_seq) = (src.seq(), dst.seq());
            if src_seq == 0 {
                return;
            }
            let src_l = self.pin_slot(src, src_seq);
            let dst_l = (dst_seq != 0).then(|| self.pin_slot(dst, dst_seq));

            let mut locked = lock_pair(src_l, src_seq, dst_l, dst_seq);

            /*
             * Keep-oldest, like pin_add(): a dst already pinning an older seq
             * must not be moved forward, or we release an obligation - the
             * callers (interior update pin transfers: reparent,
             * will_free_node) copy several pins into one and need the min. A
             * pin at an older seq covers src_seq.
             */
            let keep = dst_seq != 0 && dst_seq <= src_seq;
            let race = src_seq != src.seq() || dst_seq != dst.seq();
            let reclaim = !race && !keep &&
                self.set_locked(dst_l, src_l, locked.new_lists(), dst, src_seq, ty, flush);
            drop(locked);

            if !race {
                /* If the journal is currently full, we might want to call flush_fn immediately: */
                if !keep && src_seq == self.last_seq() {
                    closure_wake_up(&self.j.async_wait);
                }
                if reclaim {
                    self.maybe_update_last_seq();
                }
                return;
            }
        }
    }

    pub fn drop_pin(&self, pin: &Pin) {
        let reclaim = loop {
            let seq = pin.seq();
            if seq == 0 {
                break false;
            }
            let l = self.pin_slot(pin, seq);
            let _g = l.lists.lock();
            if pin.seq() != seq {
                continue;
            }
            let reclaim = self.drop_locked(l, pin);
            pin.set_seq(0);
            break reclaim;
        };

        if reclaim {
            self.maybe_update_last_seq();
        }
    }

    /*
     * An entry the journal is done with - retired past last_seq, or dropped
     * off the front of the pin fifo - must hold no pins. count is the pins on
     * its lists plus refs that are gone by then, so a pin still listed means
     * count was decremented for a pin that never left, or a pin was added to
     * an entry already retired. Either way the pin now sits on a seq nothing
     * will flush, and slot() will hand its slot to a new seq.
     */
    fn check_retired(&self, l: &PinList, seq: u64) {
        /*
         * Lockless, after the caller saw count == 0: a dropped pin leaves its
         * list before count is decremented (drop_locked(), whose fetch_sub()
         * orders the two), so pair that here or a weakly ordered CPU can see
         * the count and a stale list together:
         */
        fence(Acquire);

        let lists = unsafe { &*l.lists.data_ptr() };
        let first = lists.all().find_map(|l| l.first()).map(|p| p.flush_fn());
        let count = l.count.load(Relaxed);

        if first.is_some() || count != 0 {
            let _f = MemallocFlags::noreclaim();
            warn_once!(true, self.fs(), "journal seq {} retired with count {}{}{}", seq, count,
                       if first.is_some() { ", pins still listed, first " } else { "" },
                       FlushFn(first.flatten()));
        }
    }

    /* Reclaim's */

    /*
     * The next pin to flush - an unflushed one of the oldest entry with one of
     * a type allowed: up to @seq_to_flush, of those in @below; any, of those in
     * @above - now the flush in progress, with its seq and flush function
     */
    fn next_to_flush(&self, seq_to_flush: u64, below: u32, above: u32)
        -> Option<(*mut c::journal_entry_pin, u64, c::journal_pin_flush_fn)>
    {
        for seq in self.front()..self.back() {
            let l = self.slot(seq);
            let mut g = l.lists.lock();

            /*
             * Flushing journal pins (writing btree nodes) requires
             * consuming journal space: don't get ahead of journal replay to
             * avoid deadlocking
             */
            if l.unreplayed.get() || (seq > seq_to_flush && above == 0) {
                break;
            }

            for (i, list) in g.protected().unflushed.iter().enumerate() {
                if ((1 << i) & below != 0 && seq <= seq_to_flush) || (1 << i) & above != 0 {
                    if let Some(pin) = list.first() {
                        assert!(self.j.flush_in_progress.load(Relaxed).is_null(),
                                "journal pin flush started with one in progress");
                        self.j.flush_in_progress.store(pin.as_ptr(), Relaxed);
                        self.j.flush_in_progress_dropped.set(false);
                        return Some((pin.as_ptr(), seq, pin.flush_fn()));
                    }
                }
            }
        }
        None
    }

    /*
     * @pin, flushed for @seq, onto its entry's flushed list - unless the flush
     * failed, or the pin was dropped or rearmed meanwhile: no longer the flush
     * in progress
     */
    fn flushed(&self, pin: &Pin, seq: u64, ok: bool) {
        /*
         * Unchecked: if flush_fn dropped the pin, @seq may have retired since.
         * If it didn't, the pin still holds @seq, and the move below checks it:
         */
        let mut g = self.slot(seq).lists.lock();

        if ok && !self.j.flush_in_progress_dropped.get() {
            self.pin_slot(pin, seq);
            unsafe {
                PinListHead::unlink(pin);
                g.protected().flushed.push_front(pin);
            }
        }
        self.j.flush_in_progress.store(ptr::null_mut(), Relaxed);
        self.j.flush_in_progress_dropped.set(false);
    }

    fn still_flushing(&self, seq_to_flush: u64, types: u32) -> bool {
        let end = if seq_to_flush != u64::MAX { self.back().min(seq_to_flush + 1) } else { self.back() };

        (self.front()..end).any(|seq| {
            let mut g = self.slot(seq).lists.lock();
            let lists = g.protected();

            lists.unflushed.iter().enumerate().any(|(i, l)| types & (1 << i) != 0 && !l.is_empty()) ||
                lists.flushed.iter().any(|p| types & (1 << p.pin_type() as u32) != 0)
        })
    }

    /* The last entry written to @dev - or, @dev < 0, written to fewer than @replicas */
    fn last_dev_seq(&self, dev: c_int, replicas: c_uint) -> u64 {
        (self.front()..self.back()).rev()
            .find(|&seq| {
                let devs = self.slot(seq).devs.get();
                if dev >= 0 { devs.has(dev as u8) } else { u32::from(devs.nr) < replicas }
            })
            .unwrap_or(0)
    }

    fn to_text(&self, out: &mut Printbuf, seq: u64, mut nr: Option<&mut c_uint>) {
        let l = self.slot(seq);
        let mut g = l.lists.lock();
        let _f = MemallocFlags::noreclaim();

        prt!(out, "{}: count {}", seq, l.count.load(Relaxed));
        if l.unreplayed.get() {
            prt!(out, " unreplayed");
        }
        out.newline();
        let out = &mut *out.indent(2);

        let mut r = c::bch_replicas_padded::default();
        unsafe {
            bch2_devlist_to_replicas(&raw mut r.e, DataType::journal, l.devs.get().list());
            bch2_replicas_entry_to_text(out, &raw const r.e);
        }
        out.newline();

        let lists = g.protected();
        let mut prt_pins = |out: &mut Printbuf, name: &str, lists: &mut dyn Iterator<Item = &PinListHead>| {
            prt!(out, "{}:\n", name);
            for pin in lists.flat_map(|l| l.iter()) {
                let f = pin.flush_fn().map_or(ptr::null(), |f| f as *const c_void);
                unsafe { c::bch2_prt_printf(out.as_raw(), c"\t%px %ps\n".as_ptr().cast(), pin.as_ptr(), f) };
                if let Some(nr) = nr.as_deref_mut() {
                    *nr += 1;
                }
            }
        };
        prt_pins(out, "unflushed", &mut lists.unflushed.iter());
        prt_pins(out, "flushed", &mut core::iter::once(&lists.flushed));
    }
}

/* The fifo's own: its buffer, sized to the dirty entries */

/*
 * The size the fifo should be: grown 4x while it's filling, up to
 * JOURNAL_PIN_MAX; and over JOURNAL_PIN_MAX - a mount that found more dirty
 * entries than that - back down to it, once the live entries fit.
 *
 * Returns the current size when there's nothing to do.
 */
fn want_size(r: &PinRange) -> u64 {
    let (size, used) = (r.size.get(), r.back.get() - r.front.get());
    let max = JOURNAL_PIN_MAX;

    if size > max {
        if used < max / 2 { max } else { size }
    } else if size < max && size - used < size / 2 {
        (size * 4).min(max)
    } else {
        size
    }
}

/*
 * The fifo resized, to want_size(): a fresh buffer, every entry initialized,
 * then each live entry's lists moved to their new place - the pins on them
 * point at their old heads.
 *
 * Must NOT be called with j->lock held - alloc + percpu_down_write may sleep.
 * Called only from pin_resize_work, so resizes don't race each other.
 */
fn resize(j: &c::journal) -> Result<(), BchError> {
    let fs = j.fs();
    let r = j.pin_range();

    /*
     * Resizing the journal pin fifo is particularly expensive, thanks to
     * the percpu rwsem - so it grows 4x at a time
     */
    let new_size = want_size(r);
    if new_size == r.size.get() {
        return Ok(());
    }

    let entries = new_entries(new_size.next_power_of_two() as usize);
    fs.event_inc_trace(events::journal_pin_resize, |out| prt!(out, "new size: {}", new_size));
    let entries = entries.map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_pin_fifo))?;

    let g = j.pin_resize_lock.write();
    let buf = unsafe { j.pins.protected_mut() };

    /*
     * Shrinking: entries opened since we decided may no longer fit - and
     * the fifo must never have more live than its size
     */
    if r.back.get() - r.front.get() > new_size {
        return Ok(());
    }

    let mask = entries.len() - 1;
    let old_mask = buf.entries.len() - 1;
    for seq in r.front.get()..r.back.get() {
        let (old, new) = (&buf.entries[seq as usize & old_mask], &entries[seq as usize & mask]);

        new.count.store(old.count.load(Relaxed), Relaxed);
        new.unreplayed.set(old.unreplayed.get());
        new.devs.set(old.devs.get());
        new.bytes.set(old.bytes.get());

        let (mut og, mut ng) = (old.lists.lock(), new.lists.lock_nested(1));
        for (o, n) in og.protected().all_mut().zip(ng.protected().all_mut()) {
            unsafe { o.move_to(n) };
        }
    }

    let old = core::mem::replace(&mut buf.entries, entries);
    r.size.set(new_size);
    drop(g);
    drop(old);
    Ok(())
}

/*
 * The fifo for [last_seq, cur_seq), each entry unreplayed with a ref replay
 * puts. However many dirty entries there are, we have to replay them: no cap
 * here. Filesystems from before JOURNAL_PIN_MAX can have millions; reclaim
 * works them back down and the fifo shrinks to the ceiling.
 */
fn start(j: &c::journal, last_seq: u64, cur_seq: u64) -> Result<(), BchError> {
    let fs = j.fs();
    let mut nr = cur_seq - last_seq;

    /*
     * Extra fudge factor, in case we crashed when the journal pin fifo was
     * nearly or completely full. We'll need to be able to open additional
     * journal entries (at least a few) in order for journal replay to get
     * going. Capped: rounded up to a power of two, 25% of millions of
     * entries can double an allocation that's already gigabytes.
     */
    nr += (nr / 4).min(JOURNAL_PIN);
    let size = nr.max(JOURNAL_PIN).next_power_of_two();

    let entries = {
        let _f = MemallocFlags::nowarn();
        new_entries(size as usize)
    };
    let entries = entries.map_err(|_| {
        bch_err!(fs, "error allocating journal pin fifo: {} dirty journal entries ({}-{}), {} bytes",
                 cur_seq - last_seq, last_seq, cur_seq, (size as usize).saturating_mul(size_of::<PinList>()));
        fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_pin_fifo)
    })?;

    for seq in last_seq..cur_seq {
        let l = &entries[(seq & (size - 1)) as usize];
        l.count.store(1, Relaxed);
        l.unreplayed.set(true);
    }

    let g = j.pin_resize_lock.write();
    let old = core::mem::replace(unsafe { &mut j.pins.protected_mut().entries }, entries);
    let r = j.pin_range();
    r.front.set(last_seq);
    r.back.set(cur_seq);
    r.size.set(size);
    drop(g);
    drop(old);
    Ok(())
}

#[c_export]
pub fn bch2_journal_pin_set(j: &c::journal, seq: u64, pin: &Opaque<c::journal_entry_pin>,
                            ty: c::journal_pin_type, flush: c::journal_pin_flush_fn) {
    j.pins().set(seq, pin, ty, flush)
}

#[c_export]
pub fn bch2_journal_pin_copy(j: &c::journal, dst: &Opaque<c::journal_entry_pin>,
                             src: &Opaque<c::journal_entry_pin>, ty: c::journal_pin_type,
                             flush: c::journal_pin_flush_fn) {
    j.pins().copy(dst, src, ty, flush)
}

#[c_export]
pub fn bch2_journal_pin_drop(j: &c::journal, pin: &Opaque<c::journal_entry_pin>) {
    j.pins().drop_pin(pin)
}

#[c_export]
pub fn bch2_journal_pin_fifo_want_resize(j: &c::journal) -> bool {
    want_size(j.pin_range()) != j.pin_range().size.get()
}

#[c_export]
pub fn bch2_journal_pin_fifo_resize(j: &c::journal) -> c_int {
    ret_to_c(resize(j))
}

#[c_export]
pub fn bch2_journal_pins_start(j: &c::journal, last_seq: u64, cur_seq: u64) -> c_int {
    ret_to_c(start(j, last_seq, cur_seq))
}

#[c_export]
pub fn bch2_journal_pins_front(j: &c::journal) -> u64 {
    j.pin_range().front.get()
}

#[c_export]
pub fn bch2_journal_pins_back(j: &c::journal) -> u64 {
    j.pin_range().back.get()
}

#[c_export]
pub fn bch2_journal_pins_size(j: &c::journal) -> u64 {
    j.pin_range().size.get()
}

/* The most dirty journal entries we let accumulate before reclaim must act */
#[c_export]
pub fn bch2_journal_pin_limit(j: &c::journal) -> u64 {
    j.pin_range().size.get().min(JOURNAL_PIN_MAX)
}

#[c_export]
pub fn bch2_journal_pins_used(j: &c::journal) -> u64 {
    let r = j.pin_range();
    r.back.get() - r.front.get()
}

macro_rules! held_exports {
    ($($cname:ident => $method:ident($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;)*) => { $(
        /// # Safety
        /// The caller holds pin_resize_lock for read - and j->lock, where an
        /// entry's bytes, or the fifo's front or back, change.
        #[c_export]
        pub unsafe fn $cname(j: &c::journal, $($arg: $ty),*) $(-> $ret)? {
            unsafe { j.pins_held() }.$method($($arg),*)
        }
    )* };
}

held_exports! {
    bch2_journal_pins_push              => push(seq: u64);
    bch2_journal_pins_advance_front     => advance_front(last_seq_ondisk: u64);
    bch2_journal_pins_last_dev_seq      => last_dev_seq(dev: c_int, replicas: c_uint) -> u64;
    bch2_journal_seq_put                => seq_put(seq: u64) -> bool;
    bch2_journal_seq_unpinned           => seq_unpinned(seq: u64) -> bool;
    bch2_journal_seq_replayed           => seq_replayed(seq: u64) -> bool;
    bch2_journal_seq_set_bytes          => seq_set_bytes(seq: u64, bytes: c_uint);
    bch2_journal_seq_take_bytes         => seq_take_bytes(seq: u64) -> c_uint;
    bch2_journal_seq_devs               => seq_devs(seq: u64) -> c::bch_devs_list;
    bch2_journal_seq_set_devs           => seq_set_devs(seq: u64, devs: &c::bch_devs_list);
}

#[c_export]
pub fn bch2_journal_next_pin(j: &c::journal, seq_to_flush: u64, below: c_uint, above: c_uint,
                             seq: &mut u64, flush: &mut c::journal_pin_flush_fn)
    -> *mut c::journal_entry_pin
{
    let Some((pin, s, f)) = j.pins().next_to_flush(seq_to_flush, below, above) else {
        return ptr::null_mut();
    };
    *seq = s;
    *flush = f;
    pin
}

#[c_export]
pub fn bch2_journal_pin_flushed(j: &c::journal, pin: &Opaque<c::journal_entry_pin>, seq: u64,
                                ok: bool) {
    j.pins().flushed(pin, seq, ok)
}

#[c_export]
pub fn bch2_journal_pins_still_flushing(j: &c::journal, seq_to_flush: u64, types: c_uint) -> bool {
    j.pins().still_flushing(seq_to_flush, types)
}

#[c_export]
pub fn bch2_journal_seq_pins_to_text(out: &mut Printbuf, j: &c::journal, seq: &mut u64,
                                     nr: Option<&mut c_uint>) -> bool {
    if !j.flags.get(c::journal_flags::JOURNAL_running) {
        return true;
    }

    *seq = (*seq).max(j.lock.last_seq.get());

    let pins = j.pins();
    if *seq >= pins.back() {
        return true;
    }

    /* nothing holds it - only reclaim hasn't got to it yet */
    if pins.slot(*seq).count.load(Relaxed) == 0 {
        return false;
    }

    pins.to_text(out, *seq, nr);
    false
}
