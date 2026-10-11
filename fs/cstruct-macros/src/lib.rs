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
//!   c_xmacro_from_c!(..) a list C still defines, read from its header: the
//!                        same NAME!(cb), no record (xmacro_from_c.rs)
//!   c_bitmask! { .. }    bitfields in a flags word: Rust gets a getter and
//!                        a setter on the struct for each
//!   c_ioctl! { .. }      ioctl numbers: C gets the _IO*() #defines, Rust a
//!                        marker type for each, its opcode and argument type
//!   c_enum! { .. }       an enum, #[open], #[closed] or #[flags]: Rust gets
//!                        the type that is, and its values as constants
//!   c_const! { .. }      integer constants: C gets a #define each
//!   c_typedef! { .. }    a type alias: C gets a typedef
//!   c_same! { .. }       a Rust type with a C type's layout: checked in C
//!   c_extern! { .. }     what Rust calls of a C header: C gets the
//!                        prototypes, which its own must agree with
//!   #[c_export]          a Rust fn C calls: C gets the prototype; Rust,
//!                        unless it's extern "C", a definition with C's
//!                        signature, calling the fn
//!   tagged_union! { .. } a tagged union with a stable representation: C's
//!                        storage, the tag a Determinant of it, arms from an
//!                        x-macro list, read as an enum (tagged_union.rs)
//!
//! Zero dependencies, as typeinfo-macros: the kernel build compiles this with
//! a bare rustc.

use proc_macro::{Delimiter, Group, Punct, Spacing, Span, TokenStream, TokenTree};

// A proc-macro crate may export only its macros: the parser's pub items go in
// a private module - pub for rust_types_gen, which include!s it too.
#[allow(unreachable_pub)]
mod cstruct {
    include!("../../types/cstruct.rs");
}
use cstruct::{enum_text, expand_enum_list, parse_c_export, parse_cstruct, parse_items, rust_ident,
              CEnumDef, CEnumKind, CItem, CXEntry};

mod tagged_union;
mod xmacro_from_c;

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

/// @decl through @list - LIST!(@callback [decl]) - which calls @callback back
/// with its entries: { [decl] (entry), (entry), ... }. The declaration goes as
/// tokens, not text, so what's defined in it keeps its spans.
fn through_list(list: &str, callback: &str, decl: TokenStream) -> TokenStream {
    let mut cb: TokenStream = callback.parse().unwrap();
    cb.extend([TokenTree::Group(Group::new(Delimiter::Bracket, decl))]);
    let mut out: TokenStream = format!("crate::cstructs::c::{list}!").parse().unwrap();
    out.extend([TokenTree::Group(Group::new(Delimiter::Parenthesis, cb)),
                TokenTree::Punct(Punct::new(';', Spacing::Alone))]);
    out
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
#[proc_macro_derive(CStruct, attributes(c, c_anon, c_bitfield, c_inline, c_packed, c_struct_group, c_typedef))]
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
    // A #[c_bitfield] field's members, on the struct: its type's forwarders
    // (#[bitfield]), by the type's own path.
    let mut forward = String::new();
    for f in def.fields.iter().filter(|f| f.bitfield) {
        let ty = f.ty.trim();
        let (path, ty) = ty.rsplit_once("::").map_or(("", ty), |(p, t)| (p, t));
        let path = if path.is_empty() { String::new() } else { format!("{path}::") };
        forward.push_str(&format!("{path}__{ty}_forward!({name}, {});\n", f.rust_name()));
    }
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
    format!("{forward}{}", record(&text, &nums)).parse().unwrap()
}

