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

/// A field of C's struct as a state's definition has it, inline: pub only
/// where C's is pub and holds no count - not the count, written private or
/// pub(crate), nor an embedded header, Ffi<..>, whose own count it is, nor
/// one of @private, which the family names: a counted type's count. Those
/// read through the definition's Deref to its head, and can't be written: a
/// count changes only through what holds the value.
fn inline_field(f: &CFieldDef, private: &[&str]) -> String {
    let vis = if f.vis == "pub" && !f.ty.contains("Ffi<") && !private.contains(&f.rust_name().as_str()) {
        "pub "
    } else {
        ""
    };
    format!("    {vis}{}: {},\n", f.rust_name(), f.ty.trim())
}

/// @item's #[repr(..)]s, as written.
fn repr_attrs(item: &TokenStream) -> String {
    let tts: Vec<TokenTree> = item.clone().into_iter().collect();
    tts.windows(2)
        .filter_map(|w| match (&w[0], &w[1]) {
            (TokenTree::Punct(p), TokenTree::Group(g))
                if p.as_char() == '#' && g.delimiter() == Delimiter::Bracket
                    && g.stream().to_string().trim_start().starts_with("repr") =>
                Some(format!("#[{}]\n", g.stream())),
            _ => None,
        })
        .collect()
}

