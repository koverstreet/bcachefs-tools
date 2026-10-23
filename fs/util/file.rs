// SPDX-License-Identifier: GPL-2.0

//! A file's operations from a trait. A type implements FileOps, under
//! #[vtable], and FileVTable::<P>::OPS is the file_operations of a file whose
//! private data is P - that type, or one holding it (FileData): each op filled
//! in only where it's implemented, release() always - it drops the private
//! data - and in-kernel .owner this module, so it isn't unloaded under an
//! open file. The kernel crate's own (MiscDevice,
//! debugfs's) are tied to their users; this is MiscDevice's shape, for any
//! file, in both builds - the tools serve debugfs files over http, through
//! the shim's debugfs and these same ops.
//!
//! The private data is a Pin<KBox<T>>: made by open() for a file opened by
//! path (debugfs's), or handed to NewFile::new() for a file made for
//! userspace with its object already there (thread_with_file's) - an
//! anonymous inode's, on a reserved fd that install() hands over.
//!
//! UserBuf is a read() or write()'s buffer in userspace, and how far the call
//! has got: what it moved is what it returns, even if it then fails. Its
//! copies are copy_to_user()'s, which may fault and sleep, or - for copies
//! made under a spinlock - the nofault ones, of what fault_in_*() made
//! present.
//!
//! poll() is kernel only, and its queue is ours, not the kernel crate's
//! PollCondVar: that one's drop calls __wake_up_pollfree(), which isn't
//! exported to modules.

use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::KBox;
use crate::util::ffi::{c_char, Opaque};
use core::marker::PhantomData;
use core::pin::Pin;

#[cfg(kernel)]
use {
    crate::errcode::errptr_to_result,
    core::ffi::c_void,
    core::mem::MaybeUninit,
    core::ptr::NonNull,
    kernel::fs::file::FileDescriptorReservation,
    kernel::sync::aref::ARef,
    crate::util::kernel::current_comm,
};

use core::ffi::{c_int, c_uint};
// long, as the bindings have it: the kernel's is isize, core's i64
#[cfg(kernel)]
use kernel::ffi::{c_long as CLong, c_ulong as CUlong};
#[cfg(not(kernel))]
use core::ffi::{c_long as CLong, c_ulong as CUlong};

#[repr(transparent)]
pub struct File(Opaque<c::file>);

impl File {
    /// # Safety
    /// @f is a file, open for 'a.
    pub unsafe fn from_raw<'a>(f: *mut c::file) -> &'a File {
        unsafe { &*f.cast::<File>() }
    }

    pub fn as_ptr(&self) -> *mut c::file {
        self.0.as_ptr()
    }

    #[cfg(kernel)]
    pub fn nonblock(&self) -> bool {
        // SAFETY: as READ_ONCE(): fcntl() changes it
        let flags = unsafe { core::ptr::read_volatile(&raw const (*self.as_ptr()).f_flags) };
        flags & c::O_NONBLOCK != 0
    }
}

/// Implemented with cstruct_macros' #[vtable], which sets HAS_* for the ops
/// an impl has.
pub trait FileOps: Sized + Send + Sync + 'static {
    const HAS_OPEN:  bool = false;
    const HAS_READ:  bool = false;
    const HAS_WRITE: bool = false;
    const HAS_POLL:  bool = false;
    const HAS_FLUSH: bool = false;
    const HAS_IOCTL: bool = false;

    /// For a file opened by path: its private data.
    fn open(_inode: &Opaque<c::inode>, _file: &File) -> Result<Pin<KBox<Self>>, BchError> {
        unreachable!("not in the vtable")
    }

    fn read(&self, _file: &File, _buf: &mut UserBuf) -> Result<(), BchError> {
        unreachable!("not in the vtable")
    }

    fn write(&self, _file: &File, _buf: &mut UserBuf) -> Result<(), BchError> {
        unreachable!("not in the vtable")
    }

    #[cfg(kernel)]
    fn poll(&self, _file: &File, _pt: &PollTable<'_>) -> u32 {
        unreachable!("not in the vtable")
    }

    /// What close(2) returns.
    fn flush(&self, _file: &File) -> c_int {
        unreachable!("not in the vtable")
    }

    fn ioctl(&self, _file: &File, _cmd: c_uint, _arg: CUlong) -> CLong {
        unreachable!("not in the vtable")
    }
}

