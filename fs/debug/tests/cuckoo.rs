// SPDX-License-Identifier: GPL-2.0

//! data_structures/cuckoo.rs's tests, run by perf_test, and by cargo test with
//! EXTRA_CFLAGS=-DCONFIG_BCACHEFS_TESTS - built only with CONFIG_BCACHEFS_TESTS.
//! A lookup that misses is silent: these check every
//! lookup against what it should find. Each runs on sets and on maps of
//! entries (TestItem), by SipHash and by golden ratio, in two ways and three.
//!
//! test_cuckoo: one thread, against a model, through evictions, rehashes and
//! clear() - every kind of table.
//!
//! test_cuckoo_torture: WRITERS writers at once, with no lock, churn KEYS
//! keys, 0 and (1 << 63) - 3 among them, through every path: eviction
//! chains, rehashes as the table fills and empties. Each has
//! its own share of the keys, so a key's ops are one writer's, in order, and
//! its state below stays exact. HOT of them are only ever inserted, so a
//! map's entries for them are replaced under readers all the time. Every
//! EPOCH ops each, the writers meet: the last in checks the table against
//! their counts, and now and then clear()s it - so clear() runs under
//! readers, but not beside another writer's op, whose effect the per-key
//! state couldn't follow. Readers on every other CPU look
//! keys up, half on the hot keys, an eighth of their runs in one RCU section
//! across 64 lookups, so a buffer replaced meanwhile must stay readable and
//! right. Round by round, faults make the rare paths routine (rounds()), each
//! round printing how often it reached them - for a SipHash 2-way and a
//! golden ratio 3-way table, of each kind. Then the same faults again, for
//! shared rounds: every writer insert_new()s the same SHARED keys at once,
//! so inserts of a key race each other, in one table and across rehashes,
//! and at each meet exactly one of them must be in.
//!
//! The check is exact, not statistical: each key has a sequence count, odd
//! while an op on it is in flight, and the value the op left - an entry's,
//! or a set's presence. A lookup between two loads of the count that see it
//! even and unchanged must find exactly that value, or nothing if that's what
//! it is - the count's Release after the op and the reader's Acquire put the
//! op before the lookup, and the next op's fence after its odd store puts the
//! lookup before anything it does - so a mismatch is a bug, never a race the
//! test can't see. Writers check each result against the same state; an
//! allocation failure is a failure unless injected; at rest, every key is
//! checked as its last op left it.
//!
//! test_cuckoo_bench: for each hash and ways, on sets - the cost of an
//! insert, a hit, a miss and a remove, at sizes from cache-resident to well
//! past it; readers' lookups a second at 1, 2, 4... CPUs; and readers on all
//! but one CPU with a writer removing and putting back members under them -
//! hits and misses apart, as only a miss should see the writer. Members are
//! odd keys, misses even ones.

use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering::{AcqRel, Acquire, Relaxed, Release}};

use super::TestRet;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::locking::Locked;
use crate::new_lock;
use crate::prt;
use crate::util::alloc::KVVec;
use crate::data_structures::cuckoo::{Cuckoo, CuckooHasher, CuckooItem, CuckooMultimap, Entry, Faults, Registration};
use crate::util::hash::GoldenRatio;
use crate::util::siphash::SipHash13Key;
use crate::os::kernel::{cond_resched, local_clock, num_online_cpus, random_u64, random_u64_below};
use crate::util::par::{self, Backend};
use crate::util::printbuf::{HumanReadable, Printbuf};
use crate::util::projections::Volatile;
use crate::util::rcu::read_lock;

#[cfg(not(kernel))]
use bcachefs_shim as kernel;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// A map's entry: its key, and the value the op that put it in left.
struct TestEntry {
    key: u64,
    val: AtomicU64,
}

crate::keyed_by!(TestEntry, |e| &e.key => u64);

/// What the tests put in a table: an item for a key's op, and what an item
/// found says - 0 for absent, else its value.
trait TestItem: CuckooItem<Target: core::borrow::Borrow<u64>> + Send + 'static {
    /// The entries a map's items point to: four a key, an op putting in the
    /// next, so the value it replaced is told from its own - and one isn't
    /// reused until three more ops on its key, which a lookup holding it sees
    /// in the key's sequence count. They outlive the table.
    type Pool: Sync;

    fn pool(keys: &[u64]) -> Result<Self::Pool, BchError>;

    /// Key @i's item, value @v, for the op at @seq.
    fn make(pool: &Self::Pool, i: usize, k: u64, seq: u64, v: u64) -> Self;

    fn value(self) -> u64;
}

impl TestItem for u64 {
    type Pool = ();

    fn pool(_keys: &[u64]) -> Result<(), BchError> {
        Ok(())
    }

    fn make(_pool: &(), _i: usize, k: u64, _seq: u64, _v: u64) -> u64 {
        k
    }

    fn value(self) -> u64 {
        1
    }
}

impl TestItem for Entry<TestEntry> {
    type Pool = KVVec<[TestEntry; 4]>;