/// A state's definition, @ty: a DST - @fields inline, as @repr lays them
/// out, those of @private private (inline_field()), then @tail, a slice of
/// @elem - and C's struct, @ffi, its HasFfi.
///
/// Its fields are @head's, at @head's offsets - asserted - so it derefs to
/// @head: reads, and @head's methods, written once there. No DerefMut: a &mut
/// to the head would let a count - its own, or an embedded header's - be
/// rewritten, or the whole head assigned or swapped, from outside what holds
/// the value. Its own fields that hold one are private (inline_field()), and
/// it's unsized, so it can't be assigned or swapped whole either.
///
/// It's Unsized: from_raw() at the length's worth of @elem past the head;
/// SelfSized, and its bytes, by its reference's length; and an arm's,
/// ArmRef, cast() from the bytes its storage says are its. @doc is its own;
/// @methods, more of them.
#[allow(clippy::too_many_arguments)]
fn dst(ty: &str, doc: &str, repr: &str, fields: &[CFieldDef], private: &[&str], head: &str, tail: &str, elem: &str,
       ffi: &str, methods: &str) -> String {
    let inline: String = fields.iter().map(|f| inline_field(f, private)).collect();
    let offsets: String = fields.iter()
        .map(|f| format!("assert!(::core::mem::offset_of!({ty}, {0}) == ::core::mem::offset_of!({head}, {0}),\n\
                              \"{ty}.{0}: not at {head}'s offset\");\n", f.rust_name()))
        .collect();
    let tail_start = match fields.last() {
        Some(l) => format!("::core::mem::offset_of!({ty}, {}) + ::core::mem::size_of::<{}>()",
                           l.rust_name(), l.ty.trim()),
        None => "0".to_string(),
    };
    format!(
        "{doc}\
         {repr}\
         #[allow(non_camel_case_types)]\n\
         pub struct {ty} {{\n\
         {inline}\
             pub {tail}: [{elem}],\n\
         }}\n\
         \n\
         impl ::core::ops::Deref for {ty} {{\n\
             type Target = {head};\n\
             #[inline]\n\
             fn deref(&self) -> &{head} {{\n\
                 // SAFETY: its first size_of::<{head}>() bytes are {head}'s fields, at\n\
                 // {head}'s offsets, its tail after them - asserted below - and\n\
                 // {head} is Plain\n\
                 unsafe {{ &*(self as *const Self).cast::<{head}>() }}\n\
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
             {offsets}\
             // its tail starts where its head ends: from_raw()'s length, and\n\
             // the Deref, count on it\n\
             assert!(({tail_start}).next_multiple_of(::core::mem::align_of::<{elem}>())\n\
                         == ::core::mem::size_of::<{head}>(),\n\
                     \"{ty}: its tail starts where {head} ends\");\n\
         }};\n\
         \n\
         impl {VS}::SelfSized for {ty} {{\n\
             #[inline]\n\
             fn bytes(&self) -> usize {{\n\
                 ::core::mem::size_of_val(self)\n\
             }}\n\
         }}\n\
         \n\
         // as a tagged union's arm, over the bytes its storage says are its\n\
         impl {VS}::ArmRef for {ty} {{\n\
             type Ref<'a> = &'a Self;\n\
             type Mut<'a> = &'a mut Self;\n\
             \n\
             fn arm_ref(b: &[u8]) -> Option<&Self> {{\n\
                 {VS}::cast(b)\n\
             }}\n\
             \n\
             fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {{\n\
                 {VS}::cast_mut(b)\n\
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
/// definitions - and @name<S>, which picks one, documented @doc. Every family
/// has @name__unvalidated and @name__validated: a plain alias where a state
/// shares another's definition, so a container's ArmOf impls can name each
/// state's - concrete types, not projections.
///
/// And __@name__state_impls!{ items }: the items, impl'd on each of
/// @with_fields, the definitions with C's fields inline - for c_bitmask!'s
/// setters, which the Deref to C's struct can't reach: there's no DerefMut.
fn family(name: &str, unvalidated: &str, validated: &str, whole: &str, with_fields: &[String],
          doc: &str) -> String {
    let aliases: String = [("unvalidated", unvalidated), ("validated", validated)].iter()
        .filter(|(st, def)| *def != format!("{name}__{st}"))
        .map(|(st, def)| format!("#[allow(non_camel_case_types)]\n\
                                  pub type {name}__{st} = {def};\n"))
        .collect();
    let state_impls: String = with_fields.iter()
        .map(|d| format!("impl {d} {{ $($items)* }}\n"))
        .collect();
    let state_impls = format!(
        "#[doc(hidden)]\n\
         #[allow(unused_macros)]\n\
         macro_rules! __{name}__state_impls {{\n\
             ($($items:tt)*) => {{\n{state_impls}}};\n\
         }}\n");
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
         {aliases}\
         {state_impls}\
         {doc}\
         #[allow(non_camel_case_types)]\n\
         pub type {name}<S = {VS}::Validated> = <S as {VS}::State>::Of<{name}__family>;\n")
}

/// The methods of @name's state @st's definition, a family whose count is
/// its own - an unsized vstruct's, a counted type's: made from C's pointer,
/// or, Unvalidated, from bytes; and handed back to C.
fn state_methods(name: &str, ffi: &str, st: &str) -> String {
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
    format!(
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
         {from_bytes}")
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
    // C's fields, inline in each state's definition - but its flexible array,
    // where that's the tail
    let fields = if tail == last { &def.fields[..def.fields.len() - 1] } else { &def.fields[..] };
    let repr = repr_attrs(&item);

    let state = |st: &str, what: &str| -> String {
        dst(&format!("{name}__{}", st.to_lowercase()),
            &format!("/// {name}<{st}>: {what}. C's fields - those that hold a count\n\
                      /// private - then its tail, as long as its count says. It derefs\n\
                      /// to C's struct {name}, Ffi<{name}>, to read.\n"),
            &repr, fields, &[], &ffi, &tail, &elem, &ffi, &state_methods(&name, &ffi, st))
    };

    let mut out = rename_struct(item, &name, &ffi)?;
    out.extend(tokens(&format!(
        "{}{}\
         const _: () = {{\n\
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
               &[format!("{name}__unvalidated"), format!("{name}__validated")],
               &format!("/// {name}, in state S - Validated unless named: a reference to one\n\
                         /// reaches every byte of it and none past. C's struct {name} is\n\
                         /// Ffi<{name}>.\n")))));
    Ok(out)
}

/// #[counted], #[counted(PATH)], #[counted(PATH, tail = NAME: [E])]: the
/// family of a type that's a header and an array it counts in elements - an
/// ioctl's reply, a replicas usage record. Not a vstruct: no u64s, no entries
/// to walk; its tail is just the array.
///
/// PATH is the count, as vstruct!'s is - a field, or a field of one it
/// embeds (r.nr_devs) - and nr_entries if not named, the convention. Its
/// first field holds the count, so that's private in each state's
/// definition; C's struct keeps it as written, so a header can still be
/// built standalone. The array is the item's trailing [E; 0], or, where it's
/// a field of a field, `tail = NAME: [E]` - the states' NAME.
///
/// C's struct gets FlexArray from the count - its size, TAIL and the
/// elements past it - and CountedFfi, the way to its Unvalidated definition
/// from what holds its bytes.
pub(crate) fn counted(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    let name = def.name.clone();
    let ffi = format!("{name}__ffi");

    let mut count = "nr_entries".to_string();
    let mut tail_spec = None;
    for part in attr.to_string().split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.strip_prefix("tail").map(str::trim_start).and_then(|p| p.strip_prefix('=')) {
            Some(t) => tail_spec = Some(t.trim().to_string()),
            None => count = part.replace(' ', ""),
        }
    }
    let count_field = count.split('.').next().unwrap_or_default().to_string();
    if !def.fields.iter().any(|f| f.rust_name() == count_field) {
        return Err(format!("{name}: #[counted({count})], but {name} has no field {count_field} - \
                            name its count, #[counted(FIELD)] or #[counted(FIELD.COUNT)]"));
    }

    // the array: a trailing [E; 0], or a field of a field - after C's struct,
    // where dst() asserts its tail starts
    let (tail, elem, fields, tail_at) = match tail_spec {
        Some(t) => {
            let bad = || format!("{name}: #[counted(.., tail = NAME: [E])], not tail = {t}");
            let (n, e) = t.split_once(':').ok_or_else(bad)?;
            let e = e.trim().strip_prefix('[').and_then(|e| e.strip_suffix(']'))
                .filter(|e| !e.contains(';')).ok_or_else(bad)?;
            (n.trim().to_string(), e.trim().to_string(), &def.fields[..],
             format!("::core::mem::size_of::<{ffi}>()"))
        }
        None => {
            let last = def.fields.last().ok_or_else(|| format!("{name}: a counted type has fields"))?;
            match field_type(&last.ty)? {
                FieldType::Array { elem, n } if n == "0" =>
                    (last.rust_name(), elem, &def.fields[..def.fields.len() - 1],
                     format!("::core::mem::offset_of!({ffi}, {})", last.rust_name())),
                _ => return Err(format!("{name}: its last field, {}, isn't a flexible array, [E; 0] - \
                                         #[counted(.., tail = NAME: [E])] says where its array is",
                                        last.name)),
            }
        }
    };
    let repr = repr_attrs(&item);
    let private = [count_field.as_str()];

    let state = |st: &str, what: &str| -> String {
        dst(&format!("{name}__{}", st.to_lowercase()),
            &format!("/// {name}<{st}>: {what}. C's fields - those that hold a count\n\
                      /// private - then its array, {tail}, as long as its count says.\n\
                      /// It derefs to C's struct {name}, Ffi<{name}>, to read.\n"),
            &repr, fields, &private, &ffi, &tail, &elem, &ffi, &state_methods(&name, &ffi, st))
    };

    let mut out = rename_struct(item, &name, &ffi)?;
    out.extend(tokens(&format!(
        "impl {VS}::FlexArray for {ffi} {{\n\
             type Elem = {elem};\n\
             const TAIL: usize = {tail_at};\n\
             #[inline]\n\
             fn nr(&self) -> usize {{\n\
                 // by value: a packed struct's field can't be borrowed\n\
                 {VS}::Count::count(&{{ self.{count} }})\n\
             }}\n\
         }}\n\
         \n\
         impl {VS}::CountedFfi for {ffi} {{\n\
             type Unvalidated = {name}__unvalidated;\n\
         }}\n\
         \n\
         // a reference as long as its count says is a whole number of its\n\
         // alignment only if its elements are: else it claims bytes past it\n\
         const _: () = assert!(::core::mem::size_of::<{elem}>() % ::core::mem::align_of::<{ffi}>() == 0,\n\
             \"{name}: its elements aren't a whole number of its alignment\");\n\
         \n\
         {}{}{}",
        state("Unvalidated", "bytes from outside - from the kernel - its count checked \
                              against them when it's made"),
        state("Validated", "what C read and validated, its count trusted as C trusts it"),
        family(&name, &format!("{name}__unvalidated"), &format!("{name}__validated"),
               &format!("{name}__validated"),
               &[format!("{name}__unvalidated"), format!("{name}__validated")],
               &format!("/// {name}, in state S - Validated unless named: a reference to one\n\
                         /// reaches every element of it and none past. C's struct {name} is\n\
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
/// too, as the cast checks the floor - is X__validated, a DST: the floor's
/// fields, then the bytes there are, at exactly the value's length; it derefs
/// to the floor, to read. A field from FIELD on is a getter, zero past the
/// end, and whole() is all of it, if it's all there.
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
    sized_family(&def, item, Some((from, at)))
}

/// #[whole]: the family of a type every valid value of which has all of it -
/// C's validate rejects one shorter - though it may be longer, written by a
/// version since: a superblock section or journal entry of fixed fields.
///
/// C's definition is the item, renamed X__ffi, with #[c_name(X)]. Every
/// state of it is X__validated, a DST: C's fields, those that hold a count
/// private, then the bytes past them there are, at exactly the value's
/// length; it derefs to C's struct, to read.
pub(crate) fn whole(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    if !attr.is_empty() {
        return Err(format!("{}: #[whole] takes nothing", def.name));
    }
    sized_family(&def, item, None)
}

/// #[grown] and #[whole]: the family of sized C struct @def, written @item -
/// @grew, the first field an older version's value may lack and its index,
/// if it grew.
///
/// Both have a definition of all of it, every field inline: X__whole, if it
/// grew - its Whole; else X__validated, its every state. Only that one is
/// handed to C, as_ffi(): it's the one sure to hold all of C's struct.
fn sized_family(def: &CStructDef, item: TokenStream, grew: Option<(&str, usize)>)
    -> Result<TokenStream, String>
{
    let name = def.name.clone();
    let ffi = format!("{name}__ffi");
    let all = match grew {
        Some(_) => format!("{name}__whole"),
        None => format!("{name}__validated"),
    };
    let all_def = dst(&all,
        &format!("/// {name}, all of it: C's fields - those that hold a count private - then\n\
                  /// the bytes past them there are, written by a version since, at\n\
                  /// exactly the value's length. It derefs to C's struct {name},\n\
                  /// Ffi<{name}>, to read.\n"),
        &repr_attrs(&item), &def.fields, &[], &ffi, "_rest", "u8", &ffi,
        &format!("/// For C: a pointer to all of it.\n\
                  pub fn as_ffi(&self) -> *const {ffi} {{\n\
                      (self as *const Self).cast()\n\
                  }}\n\
                  \n\
                  /// For C, to edit: a pointer to all of it.\n\
                  pub fn as_ffi_mut(&mut self) -> *mut {ffi} {{\n\
                      (self as *mut Self).cast()\n\
                  }}\n"));

    let Some((from, at)) = grew else {
        let mut out = rename_struct(item, &name, &ffi)?;
        out.extend(tokens(&format!(
            "{all_def}{}",
            family(&name, &all, &all, &all, &[all.clone()],
                   &format!("/// {name}, in state S - all of it in every one: C's validate rejects\n\
                             /// one shorter. C's struct {name} is Ffi<{name}>.\n")))));
        return Ok(out);
    };
    let (floor, later) = def.fields.split_at(at);

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

    const METHODS: &[&str] = &["whole", "whole_mut", "as_bytes", "as_bytes_mut"];
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
        "/// All of it, {name}<Whole>, if it's all there - every field a field.\n\
         pub fn whole(&self) -> Option<&{all}> {{\n\
             {VS}::cast(self.as_bytes())\n\
         }}\n\
         \n\
         /// whole(), to edit.\n\
         pub fn whole_mut(&mut self) -> Option<&mut {all}> {{\n\
             {VS}::cast_mut(self.as_bytes_mut())\n\
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
         {all_def}\
         {family}",
        plain = plain(&fl, &floor_tys),
        dst = dst(&ty,
                  &format!("/// {name}<Validated>: what C read and validated, written by any version\n\
                            /// since it had its floor - the floor's fields, those that hold a\n\
                            /// count private, then the bytes there are, at exactly the value's\n\
                            /// length. It derefs to the floor, to read. A field it gained later\n\
                            /// is a getter, zero where the value ends before it; whole() is all\n\
                            /// of it, {name}<Whole>, if it's all there.\n"),
                  "#[repr(C)]\n", floor, &[], &fl, "_rest", "u8", &ffi, &methods),
        // the cast checks the floor: an unvalidated one is as good. Its later
        // fields are inline only in its Whole, so that's where c_bitmask!'s
        // setters go
        family = family(&name, &ty, &ty, &all, &[all.clone()],
                        &format!("/// {name}, in state S - Validated unless named: from any version since\n\
                                  /// its floor, at its own length. {name}<Whole> is all of it, every\n\
                                  /// field a field. C's struct {name} is Ffi<{name}>.\n")))));
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
