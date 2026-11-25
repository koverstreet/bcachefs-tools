// SPDX-License-Identifier: GPL-2.0

//! A data update run as an async task: the update, an Update, is the home of
//! its task's box (util/task's TaskSlot, an Exclusive one), and the body - the
//! async fn that runs it, a mover's (moving_update.rs) or a promote's
//! (promote.rs) - its future, which has the update as its own.
//!
//! The update is pinned in the box: its IO holds pointers into it - the read
//! into rbio, the write into op.
//!
//! Each step of its life has a type:
//! - spawn() makes the box, with the update zeroed and no task yet: Spawned -
//!   or a promote's, spawn_held(), the update holding its InFlight, whose
//!   write ref its queue's found through.
//! - init() sets the update up in the spawner's transaction: Ready - or, on
//!   error, the box is dropped. A mover's starts at once, and reads itself,
//!   awaiting read_update(), which borrows the update's rbio and key; a
//!   promote's waits for the read it's made for to read into its rbio -
//!   AwaitingRead - which starts it with what it came to.
//! - The task's future is made as it starts, by its body, from a Body: the
//!   update, as the task's own, and what init() took, which exit() gives back.
//!
//! Body derefs to the data_update. While its IO is in flight the IO writes the
//! update, so no other reference to it may be live then: write() takes the
//! Body by &mut until the IO is done - lending the op to the write path's
//! write(), awaited as a mover's read() is.
//!
//! An update in flight is on the data_update async object list, which
//! debugfs walks under RCU, reading it racily: so the task's memory is freed
//! a grace period after it's done, as every task's is, and the Update dropped
//! then too, its key buffer with it - an RcuDropMember's.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, TransAttempt};
use crate::c;
use crate::data::moving::Moving;
use crate::data::read::{Rbio, ReadDone};
use crate::data::scrub_journal::BadRef;
use crate::data::update::Held;
use crate::debug::async_objs::{AsyncObj, AsyncObjSlot};
use crate::errcode::{ret_to_result_void, BchError};
use crate::data::promote::InFlight;
use crate::fs::Fs;
use crate::os::workqueue::Queue;
use crate::util::alloc::Kmalloc;
use crate::util::gather::Gather;
use crate::util::task::{waiter, Exclusive, Resume, Spawn, TaskList, TaskSlot};
use crate::util::rcu::RcuDropMember;
use crate::util::Printbuf;
use core::cell::UnsafeCell;
use core::ffi::c_int;
use core::future::Future;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{pin_data, pin_init, pinned_drop};
use crate::format::Btree;

// SAFETY: its pointers - to the filesystem, its devices, its read - are good
// on any thread, and an update runs on whichever thread its IO completes on
unsafe impl Send for c::data_update {}

#[pin_data(PinnedDrop)]
pub(crate) struct Update {
    /// A promote's read, as it came to: given as its task's started
    read:      Option<ReadDone>,
    /// A mover's turn to write - moving_update.rs; a promote's is never used
    #[pin]
    turn:      Gather<(), waiter::Async>,
    listed:    AsyncObjSlot,
    /// A promote's - spawn_held()'s: its write ref, what keeps its queue, and
    /// its place under the limit
    promote:   Option<InFlight>,
    /// Zeroed until init(), which expects that; read by the listing's walk
    u:         UnsafeCell<MaybeUninit<c::data_update>>,
    /// The listing's walk may be reading its key
    rcu:       RcuDropMember,
}

impl Update {
    /// Its data_update, init()ed: every Update but alloc()'s and init()'s is.
    fn data(&self) -> &c::data_update {
        unsafe { (*self.u.get()).assume_init_ref() }
    }

    fn data_mut(self: Pin<&mut Self>) -> &mut c::data_update {
        unsafe { self.project().u.get_mut().assume_init_mut() }
    }
}

type Slot<'q, F> = TaskSlot<'q, Update, F, Kmalloc, Exclusive>;

/// How an update's task has its queue: the spawner's, which it says outlives
/// the task - spawn()'s - or, a promote's, @on's in the filesystem the
/// update's write ref keeps - spawn_held()'s.
pub struct Borrowed<'q> {
    at:    Spawn<'q, 'static>,
    tasks: &'q TaskList,
}

pub struct WriteHeld(fn(&Fs) -> &Queue);

fn alloc<'q, F: Future + Send>(t: &TransAttempt<'_, '_>, enomem: c::Errcode) -> Result<Slot<'q, F>, BchError> {
    let fs = t.fs();
    t.allocate_dropping_locks(|| {
        let init = pin_init!(Update {
            read:      None,
            turn <- Gather::new(),
            listed:    AsyncObjSlot::none(),
            promote:   None,
            u:         UnsafeCell::new(MaybeUninit::zeroed()),
            // its drop doesn't sleep
            rcu:       unsafe { RcuDropMember::new() },
        });
        TaskSlot::exclusive_in(Kmalloc, init)
    })
    .map_err(|e| if e.matches(c::ENOMEM) { fs.err(enomem) } else { e })
}