/// A file's private data: the T whose ops the file has, or something holding
/// one - as a thread's file holds the thread, and what it runs on.
pub trait FileData: Send + Sync + 'static {
    type Ops: FileOps;

    /// What FileOps::open() makes is the private data itself.
    const OPENABLE: bool = false;

    fn ops(&self) -> &Self::Ops;

    fn opened(_p: Pin<KBox<Self::Ops>>) -> Pin<KBox<Self>> {
        unreachable!("not OPENABLE")
    }
}

impl<T: FileOps> FileData for T {
    type Ops = T;
    const OPENABLE: bool = true;

    fn ops(&self) -> &T {
        self
    }

    fn opened(p: Pin<KBox<T>>) -> Pin<KBox<T>> {
        p
    }
}

pub struct FileVTable<P>(PhantomData<P>);

impl<P: FileData> FileVTable<P> {
    const OPS: c::file_operations = {
        // SAFETY: zero is NULL, "not implemented"
        let mut ops: c::file_operations = unsafe { core::mem::zeroed() };
        #[cfg(all(kernel, MODULE))]
        {
            ops.owner = &raw mut kernel::bindings::__this_module;
        }
        ops.release = Some(Self::release);
        if <P::Ops>::HAS_OPEN {
            assert!(P::OPENABLE, "open() makes the ops' type, not the private data");
            ops.open = Some(Self::open);
        }
        if <P::Ops>::HAS_READ {
            ops.read = Some(Self::read);
        }
        // the shim's file_operations has only open, release and read
        #[cfg(kernel)]
        {
            if <P::Ops>::HAS_WRITE {
                ops.write = Some(Self::write);
            }
            if <P::Ops>::HAS_POLL {
                ops.poll = Some(Self::poll);
            }
            if <P::Ops>::HAS_FLUSH {
                ops.flush = Some(Self::flush);
            }
            if <P::Ops>::HAS_IOCTL {
                ops.unlocked_ioctl = Some(Self::ioctl);
            }
        }
        ops
    };

    pub fn ops() -> &'static c::file_operations {
        &Self::OPS
    }

    unsafe extern "C" fn open(inode: *mut c::inode, file: *mut c::file) -> c_int {
        // SAFETY: a file being opened, and its inode
        let (inode, f) = unsafe { (&*inode.cast::<Opaque<c::inode>>(), File::from_raw(file)) };
        match <P::Ops>::open(inode, f) {
            Ok(p) => {
                unsafe { (*file).private_data = into_private(P::opened(p)) };
                0
            }
            Err(e) => e.class(),
        }
    }

    unsafe extern "C" fn release(_inode: *mut c::inode, file: *mut c::file) -> c_int {
        // SAFETY: open()'s or NewFile::new()'s, and the file's last use
        drop(unsafe { from_private::<P>((*file).private_data) });
        0
    }

    unsafe extern "C" fn read(file: *mut c::file, buf: *mut c_char, len: usize,
                              _ppos: *mut c::loff_t) -> isize {
        let mut u = UserBuf::new(buf, len);
        // SAFETY: an open file of P's
        let r = unsafe { file_data::<P>(file).ops().read(File::from_raw(file), &mut u) };
        u.ret(r)
    }

    #[cfg(kernel)]
    unsafe extern "C" fn write(file: *mut c::file, buf: *const c_char, len: usize,
                               _ppos: *mut c::loff_t) -> isize {
        let mut u = UserBuf::new(buf, len);
        // SAFETY: an open file of P's
        let r = unsafe { file_data::<P>(file).ops().write(File::from_raw(file), &mut u) };
        u.ret(r)
    }

    #[cfg(kernel)]
    unsafe extern "C" fn poll(file: *mut c::file, pt: *mut c::poll_table) -> c::__poll_t {
        // SAFETY: an open file of P's, and the table it's polled with
        unsafe { file_data::<P>(file).ops().poll(File::from_raw(file), &PollTable(pt, PhantomData)) }
    }

    #[cfg(kernel)]
    unsafe extern "C" fn flush(file: *mut c::file, _id: c::fl_owner_t) -> c_int {
        // SAFETY: an open file of P's
        unsafe { file_data::<P>(file).ops().flush(File::from_raw(file)) }
    }

    #[cfg(kernel)]
    unsafe extern "C" fn ioctl(file: *mut c::file, cmd: c_uint, arg: CUlong) -> CLong {
        // SAFETY: an open file of P's
        unsafe { file_data::<P>(file).ops().ioctl(File::from_raw(file), cmd, arg) }
    }
}

