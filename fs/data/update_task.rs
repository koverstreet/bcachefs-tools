// SPDX-License-Identifier: GPL-2.0

//! A data update run as an async task. UpdateTask is the task's future: the
//! update, an Update, and the body - the async fn that runs it, a mover's
//! (moving_update.rs) or a promote's (promote.rs).
//!
//! The update is pinned in the task. C holds pointers into it - the read path
//! into rbio, the write path into op - and the endios get from those to its
//! Dones by container_of.
//!
//! Each step of its life has a type:
//! - spawn() makes the task, with the update zeroed: Spawned.
//! - init() sets the update up in the spawner's transaction: Ready - or, on
//!   error, the task's dropped unstarted. The spawner start()s the task with
//!   its body. A mover's body issues the read itself, Body::issue_read(); a
//!   promote's waits on the one C issues once the promote's made, its Done
//!   made first, by start_read().
//! - The body holds the update as a Body.
//!
//! Body derefs to the data_update, and Ready to its Body. While an IO is in flight C writes
//! the update, so no Rust reference to it may be live then: read() and
//! write() take the Body by &mut until the IO is done, and each deref makes a
//! new reference from the raw pointer.
//!
//! read() and write() wait on C's IO through Completions, each started on its
//! Done before the IO's issued, and completed by its endio. When the read and
//! write paths are tasks, they'll await those tasks instead, and the Dones,
//! the endio shims and op.end_io go away.
//!
//! An update in flight is on the data_update async object list, which
//! debugfs walks under RCU: so the task's memory is freed a grace period after it's done, as every
//! task's is, and the key buffer likewise by exit_rcu().

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::c;
use crate::data::moving::Moving;
use crate::data::update::Held;
use crate::debug::async_objs::{obj_to_text, AsyncObj, AsyncObjSlot};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::alloc::gfp_flags;
use crate::util::task::{Completion, Done};
use crate::util::gather::Gather;
use crate::util::task::{waiter, Resumable, Resume, Spawn};
use crate::util::task::TaskBox;
use crate::util::Printbuf;
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use cstruct_macros::c_export;
use core::future::Future;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};
use pin_init::{pin_data, pin_init, pinned_drop};

// SAFETY: its pointers - to the filesystem, its devices, its read - are good
// on any thread, and an update runs on whichever thread its IO completes on
unsafe impl Send for c::data_update {}

#[pin_data(PinnedDrop)]
pub struct Update {
    /// Born with their waiters, as their IOs are submitted
    read:      Option<Done>,
    write:     Option<Done>,
    /// A mover's turn to write - moving_update.rs; a promote's is never used
    #[pin]
    turn:      Gather<(), waiter::Async>,
    listed:    AsyncObjSlot,
    /// From init() till exit()
    held:      Option<Held>,
    /// Zeroed until init(), which expects that
    u:         UnsafeCell<MaybeUninit<c::data_update>>,
}

#[pin_data]
pub struct UpdateTask<B> {
    /// Declared first so it's dropped first: it points into @update
    #[pin]
    body:   Option<B>,
    #[pin]
    update: Update,
}

impl<B: Future<Output = ()> + Send> UpdateTask<B> {
    /// Makes the task, to run on @at, with the update zeroed. Out of memory is
    /// reported as @enomem.
    ///
    /// # Safety
    /// @at, and what the body borrows, outlive the task.
    pub unsafe fn spawn<'q>(t: &TransAttempt<'_, '_>, at: Spawn<'q, '_>, enomem: c::bch_errcode)
        -> Result<Spawned<'q, B>, BchError>
    {
        let fs = t.fs();
        t.allocate_dropping_locks(|gfp| {
            let init = pin_init!(Self {
                body: None,
                update <- pin_init!(Update {
                    read:      None,
                    write:     None,
                    turn <- Gather::new(),
                    listed:    AsyncObjSlot::none(),
                    held:      None,
                    u:         UnsafeCell::new(MaybeUninit::zeroed()),
                }),
            });
            TaskBox::new(at, init, gfp_flags(gfp))
        })
        .map(Spawned)
        .map_err(|e| if e.matches(c::ENOMEM) { fs.err(enomem) } else { e })
    }
}

impl<B: Future<Output = ()>> Future for UpdateTask<B> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.project().body.as_pin_mut().expect("a data update's task started without its body").poll(cx)
    }
}

/// A task whose update is still zeroed: a TaskBox, so freed a grace period
/// after it's done - the async object list's walk may still be reading it.
pub struct Spawned<'q, B: Future<Output = ()>>(TaskBox<'q, UpdateTask<B>>);

