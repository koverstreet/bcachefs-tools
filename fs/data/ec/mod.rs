// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

pub mod buf;
pub mod check;
pub mod delete;
pub mod format;
pub mod head;
pub mod io;
pub mod open;
pub mod repair;
pub mod stripe;
pub mod stripe_new;
pub mod stripe_new_blocks;
pub mod stripe_new_reuse;
pub mod stripe_new_tasks;
pub mod trigger;
pub mod update;

use crate::cstructs::c;
use crate::fs::Fs;
use crate::locking::{ByteLock, Locked, Lockdep, Mutex, SpinLockIrq};
use crate::new_lock;
use crate::util::alloc::{KBox, Vec};
use crate::os::bio::BioSet;
use crate::data_structures::cuckoo::Cuckoo;
use crate::util::futex::EventCount;
use crate::data_structures::genradix::SharedGenRadix;
use crate::os::workqueue::{ReffedWork, Workqueue, WqFlags};
use core::pin::Pin;
use core::sync::atomic::{AtomicI64, AtomicU64};
use cstruct_macros::{placed, MaybeZeroable, Init, TypeInfo};
use pin_init::pin_data;
use delete::delete_empty_stripes;
use io::EcBio;
use stripe_new::NewStripes;

#[placed(Fs.ec)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct bch_fs_ec {
    /// Stripe buffers' memory in use, against ec_stripe_buf_limit: Rust's
    /// alone.
    #[pin]
    #[init(new_lock!(0))]
    pub(crate) stripe_buf_bytes: Locked<SpinLockIrq, usize>,
    /// Woken as it's freed, for who waits for it.
    pub(crate) stripe_buf_freed: EventCount,

    /// The open stripes, by index, and the buckets of stripes being made:
    /// see trigger.rs. Rust's alone.
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) stripes_open: Cuckoo,
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) stripes_new_buckets: Cuckoo,

    /// The stripe heads, made on first use - boxed, as their users hold them
    /// without this lock: Rust's alone.
    #[pin]
    #[init(new_lock!(Vec::new()))]
    pub(crate) stripe_heads: Locked<Lockdep<ByteLock>, Vec<Pin<KBox<c::ec_stripe_head>>>>,

    /// Each disk label's ec_dev_stripe_state, made on first use - boxed, as
    /// stripe heads point at them: Rust's alone.
    #[pin]
    #[init(new_lock!(Vec::new()))]
    pub(crate) dev_stripe_states: Locked<Lockdep<ByteLock>, Vec<Pin<KBox<c::ec_dev_stripe_state>>>>,

    /// The stripes being made, once their heads or repairs hand them on,
    /// until their task is done with them; and room for those held. Boxed,
    /// as their users hold them without this lock: Rust's alone. See
    /// stripe_new.rs.
    #[pin]
    #[init(new_lock!(NewStripes::new()))]
    pub(crate) new_stripes: Locked<Mutex, NewStripes>,
    /// Woken as a stripe goes in flight, and as one's freed: for the flushes.
    pub(crate) new_stripes_changed: EventCount,
    pub stripe_new_seq: AtomicI64,

    /// What stripes being made's tasks run on: destroyed with the filesystem,
    /// which drains their last runs - see stripe_new.rs.
    #[init(Workqueue::new(c"bcachefs_ec_create", WqFlags::UNBOUND, 0))]
    pub stripe_create_wq: Workqueue,
    /// Where stripe_idx_alloc() looks for a free slot first: stripe heads'
    /// allocations race on it, each under its own lock.
    pub stripe_hint: AtomicU64,

    #[pin]
    pub(crate) stripe_delete_work: ReffedWork<delete_empty_stripes, crate::field!(super.writes)>,

    /// The stripe block IO's bios: destroyed with the filesystem, after its
    /// rounds of IO, which wait for them
    #[pin]
    #[init(BioSet::new(1))]
    pub block_bioset: BioSet<EcBio<&'static mut [u8]>>,

    /// gc's copies of the stripes: trigger.rs's alone - allocated as the gc
    /// triggers find them, from several threads at once.
    pub(crate) gc_stripes: SharedGenRadix<c::gc_stripe>,
}
