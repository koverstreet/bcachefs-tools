use crate::btree::iter::BtreeTrans;
use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::util::printbuf::Printbuf;
use core::ffi::CStr;
#[cfg(feature = "std")]
use core::ffi::c_char;
#[cfg(feature = "std")]
use std::ffi::CString;

#[allow(non_camel_case_types)]
pub type opt_id = c::bch_opt_id;

/// An accessor for every option, from BCH_OPTS(): an OPT_BOOL() as bool, the
/// rest as their C type.
macro_rules! bch_opts {
    ($(($name:ident, $ty:ty, $flags:expr, $kind:ident $kind_args:tt $(, $($rest:tt)*)?)),* $(,)?) => {
        impl c::bch_opts {
            $(bch_opts!(@accessor $name, $ty, $kind);)*
        }
    };
    (@accessor $name:ident, $ty:ty, OPT_BOOL) => {
        pub fn $name(&self) -> bool { self.$name != 0 }
    };
    (@accessor $name:ident, $ty:ty, $kind:ident) => {
        pub fn $name(&self) -> $ty { self.$name }
    };
}
c::BCH_OPTS!(bch_opts);

/// Return the opt table as a proper slice.
///
/// bindgen generates `bch2_opt_table` as a zero-length array since it can't
/// determine the size. This wraps it safely using `bch2_opts_nr`.
pub fn opt_table() -> &'static [c::bch_option] {
    unsafe {
        core::slice::from_raw_parts(
            c::bch2_opt_table.as_ptr(),
            opt_id::nr.0 as usize,
        )
    }
}

#[macro_export]
macro_rules! opt_set {
    ($opts:ident, $n:ident, $v:expr) => {
        bcachefs_kernel::paste! {
            $opts.$n = $v;
            $opts.[<set_ $n _defined>](1)
        }
    };
}

#[macro_export]
macro_rules! opt_defined {
    ($opts:ident, $n:ident) => {
        bcachefs_kernel::paste! {
            $opts.[< $n _defined>]()
        }
    };
}

#[macro_export]
macro_rules! opt_get {
    ($opts:ident, $n:ident) => {
        if bcachefs_kernel::opt_defined!($opts, $n) == 0 {
            bcachefs_kernel::paste! {
                unsafe {
                    bcachefs_kernel::c::bch2_opts_default.$n
                }
            }
        } else {
            bcachefs_kernel::paste! {
                $opts.$n
            }
        }
    };
}

/// Safe conversion from opt table index to bch_opt_id.
///
/// Panics if idx >= bch2_opts_nr (i.e. out of the opt_table range).
pub fn opt_id(idx: usize) -> c::bch_opt_id {
    assert!(idx < opt_id::nr.0 as usize);
    c::bch_opt_id(idx as u32)
}

/// Check whether an option is explicitly defined in the opts struct.
pub fn opt_defined_by_id(opts: &c::bch_opts, id: c::bch_opt_id) -> bool {
    unsafe { c::bch2_opt_defined_by_id(opts, id) }
}

/// Get the value of an option by id.
pub fn opt_get_by_id(opts: &c::bch_opts, id: c::bch_opt_id) -> u64 {
    unsafe { c::bch2_opt_get_by_id(opts, id) }
}

/// Reference to the default opts (C global).
pub fn opts_default() -> &'static c::bch_opts {
    unsafe { &c::bch2_opts_default }
}

/// Set a superblock option from the opt table.
pub fn opt_set_sb(sb: &mut c::bch_sb, dev_idx: i32, opt: &c::bch_option, v: u64) {
    // val is only used by BCH_OPT_STR_MEMBER options, not set through this path:
    unsafe { c::__bch2_opt_set_sb(sb, dev_idx, opt, v, core::ptr::null()); }
}

/// Set an option value in a bch_opts struct by id.
pub fn opt_set_by_id(opts: &mut c::bch_opts, id: c::bch_opt_id, v: u64) {
    unsafe { c::bch2_opt_set_by_id(opts, id, v) }
}

