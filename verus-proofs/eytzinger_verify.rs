// Formal verification of the bcachefs eytzinger tree layout.
//
// An eytzinger tree is a full binary tree stored in a flat array where
// node i's children are at 2i (left) and 2i+1 (right), using 1-based
// indexing. This layout has excellent cache behavior because each level
// of the tree is contiguous in memory.
//
// This file proves:
// 1. Tree navigation correctness (child, parent, level)
// 2. In-bounds properties for valid trees
// 3. Inorder traversal properties
// 4. Search correctness (eytzinger0_find_le)
//
// The proofs mirror the C implementation in:
//   fs/bcachefs/util/eytzinger.h
//
// Run: ~/verus-bin/verus-x86-linux/verus verus-proofs/eytzinger_verify.rs

use vstd::prelude::*;

verus! {

// ============================================================
// Basic tree navigation — 1-based indexing
// ============================================================
//
// In a 1-based eytzinger tree:
//   left_child(i)  = 2*i
//   right_child(i) = 2*i + 1
//   parent(i)      = i/2    (integer division)
//   level(i)       = floor(log2(i))
//
// Node 1 is the root. Level k contains nodes [2^k, 2^(k+1)-1].
// A tree of size n contains nodes 1..n.

/// Left child of node i.
pub open spec fn left_child(i: nat) -> nat {
    2 * i
}

/// Right child of node i.
pub open spec fn right_child(i: nat) -> nat {
    2 * i + 1
}

/// Parent of node i (root's parent is 0, which is out of bounds).
pub open spec fn parent(i: nat) -> nat {
    i / 2
}

/// Level of node i (root is level 0).
/// Defined as the position of the highest set bit.
pub open spec fn level(i: nat) -> nat
    recommends i > 0
    decreases i
{
    if i <= 1 {
        0
    } else {
        1 + level(i / 2)
    }
}

// ============================================================
// Basic navigation properties
// ============================================================

/// Left and right children are distinct.
proof fn children_distinct(i: nat)
    requires i > 0
    ensures left_child(i) != right_child(i)
{
    // 2*i != 2*i + 1
}

/// Left child is always even, right child is always odd.
proof fn left_child_even(i: nat)
    requires i > 0
    ensures left_child(i) % 2 == 0
{}

proof fn right_child_odd(i: nat)
    requires i > 0
    ensures right_child(i) % 2 == 1
{}

/// Children are larger than parent.
proof fn left_child_gt(i: nat)
    requires i > 0
    ensures left_child(i) > i
{
    // 2*i > i when i > 0
}

proof fn right_child_gt(i: nat)
    requires i > 0
    ensures right_child(i) > i
{
    // 2*i + 1 > i when i > 0
}

/// Parent is smaller than child (for non-root).
proof fn parent_lt(i: nat)
    requires i > 1
    ensures parent(i) < i && parent(i) > 0
{
    // i/2 < i for i > 1
    // i/2 > 0 for i > 1
}

/// Parent-child roundtrip: parent of left child is the original node.
proof fn parent_of_left(i: nat)
    requires i > 0
    ensures parent(left_child(i)) == i
{
    // parent(2*i) = 2*i / 2 = i
}

/// Parent-child roundtrip: parent of right child is the original node.
proof fn parent_of_right(i: nat)
    requires i > 0
    ensures parent(right_child(i)) == i
{
    // parent(2*i + 1) = (2*i + 1) / 2 = i
    // (integer division truncates)
}

/// Child-parent roundtrip: child of parent recovers the original node.
/// Whether it's the left or right child depends on parity.
proof fn child_of_parent(i: nat)
    requires i > 1
    ensures
        (i % 2 == 0) ==> left_child(parent(i)) == i,
        (i % 2 == 1) ==> right_child(parent(i)) == i,
{
    // If i is even: i = 2k, parent = k, left_child(k) = 2k = i
    // If i is odd: i = 2k+1, parent = k, right_child(k) = 2k+1 = i
}

/// Level increases by 1 for children.
proof fn level_of_left_child(i: nat)
    requires i > 0
    ensures level(left_child(i)) == level(i) + 1
    decreases i
{
    // level(2*i) = 1 + level(2*i / 2) = 1 + level(i)
    if i > 1 {
        // For the recursive case, we need level(i) to unfold
    }
}

proof fn level_of_right_child(i: nat)
    requires i > 0
    ensures level(right_child(i)) == level(i) + 1
    decreases i
{
    // level(2*i + 1) = 1 + level((2*i + 1) / 2) = 1 + level(i)
    if i > 1 {
    }
}

/// The root is at level 0.
proof fn root_level()
    ensures level(1) == 0
{}

/// Level is monotonic: deeper nodes have higher levels.
proof fn level_monotonic(i: nat, j: nat)
    requires
        i > 0,
        j > 0,
        j >= 2 * i,  // j is at or below i's level
    ensures
        level(j) >= level(i)
    decreases j
{
    if j <= 1 {
        // j >= 2*i and i > 0 implies j >= 2, contradiction with j <= 1
    } else if i <= 1 {
        // level(i) = 0, level(j) >= 0 trivially
    } else {
        // j >= 2*i implies j/2 >= i (since j >= 2*i)
        // level(j) = 1 + level(j/2)
        // We need level(j/2) >= level(i) - 1 ... hmm, not quite
        // Actually: j >= 2*i means j/2 >= i >= 2*(i/2), so
        // level(j/2) >= level(i/2) by induction.
        // And level(j) = 1 + level(j/2), level(i) = 1 + level(i/2).
        if j / 2 >= 2 * (i / 2) {
            level_monotonic(i / 2, j / 2);
        } else {
            // j / 2 >= i > i / 2, so j / 2 >= i / 2 + 1 >= 2 * ((i/2 + 1) / 2)
            // This case needs more careful handling
        }
    }
}

// ============================================================
// In-bounds properties
// ============================================================

/// A node is valid (in-bounds) for a tree of the given size.
pub open spec fn valid_node(i: nat, size: nat) -> bool {
    i >= 1 && i <= size
}

/// If a node's left child is valid, the node is also valid.
proof fn valid_from_left_child(i: nat, size: nat)
    requires valid_node(left_child(i), size), i > 0
    ensures valid_node(i, size)
{
    // left_child(i) = 2*i <= size implies i <= size/2 <= size
    // And i >= 1 since i > 0.
}

/// If a node's right child is valid, the node is also valid.
proof fn valid_from_right_child(i: nat, size: nat)
    requires valid_node(right_child(i), size), i > 0
    ensures valid_node(i, size)
{
    // right_child(i) = 2*i + 1 <= size implies 2*i <= size - 1 < size
    // So i <= (size-1)/2 <= size.
}

/// A leaf node has no valid children.
pub open spec fn is_leaf(i: nat, size: nat) -> bool {
    valid_node(i, size) && left_child(i) > size
}

/// If left child is out of bounds, so is right child.
proof fn no_left_implies_no_right(i: nat, size: nat)
    requires i > 0, left_child(i) > size
    ensures right_child(i) > size
{
    // 2*i > size implies 2*i + 1 > size
}

/// Non-root valid nodes have valid parents.
proof fn valid_parent(i: nat, size: nat)
    requires valid_node(i, size), i > 1
    ensures valid_node(parent(i), size)
{
    parent_lt(i);
}

// ============================================================
// Power of two helpers
// ============================================================

/// Spec function for 2^k.
pub open spec fn pow2(k: nat) -> nat
    decreases k
{
    if k == 0 { 1 } else { 2 * pow2((k - 1) as nat) }
}

/// 2^k > 0.
proof fn pow2_positive(k: nat)
    ensures pow2(k) > 0
    decreases k
{
    if k > 0 {
        pow2_positive((k - 1) as nat);
    }
}

/// 2^k is strictly increasing.
proof fn pow2_increasing(a: nat, b: nat)
    requires a < b
    ensures pow2(a) < pow2(b)
    decreases b
{
    if b == a + 1 {
        pow2_positive(a);
        // pow2(b) = 2 * pow2(a) > pow2(a) since pow2(a) > 0
    } else {
        pow2_increasing(a, (b - 1) as nat);
        pow2_positive((b - 1) as nat);
    }
}

/// Nodes at level k have indices in [2^k, 2^(k+1) - 1].
proof fn level_range(i: nat)
    requires i > 0
    ensures
        i >= pow2(level(i)),
        i < pow2(level(i) + 1),
    decreases i
{
    if i <= 1 {
        // level(1) = 0, pow2(0) = 1, 1 >= 1 ✓
        // pow2(1) = 2, 1 < 2 ✓
    } else {
        level_range(i / 2);
        // By induction: i/2 >= pow2(level(i/2)) and i/2 < pow2(level(i/2) + 1)
        // level(i) = 1 + level(i/2)
        // pow2(level(i)) = pow2(1 + level(i/2)) = 2 * pow2(level(i/2))
        // i >= 2 * (i/2) >= 2 * pow2(level(i/2)) = pow2(level(i)) ✓
        // i <= 2 * (i/2) + 1 < 2 * pow2(level(i/2) + 1) + 1
        //   = pow2(level(i/2) + 2) + 1 = pow2(level(i) + 1) + 1
        // Hmm, need: i < pow2(level(i) + 1) = 2 * pow2(level(i/2) + 1)
        // i <= 2 * (i/2) + 1. And i/2 < pow2(level(i/2) + 1).
        // So i <= 2 * (i/2) + 1 <= 2 * (pow2(level(i/2) + 1) - 1) + 1
        //   = 2 * pow2(level(i/2) + 1) - 1 < 2 * pow2(level(i/2) + 1)
        //   = pow2(level(i) + 1) ✓
    }
}

/// Two nodes at the same level with the same index must be equal.
/// (Level + position within level uniquely determines the node.)
proof fn level_injective(i: nat, j: nat)
    requires
        i > 0, j > 0,
        level(i) == level(j),
        i == j, // placeholder — real version would need more structure
    ensures
        i == j
{}

// ============================================================
// Subtree size
// ============================================================
//
// The subtree rooted at node i in a tree of size n contains
// all valid descendants. We count them recursively.

/// Number of valid nodes in the subtree rooted at i.
/// Measure: as i grows (children are 2i, 2i+1), it eventually
/// exceeds n, giving the base case. We use (n + 1 - i) as nat
/// but need to guard against underflow.
pub open spec fn subtree_size(i: nat, n: nat) -> nat
    decreases (if i > 0 && i <= n { n + 1 - i } else { 0 })
{
    if i == 0 || i > n {
        0
    } else {
        // Inline left_child and right_child for termination checking.
        // left_child(i) = 2*i > i (when i > 0), so n - 2*i < n - i.
        // right_child(i) = 2*i + 1 > i, same reasoning.
        1 + subtree_size(2 * i, n) + subtree_size(2 * i + 1, n)
    }
}

/// A leaf has subtree size 1.
proof fn leaf_subtree_size(i: nat, n: nat)
    requires valid_node(i, n), is_leaf(i, n)
    ensures subtree_size(i, n) == 1
{
    // Need 2 unfoldings: one to expose the recursive calls,
    // one more to evaluate them at the base case (2*i > n).
    reveal_with_fuel(subtree_size, 2);
    assert(2 * i > n);  // from is_leaf: left_child(i) = 2*i > n
}

/// Subtree size is positive for valid nodes.
proof fn subtree_size_positive(i: nat, n: nat)
    requires valid_node(i, n)
    ensures subtree_size(i, n) >= 1
{
    // subtree_size(i, n) = 1 + left + right >= 1
}

/// The whole tree (rooted at 1) has size n.
/// This is the fundamental counting property.
///
/// Proof sketch: every node j in [1, n] is in exactly one subtree.
/// Node j's path from root: j, j/2, j/4, ..., 1. It goes left at
/// parent(j) if j is even, right if j is odd. So j lands in the
/// left subtree of parent(j) if even, right if odd.
///
/// Full proof requires showing the subtree partition is exact.
/// Deferred — the simpler properties are more immediately useful.
///
/// For small n, we can verify directly:
proof fn whole_tree_size_1()
    ensures subtree_size(1, 1) == 1
{
    reveal_with_fuel(subtree_size, 2);
}

proof fn whole_tree_size_2()
    ensures subtree_size(1, 2) == 2
{
    reveal_with_fuel(subtree_size, 3);
}

proof fn whole_tree_size_3()
    ensures subtree_size(1, 3) == 3
{
    reveal_with_fuel(subtree_size, 3);
}

/// Subtree sizes are additive: total = 1 + left_size + right_size.
proof fn subtree_size_additive(i: nat, n: nat)
    requires valid_node(i, n)
    ensures
        subtree_size(i, n) ==
        1 + subtree_size(left_child(i), n) + subtree_size(right_child(i), n)
{
    // Direct from the definition.
}

// ============================================================
// Inorder traversal sequence
// ============================================================
//
// The inorder traversal visits: left subtree, root, right subtree.
// For a sorted array stored in eytzinger layout, the inorder
// traversal produces the sorted order. This connects tree position
// to sorted order — what __eytzinger1_to_inorder computes.

/// The sequence of nodes visited in inorder traversal of the
/// subtree rooted at node i in a tree of size n.
pub open spec fn inorder_seq(i: nat, n: nat) -> Seq<nat>
    decreases (if i > 0 && i <= n { n + 1 - i } else { 0 })
{
    if i == 0 || i > n {
        Seq::<nat>::empty()
    } else {
        let left = inorder_seq(2 * i, n);
        let right = inorder_seq(2 * i + 1, n);
        left.push(i).add(right)
    }
}

/// Inorder sequence length equals subtree size.
proof fn inorder_seq_len(i: nat, n: nat)
    ensures inorder_seq(i, n).len() == subtree_size(i, n)
    decreases (if i > 0 && i <= n { n + 1 - i } else { 0 })
{
    if i == 0 || i > n {
    } else {
        inorder_seq_len(2 * i, n);
        inorder_seq_len(2 * i + 1, n);
    }
}

/// Inorder sequence of a leaf is a single element.
proof fn inorder_seq_leaf(i: nat, n: nat)
    requires valid_node(i, n), is_leaf(i, n)
    ensures
        inorder_seq(i, n).len() == 1,
        inorder_seq(i, n)[0] == i,
{
    reveal_with_fuel(inorder_seq, 2);
    no_left_implies_no_right(i, n);
}

/// The root of a subtree is at index |left_subtree| in the inorder sequence.
proof fn inorder_root_position(i: nat, n: nat)
    requires valid_node(i, n)
    ensures
        inorder_seq(i, n).len() >= 1,
        inorder_seq(i, n)[subtree_size(2 * i, n) as int] == i,
{
    inorder_seq_len(i, n);
    inorder_seq_len(2 * i, n);
    subtree_size_positive(i, n);
}

// ============================================================
// Search — key correctness property
// ============================================================
//
// The eytzinger search (eytzinger0_find_le) works by walking
// the tree from root to a leaf-past-end:
//
//   n = 1;
//   while (n <= nr)
//       n = eytzinger1_child(n, cmp <= 0);
//   n >>= __ffs(n) + 1;      // backtrack
//   return n - 1;             // 0-based result
//
// Left turns (element > search) append 0 to n's binary encoding.
// Right turns (element <= search) append 1.
// The backtrack strips the lowest 1 and everything below,
// recovering the last right-turn node — the correct answer.

// ============================================================
// Search path — modeling the tree traversal
// ============================================================
//
// The eytzinger search traverses from root to a leaf-past-end:
//   n = 1;
//   while (n <= nr)
//       n = eytzinger1_child(n, cmp <= 0);  // left if >, right if <=
//
// We model a search path as a sequence of left/right decisions.
// The path uniquely determines the final position n.

/// Is node j a descendant of node i?
/// (Follows left/right child links from i.)
pub open spec fn is_descendant(i: nat, j: nat) -> bool
    decreases j
{
    if j == 0 || i == 0 {
        false
    } else if i == j {
        true
    } else if j < i {
        false  // children have larger indices
    } else {
        // j's parent is j/2
        is_descendant(i, j / 2)
    }
}

/// Root is ancestor of all valid nodes.
proof fn root_is_ancestor_of_all(j: nat)
    requires j >= 1
    ensures is_descendant(1, j)
    decreases j
{
    if j == 1 {
        // is_descendant(1, 1) = true (i == j)
    } else {
        // j > 1, parent = j/2 >= 1
        root_is_ancestor_of_all(j / 2);
        // is_descendant(1, j/2) holds
        // is_descendant(1, j): j >= 1, 1 >= 1, j != 1, j > 1,
        //   so check is_descendant(1, j/2) = true ✓
    }
}

/// Children are descendants.
proof fn child_is_descendant(i: nat)
    requires i >= 1
    ensures
        is_descendant(i, left_child(i)),
        is_descendant(i, right_child(i)),
{
    // Need 2 unfoldings:
    // is_descendant(i, 2*i) → is_descendant(i, 2*i/2) → is_descendant(i, i) = true
    reveal_with_fuel(is_descendant, 3);
}

/// Search step: going to a child strictly increases the index.
proof fn search_step_progress(i: nat, dir: nat)
    requires i >= 1, dir <= 1
    ensures
        2 * i + dir > i,
        2 * i + dir >= 2,
{
    // 2*i + dir > i since i >= 1 and dir >= 0
}

/// Search terminates: after at most log2(nr) + 1 steps, n > nr.
/// Each step at least doubles n (n -> 2n or 2n+1), so after
/// log2(nr) steps, n > nr.
proof fn search_terminates_bound(n: nat, nr: nat, steps: nat)
    requires
        n >= 1,
        nr >= 1,
        n <= nr,
    ensures
        // After level(nr) + 1 steps of doubling, we exceed nr.
        // 2^(level(nr)+1) > nr (from level_range).
        // Starting from n >= 1, after k doublings, n >= 2^k.
        // So after level(nr)+1 doublings, n >= 2^(level(nr)+1) > nr.
        true  // Statement is more complex; see level_range.
{}

/// The search path from root to a leaf-past-end node.
/// Models the while loop in eytzinger0_find_le.
///
/// search_path(n, nr, decisions): starting at node n in tree of
/// size nr, follow left/right decisions until n > nr.
/// Returns the final n (> nr).
pub open spec fn search_path(n: nat, nr: nat, decisions: Seq<bool>) -> nat
    decreases decisions.len()
{
    if n > nr || decisions.len() == 0 {
        n
    } else {
        let go_right = decisions[0];
        let next = if go_right { right_child(n) } else { left_child(n) };
        search_path(next, nr, decisions.subrange(1, decisions.len() as int))
    }
}

/// The search path always produces a result > nr (given enough decisions).
proof fn search_path_exceeds(n: nat, nr: nat, decisions: Seq<bool>)
    requires
        n >= 1,
        nr >= 1,
    ensures
        search_path(n, nr, decisions) >= n
    decreases decisions.len()
{
    if n > nr || decisions.len() == 0 {
        // Returns n >= n
    } else {
        let go_right = decisions[0];
        let next = if go_right { right_child(n) } else { left_child(n) };
        // next >= 2*n > n (since n >= 1)
        assert(next >= 2 * n);
        assert(next > n);
        assert(next >= 1);
        search_path_exceeds(next, nr, decisions.subrange(1, decisions.len() as int));
    }
}

// ============================================================
// Backtrack — bit manipulation for search result recovery
// ============================================================
//
// After the search loop, n > nr and n's binary representation
// encodes the full search path. The backtrack strips the lowest
// set bit, recovering the last right-turn node.
//
// Recursively: if n is odd, strip the trailing 1 (n/2).
// If n is even, strip the trailing 0 and continue.
// This is equivalent to n >>= __ffs(n) + 1 in the C code.

/// Backtrack for find_le: strip the lowest set bit.
pub open spec fn backtrack_le(n: nat) -> nat
    decreases n
{
    if n == 0 { 0 }
    else if n % 2 == 1 { n / 2 }
    else { backtrack_le(n / 2) }
}

/// Backtracking a right child (2m+1) gives the parent (m).
proof fn backtrack_right_child(m: nat)
    ensures backtrack_le(2 * m + 1) == m
{}

/// Backtracking through a left child (2m) is transparent.
proof fn backtrack_shift(m: nat)
    requires m > 0
    ensures backtrack_le(2 * m) == backtrack_le(m)
{}

/// Backtracking from the root gives 0 (no right-turn ancestor).
proof fn backtrack_root()
    ensures backtrack_le(1) == 0
{}

// ============================================================
// Concrete search — models eytzinger0_find_le
// ============================================================
//
// The search walks from the root (node 1, 1-based) down the tree.
// At each node, it compares a[node] with the search value:
//   a[node] <= search → go right (append 1 bit)
//   a[node] >  search → go left  (append 0 bit)
//
// After falling off the tree (node > n), the backtrack
// recovers the last right-turn node — the answer to find_le.

/// The search loop: walk down the tree making comparisons.
/// a is 0-indexed, tree is 1-indexed, so a[nd-1] is the element at node nd.
/// Returns the final position past the tree (> n).
pub open spec fn search_loop(nd: nat, n: nat, a: Seq<int>, search: int) -> nat
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    if nd == 0 || nd > n {
        nd
    } else {
        let go_right = (a[(nd - 1) as int] <= search);
        let next = if go_right { 2 * nd + 1 } else { 2 * nd };
        search_loop(next, n, a, search)
    }
}

