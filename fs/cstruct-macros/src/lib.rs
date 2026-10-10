// SPDX-License-Identifier: GPL-2.0
//! The macros for types defined in Rust that C shares - see fs/types/lib.rs.
//!
//! Each parses its input with fs/types/cstruct.rs - the parser
//! rust_types_gen uses for the same text - so a mistake is a compile error
//! here, at the item, rather than bad C later. Each expands to its Rust side
//! and to its record: built with --cfg bch_cstruct_records, a static in the
//! .discard.bch_cstruct section, with the item's source as text and, for a struct,
//! its layout. Without that cfg, the record is compiled out.
//!
//!   #[derive(CStruct)]   a #[repr(C)] struct or union; no Rust side
//!   #[bitfield(uN)]      bitfield_struct::bitfield, recorded: the struct
//!                        that holds one in a #[c_bitfield] field declares
//!                        its members in C
//!   c_verbatim!(r#".."#) C carried as is; no Rust side
//!   c_xmacro! { .. }     an x-macro list: Rust gets NAME!(cb), handing cb!
//!                        the whole list
//!   c_bitmask! { .. }    bitfields in a flags word: Rust gets a getter and
//!                        a setter on the struct for each
//!   c_ioctl! { .. }      ioctl numbers: C gets the _IO*() #defines, Rust a
//!                        marker type for each, its opcode and argument type
//!   c_enum! { .. }       an enum, #[open], #[closed] or #[flags]: Rust gets
//!                        the type that is, and its values as constants
//!   c_const! { .. }      an integer constant: C gets a #define
//!   c_typedef! { .. }    a type alias: C gets a typedef
//!   c_same! { .. }       a Rust type with a C type's layout: checked in C
//!   c_extern! { .. }     what Rust calls of a C header: C gets the
//!                        prototypes, which its own must agree with
//!   rust_c_extern! { .. } what C calls of Rust through a pointer: C gets
//!                        the prototypes, Rust definitions with C's
//!                        signatures, calling the Rust functions
//!   tagged_union! { .. } a tagged union with a stable representation: C's
//!                        storage, the tag a Determinant of it, arms from an
//!                        x-macro list, read as an enum (tagged_union.rs)
//!
//! Zero dependencies, as typeinfo-macros: the kernel build compiles this with
//! a bare rustc.

use proc_macro::{Delimiter, TokenStream, TokenTree};

// A proc-macro crate may export only its macros: the parser's pub items go in
// a private module - pub for rust_types_gen, which include!s it too.
#[allow(unreachable_pub)]
mod cstruct {
    include!("../../types/cstruct.rs");
}
use cstruct::{parse_cstruct, parse_items, CEnumKind, CItem, CXEntry};

mod tagged_union;

/// A tagged union with a stable representation: see tagged_union.rs.
#[proc_macro]
pub fn tagged_union(input: TokenStream) -> TokenStream {
    tagged_union::tagged_union(input)
}

/// tagged_union!'s callback from its list, with the list's entries.
#[doc(hidden)]
#[proc_macro]
pub fn __tagged_union(input: TokenStream) -> TokenStream {
    tagged_union::expand(input)
}

fn compile_error(msg: &str) -> TokenStream {
    format!("::core::compile_error!({msg:?});").parse().unwrap()
}

/// The record of @text, and @nums: Rust expressions, the numbers that go
/// with it - see fs/types/lib.rs.
fn record(text: &str, nums: &[String]) -> String {
    format!(
        "#[cfg(bch_cstruct_records)]\n\
         const _: () = {{\n\
             const TEXT: &str = {text:?};\n\
             const NUMS: &[u64] = &[{}];\n\
             const LEN: usize = crate::types::record_len(::core::file!(), TEXT, NUMS.len());\n\
             #[used]\n\
             #[link_section = \".discard.bch_cstruct\"]\n\
             static RECORD: [u8; LEN] =\n\
                 crate::types::record::<LEN>(::core::file!(), ::core::line!(), TEXT, NUMS);\n\
         }};\n",
        nums.join(", "))
}

/// @text, which must parse as one item of the kind @is_kind accepts: @what
/// names that kind for the error.
fn one_item(text: &str, what: &str, is_kind: fn(&CItem) -> bool) -> Result<CItem, String> {
    let mut items = parse_items(text)?;
    match items.pop() {
        Some(item) if items.is_empty() && is_kind(&item) => Ok(item),
        _ => Err(format!("expected {what}")),
    }
}