    fn pool(keys: &[u64]) -> Result<Self::Pool, BchError> {
        let mut pool = KVVec::new();
        for &key in keys {
            pool.push(core::array::from_fn(|_| TestEntry { key, val: AtomicU64::new(0) }))?;
        }
        Ok(pool)
    }

    fn make(pool: &Self::Pool, i: usize, _k: u64, seq: u64, v: u64) -> Self {
        let e = &pool[i][(seq / 2) as usize % 4];
        e.val.store(v, Relaxed);
        // SAFETY: the pool outlives every table, and its keys never change
        unsafe { Entry::new(NonNull::from(e)) }
    }

    fn value(self) -> u64 {
        // SAFETY: the pool outlives every table
        unsafe { self.as_ptr().as_ref() }.val.load(Relaxed)
    }
}

type Map = Entry<TestEntry>;

// ── The model test ───────────────────────────────────────────────────────

fn model<I: TestItem, S: CuckooHasher, const W: usize>(name: &str) -> TestRet {
    const NR: usize = 512;
    let key = |i: usize| (i as u64).wrapping_mul(0x9E3779B97F4A7C15) >> 2;
    let mut keys = KVVec::new();
    for i in 0..NR {
        keys.push(key(i))?;
    }
    let pool = I::pool(&keys)?;

    let t = Cuckoo::<I, S, W>::default();
    // What each key's last op left it, and its ops so far that changed it,
    // make()'s @seq - so an op's item is never the entry in the table: on the
    // heap, as a kernel stack is small
    let mut st = KVVec::new();
    for _ in 0..NR {
        st.push((0u64, 0u64))?;
    }

    for op in 0..20_000u64 {
        let i = random_u64_below(NR as u64) as usize;
        let (k, (want, ops)) = (key(i), &mut st[i]);
        *ops += 1;
        let item = I::make(&pool, i, k, *ops * 2, op | 1 << 63);
        match random_u64_below(4) {
            0 => {
                assert_eq!(t.remove(&k).map_or(0, I::value), *want, "{name}: remove of {k:#x}");
                *want = 0;
            }
            1 => match t.insert_new(item)? {
                Ok(()) => {
                    assert_eq!(*want, 0, "{name}: insert_new of {k:#x}, which was in");
                    *want = item.value();
                }
                Err(had) => {
                    assert_eq!(had.value(), *want, "{name}: insert_new of {k:#x}");
                    *ops -= 1;
                }
            },
            _ => {
                assert_eq!(t.insert(item)?.map_or(0, I::value), *want, "{name}: insert of {k:#x}");
                *want = item.value();
            }
        }
    }

    for (i, &(want, _)) in st.iter().enumerate() {
        assert_eq!(t.get(&key(i)).map_or(0, I::value), want, "{name}: lookup of {:#x}", key(i));
    }
    assert_eq!(t.check().unwrap_or_else(|e| panic!("{name}: {e}")), st.iter().filter(|s| s.0 != 0).count());

    t.clear();
    assert!(t.get(&key(1)).is_none(), "{name}: cleared, but not empty");
    Ok(())
}

/// A dropped Registration's entry is out of its table - which nothing else
/// would show, but every later register() of its key failing.
fn registration() -> TestRet {
    let t = Cuckoo::<Entry<u64>>::default();
    let e = 7u64;
    // SAFETY: @e outlives its registration, and the table is read only here
    let r = unsafe { Registration::register(&t, e, NonNull::from(&e)) };
    assert!(r.is_ok() && t.contains(&e), "registration: not in the table");
    drop(r);
    assert!(!t.contains(&e), "registration: dropped, and still in the table");
    Ok(())
}

fn models() -> TestRet {
    registration()?;
    model::<u64, SipHash13Key, 2>("set, siphash, 2-way")?;
    model::<u64, SipHash13Key, 3>("set, siphash, 3-way")?;
    model::<u64, GoldenRatio, 2>("set, golden, 2-way")?;
    model::<u64, GoldenRatio, 3>("set, golden, 3-way")?;
    model::<Map, SipHash13Key, 2>("map, siphash, 2-way")?;
    model::<Map, SipHash13Key, 3>("map, siphash, 3-way")?;
    model::<Map, GoldenRatio, 2>("map, golden, 2-way")?;
    model::<Map, GoldenRatio, 3>("map, golden, 3-way")
}

pub(super) fn test_cuckoo(_fs: &Fs, _nr: u64) -> TestRet {
    models()
}

// ── The torture test ─────────────────────────────────────────────────────

const KEYS: usize = 4096;
const HOT: usize = 16;
const WRITERS: usize = 4;
/// Ops each writer does between meetings
const EPOCH: u64 = 1024;

/// A key's state: @seq odd while an op on it is in flight, and @expect what
/// the last op left it - 0, absent.
#[derive(Default)]
struct KeyState {
    seq:    AtomicU64,
    expect: AtomicU64,
}

/// The smallest and largest a key can be are among them; the rest are
/// scattered.
fn key(i: usize) -> u64 {
    match i {
        0 => 0,
        1 => (1 << 63) - 3,
        _ => (i as u64).wrapping_mul(0x9E3779B97F4A7C15) >> 2,
    }
}