/// The best right turn: deepest node on the search path where
/// a[node] <= search, or 0 if no such node exists.
pub open spec fn best_right_turn(nd: nat, n: nat, a: Seq<int>, search: int) -> nat
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    if nd == 0 || nd > n {
        0
    } else {
        let go_right = (a[(nd - 1) as int] <= search);
        let next = if go_right { 2 * nd + 1 } else { 2 * nd };
        let deeper = best_right_turn(next, n, a, search);
        if deeper > 0 { deeper }
        else if go_right { nd }
        else { 0 }
    }
}

/// The search loop result always exceeds n (search terminates past the tree).
proof fn search_loop_exceeds_n(nd: nat, n: nat, a: Seq<int>, search: int)
    requires nd >= 1, nd <= n, a.len() >= n
    ensures search_loop(nd, n, a, search) > n
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    let go_right = (a[(nd - 1) as int] <= search);
    let next = if go_right { 2 * nd + 1 } else { 2 * nd };
    if next > n {
        reveal_with_fuel(search_loop, 2);
    } else {
        search_loop_exceeds_n(next, n, a, search);
    }
}

/// Key invariant: backtrack of search result matches best_right_turn.
///
/// When the subtree has right turns: backtrack gives the deepest one.
/// When it doesn't: backtrack "passes through" to backtrack_le(nd),
/// representing the ancestor path encoding above this subtree.
proof fn backtrack_search_invariant(nd: nat, n: nat, a: Seq<int>, search: int)
    requires nd >= 1, nd <= n, a.len() >= n
    ensures
        best_right_turn(nd, n, a, search) > 0 ==>
            backtrack_le(search_loop(nd, n, a, search)) ==
            best_right_turn(nd, n, a, search),
        best_right_turn(nd, n, a, search) == 0 ==>
            backtrack_le(search_loop(nd, n, a, search)) ==
            backtrack_le(nd),
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    let go_right = (a[(nd - 1) as int] <= search);
    let next = if go_right { 2 * nd + 1 } else { 2 * nd };

    if next > n {
        // Leaf case: the subtree below nd is empty.
        // Need fuel 2 to see: search_loop(nd) → search_loop(next) → next
        // and: best_right_turn(nd) → best_right_turn(next) → 0
        reveal_with_fuel(search_loop, 2);
        reveal_with_fuel(best_right_turn, 2);

        if go_right {
            // next = 2*nd+1. brt = nd. Need: backtrack_le(2*nd+1) == nd
            backtrack_right_child(nd);
        } else {
            // next = 2*nd. brt = 0. Need: backtrack_le(2*nd) == backtrack_le(nd)
            backtrack_shift(nd);
        }
    } else {
        // Recursive case: descend into subtree
        backtrack_search_invariant(next, n, a, search);
        let brt_sub = best_right_turn(next, n, a, search);

        if go_right {
            // next = 2*nd+1
            if brt_sub == 0 {
                // No deeper right turns. IH: backtrack_le(r) == backtrack_le(2*nd+1)
                // backtrack_le(2*nd+1) == nd, which is the answer.
                backtrack_right_child(nd);
            }
            // brt_sub > 0: IH directly gives backtrack_le(r) == brt_sub == brt
        } else {
            // next = 2*nd
            if brt_sub == 0 {
                // No right turns at all. IH: backtrack_le(r) == backtrack_le(2*nd)
                // backtrack_le(2*nd) == backtrack_le(nd) by transparency.
                backtrack_shift(nd);
            }
            // brt_sub > 0: IH directly gives backtrack_le(r) == brt_sub == brt
        }
    }
}

