// SPDX-License-Identifier: GPL-2.0

//! Kobjects, for userspace: the kernel port's interface - os/linux_kernel/
//! kobject.rs - over a sysfs of our own, the tree http.rs serves. A kobject's
//! type is made at compile time from its owner's KobjOwner impl, as the
//! kernel port's is: its release, how its files read, and its files as
//! attribute groups, a named group a subdirectory of the kobject's with no
//! kobject of its own.

use core::ffi::{c_char, CStr};
use core::marker::PhantomData;
use core::ptr::{self, null_mut, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering::{AcqRel, Acquire, Relaxed, Release}};
use std::ffi::CString;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};

use crate::c;
use crate::errcode::BchError;
use crate::os::kernel::PAGE_SIZE;
use crate::os::sysfs::{self, obj_of, Attribute, Bin, Dir, Files, ShowFn, StoreFn};
use crate::util::place::{container_of_raw, Enclosing, Place, Placed};
use crate::util::Printbuf;

/// A Bin's file.
pub struct BinFile {
    attr:  Attribute,
    read:  fn(&Kobject, &mut [u8], u64) -> Result<usize, BchError>,
    write: fn(&Kobject, &[u8], u64) -> Result<usize, BchError>,
}

impl BinFile {
    pub(crate) const fn new<B: Bin>(name: &'static CStr, mode: u16) -> Self {
        BinFile { attr: sysfs::attribute(name, mode), read: bin_read::<B>, write: bin_write::<B> }
    }
}

fn bin_read<B: Bin>(kobj: &Kobject, buf: &mut [u8], off: u64) -> Result<usize, BchError> {
    let obj = unsafe { obj_of::<B::Dir>(ptr::from_ref(kobj).cast_mut()) };
    B::read(obj, buf, off)
}

fn bin_write<B: Bin>(kobj: &Kobject, buf: &[u8], off: u64) -> Result<usize, BchError> {
    let obj = unsafe { obj_of::<B::Dir>(ptr::from_ref(kobj).cast_mut()) };
    B::write(obj, buf, off)
}

/// A group's bin files: NULL terminated, N files plus one.
#[repr(transparent)]
pub struct BinFiles<const N: usize>([*const BinFile; N]);

unsafe impl<const N: usize> Sync for BinFiles<N> {}

impl<const N: usize> BinFiles<N> {
    pub const fn new<const M: usize>(files: [&'static BinFile; M]) -> Self {
        assert!(M + 1 == N);

        let mut ptrs = [core::ptr::null(); N];
        let mut i = 0;
        while i < M {
            ptrs[i] = files[i];
            i += 1;
        }
        BinFiles(ptrs)
    }
}

/// An attribute group: a kobject's files, or with a name a subdirectory of
/// them.
pub struct Group {
    name:  Option<&'static CStr>,
    attrs: *const *const Attribute,
    bin:   *const *const BinFile,
}

unsafe impl Sync for Group {}

impl Group {
    pub const fn new<const N: usize>(name: Option<&'static CStr>, files: &'static Files<N>) -> Self {
        Group { name, attrs: files.as_ptr().cast_const().cast(), bin: core::ptr::null() }
    }

    pub const fn with_bin<const N: usize, const B: usize>(name: Option<&'static CStr>, files: &'static Files<N>,
                                                          bin: &'static BinFiles<B>) -> Self {
        Group { name, attrs: files.as_ptr().cast_const().cast(), bin: bin.0.as_ptr() }
    }
}

/// A kobject's groups, for its type: NULL terminated, N groups plus one.
#[repr(transparent)]
pub struct Groups<const N: usize>([*const Group; N]);

unsafe impl<const N: usize> Sync for Groups<N> {}

impl<const N: usize> Groups<N> {
    pub const fn new<const M: usize>(groups: [&'static Group; M]) -> Self {
        assert!(M + 1 == N);

        let mut ptrs = [core::ptr::null(); N];
        let mut i = 0;
        while i < M {
            ptrs[i] = groups[i];
            i += 1;
        }
        Groups(ptrs)
    }

    pub const fn list(&'static self) -> GroupList {
        GroupList(self.0.as_ptr())
    }
}

/// Groups<N>, its N forgotten: what a kobject's type points to.
#[derive(Clone, Copy)]
pub struct GroupList(*const *const Group);

/// Where a kobject is, as a type: what it's the kobject of.
///
/// # Safety
/// owner() of a kobject at this place is the Owner it's in.
pub(crate) unsafe trait KobjPlace {
    type Owner;