fn into_private<T>(p: Pin<KBox<T>>) -> *mut core::ffi::c_void {
    // SAFETY: from_private() takes it back, pinned
    KBox::into_raw(unsafe { Pin::into_inner_unchecked(p) }).cast()
}

/// # Safety
/// @p is into_private()'s, of a T, and no one else's.
unsafe fn from_private<T>(p: *mut core::ffi::c_void) -> Pin<KBox<T>> {
    unsafe { Pin::new_unchecked(KBox::from_raw(p.cast())) }
}

/// # Safety
/// @file is an open file of P's - its file_operations FileVTable::<P>'s.
pub unsafe fn file_data<'a, P: FileData>(file: *mut c::file) -> &'a P {
    unsafe { &*(*file).private_data.cast::<P>() }
}

/// A file made for userspace, not yet given it: install() does.
#[cfg(kernel)]
pub struct NewFile<P: FileData> {
    fd:   FileDescriptorReservation,
    file: ARef<kernel::fs::File>,
    _p:   PhantomData<P>,
}

#[cfg(kernel)]
impl<P: FileData> NewFile<P> {
    /// An anonymous inode's file, of @data, named for the caller: O_CLOEXEC,
    /// readable and writable as its ops are.
    pub fn new(data: Pin<KBox<P>>) -> Result<Self, BchError> {
        let name = current_comm();
        let flags = c::O_CLOEXEC | match (<P::Ops>::HAS_READ, <P::Ops>::HAS_WRITE) {
            (true,  true)  => c::O_RDWR,
            (true,  false) => c::O_RDONLY,
            (false, true)  => c::O_WRONLY,
            (false, false) => 0,
        };
        let fd = FileDescriptorReservation::get_unused_fd_flags(flags)?;

        let p = into_private(data);
        let file = unsafe {
            c::anon_inode_getfile(name.as_cstr().as_ptr().cast(), FileVTable::<P>::ops(), p, flags as c_int)
        };
        match errptr_to_result(NonNull::new(file).expect("a file, or an error")) {
            // SAFETY: anon_inode_getfile()'s ref, now ours
            Ok(f) => Ok(NewFile { fd, file: unsafe { ARef::from_raw(f.cast()) }, _p: PhantomData }),
            Err(e) => {
                // SAFETY: no file took it
                drop(unsafe { from_private::<P>(p) });
                Err(e)
            }
        }
    }

    pub fn data(&self) -> &P {
        // SAFETY: ours, and not released: no fd's installed
        unsafe { file_data::<P>(self.file.as_ptr()) }
    }

    /// A ref on the file, for the caller to fput().
    pub fn get_file(&self) -> NonNull<c::file> {
        ARef::into_raw(self.file.clone()).cast()
    }

    /// The fd - from here userspace may close it at any moment.
    pub fn install(self) -> c_int {
        let fd = self.fd.reserved_fd();
        self.fd.fd_install(self.file);
        fd as c_int
    }
}

#[cfg(kernel)]
pub struct PollTable<'a>(*mut c::poll_table, PhantomData<&'a ()>);

