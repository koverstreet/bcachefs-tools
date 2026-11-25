// SPDX-License-Identifier: GPL-2.0

//! debug/tests/raid.rs's benchmark of util/raid.rs, in userspace.

fn main() {
    bcachefs_kernel::os::sched::linux_thread_init();
    bcachefs_kernel::debug::tests::raid::bench().expect("raid bench");
}