/// Makes the update's box, its task to run on @at, with the update zeroed. Out
/// of memory is reported as @enomem.
///
/// # Safety
/// @at, and what the body borrows, outlive the task; the body keeps its Body
/// only in the future it makes.
pub unsafe fn spawn<'q, F: Future + Send>(t: &TransAttempt<'_, '_>, at: Spawn<'q, 'static>, tasks: &'q TaskList,
                                          enomem: c::Errcode) -> Result<Spawned<'q, F>, BchError> {
    Ok(Spawned { slot: alloc(t, enomem)?, on: Borrowed { at, tasks } })
}

/// spawn(), for a promote: on the queue @on finds in the filesystem
/// @in_flight's write ref keeps - which the update holds till it's dropped,
/// after its task.
pub(crate) fn spawn_held<F: Future + Send>(t: &TransAttempt<'_, '_>, in_flight: InFlight, on: fn(&Fs) -> &Queue,
                                           enomem: c::Errcode) -> Result<Spawned<'static, F, WriteHeld>, BchError> {
    let mut slot = alloc(t, enomem)?;
    *slot.home().project().promote = Some(in_flight);
    Ok(Spawned { slot, on: WriteHeld(on) })
}

/// An update still zeroed, in a box with no task yet: freed a grace period
/// after it's done - the async object list's walk may still be reading it.
pub struct Spawned<'q, F, K = Borrowed<'q>> {
    slot: Slot<'q, F>,
    on:   K,
}

impl<'q, F: Future + Send, K> Spawned<'q, F, K> {
    /// Sets the update up with data_update::init() and readies it for its
    /// read: lists it. On error the box is dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn init(mut self, t: &TransAttempt<'_, '_>, iter: Option<&BtreeIter<'_>>, moving: Option<&Moving<'_>>,
                wp: c::write_point_specifier, io_opts: &c::bch_inode_opts, data_opts: c::data_update_opts,
                btree: Btree, k: BkeySC<'_>) -> Result<Ready<'q, F, K>, BchError> {
        let mut up = self.slot.home();
        // SAFETY: zeroed, and pinned in the box
        let held = unsafe {
            let u = &mut *up.as_mut().project().u.get_mut().as_mut_ptr();
            u.init(t, iter, moving, wp, io_opts, data_opts, btree, k)?
        };
        // SAFETY: the box's home, pinned
        let listed = unsafe { AsyncObjSlot::new(t.fs(), &*up) };
        *up.as_mut().project().listed = listed;

        Ok(Ready { slot: self.slot, on: self.on, held })
    }
}

/// An update that's set up, its box's task not made yet: the spawner makes it.
pub struct Ready<'q, F, K = Borrowed<'q>> {
    slot: Slot<'q, F>,
    on:   K,
    held: Held,
}

impl<F: Future + Send, K> Ready<'_, F, K> {
    /// Its turn to write, as a mover's update: shared, as the moving
    /// context's updates fifo holds a ref on it.
    pub fn turn(&self) -> &Gather<(), waiter::Async> {
        &self.slot.turn
    }
}

impl<'q, F: Future<Output = ()> + Send> Ready<'q, F> {
    /// The task, its future made by @body from the update's Body, started:
    /// its body reads.
    pub fn start(self, body: impl FnOnce(Body<'q>) -> F) {
        let Ready { slot, on: Borrowed { at, tasks }, held } = self;
        // SAFETY: spawn()'s: its queue, and what the body borrows, outlive it
        unsafe { slot.start_unchecked(at, tasks, move |up| body(Body { up, held })) };
    }
}

/// What makes an AwaitingRead's future from its update's Body, once the read's
/// done: for every lifetime of the Body, so it can keep it only in the future.
pub trait ReadBody: Send + 'static {
    type Fut<'h>: Future<Output = ()> + Send;

    fn make(self, u: Body<'_>) -> Self::Fut<'_>;
}

impl<F: Future + Send> Ready<'static, F, WriteHeld> {
    /// The task, to have its future made by @body, for a read another does
    /// into its rbio - a promote's - to start.
    pub fn awaiting_read<B: ReadBody<Fut<'static> = F>>(self, body: B) -> AwaitingRead<B> {
        let Ready { slot, on: WriteHeld(on), held } = self;
        AwaitingRead { slot, on, held, body }
    }
}

