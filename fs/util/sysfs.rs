// SPDX-License-Identifier: GPL-2.0

//! sysfs files in Rust.
//!
//! A directory - a Dir - is a kobject in some object T, a bch_fs or a bch_dev,
//! and its files. Usually those are a table of Attr<T>: the C struct
//! attribute sysfs sees (name, mode) with its show and store beside it, the
//! kernel's kobj_attribute pattern - so the directory's sysfs_ops get from
//! the attribute sysfs hands them to its handlers by container_of(), and from
//! the kobject to T by the kobject's place in T. A directory of one file per
//! list entry has a ListAttrs instead, and one whose files are something
//! else's attributes - options' - overrides Dir::show() and store().
//!
//! The files are `struct attribute *foo_files[]` to C (Files), in attribute
//! groups (kobject.rs), and the ops a `struct sysfs_ops` (ops()): a named
//! group's files are its kobject's, so one Dir is all of them.
//!
//! A show writes to a Printbuf, which goes to sysfs's page with a trailing
//! newline, cut at PAGE_SIZE - 1 - ENOMEM if the printbuf couldn't allocate.
//! A store gets what was written - with the newline echo leaves - and takes
//! all of it or fails.
//!
//! kernfs won't open a file for a direction its mode doesn't allow - root
//! included - so a missing show or store is never called.

use core::ffi::CStr;

use crate::c;
use crate::errcode::BchError;
use crate::util::ffi::c_char;
use crate::util::kobject::KobjPlace;
use crate::util::Printbuf;

pub type Show<T>  = fn(&T, &mut Printbuf);
pub type Store<T> = fn(&T, &str) -> Result<(), BchError>;

/// A sysfs file of an object T: its C attribute, and what reading and writing
/// it do.
#[repr(C)]
pub struct Attr<T: 'static> {
    attr:  c::attribute,
    show:  Option<Show<T>>,
    store: Option<Store<T>>,
}

impl<T> Attr<T> {
    const fn new(name: &'static CStr, mode: u16, show: Option<Show<T>>, store: Option<Store<T>>) -> Self {
        // SAFETY: the rest of a struct attribute is lockdep's, zeroed for a
        // static one
        let mut attr: c::attribute = unsafe { core::mem::zeroed() };
        attr.name = name.as_ptr().cast();
        attr.mode = mode;
        Attr { attr, show, store }
    }

    /// 0444
    pub const fn ro(name: &'static CStr, show: Show<T>) -> Self {
        Self::new(name, 0o444, Some(show), None)
    }

    /// 0200
    pub const fn wo(name: &'static CStr, store: Store<T>) -> Self {
        Self::new(name, 0o200, None, Some(store))
    }

    /// 0644
    pub const fn rw(name: &'static CStr, show: Show<T>, store: Store<T>) -> Self {
        Self::new(name, 0o644, Some(show), Some(store))
    }
}

/// A directory's files as C has them: an array of pointers to each Attr's
/// attribute, NULL terminated. N is the number of files plus one -
/// `Files<{ FOO.len() + 1 }>`.
#[repr(transparent)]
pub struct Files<const N: usize>([*const c::attribute; N]);

impl<const N: usize> Files<N> {
    pub const fn new<T>(attrs: &'static [Attr<T>]) -> Self {
        assert!(attrs.len() + 1 == N);

        let mut files = [core::ptr::null(); N];
        let mut i = 0;
        while i < attrs.len() {
            files[i] = &raw const attrs[i].attr;
            i += 1;
        }
        Files(files)
    }
}

/// One file per entry of a list - a counter, a time stat - each alike, so an
/// attribute and nothing beside it: which entry a file is, from where its
/// attribute is in the array. A static, for that: a const's copies would be
/// at other addresses.
#[repr(transparent)]
pub struct ListAttrs<const N: usize>([c::attribute; N]);

