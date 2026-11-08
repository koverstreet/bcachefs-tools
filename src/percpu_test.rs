//! Rust `#[test]` wrapper for the percpu shim's slot lifetime test.
//!
//! The assertions live in C (`c_src/percpu_test.c`) so they can use the percpu
//! macros directly. This just invokes the C runner and reports the failure
//! count, so it runs under `cargo test` with no kernel.

unsafe extern "C" {
    fn rust_percpu_test() -> std::ffi::c_int;
}

#[test]
fn percpu_slot_lifetime() {
    // SAFETY: rust_percpu_test() allocates and frees its own percpu counters
    // and joins every thread it starts before returning.
    let fails = unsafe { rust_percpu_test() };
    assert_eq!(
        fails, 0,
        "rust_percpu_test reported {fails} failed assertion(s); see stderr"
    );
}
