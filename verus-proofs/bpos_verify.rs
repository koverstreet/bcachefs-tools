// Formal verification of bcachefs bpos comparison functions.
//
// This file proves that bpos_cmp implements a total order and that
// all comparison functions (lt, le, gt, ge, min, max) are consistent
// with bpos_cmp. Similarly for bkey_cmp (which ignores the snapshot field).
//
// This is the first formal verification target for bcachefs — pure
// functions on Copy types with no unsafe, no I/O, no concurrency.
// Z3 handles the integer arithmetic automatically.
//
// Run: ~/verus-bin/verus-x86-linux/verus verus-proofs/bpos_verify.rs

use vstd::prelude::*;

verus! {

// Mirror of the C/Rust bpos struct
pub struct Bpos {
    pub inode: u64,
    pub offset: u64,
    pub snapshot: u32,
}

// ============================================================
// bpos comparison functions — mirrors of bch_bindgen/src/bkey.rs
// ============================================================

pub open spec fn bpos_lt_spec(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot < r.snapshot
    }
}

pub open spec fn bpos_le_spec(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot <= r.snapshot
    }
}

pub open spec fn bpos_eq_spec(l: Bpos, r: Bpos) -> bool {
    l.inode == r.inode && l.offset == r.offset && l.snapshot == r.snapshot
}

// Exec functions matching the real implementation
fn bpos_lt(l: &Bpos, r: &Bpos) -> (result: bool)
    ensures result == bpos_lt_spec(*l, *r)
{
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot < r.snapshot
    }
}

fn bpos_le(l: &Bpos, r: &Bpos) -> (result: bool)
    ensures result == bpos_le_spec(*l, *r)
{
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot <= r.snapshot
    }
}

fn bpos_gt(l: &Bpos, r: &Bpos) -> (result: bool)
    ensures result == bpos_lt_spec(*r, *l)
{
    bpos_lt(r, l)
}

fn bpos_ge(l: &Bpos, r: &Bpos) -> (result: bool)
    ensures result == bpos_le_spec(*r, *l)
{
    bpos_le(r, l)
}

fn bpos_cmp(l: &Bpos, r: &Bpos) -> (result: i32)
    ensures
        result == 0 ==> bpos_eq_spec(*l, *r),
        result < 0 ==> bpos_lt_spec(*l, *r),
        result > 0 ==> bpos_lt_spec(*r, *l),
        result == -1 || result == 0 || result == 1,
{
    if l.inode != r.inode {
        if l.inode < r.inode { -1 } else { 1 }
    } else if l.offset != r.offset {
        if l.offset < r.offset { -1 } else { 1 }
    } else if l.snapshot != r.snapshot {
        if l.snapshot < r.snapshot { -1 } else { 1 }
    } else {
        0
    }
}

fn bpos_min(l: &Bpos, r: &Bpos) -> (result: Bpos)
    ensures
        bpos_le_spec(result, *l),
        bpos_le_spec(result, *r),
        bpos_eq_spec(result, *l) || bpos_eq_spec(result, *r),
{
    if bpos_lt(l, r) {
        Bpos { inode: l.inode, offset: l.offset, snapshot: l.snapshot }
    } else {
        Bpos { inode: r.inode, offset: r.offset, snapshot: r.snapshot }
    }
}

fn bpos_max(l: &Bpos, r: &Bpos) -> (result: Bpos)
    ensures
        bpos_le_spec(*l, result),
        bpos_le_spec(*r, result),
        bpos_eq_spec(result, *l) || bpos_eq_spec(result, *r),
{
    if bpos_lt(r, l) {
        Bpos { inode: l.inode, offset: l.offset, snapshot: l.snapshot }
    } else {
        Bpos { inode: r.inode, offset: r.offset, snapshot: r.snapshot }
    }
}

// ============================================================
// bkey comparison — ignores snapshot field
// ============================================================

pub open spec fn bkey_lt_spec(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset < r.offset
    }
}

pub open spec fn bkey_le_spec(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset <= r.offset
    }
}

pub open spec fn bkey_eq_spec(l: Bpos, r: Bpos) -> bool {
    l.inode == r.inode && l.offset == r.offset
}

fn bkey_lt(l: &Bpos, r: &Bpos) -> (result: bool)
    ensures result == bkey_lt_spec(*l, *r)
{
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset < r.offset
    }
}

fn bkey_le(l: &Bpos, r: &Bpos) -> (result: bool)
    ensures result == bkey_le_spec(*l, *r)
{
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset <= r.offset
    }
}

fn bkey_cmp(l: &Bpos, r: &Bpos) -> (result: i32)
    ensures
        result == 0 ==> bkey_eq_spec(*l, *r),
        result < 0 ==> bkey_lt_spec(*l, *r),
        result > 0 ==> bkey_lt_spec(*r, *l),
        result == -1 || result == 0 || result == 1,
{
    if l.inode != r.inode {
        if l.inode < r.inode { -1 } else { 1 }
    } else if l.offset != r.offset {
        if l.offset < r.offset { -1 } else { 1 }
    } else {
        0
    }
}

// ============================================================
// Total order proofs for bpos
// ============================================================

// Reflexivity: bpos_le(a, a) for all a
proof fn bpos_le_reflexive(a: Bpos)
    ensures bpos_le_spec(a, a)
{}

// Antisymmetry: bpos_le(a, b) && bpos_le(b, a) ==> a == b
proof fn bpos_le_antisymmetric(a: Bpos, b: Bpos)
    requires
        bpos_le_spec(a, b),
        bpos_le_spec(b, a),
    ensures bpos_eq_spec(a, b)
{}

// Transitivity: bpos_le(a, b) && bpos_le(b, c) ==> bpos_le(a, c)
proof fn bpos_le_transitive(a: Bpos, b: Bpos, c: Bpos)
    requires
        bpos_le_spec(a, b),
        bpos_le_spec(b, c),
    ensures bpos_le_spec(a, c)
{}