    /// # Safety
    /// @kobj is at this place.
    unsafe fn owner(kobj: *mut Kobject) -> *mut Self::Owner;
}

unsafe impl<A: Place<Type = Self, Base: Enclosing<Visible = A::Base>>> KobjPlace for Placed<Kobject, A> {
    type Owner = A::Base;

    unsafe fn owner(kobj: *mut Kobject) -> *mut A::Base {
        unsafe { container_of_raw(kobj.cast::<Self>()) }
    }
}

/// What a kobject is the kobject of: its type's.
///
/// # Safety
/// Its Dir's kobject was made with Kobject::new::<Self>(); release() frees
/// what it's given.
pub(crate) unsafe trait KobjOwner: Sized + 'static {
    const GROUPS: GroupList;
    type Dir: Dir<Obj = Self>;

    /// The last ref's been put.
    ///
    /// # Safety
    /// Nothing uses @this after.
    unsafe fn release(this: NonNull<Self>);
}

/// A kobject's type: the kernel's kobj_type.
struct KobjType {
    release: unsafe fn(*mut Kobject),
    show:    ShowFn,
    store:   StoreFn,
    groups:  GroupList,
}

struct Ktype<T>(PhantomData<T>);

impl<T: KobjOwner> Ktype<T> {
    const KTYPE: KobjType = KobjType {
        release: Self::release,
        show:    sysfs::show::<T::Dir>,
        store:   sysfs::store::<T::Dir>,
        groups:  T::GROUPS,
    };

    unsafe fn release(kobj: *mut Kobject) {
        unsafe { T::release(NonNull::new_unchecked(<T::Dir as Dir>::Kobj::owner(kobj))) }
    }
}

/// A kobject, in what it's the kobject of.
pub struct Kobject {
    ktype:    AtomicPtr<KobjType>,
    refs:     AtomicU32,
    in_sysfs: AtomicBool,
    /// add()'s, freed by the last put
    name:     AtomicPtr<c_char>,
    parent:   AtomicPtr<Kobject>,
}

unsafe impl Send for Kobject {}
unsafe impl Sync for Kobject {}

unsafe impl ::pin_init::Zeroable for Kobject {}

/// Every kobject in sysfs, in the order they were added: a directory's
/// subdirectories are those whose parent it is, and the one with none is the
/// root.
struct Tree(Vec<*const Kobject>);

// SAFETY: what it points to is TREE's
unsafe impl Send for Tree {}

static TREE: Mutex<Tree> = Mutex::new(Tree(Vec::new()));

fn lock() -> MutexGuard<'static, Tree> {
    TREE.lock().expect("sysfs lock poisoned")
}

impl Tree {
    fn root(&self) -> Option<&Kobject> {
        self.0.iter().map(|&k| unsafe { &*k }).find(|k| k.parent.load(Relaxed).is_null())
    }

    fn children<'a>(&'a self, dir: &'a Kobject) -> impl Iterator<Item = &'a Kobject> {
        self.0.iter().map(|&k| unsafe { &*k }).filter(move |k| ptr::eq(k.parent.load(Relaxed), dir))
    }
}

impl Kobject {
    /// kobject_init(), with T's type: holding the ref the owner's last put()
    /// drops.
    ///
    /// # Safety
    /// For T's kobject, its Dir's Kobj.
    pub(crate) unsafe fn new<T: KobjOwner>() -> impl ::pin_init::PinInit<Self> {
        unsafe {
            ::pin_init::pin_init_from_closure(|slot: *mut Self| {
                slot.write_bytes(0, 1);
                (*slot).init::<T>();
                Ok(())
            })
        }
    }

    /// new(), for one already in place, zeroed: a static's.
    ///
    /// # Safety
    /// As new()'s.
    pub(crate) unsafe fn init<T: KobjOwner>(&self) {
        self.ktype.store(ptr::from_ref(&Ktype::<T>::KTYPE).cast_mut(), Relaxed);
        self.refs.store(1, Relaxed);
    }

