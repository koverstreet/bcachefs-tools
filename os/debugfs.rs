// SPDX-License-Identifier: GPL-2.0

//! debugfs files, read a piece at a time: a file's text is printed into a
//! printbuf and copied out as the reader's buffer has room, the rest left for
//! the next read - so it's never all in memory at once.
//!
//! A file's object is borrowed for each read, never held between them:
//! debugfs_remove() waits out reads in progress and fails later ones, and
//! release(), which can run after it, doesn't touch it.
//!
//! What a reader's buffer is, and how files are made and found, are the
//! port's debugfs.rs, which has this module too.

use core::ops::{ControlFlow, Deref, DerefMut};

use crate::c;
use crate::errcode::BchError;
use crate::util::rcu::FreeAnytime;
use crate::util::Printbuf;
use super::UserBuf;

/// A debugfs file - the type is the file's cursor, Default at open.
pub trait DebugFile: Default + Unpin + FreeAnytime + 'static {
    /// What the file is of: what create_file() was given.
    type Obj;

    /// Whether the printbuf may grow with vmalloc(), past kmalloc()'s limit.
    const MAY_VMALLOC: bool = true;

    /// Print from where the last read left off, until @out is full or there's
    /// no more.
    fn read(&mut self, obj: &Self::Obj, out: &mut DebugOut<'_>) -> Result<(), BchError>;
}

/// A read in progress: derefs to the open file's printbuf.
pub struct DebugOut<'a> {
    pub(super) buf: &'a mut Printbuf,
    pub(super) u:   &'a mut UserBuf,
}

impl DebugOut<'_> {
    /// Copy out what's printed, as much as fits; Break once the reader's
    /// buffer is full.
    pub fn flush(&mut self) -> Result<ControlFlow<()>, BchError> {
        let pending = self.buf.as_bytes();
        if !pending.is_empty() {
            let want = pending.len().min(self.u.len());
            let copied = self.u.copy_to(pending);
            self.buf.drop_front(copied);

            if copied != want {
                return Err(c::EFAULT.into());
            }
        }

        Ok(if self.u.len() == 0 { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
    }

    /// @next prints the next item - false if that was the last, or there was
    /// none - flushing after each, until there are no more or @self is full.
    pub fn print_each(&mut self, mut next: impl FnMut(&mut Printbuf) -> Result<bool, BchError>)
        -> Result<(), BchError>
    {
        while self.flush()?.is_continue() && next(self.buf)? {}
        Ok(())
    }
}

impl Deref for DebugOut<'_> {
    type Target = Printbuf;
    fn deref(&self) -> &Printbuf { self.buf }
}

impl DerefMut for DebugOut<'_> {
    fn deref_mut(&mut self) -> &mut Printbuf { self.buf }
}

/// One read: what's left from the last, then what @cursor prints of @obj,
/// as far as the reader's buffer goes.
pub(super) fn read_locked<F: DebugFile>(cursor: &mut F, obj: &F::Obj, out: &mut DebugOut<'_>)
    -> Result<(), BchError>
{
    if out.flush()?.is_continue() {
        cursor.read(obj, out)?;
    }
    if out.allocation_failure() {
        return Err(c::ENOMEM.into());
    }
    // the read's done, whether this fills the reader's buffer or not
    out.flush().map(drop)
}