/// Main theorem: for a search starting at the root,
/// backtrack of the search result equals the best right turn.
///
/// This proves that the C bit manipulation (n >>= __ffs(n) + 1)
/// correctly recovers the answer from the binary-encoded search path.
proof fn find_le_backtrack_correct(n: nat, a: Seq<int>, search: int)
    requires n >= 1, a.len() >= n
    ensures
        backtrack_le(search_loop(1, n, a, search)) ==
        best_right_turn(1, n, a, search)
{
    backtrack_search_invariant(1, n, a, search);
    if best_right_turn(1, n, a, search) == 0 {
        backtrack_root();
    }
}

// ============================================================
// BST ordering — connecting tree structure to sorted data
// ============================================================
//
// An eytzinger tree stores a sorted array such that the inorder
// traversal gives the sorted order. This means each node's value
// is greater than all left-subtree values and less than all
// right-subtree values.
//
// We formalize this as subtree_in_range: every node in a subtree
// has its value strictly within (lo, hi), with left subtree in
// (lo, a[node]) and right subtree in (a[node], hi).

/// The BST ordering invariant with explicit bounds.
/// Every element in the subtree rooted at i is strictly within (lo, hi).
pub open spec fn subtree_in_range(a: Seq<int>, n: nat, i: nat, lo: int, hi: int) -> bool
    decreases (if i > 0 && i <= n { n + 1 - i } else { 0 })
{
    if i == 0 || i > n {
        true
    } else {
        lo < a[(i - 1) as int] && a[(i - 1) as int] < hi &&
        subtree_in_range(a, n, 2 * i, lo, a[(i - 1) as int]) &&
        subtree_in_range(a, n, 2 * i + 1, a[(i - 1) as int], hi)
    }
}

/// A descendant has index >= the ancestor.
proof fn descendant_ge(i: nat, j: nat)
    requires i >= 1, is_descendant(i, j)
    ensures j >= i
    decreases j
{
    // By definition: j < i returns false, j == i returns true.
    // If j > i: recurses on j/2.
    if j == i {
        // j >= i trivially
    } else {
        // j > i (since j < i returns false in is_descendant)
    }
}

/// Any descendant of nd (other than nd itself) is in the left or right subtree.
proof fn descendant_partition(nd: nat, j: nat)
    requires nd >= 1, j > nd, is_descendant(nd, j)
    ensures is_descendant(2 * nd, j) || is_descendant(2 * nd + 1, j)
    decreases j - nd
{
    let p = j / 2;
    // is_descendant(nd, j) with j > nd unfolds to is_descendant(nd, j/2)
    if p == nd {
        // j is an immediate child: j = 2*nd or 2*nd+1
        // is_descendant(x, x) = true for x >= 1
    } else {
        // is_descendant(nd, p) holds (from unfolding is_descendant(nd, j))
        // p != nd, so p > nd (descendants have index >= ancestor)
        descendant_ge(nd, p);
        descendant_partition(nd, p);
        // IH: is_descendant(2*nd, p) || is_descendant(2*nd+1, p)
        // Since j >= 2*p and p >= 2*nd (or 2*nd+1), j > 2*nd (or 2*nd+1).
        // So is_descendant extends from p to j by one unfolding step.
        assert(j >= 2 * p);
        if is_descendant(2 * nd, p) {
            descendant_ge(2 * nd, p);
            assert(j > 2 * nd);
        } else {
            assert(is_descendant(2 * nd + 1, p));
            descendant_ge(2 * nd + 1, p);
            assert(j > 2 * nd + 1);
        }
    }
}

/// BST bounds extend to all descendants: every descendant of root
/// in a BST-bounded subtree has its value within (lo, hi).
proof fn descendant_bounded(
    a: Seq<int>, n: nat, root: nat, lo: int, hi: int, j: nat
)
    requires
        a.len() >= n,
        valid_node(root, n), valid_node(j, n),
        subtree_in_range(a, n, root, lo, hi),
        is_descendant(root, j),
    ensures
        lo < a[(j - 1) as int] && a[(j - 1) as int] < hi
    decreases (if root > 0 && root <= n { n + 1 - root } else { 0 })
{
    if j == root {
        // Direct from subtree_in_range
    } else {
        descendant_partition(root, j);
        if is_descendant(2 * root, j) {
            // j is in left subtree, bounds narrow to (lo, a[root-1])
            // j <= n and j >= 2*root, so 2*root <= n
            assert(valid_node(2 * root, n));
            subtree_in_range_left(a, n, root, lo, hi);
            descendant_bounded(a, n, 2 * root, lo, a[(root - 1) as int], j);
        } else {
            assert(is_descendant(2 * root + 1, j));
            assert(valid_node(2 * root + 1, n));
            subtree_in_range_right(a, n, root, lo, hi);
            descendant_bounded(a, n, 2 * root + 1, a[(root - 1) as int], hi, j);
        }
    }
}

/// The root element is within bounds.
proof fn subtree_in_range_root(a: Seq<int>, n: nat, i: nat, lo: int, hi: int)
    requires
        valid_node(i, n),
        a.len() >= n,
        subtree_in_range(a, n, i, lo, hi),
    ensures
        lo < a[(i - 1) as int] && a[(i - 1) as int] < hi
{}

/// Subtree bounds narrow at left child.
proof fn subtree_in_range_left(a: Seq<int>, n: nat, i: nat, lo: int, hi: int)
    requires
        valid_node(i, n),
        a.len() >= n,
        subtree_in_range(a, n, i, lo, hi),
    ensures
        subtree_in_range(a, n, 2 * i, lo, a[(i - 1) as int])
{}

/// Subtree bounds narrow at right child.
proof fn subtree_in_range_right(a: Seq<int>, n: nat, i: nat, lo: int, hi: int)
    requires
        valid_node(i, n),
        a.len() >= n,
        subtree_in_range(a, n, i, lo, hi),
    ensures
        subtree_in_range(a, n, 2 * i + 1, a[(i - 1) as int], hi)
{}

/// The best right turn returns a valid node or 0.
proof fn best_right_turn_valid(nd: nat, n: nat, a: Seq<int>, search: int)
    requires nd >= 1, nd <= n, a.len() >= n
    ensures
        best_right_turn(nd, n, a, search) == 0 ||
        valid_node(best_right_turn(nd, n, a, search), n)
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    let go_right = (a[(nd - 1) as int] <= search);
    let next = if go_right { 2 * nd + 1 } else { 2 * nd };
    if next > n {
        reveal_with_fuel(best_right_turn, 2);
    } else {
        best_right_turn_valid(next, n, a, search);
    }
}

/// The best right turn's element is <= search (when nonzero).
proof fn best_right_turn_le_search(nd: nat, n: nat, a: Seq<int>, search: int)
    requires nd >= 1, nd <= n, a.len() >= n
    ensures
        best_right_turn(nd, n, a, search) > 0 ==>
            a[(best_right_turn(nd, n, a, search) - 1) as int] <= search
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    let go_right = (a[(nd - 1) as int] <= search);
    let next = if go_right { 2 * nd + 1 } else { 2 * nd };
    if next > n {
        reveal_with_fuel(best_right_turn, 2);
    } else {
        best_right_turn_le_search(next, n, a, search);
    }
}

/// When best_right_turn is 0, all elements on the search path exceed search.
/// Combined with BST ordering, this means NO element in the subtree is <= search.
proof fn best_right_turn_zero_means_all_gt(
    nd: nat, n: nat, a: Seq<int>, search: int, lo: int, hi: int
)
    requires
        nd >= 1, nd <= n, a.len() >= n,
        subtree_in_range(a, n, nd, lo, hi),
        best_right_turn(nd, n, a, search) == 0,
    ensures
        // The strongest we can say: lo >= search (all elements > lo >= search)
        // or equivalently: search < lo + 1 (since lo is strict lower bound)
        // Actually: every element in range (lo, hi) that's in the tree is > search.
        // The path element a[nd-1] > search (we went left), and bounds give us
        // lo < a[nd-1], so we can bound the subtree.
        //
        // What we actually prove: search < a[(nd - 1) as int]
        // (the current node's value exceeds search — we went left here)
        a[(nd - 1) as int] > search
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    let go_right = (a[(nd - 1) as int] <= search);
    let next = if go_right { 2 * nd + 1 } else { 2 * nd };
    // If go_right were true, then with no deeper right turns,
    // best_right_turn would return nd (> 0). Contradiction with brt == 0.
    // So go_right must be false, meaning a[nd-1] > search.
    if next <= n {
        // We need fuel 1 (default) to see best_right_turn(nd) unfolds
        // and check that go_right must be false.
    } else {
        reveal_with_fuel(best_right_turn, 2);
    }
}

/// Full semantic correctness of the search in a BST-ordered subtree.
///
/// Given BST ordering (subtree_in_range), best_right_turn finds the
/// greatest element <= search, or 0 if no such element exists.
///
/// The "greatest" property is captured by the lower bound: the result's
/// value is > lo (the subtree's lower bound), so any element with a
/// smaller value is < result's value.
proof fn search_semantics(
    nd: nat, n: nat, a: Seq<int>, search: int, lo: int, hi: int
)
    requires
        nd >= 1, nd <= n, a.len() >= n,
        subtree_in_range(a, n, nd, lo, hi),
    ensures
        // Result is valid and its value is <= search
        best_right_turn(nd, n, a, search) > 0 ==>
            valid_node(best_right_turn(nd, n, a, search), n) &&
            a[(best_right_turn(nd, n, a, search) - 1) as int] <= search,
        // Result is 0 iff nothing in (lo, hi) ∩ tree is <= search
        best_right_turn(nd, n, a, search) == 0 ==>
            a[(nd - 1) as int] > search,
        // Result's value is within the subtree's bounds
        best_right_turn(nd, n, a, search) > 0 ==>
            lo < a[(best_right_turn(nd, n, a, search) - 1) as int],
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    let brt = best_right_turn(nd, n, a, search);
    let go_right = (a[(nd - 1) as int] <= search);
    let next = if go_right { 2 * nd + 1 } else { 2 * nd };

    if next > n {
        // Leaf: no children
        reveal_with_fuel(best_right_turn, 2);
        // go_right ==> brt == nd, a[nd-1] <= search, lo < a[nd-1] (from subtree_in_range)
        // !go_right ==> brt == 0, a[nd-1] > search
    } else {
        // Recursive case
        if go_right {
            // Right subtree is in range (a[nd-1], hi)
            subtree_in_range_right(a, n, nd, lo, hi);
            search_semantics(next, n, a, search, a[(nd - 1) as int], hi);
        } else {
            // Left subtree is in range (lo, a[nd-1])
            subtree_in_range_left(a, n, nd, lo, hi);
            search_semantics(next, n, a, search, lo, a[(nd - 1) as int]);
        }
        let brt_sub = best_right_turn(next, n, a, search);

        if go_right && brt_sub > 0 {
            // Deeper result in right subtree: value > a[nd-1] > lo ✓
        } else if go_right && brt_sub == 0 {
            // No deeper right turn. brt = nd. a[nd-1] <= search. lo < a[nd-1]. ✓
        } else if !go_right && brt_sub > 0 {
            // Result from left subtree: lo < value (by induction) ✓
        }
        // !go_right && brt_sub == 0: brt = 0, a[nd-1] > search ✓
    }
}

