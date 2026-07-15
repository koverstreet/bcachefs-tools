use std::ffi::{c_char, CStr};

/// Demangle a C-string of a (possibly Rust/C++ Itanium) mangled name.
///
/// Writes a NUL-terminated demangled form into `out[0..out_len]`. If the input
/// is not mangled, returns it unchanged. Returns the number of bytes written
/// (excluding NUL), or 0 if `out_len == 0`.
#[no_mangle]
pub unsafe extern "C" fn bch2_demangle(
    mangled: *const c_char,
    out: *mut c_char,
    out_len: usize,
) -> usize {
    if out_len == 0 || mangled.is_null() || out.is_null() {
        return 0;
    }
    let s = match CStr::from_ptr(mangled).to_str() {
        Ok(s)  => s,
        Err(_) => return 0,
    };
    // {:#} suppresses the trailing hash for legacy Rust mangling.
    let demangled = format!("{:#}", rustc_demangle::demangle(s));
    let bytes = demangled.as_bytes();
    let n = bytes.len().min(out_len - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, n);
    *out.add(n) = 0;
    n
}
