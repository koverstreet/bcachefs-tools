// SPDX-License-Identifier: GPL-2.0
//! What util/vstructs.rs's traits need of a type C shares, from its
//! definition: #[derive(Plain)] - and #[bitfield], every one of which is -
//! #[counted_by(FIELD)], a CStruct's FlexArray, and #[derive(View)], its
//! fields' projections on a View of it.
//!
//! Plain is a promise: every bit pattern of the type's bytes is a value of
//! it, and every byte is a field's. These impls check it at compile time,
//! rather than take it on trust: each field's type is Plain, and the fields'
//! sizes add up to the type's, so there's no padding - zerocopy's FromBytes,
//! for the types we define.

use proc_macro::TokenStream;

use crate::cstruct::{field_type, is_phantom, parse_cstruct, CFieldDef, CKind, CStructDef, FieldType};
use crate::tokens;

/// #[derive(Plain)], on a struct - not a union, whose bytes past a short
/// arm's end are no field's. An unsized vstruct's (cstruct.rs' CGeneric) is
/// its sized instance's, C's: the unsized one's bytes are that header's,
/// then its tail's elements - Plain too, as its tail field checks.
pub(crate) fn derive_plain(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    if def.kind != CKind::Struct {
        return Err(format!("{}: Plain is for structs", def.name));
    }
    let tys: Vec<&str> = def.fields.iter()
        .map(|f| f.ty.as_str())
        .filter(|ty| !is_phantom(ty))
        .collect();
    Ok(tokens(&plain(&def.instance(), &tys)))
}

/// Plain for @name, whose fields are of types @tys: the impl, and the
/// checks that make it sound - a compile error if it isn't. Native too, as
/// itself.
pub(crate) fn plain(name: &str, tys: &[&str]) -> String {
    let fields: String = tys.iter()
        .map(|ty| format!("crate::util::vstructs::assert_plain::<{ty}>();\n"))
        .collect();
    let sizes: String = tys.iter()
        .map(|ty| format!(" + ::core::mem::size_of::<{ty}>()"))
        .collect();
    // and Native as itself: a struct reads as a struct
    format!(
        "unsafe impl crate::util::vstructs::Plain for {name} {{}}\n\
         impl crate::util::vstructs::Native for {name} {{\n\
             type Native = Self;\n\
             fn to_native(self) -> Self {{ self }}\n\
             fn from_native(n: Self) -> Self {{ n }}\n\
         }}\n\
         const _: () = {{\n\
             {fields}\
             assert!(::core::mem::size_of::<{name}>() == 0{sizes},\n\
                     \"{name}: padding between its fields - it isn't Plain\");\n\
         }};\n")
}

/// A View's own methods, which a field's projection can't be named.
const VIEW_METHODS: &[&str] = &["new", "get", "set", "whole", "whole_mut", "project", "as_view", "as_bytes"];

/// #[derive(View)], on a Plain struct: a projection for each field, on View
/// and ViewMut - `v.field()`, a View of the field, until field projections
/// make it `v.field`. Not for fields named `_..`, padding and C's internals,
/// or a trailing `[T; 0]`: a flexible array is View::as_bytes()'s. And
/// __NAME_min!(FIELD), for a type whose values may end before FIELD: see
/// min_arm().
pub(crate) fn derive_view(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    if def.kind != CKind::Struct {
        return Err(format!("{}: a View is of a struct", def.name));
    }
    if !def.generics.is_empty() {
        return Err(format!("{}: an unsized vstruct's reference has its length, and its header is \
                            whole: its fields are fields, with no View", def.name));
    }
    let name = &def.name;
    let (mut get, mut get_mut) = (String::new(), String::new());
    for f in &def.fields {
        let field = f.rust_name();
        let ty = f.ty.trim();
        if field.starts_with('_') || matches!(field_type(ty)?, FieldType::Array { n, .. } if n == "0") {
            continue;
        }
        if VIEW_METHODS.contains(&field.as_str()) {
            return Err(format!("{name}.{field}: a View's own method is named {field} - \
                                its projection would be hidden"));
        }
        let off = format!("::core::mem::offset_of!({name}, {field})");
        get.push_str(&format!(
            "pub fn {field}(&self) -> crate::util::vstructs::View<'a, {ty}> {{\n\
                 self.project({off}, \"{field}\")\n\
             }}\n"));
        get_mut.push_str(&format!(
            "pub fn {field}(&mut self) -> crate::util::vstructs::ViewMut<'_, {ty}> {{\n\
                 self.project({off}, \"{field}\")\n\
             }}\n"));
    }
    let arms: String = def.fields.iter().enumerate()
        .map(|(i, f)| min_arm(name, &def.fields[..i], &f.rust_name()))
        .collect();
    Ok(tokens(&format!(
        "#[allow(dead_code)]\n\
         impl<'a> crate::util::vstructs::View<'a, {name}> {{\n{get}}}\n\
         #[allow(dead_code)]\n\
         impl<'a> crate::util::vstructs::ViewMut<'a, {name}> {{\n{get_mut}}}\n\
         #[allow(unused_macros)]\n\
         macro_rules! __{name}_min {{\n{arms}}}\n\
         #[allow(unused_imports)]\n\
         pub(crate) use __{name}_min;\n")))
}