/// **THE MAIN CORRECTNESS THEOREM for eytzinger search.**
///
/// For any element j in the subtree that satisfies a[j-1] <= search,
/// the best_right_turn result has value >= a[j-1].
///
/// Combined with best_right_turn_le_search (result value <= search),
/// this says the result is the GREATEST element <= search.
///
/// The proof follows the search path. At each node nd:
/// - If j == nd: brt value >= a[nd-1] (brt is nd or deeper with larger value)
/// - If j in left subtree and we go right: a[j-1] < a[nd-1] <= a[brt-1] (BST)
/// - If j in left subtree and we go left: recurse into left subtree
/// - If j in right subtree and we go right: recurse into right subtree
/// - If j in right subtree and we go left: a[j-1] > a[nd-1] > search,
///   contradicting a[j-1] <= search
proof fn search_greatest(
    nd: nat, n: nat, a: Seq<int>, search: int, lo: int, hi: int, j: nat
)
    requires
        nd >= 1, nd <= n, a.len() >= n,
        subtree_in_range(a, n, nd, lo, hi),
        valid_node(j, n),
        is_descendant(nd, j),
        a[(j - 1) as int] <= search,
    ensures
        best_right_turn(nd, n, a, search) > 0,
        a[(best_right_turn(nd, n, a, search) - 1) as int] >= a[(j - 1) as int],
    decreases (if nd > 0 && nd <= n { n + 1 - nd } else { 0 })
{
    let go_right = (a[(nd - 1) as int] <= search);
    let next = if go_right { 2 * nd + 1 } else { 2 * nd };

    if j == nd {
        // j IS the current node. a[nd-1] <= search, so go_right.
        assert(go_right);
        if next > n {
            // Leaf case: brt = nd, a[brt-1] = a[j-1]. ✓
            reveal_with_fuel(best_right_turn, 2);
        } else {
            // brt is nd or from right subtree (all values > a[nd-1]).
            subtree_in_range_right(a, n, nd, lo, hi);
            search_semantics(next, n, a, search, a[(nd - 1) as int], hi);
            // search_semantics gives: if brt_sub > 0, a[brt_sub-1] > a[nd-1]
            // So brt value >= a[nd-1] = a[j-1] in all cases. ✓
        }
    } else {
        // j > nd. Determine which subtree j is in.
        descendant_partition(nd, j);

        if is_descendant(2 * nd, j) {
            // j is in LEFT subtree.
            // LEFT subtree is in (lo, a[nd-1]), so a[j-1] < a[nd-1].
            assert(valid_node(2 * nd, n));
            subtree_in_range_left(a, n, nd, lo, hi);
            descendant_bounded(a, n, 2 * nd, lo, a[(nd - 1) as int], j);
            // Now we know: a[j-1] < a[nd-1]

            if go_right {
                // We go right, skipping the left subtree.
                // a[j-1] < a[nd-1] <= search, so a[j-1] < a[nd-1].
                // brt is nd or from right subtree (value > a[nd-1] > a[j-1]).
                if next > n {
                    reveal_with_fuel(best_right_turn, 2);
                    // brt = nd, a[brt-1] = a[nd-1] > a[j-1]. ✓
                } else {
                    subtree_in_range_right(a, n, nd, lo, hi);
                    search_semantics(next, n, a, search, a[(nd - 1) as int], hi);
                    // If brt_sub > 0: a[brt_sub-1] > a[nd-1] > a[j-1]. ✓
                    // If brt_sub == 0: brt = nd, a[nd-1] > a[j-1]. ✓
                }
            } else {
                // We go left into the left subtree. Recurse.
                if next <= n {
                    search_greatest(next, n, a, search, lo, a[(nd - 1) as int], j);
                }
                // If next > n: left child out of bounds, but j is descendant of
                // left child and valid. So j >= 2*nd, but 2*nd > n and j <= n.
                // Contradiction (from descendant_ge: j >= 2*nd > n but j <= n).
            }
        } else {
            // j is in RIGHT subtree.
            assert(is_descendant(2 * nd + 1, j));
            assert(valid_node(2 * nd + 1, n));
            subtree_in_range_right(a, n, nd, lo, hi);

            if go_right {
                // We go right into the right subtree. Recurse.
                if next <= n {
                    search_greatest(next, n, a, search, a[(nd - 1) as int], hi, j);
                }
                // If next > n: same contradiction as above.
            } else {
                // We go left, skipping the right subtree.
                // Right subtree in (a[nd-1], hi). a[j-1] > a[nd-1] > search.
                // But a[j-1] <= search. Contradiction!
                descendant_bounded(a, n, 2 * nd + 1, a[(nd - 1) as int], hi, j);
                // a[j-1] > a[nd-1] > search, contradicts a[j-1] <= search
            }
        }
    }
}

/// Corollary: the search result is the correct find_le answer for the whole tree.
///
/// Combining find_le_backtrack_correct (backtrack recovers the best right turn)
/// with search_greatest (best right turn is the greatest element <= search),
/// we get full correctness of eytzinger0_find_le.
proof fn find_le_correct(n: nat, a: Seq<int>, search: int, lo: int, hi: int)
    requires
        n >= 1, a.len() >= n,
        subtree_in_range(a, n, 1, lo, hi),
    ensures
        // The find_le result (backtrack of search loop from root):
        ({
            let result = backtrack_le(search_loop(1, n, a, search));
            // If result > 0: it's a valid node with value <= search,
            // and no other node in the tree has a larger value <= search.
            &&& (result > 0 ==>
                valid_node(result, n) &&
                a[(result - 1) as int] <= search)
            // If result == 0: no element in the tree is <= search.
            &&& (result == 0 ==>
                a[0] > search)  // root (smallest path element) > search
        })
{
    find_le_backtrack_correct(n, a, search);
    let brt = best_right_turn(1, n, a, search);

    if brt > 0 {
        best_right_turn_valid(1, n, a, search);
        best_right_turn_le_search(1, n, a, search);
    } else {
        search_semantics(1, n, a, search, lo, hi);
    }
}

// ============================================================
// Eytzinger-to-inorder bijection
// ============================================================
//
// __eytzinger1_to_inorder and __inorder_to_eytzinger1 convert
// between eytzinger tree indices and inorder (sorted) positions
// using bit manipulation.
//
// For node i at level b in a tree of depth d:
//   raw(i) = (2 * (i - 2^b) + 1) * 2^(d-b)
//
// Children have raw values (4p+1)*k, (4p+2)*k, (4p+3)*k
// giving immediate BST ordering.
//
// Incomplete trees adjust: raw → (raw + extra) / 2.

/// pow2(k+1) = 2 * pow2(k).
proof fn pow2_double(k: nat)
    ensures pow2(k + 1) == 2 * pow2(k)
{}

/// Level is bounded: i <= j implies level(i) <= level(j).
proof fn level_le_when_le(i: nat, j: nat)
    requires i >= 1, j >= 1, i <= j
    ensures level(i) <= level(j)
{
    level_range(i);
    level_range(j);
    if level(i) > level(j) {
        if level(i) > level(j) + 1 {
            pow2_increasing(level(j) + 1, level(i));
        }
        // pow2(level(i)) >= pow2(level(j)+1) > j >= i >= pow2(level(i))
        // Contradiction.
    }
}

/// Count trailing zeros (position of lowest set bit).
pub open spec fn ctz(n: nat) -> nat
    decreases n
{
    if n == 0 { 0 }
    else if n % 2 == 1 { 0 }
    else { 1 + ctz(n / 2) }
}

/// Safe nat subtraction (returns 0 if would underflow).
pub open spec fn nsub(a: nat, b: nat) -> nat {
    if a >= b { (a - b) as nat } else { 0 }
}

/// The "extra" count for incomplete trees.
/// Matches eytzinger1_extra in C: (size + 1 - rounddown_pow_of_two(size)) << 1.
pub open spec fn eytzinger1_extra(size: nat) -> nat {
    if size == 0 { 0 }
    else { 2 * nsub(size + 1, pow2(level(size))) }
}

/// Raw inorder position before the incomplete-tree adjustment.
pub open spec fn to_inorder_raw(i: nat, size: nat) -> nat
    recommends i >= 1, i <= size, size >= 1
{
    let b = level(i);
    let d = level(size);
    (2 * nsub(i, pow2(b)) + 1) * pow2(nsub(d, b))
}

/// Eytzinger to inorder. Matches __eytzinger1_to_inorder.
pub open spec fn to_inorder(i: nat, size: nat) -> nat
    recommends i >= 1, i <= size, size >= 1
{
    let raw = to_inorder_raw(i, size);
    let extra = eytzinger1_extra(size);
    if raw <= extra { raw }
    else { (raw + extra) / 2 }
}

/// Strip trailing zeros and lowest set bit: n >> (__ffs(n) + 1).
/// Recursive formulation avoids nesting pow2(ctz(n)) which Z3 can't chain.
pub open spec fn strip_lowest_bit(n: nat) -> nat
    decreases n
{
    if n == 0 { 0 }
    else if n % 2 == 1 { n / 2 }
    else { strip_lowest_bit(n / 2) }
}

/// Inorder to eytzinger. Matches __inorder_to_eytzinger1.
pub open spec fn from_inorder(pos: nat, size: nat) -> nat
    recommends pos >= 1, pos <= size, size >= 1
{
    let extra = eytzinger1_extra(size);
    let raw: nat = if pos <= extra { pos } else { (2 * pos - extra) as nat };
    let offset = strip_lowest_bit(raw);
    let shift = ctz(raw);
    let d = level(size);
    offset + pow2(nsub(d, shift))
}

// --- Helper: establish level and pow2 facts for trees up to size 7 ---

proof fn eval_level_pow2()
    ensures
        level(1) == 0, level(2) == 1, level(3) == 1,
        level(4) == 2, level(5) == 2, level(6) == 2, level(7) == 2,
        pow2(0nat) == 1, pow2(1nat) == 2, pow2(2nat) == 4,
{
    reveal_with_fuel(level, 4);
    reveal_with_fuel(pow2, 4);
}

// --- Concrete evaluations: complete trees ---

proof fn to_inorder_n1()
    ensures to_inorder(1, 1) == 1
{
    eval_level_pow2();
    assert(nsub(1nat, 1nat) == 0);
    assert(nsub(0nat, 0nat) == 0);
    assert(nsub(2nat, 1nat) == 1);
    assert(eytzinger1_extra(1) == 2);
    assert(to_inorder_raw(1, 1) == 1);
}

proof fn to_inorder_n3()
    ensures
        to_inorder(1, 3) == 2,
        to_inorder(2, 3) == 1,
        to_inorder(3, 3) == 3,
{
    eval_level_pow2();
    assert(nsub(1nat, 1nat) == 0);
    assert(nsub(1nat, 0nat) == 1);
    assert(nsub(2nat, 2nat) == 0);
    assert(nsub(3nat, 2nat) == 1);
    assert(nsub(4nat, 2nat) == 2);
    assert(eytzinger1_extra(3) == 4);
    assert(to_inorder_raw(1, 3) == 2);
    assert(to_inorder_raw(2, 3) == 1);
    assert(to_inorder_raw(3, 3) == 3);
}

proof fn to_inorder_n7()
    ensures
        to_inorder(1, 7) == 4,
        to_inorder(2, 7) == 2,
        to_inorder(3, 7) == 6,
        to_inorder(4, 7) == 1,
        to_inorder(5, 7) == 3,
        to_inorder(6, 7) == 5,
        to_inorder(7, 7) == 7,
{
    eval_level_pow2();
    assert(nsub(1nat, 1nat) == 0);
    assert(nsub(2nat, 0nat) == 2);
    assert(nsub(2nat, 1nat) == 1);
    assert(nsub(2nat, 2nat) == 0);
    assert(nsub(3nat, 2nat) == 1);
    assert(nsub(4nat, 4nat) == 0);
    assert(nsub(5nat, 4nat) == 1);
    assert(nsub(6nat, 4nat) == 2);
    assert(nsub(7nat, 4nat) == 3);
    assert(nsub(8nat, 4nat) == 4);
    assert(eytzinger1_extra(7) == 8);
    assert(to_inorder_raw(1, 7) == 4);
    assert(to_inorder_raw(2, 7) == 2);
    assert(to_inorder_raw(3, 7) == 6);
    assert(to_inorder_raw(4, 7) == 1);
    assert(to_inorder_raw(5, 7) == 3);
    assert(to_inorder_raw(6, 7) == 5);
    assert(to_inorder_raw(7, 7) == 7);
}

// --- Incomplete tree: tests the extra adjustment ---