impl<F: Future + Send, K> Deref for Ready<'_, F, K> {
    type Target = c::data_update;

    fn deref(&self) -> &c::data_update {
        self.slot.data()
    }
}

impl<F: Future + Send, K> DerefMut for Ready<'_, F, K> {
    fn deref_mut(&mut self) -> &mut c::data_update {
        self.slot.home().data_mut()
    }
}

/// An update's task, waiting for the read into its rbio: made and started
/// only with what the read came to, which the body takes with Body::read().
/// Dropped unstarted, the update is never exit()ed.
pub struct AwaitingRead<B: ReadBody> {
    slot: Slot<'static, B::Fut<'static>>,
    on:   fn(&Fs) -> &Queue,
    held: Held,
    body: B,
}

impl<B: ReadBody> AwaitingRead<B> {
    pub fn rbio(&self) -> &Rbio {
        &self.slot.data().rbio
    }

    /// Its rbio, the task not started: the read's, reading into it.
    pub fn rbio_mut(&mut self) -> &mut Rbio {
        &mut self.slot.home().data_mut().rbio
    }

    pub fn start(self, read: ReadDone) {
        let AwaitingRead { mut slot, on, held, body } = self;
        *slot.home().project().read = Some(read);
        let fs = NonNull::from(slot.promote.as_ref().expect("a promote's update, with its InFlight").fs());
        // its queue and task list, through the write ref the update holds -
        // which the Body never reaches - till it's dropped, after the future;
        // the body borrows nothing, and its make() can keep the Body, for any
        // lifetime, only in the future it makes
        unsafe {
            let fs = fs.as_ref();
            slot.start_unchecked(Spawn::Queue(on(fs)), fs.tasks(), move |up| body.make(Body { up, held }))
        }
    }
}

#[pinned_drop]
impl PinnedDrop for Update {
    fn drop(self: Pin<&mut Self>) {
        // the key buffer, dropped a grace period on, past the list's walk.
        // Zeroed, for an update never init()ed, it's valid too
        unsafe { core::ptr::drop_in_place(&raw mut (*self.u.get().cast::<c::data_update>()).index.k) };
    }
}

// SAFETY: the task is dropped and its memory freed a grace period after it's
// done, its key buffer with it. A promote's to_text() also reads the
// parent read while read_done isn't set - which races with the read
// completing just after: the XXX at the top of async_objs.rs. A mover's
// doesn't read its context, which isn't freed a grace period after it's done
unsafe impl AsyncObj for Update {
    const LIST: c::AsyncObjKind = c::AsyncObjKind::data_update;

    #[cold]
    fn to_text(&self, out: &mut Printbuf, _: &Fs) {
        crate::data::update::data_update_inflight_to_text(out, self.data())
    }
}

// ── Body ─────────────────────────────────────────────────────────────────

/// The update, its task's own, and what it holds from init() till exit().
pub struct Body<'h> {
    up:   Pin<&'h mut Update>,
    held: Held,
}

impl Body<'_> {
    /// Its turn to write, as a mover's update: shared, as the moving
    /// context's updates fifo holds a ref on it.
    pub fn turn(&self) -> &Gather<(), waiter::Async> {
        &self.up.turn
    }

    /// What the read another did came to: AwaitingRead's, taken once.
    pub fn read(&mut self) -> ReadDone {
        self.up.as_mut().project().read.take().expect("a data update's read, given as its task started")
    }

    /// The write of what @read read, if read_done() says there is one,
    /// resumed @resume once it's done: the update's result. A journal scrub's
    /// update puts what it finds bad to @found.
    pub async fn write(&mut self, read: ReadDone, found: Option<&BadRef>, resume: Resume<'_>)
        -> Result<(), BchError>
    {
        if self.read_done(read, found)? {
            let u = &mut **self;
            use crate::data::write::{write, WriteBio, WriteFor};
            write(&mut u.op, WriteBio::Update(&mut u.pages), WriteFor::Update(&u.index), resume).await;
            ret_to_result_void(c_int::from(u.op.error))?;
        }
        Ok(())
    }

    /// Takes the update off its async object list, and exit()s it with what
    /// it held.
    pub fn exit(self, ret: Result<(), BchError>) {
        let Body { mut up, held } = self;
        *up.as_mut().project().listed = AsyncObjSlot::none();
        up.data_mut().exit(held, ret)
    }
}

impl Deref for Body<'_> {
    type Target = c::data_update;

    fn deref(&self) -> &c::data_update {
        self.up.data()
    }
}

impl DerefMut for Body<'_> {
    fn deref_mut(&mut self) -> &mut c::data_update {
        self.up.as_mut().data_mut()
    }
}