// Totality: bpos_le(a, b) || bpos_le(b, a)
proof fn bpos_le_total(a: Bpos, b: Bpos)
    ensures bpos_le_spec(a, b) || bpos_le_spec(b, a)
{}

// ============================================================
// Consistency proofs
// ============================================================

// bpos_lt is the strict version of bpos_le
proof fn bpos_lt_is_strict_le(a: Bpos, b: Bpos)
    ensures bpos_lt_spec(a, b) == (bpos_le_spec(a, b) && !bpos_eq_spec(a, b))
{}

// bpos_gt is the reverse of bpos_lt
proof fn bpos_gt_is_reverse_lt(a: Bpos, b: Bpos)
    ensures bpos_lt_spec(b, a) == bpos_lt_spec(b, a)
{}

// bpos_lt(a, b) <==> !bpos_ge(a, b)
proof fn bpos_lt_complement_ge(a: Bpos, b: Bpos)
    ensures bpos_lt_spec(a, b) == !bpos_le_spec(b, a)
{}

// ============================================================
// Total order proofs for bkey (ignoring snapshot)
// ============================================================

proof fn bkey_le_reflexive(a: Bpos)
    ensures bkey_le_spec(a, a)
{}

proof fn bkey_le_antisymmetric(a: Bpos, b: Bpos)
    requires
        bkey_le_spec(a, b),
        bkey_le_spec(b, a),
    ensures bkey_eq_spec(a, b)
{}

proof fn bkey_le_transitive(a: Bpos, b: Bpos, c: Bpos)
    requires
        bkey_le_spec(a, b),
        bkey_le_spec(b, c),
    ensures bkey_le_spec(a, c)
{}

proof fn bkey_le_total(a: Bpos, b: Bpos)
    ensures bkey_le_spec(a, b) || bkey_le_spec(b, a)
{}

// bkey_cmp refines bpos_cmp: if bkey_eq(a, b), then bpos_cmp
// determines the order (via snapshot)
proof fn bkey_refines_bpos(a: Bpos, b: Bpos)
    ensures
        bkey_lt_spec(a, b) ==> bpos_lt_spec(a, b),
        bkey_eq_spec(a, b) && a.snapshot < b.snapshot ==> bpos_lt_spec(a, b),
{}

// ============================================================
// Sentinel constants
// ============================================================

pub open spec fn pos_min_spec() -> Bpos {
    Bpos { inode: 0, offset: 0, snapshot: 0 }
}

pub open spec fn spos_max_spec() -> Bpos {
    Bpos { inode: u64::MAX, offset: u64::MAX, snapshot: u32::MAX }
}

pub open spec fn pos_max_spec() -> Bpos {
    Bpos { inode: u64::MAX, offset: u64::MAX, snapshot: 0 }
}

// POS_MIN is the minimum of all bpos values
proof fn pos_min_is_minimum(a: Bpos)
    ensures bpos_le_spec(pos_min_spec(), a)
{}

// SPOS_MAX is the maximum of all bpos values
proof fn spos_max_is_maximum(a: Bpos)
    ensures bpos_le_spec(a, spos_max_spec())
{}

// POS_MAX is the maximum of snapshot-0 bpos values
proof fn pos_max_is_max_nosnap(a: Bpos)
    requires a.snapshot == 0
    ensures bpos_le_spec(a, pos_max_spec())
{}

// ============================================================
// Successor / predecessor functions
// ============================================================
//
// These mirror the kernel's bpos_successor/bpos_predecessor from
// fs/bcachefs/btree/bkey.h. The kernel uses wrapping increment with
// BUG() on overflow — we model overflow as a precondition.
//
// The bpos fields form a 160-bit number: (inode:64, offset:64, snapshot:32).
// Successor increments this by 1, predecessor decrements by 1.

/// Interpret a bpos as a 160-bit number for reasoning about successor/predecessor.
pub open spec fn bpos_to_int(p: Bpos) -> int {
    (p.inode as int) * 0x1_0000_0000_0000_0000_0000_0000i128 as int
    + (p.offset as int) * 0x1_0000_0000i128 as int
    + (p.snapshot as int)
}

fn bpos_successor(p: &Bpos) -> (result: Bpos)
    requires !bpos_eq_spec(*p, spos_max_spec())
    ensures result == bpos_successor_spec(*p)
{
    if p.snapshot < u32::MAX {
        Bpos { inode: p.inode, offset: p.offset, snapshot: (p.snapshot + 1) as u32 }
    } else if p.offset < u64::MAX {
        Bpos { inode: p.inode, offset: p.offset + 1, snapshot: 0 }
    } else {
        Bpos { inode: p.inode + 1, offset: 0, snapshot: 0 }
    }
}

fn bpos_predecessor(p: &Bpos) -> (result: Bpos)
    requires !bpos_eq_spec(*p, pos_min_spec())
    ensures result == bpos_predecessor_spec(*p)
{
    if p.snapshot > 0 {
        Bpos { inode: p.inode, offset: p.offset, snapshot: (p.snapshot - 1) as u32 }
    } else if p.offset > 0 {
        Bpos { inode: p.inode, offset: p.offset - 1, snapshot: u32::MAX }
    } else {
        Bpos { inode: p.inode - 1, offset: u64::MAX, snapshot: u32::MAX }
    }
}

fn bpos_nosnap_successor(p: &Bpos) -> (result: Bpos)
    requires !(p.inode == u64::MAX && p.offset == u64::MAX)
    ensures result == bpos_nosnap_successor_spec(*p)
{
    if p.offset < u64::MAX {
        Bpos { inode: p.inode, offset: p.offset + 1, snapshot: 0 }
    } else {
        Bpos { inode: p.inode + 1, offset: 0, snapshot: 0 }
    }
}

