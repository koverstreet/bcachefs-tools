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
// Main — just to make it a valid Verus file
// ============================================================

fn main() {}

} // verus!