#[cfg(kernel)]
impl PollTable<'_> {
    pub fn wait(&self, file: &File, q: &PollQueue) {
        // SAFETY: the file being polled, its table, and a queue that lives
        // as long as the file
        unsafe { c::rust_poll_wait(file.as_ptr(), q.0.as_ptr(), self.0) };
    }
}

/// What poll() waits on: a wait_queue_head_t, poll_wait() taking nothing
/// else. It must live as long as the file - in its private data - which is
/// what makes it safe to free without POLLFREE: __fput() takes epoll's
/// entries off it, eventpoll_release(), before ->release() frees it.
#[cfg(kernel)]
#[repr(transparent)]
pub struct PollQueue(Opaque<c::wait_queue_head>);

// SAFETY: C's, which any thread may wait on and wake
#[cfg(kernel)]
unsafe impl Send for PollQueue {}
#[cfg(kernel)]
unsafe impl Sync for PollQueue {}

#[cfg(kernel)]
impl PollQueue {
    pub fn new() -> impl ::pin_init::PinInit<Self> {
        let key = ::kernel::static_lock_class!().as_ptr().cast();
        // SAFETY: init can't fail; the head's pinned from here on
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::__init_waitqueue_head((*slot).0.as_ptr(), c"poll".as_ptr().cast(), key);
                Ok(())
            })
        }
    }

    pub fn wake(&self) {
        unsafe { c::__wake_up(self.0.as_ptr(), kernel::task::TASK_NORMAL, 0, core::ptr::null_mut()) };
    }
}

/// A read() or write()'s buffer in userspace, and how far the call has got.
pub struct UserBuf {
    addr:   usize,
    len:    usize,
    copied: usize,
}

impl UserBuf {
    fn new(ubuf: *const c_char, len: usize) -> Self {
        UserBuf { addr: ubuf as usize, len, copied: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    fn ptr(&self) -> *mut c_char {
        self.addr as *mut c_char
    }

    fn advance(&mut self, n: usize) {
        self.addr   += n;
        self.len    -= n;
        self.copied += n;
    }

    /// What the call returns: what it moved, if anything, else @r's error.
    fn ret(&self, r: Result<(), BchError>) -> isize {
        if self.copied != 0 {
            self.copied as isize
        } else {
            r.map_or_else(|e| e.class() as isize, |()| 0)
        }
    }

    /// As much of @src as fits, by copy_to_user(), which may fault: how much
    /// went - short only if it faulted.
    pub fn copy_to(&mut self, src: &[u8]) -> usize {
        let n = src.len().min(self.len);
        let left = unsafe { c::copy_to_user(self.ptr().cast(), src.as_ptr().cast_mut().cast(), n as _) } as usize;
        self.advance(n - left);
        n - left
    }
}

#[cfg(kernel)]
impl UserBuf {
    /// How much of it is present to write, faulting it in as far as it can.
    pub fn fault_in_writeable(&self) -> usize {
        self.len - unsafe { c::rust_fault_in_writeable(self.ptr(), self.len) }
    }

    pub fn fault_in_readable(&self) -> usize {
        self.len - unsafe { c::rust_fault_in_readable(self.ptr(), self.len) }
    }

    /// All of @src, without faulting - or none of it, if any wasn't present.
    pub fn copy_to_nofault(&mut self, src: &[u8]) -> bool {
        let ok = src.len() <= self.len &&
            unsafe { c::copy_to_user_nofault(self.ptr().cast(), src.as_ptr().cast(), src.len()) } == 0;
        if ok {
            self.advance(src.len());
        }
        ok
    }

    /// @dst filled, without faulting - or not, if any wasn't present.
    pub fn copy_from_nofault(&mut self, dst: &mut [MaybeUninit<u8>]) -> bool {
        let ok = dst.len() <= self.len &&
            unsafe { c::copy_from_user_nofault(dst.as_mut_ptr().cast(), self.ptr().cast::<c_void>(), dst.len()) } == 0;
        if ok {
            self.advance(dst.len());
        }
        ok
    }
}