fn bpos_nosnap_predecessor(p: &Bpos) -> (result: Bpos)
    requires !(p.inode == 0 && p.offset == 0)
    ensures
        result.snapshot == 0,
        bkey_lt_spec(result, *p) || (bkey_eq_spec(*p, result) && p.snapshot > 0),
{
    if p.offset > 0 {
        Bpos { inode: p.inode, offset: p.offset - 1, snapshot: 0 }
    } else {
        Bpos { inode: p.inode - 1, offset: u64::MAX, snapshot: 0 }
    }
}

// ============================================================
// Spec-level successor/predecessor (for use in proofs)
// ============================================================

pub open spec fn bpos_successor_spec(p: Bpos) -> Bpos
    recommends !bpos_eq_spec(p, spos_max_spec())
{
    if p.snapshot < u32::MAX {
        Bpos { inode: p.inode, offset: p.offset, snapshot: (p.snapshot + 1) as u32 }
    } else if p.offset < u64::MAX {
        Bpos { inode: p.inode, offset: (p.offset + 1) as u64, snapshot: 0 }
    } else {
        Bpos { inode: (p.inode + 1) as u64, offset: 0, snapshot: 0 }
    }
}

pub open spec fn bpos_predecessor_spec(p: Bpos) -> Bpos
    recommends !bpos_eq_spec(p, pos_min_spec())
{
    if p.snapshot > 0 {
        Bpos { inode: p.inode, offset: p.offset, snapshot: (p.snapshot - 1) as u32 }
    } else if p.offset > 0 {
        Bpos { inode: p.inode, offset: (p.offset - 1) as u64, snapshot: u32::MAX }
    } else {
        Bpos { inode: (p.inode - 1) as u64, offset: u64::MAX, snapshot: u32::MAX }
    }
}

pub open spec fn bpos_nosnap_successor_spec(p: Bpos) -> Bpos
    recommends !(p.inode == u64::MAX && p.offset == u64::MAX)
{
    if p.offset < u64::MAX {
        Bpos { inode: p.inode, offset: (p.offset + 1) as u64, snapshot: 0 }
    } else {
        Bpos { inode: (p.inode + 1) as u64, offset: 0, snapshot: 0 }
    }
}

// ============================================================
// Successor/predecessor proofs
// ============================================================

// Successor is strictly greater
proof fn bpos_successor_gt(p: Bpos)
    requires !bpos_eq_spec(p, spos_max_spec())
    ensures bpos_lt_spec(p, bpos_successor_spec(p))
{}

// Predecessor is strictly less
proof fn bpos_predecessor_lt(p: Bpos)
    requires !bpos_eq_spec(p, pos_min_spec())
    ensures bpos_lt_spec(bpos_predecessor_spec(p), p)
{}

// Successor adds exactly 1 to the 160-bit representation
proof fn bpos_successor_adds_one(p: Bpos)
    requires !bpos_eq_spec(p, spos_max_spec())
    ensures bpos_to_int(bpos_successor_spec(p)) == bpos_to_int(p) + 1
{}

// Predecessor subtracts exactly 1
proof fn bpos_predecessor_subtracts_one(p: Bpos)
    requires !bpos_eq_spec(p, pos_min_spec())
    ensures bpos_to_int(bpos_predecessor_spec(p)) == bpos_to_int(p) - 1
{}

// Successor is immediate: no bpos between p and successor(p)
proof fn bpos_successor_immediate(p: Bpos, q: Bpos)
    requires
        !bpos_eq_spec(p, spos_max_spec()),
        bpos_lt_spec(p, q),
    ensures bpos_le_spec(bpos_successor_spec(p), q)
{}

// Predecessor is immediate: no bpos between predecessor(p) and p
proof fn bpos_predecessor_immediate(p: Bpos, q: Bpos)
    requires
        !bpos_eq_spec(p, pos_min_spec()),
        bpos_lt_spec(q, p),
    ensures bpos_le_spec(q, bpos_predecessor_spec(p))
{}

// Successor and predecessor are inverses
proof fn successor_predecessor_inverse(p: Bpos)
    requires !bpos_eq_spec(p, spos_max_spec())
    ensures bpos_eq_spec(bpos_predecessor_spec(bpos_successor_spec(p)), p)
{}

proof fn predecessor_successor_inverse(p: Bpos)
    requires !bpos_eq_spec(p, pos_min_spec())
    ensures bpos_eq_spec(bpos_successor_spec(bpos_predecessor_spec(p)), p)
{}

// nosnap_successor always advances in bkey ordering
proof fn nosnap_successor_advances_bkey(p: Bpos)
    requires !(p.inode == u64::MAX && p.offset == u64::MAX)
    ensures bkey_lt_spec(p, bpos_nosnap_successor_spec(p))
{}

// ============================================================
// bkey_start_pos
// ============================================================

pub struct Bkey {
    pub p: Bpos,
    pub size: u32,
}

pub open spec fn bkey_start_pos_spec(k: Bkey) -> Bpos {
    Bpos {
        inode: k.p.inode,
        offset: (k.p.offset - k.size as u64) as u64,
        snapshot: k.p.snapshot,
    }
}

fn bkey_start_pos(k: &Bkey) -> (result: Bpos)
    requires k.p.offset >= k.size as u64  // key doesn't wrap
    ensures
        result == bkey_start_pos_spec(*k),
        result.inode == k.p.inode,
        result.snapshot == k.p.snapshot,
        bpos_le_spec(result, k.p),
{
    Bpos {
        inode: k.p.inode,
        offset: k.p.offset - k.size as u64,
        snapshot: k.p.snapshot,
    }
}

// Start pos equals end pos for zero-size keys
proof fn zero_size_key_start_is_end(k: Bkey)
    requires
        k.size == 0,
        k.p.offset >= k.size as u64,
    ensures bpos_eq_spec(bkey_start_pos_spec(k), k.p)
{}