/// __@name_min!'s arm for @field: @name up to it, its fields @before - the
/// fields every valid value has, if one may end before @field - as
/// @name_min, and @name Floored by it. For a key type, whose
/// BCH_BKEY_TYPES() names @field: bkey_types! invokes it, through crate::c.
///
/// That's in another module, where @before's types as written may not be
/// named, so it names them as every format file does: c and crate::types.
fn min_arm(name: &str, before: &[CFieldDef], field: &str) -> String {
    let min = format!("{name}_min");
    let fields: String = before.iter()
        .map(|f| format!("pub {}: {},\n", f.rust_name(), f.ty.trim()))
        .collect();
    let offsets: String = before.iter()
        .map(|f| format!("assert!(::core::mem::offset_of!({min}, {0}) == \
                          ::core::mem::offset_of!(crate::cstructs::c::{name}, {0}));\n", f.rust_name()))
        .collect();
    let tys: Vec<&str> = before.iter().map(|f| f.ty.trim()).collect();
    let plain = plain(&min, &tys);
    format!(
        "({field}) => {{\n\
             mod __{min} {{\n\
                 #[allow(unused_imports)]\n\
                 use crate::cstructs::c;\n\
                 #[allow(unused_imports)]\n\
                 use crate::types::*;\n\
                 /// {name} up to {field}: the fields every valid one has.\n\
                 #[repr(C)]\n\
                 #[derive(Clone, Copy)]\n\
                 #[allow(non_camel_case_types)]\n\
                 pub struct {min} {{\n{fields}}}\n\
                 {plain}\
                 unsafe impl crate::util::vstructs::Floored for crate::cstructs::c::{name} {{\n\
                     type Min = {min};\n\
                 }}\n\
                 const _: () = {{\n\
                     {offsets}\
                     assert!(::core::mem::size_of::<{min}>() == \
                             ::core::mem::offset_of!(crate::cstructs::c::{name}, {field}));\n\
                 }};\n\
             }}\n\
             pub use __{min}::{min};\n\
         }};\n")
}