// SAFETY: its pointers are to static names; nothing writes it
unsafe impl<const N: usize> Sync for ListAttrs<N> {}

impl<const N: usize> ListAttrs<N> {
    pub const fn new(names: &[&'static CStr; N], mode: u16) -> Self {
        Self::with_modes(names, &[mode; N])
    }

    /// Each entry's file, with a mode of its own.
    pub const fn with_modes(names: &[&'static CStr; N], modes: &[u16; N]) -> Self {
        // SAFETY: as Attr::new()'s
        let mut attrs: [c::attribute; N] = unsafe { core::mem::zeroed() };
        let mut i = 0;
        while i < N {
            attrs[i].name = names[i].as_ptr().cast();
            attrs[i].mode = modes[i];
            i += 1;
        }
        ListAttrs(attrs)
    }

    /// The entry @attr is the file of, if it's one of these.
    pub fn index(&self, attr: &c::attribute) -> Option<usize> {
        let i = (core::ptr::from_ref(attr) as usize)
            .wrapping_sub(self.0.as_ptr() as usize) / size_of::<c::attribute>();
        (i < N && core::ptr::eq(&self.0[i], attr)).then_some(i)
    }
}

impl<const N: usize> Files<N> {
    /// A ListAttrs' files - M of them, N = M + 1.
    pub const fn list<const M: usize>(attrs: &'static ListAttrs<M>) -> Self {
        Self::with_list::<(), M>(&[], attrs, &[true; M])
    }

    /// @attrs' files, then those of @list's entries that @which says: as many
    /// as there are, plus one.
    pub const fn with_list<T, const M: usize>(attrs: &'static [Attr<T>], list: &'static ListAttrs<M>,
                                              which: &[bool; M]) -> Self {
        let mut files = [core::ptr::null(); N];
        let mut n = 0;
        while n < attrs.len() {
            files[n] = &raw const attrs[n].attr;
            n += 1;
        }
        let mut i = 0;
        while i < M {
            if which[i] {
                files[n] = &raw const list.0[i];
                n += 1;
            }
            i += 1;
        }
        assert!(n + 1 == N);
        Files(files)
    }

    /// As a group has them.
    pub const fn as_ptr(&'static self) -> *mut *mut c::attribute {
        self.0.as_ptr().cast_mut().cast()
    }
}

unsafe impl<const N: usize> Sync for Files<N> {}

/// A directory: the kobject it is, what it asks of every store, and how its
/// files read and write - by default, as the Attr<Obj>s they are.
///
/// # Safety
/// ops::<Self>() is only ever the sysfs_ops of an Obj's kobject; and its files
/// are Attr<Obj>s, unless show and store are overridden.
pub(crate) unsafe trait Dir: 'static {
    type Obj: 'static;
    /// The directory's kobject: where in Obj it is.
    type Kobj: KobjPlace<Owner = Self::Obj>;

    /// Before any store: may refuse it.
    fn may_store(_obj: &Self::Obj) -> Result<(), BchError> {
        Ok(())
    }

    fn show(obj: &Self::Obj, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
        // SAFETY: the trait's: this is an Attr<Obj>'s
        unsafe { attr_show(obj, attr, out) }
    }

    fn store(obj: &Self::Obj, attr: &c::attribute, s: &str) -> Result<(), BchError> {
        // SAFETY: the trait's: this is an Attr<Obj>'s
        unsafe { attr_store(obj, attr, s) }
    }
}

/// Reading @attr, an Attr<T>'s: Dir's default, for one whose own show()
/// has some other files too.
///
/// # Safety
/// @attr is an Attr<T>'s.
pub unsafe fn attr_show<T: 'static>(obj: &T, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
    let attr = unsafe { &*core::ptr::from_ref(attr).cast::<Attr<T>>() };
    if let Some(show) = attr.show {
        show(obj, out);
    }
    Ok(())
}