/// bitfield_struct::bitfield, recorded - and __NAME_forward!(STRUCT, field),
/// which #[derive(CStruct)] invokes for a #[c_bitfield] field of this type:
/// each member's getter and setter on the struct that holds it, as C has a
/// bitfield's members on its struct. Not `_` members: those are padding.
///
/// Members are written as bilge has them, the type giving the width:
///
///   #[bitfield(u64, ty = u32)]
///   pub struct foo_bits { type_: u2, offset: u40, cached: bool, __pad: u3, .. }
///
/// `name: uN` is N bits, read and written as N rounded up to an integer -
/// offset above is a u64; `name: bool` is one bit. A member of any other type -
/// an enum - gives its width, #[bits(N)], as bitfield-struct has it. Members
/// are pub, as C's are. ty is C's declared type of the integer members, which
/// C lays the bitfield out by - the storage type if not given: the generator's,
/// not passed on.
///
/// With #[derive(CStruct)], the bitfield is itself the C struct - C's
/// `struct bch_extent_ptr { __u64 type:1, ...; }` - and C's attributes go on
/// it as on any: #[c_packed], and `align = 8` for __aligned(8), which a u64
/// bitfield needs said on i386 (Align8U64). It's recorded as the struct and
/// #[c_bitfield] member the generator knows - as `NAME { bits: NAME_bits }`.
#[proc_macro_attribute]
pub fn bitfield(attr: TokenStream, item: TokenStream) -> TokenStream {
    match bitfield_expand(attr, item) {
        Ok(out) => out.parse().unwrap(),
        Err(e) => compile_error(&format!("#[bitfield]: {e}")),
    }
}

fn bitfield_expand(attr: TokenStream, item: TokenStream) -> Result<String, String> {
    let args = bitfield_args(attr)?;
    let s = bitfield_struct(item)?;
    let name = &s.name;
    if args.align8 && !s.c_struct {
        return Err(format!("{name}: align = 8 is a C struct's - #[derive(CStruct)]"));
    }

    // The bitfield's record: a member of a struct in C - when it's the struct
    // itself, of the struct recorded below, as NAME_bits.
    let rec_args = match &args.ty {
        Some(ty) => format!("{}, ty = {ty}", args.storage),
        None     => args.storage.clone(),
    };
    let text = if s.c_struct {
        format!("#[bitfield({rec_args})] pub struct {name}_bits {}", s.body)
    } else {
        format!("#[bitfield({rec_args})] pub struct {name} {}", s.body)
    };
    let CItem::Bitfield(bf) = one_item(&text, "a bitfield struct", |i| matches!(i, CItem::Bitfield(_)))? else {
        unreachable!()
    };
    let mut out = record(&text, &[]);

    if s.c_struct {
        let repr = if args.align8 { "#[repr(C, align(8))]" } else { "#[repr(C)]" };
        let packed = if s.packed { "#[c_packed]" } else { "" };
        let text = format!("{repr} #[derive(CStruct)] {packed} \
                            pub struct {name} {{ #[c_bitfield] pub bits: {name}_bits, }}");
        one_item(&text, "a struct", |i| matches!(i, CItem::Struct(_)))?;
        let size  = format!("::core::mem::size_of::<{name}>() as u64");
        let align = format!("::core::mem::align_of::<{name}>() as u64");
        out.push_str(&record(&text, &[size.clone(), align.clone(), "0".into(), size, align]));
    } else {
        // A member of a C struct: its getters and setters for that struct
        let mut fns = String::new();
        for (m, ty, _) in &bf.fields {
            let m = rust_ident(m);
            if !m.starts_with('_') {
                fns.push_str(&format!(
                    "pub fn {m}(&self) -> {ty} {{ let b = self.$field; b.{m}() }}\n\
                     pub fn set_{m}(&mut self, v: {ty}) {{ let mut b = self.$field; b.set_{m}(v); self.$field = b; }}\n"));
            }
        }
        out.push_str(&format!(
            "#[allow(unused_macros)]\n\
             macro_rules! __{name}_forward {{ ($s:ident, $field:ident) => {{ impl $s {{\n{fns}}} }} }}\n\
             #[allow(unused_imports)]\npub(crate) use __{name}_forward;\n"));
    }

    let mut bs_args = vec![args.storage.clone()];
    bs_args.extend(args.rest.iter().map(|a| a.iter().cloned().collect::<TokenStream>().to_string()));
    if args.align8 {
        bs_args.extend(["repr = crate::types::Align8U64", "from = crate::types::Align8U64::from_u64",
                        "into = crate::types::Align8U64::to_u64"].map(String::from));
    }
    out.push_str(&format!("#[::bitfield_struct::bitfield({})] {}\n{}",
                          bs_args.join(", "), s.item, typeinfo_size_only(name, "Struct")));
    Ok(out)
}

