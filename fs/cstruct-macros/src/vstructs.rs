// SPDX-License-Identifier: GPL-2.0
//! What util/vstructs.rs's traits need of a type C shares, from its
//! definition: #[derive(Plain)] - and #[bitfield], every one of which is -
//! #[counted_by(FIELD)], a CStruct's FlexArray, #[derive(View)], its
//! fields' projections on a View of it, and the families of definitions,
//! one per state: #[unsized_vstruct], an unsized vstruct's; #[grown], a type
//! that grew between format versions.
//!
//! Plain is a promise: every bit pattern of the type's bytes is a value of
//! it, and every byte is a field's. These impls check it at compile time,
//! rather than take it on trust: each field's type is Plain, and the fields'
//! sizes add up to the type's, so there's no padding - zerocopy's FromBytes,
//! for the types we define.

use proc_macro::{Delimiter, Ident, TokenStream, TokenTree};

use crate::cstruct::{field_type, parse_cstruct, CFieldDef, CKind, CStructDef, FieldType};
use crate::tokens;

/// #[derive(Plain)], on a struct - not a union, whose bytes past a short
/// arm's end are no field's. An unsized vstruct's states aren't: they're its
/// C definition's - #[unsized_vstruct]'s X__ffi - and then its tail's
/// elements, Plain too, as the attribute checks.
pub(crate) fn derive_plain(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    if def.kind != CKind::Struct {
        return Err(format!("{}: Plain is for structs", def.name));
    }
    let tys: Vec<&str> = def.fields.iter().map(|f| f.ty.as_str()).collect();
    Ok(tokens(&plain(def.rust_ident(), &tys)))
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
    let name = def.rust_ident();
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

// ---- Families: a type's definitions, one per state (util/vstructs.rs) ----

const VS: &str = "crate::util::vstructs";

/// A state's definition, @ty: a DST - @head, which it derefs to, its fields
/// and methods; then @tail, a slice of @elem - and C's struct, @ffi, its
/// HasFfi. It's Unsized: from_raw() at the length's worth of @elem past the
/// head; and SelfSized, and its bytes, by its reference's length. @doc is
/// its own; @methods, more of them.
fn dst(ty: &str, doc: &str, head: &str, tail: &str, elem: &str, ffi: &str, methods: &str) -> String {
    format!(
        "{doc}\
         #[repr(C)]\n\
         #[allow(non_camel_case_types)]\n\
         pub struct {ty} {{\n\
             head: {head},\n\
             pub {tail}: [{elem}],\n\
         }}\n\
         \n\
         impl ::core::ops::Deref for {ty} {{\n\
             type Target = {head};\n\
             #[inline]\n\
             fn deref(&self) -> &{head} {{\n\
                 &self.head\n\
             }}\n\
         }}\n\
         \n\
         impl ::core::ops::DerefMut for {ty} {{\n\
             #[inline]\n\
             fn deref_mut(&mut self) -> &mut {head} {{\n\
                 &mut self.head\n\
             }}\n\
         }}\n\
         \n\
         impl {VS}::HasFfi for {ty} {{\n\
             type Ffi = {ffi};\n\
         }}\n\
         \n\
         // SAFETY: its head, then its tail right after - as the assert below\n\
         // checks - both Plain; from_raw() makes the tail as long as @bytes past\n\
         // the head\n\
         unsafe impl {VS}::Unsized for {ty} {{\n\
             type Head = {head};\n\
             type Elem = {elem};\n\
             #[inline]\n\
             fn from_raw(p: *const u8, bytes: usize) -> *const Self {{\n\
                 let n = (bytes - ::core::mem::size_of::<{head}>()) / ::core::mem::size_of::<{elem}>();\n\
                 ::core::ptr::slice_from_raw_parts(p.cast::<{elem}>(), n) as *const Self\n\
             }}\n\
             #[inline]\n\
             fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {{\n\
                 let n = (bytes - ::core::mem::size_of::<{head}>()) / ::core::mem::size_of::<{elem}>();\n\
                 ::core::ptr::slice_from_raw_parts_mut(p.cast::<{elem}>(), n) as *mut Self\n\
             }}\n\
         }}\n\
         \n\
         const _: () = {{\n\
             {VS}::assert_plain::<{elem}>();\n\
             // the tail right after the head, no padding between\n\
             assert!(::core::mem::align_of::<{elem}>() <= ::core::mem::align_of::<{head}>(),\n\
                     \"{ty}: its tail is aligned no more than its head\");\n\
         }};\n\
         \n\
         impl {VS}::SelfSized for {ty} {{\n\
             #[inline]\n\
             fn bytes(&self) -> usize {{\n\
                 ::core::mem::size_of_val(self)\n\
             }}\n\
         }}\n\
         \n\
         #[allow(dead_code)]\n\
         impl {ty} {{\n\
             /// Its bytes, every one.\n\
             pub fn as_bytes(&self) -> &[u8] {{\n\
                 {VS}::bytes_of(self)\n\
             }}\n\
             \n\
             /// Its bytes, to edit: for its own walks - a count may be in them.\n\
             pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {{\n\
                 {VS}::bytes_of_mut(self)\n\
             }}\n\
             \n\
             {methods}\
         }}\n\
         \n")
}

/// @name's family, @name__family - its Unvalidated, Validated and Whole
/// definitions - and @name<S>, which picks one, documented @doc.
fn family(name: &str, unvalidated: &str, validated: &str, whole: &str, doc: &str) -> String {
    format!(
        "#[doc(hidden)]\n\
         #[allow(non_camel_case_types)]\n\
         pub struct {name}__family;\n\
         \n\
         impl {VS}::Family for {name}__family {{\n\
             type Unvalidated = {unvalidated};\n\
             type Validated = {validated};\n\
             type Whole = {whole};\n\
         }}\n\
         \n\
         {doc}\
         #[allow(non_camel_case_types)]\n\
         pub type {name}<S = {VS}::Validated> = <S as {VS}::State>::Of<{name}__family>;\n")
}

/// #[unsized_vstruct]: the family of an unsized vstruct's definitions, one
/// per state (util/vstructs.rs' Family), from C's struct as written.
///
/// C's definition is the item, renamed X__ffi, with #[c_name(X)]: C has it
/// as struct X, Rust as Ffi<X> - the alias X is the family's. Each state's,
/// X__unvalidated and X__validated, is Rust's alone: a DST, the header and
/// then its tail, as long as its count says - which derefs to the header,
/// where its fields and methods are, written once. Its header never grew:
/// Validated is whole.
///
/// The tail is the item's trailing flexible array, `NAME: [E; 0]` - the
/// states' is `NAME: [E]` - or, where the header doesn't end in one, what
/// the attribute names: `#[unsized_vstruct(tail = [E])]`, the states' `_tail`
/// - a superblock field's, past its header; a btree node's, its bset's keys.
pub(crate) fn unsized_vstruct(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    let name = def.name.clone();
    let last = def.fields.last().ok_or_else(|| format!("{name}: an unsized vstruct has fields"))?;
    let attr = attr.to_string();
    let (tail, elem) = match attr.trim() {
        "" => match field_type(&last.ty)? {
            FieldType::Array { elem, n } if n == "0" => (last.rust_name(), elem),
            _ => return Err(format!("{name}: its last field, {}, isn't a flexible array, [E; 0] - \
                                     #[unsized_vstruct(tail = [E])] says what its tail holds",
                                    last.name)),
        },
        a => match a.strip_prefix("tail").map(str::trim_start).and_then(|a| a.strip_prefix('=')) {
            Some(t) => match t.trim().strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
                Some(e) if !e.contains(';') => ("_tail".to_string(), e.trim().to_string()),
                _ => return Err(format!("{name}: tail = [E], a slice - not {t}")),
            },
            None => return Err(format!("{name}: #[unsized_vstruct] or #[unsized_vstruct(tail = [E])], \
                                        not ({a})")),
        },
    };
    let ffi = format!("{name}__ffi");
    let last = last.rust_name();

    let state = |st: &str, what: &str| -> String {
        let from_bytes = if st == "Unvalidated" {
            format!("/// The {name} at the start of @bytes - off a device, from the\n\
                     /// kernel - as many as its count says: an error if they don't hold\n\
                     /// its header, aligned, or that many.\n\
                     pub fn from_bytes(bytes: &[u8]) -> Result<&Self, {VS}::VStructError> {{\n\
                         {VS}::from_bytes::<Self>(bytes)\n\
                     }}\n\
                     \n\
                     /// from_bytes(), mutably.\n\
                     pub fn from_bytes_mut(bytes: &mut [u8]) -> Result<&mut Self, {VS}::VStructError> {{\n\
                         {VS}::from_bytes_mut::<Self>(bytes)\n\
                     }}\n")
        } else {
            String::new()
        };
        let methods = format!(
            "/// The {name} C's pointer @p is to, as long as its count says: not\n\
             /// widened from a reference to its header, which reaches only that.\n\
             /// In the state the caller names - {name}::<Validated>, for what C\n\
             /// read and validated - never one inferred.\n\
             ///\n\
             /// # Safety\n\
             /// @p points to a {name} whose count is right, and what its state\n\
             /// says of its entries' is true - for Validated, that C checked\n\
             /// them; and those bytes stay valid, and unwritten, for 'a.\n\
             pub unsafe fn from_ffi<'a>(p: *const {ffi}) -> &'a Self {{\n\
                 // SAFETY: the caller's\n\
                 unsafe {{ {VS}::from_ffi::<Self>(p) }}\n\
             }}\n\
             \n\
             /// from_ffi(), mutably.\n\
             ///\n\
             /// # Safety\n\
             /// As from_ffi()'s, and nothing else touches those bytes for 'a.\n\
             pub unsafe fn from_ffi_mut<'a>(p: *mut {ffi}) -> &'a mut Self {{\n\
                 // SAFETY: the caller's\n\
                 unsafe {{ {VS}::from_ffi_mut::<Self>(p) }}\n\
             }}\n\
             \n\
             /// For C: a pointer that reaches all of it.\n\
             pub fn as_ffi(&self) -> *const {ffi} {{\n\
                 (self as *const Self).cast()\n\
             }}\n\
             \n\
             /// For C, to edit: a pointer that reaches all of it.\n\
             pub fn as_ffi_mut(&mut self) -> *mut {ffi} {{\n\
                 (self as *mut Self).cast()\n\
             }}\n\
             \n\
             {from_bytes}");
        dst(&format!("{name}__{}", st.to_lowercase()),
            &format!("/// {name}<{st}>: {what}. Its header - C's struct {name},\n\
                      /// Ffi<{name}>, which it derefs to - then its tail, as long as its\n\
                      /// count says.\n"),
            &ffi, &tail, &elem, &ffi, &methods)
    };

    let mut out = rename_struct(item, &name, &ffi)?;
    out.extend(tokens(&format!(
        "{}{}\
         const _: () = {{\n\
             // no padding after its last element, which a reference - as long\n\
             // as size_of_val(), rounded up to its alignment - would claim\n\
             assert!(::core::mem::size_of::<{elem}>() % ::core::mem::align_of::<{ffi}>() == 0,\n\
                     \"{name}: its tail's elements are a multiple of its alignment long\");\n\
             // C's struct ends at its last field - its flexible array, or an\n\
             // embedded vstruct's header, which ends where that one's tail starts\n\
             assert!(::core::mem::offset_of!({ffi}, {last}) + crate::types::field_size(\n\
                         |p: *const {ffi}| unsafe {{ ::core::ptr::addr_of!((*p).{last}) }}) as usize\n\
                     == ::core::mem::size_of::<{ffi}>(),\n\
                     \"{name}: its tail starts where its header ends\");\n\
         }};\n\
         \n\
         {}",
        state("Unvalidated", "bytes from outside - off a device, from the kernel - its count \
                              checked against them when it's made"),
        state("Validated", "what C read and validated, its counts trusted as C trusts them"),
        family(&name, &format!("{name}__unvalidated"), &format!("{name}__validated"),
               &format!("{name}__validated"),
               &format!("/// {name}, in state S - Validated unless named: a reference to one\n\
                         /// reaches every byte of it and none past. C's struct {name} is\n\
                         /// Ffi<{name}>.\n")))));
    Ok(out)
}

