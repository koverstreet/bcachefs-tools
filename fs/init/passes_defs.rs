// SPDX-License-Identifier: GPL-2.0

//! The data types of init/passes_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use cstruct_macros::{c_enum, c_verbatim};

c_enum! {
    #[flags]
    pub enum bch_run_recovery_pass_flags: u32 {
        RUN_RECOVERY_PASS_ratelimit = 1 << 0,
        /*
         * Schedule in memory only, without taking sb_lock, so it's safe from
         * contexts that hold btree locks (e.g. triggers): the schedule touches
         * only in-memory recovery state and never writes the superblock. The
         * need is re-derivable, so persistence isn't required.
         */
        RUN_RECOVERY_PASS_ephemeral = 1 << 1,
        /*
         * Don't schedule if the pass already completed successfully this
         * instance: for callers that schedule cleanup passes on encountering
         * damage those passes might not fix. If the pass ran and the damage is
         * still here, rescheduling can't help - it just re-arms the pass in the
         * superblock on every encounter, forcing fsck on every subsequent mount.
         */
        RUN_RECOVERY_PASS_skip_if_complete = 1 << 2,
    }
}

c_verbatim!(r#"
struct sb_write;
"#);