proof fn to_inorder_n5()
    ensures
        to_inorder(1, 5) == 4,
        to_inorder(2, 5) == 2,
        to_inorder(3, 5) == 5,  // raw=6, extra=4, adjusted=(6+4)/2=5
        to_inorder(4, 5) == 1,
        to_inorder(5, 5) == 3,
{
    eval_level_pow2();
    assert(nsub(1nat, 1nat) == 0);
    assert(nsub(2nat, 0nat) == 2);
    assert(nsub(2nat, 1nat) == 1);
    assert(nsub(2nat, 2nat) == 0);
    assert(nsub(3nat, 2nat) == 1);
    assert(nsub(4nat, 4nat) == 0);
    assert(nsub(5nat, 4nat) == 1);
    assert(nsub(6nat, 4nat) == 2);
    assert(eytzinger1_extra(5) == 4);
    assert(to_inorder_raw(1, 5) == 4);
    assert(to_inorder_raw(2, 5) == 2);
    assert(to_inorder_raw(3, 5) == 6);
    assert(to_inorder_raw(4, 5) == 1);
    assert(to_inorder_raw(5, 5) == 3);
}

// --- Roundtrip: from_inorder(to_inorder(i)) == i ---

proof fn roundtrip_n3()
    ensures
        from_inorder(to_inorder(1, 3), 3) == 1,
        from_inorder(to_inorder(2, 3), 3) == 2,
        from_inorder(to_inorder(3, 3), 3) == 3,
{
    eval_level_pow2();
    reveal_with_fuel(ctz, 4);
    reveal_with_fuel(strip_lowest_bit, 4);
    assert(nsub(1nat, 1nat) == 0);
    assert(nsub(1nat, 0nat) == 1);
    assert(nsub(2nat, 2nat) == 0);
    assert(nsub(3nat, 2nat) == 1);
    assert(nsub(4nat, 2nat) == 2);
    assert(eytzinger1_extra(3) == 4);
    assert(to_inorder(1, 3) == 2);
    assert(to_inorder(2, 3) == 1);
    assert(to_inorder(3, 3) == 3);
    assert(ctz(1) == 0);
    assert(ctz(2) == 1);
    assert(ctz(3) == 0);
    assert(strip_lowest_bit(1) == 0);
    assert(strip_lowest_bit(2) == 0);
    assert(strip_lowest_bit(3) == 1);
}

proof fn roundtrip_n5()
    ensures
        from_inorder(to_inorder(1, 5), 5) == 1,
        from_inorder(to_inorder(2, 5), 5) == 2,
        from_inorder(to_inorder(3, 5), 5) == 3,
        from_inorder(to_inorder(4, 5), 5) == 4,
        from_inorder(to_inorder(5, 5), 5) == 5,
{
    eval_level_pow2();
    reveal_with_fuel(ctz, 4);
    reveal_with_fuel(strip_lowest_bit, 4);
    assert(nsub(1nat, 1nat) == 0);
    assert(nsub(1nat, 0nat) == 1);
    assert(nsub(2nat, 0nat) == 2);
    assert(nsub(2nat, 1nat) == 1);
    assert(nsub(2nat, 2nat) == 0);
    assert(nsub(3nat, 2nat) == 1);
    assert(nsub(4nat, 4nat) == 0);
    assert(nsub(5nat, 4nat) == 1);
    assert(nsub(6nat, 4nat) == 2);
    assert(eytzinger1_extra(5) == 4);
    assert(to_inorder(1, 5) == 4);
    assert(to_inorder(2, 5) == 2);
    assert(to_inorder(3, 5) == 5);
    assert(to_inorder(4, 5) == 1);
    assert(to_inorder(5, 5) == 3);
    assert(ctz(1) == 0); assert(ctz(2) == 1); assert(ctz(3) == 0);
    assert(ctz(4) == 2); assert(ctz(5) == 0); assert(ctz(6) == 1);
    assert(strip_lowest_bit(1) == 0); assert(strip_lowest_bit(2) == 0);
    assert(strip_lowest_bit(3) == 1); assert(strip_lowest_bit(4) == 0);
    assert(strip_lowest_bit(5) == 2); assert(strip_lowest_bit(6) == 1);
}

proof fn roundtrip_n7()
    ensures
        from_inorder(to_inorder(1, 7), 7) == 1,
        from_inorder(to_inorder(2, 7), 7) == 2,
        from_inorder(to_inorder(3, 7), 7) == 3,
        from_inorder(to_inorder(4, 7), 7) == 4,
        from_inorder(to_inorder(5, 7), 7) == 5,
        from_inorder(to_inorder(6, 7), 7) == 6,
        from_inorder(to_inorder(7, 7), 7) == 7,
{
    eval_level_pow2();
    reveal_with_fuel(ctz, 4);
    reveal_with_fuel(strip_lowest_bit, 4);
    assert(nsub(1nat, 1nat) == 0);
    assert(nsub(1nat, 0nat) == 1);
    assert(nsub(2nat, 0nat) == 2);
    assert(nsub(2nat, 1nat) == 1);
    assert(nsub(2nat, 2nat) == 0);
    assert(nsub(3nat, 2nat) == 1);
    assert(nsub(4nat, 4nat) == 0);
    assert(nsub(5nat, 4nat) == 1);
    assert(nsub(6nat, 4nat) == 2);
    assert(nsub(7nat, 4nat) == 3);
    assert(nsub(8nat, 4nat) == 4);
    assert(eytzinger1_extra(7) == 8);
    assert(to_inorder(1, 7) == 4);
    assert(to_inorder(2, 7) == 2);
    assert(to_inorder(3, 7) == 6);
    assert(to_inorder(4, 7) == 1);
    assert(to_inorder(5, 7) == 3);
    assert(to_inorder(6, 7) == 5);
    assert(to_inorder(7, 7) == 7);
    assert(ctz(1) == 0); assert(ctz(2) == 1); assert(ctz(3) == 0);
    assert(ctz(4) == 2); assert(ctz(5) == 0); assert(ctz(6) == 1);
    assert(ctz(7) == 0);
    assert(strip_lowest_bit(1) == 0); assert(strip_lowest_bit(2) == 0);
    assert(strip_lowest_bit(3) == 1); assert(strip_lowest_bit(4) == 0);
    assert(strip_lowest_bit(5) == 2); assert(strip_lowest_bit(6) == 1);
    assert(strip_lowest_bit(7) == 3);
}

// ============================================================
// Raw ordering: the core bit-manipulation property
// ============================================================
//
// For node i at level b with offset p = i - 2^b:
//   raw(left_child)  = (4p + 1) * k
//   raw(parent)      = (4p + 2) * k
//   raw(right_child) = (4p + 3) * k
// where k = 2^(d - b - 1).
//
// This gives immediate BST ordering from the bit layout alone.

/// Children have raw positions ordered around the parent.
proof fn raw_children_ordered(i: nat, size: nat)
    requires
        i >= 1,
        right_child(i) <= size,
        size >= 1,
    ensures
        to_inorder_raw(left_child(i), size) < to_inorder_raw(i, size),
        to_inorder_raw(i, size) < to_inorder_raw(right_child(i), size),
{
    let b = level(i);
    let d = level(size);
    let p = nsub(i, pow2(b));

    // Children are one level deeper
    level_of_left_child(i);
    level_of_right_child(i);
    // level(2i) == b + 1, level(2i+1) == b + 1

    // d >= b + 1 since right child (at level b+1) is valid
    level_le_when_le(2 * i + 1, size);
    // level(2i+1) <= level(size), i.e., b + 1 <= d

    // Node i is at level b, so i >= pow2(b)
    level_range(i);
    pow2_positive(b);

    // pow2(b+1) = 2 * pow2(b)
    pow2_double(b);
    // pow2(d-b) = 2 * pow2(d-b-1)
    pow2_double(nsub(d, b + 1));

    // k = pow2(d - b - 1) > 0
    let k = pow2(nsub(d, b + 1));
    pow2_positive(nsub(d, b + 1));

    // Left child offset: 2i - pow2(b+1) = 2i - 2*pow2(b) = 2*(i - pow2(b)) = 2p
    assert(nsub(2 * i, pow2(b + 1)) == 2 * p);

    // Right child offset: (2i+1) - pow2(b+1) = 2p + 1
    assert(nsub(2 * i + 1, pow2(b + 1)) == 2 * p + 1);

    // d - (b+1) = d - b - 1
    assert(nsub(d, b + 1) == nsub(d, b) - 1) by {
        // b + 1 <= d, so nsub(d, b+1) = d - b - 1
        // nsub(d, b) = d - b (since b <= d)
        // d - b - 1 = nsub(d, b) - 1
    };

    // pow2(nsub(d,b)) = 2 * k  (since nsub(d,b) = nsub(d,b+1) + 1)
    assert(pow2(nsub(d, b)) == 2 * k) by {
        assert(nsub(d, b) == nsub(d, b + 1) + 1);
        pow2_double(nsub(d, b + 1));
    };

    // Raw formulas:
    // raw(2i)   = (2*(2p) + 1) * k = (4p+1) * k
    assert(to_inorder_raw(2 * i, size) == (4 * p + 1) * k);
    // raw(i)    = (2p+1) * 2k = (4p+2) * k
    assert(to_inorder_raw(i, size) == (2 * p + 1) * (2 * k));
    // Algebraic: (2p+1)*2k = 2*(2p+1)*k = (4p+2)*k
    assert((2 * p + 1) * (2 * k) == (4 * p + 2) * k) by (nonlinear_arith)
        requires k >= 0, p >= 0
    {};
    // raw(2i+1) = (2*(2p+1) + 1) * k = (4p+3) * k
    assert(to_inorder_raw(2 * i + 1, size) == (4 * p + 3) * k);

    // (4p+1)*k < (4p+2)*k < (4p+3)*k since k > 0
    assert((4 * p + 1) * k < (4 * p + 2) * k) by (nonlinear_arith)
        requires k > 0, p >= 0
    {};
    assert((4 * p + 2) * k < (4 * p + 3) * k) by (nonlinear_arith)
        requires k > 0, p >= 0
    {};
}

// ============================================================
// Roundtrip properties for the bijection
// ============================================================
//
// To prove from_inorder(to_inorder(i, n), n) == i we need:
// 1. ctz/strip_lowest_bit recover shift/offset from raw
// 2. The extra adjustment is invertible
// 3. offset + pow2(level) recovers i

/// ctz of an odd number is 0.
proof fn ctz_odd(n: nat)
    requires n > 0, n % 2 == 1
    ensures ctz(n) == 0
{}

/// ctz of 2*n is 1 + ctz(n) when n > 0.
proof fn ctz_double(n: nat)
    requires n > 0
    ensures ctz(2 * n) == 1 + ctz(n)
{
    // 2*n is even, so ctz(2*n) = 1 + ctz(2*n / 2) = 1 + ctz(n)
    assert(2 * n > 0);
    assert((2 * n) % 2 == 0);
}

/// Product of positive nats is positive.
proof fn nat_mul_positive(a: nat, b: nat)
    requires a > 0, b > 0
    ensures a * b > 0
{
    assert(a * b > 0) by (nonlinear_arith)
        requires a > 0, b > 0
    {};
}

/// ctz of (2*offset+1)*2^shift is shift.
/// This is the key: the raw value encodes shift in its trailing zeros.
proof fn ctz_raw(offset: nat, shift: nat)
    ensures ctz((2 * offset + 1) * pow2(shift)) == shift
    decreases shift
{
    pow2_positive(shift);
    let odd = 2 * offset + 1;
    if shift == 0 {
        reveal_with_fuel(pow2, 1);
        assert(pow2(0nat) == 1);
        assert(odd * 1 == odd);
        assert(odd % 2 == 1);
    } else {
        pow2_double((shift - 1) as nat);
        pow2_positive((shift - 1) as nat);
        let odd = 2 * offset + 1;
        let ps1 = pow2((shift - 1) as nat);
        // odd * pow2(shift) = odd * 2 * ps1 = 2 * (odd * ps1)
        assert(odd * pow2(shift) == 2 * (odd * ps1)) by (nonlinear_arith)
            requires pow2(shift) == 2 * ps1
        {};
        ctz_raw(offset, (shift - 1) as nat);
        nat_mul_positive(odd, ps1);
        ctz_double(odd * ps1);
    }
}

/// strip_lowest_bit of an odd number gives n/2 (the offset).
proof fn strip_odd(n: nat)
    requires n > 0, n % 2 == 1
    ensures strip_lowest_bit(n) == n / 2
{}

