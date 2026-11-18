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
use crate::os::ops::{from_raw, SelfOf};
use core::ffi::{c_int, CStr};
use core::marker::PhantomData;
use core::ops::Deref;
use core::pin::Pin;

#[cfg(kernel)]
use {
    crate::errcode::{errptr_to_result, loff_to_result, ret_to_result_void},
    crate::os::kernel::loff,
    crate::os::dentry::Dentry,
    crate::os::dir_context::DirContext,
    crate::os::folio::{FsMapping, Mapping},
    crate::os::iov_iter::IovIter,
    crate::os::kiocb::Kiocb,
    crate::os::super_block::IsInode,
    core::ffi::{c_uint, c_void},
    core::mem::MaybeUninit,
    core::ptr::NonNull,
    kernel::fs::file::FileDescriptorReservation,
    kernel::sync::aref::{ARef, AlwaysRefCounted},
    crate::os::kernel::current_comm,
};

// long, as the bindings have it: the kernel's is isize, core's i64
#[cfg(kernel)]
use kernel::ffi::{c_long as CLong, c_ulong as CUlong};

/// A file, open: of D's - its FileData, whose table dispatched it - or of
/// any type, Any's.
#[repr(transparent)]
pub struct File<D = Any>(Opaque<c::file>, PhantomData<D>);

/// What a File, Dentry or VfsInode of no one type is of, or on.
pub enum Any {}

impl<D> File<D> {
    pub fn as_ptr(&self) -> *mut c::file {
        self.0.as_ptr()
    }
}

// f_count's, as the kernel crate's File's
#[cfg(kernel)]
unsafe impl<D> AlwaysRefCounted for File<D> {
    fn inc_ref(&self) {
        unsafe { kernel::bindings::get_file(self.as_ptr().cast()) };
    }

    unsafe fn dec_ref(obj: NonNull<Self>) {
        unsafe { kernel::bindings::fput(obj.cast().as_ptr()) }
    }
}

impl<D: FileData> File<D> {
    /// # Safety
    /// @f is an open file of D's - its file_operations FileVTable::<D>'s -
    /// for 'a.
    pub unsafe fn of_raw<'a>(f: *mut c::file) -> &'a Self {
        unsafe { &*f.cast::<Self>() }
    }

    pub fn ops(&self) -> &D::Ops {
        unsafe { D::ops_of(self.as_ptr()) }
    }
}

impl<D: FileData> Deref for File<D> {
    type Target = File;

    fn deref(&self) -> &File {
        unsafe { File::from_raw(self.as_ptr()) }
    }
}

/// A file of D's being opened, by D's check_open(): no one's yet but the
/// opener's. Typed, as an open file is, because D's state isn't the private
/// data open would set - it's CHECKS_OPEN.
#[cfg(kernel)]
pub struct Opening<'a, D: FileData>(&'a File<D>);

#[cfg(kernel)]
impl<'a, D: FileData> Opening<'a, D> {
    /// # Safety
    /// @file is being opened, in its f_op->open(), its table FileVTable::<D>'s,
    /// and D CHECKS_OPEN.
    pub unsafe fn from_raw(file: *mut c::file) -> Self {
        Opening(unsafe { File::of_raw(file) })
    }

    pub fn file(&self) -> &'a File<D> {
        self.0
    }

    /// O_DIRECT allowed on it: FMODE_CAN_ODIRECT.
    pub fn set_can_odirect(&self) {
        unsafe { (*self.0.as_ptr()).f_mode |= c::FMODE_CAN_ODIRECT }
    }

    /// The open's generic checks - a large file, opened without
    /// O_LARGEFILE, refused: generic_file_open().
    pub fn generic_open(&self) -> Result<(), BchError> {
        let f = self.0.as_ptr();
        ret_to_result_void(unsafe { c::generic_file_open((*f).f_inode, f) })
    }
}

/// The file ->tmpfile() opens: its dentry negative, the new inode's to be -
/// in a directory of the filesystem whose inodes are I.
#[cfg(kernel)]
pub struct OpeningTmpfile<'a, I>(&'a File, PhantomData<I>);

#[cfg(kernel)]
impl<'a, I: IsInode> OpeningTmpfile<'a, I> {
    pub fn dentry(&self) -> Dentry<'a, I> {
        unsafe { Dentry::borrow_raw(self.0.dentry()) }
    }

    /// @inode's, opened: d_mark_tmpfile(), the dentry instantiated with it,
    /// finish_open_simple().
    pub fn finish(self, inode: ARef<I>) -> Result<(), BchError> {
        let vinode = core::ptr::from_ref::<I>(&inode).cast_mut().cast::<c::inode>();
        unsafe { c::d_mark_tmpfile(self.0.as_ptr(), vinode) };
        self.dentry().instantiate(inode);
        ret_to_result_void(unsafe { c::finish_open_simple(self.0.as_ptr(), 0) })
    }
}