/// Safe accessors for bch_option C string fields.
///
/// These methods provide safe access to the static C strings in the
/// option table. The strings are guaranteed to be valid for the process lifetime.
impl c::bch_option {
    /// Get the option name as a Rust string.
    ///
    /// Returns None if the name pointer is null or contains invalid UTF-8.
    pub fn name(&self) -> Option<&'static str> {
        if self.attr.name.is_null() {
            return None;
        }
        // attr is the kernel's struct attribute; its `name` is `*const u8` in
        // kernel::bindings (kernel builds char unsigned), so cast to c_char.
        unsafe { CStr::from_ptr(self.attr.name as *const core::ffi::c_char) }
            .to_str()
            .ok()
    }

    /// Get the option hint as a Rust string.
    ///
    /// Returns None if the hint pointer is null or contains invalid UTF-8.
    pub fn hint(&self) -> Option<&'static str> {
        if self.hint.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.hint) }
            .to_str()
            .ok()
    }

    /// Get the option help text as a Rust string.
    ///
    /// Returns None if the help pointer is null or contains invalid UTF-8.
    pub fn help(&self) -> Option<&'static str> {
        if self.help.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.help) }
            .to_str()
            .ok()
    }

    /// Collect choices array into a Vec of Rust strings.
    ///
    /// The choices array is null-terminated. Returns an empty Vec if
    /// the pointer is null. Invalid UTF-8 entries are skipped.
    #[cfg(feature = "std")]
    pub fn choices(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.choices.is_null() {
            return v;
        }

        let mut i = 0;
        loop {
            let p = unsafe { *self.choices.add(i) };
            if p.is_null() {
                break;
            }
            if let Ok(s) = unsafe { CStr::from_ptr(p) }.to_str() {
                v.push(s);
            }
            i += 1;
        }
        v
    }
}

#[cfg(feature = "std")]
pub fn parse_mount_opts(fs: Option<&mut Fs>, optstr: Option<&str>, ignore_unknown: bool)
        -> Result<c::bch_opts, crate::errcode::BchError> {
    let mut opts: c::bch_opts = Default::default();

    if let Some(optstr) = optstr {
        let optstr = CString::new(optstr).unwrap();
        let optstr_ptr = optstr.as_ptr();

        let ret = unsafe {
            c::bch2_parse_mount_opts(fs.map_or(core::ptr::null_mut(), |f| f.raw),
                                     &mut opts as *mut c::bch_opts,
                                     core::ptr::null_mut(),
                                     optstr_ptr as *mut c_char,
                                     ignore_unknown)
        };

        drop(optstr);

        if ret != 0 {
            return Err(crate::errcode::BchError::from_raw(-ret));
        }
    }
    Ok(opts)
}

/// Join a slice of option strings with commas and parse into bch_opts.
///
/// Convenience wrapper for callers that accumulate options as a Vec<String>.
/// Caller passes an empty slice for default opts.
#[cfg(feature = "std")]
pub fn parse_mount_opts_vec(opts: &[String], ignore_unknown: bool)
        -> Result<c::bch_opts, crate::errcode::BchError> {
    if opts.is_empty() {
        return parse_mount_opts(None, None, ignore_unknown);
    }
    let joined = opts.join(",");
    parse_mount_opts(None, Some(&joined), ignore_unknown)
}

/// Print a data type directly into a Printbuf via bch2_prt_data_type.
pub fn prt_data_type(out: &mut Printbuf, t: c::bch_data_type) {
    unsafe { c::bch2_prt_data_type(out.as_raw(), t) }
}

/// Print a compression type directly into a Printbuf via bch2_prt_compression_type.
pub fn prt_compression_type(out: &mut Printbuf, t: c::bch_compression_type) {
    unsafe { c::bch2_prt_compression_type(out.as_raw(), t) }
}

/// Print a reconcile accounting type directly into a Printbuf.
pub fn prt_reconcile_type(out: &mut Printbuf, t: c::bch_reconcile_accounting_type) {
    unsafe { c::bch2_prt_reconcile_accounting_type(out.as_raw(), t) }
}

/// Look up an option by name. The typed id plus the table entry.
pub fn opt_lookup(name: &core::ffi::CStr) -> Option<(c::bch_opt_id, &'static c::bch_option)> {
    let id = unsafe { c::bch2_opt_lookup(name.as_ptr()) };
    if id < 0 || id as usize >= opt_table().len() {
        return None;
    }
    Some((opt_id(id as usize), &opt_table()[id as usize]))
}