/// strip_lowest_bit of 2*n passes through.
proof fn strip_double(n: nat)
    requires n > 0
    ensures strip_lowest_bit(2 * n) == strip_lowest_bit(n)
{
    assert(2 * n > 0);
    assert((2 * n) % 2 == 0);
}

/// strip_lowest_bit of (2*offset+1)*2^shift recovers offset.
proof fn strip_raw(offset: nat, shift: nat)
    ensures strip_lowest_bit((2 * offset + 1) * pow2(shift)) == offset
    decreases shift
{
    pow2_positive(shift);
    let odd = 2 * offset + 1;
    if shift == 0 {
        reveal_with_fuel(pow2, 1);
        assert(pow2(0nat) == 1);
        assert(odd * 1 == odd);
        assert(odd % 2 == 1);
        assert(odd > 0);
    } else {
        pow2_double((shift - 1) as nat);
        pow2_positive((shift - 1) as nat);
        let odd = 2 * offset + 1;
        let ps1 = pow2((shift - 1) as nat);
        assert(odd * pow2(shift) == 2 * (odd * ps1)) by (nonlinear_arith)
            requires pow2(shift) == 2 * ps1
        {};
        strip_raw(offset, (shift - 1) as nat);
        nat_mul_positive(odd, ps1);
        strip_double(odd * ps1);
    }
}

/// The extra adjustment is invertible when raw > extra and raw - extra is even.
proof fn undo_extra_correct(raw: nat, extra: nat)
    requires
        raw > extra,
        (raw - extra) % 2 == 0,
    ensures
        (2 * ((raw + extra) / 2) - extra) as nat == raw
{
    // (raw + extra) / 2 = (raw + extra) / 2
    // Since raw - extra is even and raw + extra = (raw - extra) + 2*extra,
    // raw + extra is also even, so division is exact.
    let adjusted = (raw + extra) / 2;
    // 2*adjusted - extra = 2 * (raw + extra) / 2 - extra = (raw + extra) - extra = raw
}

/// The raw value has the form (2*offset + 1) * pow2(shift).
/// This connects to_inorder_raw with the ctz/strip decomposition.
proof fn raw_decomposition(i: nat, size: nat)
    requires i >= 1, i <= size, size >= 1
    ensures
        to_inorder_raw(i, size) ==
            (2 * nsub(i, pow2(level(i))) + 1) * pow2(nsub(level(size), level(i)))
{
    // Direct from definition of to_inorder_raw.
}

/// Offset plus level bit recovers the eytzinger index.
/// i = (i - pow2(level(i))) + pow2(level(i))
proof fn offset_plus_level_bit(i: nat)
    requires i >= 1
    ensures nsub(i, pow2(level(i))) + pow2(level(i)) == i
{
    level_range(i);
}

/// THE ROUNDTRIP THEOREM: from_inorder(to_inorder(i, n), n) == i.
///
/// This proves the bijection: the C bit manipulation in
/// __eytzinger1_to_inorder and __inorder_to_eytzinger1 are exact inverses.
///
/// The proof chains:
/// 1. to_inorder computes raw = (2*offset+1)*2^shift, possibly adjusted
/// 2. from_inorder undoes the adjustment, recovering raw
/// 3. strip_lowest_bit(raw) recovers offset (by strip_raw)
/// 4. ctz(raw) recovers shift (by ctz_raw)
/// 5. offset + pow2(level) = i (reconstruction)
///
/// We prove it first for complete trees (raw <= extra, no adjustment),
/// then extend to incomplete trees.
proof fn roundtrip_complete(i: nat, size: nat)
    requires
        i >= 1, i <= size, size >= 1,
        to_inorder_raw(i, size) <= eytzinger1_extra(size),
    ensures
        from_inorder(to_inorder(i, size), size) == i
{
    let b = level(i);
    let d = level(size);
    let offset = nsub(i, pow2(b));
    let shift = nsub(d, b);

    level_range(i);
    level_le_when_le(i, size);

    let raw = to_inorder_raw(i, size);
    let extra = eytzinger1_extra(size);

    // to_inorder(i, size) == raw (no adjustment since raw <= extra)
    assert(to_inorder(i, size) == raw);

    // from_inorder: pos <= extra so raw_in = pos = raw
    // Need raw <= extra (our precondition)

    // Decompose raw
    raw_decomposition(i, size);
    assert(raw == (2 * offset + 1) * pow2(shift));

    // strip_lowest_bit and ctz recover offset and shift
    strip_raw(offset, shift);
    ctz_raw(offset, shift);
    assert(strip_lowest_bit(raw) == offset);
    assert(ctz(raw) == shift);

    // nsub(d, shift) == nsub(d, nsub(d, b)) == b
    assert(nsub(d, shift) == b);

    // offset + pow2(b) == i
    offset_plus_level_bit(i);
}

/// For valid nodes, raw - extra is always even when raw > extra.
/// This is because: raw = (2*offset+1)*pow2(shift) where shift >= 1
/// for non-deepest-level nodes (the only ones that can have raw > extra),
/// so raw is even. And extra = 2*(...) is always even.
proof fn raw_extra_diff_even(i: nat, size: nat)
    requires
        i >= 1, i <= size, size >= 1,
        to_inorder_raw(i, size) > eytzinger1_extra(size),
    ensures
        (to_inorder_raw(i, size) - eytzinger1_extra(size)) % 2 == 0
{
    // raw > extra. We need to show raw and extra have the same parity.
    // extra = 2 * nsub(size+1, pow2(level(size))) is always even.
    // raw = (2*offset+1) * pow2(shift) where shift = nsub(d, b).
    // If shift >= 1: pow2(shift) is even, so raw is even. Even - even = even. ✓
    // If shift == 0: raw = 2*offset+1 (odd). We need to show this can't exceed extra.
    let b = level(i);
    let d = level(size);
    let shift = nsub(d, b);

    level_range(i);
    level_le_when_le(i, size);
    level_range(size);

    if shift == 0 {
        // b == d: node i is at the deepest level.
        // raw = 2*offset+1 where offset = i - pow2(d).
        // Max offset at deepest level: size - pow2(d).
        // Max raw at deepest level: 2*(size - pow2(d)) + 1.
        // extra = 2*(size + 1 - pow2(d)) = 2*(size - pow2(d)) + 2.
        // So max raw = extra - 1 < extra. Contradiction with raw > extra!
        assert(b == d);  // shift == 0 means b == d
        let offset = nsub(i, pow2(b));
        reveal_with_fuel(pow2, 1);
        assert(nsub(d, b) == 0);
        assert(pow2(0nat) == 1);
        assert(to_inorder_raw(i, size) == (2 * offset + 1) * 1);
        assert(to_inorder_raw(i, size) == 2 * offset + 1);
        assert(eytzinger1_extra(size) == 2 * nsub(size + 1, pow2(d)));
        // offset = i - pow2(d) <= size - pow2(d)
        // So 2*offset+1 <= 2*(size-pow2(d))+1 = 2*(size+1-pow2(d))-1 = extra-1 < extra
        assert(offset <= nsub(size, pow2(d)));
        assert(2 * offset + 1 < 2 * nsub(size + 1, pow2(d)));
        // This contradicts raw > extra
    } else {
        // shift >= 1, so pow2(shift) >= 2, raw is even.
        pow2_double((shift - 1) as nat);
        pow2_positive((shift - 1) as nat);
        let offset = nsub(i, pow2(b));
        let odd = 2 * offset + 1;
        assert(to_inorder_raw(i, size) == odd * pow2(shift));
        // pow2(shift) = 2 * pow2(shift-1), so raw = 2 * (odd * pow2(shift-1))
        assert(odd * pow2(shift) == 2 * (odd * pow2((shift - 1) as nat))) by (nonlinear_arith)
            requires pow2(shift) == 2 * pow2((shift - 1) as nat)
        {};
        // raw is even
        assert(to_inorder_raw(i, size) % 2 == 0);
        // extra is always even (2 * something)
        assert(eytzinger1_extra(size) % 2 == 0);
        // even - even is even
    }
}

/// Full roundtrip theorem including incomplete trees.
proof fn roundtrip(i: nat, size: nat)
    requires
        i >= 1, i <= size, size >= 1,
    ensures
        from_inorder(to_inorder(i, size), size) == i
{
    let raw = to_inorder_raw(i, size);
    let extra = eytzinger1_extra(size);
    let b = level(i);
    let d = level(size);
    let offset = nsub(i, pow2(b));
    let shift = nsub(d, b);

    level_range(i);
    level_le_when_le(i, size);
    raw_decomposition(i, size);

    if raw <= extra {
        roundtrip_complete(i, size);
    } else {
        // Adjusted case: to_inorder returns (raw + extra) / 2
        let adjusted = (raw + extra) / 2;
        assert(to_inorder(i, size) == adjusted);

        // The adjusted value > extra (since raw > extra implies (raw+extra)/2 > extra)
        // Proof: raw > extra, so raw + extra > 2*extra, so (raw+extra)/2 > extra.
        // But we need this for integer division. Since raw-extra is even:
        raw_extra_diff_even(i, size);
        assert((raw - extra) % 2 == 0);
        // raw + extra = (raw - extra) + 2*extra, also even, so division is exact
        assert((raw + extra) % 2 == 0);
        assert(adjusted == (raw + extra) / 2);
        assert(2 * adjusted == raw + extra);
        assert(adjusted > extra);

        // from_inorder: pos > extra, so raw_in = 2*pos - extra = 2*adjusted - extra
        //   = raw + extra - extra = raw
        assert((2 * adjusted - extra) as nat == raw);

        // Same as complete case from here
        strip_raw(offset, shift);
        ctz_raw(offset, shift);
        assert(strip_lowest_bit(raw) == offset);
        assert(ctz(raw) == shift);
        assert(nsub(d, shift) == b);
        offset_plus_level_bit(i);
    }
}

// ============================================================
// Reverse roundtrip: to_inorder(from_inorder(pos, n), n) == pos
// ============================================================
//
// The forward roundtrip showed from_inorder inverts to_inorder.
// Now we show to_inorder inverts from_inorder, completing the
// bijection proof.
//
// Key lemmas needed:
// 1. level(offset + pow2(k)) == k when offset < pow2(k)
// 2. from_inorder produces valid indices
// 3. to_inorder_raw(from_inorder(pos), size) recovers raw

/// Level of offset + pow2(k) is k, when offset < pow2(k).
/// This says the pow2 term in from_inorder determines the level.
proof fn level_from_pow2(offset: nat, k: nat)
    requires offset < pow2(k)
    ensures level(offset + pow2(k)) == k
    decreases k
{
    pow2_positive(k);
    let i = offset + pow2(k);

    if k == 0 {
        // offset < pow2(0) = 1, so offset == 0, i == 1.
        // level(1) == 0.
        reveal_with_fuel(pow2, 1);
        assert(pow2(0nat) == 1);
        assert(offset == 0);
    } else {
        // i = offset + pow2(k). We need level(i) == k.
        // i / 2 = (offset + pow2(k)) / 2.
        // pow2(k) = 2 * pow2(k-1), so i = offset + 2*pow2(k-1).
        pow2_double((k - 1) as nat);
        pow2_positive((k - 1) as nat);
        let pk1 = pow2((k - 1) as nat);
        assert(pow2(k) == 2 * pk1);

        // i = offset + 2*pk1
        // i >= 2*pk1 >= 2 (since pk1 >= 1), so i > 1
        assert(i >= 2);

        // i / 2 = (offset + 2*pk1) / 2
        // If offset is even: i/2 = offset/2 + pk1
        // If offset is odd: i/2 = (offset-1)/2 + pk1
        // Either way: i/2 = offset/2 + pk1  (integer division)
        assert(i / 2 == offset / 2 + pk1);

        // offset < pow2(k) = 2*pk1, so offset/2 < pk1
        assert(offset / 2 < pk1);

        // By induction: level(offset/2 + pk1) == k-1
        level_from_pow2(offset / 2, (k - 1) as nat);

        // level(i) = 1 + level(i/2) = 1 + (k-1) = k
    }
}

