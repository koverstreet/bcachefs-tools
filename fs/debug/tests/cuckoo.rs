// SPDX-License-Identifier: GPL-2.0

//! util/cuckoo.rs's tests, run by perf_test - so built only with
//! CONFIG_BCACHEFS_TESTS. A lookup that misses is silent: these check every
//! lookup against what it should find. Each runs on sets and on maps of
//! entries (TestItem), by SipHash and by golden ratio, in two ways and three.
//!
//! test_cuckoo: one thread, against a model, through evictions, rehashes and
//! clear() - every kind of table.
//!
//! test_cuckoo_torture: the table as its users will have it, published by a
//! Lock - a mutex, so a writer can be preempted mid-op, which no reader may
//! wait on - read through the lock, written through its guard. Two writers
//! taking turns churn KEYS keys, key 0 and u64::MAX - 1 among them, through
//! every path: eviction chains, rehashes as the table fills and empties,
//! clear(). HOT of them are only ever inserted, so a map's entries for them
//! are replaced under readers all the time. Readers on every other CPU look
//! keys up, half on the hot keys, an eighth of their runs in one RCU section
//! across 64 lookups, so a buffer replaced meanwhile must stay readable and
//! right. Round by round, faults make the rare paths routine (rounds()), each
//! round printing how often it reached them - for a SipHash 2-way and a
//! golden ratio 3-way table, of each kind.
//!
//! The check is exact, not statistical: each key has a sequence count, odd
//! while an op on it is in flight, and the value the op left - an entry's,
//! or a set's presence. A lookup between two loads of the count that see it
//! even and unchanged must find exactly that value, or nothing if that's what
//! it is - the count's Release after the op and the reader's Acquire put the
//! op before the lookup, and the next op's fence after its odd store puts the
//! lookup before anything it does - so a mismatch is a bug, never a race the
//! test can't see. Writers check each result against the same state, and
//! check() and the item count every 10000 ops; an allocation failure is a
//! failure unless injected; at rest, every key is checked as its last op left
//! it.
//!
//! test_cuckoo_bench: for each hash and ways, on sets - the cost of an
//! insert, a hit, a miss and a remove, at sizes from cache-resident to well
//! past it; readers' lookups a second at 1, 2, 4... CPUs; and readers on all
//! but one CPU with a writer removing and putting back members under them -
//! hits and misses apart, as only a miss should see the writer. Members are
//! odd keys, misses even ones.

use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering::{Acquire, Relaxed, Release}};

use super::TestRet;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::locking::Lock;
use crate::new_lock;
use crate::prt;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::cuckoo::{Cuckoo, CuckooItem, CuckooWriter, Entry, Faults};
use crate::util::hash::GoldenRatio;
use crate::util::siphash::SipHash13Key;
use core::hash::BuildHasher;
use crate::util::kernel::{cond_resched, local_clock, random_u64, random_u64_below};
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

fn nr_cpus() -> u32 {
    #[cfg(kernel)]
    return crate::util::kernel::num_online_cpus();
    #[cfg(not(kernel))]
    4
}

/// A map's entry: its key, and the value the op that put it in left.
struct TestEntry {
    key: u64,
    val: AtomicU64,
}

// A map's entry is hashed and compared by its key, and looked up by it
impl core::hash::Hash for TestEntry {
    fn hash<S: core::hash::Hasher>(&self, h: &mut S) {
        self.key.hash(h)
    }
}

impl PartialEq for TestEntry {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key
    }
}

impl Eq for TestEntry {}

impl core::borrow::Borrow<u64> for TestEntry {
    fn borrow(&self) -> &u64 {
        &self.key
    }
}

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
            pool.push(core::array::from_fn(|_| TestEntry { key, val: AtomicU64::new(0) }), GFP_KERNEL)?;
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

