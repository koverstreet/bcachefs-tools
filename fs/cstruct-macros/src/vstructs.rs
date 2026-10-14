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

use crate::cstruct::{field_type, parse_cstruct, parse_family_args, CFieldDef, CKind, CStructDef, FieldType};
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

/// What a state's definition is laid out as, for dst().
#[derive(Clone, Copy)]
struct Shape<'a> {
    /// The item's #[repr]s, as written.
    repr:    &'a str,
    /// C's fields it has inline, at @head's offsets.
    fields:  &'a [CFieldDef],
    /// Those the family makes private: a counted type's count.
    private: &'a [&'a str],
    /// What precedes the tail: C's struct, or a grown type's floor.
    head:    &'a str,
    /// What it derefs to, #[repr(transparent)] over @head if not @head
    /// itself: a #[whole] type's X<Whole>, where X's methods are.
    deref:   &'a str,
    /// After the fields: @tail, a slice of @elem.
    tail:    &'a str,
    elem:    &'a str,
    /// C's struct: its HasFfi.
    ffi:     &'a str,
    /// Where the tail starts, if not where the head ends: inside it, the
    /// head being C's struct rounded up into the tail's first bytes - a key
    /// value's (#[whole(tail = ..)]).
    tail_at: Option<&'a str>,
}

