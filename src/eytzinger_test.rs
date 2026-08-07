//! Rust `#[test]` wrapper for the eytzinger sort/search unit test.
//!
//! The assertions live in C (`c_src/eytzinger_test.c`) so they can reach the
//! macros and static-inlines directly - `eytzinger1_find` and the
//! `darray_eytzinger1_*` wrappers that `snapshot_id_dying()` uses aren't
//! visible through bindgen. This just invokes the C runner and reports the
//! failure count, so the whole thing runs under `cargo test` with no kernel.

#[test]
fn eytzinger_sort_find() {
    // SAFETY: rust_eytzinger_test() is self-contained - it allocates and frees
    // its own memory and touches no global state.
    let fails = unsafe { bch_bindgen::c::rust_eytzinger_test() };
    assert_eq!(
        fails, 0,
        "rust_eytzinger_test reported {fails} failed assertion(s); see stderr"
    );
}
