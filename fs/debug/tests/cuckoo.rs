// SPDX-License-Identifier: GPL-2.0

//! util/cuckoo.rs's tests, run by perf_test - so built only with
//! CONFIG_BCACHEFS_TESTS. A lookup that misses is silent: these check every
//! lookup against what it should find, on sets and on maps of entries
//! (TestItem), by SipHash and by golden ratio, in two ways and three.
//!
//! test_cuckoo: one thread, against a model, through evictions, rehashes and
//! clear() - every kind of table.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::TestRet;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::cuckoo::{Cuckoo, CuckooEntry, CuckooHash, CuckooItem, Entry, GoldenRatio, SipHash};
use crate::util::kernel::random_u64_below;

/// A map's entry: its key, and the value the op that put it in left.
struct TestEntry {
    key: u64,
    val: AtomicU64,
}

impl CuckooEntry for TestEntry {
    fn key(&self) -> u64 {
        self.key
    }
}

/// What the tests put in a table: an item for a key's op, and what an item
/// found says - 0 for absent, else its value.
trait TestItem: CuckooItem + Send + 'static {
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

fn model<I: TestItem, H: CuckooHash, const W: usize>(name: &str) -> TestRet {
    const NR: usize = 512;
    let key = |i: usize| (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let mut keys = KVVec::new();
    for i in 0..NR {
        keys.push(key(i), GFP_KERNEL)?;
    }
    let pool = I::pool(&keys)?;

    let mut t = Cuckoo::<I, H, W>::default();
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
            assert_eq!(w.remove(k).map_or(0, I::value), *want, "{name}: remove of {k:#x}");
            *want = 0;
        }
    }

    for (i, &(want, _)) in st.iter().enumerate() {
        assert_eq!(w.get(key(i)).map_or(0, I::value), want, "{name}: lookup of {:#x}", key(i));
    }

    w.clear();
    assert!(w.get(key(1)).is_none(), "{name}: cleared, but not empty");
    Ok(())
}

pub(super) fn test_cuckoo(_fs: &Fs, _nr: u64) -> TestRet {
    model::<u64, SipHash, 2>("set, siphash, 2-way")?;
    model::<u64, SipHash, 3>("set, siphash, 3-way")?;
    model::<u64, GoldenRatio, 2>("set, golden, 2-way")?;
    model::<u64, GoldenRatio, 3>("set, golden, 3-way")?;
    model::<Map, SipHash, 2>("map, siphash, 2-way")?;
    model::<Map, SipHash, 3>("map, siphash, 3-way")?;
    model::<Map, GoldenRatio, 2>("map, golden, 2-way")?;
    model::<Map, GoldenRatio, 3>("map, golden, 3-way")
}