/// #[grown(from = FIELD)]: the family of a type that grew between format
/// versions, from C's struct as written - FIELD the first field a valid
/// value may lack, written by a version from before it.
///
/// C's definition is the item, renamed X__ffi, with #[c_name(X)]: the whole
/// struct, X<Whole> - every field there, so every field a field. X__floor is
/// the fields before FIELD, every valid value's. X<Validated> - Unvalidated
/// too, as the cast checks the floor - is X__validated, a DST: the floor,
/// which it derefs to, then the bytes there are, at exactly the value's
/// length; a field from FIELD on is a getter, zero past the end, and
/// whole() is all of it, if it's all there.
pub(crate) fn grown(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    let name = def.name.clone();
    let attr = attr.to_string();
    let from = attr.trim().strip_prefix("from").map(str::trim_start).and_then(|a| a.strip_prefix('='))
        .map(str::trim)
        .ok_or_else(|| format!("{name}: #[grown(from = FIELD)], FIELD the first field an older \
                                version's value may lack - not ({attr})"))?;
    let at = def.fields.iter().position(|f| f.rust_name() == from)
        .ok_or_else(|| format!("{name}: #[grown(from = {from})], but {name} has no field {from}"))?;
    if at == 0 {
        return Err(format!("{name}: #[grown(from = {from})]: every valid value has at least one field"));
    }
    let (floor, later) = def.fields.split_at(at);

    let ffi = format!("{name}__ffi");
    let fl = format!("{name}__floor");
    let ty = format!("{name}__validated");

    let floor_fields: String = floor.iter()
        .map(|f| format!("    pub {}: {},\n", f.rust_name(), f.ty.trim()))
        .collect();
    let floor_tys: Vec<&str> = floor.iter().map(|f| f.ty.trim()).collect();
    let floor_offsets: String = floor.iter()
        .map(|f| format!("assert!(::core::mem::offset_of!({fl}, {0}) == ::core::mem::offset_of!({ffi}, {0}));\n",
                         f.rust_name()))
        .collect();

    const METHODS: &[&str] = &["whole", "whole_mut", "as_bytes", "as_bytes_mut", "from_bytes", "from_bytes_mut"];
    let mut getters = String::new();
    for f in later {
        let field = f.rust_name();
        if METHODS.contains(&field.as_str()) {
            return Err(format!("{name}.{field}: {ty}'s own method is named {field} - its getter \
                                would be hidden"));
        }
        let fty = f.ty.trim();
        getters.push_str(&format!(
            "/// {field}, or zero where the value ends before it: one written\n\
             /// before {name} had it.\n\
             pub fn {field}(&self) -> {fty} {{\n\
                 {VS}::read_past::<{fty}>(&self._rest,\n\
                     ::core::mem::offset_of!({ffi}, {field}) - ::core::mem::size_of::<{fl}>())\n\
             }}\n\
             \n"));
    }

    let methods = format!(
        "/// @b as one, every byte of it: None if it can't be - fewer than its\n\
         /// floor, not aligned for it, or not a whole number of its alignment\n\
         /// long, which a reference, as long as size_of_val(), would claim past.\n\
         fn from_bytes(b: &[u8]) -> Option<&Self> {{\n\
             let align = ::core::mem::align_of::<{fl}>();\n\
             (b.len() >= ::core::mem::size_of::<{fl}>() && b.as_ptr() as usize % align == 0\n\
              && b.len() % align == 0)\n\
                 // SAFETY: as checked: the floor's bytes are there, aligned, and the\n\
                 // reference claims exactly @b\n\
                 .then(|| unsafe {{ &*<Self as {VS}::Unsized>::from_raw(b.as_ptr(), b.len()) }})\n\
         }}\n\
         \n\
         /// from_bytes(), mutably.\n\
         fn from_bytes_mut(b: &mut [u8]) -> Option<&mut Self> {{\n\
             let align = ::core::mem::align_of::<{fl}>();\n\
             (b.len() >= ::core::mem::size_of::<{fl}>() && b.as_ptr() as usize % align == 0\n\
              && b.len() % align == 0)\n\
                 // SAFETY: as from_bytes()\n\
                 .then(|| unsafe {{ &mut *<Self as {VS}::Unsized>::from_raw_mut(b.as_mut_ptr(), b.len()) }})\n\
         }}\n\
         \n\
         /// All of it, {name}<Whole>, if it's all there - every field a field.\n\
         pub fn whole(&self) -> Option<&{ffi}> {{\n\
             (::core::mem::size_of_val(self) >= ::core::mem::size_of::<{ffi}>()\n\
              && (self as *const Self).cast::<u8>() as usize % ::core::mem::align_of::<{ffi}>() == 0)\n\
                 // SAFETY: that many bytes are its, aligned, and C's struct is Plain\n\
                 .then(|| unsafe {{ &*(self as *const Self).cast::<{ffi}>() }})\n\
         }}\n\
         \n\
         /// whole(), to edit.\n\
         pub fn whole_mut(&mut self) -> Option<&mut {ffi}> {{\n\
             (::core::mem::size_of_val(self) >= ::core::mem::size_of::<{ffi}>()\n\
              && (self as *const Self).cast::<u8>() as usize % ::core::mem::align_of::<{ffi}>() == 0)\n\
                 // SAFETY: as whole()\n\
                 .then(|| unsafe {{ &mut *(self as *mut Self).cast::<{ffi}>() }})\n\
         }}\n\
         \n\
         {getters}");

    let mut out = rename_struct(item, &name, &ffi)?;
    out.extend(tokens(&format!(
        "/// {name}'s floor: the fields every valid one has - all it had before\n\
         /// it grew, from {from} on.\n\
         #[repr(C)]\n\
         #[derive(Clone, Copy)]\n\
         #[allow(non_camel_case_types)]\n\
         pub struct {fl} {{\n{floor_fields}}}\n\
         {plain}\
         const _: () = {{\n\
             {floor_offsets}\
             assert!(::core::mem::size_of::<{fl}>() == ::core::mem::offset_of!({ffi}, {from}),\n\
                     \"{name}: its floor ends where {from} starts\");\n\
         }};\n\
         \n\
         {dst}\
         impl {VS}::ArmRef for {ty} {{\n\
             type Ref<'a> = &'a Self;\n\
             type Mut<'a> = &'a mut Self;\n\
             \n\
             fn arm_ref(b: &[u8]) -> Option<&Self> {{\n\
                 Self::from_bytes(b)\n\
             }}\n\
             \n\
             fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {{\n\
                 Self::from_bytes_mut(b)\n\
             }}\n\
         }}\n\
         \n\
         {family}",
        plain = plain(&fl, &floor_tys),
        dst = dst(&ty,
                  &format!("/// {name}<Validated>: what C read and validated, written by any version\n\
                            /// since it had its floor - the floor, which it derefs to, then the\n\
                            /// bytes there are, at exactly the value's length. A field it gained\n\
                            /// later is a getter, zero where the value ends before it; whole() is\n\
                            /// all of it, {name}<Whole>, if it's all there.\n"),
                  &fl, "_rest", "u8", &ffi, &methods),
        // the cast checks the floor: an unvalidated one is as good
        family = family(&name, &ty, &ty, &ffi,
                        &format!("/// {name}, in state S - Validated unless named: from any version since\n\
                                  /// its floor, at its own length. {name}<Whole> is all of it - C's\n\
                                  /// struct {name}, Ffi<{name}>.\n")))));
    Ok(out)
}

