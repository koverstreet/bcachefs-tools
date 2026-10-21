// SPDX-License-Identifier: GPL-2.0

//! Events - sb/counters.h's event_inc(), event_add() and event_trace() - for
//! Rust, each event a type; C's macros are as they were.
//!
//! An event is one of BCH_PERSISTENT_COUNTERS() (counters_format.rs): a
//! counter in the filesystem's counters.now[], per CPU, and the tracepoint of
//! the same name every counter has (debug/trace.h). Its type, events::<name>,
//! carries what the counter counts - occurrences (TYPE_COUNTER) or sectors
//! (TYPE_SECTORS) - so event_inc() takes the one and event_add() the other,
//! as C's counter_typecheck() checks.
//!
//! Counters are added to with util/percpu.rs's this_cpu_add(). A tracepoint is
//! checked as C's trace_<name>_enabled() is, a static branch on its key -
//! util/jump_label.rs's static_branch_unlikely!() - so a disabled one costs a NOP; its
//! message is built only once it's on, out of line, as __event_trace() builds
//! it, and handed to C's rust_do_trace_<name>(), which debug/trace.c makes for
//! every tracepoint with CREATE_RUST_TRACE_POINTS. Userspace has no
//! tracepoints: none is ever on.
//!
//! XXX: a tracepoint fired only from Rust gets no __tracepoint_check entry, as
//! C's use sites leave, so a WARN_ON_UNUSED_TRACEPOINTS build calls it unused.

use core::ffi::c_int;

use cstruct_macros::c_export;

use crate::c;
use crate::util::vstructs::Unvalidated;
use crate::fs::Fs;
use crate::prt;
use crate::sb::io::{sb_field_u64s, SbFieldOps, COUNTERS};
use crate::types::Le64;
use crate::util::ffi::Opaque;
use crate::util::kernel::{nsecs_to_jiffies, NSEC_PER_SEC};
use crate::util::percpu::{percpu_u64_get, percpu_u64_set, this_cpu_add};
use crate::util::workqueue::Queue;
use crate::util::Printbuf;

/// What an event's counter counts.
pub trait Kind {}

/// Occurrences: TYPE_COUNTER, counted with event_inc().
pub enum Count {}

/// An amount in sectors: TYPE_SECTORS, added with event_add().
pub enum Sectors {}

impl Kind for Count {}
impl Kind for Sectors {}

/// One of BCH_PERSISTENT_COUNTERS(): its counter, and its tracepoint.
pub trait Event: Copy {
    type Kind: Kind;

    /// Its counter's index in counters.now[]: BCH_COUNTER_<name>.
    const IDX: usize;

    /// Whether its tracepoint is on: trace_<name>_enabled().
    fn trace_enabled() -> bool;

    /// Fire its tracepoint, with @msg: trace_<name>().
    fn trace(fs: &Fs, msg: &Printbuf);
}

macro_rules! event_kind {
    (TYPE_COUNTER) => { Count };
    (TYPE_SECTORS) => { Sectors };
}

macro_rules! events {
    ($(($name:ident, $id:tt, $ty:tt $(, $($rest:tt)*)?)),* $(,)?) => { ::paste::paste! {
        /// The events of BCH_PERSISTENT_COUNTERS(), each a type.
        #[allow(non_camel_case_types)]
        pub mod events {
            $(
                #[derive(Clone, Copy)]
                pub struct $name;
            )*
        }

        $(
            impl Event for events::$name {
                type Kind = event_kind!($ty);

                const IDX: usize = c::[<BCH_COUNTER_ $name>] as usize;

                #[cfg(all(kernel, CONFIG_TRACEPOINTS))]
                #[inline(always)]
                fn trace_enabled() -> bool {
                    // SAFETY: a tracepoint's key, which C defines
                    unsafe {
                        crate::static_branch_unlikely!(tracepoints::[<__tracepoint_ $name>],
                                                       c::tracepoint, key)
                    }
                }

                #[cfg(not(all(kernel, CONFIG_TRACEPOINTS)))]
                #[inline(always)]
                fn trace_enabled() -> bool {
                    false
                }

                #[cfg(all(kernel, CONFIG_TRACEPOINTS))]
                fn trace(fs: &Fs, msg: &Printbuf) {
                    // SAFETY: @fs is live, and @msg a C string the tracepoint
                    // copies
                    unsafe {
                        tracepoints::[<rust_do_trace_ $name>](fs.as_ptr(), msg.as_c_str().as_ptr());
                    }
                }

                #[cfg(not(all(kernel, CONFIG_TRACEPOINTS)))]
                fn trace(_fs: &Fs, _msg: &Printbuf) {}
            }
        )*

        /// The tracepoints, C's: debug/trace.c's.
        #[cfg(all(kernel, CONFIG_TRACEPOINTS))]
        mod tracepoints {
            use crate::c;

            extern "C" {
                $(
                    pub static [<__tracepoint_ $name>]: c::tracepoint;
                    pub fn [<rust_do_trace_ $name>](c: *mut c::bch_fs, str: *const ::core::ffi::c_char);
                )*
            }
        }
    } };
}
c::BCH_PERSISTENT_COUNTERS!(events);