impl<'q, B: Future<Output = ()> + Send> Spawned<'q, B> {
    /// Sets the update up with data_update::init() and readies it for its
    /// read: sets op.end_io, and lists it. On error the task is dropped
    /// unstarted.
    #[allow(clippy::too_many_arguments)]
    pub fn init(mut self, t: &TransAttempt<'_, '_>, iter: Option<&BtreeIter<'_>>, moving: Option<&Moving<'_>>,
                wp: c::write_point_specifier, io_opts: &c::bch_inode_opts, data_opts: c::data_update_opts,
                btree: c::btree_id, k: BkeySC<'_>) -> Result<Ready<'q, B>, BchError> {
        let mut up = self.0.get().future().project().update;
        // SAFETY: zeroed, and pinned in the task
        unsafe {
            let u = &mut *up.u.get().cast::<c::data_update>();
            let held = u.init(t, iter, moving, wp, io_opts, data_opts, btree, k)?;
            u.op.end_io = Some(write_done);
            *up.as_mut().project().held = Some(held);
        }
        // SAFETY: a field of the pinned task
        let slot = unsafe { AsyncObjSlot::new(t.fs(), &*up) };
        *up.as_mut().project().listed = slot;

        let body = Body { up: NonNull::from(up.as_ref().get_ref()), read: None };
        Ok(Ready { task: self.0, body })
    }
}

/// An update that's set up, in a task not started yet: the spawner start()s
/// it.
pub struct Ready<'q, B: Future<Output = ()>> {
    task: TaskBox<'q, UpdateTask<B>>,
    body: Body,
}

impl<B: Future<Output = ()> + Send> Ready<'_, B> {
    /// Right before the read's issued.
    pub fn start_read(&mut self) {
        let task = self.task.get();
        // SAFETY: the Body, in the task, awaits it or drops it; the update's,
        // not yet reached by its IO
        let (slot, waiter) = unsafe { (&mut (*self.body.up.as_ptr()).read, waiter::Async::of(&task)) };
        self.body.read = Some(Completion::new(slot, waiter));
    }

    /// Starts the task, @body running the update.
    pub fn start(self, body: impl FnOnce(Body) -> B) {
        let Ready { mut task, body: b } = self;
        task.get().future().project().body.set(Some(body(b)));
        // SAFETY: spawn()'s: its queue, and what the body borrows, outlive it
        unsafe { task.start_unchecked() };
    }
}

impl<B: Future<Output = ()>> Deref for Ready<'_, B> {
    type Target = Body;

    fn deref(&self) -> &Body {
        &self.body
    }
}

impl<B: Future<Output = ()>> DerefMut for Ready<'_, B> {
    fn deref_mut(&mut self) -> &mut Body {
        &mut self.body
    }
}

impl Update {
    /// The Update @u is in.
    ///
    /// # Safety
    /// @u is an Update's.
    unsafe fn of(u: *mut c::data_update) -> *mut Self {
        // SAFETY: the caller's; @u is in an UnsafeCell, laid out as its value
        unsafe { u.byte_sub(core::mem::offset_of!(Self, u)).cast() }
    }

    /// Called by the read path when @u's read completes.
    ///
    /// # Safety
    /// @u is an Update's, its read issued.
    pub unsafe fn read_done(u: *mut c::data_update) {
        // SAFETY: the caller's. The Body is waiting, holding no reference; we
        // touch only read_done, an atomic, and the slot
        unsafe {
            (*u).read_done.store(true, core::sync::atomic::Ordering::Relaxed);
            (*Self::of(u)).read.as_ref().expect("a data update's read done, never issued").complete();
        }
    }
}

/// # Safety
/// @op is an Update's update's write op.
unsafe extern "C" fn write_done(op: *mut c::bch_write_op) {
    // SAFETY: the caller's; as in read_done()
    unsafe {
        let u = op.byte_sub(core::mem::offset_of!(c::data_update, op)).cast::<c::data_update>();
        (*Update::of(u)).write.as_ref().expect("a data update's write done, never issued").complete();
    }
}

#[pinned_drop]
impl PinnedDrop for Update {
    fn drop(self: Pin<&mut Self>) {
        // exit() leaves the key buffer for here, as the list's walk may still
        // be reading it. Zeroed, for an update never init()ed, it's valid too
        // SAFETY: a raw place, as the update may be zeroed
        unsafe { (*self.u.get().cast::<c::data_update>()).k.exit_rcu() };
    }
}

