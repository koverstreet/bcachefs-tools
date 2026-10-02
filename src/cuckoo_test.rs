//! Rust `#[test]` wrapper for the cuckoo u64 set unit test.
//!
//! The assertions live in C (`c_src/cuckoo_test.c`) so they can reach the
//! static inlines directly. This just invokes the C runner and reports the
//! failure count, so it runs under `cargo test` with no kernel.

#[test]
fn cuckoo_u64_set() {
    // SAFETY: rust_cuckoo_test() is self-contained - it allocates and frees
    // its own memory and touches no global state.
    let fails = unsafe { bch_bindgen::c::rust_cuckoo_test() };
    assert_eq!(
        fails, 0,
        "rust_cuckoo_test reported {fails} failed assertion(s); see stderr"
    );
}