/// @ts's top-level comma separated items - commas in a type's <..> aren't -
/// less empty ones: a trailing comma's.
fn comma_list(ts: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut items = vec![Vec::new()];
    let mut depth = 0;
    for t in ts {
        match &t {
            TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '>' => depth -= 1,
            TokenTree::Punct(p) if p.as_char() == ',' && depth == 0 => {
                items.push(Vec::new());
                continue;
            }
            _ => {}
        }
        items.last_mut().unwrap().push(t);
    }
    items.retain(|i| !i.is_empty());
    items
}

/// `KEY = VALUE`: KEY, and VALUE as text.
fn key_value(toks: &[TokenTree]) -> Option<(String, String)> {
    match toks {
        [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' && !v.is_empty() =>
            Some((k.to_string(), v.iter().cloned().collect::<TokenStream>().to_string())),
        _ => None,
    }
}

/// @ts, every token at @span: for what a macro makes in place of what's
/// written, so an error in it points there.
fn respan(ts: TokenStream, span: Span) -> TokenStream {
    ts.into_iter().map(|t| match t {
        TokenTree::Group(g) => {
            let mut n = Group::new(g.delimiter(), respan(g.stream(), span));
            n.set_span(span);
            n.into()
        }
        mut t => { t.set_span(span); t }
    }).collect()
}

/// #[bitfield]'s arguments: bitfield-struct's, its storage first, and ours.
struct BitfieldArgs {
    storage: String,
    /// The rest of bitfield-struct's
    rest:    Vec<Vec<TokenTree>>,
    /// `ty = T`: C's declared type of the integer members
    ty:      Option<String>,
    /// `align = 8`: C's __aligned(8), on a u64 bitfield that's a C struct
    align8:  bool,
}

fn bitfield_args(attr: TokenStream) -> Result<BitfieldArgs, String> {
    let mut rest = comma_list(attr);
    let storage = match rest.first().map(Vec::as_slice) {
        Some([TokenTree::Ident(s)]) => s.to_string(),
        _ => return Err("expected the storage type first: #[bitfield(uN, ..)]".into()),
    };
    rest.remove(0);
    let mut ty = None;
    let mut align = None;
    rest.retain(|a| match key_value(a) {
        Some((k, v)) if k == "ty"    => { ty = Some(v); false }
        Some((k, v)) if k == "align" => { align = Some(v); false }
        _ => true,
    });
    let align8 = match align.as_deref() {
        None => false,
        Some("8") if storage == "u64" => true,
        Some(n) => return Err(format!("align = {n}: there's only align = 8, on u64 storage")),
    };
    Ok(BitfieldArgs { storage, rest, ty, align8 })
}

/// A #[bitfield] struct, its attributes that are ours taken out: CStruct and
/// TypeInfo from its derive - C's struct, and we give the TypeInfo - and
/// #[c_packed].
struct BitfieldStruct {
    name:     String,
    /// The struct, for bitfield-struct: its members canonical
    item:     TokenStream,
    /// Its body, those members - for the record
    body:     TokenStream,
    /// #[derive(CStruct)]: the bitfield is C's struct, not a member of one
    c_struct: bool,
    /// #[c_packed]: C's is __packed
    packed:   bool,
}

fn bitfield_struct(item: TokenStream) -> Result<BitfieldStruct, String> {
    let mut s = BitfieldStruct { name: String::new(), item: TokenStream::new(),
                                 body: TokenStream::new(), c_struct: false, packed: false };
    let mut toks = item.into_iter().peekable();
    while let Some(t) = toks.next() {
        match t {
            TokenTree::Punct(p) if p.as_char() == '#' => {
                let Some(TokenTree::Group(attr)) = toks.next() else {
                    return Err("expected an attribute after #".into());
                };
                if let Some(attr) = attr_less_ours(attr, &mut s) {
                    s.item.extend([TokenTree::Punct(p), attr.into()]);
                }
            }
            TokenTree::Ident(i) if i.to_string() == "struct" => {
                if let Some(TokenTree::Ident(n)) = toks.peek() {
                    s.name = n.to_string();
                }
                s.item.extend([TokenTree::Ident(i)]);
            }
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                let mut body = Group::new(Delimiter::Brace, bitfield_members(g.stream())?);
                body.set_span(g.span());
                s.body.extend([TokenTree::Group(body.clone())]);
                s.item.extend([TokenTree::Group(body)]);
            }
            t => s.item.extend([t]),
        }
    }
    if s.name.is_empty() {
        return Err("expected a struct".into());
    }
    if s.packed && !s.c_struct {
        return Err(format!("{}: #[c_packed] is a C struct's - #[derive(CStruct)]", s.name));
    }
    Ok(s)
}