/// `#[c("...")]` is the field's C declaration, name included; `#[c_anon("...")]`
/// a C declaration that doesn't name it. A bare `#[c_anon]` is the field's
/// type written in its place, unnamed - C's anonymous struct or union - and
/// `#[c_inline]` the same, named; `#[c_struct_group]` as the kernel's
/// struct_group(). `#[c_bitfield]` marks a field of a #[bitfield] type, whose
/// members C declares in its place.
/// `#[c_typedef]` on the struct: C has it as a typedef - `typedef struct {
/// ... } name`, or with `#[c_typedef(NAME)]`, `typedef struct name { ... }
/// NAME`.
#[proc_macro_derive(CStruct, attributes(c, c_align, c_anon, c_bitfield, c_inline, c_packed, c_struct_group, c_typedef))]
pub fn derive_cstruct(input: TokenStream) -> TokenStream {
    let text = format!("#[derive(CStruct)] {input}");
    let def = match parse_cstruct(&text) {
        Ok(def) => def,
        Err(e) => return compile_error(&format!("#[derive(CStruct)]: {e}")),
    };
    if !def.repr_c() {
        return compile_error(&format!("#[derive(CStruct)]: {} must be #[repr(C)]: C shares it", def.name));
    }

    let name = &def.name;
    let mut nums = vec![
        format!("::core::mem::size_of::<{name}>() as u64"),
        format!("::core::mem::align_of::<{name}>() as u64"),
    ];
    for f in &def.fields {
        let field = f.rust_name();
        let proj = format!("|p: *const {name}| unsafe {{ ::core::ptr::addr_of!((*p).{field}) }}");
        nums.push(format!("::core::mem::offset_of!({name}, {field}) as u64"));
        nums.push(format!("crate::types::field_size({proj})"));
        nums.push(format!("crate::types::field_align({proj})"));
    }
    record(&text, &nums).parse().unwrap()
}

/// bitfield_struct::bitfield, recorded.
#[proc_macro_attribute]
pub fn bitfield(attr: TokenStream, item: TokenStream) -> TokenStream {
    let text = format!("#[bitfield({attr})] {item}");
    let name = match one_item(&text, "a bitfield struct", |i| matches!(i, CItem::Bitfield(_))) {
        Ok(CItem::Bitfield(b)) => b.name,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("#[bitfield]: {e}")),
    };
    format!("{}#[::bitfield_struct::bitfield({attr})] {item}\n{}",
            record(&text, &[]), typeinfo_size_only(&name, "Struct"))
        .parse().unwrap()
}

/// What #[derive(TypeInfo)] gives an enum or union: its size, no fields.
/// Structs deriving it describe fields of this type by reference to it.
fn typeinfo_size_only(name: &str, shape: &str) -> String {
    format!(
        "#[automatically_derived]\n\
         impl crate::typeinfo::TypeInfo for {name} {{\n\
             const INFO: &'static crate::typeinfo::StructInfo = &crate::typeinfo::StructInfo {{\n\
                 name: \"{name}\",\n\
                 size: ::core::mem::size_of::<{name}>(),\n\
                 shape: crate::typeinfo::Shape::{shape},\n\
                 fields: &[],\n\
             }};\n\
         }}\n")
}

/// C carried as is: c_verbatim!(r#"..."#).
#[proc_macro]
pub fn c_verbatim(input: TokenStream) -> TokenStream {
    let text = format!("c_verbatim!({input})");
    if let Err(e) = one_item(&text, "one string", |i| matches!(i, CItem::Verbatim(_))) {
        return compile_error(&format!("c_verbatim!: {e}"));
    }
    record(&text, &[]).parse().unwrap()
}

