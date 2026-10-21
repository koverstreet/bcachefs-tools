// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::locking::SeqLock;
use crate::types::rust_only;
use cstruct_macros::CStruct;

/*
 * Lame progress indicators
 *
 * We don't like to use these because they print to the dmesg console, which is
 * spammy - we much prefer to be wired up to a userspace programm (e.g. via
 * thread_with_file) and have it print the progress indicator.
 *
 * But some code is old and doesn't support that, or runs in a context where
 * that's not yet practical (mount).
 *
 * The state is Rust's, under a seqlock: readers - sysfs, the recovery status
 * ioctl - copy it out whole while the work updates it. C embeds one, zeroed,
 * and goes through init/progress.rs's functions.
 */
#[repr(C)]
#[derive(CStruct)]
pub struct progress_indicator {
    pub(crate) state: rust_only!(SeqLock<crate::init::progress::ProgressState>),
}