/// Writing @attr, an Attr<T>'s: as attr_show().
///
/// # Safety
/// @attr is an Attr<T>'s.
pub unsafe fn attr_store<T: 'static>(obj: &T, attr: &c::attribute, s: &str) -> Result<(), BchError> {
    let attr = unsafe { &*core::ptr::from_ref(attr).cast::<Attr<T>>() };
    match attr.store {
        Some(store) => store(obj, s),
        None        => Err(c::EINVAL.into()),
    }
}

/// A Dir's sysfs_ops.
pub(crate) const fn ops<D: Dir>() -> c::sysfs_ops {
    c::sysfs_ops {
        show:  Some(show::<D>),
        store: Some(store::<D>),
    }
}

/// The object @kobj is D's kobject in: container_of().
///
/// # Safety
/// @kobj is D's kobject, in a live D::Obj.
pub(crate) unsafe fn obj_of<'a, D: Dir>(kobj: *mut c::kobject) -> &'a D::Obj {
    unsafe { &*D::Kobj::owner(kobj) }
}

unsafe extern "C" fn show<D: Dir>(kobj: *mut c::kobject, attr: *mut c::attribute,
                                  buf: *mut c_char) -> isize {
    // SAFETY: ops()'s, which only D's kobject has; sysfs's attribute and page
    unsafe {
        let obj = obj_of::<D>(kobj);
        show_page(buf, |out| D::show(obj, &*attr, out))
    }
}

unsafe extern "C" fn store<D: Dir>(kobj: *mut c::kobject, attr: *mut c::attribute,
                                   buf: *const c_char, size: usize) -> isize {
    // SAFETY: ops()'s, which only D's kobject has
    let obj = unsafe { obj_of::<D>(kobj) };
    if let Err(e) = D::may_store(obj) {
        return e.class() as isize;
    }
    // SAFETY: sysfs's attribute, and buffer of @size bytes
    unsafe { store_buf(buf, size, |s| D::store(obj, &*attr, s)) }
}

/// A show: what @f writes - into sysfs's page @buf itself, cut off at its end -
/// or @f's error, as an errno.
///
/// # Safety
/// @buf is a PAGE_SIZE buffer.
pub unsafe fn show_page(buf: *mut c_char, f: impl FnOnce(&mut Printbuf) -> Result<(), BchError>) -> isize {
    // SAFETY: the caller's
    let mut out = unsafe { Printbuf::new_extern(buf, crate::util::kernel::PAGE_SIZE) };

    match f(&mut out) {
        Ok(()) => {
            out.ensure_trailing_newline();
            out.as_bytes().len() as isize
        }
        Err(e) => e.class() as isize,
    }
}

/// A store: @f given what was written, @size bytes at @buf - all of it taken
/// if @f succeeds. Not UTF-8 is EINVAL.
///
/// # Safety
/// @buf has @size bytes.
pub unsafe fn store_buf(buf: *const c_char, size: usize,
                        f: impl FnOnce(&str) -> Result<(), BchError>) -> isize {
    // SAFETY: the caller's
    let bytes = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), size) };
    let ret = core::str::from_utf8(bytes)
        .map_err(|_| BchError::from(c::EINVAL))
        .and_then(f);

    match ret {
        Ok(())  => size as isize,
        Err(e)  => e.class() as isize,
    }
}

/// What a store writes as a number: kstrto*(), base 10 - one trailing newline
/// allowed, ERANGE if it doesn't fit @T.
pub fn parse_int<T>(s: &str) -> Result<T, BchError>
    where T: core::str::FromStr<Err = core::num::ParseIntError>
{
    use core::num::IntErrorKind;

    s.strip_suffix('\n').unwrap_or(s)
        .parse()
        .map_err(|e: core::num::ParseIntError| match e.kind() {
            IntErrorKind::PosOverflow |
            IntErrorKind::NegOverflow => c::ERANGE.into(),
            _                         => c::EINVAL.into(),
        })
}
