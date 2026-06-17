use crate::c;

pub fn random_u64_below(ceil: u64) -> u64 {
    assert!(ceil > 0);
    unsafe { c::bch2_get_random_u64_below(ceil) }
}

pub fn sched_clock() -> u64 {
    unsafe { c::sched_clock() }
}

pub fn cond_resched() {
    unsafe { c::cond_resched() };
}