    fn ktype(&self) -> &'static KobjType {
        let t = self.ktype.load(Relaxed);
        assert!(!t.is_null(), "kobject used before it was initialized");
        unsafe { &*t }
    }

    fn name(&self) -> &CStr {
        let name = self.name.load(Relaxed);
        if name.is_null() { c"" } else { unsafe { CStr::from_ptr(name) } }
    }

    /// In sysfs: added, and not deleted since.
    pub fn in_sysfs(&self) -> bool {
        self.in_sysfs.load(Acquire)
    }

    /// Into sysfs as @name, in @parent's directory - or at the top, without
    /// one: its groups' files with it.
    pub fn add(&self, parent: Option<&Kobject>, name: &CStr) -> Result<(), BchError> {
        self.name.store(CString::from(name).into_raw(), Relaxed);

        let mut tree = lock();
        match parent {
            Some(parent) => {
                parent.get();
                self.parent.store(ptr::from_ref(parent).cast_mut(), Relaxed);
            }
            None => assert!(tree.root().is_none(), "kobject {name:?} added as a second root"),
        }
        tree.0.push(self);
        self.in_sysfs.store(true, Release);
        Ok(())
    }

    /// A link in its directory, @name, to @target's: userspace's sysfs has no
    /// links.
    pub fn link(&self, _target: &Kobject, _name: &CStr) -> Result<(), BchError> {
        Ok(())
    }

    /// link()'s @name, removed.
    pub fn unlink(&self, _name: &CStr) {}

    /// A KOBJ_CHANGE uevent: nothing in userspace listens for one.
    pub fn uevent_change(&self, _env: &[&CStr]) -> Result<(), BchError> {
        Ok(())
    }

    /// Out of sysfs, if it's there.
    pub fn del(&self) {
        if !self.in_sysfs() {
            return;
        }

        {
            let mut tree = lock();
            match tree.0.iter().position(|&k| ptr::eq(k, self)) {
                Some(i) => { tree.0.remove(i); }
                None    => eprintln!("kobject {:?} not found in sysfs when deleting it", self.name()),
            }
            self.in_sysfs.store(false, Release);
        }

        let parent = self.parent.swap(null_mut(), Relaxed);
        if !parent.is_null() {
            unsafe { Kobject::put(parent) };
        }
    }

    /// A ref, for put() to give back.
    pub fn get(&self) {
        let _ = self.ktype();
        self.refs.fetch_add(1, Relaxed);
    }

    /// A ref given back: the last releases the owner - frees it.
    ///
    /// # Safety
    /// The caller holds the ref; if it's the last, nothing uses the owner
    /// after.
    pub unsafe fn put(this: *const Kobject) {
        let k = unsafe { &*this };
        let t = k.ktype();

        if k.refs.fetch_sub(1, AcqRel) != 1 {
            return;
        }

        /* remove from sysfs if the caller did not do it */
        k.del();

        let name = k.name.swap(null_mut(), Relaxed);
        if !name.is_null() {
            drop(unsafe { CString::from_raw(name) });
        }

        unsafe { (t.release)(this.cast_mut()) };
    }
}

// ── Reading sysfs: http.rs's ───────────────────────────────────────────────

/// What a NULL terminated array of pointers points to.
fn null_terminated<'a, T: 'a>(mut p: *const *const T) -> impl Iterator<Item = &'a T> {
    core::iter::from_fn(move || unsafe {
        if p.is_null() || (*p).is_null() {
            return None;
        }
        let i = &**p;
        p = p.add(1);
        Some(i)
    })
}

fn groups(k: &Kobject) -> impl Iterator<Item = &'static Group> + use<> {
    null_terminated(k.ktype().groups.0)
}

fn group_attrs(g: &Group) -> impl Iterator<Item = &'static Attribute> + use<> {
    null_terminated(g.attrs)
}

fn group_bin_attrs(g: &Group) -> impl Iterator<Item = &'static BinFile> + use<> {
    null_terminated(g.bin)
}

/// A kobject's own files, its unnamed groups': its named groups are
/// subdirectories.
fn files(k: &Kobject) -> impl Iterator<Item = &'static Group> + use<> {
    groups(k).filter(|g| g.name.is_none())
}

enum Entry<'a> {
    Dir(&'a Kobject),
    Group(&'static Group),
    File(&'a Kobject, &'static Attribute),
    Bin(&'a Kobject, &'static BinFile),
}

/// The file or directory at @path: a file's kobject is its directory's, or
/// its named group's.
fn lookup<'a>(tree: &'a Tree, path: &str) -> Result<Entry<'a>, BchError> {
    let mut dir = tree.root().ok_or(BchError::from(c::ENOENT))?;
    let mut grp: Option<&'static Group> = None;

    let names: Vec<&str> = path.split('/').filter(|n| !n.is_empty()).collect();
    for (i, &name) in names.iter().enumerate() {
        let is = |n: &CStr| n.to_bytes() == name.as_bytes();

        if i + 1 == names.len() {
            let in_dir: Vec<_> = match grp {
                Some(g) => vec![g],
                None    => files(dir).collect(),
            };
            if let Some(a) = in_dir.iter().flat_map(|g| group_attrs(g)).find(|a| is(a.name)) {
                return Ok(Entry::File(dir, a));
            }
            if let Some(b) = in_dir.iter().flat_map(|g| group_bin_attrs(g)).find(|b| is(b.attr.name)) {
                return Ok(Entry::Bin(dir, b));
            }
        }

        if grp.is_some() {
            return Err(c::ENOENT.into());
        }

        if let Some(g) = groups(dir).find(|g| g.name.is_some_and(is)) {
            grp = Some(g);
        } else if let Some(child) = tree.children(dir).find(|k| is(k.name())) {
            dir = child;
        } else {
            return Err(c::ENOENT.into());
        }
    }

    Ok(match grp {
        Some(g) => Entry::Group(g),
        None    => Entry::Dir(dir),
    })
}