// SAFETY: the task's memory is freed a grace period after it's dropped, and
// the key buffer likewise by exit_rcu(). A promote's to_text() also reads the
// parent read while read_done isn't set - which races with the read
// completing just after: the XXX at the top of async_objs.rs. A mover's
// doesn't read its context, which isn't freed a grace period after it's done
unsafe impl AsyncObj for Update {
    const LIST: c::bch_async_obj_lists = c::bch_async_obj_lists::BCH_ASYNC_OBJ_LIST_data_update;

    #[cold]
    fn to_text(&self, out: &mut Printbuf, fs: &Fs) {
        // SAFETY: init()ed before it was listed
        let u = unsafe { (*self.u.get()).assume_init_ref() };
        if u.rbio.bits().promote() != 0 {
            crate::data::promote::promote_to_text(u, out, fs)
        } else {
            crate::data::update::bch2_data_update_inflight_to_text(out, u)
        }
    }
}

#[cold]
#[c_export]
pub unsafe fn bch2_data_update_obj_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, obj: *mut c_void) {
    // SAFETY: the caller's
    unsafe { obj_to_text::<Update>(out, c, obj) }
}

// ── Body ─────────────────────────────────────────────────────────────────

/// A handle on an init()ed update: the spawner's, in a Ready, then the body's.
pub struct Body {
    up:   NonNull<Update>,
    read: Option<Completion<'static>>,
}

// SAFETY: the update is the task's, and a task may run on any thread
unsafe impl Send for Body {}

impl Body {
    /// The Update, for its Completions, Gather and slots - not the
    /// data_update, which C writes.
    fn update(&self) -> &Update {
        // SAFETY: pinned in the task, which outlives its body
        unsafe { self.up.as_ref() }
    }

    /// Its turn to write, as a mover's update: shared, as the moving
    /// context's updates fifo holds a ref on it.
    pub fn turn(&self) -> Pin<&Gather<(), waiter::Async>> {
        // SAFETY: @turn is structurally pinned
        unsafe { Pin::new_unchecked(&self.update().turn) }
    }

    /// Issues the read, from the task, and waits for it: @issue submits it, or
    /// fails before it touches the rbio, so it never went out.
    pub async fn issue_read(&mut self, issue: impl FnOnce(&mut c::data_update) -> Result<(), BchError>)
        -> Result<(), BchError>
    {
        let waiter = waiter::Async::current().await;
        // SAFETY: not tied to the Body, which the read is lent
        let read = Completion::new(unsafe { &mut (*self.up.as_ptr()).read }, waiter);
        if let Err(e) = issue(self) {
            // ended at submit: completed, as its endio would, so the drop
            // doesn't wait
            unsafe { (*self.up.as_ptr()).read.as_ref() }.expect("the read's Done, just made").complete();
            drop(read);
            return Err(e);
        }
        read.resume_on(Resume::Spawned).await;
        Ok(())
    }

    /// Waits for the read the spawner issued.
    pub async fn read(&mut self) {
        let read = self.read.take().expect("read awaited once");
        read.resume_on(Resume::Spawned).await
    }

    /// Issues the write, if read_done() says there is one, and waits for it.
    pub async fn write(&mut self) {
        if self.read_done() {
            let waiter = waiter::Async::current().await;
            // SAFETY: not tied to the Body, which the write is lent
            let write = Completion::new(unsafe { &mut (*self.up.as_ptr()).write }, waiter);
            c::data_update::write(self);
            write.resume_on(Resume::Spawned).await
        }
    }

    /// Takes the update off its async object list, and exit()s it.
    pub fn exit(mut self, ret: c_int) {
        // SAFETY: only we touch these - the walk reads the list, not the slot
        let held = unsafe {
            let up = self.up.as_ptr();
            (*up).listed = AsyncObjSlot::none();
            (*up).held.take()
        };
        c::data_update::exit(&mut self, held.expect("a data update exit()ed twice"), ret)
    }
}

impl Deref for Body {
    type Target = c::data_update;

    fn deref(&self) -> &c::data_update {
        // SAFETY: init()ed before a Body's made. No IO it's lent to is in
        // flight: read() and write() hold the Body by &mut till they're done
        unsafe { (*self.update().u.get()).assume_init_ref() }
    }
}

impl DerefMut for Body {
    fn deref_mut(&mut self) -> &mut c::data_update {
        // SAFETY: as deref(); and the Body is the update's only owner
        unsafe { (*(*self.up.as_ptr()).u.get()).assume_init_mut() }
    }
}