/// An x-macro list: c_xmacro! { NAME(x) { (args), ... } }. Rust gets
/// NAME!(cb), which expands to cb! { (args), ... } - and NAME!(cb [..]),
/// to cb! { [..] (args), ... }: a c_enum! built from lists passes on the
/// variants so far. cb may be a path.
///
/// An entry can be another list, SUB(), or another list through a mapping
/// macro, SUB(MAP(params) => (args)) - C's #define MAP(params) x(args). Then
/// NAME!(cb) is a chain of macros, one per list it splices in: each is SUB!
/// handed the entries so far, which adds SUB's, mapped, and those after,
/// and calls the next; the last calls cb!. They're named by path, through
/// crate::cstructs::c - NAME is expanded wherever it's used.
#[proc_macro]
pub fn c_xmacro(input: TokenStream) -> TokenStream {
    let text = format!("c_xmacro! {{ {input} }}");
    let x = match one_item(&text, "NAME(x) { (args), ... }", |i| matches!(i, CItem::XMacro(_))) {
        Ok(CItem::XMacro(x)) => x,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_xmacro!: {e}")),
    };
    let name = &x.name;
    // The list: the brace group after NAME(x), past any attributes - its
    // entries' tokens, split at the commas.
    let list = input.into_iter()
        .filter_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g.stream()),
            _ => None,
        })
        .last()
        .expect("parse_items found the list");
    let mut entries: Vec<String> = vec![String::new()];
    for t in list {
        match &t {
            TokenTree::Punct(p) if p.as_char() == ',' => entries.push(String::new()),
            _ => {
                let last = entries.last_mut().unwrap();
                last.push_str(&TokenStream::from(t).to_string());
                last.push(' ');
            }
        }
    }
    entries.retain(|e| !e.trim().is_empty());

    let mut out = record(&text, &[]);
    let subs: Vec<usize> = x.entries.iter().enumerate()
        .filter(|(_, e)| matches!(e, CXEntry::Sub { .. }))
        .map(|(i, _)| i)
        .collect();

    if subs.is_empty() {
        let list = entries.join(", ");
        out.push_str(&format!(
            "#[allow(unused_macros)]\n\
             macro_rules! {name} {{ ($($cb:ident)::+ $([$($pre:tt)*])?) => {{ $($cb)::+! {{ $([$($pre)*])? {list} }} }} }}\n\
             #[allow(unused_imports)]\npub(crate) use {name};\n"));
        return out.parse().unwrap();
    }

    // The plain entries before the first list spliced in, between each and
    // the next, and after the last.
    let lits = |from: usize, to: usize| -> String {
        entries[from..to].iter().map(|e| format!("{e}, ")).collect()
    };
    let path = "crate::cstructs::c";
    let carry = "($($cb)::+) ($($pre)?)";
    for (n, &i) in subs.iter().enumerate() {
        let CXEntry::Sub { map, .. } = &x.entries[i] else { unreachable!() };
        let (pattern, mapped) = match map {
            None => ("($($e:tt)*)".to_string(), "($($e)*)".to_string()),
            Some(m) => {
                let pattern = format!("({})", m.params.iter().map(|p| format!("${p}:tt")).collect::<Vec<_>>().join(", "));
                // The template: params are $p, A##p is pasted.
                let arg = |a: &str| -> String {
                    let parts: Vec<String> = a.split("##").map(|p| {
                        let p = p.trim();
                        if m.params.iter().any(|q| q == p) { format!("${p}") } else { p.to_string() }
                    }).collect();
                    if parts.len() == 1 { parts[0].clone() } else { format!("[<{}>]", parts.join(" ")) }
                };
                (pattern, format!("({})", m.template.split(',').map(arg).collect::<Vec<_>>().join(", ")))
            }
        };
        let after = lits(i + 1, subs.get(n + 1).copied().unwrap_or(entries.len()));
        let acc = format!("$($acc)* $({mapped},)* {after}");
        let next = match subs.get(n + 1) {
            Some(&j) => {
                let CXEntry::Sub { list: next_list, .. } = &x.entries[j] else { unreachable!() };
                format!("{path}::{next_list}!({path}::__{name}_{} [{carry} {acc}]);", n + 1)
            }
            None => format!("$($cb)::+! {{ $($pre)? {acc} }}"),
        };
        out.push_str(&format!(
            "#[allow(unused_macros)]\n\
             macro_rules! __{name}_{n} {{\n\
                 ([($($cb:ident)::+) ($($pre:tt)?) $($acc:tt)*] $({pattern}),* $(,)?) => {{ ::paste::paste! {{ {next} }} }};\n\
             }}\n\
             #[allow(unused_imports)]\npub(crate) use __{name}_{n};\n"));
    }
    let CXEntry::Sub { list: first, .. } = &x.entries[subs[0]] else { unreachable!() };
    out.push_str(&format!(
        "#[allow(unused_macros)]\n\
         macro_rules! {name} {{ ($($cb:ident)::+ $($pre:tt)?) => {{ {path}::{first}!({path}::__{name}_0 [{carry} {}]); }} }}\n\
         #[allow(unused_imports)]\npub(crate) use {name};\n",
        lits(0, subs[0])));
    out.parse().unwrap()
}