/// Whether @attr's mode lets it be read, or written - @mask: kernfs won't
/// open it otherwise.
fn allowed(attr: &Attribute, mask: u16) -> Result<(), BchError> {
    match attr.mode & mask {
        0 => Err(c::EACCES.into()),
        _ => Ok(()),
    }
}

fn readable(attr: &Attribute) -> Result<(), BchError> {
    allowed(attr, 0o444)
}

/// @data, written whole to the file at @path in sysfs: a store, or a bin
/// file's writes.
pub fn write_file(path: &str, data: &[u8]) -> Result<(), BchError> {
    let tree = lock();

    match lookup(&tree, path)? {
        Entry::File(k, a) => {
            allowed(a, 0o222)?;
            let s = core::str::from_utf8(data).map_err(|_| BchError::from(c::EINVAL))?;
            (k.ktype().store)(k, a, s)
        }
        Entry::Bin(k, b) => {
            allowed(&b.attr, 0o222)?;
            let mut off = 0;
            while off < data.len() {
                match (b.write)(k, &data[off..], off as u64)? {
                    0 => break,
                    n => off += n,
                }
            }
            Ok(())
        }
        Entry::Dir(_) | Entry::Group(_) => Err(BchError::from_raw(-libc::EISDIR)),
    }
}

/// The file at @path in sysfs, read whole - or the directory there, listed
/// as HTML - into @out.
pub fn read_or_html_dirlist(path: &str, out: &mut Vec<u8>) -> Result<(), BchError> {
    let tree = lock();

    match lookup(&tree, path)? {
        Entry::Bin(k, b) => {
            readable(&b.attr)?;
            let mut buf = vec![0u8; PAGE_SIZE];
            let mut off = 0;
            while let Ok(n @ 1..) = (b.read)(k, &mut buf, off) {
                out.extend_from_slice(&buf[..n]);
                off += n as u64;
            }
        }
        Entry::Group(g) => {
            let _ = writeln!(out, "<html>\n<p> {path} </p>");

            let mut nr = 0;
            for name in group_attrs(g).map(|a| a.name).chain(group_bin_attrs(g).map(|b| b.attr.name)) {
                let name = name.to_string_lossy();
                let _ = writeln!(out, "<p> <a href={path}/{name}>{name}</a></p>");
                nr += 1;
            }

            let _ = writeln!(out, "<p> {nr} files </p>\n</html>");
        }
        Entry::Dir(k) => {
            let _ = writeln!(out, "<html>\n<p> {path} </p>");

            let mut nr = 0;
            let mut subdir = |name: &CStr| {
                let name = name.to_string_lossy();
                let _ = writeln!(out, "<p> <a href={path}/{name}>{name}/</a></p>");
                nr += 1;
            };
            groups(k).filter_map(|g| g.name).for_each(&mut subdir);
            tree.children(k).map(Kobject::name).for_each(&mut subdir);

            if tree.root().is_some_and(|root| ptr::eq(root, k)) {
                let _ = writeln!(out, "<p> <a href=debug>debug/</a></p>");
            }

            let _ = writeln!(out, "<p> {nr} subdirs </p>");

            let mut nr = 0;
            for name in files(k).flat_map(group_attrs).map(|a| a.name)
                .chain(files(k).flat_map(group_bin_attrs).map(|b| b.attr.name))
            {
                let name = name.to_string_lossy();
                let _ = writeln!(out, "<p> <a href={path}/{name}>{name}</a></p>");
                nr += 1;
            }

            let _ = writeln!(out, "<p> {nr} files </p>\n</html>");
        }
        Entry::File(k, a) => {
            readable(a)?;
            let mut buf = Printbuf::new();
            (k.ktype().show)(k, a, &mut buf)?;
            if buf.allocation_failure() {
                return Err(c::ENOMEM.into());
            }
            buf.ensure_trailing_newline();
            out.extend_from_slice(buf.as_bytes());
        }
    }
    Ok(())
}