/// Parse an option value string; a filesystem context enables options
/// that need one. Err is the negative bch_errcode from the parser.
pub fn opt_parse(fs: Option<&Fs>, opt: &c::bch_option, val: &core::ffi::CStr,
                 err: Option<&mut Printbuf>) -> Result<u64, i32> {
    let mut v = 0u64;
    let ret = unsafe {
        c::bch2_opt_parse(fs.map_or(core::ptr::null_mut(), |f| f.raw),
                          opt, val.as_ptr(), &mut v,
                          err.map_or(core::ptr::null_mut(), |e| e.as_raw()))
    };
    if ret < 0 { Err(ret) } else { Ok(v) }
}

/// Option @opt's value @v, as text: as bch2_opt_to_text(), without flags.
pub fn opt_to_text(out: &mut Printbuf, fs: &Fs, opt: &c::bch_option, v: u64) {
    unsafe { c::bch2_opt_to_text(out.as_raw(), fs.raw, fs.sb() as *const _ as *mut _, opt, v, 0) }
}

impl c::bch_opt_id {
    /// Whether this is an option inodes can set: as bch2_opt_is_inode_opt().
    pub fn is_inode_opt(self) -> bool {
        unsafe { c::bch2_opt_is_inode_opt(self) }
    }

    /// The inode option this is, if it is one: as bch2_opt_to_inode_opt().
    pub fn inode_opt(self) -> Option<InodeOpt> {
        let id = unsafe { c::bch2_opt_to_inode_opt(self.0 as core::ffi::c_int) };
        u32::try_from(id).ok().and_then(InodeOpt::from_index)
    }
}

// ── Inode options ────────────────────────────────────────────────────────
//
// BCH_INODE_OPTS(): the options an inode can set for itself and what's
// below it. On the inode they're stored with a +1 bias, so that 0 means
// "not set" - inherited - and 1 an explicit "none"; bi_fields_set says
// which were set on the inode itself, rather than propagated from a parent.

/// An inode option: an inode_opt_id below Inode_opt_nr.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct InodeOpt(c::inode_opt_id);

impl InodeOpt {
    /// The inode option numbered @i, if there's one.
    pub fn from_index(i: u32) -> Option<InodeOpt> {
        // The enum is repr(u32), with a variant for every value below
        // Inode_opt_nr:
        (i < c::inode_opt_id::Inode_opt_nr as u32)
            .then(|| InodeOpt(unsafe { core::mem::transmute::<u32, c::inode_opt_id>(i) }))
    }

    pub fn all() -> impl Iterator<Item = InodeOpt> {
        (0..c::inode_opt_id::Inode_opt_nr as u32)
            .map(|i| InodeOpt::from_index(i).expect("below Inode_opt_nr"))
    }

    pub fn id(self) -> c::inode_opt_id {
        self.0
    }

