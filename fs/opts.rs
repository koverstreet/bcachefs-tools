use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use crate::inode_opts::InodeOpt;
use crate::sb::members::dev_ptr;
use crate::util::printbuf::Printbuf;
use crate::util::vstructs::plain_bytes;
use core::ffi::CStr;
#[cfg(feature = "std")]
use crate::util::ffi::c_char;
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

/// bindgen makes bch2_opt_table zero-length: it can't see the size.
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
            $opts.[<set_ $n _defined>](true)
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
        if !bcachefs_kernel::opt_defined!($opts, $n) {
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

pub fn opt_id(idx: usize) -> c::bch_opt_id {
    assert!(idx < opt_id::nr.0 as usize);
    c::bch_opt_id(idx as u32)
}

pub fn opt_defined_by_id(opts: &c::bch_opts, id: c::bch_opt_id) -> bool {
    unsafe { c::bch2_opt_defined_by_id(opts, id) }
}

pub fn opt_get_by_id(opts: &c::bch_opts, id: c::bch_opt_id) -> u64 {
    unsafe { c::bch2_opt_get_by_id(opts, id) }
}

pub fn opts_default() -> &'static c::bch_opts {
    unsafe { &c::bch2_opts_default }
}

pub fn opt_set_sb(sb: &mut c::bch_sb, dev_idx: i32, opt: &c::bch_option, v: u64) {
    // val is only used by BCH_OPT_STR_MEMBER options, not set through this path:
    unsafe { c::__bch2_opt_set_sb(sb.as_ffi_mut(), dev_idx, opt, v, core::ptr::null()); }
}

pub fn opt_set_by_id(opts: &mut c::bch_opts, id: c::bch_opt_id, v: u64) {
    unsafe { c::bch2_opt_set_by_id(opts, id, v) }
}

/// Option @id as the superblock has it - a device option, as member
/// @dev_idx's: bch2_opt_from_sb().
pub fn opt_from_sb(sb: &c::bch_sb, id: c::bch_opt_id, dev_idx: u32) -> u64 {
    unsafe { c::bch2_opt_from_sb(sb.as_ffi() as *mut _, id, dev_idx as i32) }
}

/// The option whose sysfs attribute @attr is, if it's one: each option's is
/// in its bch_option.
pub fn opt_from_attr(attr: &c::attribute) -> Option<c::bch_opt_id> {
    let table = opt_table();
    let i = (core::ptr::from_ref(attr) as usize)
        .wrapping_sub(core::mem::offset_of!(c::bch_option, attr))
        .wrapping_sub(table.as_ptr() as usize) / size_of::<c::bch_option>();
    (i < table.len() && core::ptr::eq(&table[i].attr, attr)).then(|| opt_id(i))
}

/// The option table's strings are static. None for a null pointer or invalid
/// UTF-8.
impl c::bch_option {
    pub fn name(&self) -> Option<&'static str> {
        if self.attr.name.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.attr.name.cast()) }
            .to_str()
            .ok()
    }

    pub fn hint(&self) -> Option<&'static str> {
        if self.hint.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.hint.cast()) }
            .to_str()
            .ok()
    }

    pub fn help(&self) -> Option<&'static str> {
        if self.help.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.help.cast()) }
            .to_str()
            .ok()
    }

    pub fn has(&self, flag: c::opt_flags) -> bool {
        self.flags.bits() & flag.bits() != 0
    }

    /// Whether the superblock has it: a field, a member's, the ext section's.
    pub fn in_sb(&self) -> bool {
        self.get_sb.is_some() ||
        self.get_member.is_some() ||
        self.get_ext.is_some() ||
        self.type_ == c::opt_type::BCH_OPT_STR_MEMBER
    }

    /// A BCH_OPT_STR_MEMBER option's value: the string in member @m's char
    /// field, up to a NUL.
    pub fn member_str<'a>(&self, m: &'a c::bch_member) -> &'a [u8] {
        assert_eq!(self.type_, c::opt_type::BCH_OPT_STR_MEMBER);

        let field = &plain_bytes(m)[self.member_offset as usize..][..self.member_size as usize];
        field.split(|&b| b == 0).next().unwrap_or_default()
    }

    /// Invalid UTF-8 entries are skipped.
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
            c::bch2_parse_mount_opts(fs.map_or(core::ptr::null_mut(), |f| f.as_ptr()),
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

#[cfg(feature = "std")]
pub fn parse_mount_opts_vec(opts: &[String], ignore_unknown: bool)
        -> Result<c::bch_opts, crate::errcode::BchError> {
    if opts.is_empty() {
        return parse_mount_opts(None, None, ignore_unknown);
    }
    let joined = opts.join(",");
    parse_mount_opts(None, Some(&joined), ignore_unknown)
}

pub fn prt_data_type(out: &mut Printbuf, t: c::bch_data_type) {
    unsafe { c::bch2_prt_data_type(out.as_raw(), t) }
}

pub fn prt_compression_type(out: &mut Printbuf, t: c::bch_compression_type) {
    unsafe { c::bch2_prt_compression_type(out.as_raw(), t) }
}

pub fn prt_reconcile_type(out: &mut Printbuf, t: c::bch_reconcile_accounting_type) {
    unsafe { c::bch2_prt_reconcile_accounting_type(out.as_raw(), t) }
}

pub fn opt_lookup(name: &core::ffi::CStr) -> Option<(c::bch_opt_id, &'static c::bch_option)> {
    let id = unsafe { c::bch2_opt_lookup(name.as_ptr().cast()) };
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
        c::bch2_opt_parse(fs.map_or(core::ptr::null_mut(), |f| f.as_ptr()),
                          opt, val.as_ptr().cast(), &mut v,
                          err.map_or(core::ptr::null_mut(), |e| e.as_raw()))
    };
    if ret < 0 { Err(ret) } else { Ok(v) }
}

