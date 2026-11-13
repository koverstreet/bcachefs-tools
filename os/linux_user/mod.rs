// SPDX-License-Identifier: GPL-2.0

// The kernel's, cfg(not(kernel)) inside, until userspace has its own
#[path = "../linux_kernel/file.rs"]
pub mod file;
#[path = "../linux_kernel/kernel.rs"]
pub mod kernel;
#[path = "../linux_kernel/ops.rs"]
pub mod ops;
pub mod thread_with_file;
