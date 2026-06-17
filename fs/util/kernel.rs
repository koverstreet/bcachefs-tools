use crate::c;

pub fn random_u64_below(ceil: u64) -> u64 {
    assert!(ceil > 0);
    unsafe { c::bch2_get_random_u64_below(ceil) }
}

// local_clock() is a static inline and cond_resched() is a macro, so neither
// binds through bindgen directly; util.h wraps both as allowlisted bch2_*
// static inlines that the codegen picks up uniformly on the kernel and
// userspace builds.
pub fn local_clock() -> u64 {
    unsafe { c::bch2_local_clock() }
}

pub fn cond_resched() {
    unsafe { c::bch2_cond_resched() };
}