/// @flags: OPT_SHOW_*. No superblock: that's for rendering without a
/// filesystem.
pub fn opt_to_text(out: &mut Printbuf, fs: &Fs, opt: &c::bch_option, v: u64, flags: u32) {
    unsafe { c::bch2_opt_to_text(out.as_raw(), fs.as_ptr(), core::ptr::null_mut(), opt, v, flags) }
}

impl c::bch_opt_id {
    pub fn is_inode_opt(self) -> bool {
        unsafe { c::bch2_opt_is_inode_opt(self) }
    }

    pub fn inode_opt(self) -> Option<InodeOpt> {
        InodeOpt::from_opt_id(self)
    }
}

// ── Changing options ─────────────────────────────────────────────────────

/// An option change in progress: the option change lock held, and the scope
/// the pre-set hooks record what the change needs into. Both are released on
/// drop, the scope first.
pub struct OptChange<'f> {
    fs:    &'f Fs,
    scope: c::opt_change_scope,
}

impl<'f> OptChange<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        unsafe { c::bch2_opt_change_lock(fs.as_ptr()) };
        // bch2_opt_change_scope_init(), a static inline:
        OptChange { fs, scope: c::opt_change_scope { c: fs.as_ptr(), ..Default::default() } }
    }

    /// Before setting option @id to @v - for device @ca, or inode @inum, or
    /// with neither (0) the filesystem: records what the change needs, and
    /// fails if it may not.
    pub fn pre_set(&mut self, ca: Option<&c::bch_dev>, inum: u64, id: c::bch_opt_id, v: u64)
        -> Result<(), BchError>
    {
        ret_to_result(unsafe {
            c::bch2_opt_hook_pre_set(self.fs.as_ptr(), dev_ptr(ca), inum, id, v, true, &mut self.scope)
        })
    }

    pub fn post_set(&self, ca: Option<&c::bch_dev>, inum: u64, id: c::bch_opt_id, v: u64) {
        unsafe { c::bch2_opt_hook_post_set(self.fs.as_ptr(), dev_ptr(ca), inum, id, v) }
    }

    /// Set @opt in the superblock - in device @ca's member for a device
    /// option - @val its string for a BCH_OPT_STR_MEMBER: whether it changed.
    pub fn set_sb(&self, ca: Option<&c::bch_dev>, opt: &c::bch_option, v: u64, val: &CStr) -> bool {
        unsafe { c::bch2_opt_set_sb(self.fs.as_ptr(), dev_ptr(ca), opt, v, val.as_ptr().cast()) }
    }

    /// Set filesystem option @id: c->opts, no longer as mount set it.
    pub fn set_fs_opt(&self, id: c::bch_opt_id, v: u64) {
        // SAFETY: c->opts changes under the option change lock, which this holds
        unsafe { c::bch2_opt_set_by_id(&raw mut (*self.fs.as_ptr()).opts, id, v) };

        let mount_opts = &self.fs.mount_opts.d;
        // SAFETY: unsigned longs, which C changes with clear_bit()
        let words = unsafe {
            core::slice::from_raw_parts(mount_opts.as_ptr().cast::<core::sync::atomic::AtomicUsize>(),
                                        mount_opts.len())
        };
        crate::util::bitmap::AtomicBitmap::from_words(words).clear(id.0 as usize);
    }
}

impl Drop for OptChange<'_> {
    fn drop(&mut self) {
        unsafe {
            c::bch2_opt_change_scope_exit(&mut self.scope);
            c::bch2_opt_change_unlock(self.fs.as_ptr());
        }
    }
}
