// SPDX-License-Identifier: GPL-2.0

//! What C has as a macro or a static inline in one build or the other, that
//! Rust calls through locking.c's rust_* functions: the contention
//! tracepoints, local locks and irq saves.

pub use crate::c::{
    rust_trace_contention_begin, rust_trace_contention_end,
    rust_local_lock_init, rust_local_lock_irqsave, rust_local_unlock_irqrestore,
    rust_local_irq_save, rust_local_irq_restore,
};
