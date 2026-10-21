// SPDX-License-Identifier: GPL-2.0

//! debugfs files, read a piece at a time: a file's text is printed into a
//! printbuf and copied out as the reader's buffer has room, the rest left for
//! the next read - so it's never all in memory at once.
//!
//! A file's object is borrowed for each read, never held between them:
//! debugfs_remove() waits out reads in progress and fails later ones, and
//! release(), which can run after it, doesn't touch it.
//!
//! The VFS doesn't serialize reads of one open file - pread() takes no
//! f_pos_lock - so its state is under a mutex.

use crate::c;
use crate::errcode::{errptr_to_result, ret_to_class, BchError};
use crate::locking::Lock;
use crate::new_lock;
use crate::util::alloc::{alloc_pinned, free_pinned};
use crate::util::ffi::c_char;
use crate::util::Printbuf;
use core::ffi::{c_int, c_ulong, c_void, CStr};
use core::marker::PhantomData;
use core::ops::{ControlFlow, Deref, DerefMut};
use core::ptr::NonNull;
use pin_init::{pin_data, pin_init};

/// A debugfs file - the type is the file's cursor, Default at open.
pub trait DebugFile: Default + Unpin + 'static {
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
    buf:  &'a mut Printbuf,
    ubuf: *mut c_void,
    size: usize,
    ret:  usize,
}

impl DebugOut<'_> {
    /// Copy out what's printed, as much as fits; Break once the reader's
    /// buffer is full.
    pub fn flush(&mut self) -> Result<ControlFlow<()>, BchError> {
        let pending = self.buf.as_bytes();
        if !pending.is_empty() {
            let bytes = pending.len().min(self.size);
            // SAFETY: copy_to_user() checks the reader's buffer is theirs
            let uncopied = unsafe {
                c::bch2_copy_to_user(self.ubuf, pending.as_ptr().cast(), bytes as c_ulong)
            } as usize;
            let copied = bytes - uncopied;

            self.ret  += copied;
            self.ubuf  = self.ubuf.wrapping_byte_add(copied);
            self.size -= copied;
            self.buf.drop_front(copied);

            if copied != bytes {
                return Err(c::EFAULT.into());
            }
        }

        Ok(if self.size == 0 { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
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

/* The file_operations */

/// An open file: file->private_data.
#[pin_data]
struct Open<F: DebugFile> {
    obj: NonNull<F::Obj>,
    #[pin]
    state: Lock<c::mutex, (F, Printbuf)>,
}

unsafe extern "C" fn open<F: DebugFile>(inode: *mut c::inode, file: *mut c::file) -> c_int {
    // SAFETY: i_private is create_file()'s @obj
    let obj = NonNull::new(unsafe { (*inode).i_private }.cast::<F::Obj>())
        .expect("debugfs file without its object");

    let mut buf = Printbuf::new();
    buf.set_may_vmalloc(F::MAY_VMALLOC);

    let open = alloc_pinned(pin_init!(Open::<F> {
        obj,
        state <- new_lock!((F::default(), buf)),
    }? BchError));
    ret_to_class(open.map(|p| unsafe { (*file).private_data = p.cast() }))
}

unsafe extern "C" fn release<F: DebugFile>(_inode: *mut c::inode, file: *mut c::file) -> c_int {
    // SAFETY: open()'s, and the file's last use
    unsafe { free_pinned((*file).private_data.cast::<Open<F>>()) };
    0
}

unsafe extern "C" fn read<F: DebugFile>(file: *mut c::file, ubuf: *mut c_char, size: usize,
                                        _ppos: *mut c::loff_t) -> isize {
    // SAFETY: open()'s, live until release(); @obj outlives reads
    let open = unsafe { &*(*file).private_data.cast::<Open<F>>() };
    let obj  = unsafe { open.obj.as_ref() };

    let mut state = open.state.lock();
    let (cursor, buf) = &mut *state;
    let mut out = DebugOut { buf, ubuf: ubuf.cast(), size, ret: 0 };

    match read_locked(cursor, obj, &mut out) {
        Ok(()) => out.ret as isize,
        Err(e) => e.class() as isize,
    }
}

fn read_locked<F: DebugFile>(cursor: &mut F, obj: &F::Obj, out: &mut DebugOut<'_>)
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

/// OPS is promoted to a static where create_file() takes its address.
struct FileOps<F>(c::file_operations, PhantomData<F>);

unsafe impl<F> Sync for FileOps<F> {}

impl<F: DebugFile> FileOps<F> {
    const OPS: Self = {
        // SAFETY: zero is NULL, "not implemented"
        let mut ops: c::file_operations = unsafe { core::mem::zeroed() };

        #[cfg(kernel)]
        { ops.owner = &raw mut kernel::bindings::__this_module; }
        ops.open    = Some(open::<F>);
        ops.release = Some(release::<F>);
        ops.read    = Some(read::<F>);

        FileOps(ops, PhantomData)
    };
}

/* Creating and removing them */

/// None if debugfs_create_dir() failed, or debugfs is disabled.
pub fn create_dir(name: &CStr, parent: Option<NonNull<c::dentry>>) -> Option<NonNull<c::dentry>> {
    let parent = parent.map_or(core::ptr::null_mut(), NonNull::as_ptr);
    let dir = unsafe { c::debugfs_create_dir(name.as_ptr().cast(), parent) };
    errptr_to_result(NonNull::new(dir).expect("a dentry, or an error")).ok()
}

/// Read-only: a DebugFile has no write. Failure is ignored, as in C.
///
/// # Safety
/// @obj outlives the file: it's removed, with remove_recursive(), first.
pub unsafe fn create_file<F: DebugFile>(name: &CStr, parent: NonNull<c::dentry>, obj: &F::Obj) {
    let ops: &'static FileOps<F> = &FileOps::OPS;
    unsafe {
        c::bch2_debugfs_create_file(name.as_ptr().cast(), 0o400, parent.as_ptr(),
                                    core::ptr::from_ref(obj).cast_mut().cast(), &ops.0);
    }
}

pub fn remove_recursive(dir: NonNull<c::dentry>) {
    // debugfs_remove_recursive() is a macro for it, in-kernel
    unsafe { c::debugfs_remove(dir.as_ptr()) }
}
