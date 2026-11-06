// SPDX-License-Identifier: GPL-2.0

//! A file's operations from a trait. A type implements FileOps, under
//! #[vtable], and FileVTable::<P>::OPS is the file_operations of a file whose
//! state is P (FileData): each op filled in only where it's implemented,
//! release() always - FileData's - and in-kernel .owner this module, so it
//! isn't unloaded under an open file. P is the file's private data - the
//! ops' type, or one holding it - unless it says otherwise: a regular file's
//! ops are its inode's (vfs/file.rs). The kernel crate's own (MiscDevice,
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
use crate::util::ops::{from_raw, SelfOf};
use core::ffi::{c_int, CStr};
use core::pin::Pin;

#[cfg(kernel)]
use {
    crate::errcode::{errptr_to_result, loff_to_result, ret_to_result_void},
    crate::util::kernel::loff,
    crate::vfs::folio::Mapping,
    core::ffi::{c_uint, c_void},
    core::marker::PhantomData,
    core::mem::MaybeUninit,
    core::ptr::NonNull,
    kernel::fs::file::FileDescriptorReservation,
    kernel::sync::aref::ARef,
    crate::util::kernel::current_comm,
};

// long, as the bindings have it: the kernel's is isize, core's i64
#[cfg(kernel)]
use kernel::ffi::{c_long as CLong, c_ulong as CUlong};

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

    /// The dentry it was opened by.
    #[cfg(kernel)]
    pub fn dentry(&self) -> *mut c::dentry {
        // 6.18 made f_path a const union member, __f_path its writable alias
        #[cfg(linux_ge = "6.18")]
        unsafe { (*self.as_ptr()).__bindgen_anon_1.f_path.dentry }
        #[cfg(not(linux_ge = "6.18"))]
        unsafe { (*self.as_ptr()).f_path.dentry }
    }

    /// FMODE_*: set at open.
    #[cfg(kernel)]
    pub fn mode(&self) -> c::fmode_t {
        unsafe { (*self.as_ptr()).f_mode }
    }

    /// O_*: as READ_ONCE(), as fcntl() changes them.
    #[cfg(kernel)]
    pub fn flags(&self) -> u32 {
        unsafe { core::ptr::read_volatile(&raw const (*self.as_ptr()).f_flags) }
    }

    #[cfg(kernel)]
    pub fn nonblock(&self) -> bool {
        self.flags() & c::O_NONBLOCK != 0
    }

    #[cfg(kernel)]
    pub fn mapping(&self) -> Mapping {
        unsafe { Mapping::from_raw((*self.as_ptr()).f_mapping) }
    }

    /// Whether it has pages in the page cache.
    #[cfg(kernel)]
    pub fn has_pagecache(&self) -> bool {
        !self.mapping().is_empty()
    }

    /// Its page cache's dirty pages from @start to @end, inclusive, written and
    /// waited on, and a writeback error since this file last saw one:
    /// file_write_and_wait_range().
    #[cfg(kernel)]
    pub fn write_and_wait_range(&self, start: u64, end: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::file_write_and_wait_range(self.as_ptr(), loff(start), loff(end)) })
    }

    /// A writeback error since this file last saw one, now seen:
    /// file_check_and_advance_wb_err().
    #[cfg(kernel)]
    pub fn check_and_advance_wb_err(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::file_check_and_advance_wb_err(self.as_ptr()) })
    }

    /// SEEK_SET, SEEK_CUR or SEEK_END: the new position, at most the
    /// filesystem's s_maxbytes - generic_file_llseek().
    #[cfg(kernel)]
    pub fn generic_llseek(&self, offset: i64, whence: c_int) -> Result<u64, BchError> {
        loff_to_result(unsafe { c::generic_file_llseek(self.as_ptr(), offset, whence) })
    }

    /// generic_llseek(), within @max, SEEK_END from @eof:
    /// generic_file_llseek_size().
    #[cfg(kernel)]
    pub fn generic_llseek_size(&self, offset: i64, whence: c_int, max: u64, eof: u64)
        -> Result<u64, BchError>
    {
        loff_to_result(unsafe { c::generic_file_llseek_size(self.as_ptr(), offset, whence, loff(max), loff(eof)) })
    }

    /// The position set to @offset, EINVAL past @max: vfs_setpos().
    #[cfg(kernel)]
    pub fn setpos(&self, offset: u64, max: u64) -> Result<u64, BchError> {
        loff_to_result(unsafe { c::vfs_setpos(self.as_ptr(), loff(offset), loff(max)) })
    }

    /// Whether writes to it are synchronous: O_SYNC, O_DSYNC, or its inode's
    /// or filesystem's sync flag.
    #[cfg(kernel)]
    pub fn is_dsync(&self) -> bool {
        self.flags() & (c::__O_SYNC | c::O_DSYNC) != 0 ||
            unsafe { c::rust_is_sync((*self.as_ptr()).f_inode) }
    }

    /// For a write: its mtime and ctime to now, and setuid and setgid
    /// dropped - file_modified().
    #[cfg(kernel)]
    pub fn modified(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::file_modified(self.as_ptr()) })
    }

    /// file_update_time(): its mtime and ctime to now, for a write.
    #[cfg(kernel)]
    pub fn update_time(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::file_update_time(self.as_ptr()) })
    }

    /// file_remove_privs(): setuid, setgid and security capabilities dropped,
    /// for a write.
    #[cfg(kernel)]
    pub fn remove_privs(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::file_remove_privs(self.as_ptr()) })
    }

    /// The VFS's checks of a remap from @pos_in to @dst at @pos_out, and its
    /// flushes; @len shortened to what's to be remapped, 0 for nothing:
    /// generic_remap_file_range_prep().
    #[cfg(kernel)]
    pub fn remap_range_prep(&self, pos_in: u64, dst: &File, pos_out: u64, len: &mut u64, flags: u32)
        -> Result<(), BchError>
    {
        let mut l = loff(*len);
        let ret = ret_to_result_void(unsafe {
            c::generic_remap_file_range_prep(self.as_ptr(), loff(pos_in), dst.as_ptr(), loff(pos_out), &mut l, flags)
        });
        *len = l as u64;
        ret
    }

    /// file_accessed(): its atime touched.
    #[cfg(kernel)]
    pub fn accessed(&self) {
        unsafe { c::file_accessed(self.as_ptr()) }
    }
}