/// @item, a struct, as @to - with #[c_name(@name)], C's name for it, after
/// its other attributes, so the derives among them see it.
fn rename_struct(item: TokenStream, name: &str, to: &str) -> Result<TokenStream, String> {
    let mut tts: Vec<TokenTree> = item.into_iter().collect();
    let at = tts.windows(2)
        .position(|w| matches!((&w[0], &w[1]), (TokenTree::Ident(s), TokenTree::Ident(n))
                               if s.to_string() == "struct" && n.to_string() == name))
        .ok_or_else(|| format!("{name}: #[unsized_vstruct] is for a struct"))?;
    tts[at + 1] = TokenTree::Ident(Ident::new(to, tts[at + 1].span()));
    let mut attrs_end = 0;
    while matches!((tts.get(attrs_end), tts.get(attrs_end + 1)),
                   (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g)))
                       if p.as_char() == '#' && g.delimiter() == Delimiter::Bracket) {
        attrs_end += 2;
    }
    let mut out: TokenStream = tts[..attrs_end].iter().cloned().collect();
    out.extend(tokens(&format!("#[c_name({name})]")));
    out.extend(tts[attrs_end..].iter().cloned());
    Ok(out)
}

/// A CStruct's FlexArray, from #[counted_by(FIELD)] on its trailing `[T; 0]`
/// - empty if it has none.
pub(crate) fn flex_array(def: &CStructDef) -> Result<String, String> {
    let name = def.rust_ident();
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