/// An enum, and what its values can be (fs/enum_kind.h):
///
///   c_enum! { #[open] pub enum btree_id: u32 { BTREE_ID_extents = 0, ... } }
///
/// Each value is also a constant of the module, of the integer type - C's
/// enum constants are all one namespace, and C refers to them by bare name:
/// in other enums' values, array lengths. The type is built on those:
///
///   #[closed]  a Rust enum
///   #[open]    a newtype over the integer, the values its constants: a value
///              from outside can be anything
///   #[flags]   a bitflags type
///
/// `_` for the name: an anonymous enum, the constants alone. A variant with
/// no value is the one before it plus one, as in C.
#[proc_macro]
pub fn c_enum(input: TokenStream) -> TokenStream {
    let text = format!("c_enum! {{ {input} }}");
    let e = match one_item(&text, "#[open|closed|flags] pub enum NAME: REPR { ... }",
                           |i| matches!(i, CItem::Enum(_))) {
        Ok(CItem::Enum(e)) => e,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_enum!: {e}")),
    };
    let repr = &e.repr;

    // A variant with no value counts from the last that has one - not from
    // the one before it, which would make a const chain as long as the enum:
    // too deep for rustc, at bch_persistent_counters' hundreds.
    let mut out = String::new();
    let mut base: Option<&str> = None;
    let mut n = 0;
    for (v, value) in &e.variants {
        let value = match (value, base) {
            (Some(x), _) => {
                base = Some(v);
                n = 0;
                x.clone()
            }
            (None, Some(b)) => format!("{b} + {n}"),
            (None, None) => format!("{n}"),
        };
        out.push_str(&format!("#[allow(non_upper_case_globals, dead_code)]\npub const {v}: {repr} = {value};\n"));
        n += 1;
    }

    let names: Vec<&str> = e.variants.iter().map(|(v, _)| v.as_str()).collect();
    let ty = match &e.name {
        Some(n) => n.clone(),
        None => repr.clone(),
    };
    if let Some(name) = &e.name {
        match e.kind {
            CEnumKind::Closed => {
                out.push_str(&format!(
                    "#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
                     #[repr({repr})]\npub enum {name} {{\n"));
                for v in &names {
                    out.push_str(&format!("    {v} = {v},\n"));
                }
                out.push_str("}\n");
            }
            CEnumKind::Open => {
                out.push_str(&format!(
                    "#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
                     #[repr(transparent)]\npub struct {name}(pub {repr});\n\
                     #[allow(non_upper_case_globals)]\nimpl {name} {{\n"));
                for v in &names {
                    out.push_str(&format!("    pub const {v}: Self = Self({v});\n"));
                }
                out.push_str("}\n");
            }
            CEnumKind::Flags => {
                out.push_str(&format!(
                    "::bitflags::bitflags! {{\n\
                     #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
                     #[repr(transparent)]\npub struct {name}: {repr} {{\n"));
                for v in &names {
                    out.push_str(&format!("    #[allow(non_upper_case_globals)]\n    const {v} = {v};\n"));
                }
                out.push_str("}\n}\n");
            }
        }
    }

    if let Some(name) = &e.name {
        out.push_str(&typeinfo_size_only(name, "Enum"));
    }

    let mut nums = vec![
        format!("::core::mem::size_of::<{ty}>() as u64"),
        format!("::core::mem::align_of::<{ty}>() as u64"),
    ];
    nums.extend(names.iter().map(|v| format!("{v} as u64")));
    out.push_str(&record(&text, &nums));
    out.parse().unwrap()
}

/// A type alias - C's typedef: c_typedef! { pub type NAME = TYPE; }, with
/// #[c("...")] for the C declarator where TYPE's canonical rendering isn't it.
#[proc_macro]
pub fn c_typedef(input: TokenStream) -> TokenStream {
    let text = format!("c_typedef! {{ {input} }}");
    let t = match one_item(&text, "pub type NAME = TYPE;", |i| matches!(i, CItem::Typedef(_))) {
        Ok(CItem::Typedef(t)) => t,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_typedef!: {e}")),
    };
    format!("#[allow(non_camel_case_types)]\npub type {} = {};\n{}", t.name, t.ty, record(&text, &[]))
        .parse().unwrap()
}

/// A Rust type with the layout of a C type:
///
///   c_same! { struct bkey_s_c == BkeySC<'static> { k, v } }
///
/// Only a record: the generator asserts the C type has the Rust type's size
/// and alignment, and the listed fields its offsets - so Rust can pass one
/// where C has the other.
#[proc_macro]
pub fn c_same(input: TokenStream) -> TokenStream {
    let text = format!("c_same! {{ {input} }}");
    let s = match one_item(&text, "struct NAME == TYPE { fields }", |i| matches!(i, CItem::Same(_))) {
        Ok(CItem::Same(s)) => s,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_same!: {e}")),
    };
    let ty = &s.rust_ty;
    let mut nums = vec![
        format!("::core::mem::size_of::<{ty}>() as u64"),
        format!("::core::mem::align_of::<{ty}>() as u64"),
    ];
    nums.extend(s.fields.iter().map(|f| format!("::core::mem::offset_of!({ty}, {f}) as u64")));
    record(&text, &nums).parse().unwrap()
}

/// What Rust calls of a C header:
///
///   c_extern! {
///       pub fn bch2_trans_begin(trans: *mut c::btree_trans) -> u32;
///       #[c("const struct bch_option bch2_opt_table[]")]
///       pub static bch2_opt_table: [c::bch_option; 0];
///   }
///
/// Rust gets the extern "C" block; C the prototypes, generated into the
/// header the C ones are in - see CExtern.
#[proc_macro]
pub fn c_extern(input: TokenStream) -> TokenStream {
    let text = format!("c_extern! {{ {input} }}");
    let ex = match one_item(&text, "fns and statics", |i| matches!(i, CItem::Extern(_))) {
        Ok(CItem::Extern(ex)) => ex,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_extern!: {e}")),
    };

    let cfg = |c: &Option<cstruct::Cfg>| c.as_ref().map(|c| format!("#[cfg({})] ", c.to_rust())).unwrap_or_default();
    let mut decls = String::new();
    for f in &ex.fns {
        let mut params: Vec<String> = f.params.iter().map(|(n, ty)| format!("{n}: {ty}")).collect();
        if f.variadic {
            params.push("...".into());
        }
        let ret = f.ret.as_ref().map(|r| format!(" -> {r}")).unwrap_or_default();
        decls.push_str(&format!("{}pub fn {}({}){ret};\n", cfg(&f.cfg), f.name, params.join(", ")));
    }
    for s in &ex.statics {
        let m = if s.mutable { "mut " } else { "" };
        decls.push_str(&format!("{}pub static {m}{}: {};\n", cfg(&s.cfg), s.name, s.ty));
    }
    format!("{}unsafe extern \"C\" {{\n{decls}}}\n{}", cfg(&ex.cfg), record(&text, &[]))
        .parse().unwrap()
}

/// rust_c_extern!'s arguments: a C type, and the Rust type the function
/// implementing it takes for it. Each pair has one layout - a pointer and its
/// reference, a #[repr(transparent)] wrapper, a c_same! - so the argument
/// converts by transmute. A C type not here is passed as it is.
const RUST_ARGS: &[(&str, &str)] = &[
    ("*mut c::bch_fs",                  "&crate::util::ffi::Opaque<c::bch_fs>"),
    ("*mut c::btree_trans",             "&crate::util::ffi::Opaque<c::btree_trans>"),
    ("*mut c::printbuf",                "&mut crate::util::printbuf::Printbuf"),
    ("*const c::bkey_validate_context", "&c::bkey_validate_context"),
    ("c::bkey_s_c",                     "crate::btree::bkey::BkeySC<'_>"),
];

/// What C calls of Rust through a pointer - bkey_ops' methods:
///
///   use crate::xattr::bch2_xattr_validate;
///   rust_c_extern! {
///       pub fn bch2_xattr_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c,
///                                  arg3: *const c::bkey_validate_context) -> core::ffi::c_int;
///   }
///
/// C gets the prototypes, as c_extern!'s. Rust gets each function defined
/// with exactly C's signature, calling the Rust function of the same name -
/// `use`d from where it's implemented - with each argument as RUST_ARGS has
/// it. kCFI checks every indirect call against a hash of the callee's type,
/// and a definition in Rust's types (&Opaque<bch_fs>, BkeySC) never hashes
/// as C's prototype does.
#[proc_macro]
pub fn rust_c_extern(input: TokenStream) -> TokenStream {
    let text = format!("rust_c_extern! {{ {input} }}");
    let ex = match one_item(&text, "fns", |i| matches!(i, CItem::Extern(_))) {
        Ok(CItem::Extern(ex)) => ex,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("rust_c_extern!: {e}")),
    };
    if !ex.statics.is_empty() || ex.fns.iter().any(|f| f.variadic) {
        return compile_error("rust_c_extern!: fns only, and not variadic");
    }

    let cfg = |c: &Option<cstruct::Cfg>| c.as_ref().map(|c| format!("#[cfg({})] ", c.to_rust())).unwrap_or_default();
    let mut defs = String::new();
    for f in &ex.fns {
        let params: Vec<String> = f.params.iter().map(|(n, ty)| format!("{n}: {ty}")).collect();
        let args: Vec<String> = f.params.iter()
            .map(|(n, ty)| match RUST_ARGS.iter().find(|(c, _)| c == ty) {
                Some((c, r)) => format!("::core::mem::transmute::<{c}, {r}>({n})"),
                None => n.clone(),
            })
            .collect();
        let ret = f.ret.as_ref().map(|r| format!(" -> {r}")).unwrap_or_default();
        defs.push_str(&format!(
            "{}{}const _: () = {{\n\
                 #[no_mangle]\n\
                 #[allow(unused_unsafe, clippy::transmute_ptr_to_ref)]\n\
                 unsafe extern \"C\" fn {name}({params}){ret} {{\n\
                     unsafe {{ self::{name}({args}) }}\n\
                 }}\n\
             }};\n",
            cfg(&ex.cfg), cfg(&f.cfg),
            name = f.name, params = params.join(", "), args = args.join(", ")));
    }
    format!("{defs}{}", record(&text, &[])).parse().unwrap()
}

/// An integer constant - C's #define: c_const! { pub const NAME: u32 = EXPR; }
/// The #define has the type's C type (`5U`); #[c_int] before it, int's, for
/// a constant C had as a plain number and Rust wants unsigned.
#[proc_macro]
pub fn c_const(input: TokenStream) -> TokenStream {
    let text = format!("c_const! {{ {input} }}");
    let c = match one_item(&text, "pub const NAME: TYPE = EXPR;", |i| matches!(i, CItem::Const(_))) {
        Ok(CItem::Const(c)) => c,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_const!: {e}")),
    };
    format!("#[allow(non_upper_case_globals, dead_code)]\npub const {}: {} = {};\n{}",
            c.name, c.ty, c.value, record(&text, &[format!("{} as u64", c.name)]))
        .parse().unwrap()
}

/// ioctl numbers, C's _IO*() #defines:
///
///   c_ioctl! {
///       BCH_IOCTL_DISK_ADD = _IOW(0xbc, 4, c::bch_ioctl_disk),
///       #[c("const char __user *")]
///       BCHFS_IOC_REINHERIT_ATTRS = _IOR(0xbc, 64, *const core::ffi::c_char),
///   }
///
/// Rust gets a marker type for each, named as the #define, implementing
/// util::ioctl::Ioctl: the opcode, made by util::ioctl's _IO*() from the
/// target's own constants, and the argument type. The record carries the
/// opcodes, and the generated header asserts C's are the same. Userspace
/// only, as util::ioctl is: the kernel build gets the #defines alone.
#[proc_macro]
pub fn c_ioctl(input: TokenStream) -> TokenStream {
    let text = format!("c_ioctl! {{ {input} }}");
    let io = match one_item(&text, "NAME = _IOW(type, nr, ARG), ...", |i| matches!(i, CItem::Ioctl(_))) {
        Ok(CItem::Ioctl(io)) => io,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_ioctl!: {e}")),
    };

    let mut out = String::new();
    let mut opcodes = Vec::new();
    for e in &io.entries {
        let (name, dir, ty, nr) = (&e.name, &e.dir, &e.ty, &e.nr);
        let (arg, opcode) = match &e.arg {
            Some(a) => (a.as_str(), format!("crate::util::ioctl::{dir}::<{a}>({ty}, {nr})")),
            None => ("()", format!("crate::util::ioctl::_IO({ty}, {nr})")),
        };
        out.push_str(&format!(
            "#[cfg(not(kernel))]\n\
             #[allow(non_camel_case_types)]\n\
             pub struct {name};\n\
             #[cfg(not(kernel))]\n\
             impl crate::util::ioctl::Ioctl for {name} {{\n\
                 const OPCODE: u32 = {opcode};\n\
                 type Arg = {arg};\n\
             }}\n"));
        opcodes.push(format!("<{name} as crate::util::ioctl::Ioctl>::OPCODE as u64"));
    }
    format!("{out}#[cfg(not(kernel))]\n{}#[cfg(kernel)]\n{}", record(&text, &opcodes), record(&text, &[]))
        .parse().unwrap()
}

/// Bitfields in a flags word: c_bitmask! { LE64_BITMASK(struct foo, field),
/// strip PREFIX_ { NAME(start, end), ... } }. Rust gets, on foo, name() and
/// set_name() for each - NAME without PREFIX_, lowercased - as C's NAME() and
/// SET_NAME(): the bits [start, end) of field, as a u64.
#[proc_macro]
pub fn c_bitmask(input: TokenStream) -> TokenStream {
    let text = format!("c_bitmask! {{ {input} }}");
    let b = match one_item(&text, "LE64_BITMASK(struct foo, field), strip PREFIX_ { NAME(start, end), ... }",
                           |i| matches!(i, CItem::Bitmask(_))) {
        Ok(CItem::Bitmask(b)) => b,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_bitmask!: {e}")),
    };

    // An LE*_BITMASK field is a zerocopy endian integer; a BITMASK one, native.
    let field = &b.field;
    let (get, set) = if b.macro_name == "BITMASK" {
        (format!("self.{field} as u64"), format!("self.{field} = new as _"))
    } else {
        (format!("u64::from(self.{field}.get())"), format!("self.{field}.set(new as _)"))
    };

    // The runtime table of them (typeinfo.rs's BitmaskField): this struct
    // field's, as a const named for them - bch_sb_flags_0_BITMASKS - which
    // typeinfo.rs's registry must list, so that a new one can't be left out.
    let c_struct = b.c_type.trim_start_matches("struct ").trim();
    let table = format!("{c_struct}_{}_BITMASKS",
                        field.replace(|c: char| !c.is_alphanumeric(), "_").trim_end_matches('_'));
    let mut table_entries = String::new();

    let mut methods = String::new();
    for e in &b.entries {
        let m = b.method(&e.name);
        // set_128_bit_macs: the setter's set_ makes it a name already
        let set_m = match m.strip_prefix('_') {
            Some(s) if s.starts_with(|c: char| c.is_ascii_digit()) => s,
            _ => m.as_str(),
        };
        let (start, end) = (&e.start, &e.end);
        table_entries.push_str(&format!(
            "crate::typeinfo::BitmaskField {{ struct_name: {c_struct:?}, field: {field:?}, name: {set_m:?}, \
             lo: ({start}) as u8, hi: ({end}) as u8 }},\n"));
        // One bit is a flag: bool, as the accessors on bindgen's structs had it.
        let one_bit = matches!((start.parse::<u32>(), end.parse::<u32>()), (Ok(s), Ok(e)) if e == s + 1);
        let (ty, from, to) = if one_bit { ("bool", " != 0", " as u64") } else { ("u64", "", "") };
        methods.push_str(&format!(
            "#[allow(dead_code)]\n\
             pub fn {m}(&self) -> {ty} {{\n\
                 (({get} >> ({start})) & (u64::MAX >> (64 - (({end}) - ({start}))))){from}\n\
             }}\n\
             #[allow(dead_code)]\n\
             pub fn set_{set_m}(&mut self, v: {ty}) {{\n\
                 let mask = (u64::MAX >> (64 - (({end}) - ({start})))) << ({start});\n\
                 let new = ({get} & !mask) | (((v{to}) << ({start})) & mask);\n\
                 {set};\n\
             }}\n"));
    }

    format!("{}impl {} {{\n{methods}}}\n\
             #[allow(non_upper_case_globals)]\n\
             pub const {table}: &[crate::typeinfo::BitmaskField] = &[\n{table_entries}];\n\
             const _: () = assert!(crate::typeinfo::bitmasks_listed({table:?}),\n\
                                   \"{table}: not in typeinfo.rs's BITMASKS - add it\");\n",
            record(&text, &[]), b.rust_type)
        .parse().unwrap()
}