/// @attr, one #[..]'s brackets, less what's ours - see BitfieldStruct - or
/// None if nothing's left.
fn attr_less_ours(attr: Group, s: &mut BitfieldStruct) -> Option<Group> {
    let inner: Vec<TokenTree> = attr.stream().into_iter().collect();
    match inner.as_slice() {
        [TokenTree::Ident(a)] if a.to_string() == "c_packed" => {
            s.packed = true;
            None
        }
        [TokenTree::Ident(a), TokenTree::Group(list)] if a.to_string() == "derive" => {
            let kept: Vec<Vec<TokenTree>> = comma_list(list.stream()).into_iter()
                .filter(|path| match path.last().map(|t| t.to_string()).as_deref() {
                    Some("CStruct") => { s.c_struct = true; false }
                    Some("TypeInfo") => false,
                    _ => true,
                })
                .collect();
            if kept.is_empty() {
                return None;
            }
            let list = kept.into_iter().map(|p| p.into_iter().collect::<TokenStream>().to_string())
                .collect::<Vec<_>>().join(", ");
            Some(Group::new(Delimiter::Bracket, respan(format!("derive({list})").parse().unwrap(), a.span())))
        }
        _ => Some(attr),
    }
}

/// A bitfield struct's members, as bitfield-struct has them - see bitfield().
fn bitfield_members(body: TokenStream) -> Result<TokenStream, String> {
    let mut out = TokenStream::new();
    for m in comma_list(body) {
        out.extend(bitfield_member(&m)?);
        out.extend([TokenTree::Punct(Punct::new(',', Spacing::Alone))]);
    }
    Ok(out)
}

/// One member: `name: uN` and `name: bool` with their #[bits(N)] and Rust
/// type, at the span of the type they replace, so an error points there; one
/// with #[bits(..)] as written. pub either way.
fn bitfield_member(m: &[TokenTree]) -> Result<TokenStream, String> {
    let mut attrs = 0;
    let mut has_bits = false;
    while let [TokenTree::Punct(p), TokenTree::Group(g), ..] = &m[attrs..] {
        if p.as_char() != '#' || g.delimiter() != Delimiter::Bracket {
            break;
        }
        has_bits |= g.stream().into_iter().next().is_some_and(|a| a.to_string() == "bits");
        attrs += 2;
    }
    // pub, pub(..): members are pub, whatever's written
    let mut rest = &m[attrs..];
    if matches!(rest.first(), Some(TokenTree::Ident(v)) if v.to_string() == "pub") {
        rest = &rest[1..];
        if matches!(rest.first(), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis) {
            rest = &rest[1..];
        }
    }
    let [TokenTree::Ident(name), TokenTree::Punct(colon), ty @ ..] = rest else {
        return Err(format!("expected a member, `name: type`: `{}`", m.iter().cloned().collect::<TokenStream>()));
    };
    if colon.as_char() != ':' || ty.is_empty() {
        return Err(format!("{name}: expected `name: type`"));
    }

    let mut out: TokenStream = m[..attrs].iter().cloned().collect();
    let ty: TokenStream = if has_bits {
        ty.iter().cloned().collect()
    } else {
        let [TokenTree::Ident(t)] = ty else {
            return Err(format!("{name}: a member without #[bits(N)] is uN or bool"));
        };
        let (bits, rust_ty) = bilge_width(&t.to_string())
            .ok_or_else(|| format!("{name}: {t}: a member without #[bits(N)] is uN or bool"))?;
        out.extend(respan(format!("#[bits({bits})]").parse().unwrap(), t.span()));
        respan(rust_ty.parse().unwrap(), t.span())
    };
    out.extend(respan("pub".parse().unwrap(), name.span()));
    out.extend([TokenTree::Ident(name.clone()), TokenTree::Punct(colon.clone())]);
    out.extend(ty);
    Ok(out)
}

