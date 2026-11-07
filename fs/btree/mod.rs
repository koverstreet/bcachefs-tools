/// An invariant of a node's - of its format, its keys, its sets: checked in
/// CONFIG_BCACHEFS_DEBUG builds, otherwise assumed, and the checks it implies
/// optimized out - a node that breaks it is then undefined behaviour, as in
/// C. Each is node read's to validate, or the aux trees' builders' to keep,
/// and a precondition for formal verification to prove.
/// Several are checked together, so the optimizer can merge their compares,
/// and assumed one by one - it doesn't split an assumed conjunction. A
/// condition is evaluated before it's assumed: its own arithmetic stays
/// checked, so it compares values the code computes anyway. The conditions
/// are the message, with the caller's location: a message's arguments would
/// cost the fastpaths they're on.
macro_rules! invariant {
    ($($cond:expr),+ $(,)?) => {
        if !cfg!(CONFIG_BCACHEFS_DEBUG) {
            $(unsafe { core::hint::assert_unchecked($cond) };)+
        } else if !($($cond)&&+) {
            $crate::btree::invariant_failed(concat!($(stringify!($cond), "; "),+))
        }
    };
}

#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn invariant_failed(cond: &'static str) -> ! {
    panic!("btree node invariant failed: {cond}")
}

pub mod bbpos;
pub mod bkey;
pub mod bkey_buf;
pub mod bkey_methods;
pub mod cache;
pub mod compat;
pub mod cow_key;
pub mod for_each;
pub mod interior;
pub mod iter;
pub mod journal_overlay;
pub mod lock_graph;
pub mod locking;
pub mod node;
pub mod node_scan;
pub mod path;
pub mod path_node;
pub mod read;
pub mod trans;
pub mod trans_attempt;
pub mod traverse;
pub mod trigger;
pub mod types;
pub mod update;
pub mod write;
pub mod write_buffer;

pub use bkey::BkeySC;
pub use iter::BtreeIter;
pub use iter::BtreeNodeIter;
pub use iter::BtreeTrans;
pub use iter::BtreeIterFlags;