/// An unsized vstruct's Unsized (cstruct.rs' CGeneric, util/vstructs.rs'
/// Unsized), on the unsized instance in every state - and its constructors:
/// from C's pointer, from_ffi(), and an Unvalidated one from bytes,
/// from_bytes(). Empty for a struct with no type parameters.
pub(crate) fn unsized_impl(def: &CStructDef) -> Result<String, String> {
    let Some(elem) = def.generics.iter().find_map(|g| g.tail.as_ref()) else {
        return Ok(String::new());
    };
    let name = &def.name;
    let inst = def.instance();
    let states: Vec<&str> = def.generics.iter()
        .filter(|g| g.tail.is_none())
        .map(|g| g.name.as_str())
        .collect();
    let [state] = states.as_slice() else {
        return Err(format!("{name}: an unsized vstruct has one state parameter"));
    };
    let args = |state_arg: &str| -> String {
        def.generics.iter()
            .map(|g| if g.tail.is_some() { g.default.clone() } else { state_arg.to_string() })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ty = format!("{name}<{}>", args(state));
    let unvalidated = format!("{name}<{}>", args("crate::util::vstructs::Unvalidated"));
    let tail = def.fields.last().map(|f| f.rust_name()).unwrap_or_default();
    Ok(format!(
        "// SAFETY: one definition - its sized instance's fields are its own, at\n\
         // the same offsets - its tail at the header's end, and Plain, as the\n\
         // asserts below check; from_raw() makes the tail as long as @bytes\n\
         // past the header\n\
         unsafe impl<{state}> crate::util::vstructs::Unsized for {ty} {{\n\
             type Ffi = {inst};\n\
             type Elem = {elem};\n\
             #[inline]\n\
             fn from_raw(p: *const u8, bytes: usize) -> *const Self {{\n\
                 let n = (bytes - ::core::mem::size_of::<Self::Ffi>()) / ::core::mem::size_of::<{elem}>();\n\
                 ::core::ptr::slice_from_raw_parts(p.cast::<{elem}>(), n) as *const Self\n\
             }}\n\
             #[inline]\n\
             fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {{\n\
                 let n = (bytes - ::core::mem::size_of::<Self::Ffi>()) / ::core::mem::size_of::<{elem}>();\n\
                 ::core::ptr::slice_from_raw_parts_mut(p.cast::<{elem}>(), n) as *mut Self\n\
             }}\n\
         }}\n\
         const _: () = {{\n\
             crate::util::vstructs::assert_plain::<{elem}>();\n\
             // no padding after its last element, which a reference - as\n\
             // long as size_of_val(), rounded up to its alignment - would claim\n\
             assert!(::core::mem::size_of::<{elem}>() % ::core::mem::align_of::<{inst}>() == 0,\n\
                     \"{name}: its tail's elements are a multiple of its alignment long\");\n\
             assert!(::core::mem::offset_of!({inst}, {tail}) == ::core::mem::size_of::<{inst}>(),\n\
                     \"{name}: its tail starts where its header ends\");\n\
         }};\n\
         #[allow(dead_code)]\n\
         impl<{state}> {ty} {{\n\
             /// The {name} C's pointer @p is to, as long as its count says: not\n\
             /// widened from a reference to its header, which reaches only\n\
             /// that. In the state the caller names - {name}::<Validated>, for\n\
             /// what C read and validated - never one inferred.\n\
             ///\n\
             /// # Safety\n\
             /// @p points to a {name} whose count is right, and what its state\n\
             /// says of its entries' is true - for Validated, that C checked\n\
             /// them; and those bytes stay valid, and unwritten, for 'a.\n\
             pub unsafe fn from_ffi<'a>(p: *const {inst}) -> &'a Self {{\n\
                 // SAFETY: the caller's\n\
                 unsafe {{ crate::util::vstructs::from_ffi::<Self>(p) }}\n\
             }}\n\
             \n\
             /// from_ffi(), mutably.\n\
             ///\n\
             /// # Safety\n\
             /// As from_ffi()'s, and nothing else touches those bytes for 'a.\n\
             pub unsafe fn from_ffi_mut<'a>(p: *mut {inst}) -> &'a mut Self {{\n\
                 // SAFETY: the caller's\n\
                 unsafe {{ crate::util::vstructs::from_ffi_mut::<Self>(p) }}\n\
             }}\n\
             \n\
             /// For C: a pointer that reaches all of it.\n\
             pub fn as_ffi(&self) -> *const {inst} {{\n\
                 (self as *const Self).cast()\n\
             }}\n\
             \n\
             /// For C, to edit: a pointer that reaches all of it.\n\
             pub fn as_ffi_mut(&mut self) -> *mut {inst} {{\n\
                 (self as *mut Self).cast()\n\
             }}\n\
             \n\
             /// Its bytes, header and tail.\n\
             pub fn as_bytes(&self) -> &[u8] {{\n\
                 crate::util::vstructs::bytes_of(self)\n\
             }}\n\
             \n\
             /// Its bytes, to edit: for its own walks - its count is in them.\n\
             pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {{\n\
                 crate::util::vstructs::bytes_of_mut(self)\n\
             }}\n\
         }}\n\
         #[allow(dead_code)]\n\
         impl {unvalidated} {{\n\
             /// The {name} at the start of @bytes - off a device, from the\n\
             /// kernel - as many as its count says: an error if they don't hold\n\
             /// its header, aligned, or that many.\n\
             pub fn from_bytes(bytes: &[u8]) -> Result<&Self, crate::util::vstructs::VStructError> {{\n\
                 crate::util::vstructs::from_bytes::<Self>(bytes)\n\
             }}\n\
             \n\
             /// from_bytes(), mutably.\n\
             pub fn from_bytes_mut(bytes: &mut [u8]) -> Result<&mut Self, crate::util::vstructs::VStructError> {{\n\
                 crate::util::vstructs::from_bytes_mut::<Self>(bytes)\n\
             }}\n\
         }}\n"))
}

/// A CStruct's FlexArray, from #[counted_by(FIELD)] on its trailing `[T; 0]`
/// - empty if it has none.
pub(crate) fn flex_array(def: &CStructDef) -> Result<String, String> {
    let name = &def.name;
    let Some((i, f)) = def.fields.iter().enumerate().find(|(_, f)| f.counted_by.is_some()) else {
        return Ok(String::new());
    };
    let count = f.counted_by.as_deref().unwrap_or_default();
    let elem = match field_type(&f.ty)? {
        FieldType::Array { elem, n } if n == "0" && i + 1 == def.fields.len() => elem,
        _ => return Err(format!("{name}.{}: #[counted_by] is for the last field, a [T; 0]", f.name)),
    };
    if !def.fields.iter().any(|g| g.rust_name() == count) {
        return Err(format!("{name}.{}: #[counted_by({count})]: {name} has no field {count}", f.name));
    }
    Ok(flex_impl(name, &f.rust_name(), &elem, count))
}

/// FlexArray for @name: @field, [@elem; 0], as long as @count says.
fn flex_impl(name: &str, field: &str, elem: &str, count: &str) -> String {
    // the count by value: a reference to a packed struct's field may be
    // unaligned
    format!(
        "impl crate::util::vstructs::FlexArray for {name} {{\n\
             type Elem = {elem};\n\
             const TAIL: usize = ::core::mem::offset_of!({name}, {field});\n\
             fn nr(&self) -> usize {{ crate::util::vstructs::Count::count(&{{ self.{count} }}) }}\n\
         }}\n")
}