/// A bilge member type's width and the Rust type it's read as: bool, and uN
/// for N bits as N rounded up to an integer.
fn bilge_width(t: &str) -> Option<(u32, String)> {
    if t == "bool" {
        return Some((1, t.to_string()));
    }
    let n: u32 = t.strip_prefix('u')?.parse().ok().filter(|n| (1..=128).contains(n))?;
    let int = [8, 16, 32, 64, 128].into_iter().find(|w| *w >= n)?;
    Some((n, format!("u{int}")))
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
    xmacro(input, true)
}

/// An x-macro list C defines, for Rust until its header is converted:
/// c_xmacro_from_c!(NAME, "header") is c_xmacro! of the header's
/// #define NAME(), unrecorded - see xmacro_from_c.rs.
#[proc_macro]
pub fn c_xmacro_from_c(input: TokenStream) -> TokenStream {
    let (list, path) = match xmacro_from_c::read(input) {
        Ok(r) => r,
        Err(e) => return compile_error(&format!("c_xmacro_from_c!: {e}")),
    };
    let Ok(list) = list.parse::<TokenStream>() else {
        return compile_error(&format!("c_xmacro_from_c!: {list}: not Rust tokens"));
    };
    let mut out: TokenStream = format!("const _: &[u8] = include_bytes!({path:?});").parse().unwrap();
    out.extend(xmacro(list, false));
    out
}

/// c_xmacro!, its record only if @recorded: a list C defines has none.
fn xmacro(input: TokenStream, recorded: bool) -> TokenStream {
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

    let mut out = if recorded { record(&text, &[]) } else { String::new() };
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
    // The callback's [pre] bracketed, as the plain list's: an optional single
    // tt after a path callback is ambiguous with its `::`.
    let CXEntry::Sub { list: first, .. } = &x.entries[subs[0]] else { unreachable!() };
    out.push_str(&format!(
        "#[allow(unused_macros)]\n\
         macro_rules! {name} {{ ($($cb:ident)::+ $([$($pre:tt)*])?) => {{ {path}::{first}!({path}::__{name}_0 [($($cb)::+) ($([$($pre)*])?) {}]); }} }}\n\
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
/// Any of them can be a #[bitfield]'s member, as C's `enum btree_id
/// btree_id:8`: it has the const into_bits() bitfield-struct converts with,
/// and from_bits() - for #[flags], bitflags' from_bits() is the checked one,
/// so a member names `from = T::from_bits_retain`. A #[closed] enum's member
/// only ever holds one of its values, as its type says; from_bits() of
/// anything else is memory corruption, and panics.
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
    match e.from.first() {
        Some((_, f)) => through_list(&f.list, "cstruct_macros::__c_enum", input),
        None => c_enum_gen(&e, &text),
    }
}

/// c_enum!'s list's callback: { [decl] (entry), (entry), ... } - the enum
/// with its first list's variants in their place: through the next list, if
/// it has another, or recorded so.
#[doc(hidden)]
#[proc_macro]
pub fn __c_enum(input: TokenStream) -> TokenStream {
    let mut trees = input.into_iter();
    let decl = match trees.next() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket => g.stream(),
        _ => return compile_error("__c_enum!: expected [declaration] then the list's entries"),
    };
    let entries: Vec<String> = trees
        .filter_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream().to_string()),
            _ => None,
        })
        .collect();
    let mut e = match one_item(&format!("c_enum! {{ {decl} }}"), "#[open|closed|flags] pub enum NAME: REPR { ... }",
                               |i| matches!(i, CItem::Enum(_))) {
        Ok(CItem::Enum(e)) => e,
        Ok(_) => unreachable!(),
        Err(e) => return compile_error(&format!("c_enum!: {e}")),
    };
    if let Err(err) = expand_enum_list(&mut e, &entries) {
        return compile_error(&format!("c_enum! {}: {err}", e.name.as_deref().unwrap_or("_")));
    }
    let decl = enum_text(&e);
    match e.from.first() {
        Some((_, f)) => through_list(&f.list, "cstruct_macros::__c_enum", decl.parse().unwrap()),
        None => c_enum_gen(&e, &format!("c_enum! {{ {decl} }}")),
    }
}