#[cfg(kernel)]
from_raw! {
    /// ->tmpfile()'s: a file being opened in a directory of I's filesystem.
    OpeningTmpfile<'a, I> [I: IsInode] = *mut c::file => |r| OpeningTmpfile(File::from_raw(r), PhantomData);
}

/// A file of a filesystem's own: its inode one of the filesystem's.
///
/// # Safety
/// Every open file of Self's has an Inode as its f_inode.
#[cfg(kernel)]
pub unsafe trait FsFile: FileData {
    type Inode: IsInode;
}

#[cfg(kernel)]
impl<D: FsFile> File<D> {
    pub fn inode(&self) -> &D::Inode {
        unsafe { &*(*self.as_ptr()).f_inode.cast::<D::Inode>() }
    }

    /// The dentry it was opened by: its inode's.
    pub fn dentry(&self) -> Dentry<'_, D::Inode> {
        unsafe { Dentry::borrow_raw(File::<Any>::dentry(self)) }
    }

    /// Its inode's page cache: a filesystem's own file's f_mapping.
    pub fn mapping(&self) -> Mapping<D::Inode>
        where D::Inode: FsMapping
    {
        self.inode().mapping()
    }
}

impl File {
    /// # Safety
    /// @f is a file, open for 'a.
    pub unsafe fn from_raw<'a>(f: *mut c::file) -> &'a File {
        unsafe { &*f.cast::<File>() }
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

    /// Its inode's device number: a device node's.
    #[cfg(kernel)]
    pub fn rdev(&self) -> c::dev_t {
        // SAFETY: an open file has an inode
        unsafe { (*(*self.as_ptr()).f_inode).i_rdev }
    }

    /// Being opened, as a file with no position: nonseekable_open().
    #[cfg(kernel)]
    pub fn nonseekable_open(&self, inode: &Opaque<c::inode>) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::nonseekable_open(inode.as_ptr(), self.as_ptr()) })
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

    /// Whether @len bytes at @pos may be read - the range, and mandatory
    /// locks and LSMs: rw_verify_area().
    #[cfg(kernel)]
    pub fn verify_read(&self, pos: c::loff_t, len: usize) -> Result<(), BchError> {
        crate::errcode::ret_to_result_void(unsafe {
            c::rw_verify_area(c::READ as c_int, self.as_ptr(), &pos, len)
        })
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
        assert!(!(<P::Ops>::HAS_OPEN && P::CHECKS_OPEN), "open() is FileOps' or FileData's, not both");
        ops.release = Some(Self::release);
        #[cfg(kernel)]
        if P::CHECKS_OPEN {
            ops.open = Some(Self::check_open);
        }
        #[cfg(all(kernel, MODULE))]
        if <P::Ops>::MODULE_REF {
            ops.owner = &raw mut kernel::bindings::__this_module;
        }
        #[cfg(kernel)]
        if <P::Ops>::PAGECACHE {
            ops.splice_read  = Some(c::filemap_splice_read);
            ops.splice_write = Some(c::iter_file_splice_write);
            #[cfg(CONFIG_TRANSPARENT_HUGEPAGE)]
            {
                ops.get_unmapped_area = Some(c::thp_get_unmapped_area);
            }
        }
    };
    type Data: FileData<Ops = Self>;
    // Each open takes a reference on the module - not where something that
    // outlives the file holds one, as a mounted filesystem's mount does
    const MODULE_REF: bool = true;
    // A file in the page cache: the kernel's splice, and THP mappings
    const PAGECACHE: bool = false;

    /// For a file opened by path: its private data. The file isn't yet of
    /// Data's - that's what this makes.
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
    fn read(&self = file, file: &File<Self::Data>, buf: &mut UserBuf) -> Result<(), BchError>
        => extern (file: *mut c::file, buf: *mut c_char, len: usize, _ppos: *mut c::loff_t) -> isize
    {
        let mut u = UserBuf::new(buf, len);
        // SAFETY: an open file of P's
        let r = unsafe { P::ops_of(file).read(File::<P>::of_raw(file), &mut u) };
        u.ret(r)
    };
    #[cfg(kernel)]
    fn write(&self = file, file: &File<Self::Data>, buf: &mut UserBuf) -> Result<(), BchError>
        => extern (file: *mut c::file, buf: *const c_char, len: usize, _ppos: *mut c::loff_t) -> isize
    {
        let mut u = UserBuf::new(buf, len);
        // SAFETY: an open file of P's
        let r = unsafe { P::ops_of(file).write(File::<P>::of_raw(file), &mut u) };
        u.ret(r)
    };
    #[cfg(kernel)]
    fn read_iter(req: &mut Kiocb<'_, Self::Data>, iter: &mut IovIter<'_>) -> isize
        => extern (iocb: *mut c::kiocb, iter: *mut c::iov_iter) -> isize
    {
        let (mut k, mut i) = unsafe { (Kiocb::<P>::from_raw(iocb), IovIter::from_raw(iter)) };
        <P::Ops>::read_iter(&mut k, &mut i)
    };
    #[cfg(kernel)]
    fn write_iter(req: &mut Kiocb<'_, Self::Data>, iter: &mut IovIter<'_>) -> isize
        => extern (iocb: *mut c::kiocb, iter: *mut c::iov_iter) -> isize
    {
        let (mut k, mut i) = unsafe { (Kiocb::<P>::from_raw(iocb), IovIter::from_raw(iter)) };
        <P::Ops>::write_iter(&mut k, &mut i)
    };
    #[cfg(kernel)]
    fn poll(&self = file, file: &File<Self::Data>, pt: PollTable<'_>) -> u32;
    /// What close(2) returns.
    #[cfg(kernel)]
    fn flush(&self = file, file: &File<Self::Data>) -> c_int
        => extern (file: *mut c::file, _id: c::fl_owner_t) -> c_int
    {
        // SAFETY: an open file of P's
        unsafe { P::ops_of(file).flush(File::<P>::of_raw(file)) }
    };
    #[cfg(kernel)]
    fn unlocked_ioctl(&self = file, file: &File<Self::Data>, cmd: c_uint, arg: CUlong) -> CLong;
    #[cfg(kernel)]
    fn compat_ioctl(&self = file, file: &File<Self::Data>, cmd: c_uint, arg: CUlong) -> CLong;
    /// The new position.
    #[cfg(kernel)]
    fn llseek(&self = file, file: &File<Self::Data>, offset: i64, whence: c_int) -> Result<u64, BchError>;
    #[cfg(kernel)]
    fn iterate_shared(&self = file, file: &File<Self::Data>, ctx: DirContext<'_>) -> Result<(), BchError>;
    #[cfg(kernel)]
    fn fsync(&self = file, file: &File<Self::Data>, start: i64, end: i64, datasync: c_int)
        -> Result<(), BchError>;
    #[cfg(kernel)]
    fn fallocate(&self = file, file: &File<Self::Data>, mode: c_int, offset: i64, len: i64)
        -> Result<(), BchError>;
    /// How much was remapped. @file_out is Data's too: the VFS remaps only
    /// within a filesystem, between regular files.
    #[cfg(kernel)]
    fn remap_file_range(&self = file_in, file_in: &File<Self::Data>, pos_in: i64,
                        file_out: &File<Self::Data>, pos_out: i64, len: i64, remap_flags: c_uint)
        -> Result<u64, BchError>;
    /// A mapping about to be made: its vm_ops, and whether it may be.
    #[cfg(kernel)]
    #[cfg(linux_ge = "6.17")]
    fn mmap_prepare(vma: crate::os::vm::VmArea<'_, Self::Data>) -> Result<(), BchError>;
    #[cfg(kernel)]
    #[cfg(not(linux_ge = "6.17"))]
    fn mmap(&self = file, file: &File<Self::Data>, vma: crate::os::vm::VmArea<'_, Self::Data>)
        -> Result<(), BchError>;
}

from_raw! {
    &'a File = *mut c::file => |r| File::from_raw(r);
    /// An op's file, dispatched by D's table: of D's.
    &'a File<D> [D: FileData] = *mut c::file => |r| File::of_raw(r);
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
    type Ops: FileOps<Data = Self>;

    /// What FileOps::open() makes is the private data itself.
    const OPENABLE: bool = false;

    /// Opened, a file of this type has nothing to make - its state is had
    /// some other way, as a regular file's is its inode's - only check_open()
    /// to run.
    const CHECKS_OPEN: bool = false;

    #[cfg(kernel)]
    fn check_open(_file: Opening<'_, Self>) -> Result<(), BchError> {
        unreachable!("not CHECKS_OPEN")
    }

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

impl<T: FileOps<Data = T>> FileData for T {
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
    #[cfg(kernel)]
    unsafe extern "C" fn check_open(_inode: *mut c::inode, file: *mut c::file) -> c_int {
        crate::errcode::ret_to_class(P::check_open(unsafe { Opening::from_raw(file) }))
    }

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

    /// A ref on the file.
    pub fn get_file(&self) -> ARef<File<P>> {
        unsafe { ARef::from_raw(ARef::into_raw(self.file.clone()).cast()) }
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
        unsafe { kernel::bindings::poll_wait(file.as_ptr(), q.0.as_ptr(), self.0) };
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
        let key = ::kernel::static_lock_class!().as_ptr();
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::__init_waitqueue_head(slot.cast(), c"poll".as_ptr().cast(), key.cast());
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
        self.len - unsafe { c::fault_in_writeable(self.ptr(), self.len) }
    }

    pub fn fault_in_readable(&self) -> usize {
        self.len - unsafe { c::fault_in_readable(self.ptr(), self.len) }
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
