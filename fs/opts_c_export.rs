//! opts.rs's wrappers for C: the tables C reads, the defaults under C's
//! name, and each a C name over a Rust function, with C's types - superblock
//! pointers, C strings, an errcode returned. Deleted with the last C caller.

use super::*;
use crate::errcode::ret_to_c;
use crate::util::ffi::Opaque;
use crate::util::vstructs::{Head, Validated};
use core::ffi::{c_int, c_uint};
use crate::util::ToText;
use cstruct_macros::c_export;

#[c_export]
pub static bch2_opts_default: c::bch_opts = DEFAULTS;

/// The tables C still reads: each enum's names, as C's string tables have
/// them.
macro_rules! c_names {
    ($($decl:tt $name:ident: $t:ty;)*) => { $(
        #[c_export]
        #[c($decl)]
        pub static $name: [*const c_char; names::c_table_len::<$t>()] = crate::c_names_table!($t);
    )* };
}
c_names! {
    "const char * const bch2_error_actions[]"  bch2_error_actions: c::bch_error_actions;
    "const char * const bch2_sb_features[]"    bch2_sb_features: c::bch_sb_feature;
    "const char * const __bch2_btree_ids[]"    __bch2_btree_ids: c::btree_id;
    "const char * const __bch2_csum_types[]"   __bch2_csum_types: c::bch_csum_type;
    "const char * const __bch2_data_types[]"   __bch2_data_types: c::bch_data_type;
    "const char * const bch2_member_states[]"  bch2_member_states: c::bch_member_state;
}

#[c_export]
pub fn bch2_prt_jset_entry_type(out: &mut Printbuf, t: c::bch_jset_entry_type) {
    t.to_text(out)
}

#[c_export]
pub fn bch2_prt_fs_usage_type(out: &mut Printbuf, t: c::bch_fs_usage_type) {
    t.to_text(out)
}

#[c_export]
pub fn bch2_prt_csum_type(out: &mut Printbuf, t: c::bch_csum_type) {
    t.to_text(out)
}

#[c_export]
pub fn bch2_prt_compression_type(out: &mut Printbuf, t: c::bch_compression_type) {
    t.to_text(out)
}

#[c_export]
pub fn bch2_prt_data_type(out: &mut Printbuf, t: c::bch_data_type) {
    t.to_text(out)
}

#[c_export]
pub fn bch2_prt_reconcile_accounting_type(out: &mut Printbuf, t: c::bch_reconcile_accounting_type) {
    t.to_text(out)
}

#[c_export]
pub fn bch2_prt_key_type_error_reason(out: &mut Printbuf, t: c::bch_key_type_errors) {
    t.to_text(out)
}

#[c_export]
pub fn bch2_opt_defined_by_id(opts: &c::bch_opts, id: c::bch_opt_id) -> bool {
    opts.defined(id)
}

#[c_export]
pub fn bch2_opts_apply(dst: &mut c::bch_opts, src: c::bch_opts) {
    dst.apply(&src)
}

/// # Safety
/// @sb is a validated superblock.
#[c_export]
pub unsafe fn bch2_opts_from_sb(opts: &mut c::bch_opts, sb: *mut Head<c::bch_sb>) -> c_int {
    // SAFETY: the caller's
    opts_from_sb(opts, unsafe { c::bch_sb::<Validated>::from_ffi(sb).as_unvalidated() });
    0
}

/// # Safety
/// @sb is a validated superblock.
#[c_export]
pub unsafe fn bch2_opts_validate_sb(sb: *mut Head<c::bch_sb>, out: &mut Printbuf) -> c_int {
    // SAFETY: the caller's
    ret_to_c(opts_validate_sb(unsafe { c::bch_sb::<Validated>::from_ffi(sb).as_unvalidated() }, out))
}

/// # Safety
/// @sb, if not NULL, is a validated superblock.
#[cold]
#[c_export]
pub unsafe fn bch2_opts_to_text(out: &mut Printbuf, opts: c::bch_opts, c: Option<&Opaque<c::bch_fs>>,
                                sb: *mut Head<c::bch_sb>,
                                mask: Option<&c::bch_opts_mask>,
                                show_mask: c_uint, hide_mask: c_uint, flags: c_uint) {
    // SAFETY: the caller's
    let sb = (!sb.is_null()).then(|| unsafe { c::bch_sb::<Validated>::from_ffi(sb).as_unvalidated() });
    opts_to_text(out, &opts, c.map(Fs::from_c), sb, mask, show_mask, hide_mask, flags)
}

#[c_export]
pub fn bch2_opts_hooks_pre_set(c: &Fs) -> c_int {
    ret_to_c(change::opts_hooks_pre_set(c))
}

/// One mount option, for the VFS's parse_param(): as parse_one_mount_opt().
///
/// # Safety
/// @name is a C string; @val one, or NULL.
#[c_export]
pub unsafe fn bch2_parse_one_mount_opt(c: Option<&Opaque<c::bch_fs>>, opts: Option<&mut c::bch_opts>,
                                       parse_later: Option<&mut Printbuf>, name: *const c_char,
                                       val: *const c_char, err: &mut Printbuf) -> c_int {
    // SAFETY: the caller's
    let name = unsafe { CStr::from_ptr(name.cast()) }.to_bytes();
    let val = (!val.is_null()).then(|| unsafe { CStr::from_ptr(val.cast()) }.to_bytes());
    ret_to_c(parse_one_mount_opt(c.map(Fs::from_c), opts, parse_later, name, val, err))
}

/// # Safety
/// @options is a C string, or NULL.
#[c_export]
pub unsafe fn bch2_parse_mount_opts(c: Option<&Opaque<c::bch_fs>>, opts: &mut c::bch_opts,
                                    parse_later: Option<&mut Printbuf>, options: *const c_char,
                                    ignore_unknown: bool) -> c_int {
    if options.is_null() {
        return 0;
    }
    // SAFETY: the caller's
    let options = unsafe { CStr::from_ptr(options.cast()) }.to_bytes();
    ret_to_c(parse_mount_opts_into(c.map(Fs::from_c), opts, parse_later, options, ignore_unknown))
}

#[c_export]
pub fn bch2_inode_opts_get(c: &Fs, ret: &mut c::bch_inode_opts, metadata: bool) {
    *ret = crate::inode_opts::get_fs(c, metadata);
}

#[cold]
#[c_export]
pub fn bch2_inode_opts_to_text(out: &mut Printbuf, c: &Fs, opts: c::bch_inode_opts) {
    crate::inode_opts::opts_to_text(out, c, &opts)
}