/// c_enum! @e's Rust, and its record, of @text.
fn c_enum_gen(e: &CEnumDef, text: &str) -> TokenStream {
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
                out.push_str(&format!("}}\nimpl {name} {{\n    pub const fn from_bits(v: {repr}) -> Self {{\n        match v {{\n"));
                for v in &names {
                    out.push_str(&format!("            x if x == Self::{v} as {repr} => Self::{v},\n"));
                }
                out.push_str(&format!(
                    "            _ => panic!(\"{name}::from_bits(): not a {name}\"),\n        }}\n    }}\n\
                     pub const fn into_bits(self) -> {repr} {{ self as {repr} }}\n}}\n"));
            }
            CEnumKind::Open => {
                out.push_str(&format!(
                    "#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
                     #[repr(transparent)]\npub struct {name}(pub {repr});\n\
                     #[allow(non_upper_case_globals)]\nimpl {name} {{\n"));
                for v in &names {
                    out.push_str(&format!("    pub const {v}: Self = Self({v});\n"));
                }
                out.push_str(&format!("    pub const fn from_bits(v: {repr}) -> Self {{ Self(v) }}\n"));
                out.push_str(&format!("    pub const fn into_bits(self) -> {repr} {{ self.0 }}\n}}\n"));
            }
            CEnumKind::Flags => {
                out.push_str(&format!(
                    "::bitflags::bitflags! {{\n\
                     #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
                     #[repr(transparent)]\npub struct {name}: {repr} {{\n"));
                for v in &names {
                    out.push_str(&format!("    #[allow(non_upper_case_globals)]\n    const {v} = {v};\n"));
                }
                out.push_str(&format!(
                    "}}\n}}\n\
                     impl {name} {{\n    pub const fn into_bits(self) -> {repr} {{ self.bits() }}\n}}\n"));
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
    out.push_str(&record(text, &nums));
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
///       pub static bch2_opt_table: [c::bch_option; 0];
///   }
///
/// Rust gets the extern "C" block; C the prototypes, generated into the
/// header the C ones are in - see CExtern. A static that isn't mut is C's
/// const, and [T; 0] its unsized array: `extern const struct bch_option
/// bch2_opt_table[];`.
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
        let must_use = if f.must_use { "#[must_use] " } else { "" };
        let cold = if f.cold { "#[cold] " } else { "" };
        decls.push_str(&format!("{}{must_use}{cold}pub fn {}({}){ret};\n", cfg(&f.cfg), f.name,
                                params.join(", ")));
    }
    for s in &ex.statics {
        let m = if s.mutable { "mut " } else { "" };
        decls.push_str(&format!("{}pub static {m}{}: {};\n", cfg(&s.cfg), s.name, s.ty));
    }
    format!("{}unsafe extern \"C\" {{\n{decls}}}\n{}", cfg(&ex.cfg), record(&text, &[]))
        .parse().unwrap()
}

/// A Rust fn C calls:
///
///   #[c_export]
///   pub fn bch2_xattr_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>,
///                              from: &c::bkey_validate_context) -> c_int { .. }
///
/// C gets the prototype, as c_extern!'s, from the fn's signature - in
/// foo/bar_gen.h, for a fn in foo/bar.rs - each Rust type as C's
/// (cstruct.rs, c_export_type()): &Opaque<bch_fs> is struct bch_fs *,
/// BkeySC struct bkey_s_c. An extern "C" fn is the symbol C calls, made
/// #[no_mangle]. Any other is called through a definition with exactly C's
/// signature, made here, which transmutes what isn't C's type: kCFI checks
/// every call through a pointer against a hash of the callee's type, and a fn
/// in Rust's types (&Opaque<bch_fs>, BkeySC) never hashes as C's prototype
/// does.
#[proc_macro_attribute]
pub fn c_export(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return compile_error("#[c_export] takes no arguments");
    }
    // What the record has: the signature, the body dropped - and $crate,
    // from a macro_rules!, as the crate this is.
    let mut sig: Vec<TokenTree> = item.clone().into_iter().collect();
    if !matches!(sig.pop(), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace) {
        return compile_error("#[c_export]: on a fn");
    }
    let sig: TokenStream = sig.into_iter().collect();
    let text = format!("#[c_export] {sig} {{}}").replace("$crate", "crate");

    let e = match parse_c_export(&text) {
        Ok(e) => e,
        Err(e) => return compile_error(&format!("#[c_export]: {e}")),
    };
    let f = &e.ex.fns[0];
    let cfg = f.cfg.as_ref().map(|c| format!("#[cfg({})] ", c.to_rust())).unwrap_or_default();
    let record = format!("{cfg}{}", record(&text, &[]));

    // extern "C": the fn is C's symbol itself
    if e.extern_c {
        let mut out: TokenStream = "#[no_mangle]".parse().unwrap();
        out.extend(item);
        out.extend(record.parse::<TokenStream>().unwrap());
        return out;
    }

    let params: Vec<String> = f.params.iter().map(|(n, ty)| format!("{n}: {ty}")).collect();
    let args: Vec<String> = f.params.iter().zip(&e.params)
        .map(|((n, c), r)| match r {
            Some(r) => format!("::core::mem::transmute::<{c}, {r}>({n})"),
            None    => n.clone(),
        })
        .collect();
    let call = format!("self::{}({})", f.name, args.join(", "));
    let (ret, call) = match (&f.ret, &e.ret) {
        (Some(c), Some(r)) => (format!(" -> {c}"), format!("::core::mem::transmute::<{r}, {c}>({call})")),
        (Some(c), None)    => (format!(" -> {c}"), call),
        (None, _)          => (String::new(), call),
    };
    let def = format!(
        "{cfg}const _: () = {{\n\
             #[allow(unused_imports)]\n\
             use crate::c;\n\
             #[no_mangle]\n\
             #[allow(unused_unsafe, clippy::transmute_ptr_to_ref)]\n\
             unsafe extern \"C\" fn {name}({params}){ret} {{\n\
                 unsafe {{ {call} }}\n\
             }}\n\
         }};\n\
         {record}",
        name = f.name, params = params.join(", "));

    let mut out = item;
    out.extend(def.parse::<TokenStream>().unwrap());
    out
}

/// Integer constants - C's #defines: c_const! { pub const NAME: u32 = EXPR; ... }
/// Each #define has its type's C type (`5U`); #[c_int] before one, int's, for
/// a constant C had as a plain number and Rust wants unsigned. The record is
/// the block's, with each constant's value, in order.
#[proc_macro]
pub fn c_const(input: TokenStream) -> TokenStream {
    let text = format!("c_const! {{ {input} }}");
    let consts = match parse_items(&text) {
        Ok(items) if !items.is_empty() && items.iter().all(|i| matches!(i, CItem::Const(_))) => items,
        Ok(_) => return compile_error("c_const!: expected pub const NAME: TYPE = EXPR;"),
        Err(e) => return compile_error(&format!("c_const!: {e}")),
    };
    let mut out = String::new();
    let mut nums = Vec::new();
    for c in &consts {
        let CItem::Const(c) = c else { unreachable!() };
        out.push_str(&format!("#[allow(non_upper_case_globals, dead_code)]\npub const {}: {} = {};\n",
                              c.name, c.ty, c.value));
        nums.push(format!("{} as u64", c.name));
    }
    format!("{out}{}", record(&text, &nums)).parse().unwrap()
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