    pub fn name(self) -> &'static CStr {
        unsafe { CStr::from_ptr(*c::bch2_inode_opts.as_ptr().add(self.0 as usize)) }
    }

    /// Its value on @inode, biased: 0 for not set.
    pub fn get(self, inode: &c::bch_inode_unpacked) -> u64 {
        // bch2_inode_opt_get()
        macro_rules! get {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if self.0 == c::inode_opt_id::[<Inode_opt_ $name>] {
                    return inode.[<bi_ $name>] as u64;
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(get);
        panic!("inode option {} out of range", self.0 as u32)
    }

    /// Set its value on @inode - biased, 0 for not set.
    pub fn set(self, inode: &mut c::bch_inode_unpacked, v: u64) {
        // bch2_inode_opt_set()
        macro_rules! set {
            ($(($name:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! { $(
                if self.0 == c::inode_opt_id::[<Inode_opt_ $name>] {
                    inode.[<bi_ $name>] = v as _;
                    return;
                }
            )* } };
        }
        c::BCH_INODE_OPTS!(set);
        panic!("inode option {} out of range", self.0 as u32)
    }

    /// Whether it's set on @inode itself, not inherited: bi_fields_set.
    pub fn is_own(self, inode: &c::bch_inode_unpacked) -> bool {
        inode.bi_fields_set & (1 << self.0 as u32) != 0
    }

    pub fn set_own(self, inode: &mut c::bch_inode_unpacked, own: bool) {
        let bit = 1 << self.0 as u32;
        if own {
            inode.bi_fields_set |= bit;
        } else {
            inode.bi_fields_set &= !bit;
        }
    }
}

/// @inode's options as filesystem options - those it sets, with the +1 bias
/// removed: as bch2_inode_opts_to_opts().
pub fn inode_opts_to_opts(inode: &c::bch_inode_unpacked) -> c::bch_opts {
    // Only reads @inode: C's signature isn't const.
    unsafe { c::bch2_inode_opts_to_opts(inode as *const _ as *mut _) }
}

/// @dst takes on @src's inode options - those it didn't set itself, and
/// casefolding only for a directory - as when it moves into directory @src:
/// as bch2_reinherit_attrs(). Whether anything changed.
pub fn reinherit_attrs(dst: &mut c::bch_inode_unpacked, src: &c::bch_inode_unpacked) -> bool {
    // Only reads @src: C's signature isn't const.
    unsafe { c::bch2_reinherit_attrs(dst, src as *const _ as *mut _) }
}

/// What changing an inode's options leaves for after the commit: a reconcile
/// scan, if the options its data goes by changed, and pushing them up to the
/// inode's older snapshots - the propagate logged op.
impl c::inode_opt_change {
    /// Nothing to do yet: as bch2_inode_opt_change_init().
    pub fn new() -> Self {
        let mut ch = Self::default();
        unsafe { c::bch2_inode_opt_change_init(&mut ch) };
        ch
    }

    /// @inode's options have changed, in @snapshot, from those that gave
    /// reconcile options @old: if those changed, schedule the scan and start
    /// the propagate - as bch2_inode_opt_change_trans().
    pub fn record(
        &mut self,
        trans:    &BtreeTrans<'_>,
        old:      &c::bch_extent_reconcile,
        inode:    &c::bch_inode_unpacked,
        snapshot: u32,
    ) -> Result<(), BchError> {
        // Only reads @old and @inode: C's signature isn't const.
        ret_to_result(unsafe {
            c::bch2_inode_opt_change_trans(trans.raw(), old as *const _ as *mut _,
                                           inode as *const _ as *mut _, snapshot, self)
        })
    }

    /// After the commit: finish the propagate, if one was started, and wake
    /// reconcile, if there's a scan for it - as bch2_inode_opt_change_finish().
    pub fn finish(&mut self, trans: &BtreeTrans<'_>) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_inode_opt_change_finish(trans.raw(), self) })
    }
}

// ── Changing options ─────────────────────────────────────────────────────

/// An option change in progress: the option change lock held, and the scope
/// the pre-set hooks record what the change needs into - as C's
/// guard(opt_change_lock) and CLASS(opt_change_scope). Both are released
/// when this is dropped, the scope first.
pub struct OptChange<'f> {
    fs:    &'f Fs,
    scope: c::opt_change_scope,
}

impl<'f> OptChange<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        unsafe { c::bch2_opt_change_lock(fs.raw) };
        OptChange { fs, scope: unsafe { c::bch2_opt_change_scope_init(fs.raw) } }
    }

    /// Before setting option @id to @v, for inode @inum - 0 for the
    /// filesystem: what the change needs, and whether it may - as
    /// bch2_opt_hook_pre_set().
    pub fn pre_set(&mut self, inum: u64, id: c::bch_opt_id, v: u64) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_opt_hook_pre_set(self.fs.raw, core::ptr::null_mut(), inum, id, v, true,
                                     &mut self.scope)
        })
    }

    /// Having set it: as bch2_opt_hook_post_set().
    pub fn post_set(&self, inum: u64, id: c::bch_opt_id, v: u64) {
        unsafe { c::bch2_opt_hook_post_set(self.fs.raw, core::ptr::null_mut(), inum, id, v) }
    }
}

impl Drop for OptChange<'_> {
    fn drop(&mut self) {
        unsafe {
            c::bch2_opt_change_scope_exit(&mut self.scope);
            c::bch2_opt_change_unlock(self.fs.raw);
        }
    }
}