/// One read, from the start of the file at @path: enough for a sysfs
/// attribute, which hands over its value in one go.
pub fn read_file(path: &CStr, buf: &mut [u8]) -> Result<usize, BchError> {
    #[cfg(kernel)]
    unsafe {
        let file = c::filp_open(path.as_ptr().cast(), c::O_RDONLY as c_int, 0);
        let file = errptr_to_result(NonNull::new(file).expect("a file, or an error"))?.as_ptr();

        let mut pos = 0;
        let r = c::kernel_read(file, buf.as_mut_ptr().cast(), buf.len(), &mut pos);
        c::fput(file);
        if r < 0 { Err(BchError::from_raw(r as c_int)) } else { Ok(r as usize) }
    }
    #[cfg(not(kernel))]
    {
        use std::{io::Read, os::unix::ffi::OsStrExt};
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
        Ok(std::fs::File::open(path)?.read(buf)?)
    }
}

crate::vfs_ops! {
    pub trait FileOps: Send + Sync for c::file_operations, FileVTable<P: FileData> = P::Ops, |ops| {
        assert!(!<P::Ops>::HAS_OPEN || P::OPENABLE, "open() makes the ops' type, not the private data");
        ops.release = Some(Self::release);
        #[cfg(all(kernel, MODULE))]
        {
            ops.owner = &raw mut kernel::bindings::__this_module;
        }
    };

    /// For a file opened by path: its private data.
    fn open(inode: &Opaque<c::inode>, file: &File) -> Result<Pin<KBox<Self>>, BchError>
        => extern (inode: *mut c::inode, file: *mut c::file) -> c_int
    {
        // SAFETY: a file being opened, and its inode
        let (i, f) = unsafe { (&*inode.cast::<Opaque<c::inode>>(), File::from_raw(file)) };
        match <P::Ops>::open(i, f) {
            Ok(p) => {
                unsafe { (*file).private_data = into_private(P::opened(p)) };
                0
            }
            Err(e) => e.class(),
        }
    };
    fn read(&self = file, file: &File, buf: &mut UserBuf) -> Result<(), BchError>
        => extern (file: *mut c::file, buf: *mut c_char, len: usize, _ppos: *mut c::loff_t) -> isize
    {
        let mut u = UserBuf::new(buf, len);
        // SAFETY: an open file of P's
        let r = unsafe { P::ops_of(file).read(File::from_raw(file), &mut u) };
        u.ret(r)
    };
    #[cfg(kernel)]
    fn write(&self = file, file: &File, buf: &mut UserBuf) -> Result<(), BchError>
        => extern (file: *mut c::file, buf: *const c_char, len: usize, _ppos: *mut c::loff_t) -> isize
    {
        let mut u = UserBuf::new(buf, len);
        // SAFETY: an open file of P's
        let r = unsafe { P::ops_of(file).write(File::from_raw(file), &mut u) };
        u.ret(r)
    };
    #[cfg(kernel)]
    fn poll(&self = file, file: &File, pt: PollTable<'_>) -> u32;
    /// What close(2) returns.
    #[cfg(kernel)]
    fn flush(&self = file, file: &File) -> c_int
        => extern (file: *mut c::file, _id: c::fl_owner_t) -> c_int
    {
        // SAFETY: an open file of P's
        unsafe { P::ops_of(file).flush(File::from_raw(file)) }
    };
    #[cfg(kernel)]
    fn unlocked_ioctl(&self = file, file: &File, cmd: c_uint, arg: CUlong) -> CLong;
    #[cfg(kernel)]
    fn compat_ioctl(&self = file, file: &File, cmd: c_uint, arg: CUlong) -> CLong;
    /// The new position.
    #[cfg(kernel)]
    fn llseek(&self = file, file: &File, offset: i64, whence: c_int) -> Result<u64, BchError>;
    #[cfg(kernel)]
    fn iterate_shared(&self = file, file: &File, ctx: *mut c::dir_context) -> Result<(), BchError>;
    #[cfg(kernel)]
    fn fsync(&self = file, file: &File, start: i64, end: i64, datasync: c_int)
        -> Result<(), BchError>;
    #[cfg(kernel)]
    fn fallocate(&self = file, file: &File, mode: c_int, offset: i64, len: i64) -> Result<(), BchError>;
    /// How much was remapped.
    #[cfg(kernel)]
    fn remap_file_range(&self = file_in, file_in: &File, pos_in: i64, file_out: &File, pos_out: i64,
                        len: i64, remap_flags: c_uint) -> Result<u64, BchError>;
    /// A mapping about to be made: its vm_ops, and whether it may be.
    #[cfg(kernel)]
    #[cfg(linux_ge = "6.17")]
    fn mmap_prepare(desc: &mut c::vm_area_desc) -> Result<(), BchError>;
    #[cfg(kernel)]
    #[cfg(not(linux_ge = "6.17"))]
    fn mmap(&self = file, file: &File, vma: &mut c::vm_area_struct) -> Result<(), BchError>;
}

from_raw! {
    &'a File = *mut c::file => |r| File::from_raw(r);
}

#[cfg(kernel)]
from_raw! {
    PollTable<'a> = *mut c::poll_table => |r| PollTable(r, PhantomData);
}

// SAFETY: SelfOf's: a file of FileVTable::<P>'s is P's, its ops P::ops_of()'s
impl<P: FileData> SelfOf<P> for *mut c::file {
    type Out = P::Ops;
    unsafe fn self_of<'a>(&self) -> &'a P::Ops { unsafe { P::ops_of(*self) } }
}

/// A file's state: by default its private data - the T whose ops the file
/// has, or something holding one, as a thread's file holds the thread, and
/// what it runs on - made by open() or NewFile::new(), dropped at release.
pub trait FileData: Sized + Send + Sync + 'static {
    type Ops: FileOps;

    /// What FileOps::open() makes is the private data itself.
    const OPENABLE: bool = false;

    fn ops(&self) -> &Self::Ops;

    /// The ops of @file.
    ///
    /// # Safety
    /// @file is an open file of this type's - FileVTable::<Self>'s.
    unsafe fn ops_of<'a>(file: *mut c::file) -> &'a Self::Ops {
        unsafe { file_data::<Self>(file) }.ops()
    }

    /// The file's last use: its private data dropped.
    ///
    /// # Safety
    /// As ops_of()'s, and nothing of @file used after.
    unsafe fn release(file: *mut c::file) {
        // SAFETY: open()'s or NewFile::new()'s
        drop(unsafe { from_private::<Self>((*file).private_data) });
    }

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

impl<P: FileData> FileVTable<P> {
    unsafe extern "C" fn release(_inode: *mut c::inode, file: *mut c::file) -> c_int {
        // SAFETY: an open file of P's, its last use
        unsafe { P::release(file) };
        0
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
            c::anon_inode_getfile(name.as_cstr().as_ptr().cast(), &FileVTable::<P>::OPS, p, flags as c_int)
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
        let head = crate::new_waitqueue!("poll");
        // SAFETY: the head, which it's transparently
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                crate::util::alloc::init_at(head, slot.cast())
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