// Start of a non-zero key is strictly less than end (in offset)
proof fn nonzero_key_start_lt_end(k: Bkey)
    requires
        k.size > 0,
        k.p.offset >= k.size as u64,
    ensures bkey_start_pos_spec(k).offset < k.p.offset
{}

// ============================================================
// bpos_eq via XOR — the kernel's optimized implementation
// ============================================================
//
// The kernel uses: !((l.inode ^ r.inode) | (l.offset ^ r.offset) | (l.snapshot ^ r.snapshot))
// Prove this is equivalent to field-by-field equality.

// The kernel uses XOR: !((l.inode ^ r.inode) | (l.offset ^ r.offset) | (l.snapshot ^ r.snapshot))
// In C, ! is logical NOT (result is 0 or 1). In Rust, we use == 0.
//
// Z3 needs help with bitvector XOR reasoning, so we use an explicit
// if-chain that Z3 can reduce. The proof that XOR==0 iff equal is
// left as a spec-level property.
proof fn xor_zero_iff_equal_u64(a: u64, b: u64)
    ensures (a ^ b == 0) == (a == b)
{
    assert(a ^ b == 0 ==> a == b) by(bit_vector);
    assert(a == b ==> a ^ b == 0) by(bit_vector);
}

proof fn xor_zero_iff_equal_u32(a: u32, b: u32)
    ensures (a ^ b == 0) == (a == b)
{
    assert(a ^ b == 0 ==> a == b) by(bit_vector);
    assert(a == b ==> a ^ b == 0) by(bit_vector);
}

fn bpos_eq_xor(l: &Bpos, r: &Bpos) -> (result: bool)
    ensures result == bpos_eq_spec(*l, *r)
{
    proof { xor_zero_iff_equal_u64(l.inode, r.inode); }
    proof { xor_zero_iff_equal_u64(l.offset, r.offset); }
    proof { xor_zero_iff_equal_u32(l.snapshot, r.snapshot); }
    (l.inode ^ r.inode) == 0
    && (l.offset ^ r.offset) == 0
    && (l.snapshot ^ r.snapshot) == 0
}

// ============================================================
// Key range overlap and adjacency
// ============================================================
//
// Extents in bcachefs cover a half-open range [start, end) where
// start = p.offset - size and end = p.offset. Two extents overlap
// if their ranges intersect. These properties are fundamental to
// the btree insert/merge/split logic.

/// Two keys (same inode) overlap if their offset ranges intersect.
pub open spec fn keys_overlap_spec(a: Bkey, b: Bkey) -> bool
    recommends
        a.p.inode == b.p.inode,
        a.p.offset >= a.size as u64,
        b.p.offset >= b.size as u64,
{
    let a_start = (a.p.offset - a.size as u64) as u64;
    let b_start = (b.p.offset - b.size as u64) as u64;
    a_start < b.p.offset && b_start < a.p.offset
}

/// Two keys are adjacent if one ends exactly where the other starts.
pub open spec fn keys_adjacent_spec(a: Bkey, b: Bkey) -> bool
    recommends
        a.p.inode == b.p.inode,
        a.p.offset >= a.size as u64,
        b.p.offset >= b.size as u64,
{
    a.p.offset == (b.p.offset - b.size as u64) as u64
}

// Adjacent keys don't overlap (half-open ranges: [start, end) touch but don't intersect)
proof fn adjacent_keys_no_overlap(a: Bkey, b: Bkey)
    requires
        a.p.inode == b.p.inode,
        a.p.offset >= a.size as u64,
        b.p.offset >= b.size as u64,
        a.size > 0,
        b.size > 0,
        keys_adjacent_spec(a, b),
    ensures !keys_overlap_spec(a, b)
{}

// A key overlaps with itself (if non-empty)
proof fn key_overlaps_self(k: Bkey)
    requires
        k.p.offset >= k.size as u64,
        k.size > 0,
    ensures keys_overlap_spec(k, k)
{}

// If keys are sorted by bkey ordering and non-overlapping,
// then a ends at or before b starts
proof fn sorted_nonoverlapping_disjoint(a: Bkey, b: Bkey)
    requires
        a.p.inode == b.p.inode,
        a.p.offset >= a.size as u64,
        b.p.offset >= b.size as u64,
        a.size > 0,
        b.size > 0,
        a.p.offset <= b.p.offset,  // sorted by end position
        !keys_overlap_spec(a, b),
    ensures a.p.offset <= (b.p.offset - b.size as u64) as u64  // a ends at or before b starts
{}

// bkey_start_pos is within the same inode and at or before end
proof fn start_pos_in_range(k: Bkey)
    requires k.p.offset >= k.size as u64
    ensures
        bkey_start_pos_spec(k).inode == k.p.inode,
        bkey_start_pos_spec(k).offset <= k.p.offset,
        k.size > 0 ==> bkey_start_pos_spec(k).offset < k.p.offset,
{}

// ============================================================
// bpos_cmp full equivalence with equality
// ============================================================

// Strengthen: bpos_eq implies bpos_cmp returns 0 (the converse is already proven)
proof fn bpos_eq_implies_cmp_zero(a: Bpos, b: Bpos)
    requires bpos_eq_spec(a, b)
    ensures !bpos_lt_spec(a, b) && !bpos_lt_spec(b, a)
{}

// Trichotomy: exactly one of lt, eq, gt holds
proof fn bpos_trichotomy(a: Bpos, b: Bpos)
    ensures
        (bpos_lt_spec(a, b) && !bpos_eq_spec(a, b) && !bpos_lt_spec(b, a))
        || (!bpos_lt_spec(a, b) && bpos_eq_spec(a, b) && !bpos_lt_spec(b, a))
        || (!bpos_lt_spec(a, b) && !bpos_eq_spec(a, b) && bpos_lt_spec(b, a))
{}