/// Never 0, which @expect means absent; another for every op on a key, so a
/// value an op replaced is told from the one that replaced it.
fn val(key: u64, seq: u64) -> u64 {
    (key.wrapping_mul(0x61C8864680B583EB) ^ seq) | 1 << 63
}

struct Failure {
    what: &'static str,
    key:  u64,
    seq:  u64,
    want: u64,
    got:  u64,
}

/// The first failure, and how many: nonzero stops every thread.
type Failures = Locked<c::spinlock_t, Option<Failure>, Volatile<u64>>;

fn fail(fails: &Failures, what: &'static str, key: u64, seq: u64, want: u64, got: u64) {
    let mut g = fails.lock();
    let nr = g.published().get();
    g.published().set(nr + 1);
    if nr == 0 {
        *g = Some(Failure { what, key, seq, want, got });
    }
}

/// Plain, then with each of the table's faults, then all of them.
fn rounds() -> [(&'static str, Faults); 6] {
    let dense = || Faults { dense: true, ..Faults::default() };
    [
        ("plain",      Faults::default()),
        // Grown only when full: most inserts evict, chains run long, and rehash
        ("dense",      dense()),
        // A writer slowed in every move, and between a mark and its settling:
        // what the move count, and the waits on what's unstable, are for
        ("moves",      Faults { move_window: 2000, ..dense() }),
        // Lookups slowed between their slots: moves land between the reads
        ("reads",      Faults { read_window: 500, ..dense() }),
        // Rehashes failing to allocate: inserts fail, the table as it was
        ("failures",   Faults { alloc_fail: 4, ..dense() }),
        ("everything", Faults { move_window: 500, read_window: 200, alloc_fail: 4, ..dense() }),
    ]
}

/// Writer @w's @nr'th op, on one of its keys, bracketed by the key's @seq.
#[allow(clippy::too_many_arguments)]
fn writer_op<I: TestItem, S: CuckooHasher, const W: usize>(
    t: &Cuckoo<I, S, W>, w: usize, items: &AtomicU64, keys: &[KeyState], pool: &I::Pool,
    fails: &Failures, injecting: bool, rng: &mut u64, nr: u64,
) {
    let r = xorshift(rng);

    // An eighth of the ops on the hot keys, which are only ever inserted - a
    // map's entry replaced under readers, after the first - the rest on any:
    // inserts mostly, then removes mostly, then removes only, in turn, so the
    // table fills, and rehashes, and empties to the hot keys, and shrinks.
    // Each key's writer is its index mod WRITERS.
    let hot = r & 7 == 0;
    let share = if hot { HOT } else { KEYS } / WRITERS;
    let i = (r >> 3) as usize % share * WRITERS + w;
    let phase = nr % 50_000;
    let filling = phase < 25_000;
    let insert = hot || match phase {
        0..25_000      => (r >> 32) % 4 != 0,
        25_000..37_500 => (r >> 32) % 4 == 0,
        _              => false,
    };

    let (st, k) = (&keys[i], key(i));
    let seq = st.seq.load(Relaxed);
    st.seq.store(seq + 1, Relaxed);
    fence(Release);

    let want = st.expect.load(Relaxed);
    let (got, now) = if insert {
        let item = I::make(pool, i, k, seq + 2, val(k, seq + 2));
        // Half the inserts of a key that's out by insert_new(), which puts
        // it in: an item had back is got
        let ret = if want == 0 && (r >> 40) & 1 != 0 {
            t.insert_new(item).map(Result::err)
        } else {
            t.insert(item)
        };
        match ret {
            Ok(old) => (old.map_or(0, I::value), item.value()),
            // An insert of an item in the table never allocates: this one
            // wasn't, and still isn't
            Err(_) => {
                if !injecting {
                    fail(fails, "allocation failure, uninjected", k, seq, want, 0);
                }
                (0, want)
            }
        }
    } else {
        (t.remove(&k).map_or(0, I::value), 0)
    };
    // emptying, now and then a shrink, against the other writers' ops; its
    // allocation failing, injected, leaves the table as it was
    if !filling && (r >> 48) & 0xff == 0 {
        let _ = t.shrink();
    }
    if got != want {
        fail(fails, "insert or remove", k, seq, want, got);
    }

    match (want != 0, now != 0) {
        (false, true) => { items.fetch_add(1, Relaxed); }
        (true, false) => { items.fetch_sub(1, Relaxed); }
        _             => {}
    }
    st.expect.store(now, Relaxed);
    st.seq.store(seq + 2, Release);
}

/// Where the writers meet, every EPOCH ops each.
#[derive(Default)]
struct Rendezvous {
    arrived: AtomicU32,
    /// Meetings so far
    met:     AtomicU64,
    stop:    AtomicBool,
}

impl Rendezvous {
    /// Wait for every writer; the last in runs @f, with the rest stopped,
    /// and its answer - whether they stop - is every writer's.
    fn meet(&self, f: impl FnOnce() -> bool) -> bool {
        let met = self.met.load(Acquire);
        if self.arrived.fetch_add(1, AcqRel) + 1 == WRITERS as u32 {
            self.stop.store(f(), Relaxed);
            self.arrived.store(0, Relaxed);
            self.met.store(met + 1, Release);
        } else {
            while self.met.load(Acquire) == met {
                cond_resched();
            }
        }
        self.stop.load(Relaxed)
    }
}

/// With every writer stopped: the table checked, its count against theirs,
/// and now and then cleared, bracketing every key. Whether to stop.
fn writers_met<I: TestItem, S: CuckooHasher, const W: usize>(
    t: &Cuckoo<I, S, W>, items: &[AtomicU64; WRITERS], keys: &[KeyState], fails: &Failures, end: u64,
) -> bool {
    let nr: u64 = items.iter().map(|n| n.load(Relaxed)).sum();
    match t.check() {
        Err(e) => fail(fails, e, 0, 0, 0, 0),
        Ok(n) if n as u64 != nr => fail(fails, "item count", 0, 0, nr, n as u64),
        Ok(_) => {}
    }

    if random_u64_below(12) == 0 {
        keys.iter().for_each(|st| { st.seq.fetch_add(1, Relaxed); });
        fence(Release);
        t.clear();
        items.iter().for_each(|n| n.store(0, Relaxed));
        keys.iter().for_each(|st| {
            st.expect.store(0, Relaxed);
            st.seq.fetch_add(1, Release);
        });
    }
    fails.get() != 0 || local_clock() >= end
}

/// One round: a fresh table with @faults, WRITERS writers and the rest of the
/// CPUs reading, for @secs.
fn round<I: TestItem, S: CuckooHasher, const W: usize>(table: &str, name: &str, faults: Faults, secs: u64)
    -> TestRet
{
    let readers = num_online_cpus().saturating_sub(WRITERS as u32).clamp(2, 30);
    let injecting = faults.alloc_fail != 0;

    let (mut keys, mut key_vals) = (KVVec::new(), KVVec::new());
    for i in 0..KEYS {
        keys.push(KeyState::default())?;
        key_vals.push(key(i))?;
    }
    let keys = &keys[..];
    let pool = I::pool(&key_vals)?;
    let pool = &pool;

    let t = Cuckoo::<I, S, W>::with_faults(faults);
    let t = &t;
    pin_init::stack_pin_init!(let fails: Failures = new_lock!(None, pin_init::Zeroable::zeroed()));
    let fails: &Failures = &fails;
    // Each writer's count of the items its keys have in the table
    let items: &[AtomicU64; WRITERS] = &Default::default();
    let meet = &Rendezvous::default();

    let writers_left = AtomicU32::new(WRITERS as u32);
    let (checked, raced, ops) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let start = local_clock();
    let end = start + secs * c::NSEC_PER_SEC as u64;

    let ret = par::scope(Backend::Kthreads, |s| {
        for i in 0..readers {
            let (writers_left, checked, raced) = (&writers_left, &checked, &raced);
            s.spawn(format_args!("cuckoo torture reader[{i}]"), move || {
                let mut rng = random_u64() | 1;
                let (mut n, mut skipped) = (0u64, 0u64);

                while writers_left.load(Acquire) != 0 && fails.get() == 0 {
                    let g = (xorshift(&mut rng) % 8 == 0).then(read_lock);
                    for _ in 0..64 {
                        let r = xorshift(&mut rng);
                        let i = if r & 1 != 0 { r >> 1 } else { (r >> 1) % HOT as u64 } as usize % KEYS;
                        let (st, k) = (&keys[i], key(i));

                        let seq = st.seq.load(Acquire);
                        if seq & 1 != 0 {
                            skipped += 1;
                            continue;
                        }
                        let want = st.expect.load(Relaxed);
                        let got = match &g {
                            Some(g) => t.get_rcu(&k, g),
                            None    => t.get(&k),
                        }.map_or(0, I::value);
                        fence(Acquire);
                        if st.seq.load(Relaxed) != seq {
                            skipped += 1;
                            continue;
                        }

                        n += 1;
                        if got != want {
                            fail(fails, "lookup", k, seq, want, got);
                        }
                    }
                    drop(g);
                    cond_resched();
                }
                checked.fetch_add(n, Relaxed);
                raced.fetch_add(skipped, Relaxed);
                Ok(())
            })?;
        }

        for w in 0..WRITERS {
            let (writers_left, ops) = (&writers_left, &ops);
            s.spawn(format_args!("cuckoo torture writer[{w}]"), move || {
                let mut rng = random_u64() | 1;
                let mut nr = 0;
                loop {
                    writer_op(t, w, &items[w], keys, pool, fails, injecting, &mut rng, nr);
                    nr += 1;
                    cond_resched();
                    if nr % EPOCH == 0 && meet.meet(|| writers_met(t, items, keys, fails, end)) {
                        break;
                    }
                }
                ops.fetch_add(nr, Relaxed);
                writers_left.fetch_sub(1, Release);
                Ok(())
            })?;
        }
        Ok(())
    });
    let ms = (local_clock() - start) / 1_000_000;

    if let Some(f) = fails.lock().take() {
        panic!("cuckoo torture, {table}, round {name}: {} failures, the first a {} of {:#x} at seq {}: \
                wanted {:#x}, got {:#x}",
               fails.get(), f.what, f.key, f.seq, f.want, f.got);
    }
    ret?;

    // Quiesced: every key as its last op left it
    for (i, st) in keys.iter().enumerate() {
        let k = key(i);
        assert_eq!(t.get(&k).map_or(0, I::value), st.expect.load(Relaxed),
                   "cuckoo torture, {table}, round {name}: {k:#x} at rest");
    }
    assert_eq!(t.check().unwrap_or_else(|e| panic!("{e}")) as u64,
               items.iter().map(|n| n.load(Relaxed)).sum::<u64>());

    let checked = checked.into_inner();
    assert!(checked > 0, "cuckoo torture, {table}, round {name}: no lookup ever checked");

    let seen = &t.faults().seen;
    let n = |c: &AtomicU64| HumanReadable(c.load(Relaxed));
    let mut msg = Printbuf::new();
    prt!(&mut msg, "cuckoo torture {table}, {name:<10} {ms}ms, {readers} readers: {} lookups checked, \
                    {} raced an op; {} writer ops, {} moves; retries {} unstable waits {} own-seed probes {} rehashes {} \
                    shrunk {} grown {} busy {} withdrawn {} stale {}\n",
         HumanReadable(checked), HumanReadable(raced.into_inner()), HumanReadable(ops.into_inner()),
         HumanReadable(t.moved()), n(&seen.retry), n(&seen.unstable), n(&seen.raced), n(&seen.rehash),
         n(&seen.shrunk), n(&seen.grown), n(&seen.busy), n(&seen.withdrawn), n(&seen.stale));
    kernel::pr_info!("{}", msg.as_str());
    Ok(())
}

/// Keys every writer inserts, in a shared round.
const SHARED: usize = 1024;

/// With every writer stopped, after a pass: each key in if and only if one
/// insert_new() of it got in, and none in twice - then every key out again,
/// by clear() or one by one. Whether to stop.
fn shared_met<I: TestItem, S: CuckooHasher, const W: usize>(
    t: &Cuckoo<I, S, W>, keys: &[u64], wins: &[AtomicU32], fails: &Failures, pass: u64, end: u64,
) -> bool {
    let mut present = 0;
    for (i, &k) in keys.iter().enumerate() {
        let won = wins[i].swap(0, Relaxed);
        let found = t.get(&k).is_some();
        if won > 1 {
            fail(fails, "insert_new() of a key in twice", k, pass, 1, won.into());
        } else if found != (won == 1) {
            fail(fails, "a key's insert_new() and its lookup", k, pass, won.into(), found.into());
        }
        present += found as usize;
    }
    match t.check() {
        Err(e) => fail(fails, e, 0, pass, 0, 0),
        Ok(n) if n != present => fail(fails, "item count", 0, pass, present as u64, n as u64),
        Ok(_) => {}
    }

    if pass % 2 == 0 {
        t.clear();
    } else {
        keys.iter().for_each(|k| { t.remove(k); });
    }
    fails.get() != 0 || local_clock() >= end
}

/// A shared round: every writer insert_new()s every one of SHARED keys, all
/// in the same order each pass, so of each key's WRITERS inserts - racing in
/// one table, or across a rehash - exactly one gets in. Each writer's items
/// are its own pool entries, so a losing insert's entry is never one in.
fn shared<I: TestItem, S: CuckooHasher, const W: usize>(table: &str, name: &str, faults: Faults, secs: u64)
    -> TestRet
{
    let injecting = faults.alloc_fail != 0;

    let (mut keys, mut wins) = (KVVec::new(), KVVec::new());
    for i in 0..SHARED {
        keys.push(key(i))?;
        wins.push(AtomicU32::new(0))?;
    }
    let (keys, wins) = (&keys[..], &wins[..]);
    let pool = I::pool(keys)?;
    let pool = &pool;

    let t = Cuckoo::<I, S, W>::with_faults(faults);
    let t = &t;
    pin_init::stack_pin_init!(let fails: Failures = new_lock!(None, pin_init::Zeroable::zeroed()));
    let fails: &Failures = &fails;
    let meet = &Rendezvous::default();
    let readers = num_online_cpus().saturating_sub(WRITERS as u32).clamp(2, 30);
    let writers_left = AtomicU32::new(WRITERS as u32);
    let (passes, checked) = (AtomicU64::new(0), AtomicU64::new(0));
    let start = local_clock();
    let end = start + secs * c::NSEC_PER_SEC as u64;

    let ret = par::scope(Backend::Kthreads, |s| {
        // Within a pass - between two meets - a key found is found as the same
        // insert every time: a tentative one returned, then withdrawn, isn't
        for r in 0..readers {
            let (writers_left, checked) = (&writers_left, &checked);
            s.spawn(format_args!("cuckoo shared reader[{r}]"), move || {
                let mut found = KVVec::new();
                for _ in 0..SHARED {
                    found.push(0u64)?;
                }
                let (mut rng, mut pass, mut n) = (random_u64() | 1, u64::MAX, 0u64);

                while writers_left.load(Acquire) != 0 && fails.get() == 0 {
                    let m = meet.met.load(Acquire);
                    if m != pass {
                        found.iter_mut().for_each(|v| *v = 0);
                        pass = m;
                    }
                    for _ in 0..64 {
                        let i = (xorshift(&mut rng) % SHARED as u64) as usize;
                        let got = t.get(&keys[i]).map_or(0, I::value);
                        if meet.met.load(Acquire) != m {
                            break;
                        }
                        if got == 0 {
                            continue;
                        }
                        n += 1;
                        if found[i] == 0 {
                            found[i] = got;
                        } else if found[i] != got {
                            fail(fails, "a key found as two of its inserts in a pass", keys[i], m, found[i], got);
                        }
                    }
                    cond_resched();
                }
                checked.fetch_add(n, Relaxed);
                Ok(())
            })?;
        }

        for w in 0..WRITERS {
            let (passes, writers_left) = (&passes, &writers_left);
            s.spawn(format_args!("cuckoo shared writer[{w}]"), move || {
                let mut pass = 0u64;
                loop {
                    for n in 0..SHARED {
                        let i = (n + pass as usize * 7) % SHARED;
                        let k = keys[i];
                        match t.insert_new(I::make(pool, i, k, 2 * w as u64, val(k, w as u64))) {
                            Ok(Ok(())) => { wins[i].fetch_add(1, Relaxed); }
                            Ok(Err(_)) => {}
                            Err(_) if injecting => {}
                            Err(_) => fail(fails, "allocation failure, uninjected", k, pass, 0, 0),
                        }
                    }
                    pass += 1;
                    cond_resched();
                    if meet.meet(|| shared_met(t, keys, wins, fails, pass, end)) {
                        break;
                    }
                }
                passes.fetch_max(pass, Relaxed);
                writers_left.fetch_sub(1, Release);
                Ok(())
            })?;
        }
        Ok(())
    });
    let ms = (local_clock() - start) / 1_000_000;

    if let Some(f) = fails.lock().take() {
        panic!("cuckoo torture, {table}, shared round {name}: {} failures, the first a {} of {:#x} in pass {}: \
                wanted {:#x}, got {:#x}",
               fails.get(), f.what, f.key, f.seq, f.want, f.got);
    }
    ret?;

    let seen = &t.faults().seen;
    let n = |c: &AtomicU64| HumanReadable(c.load(Relaxed));
    let mut msg = Printbuf::new();
    prt!(&mut msg, "cuckoo torture {table}, shared {name:<10} {ms}ms: {} passes of {SHARED} keys by {WRITERS} writers, \
                    {readers} readers: {} lookups checked; {} moves; retries {} unstable waits {} rehashes {} grown {} \
                    busy {} withdrawn {} stale {}\n",
         passes.into_inner(), HumanReadable(checked.into_inner()), HumanReadable(t.moved()), n(&seen.retry),
         n(&seen.unstable), n(&seen.rehash), n(&seen.grown), n(&seen.busy), n(&seen.withdrawn), n(&seen.stale));
    kernel::pr_info!("{}", msg.as_str());
    Ok(())
}

fn torture<I: TestItem, S: CuckooHasher, const W: usize>(table: &str) -> TestRet {
    for (name, faults) in rounds() {
        round::<I, S, W>(table, name, faults, 5)?;
    }
    for (name, faults) in rounds() {
        shared::<I, S, W>(table, name, faults, 2)?;
    }
    Ok(())
}

/// WRITERS writers add and remove values under MULTIMAP_KEYS keys they
/// share, each only its own value - up to MULTIMAP_EACH of it under a key, so
/// a key's group spills, and is made and emptied by one writer while another
/// adds to it. After each op the writer counts its value under the key, which
/// no other changes: exactly what it's left there, or an add or a remove was
/// lost to a group's coming or going. At the end each takes its values out,
/// and the multimap's empty.
fn multimap(secs: u64) -> TestRet {
    const MULTIMAP_KEYS: u64 = 8;
    const MULTIMAP_EACH: u32 = 3;

    let m = CuckooMultimap::<u64, u32, 2>::default();
    let m = &m;
    pin_init::stack_pin_init!(let fails: Failures = new_lock!(None, pin_init::Zeroable::zeroed()));
    let fails: &Failures = &fails;
    let ops = &AtomicU64::new(0);
    let end = local_clock() + secs * c::NSEC_PER_SEC as u64;

    let ret = par::scope(Backend::Kthreads, |s| {
        for w in 0..WRITERS as u32 {
            s.spawn(format_args!("cuckoo multimap writer[{w}]"), move || {
                let mut have = [0u32; MULTIMAP_KEYS as usize];
                let mut rng = random_u64() | 1;
                let mut nr = 0u64;

                while local_clock() < end && fails.get() == 0 {
                    let r = xorshift(&mut rng);
                    let k = r % MULTIMAP_KEYS;
                    let n = &mut have[k as usize];
                    if *n < MULTIMAP_EACH && (*n == 0 || (r >> 8) & 1 != 0) {
                        m.add(k, w)?;
                        *n += 1;
                    } else if !m.remove(&k, &w) {
                        fail(fails, "multimap remove, of one in", k, nr, *n as u64, 0);
                    } else {
                        *n -= 1;
                    }

                    let got = m.with(&k, |vs| vs.iter().filter(|&&v| v == w).count());
                    if got != *n as usize {
                        fail(fails, "multimap count", k, nr, *n as u64, got as u64);
                    }
                    nr += 1;
                    if nr % 1024 == 0 {
                        cond_resched();
                    }
                }

                for (k, &n) in have.iter().enumerate() {
                    for _ in 0..n {
                        if !m.remove(&(k as u64), &w) {
                            fail(fails, "multimap remove at the end", k as u64, nr, n as u64, 0);
                        }
                    }
                }
                ops.fetch_add(nr, Relaxed);
                Ok(())
            })?;
        }
        Ok(())
    });

    if let Some(f) = fails.lock().take() {
        panic!("cuckoo multimap: {} failures, the first a {} of {:#x} at op {}: wanted {}, got {}",
               fails.get(), f.what, f.key, f.seq, f.want, f.got);
    }
    ret?;
    assert!(m.is_empty(), "cuckoo multimap: groups left with every value out");
    kernel::pr_info!("cuckoo multimap: {} ops by {WRITERS} writers on {MULTIMAP_KEYS} keys\n",
                     ops.load(Relaxed));
    Ok(())
}

fn tortures() -> TestRet {
    torture::<Map, SipHash13Key, 2>("map, siphash, 2-way")?;
    torture::<Map, GoldenRatio, 3>("map, golden, 3-way")?;
    torture::<u64, SipHash13Key, 2>("set, siphash, 2-way")?;
    torture::<u64, GoldenRatio, 3>("set, golden, 3-way")?;
    multimap(5)
}

pub(super) fn test_cuckoo_torture(_fs: &Fs, _nr: u64) -> TestRet {
    tortures()
}

// ── The benchmark ────────────────────────────────────────────────────────

fn pick(keys: &[u64], rng: &mut u64) -> u64 {
    keys[(xorshift(rng) % keys.len() as u64) as usize]
}

/// @nr lookups of @keys at random, timed, and how many were found - so they
/// aren't optimized out.
fn bench_lookups<S: CuckooHasher, const W: usize>(t: &Cuckoo<u64, S, W>, keys: &[u64], nr: u64, rng: &mut u64)
    -> (u64, u64)
{
    let start = local_clock();
    let found = (0..nr).filter(|_| t.get(&pick(keys, rng)).is_some()).count() as u64;
    (local_clock() - start, core::hint::black_box(found))
}

/// @threads readers looking up @keys at random - and, with @churn, a writer
/// removing and putting back those members under them - all from one moment
/// to another @ms later: the readers' lookups a second, in millions, and the
/// writer's ops a second. Counted in the window every thread is in, not
/// timed by the slowest, so one descheduled reader doesn't skew the rest.
fn bench_threads<S: CuckooHasher, const W: usize>(
    t: &Cuckoo<u64, S, W>, keys: &[u64], ms: u64, threads: u32, churn: Option<&[u64]>,
) -> Result<(u64, u64), BchError> {
    let (lookups, writes) = (AtomicU64::new(0), AtomicU64::new(0));
    // Time for every thread to have started
    let start = local_clock() + 100 * 1_000_000;
    let end = start + ms * 1_000_000;
    let wait_start = || while local_clock() < start {
        core::hint::spin_loop();
    };

    par::scope(Backend::Kthreads, |s| {
        for i in 0..threads {
            let lookups = &lookups;
            s.spawn(format_args!("cuckoo bench reader[{i}]"), move || {
                let mut rng = random_u64() | 1;
                let (mut n, mut found) = (0u64, 0u64);
                wait_start();
                while local_clock() < end {
                    for _ in 0..256 {
                        found += t.get(&pick(keys, &mut rng)).is_some() as u64;
                    }
                    n += 256;
                }
                core::hint::black_box(found);
                lookups.fetch_add(n, Relaxed);
                Ok(())
            })?;
        }
        if let Some(members) = churn {
            let writes = &writes;
            s.spawn(format_args!("cuckoo bench writer"), move || {
                let mut rng = random_u64() | 1;
                let mut n = 0u64;
                wait_start();
                while local_clock() < end {
                    let k = pick(members, &mut rng);
                    t.remove(&k);
                    t.insert(k)?;
                    n += 2;
                }
                writes.fetch_add(n, Relaxed);
                Ok(())
            })?;
        }
        Ok(())
    })?;

    Ok((lookups.into_inner() / (ms * 1000), writes.into_inner() * 1000 / ms))
}

fn bench<S: CuckooHasher, const W: usize>(table: &str) -> TestRet {
    let cpus = num_online_cpus();

    for shift in [10u32, 14, 18, 22] {
        let n = 1usize << shift;
        let lookups = (n as u64).max(1 << 22);

        let mut rng = random_u64() | 1;
        let (mut keys, mut misses) = (KVVec::new(), KVVec::new());
        for _ in 0..n {
            keys.push((xorshift(&mut rng) | 1) & !(3 << 62))?;
            misses.push(xorshift(&mut rng) & !1)?;
        }

        let t = Cuckoo::<u64, S, W>::default();
        let t = &t;

        let start = local_clock();
        for &k in keys.iter() {
            t.insert(k)?;
        }
        let insert_ns = (local_clock() - start) / n as u64;

        let (hit_ns, found) = bench_lookups(t, &keys, lookups, &mut rng);
        assert_eq!(found, lookups, "cuckoo bench: a member not found");
        let (miss_ns, found) = bench_lookups(t, &misses, lookups, &mut rng);
        assert!(found == 0, "cuckoo bench: a miss found");

        let mut msg = Printbuf::new();
        prt!(&mut msg, "cuckoo bench {table}, {n:>8} keys: insert {insert_ns} ns, hit {} ns, miss {} ns",
             hit_ns / lookups, miss_ns / lookups);

        let mut threads = 1;
        while threads <= cpus {
            let (m, _) = bench_threads(t, &keys, 1000, threads, None)?;
            prt!(&mut msg, "; {threads} readers {m} M/s");
            threads *= 2;
        }
        let readers = cpus.saturating_sub(1).max(1);
        let (hits, w) = bench_threads(t, &keys, 1000, readers, Some(&keys))?;
        let (misses, _) = bench_threads(t, &misses, 1000, readers, Some(&keys))?;
        prt!(&mut msg, "; {readers} readers + churning writer: hits {hits} M/s, misses {misses} M/s, writer {} ops/s",
             HumanReadable(w));

        let start = local_clock();
        for &k in keys.iter() {
            t.remove(&k);
        }
        prt!(&mut msg, "; remove {} ns\n", (local_clock() - start) / n as u64);
        kernel::pr_info!("{}", msg.as_str());
    }
    Ok(())
}

/// The load a table reaches before an insert finds no chain: dense, so only
/// that grows it - the load just before each grow, by the size it grew from.
fn fill<S: CuckooHasher, const W: usize>(table: &str) -> TestRet {
    const TRIALS: usize = 8;
    const MAX_BITS: usize = 20;
    let mut at: [(u64, u64, u64); MAX_BITS + 1] = [(0, u64::MAX, 0); MAX_BITS + 1];
    let mut rng = random_u64() | 1;

    for _ in 0..TRIALS {
        let t = Cuckoo::<u64, S, W>::with_faults(Faults { dense: true, ..Faults::default() });
        let mut nr = 0u64;
        loop {
            let cap = t.capacity();
            let bits = cap.trailing_zeros() as usize;
            if cap != 0 && bits > MAX_BITS {
                break;
            }
            t.insert((xorshift(&mut rng) | 1) & !(3 << 62))?;
            if cap != 0 && t.capacity() != cap {
                let permille = nr * 1000 / cap as u64;
                let a = &mut at[bits];
                (a.0, a.1, a.2) = (a.0 + permille, a.1.min(permille), a.2 + 1);
            }
            nr += 1;
        }
    }

    let mut msg = Printbuf::new();
    prt!(&mut msg, "cuckoo fill {table}, load at no chain, ‰ (mean/min of {TRIALS}):");
    for (bits, &(sum, min, n)) in at.iter().enumerate().filter(|(b, a)| *b >= 8 && a.2 > 0) {
        prt!(&mut msg, " 2^{bits} {}/{min}", sum / n);
    }
    prt!(&mut msg, "\n");
    kernel::pr_info!("{}", msg.as_str());
    Ok(())
}

fn fills() -> TestRet {
    fill::<SipHash13Key, 2>("siphash, 2-way")?;
    fill::<SipHash13Key, 3>("siphash, 3-way")?;
    fill::<GoldenRatio, 2>("golden, 2-way")?;
    fill::<GoldenRatio, 3>("golden, 3-way")
}

pub(super) fn test_cuckoo_bench(_fs: &Fs, _nr: u64) -> TestRet {
    bench::<SipHash13Key, 2>("siphash, 2-way")?;
    bench::<SipHash13Key, 3>("siphash, 3-way")?;
    bench::<GoldenRatio, 2>("golden, 2-way")?;
    bench::<GoldenRatio, 3>("golden, 3-way")
}

#[cfg(test)]
mod tests {
    use super::*;

    // libtest runs each on a thread of its own, which libbcachefs code needs
    // made one it can run on
    fn run(name: &str, f: fn() -> TestRet) {
        unsafe { c::linux_thread_init() };
        let ret = f();
        unsafe { c::linux_thread_exit() };
        ret.unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }

    #[test]
    fn cuckoo() {
        run("cuckoo", models)
    }

    #[test]
    fn cuckoo_fill() {
        run("cuckoo fill", fills)
    }

    #[test]
    fn cuckoo_torture() {
        run("cuckoo torture", tortures)
    }
}