impl Fs {
    /// Count @_e once: event_inc().
    #[inline(always)]
    pub fn event_inc<E: Event<Kind = Count>>(&self, _e: E) {
        self.event_count::<E>(1);
    }

    /// Add @nr sectors to @_e: event_add().
    #[inline(always)]
    pub fn event_add<E: Event<Kind = Sectors>>(&self, _e: E, nr: u64) {
        self.event_count::<E>(nr);
    }

    #[inline(always)]
    fn event_count<E: Event>(&self, nr: u64) {
        // SAFETY: counters.now is per CPU, BCH_COUNTER_NR of them, from
        // bch2_fs_counters_init() until the filesystem is freed
        unsafe { this_cpu_add(self.counters.now.wrapping_add(E::IDX), nr) };
    }

    /// Fire @e's tracepoint, if it's on, with the message @msg writes - its
    /// lines after the first indented by 2: event_trace().
    #[inline(always)]
    pub fn event_trace<E: Event>(&self, e: E, msg: impl FnOnce(&mut Printbuf)) {
        if E::trace_enabled() {
            self.event_trace_on(e, msg);
        }
    }

    #[cold]
    #[inline(never)]
    fn event_trace_on<E: Event>(&self, _e: E, msg: impl FnOnce(&mut Printbuf)) {
        let mut buf = Printbuf::new();
        buf.indent_add_nextline(2);
        msg(&mut buf);
        E::trace(self, &buf);
    }

    /// event_trace(), then event_inc(): event_inc_trace().
    #[inline(always)]
    pub fn event_inc_trace<E: Event<Kind = Count>>(&self, e: E, msg: impl FnOnce(&mut Printbuf)) {
        self.event_trace(e, msg);
        self.event_inc(e);
    }

    /// event_trace(), then event_add(): event_add_trace().
    #[inline(always)]
    pub fn event_add_trace<E: Event<Kind = Sectors>>(&self, e: E, nr: u64,
                                                    msg: impl FnOnce(&mut Printbuf)) {
        self.event_trace(e, msg);
        self.event_add(e, nr);
    }
}

// ── The counters, in memory and in the superblock ──────────────────────────
//
// In memory by BCH_COUNTER_* index, COUNTERS' order; in the superblock and the
// ioctl by stable id. mount[] and recent[] are plain u64s, written racing
// readers that only print them, as in C.

const NR: usize = c::BCH_COUNTER_NR as usize;
const NR_STABLE: usize = c::BCH_COUNTER_STABLE_NR as usize;

impl c::bch_fs_counters {
    /// Counter @i, every CPU's count summed.
    fn now(&self, i: usize) -> u64 {
        // SAFETY: now is per CPU, NR of them, for the filesystem's life
        unsafe { percpu_u64_get(self.now.wrapping_add(i)) }
    }

    fn set_now(&self, i: usize, v: u64) {
        // SAFETY: as now()'s
        unsafe { percpu_u64_set(self.now.wrapping_add(i), v) }
    }

    /// For mount[] and recent[], which change under shared references.
    fn raw(&self) -> *mut Self {
        core::ptr::from_ref(self).cast_mut()
    }

    /// The counters that changed in the last ten seconds: each with what
    /// it's gone up by in each half second since, newest first.
    pub fn recent_to_text(&self, out: &mut Printbuf) {
        let oldest = &self.recent[c::NR_RECENT_COUNTERS as usize - 1];

        for (i, ctr) in COUNTERS.iter().enumerate() {
            let mut prev = self.now(i);
            if oldest[i] == prev {
                continue;
            }

            prt!(out, "{}:", ctr.name);
            for sample in &self.recent {
                prt!(out, "\t{}", prev.wrapping_sub(sample[i]));
                prev = sample[i];
            }
            prt!(out, "\n");
        }
    }
}

#[c_export]
pub fn bch2_sb_recent_counters_to_text(out: &mut Printbuf, ctrs: &c::bch_fs_counters) {
    ctrs.recent_to_text(out)
}

impl Fs {
    pub fn counter_read(&self, idx: usize) -> u64 {
        assert!(idx < NR);
        self.counters.now(idx)
    }

    /// Zero counter @idx - and what's relative to it, its count at mount and
    /// its recent samples, which would otherwise read as having gone backwards.
    pub fn counter_reset(&self, idx: usize) {
        if idx >= NR {
            return;
        }

        self.counters.set_now(idx, 0);
        let ctrs = self.counters.raw();
        // SAFETY: see the top of the section
        unsafe {
            (*ctrs).mount[idx] = 0;
            for sample in &mut (*ctrs).recent {
                sample[idx] = 0;
            }
        }
    }
}

impl SbFieldOps for c::bch_sb_field_counters {
    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_counters<Unvalidated>) {
        for ctr in COUNTERS {
            if let Some(v) = f.d.get(ctr.stable_id as usize) {
                prt!(out, "{} \t{}\n", ctr.name, v.get());
            }
        }
    }
}

