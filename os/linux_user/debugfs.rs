// SPDX-License-Identifier: GPL-2.0

//! debugfs files, for userspace: os/debugfs.rs's, in a tree of our own - what
//! http.rs serves, under debug/ - a file read whole, a page at a time, as a
//! reader of the kernel's would read it.
//!
//! A read holds the tree's lock, so removing a directory waits it out, as
//! debugfs_remove() does.

use core::ffi::CStr;
use core::ptr::{self, NonNull};
use std::ffi::CString;
use std::io::Write;
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::c;
use crate::errcode::BchError;
use crate::os::kernel::PAGE_SIZE;
use crate::util::alloc::KBox;

#[path = "../debugfs.rs"]
mod common;
pub use common::*;

/// A reader's buffer: what a read copies out, as far as its room goes.
pub struct UserBuf {
    out:  Vec<u8>,
    room: usize,
}

impl UserBuf {
    pub fn len(&self) -> usize {
        self.room
    }

    pub fn copy_to(&mut self, src: &[u8]) -> usize {
        let n = src.len().min(self.room);
        self.out.extend_from_slice(&src[..n]);
        self.room -= n;
        n
    }
}

/// A file or directory.
struct Dentry {
    name:   CString,
    /// Null at the top - the debugfs root's
    parent: *const Dentry,
    /// None for a directory
    file:   Option<Reader>,
}

/// A file's object, and its read from the start.
struct Reader {
    obj:  *const (),
    open: unsafe fn(*const ()) -> Reading,
}

/// @obj's file.
///
/// # Safety
/// @obj is the F::Obj create_file() was given, and live while it's read.
unsafe fn open<F: DebugFile>(obj: *const ()) -> Reading {
    let obj = unsafe { &*obj.cast::<F::Obj>() };
    unsafe { reading(KBox::new(read::<F>(obj))) }
}

/// @f's text appended to @out, a PAGE_SIZE read at a time.
///
/// # Safety
/// @f's object is live.
unsafe fn read_file(f: &Reader, out: &mut Vec<u8>) -> Result<(), BchError> {
    let mut r = unsafe { (f.open)(f.obj) };
    let mut u = UserBuf { out: core::mem::take(out), room: 0 };
    let ret = loop {
        u.room = PAGE_SIZE;
        if let Err(e) = read_into(&mut r, &mut u) {
            break Err(e);
        }
        if r.is_none() {
            break Ok(());
        }
    };
    *out = u.out;
    ret
}

/// Every file and directory, in the order made: a directory's are those it's
/// the parent of.
struct Tree(Vec<Box<Dentry>>);

// SAFETY: what it points to is TREE's
unsafe impl Send for Tree {}

static TREE: Mutex<Tree> = Mutex::new(Tree(Vec::new()));

fn lock() -> MutexGuard<'static, Tree> {
    TREE.lock().expect("debugfs lock poisoned")
}

impl Tree {
    fn children(&self, dir: *const Dentry) -> impl Iterator<Item = &Dentry> {
        self.0.iter().map(|d| &**d).filter(move |d| d.parent == dir)
    }
}

/// The tools' http server, started for the first filesystem's files: set by
/// the tools, unset where there's none.
pub static START_HTTP: OnceLock<fn()> = OnceLock::new();

fn create(name: &CStr, parent: Option<NonNull<Dentry>>, file: Option<Reader>) -> NonNull<Dentry> {
    /*
     * Only start the http server when an fs is actually being brought
     * up. Module-init creates the root "bcachefs" debugfs dir with no
     * parent; per-fs debugfs creates always pass one. Without this guard
     * every command (--help, version, ...) would bind the unix socket.
     */
    if let (Some(_), Some(start)) = (parent, START_HTTP.get()) {
        start();
    }

    let d = Box::new(Dentry {
        name: name.into(),
        parent: parent.map_or(ptr::null(), |p| p.as_ptr()),
        file,
    });
    let p = NonNull::from(&*d);
    lock().0.push(d);
    p
}

/// Never None: there's always a debugfs here.
fn create_dir(name: &CStr, parent: Option<NonNull<Dentry>>) -> Option<NonNull<Dentry>> {
    Some(create(name, parent, None))
}

/// # Safety
/// @obj outlives the file.
unsafe fn create_file<F: DebugFile>(name: &CStr, parent: NonNull<Dentry>, obj: &F::Obj) {
    create(name, Some(parent), Some(Reader { obj: ptr::from_ref(obj).cast(), open: open::<F> }));
}

fn remove_recursive(dir: NonNull<Dentry>) {
    let mut tree = lock();
    let mut gone = vec![dir.as_ptr().cast_const()];
    while let Some(d) = gone.pop() {
        gone.extend(tree.children(d).map(ptr::from_ref));
        tree.0.retain(|n| !ptr::eq(&**n, d));
    }
}

/// The file at @path under debugfs, read as a reader would, or its directory
/// listed as HTML, into @out.
pub fn read_or_html_dirlist(path: &str, out: &mut Vec<u8>) -> Result<(), BchError> {
    let tree = lock();

    let mut dir: *const Dentry = ptr::null();
    for name in path.split('/').filter(|n| !n.is_empty()) {
        dir = tree.children(dir)
            .find(|d| d.name.to_bytes() == name.as_bytes())
            .ok_or(BchError::from(c::Errcode::ENOENT_debugfs_lookup))?;
    }

    if let Some(f) = unsafe { dir.as_ref() }.and_then(|d| d.file.as_ref()) {
        return unsafe { read_file(f, out) };
    }

    let _ = writeln!(out, "<html>\n<p> {path} </p>");
    for d in tree.children(dir) {
        let name = d.name.to_string_lossy();
        let _ = writeln!(out, "<p> <a href={path}/{name}>{name}</a></p>");
    }
    let _ = writeln!(out, "</html>");
    Ok(())
}