/// A state's definition, @ty, laid out as @shape: a DST - its fields inline,
/// those that hold a count private (inline_field()), then its tail.
///
/// Its fields are its head's, at the head's offsets - asserted - so it derefs
/// to the head: reads, and the head's methods, written once there. No
/// DerefMut: a &mut to the head would let a count - its own, or an embedded
/// header's - be rewritten, or the whole head assigned or swapped, from
/// outside what holds the value. Its own fields that hold one are private,
/// and it's unsized, so it can't be assigned or swapped whole either.
///
/// It's Unsized: from_raw() at the length's worth of elements past the head;
/// SelfSized, and its bytes, by its reference's length; and an arm's,
/// ArmRef, cast() from the bytes its storage says are its. @doc is its own;
/// @methods, more of them.
fn dst(ty: &str, doc: &str, shape: &Shape<'_>, methods: &str) -> String {
    let Shape { repr, fields, private, head, deref, tail, elem, ffi, tail_at } = *shape;
    let tail_at = tail_at.map(str::to_string).unwrap_or_else(|| format!("::core::mem::size_of::<{head}>()"));
    let plain_fields: String = fields.iter()
        .map(|f| format!("{VS}::assert_plain::<{}>();\n", f.ty.trim()))
        .collect();
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
             type Target = {deref};\n\
             #[inline]\n\
             fn deref(&self) -> &{deref} {{\n\
                 // SAFETY: its first size_of::<{head}>() bytes are {head}'s fields, at\n\
                 // {head}'s offsets - asserted below, each Plain - and any of its\n\
                 // tail they reach: it's no shorter than {head}, as Unsized says;\n\
                 // {deref} is {head}, or #[repr(transparent)] over it\n\
                 unsafe {{ &*(self as *const Self).cast::<{deref}>() }}\n\
             }}\n\
         }}\n\
         \n\
         impl {VS}::HasFfi for {ty} {{\n\
             type Ffi = {ffi};\n\
         }}\n\
         \n\
         // SAFETY: its head's fields, each Plain, then its tail at TAIL - as the\n\
         // asserts below check - Plain; from_raw() makes the tail as long as\n\
         // @bytes past TAIL\n\
         unsafe impl {VS}::Unsized for {ty} {{\n\
             type Head = {head};\n\
             type Elem = {elem};\n\
             const TAIL: usize = {tail_at};\n\
             #[inline]\n\
             fn from_raw(p: *const u8, bytes: usize) -> *const Self {{\n\
                 let n = (bytes - Self::TAIL) / ::core::mem::size_of::<{elem}>();\n\
                 ::core::ptr::slice_from_raw_parts(p.cast::<{elem}>(), n) as *const Self\n\
             }}\n\
             #[inline]\n\
             fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {{\n\
                 let n = (bytes - Self::TAIL) / ::core::mem::size_of::<{elem}>();\n\
                 ::core::ptr::slice_from_raw_parts_mut(p.cast::<{elem}>(), n) as *mut Self\n\
             }}\n\
         }}\n\
         \n\
         const _: () = {{\n\
             {VS}::assert_plain::<{elem}>();\n\
             {plain_fields}\
             {offsets}\
             // its tail starts at TAIL - where its head ends, or inside it:\n\
             // from_raw()'s length counts on it\n\
             assert!(({tail_start}).next_multiple_of(::core::mem::align_of::<{elem}>())\n\
                         == <{ty} as {VS}::Unsized>::TAIL,\n\
                     \"{ty}: its tail starts where its fields end, at TAIL\");\n\
             assert!(<{ty} as {VS}::Unsized>::TAIL <= ::core::mem::size_of::<{head}>());\n\
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
/// definitions. Every family has @name__unvalidated and @name__validated: a
/// plain alias where a state shares another's definition, so a container's
/// ArmOf impls can name each state's - concrete types, not projections.
///
/// And __@name__state_impls!{ items }: the items, impl'd on each of
/// @with_fields, the definitions with C's fields inline - for c_bitmask!'s
/// setters, which the Deref to C's struct can't reach: there's no DerefMut.
fn family(name: &str, unvalidated: &str, validated: &str, whole: &str, with_fields: &[String])
    -> String
{
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
         {state_impls}")
}

/// @name<S>, picking one of @name's family's definitions, documented @doc:
/// for an unsized type, whose name is its family's - C's struct is only its
/// header.
fn state_alias(name: &str, doc: &str) -> String {
    format!("{doc}\
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

/// The family of a type whose count is its own - an unsized vstruct's, a
/// #[counted] type's: both states laid out as @shape - Unvalidated, bytes
/// from outside, its count checked against them; Validated, its count
/// trusted as C trusts it - and Validated its Whole, the header never having
/// grown.
fn own_count_family(name: &str, shape: &Shape<'_>) -> String {
    let state = |st: &str, what: &str| -> String {
        let ty = format!("{name}__{}", st.to_lowercase());
        dst(&ty,
            &format!("/// {name}<{st}>: {what}. C's fields - those that hold a count\n\
                      /// private - then its tail, as long as its count says. It derefs\n\
                      /// to C's struct {name}, Ffi<{name}>, to read.\n"),
            shape, &state_methods(name, shape.ffi, st))
            + &format!(
                "// a sized tagged union's arm: as many bytes as its count says\n\
                 impl {VS}::ArmAt for {ty} {{\n\
                     fn arm_at(b: &[u8]) -> Result<&Self, {VS}::VStructError> {{\n\
                         {VS}::from_bytes::<Self>(b)\n\
                     }}\n\
                 }}\n")
    };
    format!(
        "{}{}{}{}",
        state("Unvalidated", "bytes from outside - off a device, from the kernel - its count \
                              checked against them when it's made"),
        state("Validated", "what C read and validated, its counts trusted as C trusts them"),
        family(name, &format!("{name}__unvalidated"), &format!("{name}__validated"),
               &format!("{name}__validated"),
               &[format!("{name}__unvalidated"), format!("{name}__validated")]),
        state_alias(name,
               &format!("/// {name}, in state S - Validated unless named: a reference to one\n\
                         /// reaches every byte of it and none past. C's struct {name} is\n\
                         /// Ffi<{name}>.\n")))
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
/// the attribute says: `tail = [E]`, the states' `_tail`, the raw rest - a
/// superblock field's, past its header; a btree node's, its bset's keys - or
/// `tail = NAME: [E]`, an array the states call NAME.
pub(crate) fn unsized_vstruct(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    let name = def.name.clone();
    let last = def.fields.last().ok_or_else(|| format!("{name}: an unsized vstruct has fields"))?;
    let args = parse_family_args(&attr.to_string(), &["tail"]).map_err(|e| format!("{name}: {e}"))?;
    let (tail, elem) = match args.tail {
        None => match field_type(&last.ty)? {
            FieldType::Array { elem, n } if n == "0" => (last.rust_name(), elem),
            _ => return Err(format!("{name}: its last field, {}, isn't a flexible array, [E; 0] - \
                                     #[unsized_vstruct(tail = [E])] says what its tail holds",
                                    last.name)),
        },
        Some((tail, elem)) => (tail.unwrap_or_else(|| "_tail".to_string()), elem),
    };
    let ffi = format!("{name}__ffi");
    let last = last.rust_name();
    // C's fields, inline in each state's definition - but its flexible array,
    // where that's the tail
    let fields = if tail == last { &def.fields[..def.fields.len() - 1] } else { &def.fields[..] };
    let repr = repr_attrs(&item);
    let shape = Shape { repr: &repr, fields, private: &[], head: &ffi, deref: &ffi, tail: &tail, elem: &elem,
                        ffi: &ffi, tail_at: None };

    let mut out = rename_struct(item, &name, &ffi)?;
    out.extend(tokens(&format!(
        "const _: () = {{\n\
             // C's struct ends at its last field - its flexible array, or an\n\
             // embedded vstruct's header, which ends where that one's tail starts\n\
             assert!(::core::mem::offset_of!({ffi}, {last}) + crate::types::field_size(\n\
                         |p: *const {ffi}| unsafe {{ ::core::ptr::addr_of!((*p).{last}) }}) as usize\n\
                     == ::core::mem::size_of::<{ffi}>(),\n\
                     \"{name}: its tail starts where its header ends\");\n\
         }};\n\
         \n\
         {}",
        own_count_family(&name, &shape))));
    Ok(out)
}

/// #[counted], #[counted(PATH)], #[counted(PATH, tail = ..)]: the family of a
/// type that's a header and an array it counts in elements - an ioctl's
/// reply, a replicas usage record. Not a vstruct: no u64s, no entries to
/// walk; its tail is just the array.
///
/// PATH is the count, as vstruct!'s is - a field, or a field of one it
/// embeds (r.nr_devs) - and nr_entries if not named, the convention. Its
/// first field holds the count, so that's private in each state's
/// definition; C's struct keeps it as written, so a header can still be
/// built standalone. The array is the item's trailing [E; 0], or what the
/// attribute says - as #[unsized_vstruct]'s, `tail = [E]`, the states'
/// `_tail`, or `tail = NAME: [E]`, a field of a field the states call NAME.
///
/// C's struct gets FlexArray from the count - its size, TAIL and the
/// elements past it - and CountedFfi, the way to its Unvalidated definition
/// from what holds its bytes.
pub(crate) fn counted(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    let name = def.name.clone();
    let ffi = format!("{name}__ffi");

    let args = parse_family_args(&attr.to_string(), &["count", "tail"]).map_err(|e| format!("{name}: {e}"))?;
    let count = args.count.unwrap_or_else(|| "nr_entries".to_string());
    let count_field = count.split('.').next().unwrap_or_default().to_string();
    if !def.fields.iter().any(|f| f.rust_name() == count_field) {
        return Err(format!("{name}: #[counted({count})], but {name} has no field {count_field} - \
                            name its count, #[counted(FIELD)] or #[counted(FIELD.COUNT)]"));
    }

    // the array: a trailing [E; 0], or a field of a field - after C's struct,
    // where dst() asserts its tail starts
    let (tail, elem, fields, tail_at) = match args.tail {
        Some((tail, elem)) => (tail.unwrap_or_else(|| "_tail".to_string()), elem, &def.fields[..],
                               format!("::core::mem::size_of::<{ffi}>()")),
        None => {
            let last = def.fields.last().ok_or_else(|| format!("{name}: a counted type has fields"))?;
            match field_type(&last.ty)? {
                FieldType::Array { elem, n } if n == "0" =>
                    (last.rust_name(), elem, &def.fields[..def.fields.len() - 1],
                     format!("::core::mem::offset_of!({ffi}, {})", last.rust_name())),
                _ => return Err(format!("{name}: its last field, {}, isn't a flexible array, [E; 0] - \
                                         #[counted(.., tail = [E])] says where its array is",
                                        last.name)),
            }
        }
    };
    let repr = repr_attrs(&item);
    let private = [count_field.as_str()];
    let shape = Shape { repr: &repr, fields, private: &private, head: &ffi, deref: &ffi, tail: &tail,
                        elem: &elem, ffi: &ffi, tail_at: None };

    let mut out = rename_struct(item, &name, &ffi)?;
    out.extend(tokens(&format!(
        "{}\
         impl {VS}::CountedFfi for {ffi} {{\n\
             type Unvalidated = {name}__unvalidated;\n\
         }}\n\
         \n\
         // a reference as long as its count says is a whole number of its\n\
         // alignment only if its elements are: else it claims bytes past it\n\
         const _: () = assert!(::core::mem::size_of::<{elem}>() % ::core::mem::align_of::<{ffi}>() == 0,\n\
             \"{name}: its elements aren't a whole number of its alignment\");\n\
         \n\
         {}",
        flex_impl(&ffi, &tail_at, &elem, &count),
        own_count_family(&name, &shape))));
    Ok(out)
}

/// #[grown(from = FIELD)]: the family of a type that grew between format
/// versions, from C's struct as written - FIELD the first field a valid
/// value may lack, written by a version from before it.
///
/// X<S> is a generic struct over its definition in state S. X<Whole> - X -
/// is the whole struct, every field a field: the item, renamed X__whole with
/// #[c_name(X)], C's struct. X__floor is the fields before FIELD, every valid
/// value's. X<Validated> - Unvalidated too, as the cast checks the floor - is
/// a value in place, a DST: the floor's fields, then the bytes there are, at
/// exactly the value's length; it derefs to the floor, to read. A field from
/// FIELD on is a getter, zero past the end, and whole() is all of it, X, if
/// it's all there.
pub(crate) fn grown(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    let name = def.name.clone();
    let args = parse_family_args(&attr.to_string(), &["from"]).map_err(|e| format!("{name}: {e}"))?;
    let from = args.from.as_deref()
        .ok_or_else(|| format!("{name}: #[grown(from = FIELD)], FIELD the first field an older \
                                version's value may lack"))?;
    let at = def.fields.iter().position(|f| f.rust_name() == from)
        .ok_or_else(|| format!("{name}: #[grown(from = {from})], but {name} has no field {from}"))?;
    if at == 0 {
        return Err(format!("{name}: #[grown(from = {from})]: every valid value has at least one field"));
    }
    sized_family(&def, item, Some((from, at)), None)
}

/// #[whole]: the family of a type every valid value of which has all of it -
/// C's validate rejects one shorter - though it may be longer, written by a
/// version since: a superblock section or journal entry of fixed fields.
///
/// X<S> is a generic struct over its definition in state S. X<Whole> - X -
/// is the struct: the item, renamed X__whole with #[c_name(X)], C's struct.
/// X<Validated> and X<Unvalidated> are a value in place, a DST: X's fields,
/// those that hold a count private, then the bytes past them there are, at
/// exactly the value's length; it derefs to X, to read, and X's methods.
///
/// Or `#[whole(tail = NAME: [E])]`: C's struct ends in NAME: [E; 0] - and
/// perhaps other zero-length arrays, C's other names for where it starts - a
/// header and the array after it: a key value, its length the key's. The
/// tail is NAME, from where C has it: inside C's struct, if align rounds the
/// struct up into the array's first bytes - bch_inode's fields end at 14, its
/// size is 16 - which the deref to C's struct reaches, a value being no
/// shorter than that.
///
/// Or `tail = NAME: [u8]`, over any zero-length array: the bytes from where
/// it starts, for a tail that's more than that array - bch_stripe's
/// pointers, then checksums and sector counts its header sizes.
pub(crate) fn whole(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item.to_string())?;
    let args = parse_family_args(&attr.to_string(), &["tail"]).map_err(|e| format!("{}: {e}", def.name))?;
    let tail = match args.tail {
        None => None,
        Some((None, _)) => return Err(format!("{}: #[whole(tail = NAME: [E])] - the tail is C's last \
                                               field, NAME: [E; 0], named", def.name)),
        Some((Some(tail), elem)) => {
            let at = def.fields.iter().position(|f| f.rust_name() == tail)
                .ok_or_else(|| format!("{}: #[whole(tail = {tail}: ..)], but it has no field {tail}", def.name))?;
            // [E; 0] - or any zero-length array, for a tail of bytes
            match field_type(&def.fields[at].ty)? {
                FieldType::Array { elem: e, n }
                    if n == "0" && (e.trim() == elem.trim() || elem.trim() == "u8") => {}
                _ => return Err(format!("{}: #[whole(tail = {tail}: [{elem}])], but its field {tail} \
                                         isn't [{elem}; 0]", def.name)),
            }
            // after it, only C's other names for where it starts
            for f in &def.fields[at + 1..] {
                if !matches!(field_type(&f.ty)?, FieldType::Array { n, .. } if n == "0") {
                    return Err(format!("{}.{}: after the tail, {tail}, a field that isn't a \
                                        zero-length array - C's other name for its start", def.name, f.name));
                }
            }
            Some((tail, elem))
        }
    };
    sized_family(&def, item, None, tail)
}

/// #[grown] and #[whole]: the family of sized C struct @def, written @item -
/// @grew, the first field an older version's value may lack and its index,
/// if it grew.
///
/// Both have a definition of all of it, every field inline: X__whole, if it
/// grew - its Whole; else X__validated, its every state. Only that one is
/// handed to C, as_ffi(): it's the one sure to hold all of C's struct.
fn sized_family(def: &CStructDef, item: TokenStream, grew: Option<(&str, usize)>,
                tail: Option<(String, String)>)
    -> Result<TokenStream, String>
{
    let name = def.name.clone();
    // The struct as written, renamed X__whole: X<Whole>'s representation,
    // and C's struct X
    let ffi = format!("{name}__whole");
    // A value in place, as long as what holds it says: for a grown type the
    // floor and the bytes there are, below; else all of its fields, then
    // the bytes past them
    let all = format!("{name}__validated");
    let repr = repr_attrs(&item);
    let derives = derive_names(&item);
    // X is generated: a derive generic_type() doesn't forward would be
    // X__whole's alone, out of X's reach. CStruct is C's record of the
    // struct as written, which is X__whole's
    if let Some(d) = derives.iter().find(|d| *d != "CStruct" && !WHOLE_DERIVES.contains(&d.as_str())) {
        return Err(format!("{name}: #[derive({d})], but {name} - {name}<Whole> - is generated, and has \
                            only the derives generic_type() forwards to it: {}", WHOLE_DERIVES.join(", ")));
    }
    // the tail, C's last field - or the bytes past C's fields
    let (fields, tail_name, elem, tail_at) = match &tail {
        Some((t, e)) => {
            let at = def.fields.iter().position(|f| f.rust_name() == *t).unwrap_or(def.fields.len());
            (&def.fields[..at], t.as_str(), e.as_str(), Some(format!("::core::mem::offset_of!({ffi}, {t})")))
        }
        None => (&def.fields[..], "_rest", "u8", None),
    };
    let Some((from, at)) = grew else {
        // Every valid value has all of it: in place, all its fields and the
        // bytes past them, in every state but Whole, the struct itself
        let all_def = dst(&all,
            &match &tail {
                Some((t, _)) => format!("/// {name} in place: its fields - those that hold a count private -\n\
                                         /// then {t}, from where C has it, as long as the value is. It\n\
                                         /// derefs to the struct, to read.\n"),
                None => format!("/// {name} in place: its fields - those that hold a count private - then\n\
                                 /// the bytes past them there are, written by a version since, at\n\
                                 /// exactly the value's length. It derefs to the struct, to read.\n"),
            },
            &Shape { repr: &repr, fields, private: &[], head: &ffi, deref: &name, tail: tail_name, elem,
                     ffi: &ffi, tail_at: tail_at.as_deref() },
            &format!("/// For C: a pointer to all of it.\n\
                      pub fn as_ffi(&self) -> *const {ffi} {{\n\
                          (self as *const Self).cast()\n\
                      }}\n\
                      \n\
                      /// For C, to edit: a pointer to all of it.\n\
                      pub fn as_ffi_mut(&mut self) -> *mut {ffi} {{\n\
                          (self as *mut Self).cast()\n\
                      }}\n"));
        let mut out = rename_struct(item, &name, &ffi)?;
        out.extend(tokens(&format!(
            "{all_def}{}{}{}",
            family(&name, &all, &all, &ffi, &[]),
            whole_arm_ref(&ffi, derives.iter().any(|d| d == "Plain")),
            generic_type(&name, &all, &ffi, &derives,
                         &format!("/// {name}, in state S - Whole unless named: the struct, every field a field.\n\
                                   /// Validated or Unvalidated, a value in place - all its fields, then\n\
                                   /// the bytes past them a version since wrote, at the value's length.\n")))));
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
        "/// All of it, if it's all there - every field a field.\n\
         pub fn whole(&self) -> Option<&{name}> {{\n\
             // SAFETY: {name} is #[repr(transparent)] over {ffi}\n\
             {VS}::cast_plain::<{ffi}>(self.as_bytes())\n\
                 .map(|w| unsafe {{ &*(w as *const {ffi} as *const {name}) }})\n\
         }}\n\
         \n\
         /// whole(), to edit.\n\
         pub fn whole_mut(&mut self) -> Option<&mut {name}> {{\n\
             // SAFETY: as whole()'s\n\
             {VS}::cast_plain_mut::<{ffi}>(self.as_bytes_mut())\n\
                 .map(|w| unsafe {{ &mut *(w as *mut {ffi} as *mut {name}) }})\n\
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
         {family}\
         {whole}\
         {generic}",
        plain = plain(&fl, &floor_tys),
        dst = dst(&ty,
                  &format!("/// A {name} as it was written, by any version since it had its floor:\n\
                            /// the floor's fields, those that hold a count private, then the\n\
                            /// bytes there are, at exactly the value's length. It derefs to the\n\
                            /// floor, to read. A field it gained later is a getter, zero where\n\
                            /// the value ends before it; whole() is all of it, if it's all there.\n"),
                  &Shape { repr: "#[repr(C)]\n", fields: floor, private: &[], head: &fl, deref: &fl,
                           tail: "_rest", elem: "u8", ffi: &ffi, tail_at: None },
                  &methods),
        // the cast checks the floor: an unvalidated one is as good
        family = family(&name, &ty, &ty, &ffi, &[]),
        whole = whole_arm_ref(&ffi, derives.iter().any(|d| d == "Plain")),
        generic = generic_type(&name, &ty, &ffi, &derives,
                               &format!("/// {name}, in state S - Whole unless named: the struct, every field a field.\n\
                                         /// Validated or Unvalidated, a value in place, from any version since\n\
                                         /// it had its floor: the floor's fields, then the bytes there are; a\n\
                                         /// field it gained later is a getter, zero where the value ends.\n")))));
    Ok(out)
}

/// The traits @item derives.
fn derive_names(item: &TokenStream) -> Vec<String> {
    let tts: Vec<TokenTree> = item.clone().into_iter().collect();
    let mut out = Vec::new();
    for w in tts.windows(2) {
        let (TokenTree::Punct(p), TokenTree::Group(g)) = (&w[0], &w[1]) else { continue };
        if p.as_char() != '#' || g.delimiter() != Delimiter::Bracket {
            continue;
        }
        let inner: Vec<TokenTree> = g.stream().into_iter().collect();
        if let [TokenTree::Ident(d), TokenTree::Group(args)] = inner.as_slice() {
            if d.to_string() == "derive" {
                out.extend(args.stream().into_iter().filter_map(|t| match t {
                    TokenTree::Ident(i) => Some(i.to_string()),
                    _ => None,
                }));
            }
        }
    }
    out
}

/// @whole, a sized struct, from the bytes its storage says are its: all of
/// it, aligned - a value longer than it, written by a version since, is
/// still all of it. To edit, only if it's Plain (@plain): one with padding -
/// a key value's header, rounded up into its tail - would write its padding
/// over the tail.
fn whole_arm_ref(whole: &str, plain: bool) -> String {
    let (get, get_mut) = if plain {
        (format!("{VS}::cast_plain(b)"), format!("{VS}::cast_plain_mut(b)"))
    } else {
        (format!("// SAFETY: that many bytes, aligned, and its fields - each Plain, as its\n\
                  // value in place asserts - any bytes are; its padding is never read\n\
                  (b.len() >= ::core::mem::size_of::<Self>() &&\n\
                   b.as_ptr() as usize % ::core::mem::align_of::<Self>() == 0)\n\
                      .then(|| unsafe {{ &*b.as_ptr().cast::<Self>() }})"),
         "let _ = b;\nNone".to_string())
    };
    format!(
        "impl {VS}::ArmRef for {whole} {{\n\
             fn arm_ref(b: &[u8]) -> Option<&Self> {{\n\
                 {get}\n\
             }}\n\
             \n\
             fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {{\n\
                 {get_mut}\n\
             }}\n\
         }}\n\
         \n")
}

/// The derives generic_type() gives X<Whole> as the struct has them.
const WHOLE_DERIVES: &[&str] = &["Clone", "Copy", "Default", "Plain", "TypeInfo"];

/// @name<S>, documented @doc: a generic struct over its family's definition
/// in state S - @whole, the struct, for Whole; @inplace, a value in place,
/// for Validated and Unvalidated. #[repr(transparent)], so a reference to
/// one is a reference to the other, and impls attach to @name<S> for the
/// state they're for. What generic code goes by - ArmOf, ArmRef, Unsized -
/// it has as its definition does; the value traits @derives names, @name -
/// @name<Whole> - has as the struct does.
fn generic_type(name: &str, inplace: &str, whole: &str, derives: &[String], doc: &str) -> String {
    let fam = format!("{name}__family");
    let mut out = format!(
        "{doc}\
         #[repr(transparent)]\n\
         #[allow(non_camel_case_types)]\n\
         pub struct {name}<S: {VS}::State = {VS}::Whole> {{\n\
             _state:   ::core::marker::PhantomData<S>,\n\
             /// Its definition in state S - for offset_of!(), repr.field.\n\
             pub repr: <S as {VS}::State>::Of<{fam}>,\n\
         }}\n\
         \n\
         impl<S: {VS}::State> ::core::ops::Deref for {name}<S> {{\n\
             type Target = <S as {VS}::State>::Of<{fam}>;\n\
             #[inline]\n\
             fn deref(&self) -> &Self::Target {{\n\
                 &self.repr\n\
             }}\n\
         }}\n\
         \n\
         impl<S: {VS}::State> ::core::ops::DerefMut for {name}<S> {{\n\
             #[inline]\n\
             fn deref_mut(&mut self) -> &mut Self::Target {{\n\
                 &mut self.repr\n\
             }}\n\
         }}\n\
         \n\
         // An arm of what its value in place is an arm of: one tag, every state\n\
         impl<U: ?Sized, S: {VS}::State> crate::types::ArmOf<U> for {name}<S>\n\
         where\n\
             {inplace}: crate::types::ArmOf<U>,\n\
         {{\n\
             type Tag = <{inplace} as crate::types::ArmOf<U>>::Tag;\n\
             const TAG: Self::Tag = <{inplace} as crate::types::ArmOf<U>>::TAG;\n\
         }}\n\
         \n");

    // In every state, as its definition in that state is had: the pointer
    // cast keeps the definition's metadata, its last field being it
    out.push_str(&format!(
        "impl<S: {VS}::State> {VS}::ArmRef for {name}<S> {{\n\
             fn arm_ref(b: &[u8]) -> Option<&Self> {{\n\
                 // SAFETY: #[repr(transparent)] over its definition in S\n\
                 <<S as {VS}::State>::Of<{fam}> as {VS}::ArmRef>::arm_ref(b)\n\
                     .map(|d| unsafe {{ &*(d as *const <S as {VS}::State>::Of<{fam}> as *const Self) }})\n\
             }}\n\
             \n\
             fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {{\n\
                 // SAFETY: as arm_ref()'s\n\
                 <<S as {VS}::State>::Of<{fam}> as {VS}::ArmRef>::arm_mut(b)\n\
                     .map(|d| unsafe {{ &mut *(d as *mut <S as {VS}::State>::Of<{fam}> as *mut Self) }})\n\
             }}\n\
         }}\n\
         \n"));

    for st in ["Validated", "Unvalidated"] {
        out.push_str(&format!(
            "// SAFETY: #[repr(transparent)] over {inplace}, whose Unsized it is\n\
             unsafe impl {VS}::Unsized for {name}<{VS}::{st}> {{\n\
                 type Head = <{inplace} as {VS}::Unsized>::Head;\n\
                 type Elem = <{inplace} as {VS}::Unsized>::Elem;\n\
                 const TAIL: usize = <{inplace} as {VS}::Unsized>::TAIL;\n\
                 #[inline]\n\
                 fn from_raw(p: *const u8, bytes: usize) -> *const Self {{\n\
                     <{inplace} as {VS}::Unsized>::from_raw(p, bytes) as *const Self\n\
                 }}\n\
                 #[inline]\n\
                 fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {{\n\
                     <{inplace} as {VS}::Unsized>::from_raw_mut(p, bytes) as *mut Self\n\
                 }}\n\
             }}\n\
             \n\
             impl {VS}::SelfSized for {name}<{VS}::{st}> {{\n\
                 #[inline]\n\
                 fn bytes(&self) -> usize {{\n\
                     ::core::mem::size_of_val(self)\n\
                 }}\n\
             }}\n\
             \n"));
    }

    let whole_ty = format!("{name}<{VS}::Whole>");
    let has = |d: &str| derives.iter().any(|x| x == d);
    if has("Clone") {
        out.push_str(&format!(
            "impl ::core::clone::Clone for {whole_ty} {{\n\
                 fn clone(&self) -> Self {{\n\
                     {name} {{ _state: ::core::marker::PhantomData, repr: self.repr.clone() }}\n\
                 }}\n\
             }}\n\n"));
    }
    if has("Copy") {
        out.push_str(&format!("impl ::core::marker::Copy for {whole_ty} {{}}\n\n"));
    }
    if has("Default") {
        out.push_str(&format!(
            "impl ::core::default::Default for {whole_ty} {{\n\
                 fn default() -> Self {{\n\
                     {name} {{ _state: ::core::marker::PhantomData, repr: ::core::default::Default::default() }}\n\
                 }}\n\
             }}\n\n"));
    }
    if has("Plain") {
        out.push_str(&format!(
            "// SAFETY: #[repr(transparent)] over {whole}, which is Plain\n\
             unsafe impl {VS}::Plain for {whole_ty} {{}}\n\n"));
    }
    if has("TypeInfo") {
        out.push_str(&format!(
            "impl crate::typeinfo::TypeInfo for {whole_ty} {{\n\
                 const INFO: &'static crate::typeinfo::StructInfo =\n\
                     <{whole} as crate::typeinfo::TypeInfo>::INFO;\n\
             }}\n\n"));
    }
    out
}

/// @item, a struct, as @to - with #[c_name(@name)], C's name for it, after
/// its other attributes, so the derives among them see it.
fn rename_struct(item: TokenStream, name: &str, to: &str) -> Result<TokenStream, String> {
    let mut tts: Vec<TokenTree> = item.into_iter().collect();
    let at = tts.windows(2)
        .position(|w| matches!((&w[0], &w[1]), (TokenTree::Ident(s), TokenTree::Ident(n))
                               if s.to_string() == "struct" && n.to_string() == name))
        .ok_or_else(|| format!("{name}: a family is made from a struct"))?;
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
    Ok(flex_impl(name, &format!("::core::mem::offset_of!({name}, {})", f.rust_name()), &elem, count))
}

/// FlexArray for @name: @elem from @tail_at, as many as @count says.
fn flex_impl(name: &str, tail_at: &str, elem: &str, count: &str) -> String {
    // the count by value: a reference to a packed struct's field may be
    // unaligned
    format!(
        "impl {VS}::FlexArray for {name} {{\n\
             type Elem = {elem};\n\
             const TAIL: usize = {tail_at};\n\
             #[inline]\n\
             fn nr(&self) -> usize {{ {VS}::Count::count(&{{ self.{count} }}) }}\n\
         }}\n\
         \n")
}