/// The counters into the superblock, for a superblock write - its field
/// grown to a slot for every stable id if it can be, else as it is.
#[c_export]
pub fn bch2_sb_counters_from_cpu(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock
    let mut sb = unsafe { fs.sb_lock_held() };

    let u64s = sb_field_u64s::<c::bch_sb_field_counters>(NR_STABLE);
    let _ = sb.field_get_minsize::<c::bch_sb_field_counters>(u64s);
    let Some(f) = sb.field_mut::<c::bch_sb_field_counters>() else { return };

    for (i, ctr) in COUNTERS.iter().enumerate() {
        if let Some(d) = f.d.get_mut(ctr.stable_id as usize) {
            *d = Le64::new(fs.counters.now(i));
        }
    }
}

crate::fs_delayed_work! {
    /// Every half second: the counters sampled into recent[], newest first.
    pub(crate) work RecentCounters: field counters_recent;

    fn run(fs: &Fs) {
        let ctrs = fs.counters.raw();
        // SAFETY: see the top of the section
        let recent = unsafe { &mut (*ctrs).recent };
        let n = recent.len();
        recent.copy_within(..n - 1, 1);
        for (i, v) in recent[0].iter_mut().enumerate() {
            *v = fs.counters.now(i);
        }

        Self::queue(fs);
    }
}

impl RecentCounters {
    fn queue(fs: &Fs) {
        fs.counters_recent.queue(Queue::system_dfl(), nsecs_to_jiffies(NSEC_PER_SEC / 2));
    }
}

/// The superblock's counters into memory, as both the counts now and at
/// mount. A counter the superblock has no slot for starts at 0.
#[c_export]
pub fn bch2_fs_counters_init(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    let sb = fs.sb_lock();
    let d: &[Le64] = sb.field::<c::bch_sb_field_counters>().map_or(&[], |f| &f.d);
    let ctrs = fs.counters.raw();

    for (i, ctr) in COUNTERS.iter().enumerate() {
        let v = d.get(ctr.stable_id as usize).map(|v| v.get());
        if let Some(v) = v {
            fs.counters.set_now(i, v);
        }
        // SAFETY: filesystem init, before anything reads them
        unsafe { (*ctrs).mount[i] = v.unwrap_or(0) };
    }
    0
}

#[c_export]
pub fn bch2_fs_counters_init_late(c: &Opaque<c::bch_fs>) -> c_int {
    RecentCounters::queue(Fs::from_c(c));
    0
}

/// Before counters.now is freed.
#[c_export]
pub fn bch2_fs_counters_exit(c: &Opaque<c::bch_fs>) {
    Fs::from_c(c).counters_recent.cancel_sync();
}

// ── BCH_IOCTL_QUERY_COUNTERS ─────────────────────────────────────────────────

#[cfg(not(NO_BCACHEFS_CHARDEV))]
mod ioctl {
    use core::ffi::c_long;
    use core::mem::{offset_of, size_of};

    use kernel::transmute::AsBytes;
    use kernel::uaccess::{UserPtr, UserSlice};

    use super::*;
    use crate::errcode::{bch_errcode, ret_to_c, BchError};
    use crate::util::vstructs::Head;

    /// The ioctl's argument, short of d[], the counters.
    type Args = Head<c::bch_ioctl_query_counters>;

    /// @v to the user's memory at @at.
    fn put<T: AsBytes>(at: UserPtr, v: T) -> Result<(), BchError> {
        Ok(UserSlice::new(at, size_of::<T>()).writer().write(&v)?)
    }

    /// The counters, by stable id, up to the user's @nr of them: now, or at
    /// mount (BCH_IOCTL_QUERY_COUNTERS_MOUNT). @nr comes back as the number
    /// filled.
    fn query_counters(fs: &Fs, user: UserPtr) -> Result<(), BchError> {
        let arg: Args = UserSlice::new(user, size_of::<Args>()).reader().read()?;

        let flags = u32::from(arg.flags);
        if flags & !c::BCH_IOCTL_QUERY_COUNTERS_MOUNT != 0 || arg.pad != 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_query_counters_bad_flags);
        }
        let mount = flags & c::BCH_IOCTL_QUERY_COUNTERS_MOUNT != 0;

        let nr = arg.nr.min(NR_STABLE as u16);
        put(user.wrapping_byte_add(offset_of!(Args, nr)), nr)?;

        // d[] follows the head
        let d = user.wrapping_byte_add(size_of::<Args>());
        for (i, ctr) in COUNTERS.iter().enumerate() {
            if ctr.stable_id < nr {
                let v = if mount { fs.counters.mount[i] } else { fs.counters.now(i) };
                put(d.wrapping_byte_add(usize::from(ctr.stable_id) * size_of::<u64>()), v)?;
            }
        }
        Ok(())
    }

    #[c_export]
    pub fn bch2_ioctl_query_counters(c: &Opaque<c::bch_fs>,
                                     user: *mut Head<c::bch_ioctl_query_counters>) -> c_long {
        ret_to_c(query_counters(Fs::from_c(c), UserPtr::from_ptr(user.cast()))).into()
    }
}
