// SPDX-License-Identifier: GPL-2.0

//! sysfs files in Rust.
//!
//! A directory - a Dir - is a kobject in some object T, a bch_fs or a bch_dev,
//! and its files: tables of Attr<T>, each a file whole - the attribute sysfs
//! sees (name, mode), and what reading and writing it do - or, made by
//! Attr::list(), one per entry of a list, a counter or an option, each given
//! its entry. A file read and written in pages at an offset is a Bin.
//!
//! The files are in attribute groups (kobject.rs), typed by their Dir: a
//! kobject's are its own Attr<T>s, so the port's ops get from the attribute
//! they're handed to the Attr by container_of(), and from the kobject to T by
//! the kobject's place in T. What an attribute is, and how the files are read
//! and written, are the port's sysfs.rs, which has this module too: the
//! kernel's sysfs, through its C structs, or the tree userspace's http.rs
//! serves.
//!
//! A show writes to a Printbuf, with a trailing newline. A store gets what
//! was written - with the newline echo leaves - and takes all of it or fails.
//!
//! kernfs won't open a file for a direction its mode doesn't allow - root
//! included - so a missing show or store is never called.

use core::ffi::CStr;
use core::marker::PhantomData;

use crate::c;
use crate::errcode::BchError;
use crate::os::kobject::{KobjPlace, Kobject};
use crate::util::Printbuf;
use super::{attribute, Attribute};

pub type Show<T>  = fn(&T, &mut Printbuf);
pub type Store<T> = fn(&T, &str) -> Result<(), BchError>;
/// A list's file's: of entry @usize.
pub type ShowEntry<T>  = fn(&T, usize, &mut Printbuf) -> Result<(), BchError>;
pub type StoreEntry<T> = fn(&T, usize, &str) -> Result<(), BchError>;

enum Ops<T: 'static> {
    File { show: Option<Show<T>>, store: Option<Store<T>> },
    Entry { i: usize, show: ShowEntry<T>, store: StoreEntry<T> },
}

/// A sysfs file of an object T: its attribute, and what reading and writing
/// it do.
#[repr(C)]
pub struct Attr<T: 'static> {
    attr: Attribute,
    ops:  Ops<T>,
    /// What a store has to pass first: gated()'s.
    gate: Option<fn(&T) -> Result<(), BchError>>,
}

impl<T> Attr<T> {
    const fn new(name: &'static CStr, mode: u16, show: Option<Show<T>>, store: Option<Store<T>>) -> Self {
        Attr { attr: attribute(name, mode), ops: Ops::File { show, store }, gate: None }
    }

    /// A file per entry of a list, each of @names, with its mode in @modes:
    /// @show and @store given its entry.
    pub const fn list<const N: usize>(names: &[&'static CStr; N], modes: &[u16; N],
                                      show: ShowEntry<T>, store: StoreEntry<T>) -> [Self; N] {
        let mut attrs = [const { Self::new(c"", 0, None, None) }; N];
        let mut i = 0;
        while i < N {
            attrs[i] = Attr { attr: attribute(names[i], modes[i]), ops: Ops::Entry { i, show, store }, gate: None };
            i += 1;
        }
        attrs
    }

    /// What reading it writes, to @out.
    pub(crate) fn show(&self, obj: &T, out: &mut Printbuf) -> Result<(), BchError> {
        match self.ops {
            Ops::File { show: Some(show), .. } => Ok(show(obj, out)),
            Ops::File { show: None, .. }       => Ok(()),
            Ops::Entry { i, show, .. }         => show(obj, i, out),
        }
    }

    /// @s, written to it.
    pub(crate) fn store(&self, obj: &T, s: &str) -> Result<(), BchError> {
        if let Some(gate) = self.gate {
            gate(obj)?;
        }
        match self.ops {
            Ops::File { store: Some(store), .. } => store(obj, s),
            Ops::File { store: None, .. }        => Err(c::Errcode::EINVAL_sysfs_store_readonly.into()),
            Ops::Entry { i, store, .. }          => store(obj, i, s),
        }
    }

    /// The file @attr is the attribute of.
    ///
    /// # Safety
    /// @attr is an Attr<T>'s.
    pub(crate) unsafe fn of(attr: &Attribute) -> &Self {
        unsafe { &*core::ptr::from_ref(attr).cast() }
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

/// @attrs, each store refused unless @gate passes first: a directory's
/// files that can't be written until its object can take it.
pub const fn gated<T, const N: usize>(gate: fn(&T) -> Result<(), BchError>, mut attrs: [Attr<T>; N]) -> [Attr<T>; N] {
    let mut i = 0;
    while i < N {
        attrs[i].gate = Some(gate);
        i += 1;
    }
    attrs
}

/// A directory's files: an array of pointers to each Attr's attribute, NULL
/// terminated. N is the number of files plus one - `Files<D, { FOO.len() + 1 }>`.
#[repr(transparent)]
pub(crate) struct Files<D, const N: usize>([*const Attribute; N], PhantomData<fn() -> D>);

impl<D: Dir, const N: usize> Files<D, N> {
    pub const fn new(attrs: &'static [Attr<D::Obj>]) -> Self {
        Self::with_list::<0>(attrs, &[], &[])
    }

    /// @attrs' files, then those of @list's that @which says: as many as there
    /// are, plus one.
    pub const fn with_list<const M: usize>(attrs: &'static [Attr<D::Obj>], list: &'static [Attr<D::Obj>; M],
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
                files[n] = &raw const list[i].attr;
                n += 1;
            }
            i += 1;
        }
        assert!(n + 1 == N);
        Files(files, PhantomData)
    }

    /// As a group has them.
    pub const fn as_ptr(&'static self) -> *mut *mut Attribute {
        self.0.as_ptr().cast_mut().cast()
    }
}

unsafe impl<D, const N: usize> Sync for Files<D, N> {}

/// A directory: what it's the directory of, and where in it its kobject is.
/// Its files are its kobject's groups', each an Attr<Obj>.
pub(crate) trait Dir: 'static {
    type Obj: 'static;
    /// The directory's kobject: where in Obj it is.
    type Kobj: KobjPlace<Owner = Self::Obj>;
}

/// A file read and written in pages at an offset, of a Dir's object.
pub(crate) trait Bin: 'static {
    type Dir: Dir;

    /// Up to @buf's length from @off: how much - none past the end.
    fn read(obj: &<Self::Dir as Dir>::Obj, buf: &mut [u8], off: u64) -> Result<usize, BchError>;

    /// @buf, written at @off: how much of it was taken.
    fn write(obj: &<Self::Dir as Dir>::Obj, buf: &[u8], off: u64) -> Result<usize, BchError>;
}

/// The object @kobj is D's kobject in: container_of().
///
/// # Safety
/// @kobj is D's kobject, in a live D::Obj.
pub(crate) unsafe fn obj_of<'a, D: Dir>(kobj: *mut Kobject) -> &'a D::Obj {
    unsafe { &*D::Kobj::owner(kobj) }
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
            IntErrorKind::NegOverflow => c::Errcode::ERANGE_parse_int.into(),
            _                         => c::Errcode::EINVAL_parse_int.into(),
        })
}