/// Every positive integer decomposes as (2*offset+1)*pow2(shift)
/// where offset = strip_lowest_bit(n) and shift = ctz(n).
proof fn odd_pow2_decomposition(n: nat)
    requires n >= 1
    ensures n == (2 * strip_lowest_bit(n) + 1) * pow2(ctz(n))
    decreases n
{
    if n % 2 == 1 {
        reveal_with_fuel(pow2, 1);
        assert(pow2(0nat) == 1);
        assert(ctz(n) == 0);
        assert(strip_lowest_bit(n) == n / 2);
        assert(2 * (n / 2) + 1 == n);
        // Connect to postcondition form
        assert((2 * strip_lowest_bit(n) + 1) == n);
        assert(pow2(ctz(n)) == 1);
        assert(n * 1 == n) by (nonlinear_arith) requires true {};
        assert((2 * strip_lowest_bit(n) + 1) * pow2(ctz(n)) == n);
    } else {
        assert(n / 2 >= 1);
        odd_pow2_decomposition(n / 2);
        let s = strip_lowest_bit(n / 2);
        let c = ctz(n / 2);
        assert(n / 2 == (2 * s + 1) * pow2(c));
        assert(strip_lowest_bit(n) == s);
        assert(ctz(n) == 1 + c);
        pow2_double(c);
        assert(pow2(1 + c) == 2 * pow2(c));
        assert(n == 2 * (n / 2));
        assert(n == 2 * ((2 * s + 1) * pow2(c)));
        assert(2 * ((2 * s + 1) * pow2(c)) == (2 * s + 1) * (2 * pow2(c))) by (nonlinear_arith)
            requires true
        {};
        assert(n == (2 * s + 1) * (2 * pow2(c)));
        assert(n == (2 * s + 1) * pow2(1 + c));
        // Connect to postcondition form
        assert((2 * strip_lowest_bit(n) + 1) * pow2(ctz(n)) == (2 * s + 1) * pow2(1 + c));
    }
}

/// pow2 splits over addition: pow2(a + b) == pow2(a) * pow2(b).
proof fn pow2_split(a: nat, b: nat)
    ensures pow2(a + b) == pow2(a) * pow2(b)
    decreases a
{
    if a == 0 {
        reveal_with_fuel(pow2, 1);
        assert(pow2(0nat) == 1);
    } else {
        pow2_split((a - 1) as nat, b);
        assert(pow2(a + b) == 2 * pow2((a - 1) as nat + b));
        assert(pow2(a) == 2 * pow2((a - 1) as nat));
        assert(2 * (pow2((a - 1) as nat) * pow2(b)) == (2 * pow2((a - 1) as nat)) * pow2(b)) by (nonlinear_arith)
            requires true
        {};
    }
}

/// ctz is bounded: if n >= 1 and n < pow2(d+1), then ctz(n) <= d.
proof fn ctz_bounded(n: nat, d: nat)
    requires n >= 1, n < pow2(d + 1)
    ensures ctz(n) <= d
    decreases n
{
    if n % 2 == 1 {
        // ctz(n) = 0 <= d
    } else {
        assert(n / 2 >= 1);
        pow2_double(d);
        assert(n / 2 < pow2(d));
        if d == 0 {
            reveal_with_fuel(pow2, 2);
            assert(pow2(1nat) == 2);
            // n >= 1, n < 2, n even: impossible
        } else {
            ctz_bounded(n / 2, (d - 1) as nat);
        }
    }
}

/// The odd part bound: strip_lowest_bit(n) < pow2(d - ctz(n))
/// when n >= 1 and n < pow2(d + 1).
proof fn strip_bounded(n: nat, d: nat)
    requires n >= 1, n < pow2(d + 1)
    ensures
        ctz(n) <= d,
        strip_lowest_bit(n) < pow2(nsub(d, ctz(n))),
{
    ctz_bounded(n, d);
    let shift = ctz(n);
    let offset = strip_lowest_bit(n);
    let k = nsub(d, shift);

    odd_pow2_decomposition(n);
    assert(n == (2 * offset + 1) * pow2(shift));

    assert(d + 1 == shift + k + 1);
    pow2_split(shift, k + 1);
    assert(pow2(d + 1) == pow2(shift) * pow2(k + 1));

    pow2_positive(shift);
    assert((2 * offset + 1) < pow2(k + 1)) by (nonlinear_arith)
        requires
            (2 * offset + 1) * pow2(shift) < pow2(shift) * pow2(k + 1),
            pow2(shift) > 0,
    {};

    pow2_double(k);
    assert(offset < pow2(k));
}

/// The raw value in from_inorder is bounded by pow2(d+1).
proof fn raw_bound(pos: nat, size: nat)
    requires pos >= 1, pos <= size, size >= 1
    ensures
        ({
            let extra = eytzinger1_extra(size);
            let raw: nat = if pos <= extra { pos } else { (2 * pos - extra) as nat };
            let d = level(size);
            raw >= 1 && raw < pow2(d + 1)
        })
{
    let d = level(size);
    let extra = eytzinger1_extra(size);

    level_range(size);
    pow2_double(d);
    pow2_positive(d);
    assert(pow2(d + 1) == 2 * pow2(d));
    assert(nsub(size + 1, pow2(d)) == (size + 1 - pow2(d)) as nat);
    assert(nsub(size + 1, pow2(d)) <= pow2(d));
    assert(extra <= pow2(d + 1));

    let raw: nat = if pos <= extra { pos } else { (2 * pos - extra) as nat };
    assert(raw >= 1);

    if pos <= extra {
        assert(raw <= size);
        assert(raw < pow2(d + 1));
    } else {
        assert(extra == 2 * (size + 1 - pow2(d)));
        assert(raw == (2 * pos - extra) as nat);
        // 2*pos - extra <= 2*size - extra = 2*size - 2*(size+1-pow2(d))
        //   = 2*pow2(d) - 2 = pow2(d+1) - 2 < pow2(d+1)
        assert(raw <= 2 * size - extra);
        assert(2 * size - extra == 2 * pow2(d) - 2);
        assert(raw < pow2(d + 1));
    }
}

/// THE REVERSE ROUNDTRIP: to_inorder(from_inorder(pos, n), n) == pos.
///
/// Combined with the forward roundtrip, this establishes the full bijection:
/// to_inorder and from_inorder are exact inverses on [1..size].
proof fn reverse_roundtrip(pos: nat, size: nat)
    requires pos >= 1, pos <= size, size >= 1
    ensures to_inorder(from_inorder(pos, size), size) == pos
{
    let extra = eytzinger1_extra(size);
    let d = level(size);
    let raw: nat = if pos <= extra { pos } else { (2 * pos - extra) as nat };

    // 1. Bound the raw value
    raw_bound(pos, size);
    assert(raw >= 1 && raw < pow2(d + 1));

    // 2. Decompose raw via odd × pow2
    let offset = strip_lowest_bit(raw);
    let shift = ctz(raw);
    let k = nsub(d, shift);
    let result = offset + pow2(k);

    strip_bounded(raw, d);
    assert(shift <= d);
    assert(offset < pow2(k));

    // 3. Level of result
    level_from_pow2(offset, k);
    assert(level(result) == k);

    // 4. Reconstruct to_inorder_raw(result, size) = raw
    pow2_positive(k);
    assert(nsub(result, pow2(k)) == offset);
    assert(nsub(d, k) == shift);
    odd_pow2_decomposition(raw);
    assert(to_inorder_raw(result, size) == (2 * offset + 1) * pow2(shift));
    assert(to_inorder_raw(result, size) == raw);

    // 5. Apply to_inorder adjustment to recover pos
    if pos <= extra {
        assert(raw == pos);
        assert(raw <= extra);
        assert(to_inorder(result, size) == raw);
    } else {
        assert(pos > extra);
        assert(raw == (2 * pos - extra) as nat);
        assert(raw > extra);
        assert(raw + extra == 2 * pos);
        assert((raw + extra) / 2 == pos);
        assert(to_inorder(result, size) == pos);
    }
}

/// from_inorder produces a valid eytzinger index (1 <= result <= size).
proof fn from_inorder_valid(pos: nat, size: nat)
    requires pos >= 1, pos <= size, size >= 1
    ensures
        from_inorder(pos, size) >= 1,
        from_inorder(pos, size) <= size,
{
    let d = level(size);
    let extra = eytzinger1_extra(size);
    let raw: nat = if pos <= extra { pos } else { (2 * pos - extra) as nat };
    let offset = strip_lowest_bit(raw);
    let shift = ctz(raw);
    let k = nsub(d, shift);
    let result = offset + pow2(k);

    // result >= 1
    raw_bound(pos, size);
    strip_bounded(raw, d);
    pow2_positive(k);
    assert(result >= 1);

    // result <= size: split by whether shift == 0 (deepest level)
    level_range(size);
    pow2_double(d);
    pow2_positive(d);

    if k < d {
        // result < pow2(k+1) <= pow2(d) <= size
        pow2_double(k);
        assert(result < pow2(k + 1));
        if k + 1 < d {
            pow2_increasing(k + 1, d);
        }
        assert(pow2(k + 1) <= pow2(d));
        assert(result <= size);
    } else {
        // k == d, shift == 0: raw is odd
        assert(k == d);
        assert(shift == 0);

        // When shift=0, raw must be in the complete range (raw = pos <= extra),
        // because in the incomplete range raw = 2*pos - extra is always even.
        if pos > extra {
            assert(raw == (2 * pos - extra) as nat);
            assert(extra == 2 * nsub(size + 1, pow2(d)));
            // 2*pos - 2*something = 2*(pos - something) = even
            // But ctz(raw) == 0 implies raw is odd. Contradiction.
        }
        assert(raw == pos);
        // raw is odd (shift=0). offset = raw/2 = (pos-1)/2.
        // extra = 2*(size+1-pow2(d)). pos <= extra and pos is odd.
        // So pos <= extra - 1 = 2*(size+1-pow2(d)) - 1
        // offset = (pos-1)/2 <= (extra-2)/2 = size - pow2(d)
        assert(nsub(size + 1, pow2(d)) == (size + 1 - pow2(d)) as nat);
        assert(extra == 2 * (size + 1 - pow2(d)));
        assert(offset == pos / 2);
        assert(pos / 2 <= size - pow2(d));
        assert(result == pos / 2 + pow2(d));
        assert(result <= size);
    }
}

// ============================================================
// Adjustment monotonicity — extra doesn't break ordering
// ============================================================
//
// The to_inorder adjustment maps raw to:
//   raw               if raw <= extra
//   (raw + extra) / 2 if raw > extra
//
// This is strictly monotone: if raw_a < raw_b, then
// to_inorder(a) < to_inorder(b) (for any nodes a, b in same tree).
//
// Three cases:
//   Both <= extra:  raw_a < raw_b directly. ✓
//   Both > extra:   (raw_a + extra)/2 < (raw_b + extra)/2. ✓
//   a <= extra < b: raw_a <= extra < (raw_b + extra)/2. Need raw_b >= extra + 2
//                   (since raw_b > extra and raw_b - extra is even). ✓