fn model<I: TestItem, S: BuildHasher + From<[u64; 2]>, const W: usize>(name: &str) -> TestRet {
    const NR: usize = 512;
    let key = |i: usize| (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let mut keys = KVVec::new();
    for i in 0..NR {
        keys.push(key(i), GFP_KERNEL)?;
    }
    let pool = I::pool(&keys)?;

    let mut t = Cuckoo::<I, S, W>::default();
    let w = t.get_mut();
    // What each key's last op left it, and its ops so far, make()'s @seq: on
    // the heap, as a kernel stack is small
    let mut st = KVVec::new();
    for _ in 0..NR {
        st.push((0u64, 0u64), GFP_KERNEL)?;
    }

    for op in 0..20_000u64 {
        let i = random_u64_below(NR as u64) as usize;
        let (k, (want, ops)) = (key(i), &mut st[i]);
        *ops += 1;
        if random_u64_below(3) != 0 {
            let item = I::make(&pool, i, k, *ops * 2, op | 1 << 63);
            assert_eq!(w.insert(item)?.map_or(0, I::value), *want, "{name}: insert of {k:#x}");
            *want = item.value();
        } else {
            assert_eq!(w.remove(&k).map_or(0, I::value), *want, "{name}: remove of {k:#x}");
            *want = 0;
        }
    }

    for (i, &(want, _)) in st.iter().enumerate() {
        assert_eq!(w.get(&key(i)).map_or(0, I::value), want, "{name}: lookup of {:#x}", key(i));
    }
    assert_eq!(w.check().unwrap_or_else(|e| panic!("{name}: {e}")), st.iter().filter(|s| s.0 != 0).count());

    w.clear();
    assert!(w.get(&key(1)).is_none(), "{name}: cleared, but not empty");
    Ok(())
}

pub(super) fn test_cuckoo(_fs: &Fs, _nr: u64) -> TestRet {
    model::<u64, SipHash13Key, 2>("set, siphash, 2-way")?;
    model::<u64, SipHash13Key, 3>("set, siphash, 3-way")?;
    model::<u64, GoldenRatio, 2>("set, golden, 2-way")?;
    model::<u64, GoldenRatio, 3>("set, golden, 3-way")?;
    model::<Map, SipHash13Key, 2>("map, siphash, 2-way")?;
    model::<Map, SipHash13Key, 3>("map, siphash, 3-way")?;
    model::<Map, GoldenRatio, 2>("map, golden, 2-way")?;
    model::<Map, GoldenRatio, 3>("map, golden, 3-way")
}

// ── The torture test ─────────────────────────────────────────────────────

const KEYS: usize = 4096;
const HOT: usize = 16;

/// A key's state: @seq odd while an op on it is in flight, and @expect what
/// the last op left it - 0, absent.
#[derive(Default)]
struct KeyState {
    seq:    AtomicU64,
    expect: AtomicU64,
}

/// Key 0 and the largest a key can be are among them; the rest are scattered.
fn key(i: usize) -> u64 {
    match i {
        0 => 0,
        1 => u64::MAX - 1,
        _ => (i as u64).wrapping_mul(0x9E3779B97F4A7C15),
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
type Failures = Lock<c::spinlock_t, Option<Failure>, Volatile<u64>>;

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
        // A writer stopped, and off CPU, in every move: what the move count is for
        ("moves",      Faults { move_window: 2000, ..dense() }),
        // Lookups slowed between their slots: moves land between the reads
        ("reads",      Faults { read_window: 500, ..dense() }),
        // Rehashes failing to allocate: inserts fail, the table as it was
        ("failures",   Faults { alloc_fail: 4, ..dense() }),
        ("everything", Faults { move_window: 500, read_window: 200, alloc_fail: 4, ..dense() }),
    ]
}

/// The @nr'th writer op: on a key, bracketed by its @seq - or, now and then,
/// clear(), bracketing every key.
#[allow(clippy::too_many_arguments)]
fn writer_op<I: TestItem, S: BuildHasher + From<[u64; 2]>, const W: usize>(
    w: &CuckooWriter<I, S, W>, items: &mut u64, keys: &[KeyState], pool: &I::Pool,
    fails: &Failures, injecting: bool, rng: &mut u64, nr: u64,
) {
    let r = xorshift(rng);

    if r % 50_000 == 0 {
        keys.iter().for_each(|st| { st.seq.fetch_add(1, Relaxed); });
        fence(Release);
        w.clear();
        *items = 0;
        keys.iter().for_each(|st| {
            st.expect.store(0, Relaxed);
            st.seq.fetch_add(1, Release);
        });
        return;
    }

    // An eighth of the ops on the hot keys, which are only ever inserted - a
    // map's entry replaced under readers, after the first - the rest on any:
    // inserts mostly, then removes mostly, in turn, so the table fills and
    // empties and rehashes
    let hot = r & 7 == 0;
    let i = (r >> 3) as usize % if hot { HOT } else { KEYS };
    let filling = (nr / 25_000) % 2 == 0;
    let insert = hot || ((r >> 32) % 4 != 0) == filling;

    let (st, k) = (&keys[i], key(i));
    let seq = st.seq.load(Relaxed);
    st.seq.store(seq + 1, Relaxed);
    fence(Release);

    let want = st.expect.load(Relaxed);
    let (got, now) = if insert {
        let item = I::make(pool, i, k, seq + 2, val(k, seq + 2));
        match w.insert(item) {
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
        (w.remove(&k).map_or(0, I::value), 0)
    };
    if got != want {
        fail(fails, "insert or remove", k, seq, want, got);
    }

    match (want != 0, now != 0) {
        (false, true) => *items += 1,
        (true, false) => *items -= 1,
        _             => {}
    }
    st.expect.store(now, Relaxed);
    st.seq.store(seq + 2, Release);

    if nr % 10_000 == 0 {
        match w.check() {
            Err(e) => fail(fails, e, 0, 0, 0, 0),
            Ok(n) if n as u64 != *items => fail(fails, "item count", 0, 0, *items, n as u64),
            Ok(_) => {}
        }
    }
}

/// One round: a fresh table with @faults, two writers and the rest of the
/// CPUs reading, for @secs.
fn round<I: TestItem, S: BuildHasher + From<[u64; 2]>, const W: usize>(table: &str, name: &str, faults: Faults, secs: u64)
    -> TestRet
{
    const WRITERS: u32 = 2;
    let readers = nr_cpus().saturating_sub(WRITERS).clamp(2, 30);
    let injecting = faults.alloc_fail != 0;

    let (mut keys, mut key_vals) = (KVVec::new(), KVVec::new());
    for i in 0..KEYS {
        keys.push(KeyState::default(), GFP_KERNEL)?;
        key_vals.push(key(i), GFP_KERNEL)?;
    }
    let keys = &keys[..];
    let pool = I::pool(&key_vals)?;
    let pool = &pool;

    // The item count, the writers' to keep, with the table
    pin_init::stack_pin_init!(let lock: Lock<c::mutex, u64, Cuckoo<I, S, W>> =
                              new_lock!(0, Cuckoo::with_faults(faults)));
    pin_init::stack_pin_init!(let fails: Failures = new_lock!(None, pin_init::Zeroable::zeroed()));
    let (lock, fails): (&Lock<c::mutex, u64, Cuckoo<I, S, W>>, &Failures) = (&lock, &fails);
    let t: &Cuckoo<I, S, W> = lock;

    let writers_left = AtomicU32::new(WRITERS);
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

        for i in 0..WRITERS {
            let (writers_left, ops) = (&writers_left, &ops);
            s.spawn(format_args!("cuckoo torture writer[{i}]"), move || {
                let mut rng = random_u64() | 1;
                let mut nr = 0;
                while fails.get() == 0 && (nr % 256 != 0 || local_clock() < end) {
                    let mut g = lock.lock();
                    let (items, w) = g.parts();
                    writer_op(w, items, keys, pool, fails, injecting, &mut rng, nr);
                    drop(g);
                    nr += 1;
                    cond_resched();
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
    let mut g = lock.lock();
    let (items, w) = g.parts();
    for (i, st) in keys.iter().enumerate() {
        let k = key(i);
        assert_eq!(w.get(&k).map_or(0, I::value), st.expect.load(Relaxed),
                   "cuckoo torture, {table}, round {name}: {k:#x} at rest");
    }
    assert_eq!(w.check().unwrap_or_else(|e| panic!("{e}")) as u64, *items);

    let checked = checked.into_inner();
    assert!(checked > 0, "cuckoo torture, {table}, round {name}: no lookup ever checked");

    let seen = &t.faults().seen;
    let n = |c: &AtomicU64| HumanReadable(c.load(Relaxed));
    let mut msg = Printbuf::new();
    prt!(&mut msg, "cuckoo torture {table}, {name:<10} {ms}ms, {readers} readers: {} lookups checked, \
                    {} raced an op; {} writer ops, {} moves; retries {} mirror fallbacks {} rehashes {} grown {}\n",
         HumanReadable(checked), HumanReadable(raced.into_inner()), HumanReadable(ops.into_inner()),
         HumanReadable(t.moved()), n(&seen.retry), n(&seen.raced), n(&seen.rehash), n(&seen.grown));
    kernel::pr_info!("{}", msg.as_str());
    Ok(())
}

fn torture<I: TestItem, S: BuildHasher + From<[u64; 2]>, const W: usize>(table: &str) -> TestRet {
    for (name, faults) in rounds() {
        round::<I, S, W>(table, name, faults, 5)?;
    }
    Ok(())
}

pub(super) fn test_cuckoo_torture(_fs: &Fs, _nr: u64) -> TestRet {
    torture::<Map, SipHash13Key, 2>("map, siphash, 2-way")?;
    torture::<Map, GoldenRatio, 3>("map, golden, 3-way")?;
    torture::<u64, SipHash13Key, 2>("set, siphash, 2-way")?;
    torture::<u64, GoldenRatio, 3>("set, golden, 3-way")
}

// ── The benchmark ────────────────────────────────────────────────────────

fn pick(keys: &[u64], rng: &mut u64) -> u64 {
    keys[(xorshift(rng) % keys.len() as u64) as usize]
}

/// @nr lookups of @keys at random, timed, and how many were found - so they
/// aren't optimized out.
fn bench_lookups<S: BuildHasher + From<[u64; 2]>, const W: usize>(t: &Cuckoo<u64, S, W>, keys: &[u64], nr: u64, rng: &mut u64)
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
fn bench_threads<S: BuildHasher + From<[u64; 2]>, const W: usize>(
    lock: &Lock<c::mutex, (), Cuckoo<u64, S, W>>, keys: &[u64], ms: u64, threads: u32, churn: Option<&[u64]>,
) -> Result<(u64, u64), BchError> {
    let t: &Cuckoo<u64, S, W> = lock;
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
                    let mut g = lock.lock();
                    let w = g.published();
                    w.remove(&k);
                    w.insert(k)?;
                    drop(g);
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

fn bench<S: BuildHasher + From<[u64; 2]>, const W: usize>(table: &str) -> TestRet {
    let cpus = nr_cpus();

    for shift in [10u32, 14, 18, 22] {
        let n = 1usize << shift;
        let lookups = (n as u64).max(1 << 22);

        let mut rng = random_u64() | 1;
        let (mut keys, mut misses) = (KVVec::new(), KVVec::new());
        for _ in 0..n {
            keys.push((xorshift(&mut rng) | 1) & !(1 << 63), GFP_KERNEL)?;
            misses.push(xorshift(&mut rng) & !1, GFP_KERNEL)?;
        }

        pin_init::stack_pin_init!(let lock: Lock<c::mutex, (), Cuckoo<u64, S, W>> =
                                  new_lock!((), Cuckoo::default()));
        let lock: &Lock<c::mutex, (), Cuckoo<u64, S, W>> = &lock;

        let start = local_clock();
        {
            let mut g = lock.lock();
            let w = g.published();
            for &k in keys.iter() {
                w.insert(k)?;
            }
        }
        let insert_ns = (local_clock() - start) / n as u64;

        let (hit_ns, found) = bench_lookups(lock, &keys, lookups, &mut rng);
        assert_eq!(found, lookups, "cuckoo bench: a member not found");
        let (miss_ns, found) = bench_lookups(lock, &misses, lookups, &mut rng);
        assert!(found == 0, "cuckoo bench: a miss found");

        let mut msg = Printbuf::new();
        prt!(&mut msg, "cuckoo bench {table}, {n:>8} keys: insert {insert_ns} ns, hit {} ns, miss {} ns",
             hit_ns / lookups, miss_ns / lookups);

        let mut threads = 1;
        while threads <= cpus {
            let (m, _) = bench_threads(lock, &keys, 1000, threads, None)?;
            prt!(&mut msg, "; {threads} readers {m} M/s");
            threads *= 2;
        }
        let readers = cpus.saturating_sub(1).max(1);
        let (hits, w) = bench_threads(lock, &keys, 1000, readers, Some(&keys))?;
        let (misses, _) = bench_threads(lock, &misses, 1000, readers, Some(&keys))?;
        prt!(&mut msg, "; {readers} readers + churning writer: hits {hits} M/s, misses {misses} M/s, writer {} ops/s",
             HumanReadable(w));

        let start = local_clock();
        {
            let mut g = lock.lock();
            let w = g.published();
            for &k in keys.iter() {
                w.remove(&k);
            }
        }
        prt!(&mut msg, "; remove {} ns\n", (local_clock() - start) / n as u64);
        kernel::pr_info!("{}", msg.as_str());
    }
    Ok(())
}

pub(super) fn test_cuckoo_bench(_fs: &Fs, _nr: u64) -> TestRet {
    bench::<SipHash13Key, 2>("siphash, 2-way")?;
    bench::<SipHash13Key, 3>("siphash, 3-way")?;
    bench::<GoldenRatio, 2>("golden, 2-way")?;
    bench::<GoldenRatio, 3>("golden, 3-way")
}