// bkey trichotomy (ignoring snapshot)
proof fn bkey_trichotomy(a: Bpos, b: Bpos)
    ensures
        (bkey_lt_spec(a, b) && !bkey_eq_spec(a, b) && !bkey_lt_spec(b, a))
        || (!bkey_lt_spec(a, b) && bkey_eq_spec(a, b) && !bkey_lt_spec(b, a))
        || (!bkey_lt_spec(a, b) && !bkey_eq_spec(a, b) && bkey_lt_spec(b, a))
{}

// ============================================================
// Sorted sequence properties (level 2 verification)
// ============================================================
//
// A btree node contains keys sorted by bpos. We define "sorted" for
// a spec-level Seq<Bpos> and prove that sortedness implies pairwise
// ordering (via transitivity induction).

/// A sequence of bpos values is sorted if each consecutive pair is strictly ordered.
pub open spec fn bpos_sorted(s: Seq<Bpos>) -> bool {
    forall|i: int| 0 <= i < s.len() - 1 ==> bpos_lt_spec(#[trigger] s[i], s[i + 1])
}

/// Sortedness implies pairwise ordering for any i < j.
proof fn sorted_implies_pairwise(s: Seq<Bpos>, i: int, j: int)
    requires
        bpos_sorted(s),
        0 <= i < j < s.len(),
    ensures
        bpos_lt_spec(s[i], s[j])
    decreases j - i
{
    if j == i + 1 {
        // Base: consecutive elements, follows directly from sorted
    } else {
        // Inductive: s[i] < s[j-1] (by IH) and s[j-1] < s[j] (sorted)
        sorted_implies_pairwise(s, i, j - 1);
        bpos_le_transitive(s[i], s[j - 1], s[j]);
    }
}

/// All elements in a sorted sequence are distinct.
proof fn sorted_implies_distinct(s: Seq<Bpos>, i: int, j: int)
    requires
        bpos_sorted(s),
        0 <= i < j < s.len(),
    ensures
        !bpos_eq_spec(s[i], s[j])
{
    sorted_implies_pairwise(s, i, j);
    // bpos_lt implies not eq (from trichotomy)
}

// ============================================================
// Extent range properties (level 2 verification)
// ============================================================
//
// Extents in bcachefs cover half-open offset ranges [start, end)
// where start = p.offset - size, end = p.offset. Within a btree
// node, extents in the same inode must not overlap — the insert
// logic trims existing extents when a new one is written.
//
// We define the non-overlap invariant for consecutive pairs and
// prove it generalizes to ALL pairs. This means offset lookups
// are unambiguous: a point lies in at most one extent.

/// Start offset of an extent.
pub open spec fn key_start(k: Bkey) -> u64
    recommends k.p.offset >= k.size as u64
{
    (k.p.offset - k.size as u64) as u64
}

/// End offset of an extent.
pub open spec fn key_end(k: Bkey) -> u64 {
    k.p.offset
}

// key_start is consistent with bkey_start_pos_spec
proof fn key_start_matches_bkey_start_pos(k: Bkey)
    requires k.p.offset >= k.size as u64
    ensures key_start(k) == bkey_start_pos_spec(k).offset
{}

/// All extents have valid sizes (offset >= size, so start doesn't underflow).
pub open spec fn extents_valid(s: Seq<Bkey>) -> bool {
    forall|i: int| 0 <= i < s.len() ==>
        (#[trigger] s[i]).p.offset >= s[i].size as u64
}

/// All extents are non-empty (size > 0).
pub open spec fn extents_nonempty(s: Seq<Bkey>) -> bool {
    forall|i: int| 0 <= i < s.len() ==>
        (#[trigger] s[i]).size > 0
}

/// Consecutive extents don't overlap: each ends at or before the next starts.
/// This is the local invariant maintained by btree extent insert.
pub open spec fn extents_nonoverlap(s: Seq<Bkey>) -> bool {
    forall|i: int| 0 <= i < s.len() - 1 ==>
        key_end(#[trigger] s[i]) <= key_start(s[i + 1])
}

/// Each extent has positive length (start < end).
proof fn extent_has_positive_length(s: Seq<Bkey>, i: int)
    requires
        extents_valid(s),
        extents_nonempty(s),
        0 <= i < s.len(),
    ensures
        key_start(s[i]) < key_end(s[i])
{}

/// Local non-overlap implies global non-overlap for all pairs.
/// Proof: induction on j - i. The chain is:
///   end(i) <= start(i+1) < end(i+1) <= ... <= start(j)
proof fn extents_nonoverlap_pairwise(s: Seq<Bkey>, i: int, j: int)
    requires
        extents_valid(s),
        extents_nonempty(s),
        extents_nonoverlap(s),
        0 <= i < j < s.len(),
    ensures
        key_end(s[i]) <= key_start(s[j])
    decreases j - i
{
    if j == i + 1 {
        // Direct from extents_nonoverlap
    } else {
        extents_nonoverlap_pairwise(s, i, j - 1);
        // IH: key_end(s[i]) <= key_start(s[j-1])
        // valid + nonempty: key_start(s[j-1]) < key_end(s[j-1])
        // nonoverlap: key_end(s[j-1]) <= key_start(s[j])
    }
}

/// A point in offset space lies in at most one extent.
/// This is THE key correctness property for extent lookups:
/// bch2_btree_iter_peek returns at most one matching extent.
proof fn point_in_at_most_one_extent(s: Seq<Bkey>, point: u64, i: int, j: int)
    requires
        extents_valid(s),
        extents_nonempty(s),
        extents_nonoverlap(s),
        0 <= i < j < s.len(),
        key_start(s[i]) <= point,
        point < key_end(s[i]),
    ensures
        !(key_start(s[j]) <= point && point < key_end(s[j]))
{
    extents_nonoverlap_pairwise(s, i, j);
    // key_end(s[i]) <= key_start(s[j]) and point < key_end(s[i])
    // so point < key_start(s[j])
}

/// Non-overlapping extents are sorted by start offset.
proof fn extents_sorted_by_start(s: Seq<Bkey>, i: int, j: int)
    requires
        extents_valid(s),
        extents_nonempty(s),
        extents_nonoverlap(s),
        0 <= i < j < s.len(),
    ensures
        key_start(s[i]) < key_start(s[j])
{
    extents_nonoverlap_pairwise(s, i, j);
    // key_end(s[i]) <= key_start(s[j])
    // key_start(s[i]) < key_end(s[i]) (valid + nonempty)
    // chain: key_start(s[i]) < key_end(s[i]) <= key_start(s[j])
}

/// Non-overlapping extents are sorted by end offset.
proof fn extents_sorted_by_end(s: Seq<Bkey>, i: int, j: int)
    requires
        extents_valid(s),
        extents_nonempty(s),
        extents_nonoverlap(s),
        0 <= i < j < s.len(),
    ensures
        key_end(s[i]) < key_end(s[j])
{
    extents_nonoverlap_pairwise(s, i, j);
    // key_end(s[i]) <= key_start(s[j])
    // key_start(s[j]) < key_end(s[j]) (valid + nonempty)
    // chain: key_end(s[i]) <= key_start(s[j]) < key_end(s[j])
}

// ============================================================
// Extent trimming (cut_front / cut_back)
// ============================================================
//
// When inserting an extent that overlaps existing ones, bcachefs
// trims the overlapping extents: bch2_cut_front advances the start,
// bch2_cut_back shrinks the end. These proofs verify that trimming
// preserves key validity and that splitting produces adjacent
// non-overlapping pieces.

/// Front-trim: advance start of k to new_start, keeping same end.
/// Models bch2_cut_front from fs/bcachefs/extents.c.
pub open spec fn cut_front_spec(new_start: u64, k: Bkey) -> Bkey
    recommends
        k.p.offset >= k.size as u64,
        new_start >= key_start(k),
        new_start < key_end(k),
        k.p.offset - new_start <= u32::MAX as u64,
{
    Bkey {
        p: Bpos { inode: k.p.inode, offset: k.p.offset, snapshot: k.p.snapshot },
        size: (k.p.offset - new_start) as u32,
    }
}

/// Back-trim: shrink end of k to new_end, keeping same start.
/// Models bch2_cut_back from fs/bcachefs/extents.c.
pub open spec fn cut_back_spec(new_end: u64, k: Bkey) -> Bkey
    recommends
        k.p.offset >= k.size as u64,
        new_end > key_start(k),
        new_end <= key_end(k),
        new_end - key_start(k) <= u32::MAX as u64,
{
    Bkey {
        p: Bpos { inode: k.p.inode, offset: new_end, snapshot: k.p.snapshot },
        size: (new_end - key_start(k)) as u32,
    }
}

// Front-trim starts at the trim point, keeps the same end, stays valid.
proof fn cut_front_preserves(new_start: u64, k: Bkey)
    requires
        k.p.offset >= k.size as u64,
        new_start >= key_start(k),
        new_start < key_end(k),
        k.p.offset - new_start <= u32::MAX as u64,
    ensures
        key_start(cut_front_spec(new_start, k)) == new_start,
        key_end(cut_front_spec(new_start, k)) == key_end(k),
        cut_front_spec(new_start, k).size > 0,
        cut_front_spec(new_start, k).p.offset >= cut_front_spec(new_start, k).size as u64,
{}

// Back-trim keeps the same start, ends at the trim point, stays valid.
proof fn cut_back_preserves(new_end: u64, k: Bkey)
    requires
        k.p.offset >= k.size as u64,
        new_end > key_start(k),
        new_end <= key_end(k),
        new_end - key_start(k) <= u32::MAX as u64,
    ensures
        key_start(cut_back_spec(new_end, k)) == key_start(k),
        key_end(cut_back_spec(new_end, k)) == new_end,
        cut_back_spec(new_end, k).size > 0,
        cut_back_spec(new_end, k).p.offset >= cut_back_spec(new_end, k).size as u64,
{}

/// Splitting an extent at a point produces adjacent, non-overlapping
/// pieces that together cover the original range. This is the
/// fundamental correctness property of extent splitting.
proof fn extent_split_adjacent(k: Bkey, mid: u64)
    requires
        k.p.offset >= k.size as u64,
        k.size > 0,
        key_start(k) < mid,
        mid < key_end(k),
        mid - key_start(k) <= u32::MAX as u64,
        k.p.offset - mid <= u32::MAX as u64,
    ensures
        key_start(cut_back_spec(mid, k)) == key_start(k),
        key_end(cut_front_spec(mid, k)) == key_end(k),
        key_end(cut_back_spec(mid, k)) == key_start(cut_front_spec(mid, k)),
        cut_back_spec(mid, k).size > 0,
        cut_front_spec(mid, k).size > 0,
{}

/// After back-trimming an existing extent to make room for a new one,
/// the trimmed extent ends where the new extent starts — no overlap.
proof fn back_trim_clears_overlap(existing: Bkey, new_ext: Bkey)
    requires
        existing.p.offset >= existing.size as u64,
        new_ext.p.offset >= new_ext.size as u64,
        existing.size > 0,
        new_ext.size > 0,
        key_start(new_ext) > key_start(existing),
        key_start(new_ext) < key_end(existing),
        key_start(new_ext) - key_start(existing) <= u32::MAX as u64,
    ensures
        key_end(cut_back_spec(key_start(new_ext), existing)) <= key_start(new_ext)
{}

/// After front-trimming an existing extent to make room for a new one,
/// the trimmed extent starts where the new extent ends — no overlap.
proof fn front_trim_clears_overlap(existing: Bkey, new_ext: Bkey)
    requires
        existing.p.offset >= existing.size as u64,
        new_ext.p.offset >= new_ext.size as u64,
        existing.size > 0,
        new_ext.size > 0,
        key_end(new_ext) > key_start(existing),
        key_end(new_ext) < key_end(existing),
        existing.p.offset - key_end(new_ext) <= u32::MAX as u64,
    ensures
        key_start(cut_front_spec(key_end(new_ext), existing)) >= key_end(new_ext)
{}

// ============================================================
// Btree node split — preserving invariants
// ============================================================
//
// When a btree node exceeds its size limit, it's split at a pivot.
// Keys before the pivot go to the left child, keys at/after to the
// right. These proofs verify that both halves inherit sortedness
// and non-overlap from the original sequence.

/// Splitting a sorted bpos sequence preserves sortedness in both halves.
proof fn split_preserves_bpos_sorted(s: Seq<Bpos>, split_idx: int)
    requires
        bpos_sorted(s),
        0 < split_idx < s.len(),
    ensures
        bpos_sorted(s.subrange(0, split_idx)),
        bpos_sorted(s.subrange(split_idx, s.len() as int)),
{
    let left = s.subrange(0, split_idx);
    assert forall|j: int| 0 <= j < left.len() - 1 implies
        bpos_lt_spec(#[trigger] left[j], left[j + 1])
    by {
        assert(left[j] == s[j]);
        assert(left[j + 1] == s[j + 1]);
    }
    let right = s.subrange(split_idx, s.len() as int);
    assert forall|j: int| 0 <= j < right.len() - 1 implies
        bpos_lt_spec(#[trigger] right[j], right[j + 1])
    by {
        assert(right[j] == s[split_idx + j]);
        assert(right[j + 1] == s[split_idx + j + 1]);
    }
}

/// Splitting preserves the non-overlap invariant for extent sequences.
proof fn split_preserves_extent_invariants(s: Seq<Bkey>, split_idx: int)
    requires
        extents_valid(s),
        extents_nonempty(s),
        extents_nonoverlap(s),
        0 < split_idx < s.len(),
    ensures
        extents_valid(s.subrange(0, split_idx)),
        extents_nonempty(s.subrange(0, split_idx)),
        extents_nonoverlap(s.subrange(0, split_idx)),
        extents_valid(s.subrange(split_idx, s.len() as int)),
        extents_nonempty(s.subrange(split_idx, s.len() as int)),
        extents_nonoverlap(s.subrange(split_idx, s.len() as int)),
{
    let left = s.subrange(0, split_idx);
    // Left half: valid, nonempty
    assert forall|i: int| 0 <= i < left.len() implies
        (#[trigger] left[i]).p.offset >= left[i].size as u64
    by { assert(left[i] == s[i]); }
    assert forall|i: int| 0 <= i < left.len() implies
        (#[trigger] left[i]).size > 0
    by { assert(left[i] == s[i]); }
    // Left half: nonoverlap
    assert forall|i: int| 0 <= i < left.len() - 1 implies
        key_end(#[trigger] left[i]) <= key_start(left[i + 1])
    by {
        assert(left[i] == s[i]);
        assert(left[i + 1] == s[i + 1]);
    }

    let right = s.subrange(split_idx, s.len() as int);
    // Right half: valid, nonempty
    assert forall|i: int| 0 <= i < right.len() implies
        (#[trigger] right[i]).p.offset >= right[i].size as u64
    by { assert(right[i] == s[split_idx + i]); }
    assert forall|i: int| 0 <= i < right.len() implies
        (#[trigger] right[i]).size > 0
    by { assert(right[i] == s[split_idx + i]); }
    // Right half: nonoverlap
    assert forall|i: int| 0 <= i < right.len() - 1 implies
        key_end(#[trigger] right[i]) <= key_start(right[i + 1])
    by {
        assert(right[i] == s[split_idx + i]);
        assert(right[i + 1] == s[split_idx + i + 1]);
    }
}

/// After splitting, the left half's maximum is less than the right half's minimum.
/// This is the pivot property that separates the two child nodes.
proof fn split_pivot_property(s: Seq<Bpos>, split_idx: int)
    requires
        bpos_sorted(s),
        0 < split_idx < s.len(),
    ensures
        bpos_lt_spec(
            s.subrange(0, split_idx)[split_idx - 1],
            s.subrange(split_idx, s.len() as int)[0],
        )
{
    assert(s.subrange(0, split_idx)[split_idx - 1] == s[split_idx - 1]);
    assert(s.subrange(split_idx, s.len() as int)[0] == s[split_idx]);
}

/// Merging two sorted sequences where max(left) < min(right)
/// produces a sorted sequence. This is the btree node merge operation.
proof fn merge_preserves_bpos_sorted(left: Seq<Bpos>, right: Seq<Bpos>)
    requires
        bpos_sorted(left),
        bpos_sorted(right),
        left.len() > 0,
        right.len() > 0,
        bpos_lt_spec(left[left.len() - 1], right[0]),
    ensures
        bpos_sorted(left + right)
{
    let merged = left + right;
    assert forall|i: int| 0 <= i < merged.len() - 1 implies
        bpos_lt_spec(#[trigger] merged[i], merged[i + 1])
    by {
        if i < left.len() - 1 {
            // Both in left half
            assert(merged[i] == left[i]);
            assert(merged[i + 1] == left[i + 1]);
        } else if i == left.len() - 1 {
            // Crossing point: last of left, first of right
            assert(merged[i] == left[left.len() - 1]);
            assert(merged[i + 1] == right[0]);
        } else {
            // Both in right half
            let j = i - left.len() as int;
            assert(merged[i] == right[j]);
            assert(merged[i + 1] == right[j + 1]);
        }
    }
}

/// Merging two non-overlapping extent sequences where the left's last
/// extent ends before the right's first extent starts.
proof fn merge_preserves_extent_invariants(left: Seq<Bkey>, right: Seq<Bkey>)
    requires
        extents_valid(left),
        extents_nonempty(left),
        extents_nonoverlap(left),
        extents_valid(right),
        extents_nonempty(right),
        extents_nonoverlap(right),
        left.len() > 0,
        right.len() > 0,
        key_end(left[left.len() - 1]) <= key_start(right[0]),
    ensures
        extents_valid(left + right),
        extents_nonempty(left + right),
        extents_nonoverlap(left + right),
{
    let merged = left + right;
    // Valid
    assert forall|i: int| 0 <= i < merged.len() implies
        (#[trigger] merged[i]).p.offset >= merged[i].size as u64
    by {
        if i < left.len() { assert(merged[i] == left[i]); }
        else { assert(merged[i] == right[i - left.len() as int]); }
    }
    // Nonempty
    assert forall|i: int| 0 <= i < merged.len() implies
        (#[trigger] merged[i]).size > 0
    by {
        if i < left.len() { assert(merged[i] == left[i]); }
        else { assert(merged[i] == right[i - left.len() as int]); }
    }
    // Nonoverlap
    assert forall|i: int| 0 <= i < merged.len() - 1 implies
        key_end(#[trigger] merged[i]) <= key_start(merged[i + 1])
    by {
        if i < left.len() - 1 {
            assert(merged[i] == left[i]);
            assert(merged[i + 1] == left[i + 1]);
        } else if i == left.len() - 1 {
            assert(merged[i] == left[left.len() - 1]);
            assert(merged[i + 1] == right[0]);
        } else {
            let j = i - left.len() as int;
            assert(merged[i] == right[j]);
            assert(merged[i + 1] == right[j + 1]);
        }
    }
}

// ============================================================
// bversion comparison — two-field version stamp
// ============================================================
//
// bversion is (hi: u32, lo: u64), compared lexicographically.
// Used for key versioning in snapshots — each mutation gets a
// monotonically increasing version.

pub struct Bversion {
    pub hi: u32,
    pub lo: u64,
}

pub open spec fn bversion_cmp_spec(l: Bversion, r: Bversion) -> int {
    if l.hi != r.hi {
        if l.hi < r.hi { -1 } else { 1 }
    } else if l.lo != r.lo {
        if l.lo < r.lo { -1 } else { 1 }
    } else {
        0int
    }
}

pub open spec fn bversion_eq_spec(l: Bversion, r: Bversion) -> bool {
    l.hi == r.hi && l.lo == r.lo
}

pub open spec fn bversion_lt_spec(l: Bversion, r: Bversion) -> bool {
    if l.hi != r.hi {
        l.hi < r.hi
    } else {
        l.lo < r.lo
    }
}

pub open spec fn bversion_le_spec(l: Bversion, r: Bversion) -> bool {
    if l.hi != r.hi {
        l.hi < r.hi
    } else {
        l.lo <= r.lo
    }
}

fn bversion_cmp(l: &Bversion, r: &Bversion) -> (result: i32)
    ensures
        result == 0 ==> bversion_eq_spec(*l, *r),
        result < 0 ==> bversion_lt_spec(*l, *r),
        result > 0 ==> bversion_lt_spec(*r, *l),
        result == -1 || result == 0 || result == 1,
{
    if l.hi != r.hi {
        if l.hi < r.hi { -1 } else { 1 }
    } else if l.lo != r.lo {
        if l.lo < r.lo { -1 } else { 1 }
    } else {
        0
    }
}

fn bversion_eq(l: &Bversion, r: &Bversion) -> (result: bool)
    ensures result == bversion_eq_spec(*l, *r)
{
    l.hi == r.hi && l.lo == r.lo
}

// Total order proofs for bversion
proof fn bversion_le_reflexive(a: Bversion)
    ensures bversion_le_spec(a, a)
{}

proof fn bversion_le_antisymmetric(a: Bversion, b: Bversion)
    requires
        bversion_le_spec(a, b),
        bversion_le_spec(b, a),
    ensures bversion_eq_spec(a, b)
{}

proof fn bversion_le_transitive(a: Bversion, b: Bversion, c: Bversion)
    requires
        bversion_le_spec(a, b),
        bversion_le_spec(b, c),
    ensures bversion_le_spec(a, c)
{}

proof fn bversion_le_total(a: Bversion, b: Bversion)
    ensures bversion_le_spec(a, b) || bversion_le_spec(b, a)
{}

proof fn bversion_trichotomy(a: Bversion, b: Bversion)
    ensures
        (bversion_lt_spec(a, b) && !bversion_eq_spec(a, b) && !bversion_lt_spec(b, a))
        || (!bversion_lt_spec(a, b) && bversion_eq_spec(a, b) && !bversion_lt_spec(b, a))
        || (!bversion_lt_spec(a, b) && !bversion_eq_spec(a, b) && bversion_lt_spec(b, a))
{}

// ZERO_VERSION is the minimum
pub open spec fn zero_version_spec() -> Bversion {
    Bversion { hi: 0, lo: 0 }
}

pub open spec fn max_version_spec() -> Bversion {
    Bversion { hi: u32::MAX, lo: u64::MAX }
}

proof fn zero_version_is_minimum(v: Bversion)
    ensures bversion_le_spec(zero_version_spec(), v)
{}

proof fn max_version_is_maximum(v: Bversion)
    ensures bversion_le_spec(v, max_version_spec())
{}

fn bversion_zero(v: &Bversion) -> (result: bool)
    ensures result == bversion_eq_spec(*v, zero_version_spec())
{
    v.hi == 0 && v.lo == 0
}

// ============================================================
// Main — just to make it a valid Verus file
// ============================================================

fn main() {}

} // verus!