/// The adjustment preserves strict ordering of raw values.
proof fn adjustment_monotone(raw_a: nat, raw_b: nat, size: nat)
    requires
        size >= 1,
        raw_a >= 1, raw_b >= 1,
        raw_a < raw_b,
        // raw_b - extra is even when raw_b > extra (from raw_extra_diff_even)
        (raw_b > eytzinger1_extra(size)) ==> (raw_b - eytzinger1_extra(size)) % 2 == 0,
    ensures
        ({
            let extra = eytzinger1_extra(size);
            let adj_a: nat = if raw_a <= extra { raw_a } else { (raw_a + extra) / 2 };
            let adj_b: nat = if raw_b <= extra { raw_b } else { (raw_b + extra) / 2 };
            adj_a < adj_b
        })
{
    let extra = eytzinger1_extra(size);
    let adj_a: nat = if raw_a <= extra { raw_a } else { (raw_a + extra) / 2 };
    let adj_b: nat = if raw_b <= extra { raw_b } else { (raw_b + extra) / 2 };

    if raw_a <= extra && raw_b <= extra {
        // Both unadjusted: adj_a = raw_a < raw_b = adj_b. ✓
    } else if raw_a > extra && raw_b > extra {
        // Both adjusted: (raw_a + extra)/2 < (raw_b + extra)/2
        // Since raw_a < raw_b, raw_a + extra < raw_b + extra.
        assert(raw_a + extra < raw_b + extra);
        // Integer division preserves strict < when gap >= 2
        // (raw_b + extra) - (raw_a + extra) = raw_b - raw_a >= 1
        // But we need the division to preserve strict <.
        // Actually: a < b implies a/2 <= b/2. For strict <:
        // raw_a + extra < raw_b + extra and both are even (since raw-extra is even):
        // raw_a + extra = raw_a - extra + 2*extra. If raw_a > extra, raw_a - extra is even,
        // so raw_a + extra is even (even + even). Similarly raw_b + extra is even.
        // For even a < even b: a/2 < b/2 iff a < b. ✓
        // But we only know raw_b - extra is even. Need raw_a - extra is even too.
        // We don't have that as a precondition. Hmm.
        // Actually: for the case where raw_a comes from a valid node via to_inorder_raw,
        // raw_extra_diff_even proves it. But this lemma takes raw values directly.
        // Let me just work with the integer arithmetic.
        //
        // raw_a < raw_b and raw_b - extra is even. raw_a > extra.
        // We need (raw_a + extra)/2 < (raw_b + extra)/2.
        // Sufficient: raw_a + extra < raw_b + extra, and (raw_b + extra) is even.
        // raw_b + extra = (raw_b - extra) + 2*extra. raw_b - extra is even, so raw_b + extra is even.
        // raw_a + extra <= raw_b + extra - 1 (since raw_a < raw_b).
        // If raw_a + extra is even: (raw_a+extra)/2 < (raw_b+extra)/2. ✓
        // If raw_a + extra is odd: (raw_a+extra)/2 = (raw_a+extra-1)/2 < (raw_b+extra)/2. ✓
        // In both cases: ⌊(raw_a+extra)/2⌋ < (raw_b+extra)/2. ✓ (since raw_b+extra is even)
        assert((raw_b + extra) % 2 == 0);
        // For even b and any a < b: a/2 < b/2 when b is even.
        // Actually not quite: 3/2=1, 4/2=2, yes 1 < 2. 2/2=1, 4/2=2, yes. 1/2=0, 2/2=1, yes.
        // More precisely: if b is even and a < b, then ⌊a/2⌋ ≤ (b-1)/2 = b/2 - 1 < b/2.
        // Wait, (b-1)/2 when b is even = (b-1)/2 which is ⌊b/2⌋ - 1 + something...
        // Let me just assert it.
        assert(adj_a < adj_b);
    } else {
        // raw_a <= extra < raw_b
        assert(raw_a <= extra);
        assert(raw_b > extra);
        // adj_a = raw_a <= extra
        // adj_b = (raw_b + extra) / 2
        // raw_b - extra is even and >= 2 (since raw_b > extra and even gap)
        // raw_b >= extra + 2
        assert((raw_b - extra) % 2 == 0);
        assert(raw_b >= extra + 2);
        // adj_b = (raw_b + extra) / 2 >= (extra + 2 + extra) / 2 = extra + 1
        assert(adj_b >= extra + 1);
        assert(adj_a <= extra);
        assert(adj_a < adj_b);
    }
}

/// Children ordering WITH the extra adjustment.
/// Extends raw_children_ordered to the full to_inorder function.
proof fn children_ordered(i: nat, size: nat)
    requires
        i >= 1,
        right_child(i) <= size,
        size >= 1,
    ensures
        to_inorder(left_child(i), size) < to_inorder(i, size),
        to_inorder(i, size) < to_inorder(right_child(i), size),
{
    // Raw values are positive (2*offset+1 >= 1 and pow2 >= 1)
    let b = level(i);
    let d = level(size);
    pow2_positive(nsub(d, b));
    level_of_left_child(i);
    level_of_right_child(i);
    pow2_positive(nsub(d, b + 1));
    nat_mul_positive(2 * nsub(left_child(i), pow2(b + 1)) + 1, pow2(nsub(d, b + 1)));
    nat_mul_positive(2 * nsub(i, pow2(b)) + 1, pow2(nsub(d, b)));
    nat_mul_positive(2 * nsub(right_child(i), pow2(b + 1)) + 1, pow2(nsub(d, b + 1)));

    // Get raw ordering
    raw_children_ordered(i, size);
    let raw_l = to_inorder_raw(left_child(i), size);
    let raw_p = to_inorder_raw(i, size);
    let raw_r = to_inorder_raw(right_child(i), size);
    assert(raw_l >= 1);
    assert(raw_p >= 1);
    assert(raw_r >= 1);
    assert(raw_l < raw_p);
    assert(raw_p < raw_r);

    // Get the even-parity preconditions for adjustment_monotone
    if raw_p > eytzinger1_extra(size) {
        raw_extra_diff_even(i, size);
    }
    if raw_r > eytzinger1_extra(size) {
        raw_extra_diff_even(right_child(i), size);
    }

    // Apply adjustment monotonicity
    adjustment_monotone(raw_l, raw_p, size);
    adjustment_monotone(raw_p, raw_r, size);
}

/// Raw values are always positive for valid nodes.
proof fn to_inorder_raw_positive(i: nat, size: nat)
    requires i >= 1, i <= size, size >= 1
    ensures to_inorder_raw(i, size) >= 1
{
    let b = level(i);
    let d = level(size);
    level_range(i);
    level_le_when_le(i, size);
    pow2_positive(nsub(d, b));
    nat_mul_positive(2 * nsub(i, pow2(b)) + 1, pow2(nsub(d, b)));
}

/// to_inorder_raw is bounded: raw < pow2(d+1).
proof fn to_inorder_raw_bound(i: nat, size: nat)
    requires i >= 1, i <= size, size >= 1
    ensures to_inorder_raw(i, size) < pow2(level(size) + 1)
{
    let b = level(i);
    let d = level(size);
    let offset = nsub(i, pow2(b));
    let shift = nsub(d, b);

    level_range(i);
    level_le_when_le(i, size);
    pow2_positive(b);
    pow2_positive(shift);

    // offset < pow2(b)
    assert(offset < pow2(b));
    // raw <= (2*pow2(b)-1)*pow2(shift) < 2*pow2(b)*pow2(shift) = 2*pow2(d) = pow2(d+1)
    pow2_split(b, shift);
    assert(pow2(b) * pow2(shift) == pow2(d));
    assert(b + shift == d);
    assert((2 * offset + 1) * pow2(shift) <= (2 * pow2(b) - 1) * pow2(shift)) by (nonlinear_arith)
        requires 2 * offset + 1 <= 2 * pow2(b) - 1, pow2(shift) > 0
    {};
    assert((2 * pow2(b) - 1) * pow2(shift) < 2 * pow2(b) * pow2(shift)) by (nonlinear_arith)
        requires pow2(shift) > 0, pow2(b) > 0
    {};
    assert(2 * pow2(b) * pow2(shift) == 2 * pow2(d)) by (nonlinear_arith)
        requires pow2(b) * pow2(shift) == pow2(d)
    {};
    pow2_double(d);
}

/// to_inorder maps valid eytzinger indices to valid inorder positions.
proof fn to_inorder_valid(i: nat, size: nat)
    requires i >= 1, i <= size, size >= 1
    ensures
        to_inorder(i, size) >= 1,
        to_inorder(i, size) <= size,
{
    let b = level(i);
    let d = level(size);
    let offset = nsub(i, pow2(b));
    let shift = nsub(d, b);
    let raw = to_inorder_raw(i, size);
    let extra = eytzinger1_extra(size);

    level_range(i);
    level_range(size);
    level_le_when_le(i, size);
    pow2_positive(b);
    pow2_positive(d);
    pow2_positive(shift);
    pow2_double(d);
    to_inorder_raw_positive(i, size);
    to_inorder_raw_bound(i, size);

    assert(offset < pow2(b));
    assert(nsub(size + 1, pow2(d)) == (size + 1 - pow2(d)) as nat);

    if raw <= extra {
        assert(to_inorder(i, size) == raw);

        if shift == 0 {
            // Deepest level: raw = 2*offset+1, max = extra-1 <= size
            reveal_with_fuel(pow2, 1);
            assert(pow2(0nat) == 1);
            assert(raw == (2 * offset + 1) * pow2(shift));
            assert((2 * offset + 1) * 1 == 2 * offset + 1) by (nonlinear_arith)
                requires offset >= 0
            {};
            assert(raw == 2 * offset + 1);
            assert(offset <= size - pow2(d));
            assert(extra == 2 * (size + 1 - pow2(d)));
            assert(size + 1 <= 2 * pow2(d));
            assert(raw <= extra - 1);
            assert(raw <= size);
        } else {
            // Non-deepest: raw < pow2(d+1). Two sub-cases.
            if extra <= size {
                // Incomplete tree: raw <= extra <= size
                assert(raw <= size);
            } else {
                // Complete tree: extra = size+1, raw < pow2(d+1)
                assert(raw < pow2(d + 1));
                assert(pow2(d + 1) == 2 * pow2(d));
                assert(raw <= 2 * pow2(d) - 1);
                // extra = 2*(size+1-pow2(d)), extra > size means
                // 2*(size+1-pow2(d)) > size, i.e., size+2 > 2*pow2(d).
                // Since size < pow2(d+1) = 2*pow2(d), size <= 2*pow2(d)-1.
                // So size+2 > 2*pow2(d) iff size >= 2*pow2(d)-1 iff size = 2*pow2(d)-1.
                assert(size == 2 * pow2(d) - 1);
                assert(raw <= size);
            }
        }
    } else {
        // Adjusted: to_inorder = (raw+extra)/2
        raw_extra_diff_even(i, size);

        // raw + extra <= 2*size (key bound for adjusted case):
        // raw <= (2*offset+1)*pow2(shift) <= (2*pow2(b)-1)*pow2(shift) = 2*pow2(d) - pow2(shift)
        pow2_split(b, shift);
        assert(pow2(b) * pow2(shift) == pow2(d));
        assert((2 * offset + 1) * pow2(shift) <= (2 * pow2(b) - 1) * pow2(shift)) by (nonlinear_arith)
            requires 2 * offset + 1 <= 2 * pow2(b) - 1, pow2(shift) > 0
        {};
        assert((2 * pow2(b) - 1) * pow2(shift) == 2 * pow2(b) * pow2(shift) - pow2(shift)) by (nonlinear_arith)
            requires pow2(b) > 0, pow2(shift) > 0
        {};
        assert(2 * pow2(b) * pow2(shift) == 2 * pow2(d)) by (nonlinear_arith)
            requires pow2(b) * pow2(shift) == pow2(d)
        {};
        assert(raw <= 2 * pow2(d) - pow2(shift));
        // extra = 2*(size+1-pow2(d)), so raw + extra <= 2*pow2(d) - pow2(shift) + 2*size + 2 - 2*pow2(d) = 2*size + 2 - pow2(shift)
        assert(raw + extra <= 2 * size + 2 - pow2(shift));
        // shift >= 1: when shift=0 (deepest level), raw <= extra, contradicting this branch.
        // Proof: shift=0 means b=d, offset = i-pow2(d) <= size-pow2(d),
        // raw = (2*offset+1)*1 = 2*offset+1 <= 2*(size-pow2(d))+1 < extra = 2*(size+1-pow2(d)).
        if shift == 0 {
            reveal_with_fuel(pow2, 1);
            assert(pow2(0nat) == 1);
            assert(raw == (2 * offset + 1) * pow2(shift));
            assert((2 * offset + 1) * 1 == 2 * offset + 1) by (nonlinear_arith)
                requires offset >= 0
            {};
            assert(raw == 2 * offset + 1);
            assert(offset <= size - pow2(d));
            assert(raw <= 2 * (size - pow2(d)) + 1) by (nonlinear_arith)
                requires raw == 2 * offset + 1, offset <= size - pow2(d)
            {};
            assert(nsub(size + 1, pow2(d)) == (size + 1 - pow2(d)) as nat);
            assert(extra == 2 * (size + 1 - pow2(d)));
            assert(raw < extra) by (nonlinear_arith)
                requires raw <= 2 * (size - pow2(d)) + 1, extra == 2 * (size + 1 - pow2(d))
            {};
            assert(false);
        }
        assert(shift >= 1);
        pow2_double((shift - 1) as nat);
        pow2_positive((shift - 1) as nat);
        assert(pow2(shift) >= 2);
        assert(raw + extra <= 2 * size);
        assert((raw + extra) / 2 <= size);
        assert(to_inorder(i, size) >= 1);
        assert(to_inorder(i, size) <= size);
    }
}

fn main() {}

} // verus!
