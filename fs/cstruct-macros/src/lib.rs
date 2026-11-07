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
//!   xmacro_select!       NAME!(xmacro_select [TOKEN => cb]): cb! handed
//!                        the rows with TOKEN in them - a flag in a flags
//!                        column
//!   c_bitmask! { .. }    bitfields in a flags word: Rust gets a getter and
//!                        a setter on the struct for each
//!   c_ioctl! { .. }      ioctl numbers: C gets the _IO*() #defines, Rust a
//!                        marker type for each, its opcode and argument type
//!   c_enum! { .. }       an enum, #[open], #[closed] or #[flags]: Rust gets
//!                        the type that is, and its values as constants
//!   atomic_bitfield! { .. }
//!                        a bitfield's value, and an atomic word of it; no C
//!   #[open_enum]         an open enum Rust alone has: c_enum!'s #[open],
//!                        without the C
//!   c_const! { .. }      integer constants: C gets a #define each
//!   c_typedef! { .. }    a type alias: C gets a typedef
//!   c_rcu_array! { .. }  an RcuArray C reads: C gets the struct of its
//!                        buffer, a field of it a __rcu pointer to one
//!   c_same! { .. }       a Rust type with a C type's layout: checked in C
//!   c_extern! { .. }     what Rust calls of a C header: C gets the
//!                        prototypes, which its own must agree with
//!   #[c_export]          a Rust fn C calls: C gets the prototype; Rust,
//!                        unless it's extern "C", a definition with C's
//!                        signature, calling the fn
//!   tagged_union! { .. } a tagged union with a stable representation: C's
//!                        storage, the tag a Determinant of it, declared as
//!                        an enum, the arms its variants (tagged_union.rs)
//!   #[derive(TypeInfo)]  runtime type information, for kvdb: no record, no
//!                        C (typeinfo.rs)
//!   #[derive(Plain)]     util/vstructs.rs's Plain, checked at compile time;
//!                        a #[bitfield] is Plain too, and #[counted_by] on a
//!                        CStruct's trailing [T; 0] is its FlexArray
//!                        (vstructs.rs)
//!   #[derive(View)]      a projection for each field on a View of the type:
//!                        its fields read as zero past a short value's end
//!                        (vstructs.rs)
//!   #[unsized_vstruct]   on C's struct ending in a flexible array: a family
//!                        of definitions, one per state, and X<S> picking
//!                        one (vstructs.rs)
//!   #[grown(from = F)]   on C's struct of a type that grew: its family - the
//!                        whole struct, and the floor and the bytes there
//!                        are (vstructs.rs)
//!   __prt!(..)           message!()'s and prt!()'s format strings, written
//!                        into a printbuf - a ToText argument to_text()'d
//!                        into it; nothing C sees (prt.rs)
//!   #[args_stored_once]  an async fn as fn -> impl Future { async move }, so
//!                        its by-value arguments are stored once in its
//!                        future, not twice (args_stored_once.rs)
//!
//! On syn and quote - vendor/kernel-rust's: the kernel build compiles this
//! with a bare rustc.

use proc_macro2::{Delimiter, Group, Ident, Literal, Spacing, Span, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens};

// A proc-macro crate may export only its macros: the parser's pub items go in
// a private module - pub for rust_types_gen, which include!s it too.
#[allow(unreachable_pub)]
mod cstruct {
    include!("../../types/cstruct.rs");
}
use cstruct::{bitfield_type, bitfield_wrapper, enum_text, expand_enum_list, parse_c_export, parse_cstruct, parse_items, rust_ident,
              spell_concat, CEnumDef, CEnumKind, CEnumVariant, CItem, CXEntry};

mod args_stored_once;
mod atomic_bitfield;
mod detach;
mod rcu_head;
mod fieldwise;
mod options;
mod init;
mod late;
mod place;
mod prt;
mod enum_decl;
mod tagged_union;
mod to_text;
mod typeinfo;
mod vstructs;

/// A tagged union with a stable representation: see tagged_union.rs.
#[proc_macro]
pub fn tagged_union(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("tagged_union!", || tagged_union::tagged_union(input.into()))
}

/// message!()'s format string written into a printbuf, ToText arguments
/// to_text()'d into it: see prt.rs.
#[doc(hidden)]
#[proc_macro]
pub fn __prt(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("message!", || prt::prt(syn::parse(input)?))
}

fn compile_error(msg: &str) -> TokenStream {
    quote!(::core::compile_error!(#msg);)
}

/// A macro's expansion, @f, or its error as compile_error!, @what naming the
/// macro: every macro here is expand("name!", || ..), or expand_syn().
fn expand(what: &str, f: impl FnOnce() -> Result<TokenStream, String>) -> proc_macro::TokenStream {
    f().unwrap_or_else(|e| compile_error(&format!("{what}: {e}"))).into()
}

/// expand() of a macro on syn: its error at the tokens it's about.
fn expand_syn(what: &str, f: impl FnOnce() -> syn::Result<TokenStream>) -> proc_macro::TokenStream {
    f().unwrap_or_else(|e| syn::Error::new(e.span(), format!("{what}: {e}")).into_compile_error()).into()
}

/// Rust code a macro made, as tokens: it always parses, or the macro has a bug.
fn tokens(code: &str) -> TokenStream {
    code.parse().unwrap()
}

/// A record's first numbers, as every type's has them: @ty's size and
/// alignment.
fn size_align(ty: &impl ToTokens) -> Vec<TokenStream> {
    vec![quote!(::core::mem::size_of::<#ty>() as u64), quote!(::core::mem::align_of::<#ty>() as u64)]
}

/// The #[cfg(..)] on an item, if it has one.
fn cfg_attr(cfg: &Option<cstruct::Cfg>) -> TokenStream {
    cfg.as_ref().map(|c| tokens(&c.to_rust())).map(|c| quote!(#[cfg(#c)])).unwrap_or_default()
}

/// For a type C knows as @c_name: kCFI hashes an extern "C" fn's repr(C)
/// struct by its Rust name, and an enum or a transparent newtype as its
/// integer - neither what C's prototype says. In the kernel only, where the
/// feature's allowed (mod.rs).
fn cfi_encoding(c_name: &str) -> TokenStream {
    let enc = format!("{}{c_name}", c_name.len());
    quote!(#[cfg_attr(kernel, cfi_encoding = #enc)])
}

/// The record of @text, and @nums: Rust expressions, the numbers that go
/// with it - see fs/types/lib.rs.
fn record(text: &str, nums: &[TokenStream]) -> TokenStream {
    let names = cstruct::written_types(text).into_iter().map(|t| {
        let ty = tokens(&t);
        quote!((#t, ::core::any::type_name::<#ty>()))
    });
    // the count as a literal, not NUMS.len(): RECORD's type mustn't need the
    // numbers - a field's layout - or a field of an opaque type is a cycle
    let n = nums.len();
    quote! {
        #[cfg(bch_cstruct_records)]
        const _: () = {
            const TEXT: &str = #text;
            const NUMS: &[u64; #n] = &[#(#nums),*];
            const NAMES: &[(&str, &str)] = &[#(#names),*];
            const LEN: usize = crate::types::record_len(::core::file!(), TEXT, #n, NAMES);
            #[used]
            #[unsafe(link_section = ".discard.bch_cstruct")]
            static RECORD: [u8; LEN] =
                crate::types::record::<LEN>(::core::file!(), ::core::line!(), TEXT, NUMS, NAMES);
        };
    }
}

/// @decl through @list - LIST!(@callback [decl]) - which calls @callback back
/// with its entries: { [decl] (entry), (entry), ... }. The declaration goes as
/// tokens, not text, so what's defined in it keeps its spans.
fn through_list(list: &str, callback: &str, decl: TokenStream) -> TokenStream {
    let (list, cb) = (format_ident!("{list}"), tokens(callback));
    quote!(crate::cstructs::c::#list!(#cb [#decl]);)
}

/// What a list hands through_list()'s callback - { [decl] (entry), (entry),
/// ... }: the declaration, and each entry as text.
fn list_callback(input: TokenStream) -> Result<(TokenStream, Vec<String>), String> {
    let mut trees = input.into_iter();
    let decl = match trees.next() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket => g.stream(),
        _ => return Err("expected [declaration] then the list's entries".into()),
    };
    let entries = trees
        .filter_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream().to_string()),
            _ => None,
        })
        .collect();
    Ok((decl, entries))
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

/// one_item() of kind CItem::$kind, as what it holds - or return the error.
macro_rules! one_item {
    ($text:expr, $kind:ident, $what:expr) => {
        match one_item($text, $what, |i| matches!(i, CItem::$kind(_)))? {
            CItem::$kind(x) => x,
            _ => unreachable!(),
        }
    };
}

/// `#[c("...")]` is the field's C declaration, name included; `#[c_anon("...")]`
/// a C declaration that doesn't name it. A bare `#[c_anon]` is the field's
/// type written in its place, unnamed - C's anonymous struct or union - and
/// `#[c_inline]` the same, named; `#[c_struct_group]` as the kernel's
/// struct_group(); `#[c_lock("u8 lock")]` on a Locked<R, G, P>, the lock's C
/// declaration, G's members and P's after it. `#[c_bitfield]` marks a field of a
/// #[bitfield] type, whose members C declares in its place. `#[counted_by(FIELD)]` on a trailing
/// `[T; 0]`: FIELD is its length - the struct's FlexArray (vstructs.rs).
/// `#[c_typedef]` on the struct: C has it as a typedef - `typedef struct {
/// ... } name`, or with `#[c_typedef(NAME)]`, `typedef struct name { ... }
/// NAME`. `#[c_name_of(NAME)]`: C's name for it, where Rust's differs -
/// what #[c_name] leaves for the derives.
/// bch_opts' options, a field each: options.rs.
#[proc_macro_derive(Options, attributes(opt))]
pub fn derive_options(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Options)]", || options::derive(syn::parse(input)?))
}

/// `#[derive(CStruct)]`: a #[repr(C)] struct or union's record, for C's
/// declaration of it; no Rust side.
#[proc_macro_derive(CStruct, attributes(c, c_anon, c_bitfield, c_inline, c_lock, c_name_of, c_packed,
                                        c_struct_group, c_typedef, counted_by))]
pub fn derive_cstruct(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(CStruct)]", || cstruct_expand(input.into()))
}

/// `#[c_name(NAME)]`, before the derives, on a struct or union C shares
/// whose Rust name isn't C's: NAME is C's. The derives get it as
/// #[c_name_of(NAME)] - after the item's other attributes, as a derive's
/// helper comes after the derive - and kCFI its encoding (cfi_encoding()),
/// so a prototype naming it hashes as C's does. A family's C definition gets
/// one from vstructs.rs.
#[proc_macro_attribute]
pub fn c_name(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[c_name]", || {
        let name: syn::Ident = syn::parse(attr)
            .map_err(|e| syn::Error::new(e.span(), "takes C's name for the type"))?;
        let mut item: syn::DeriveInput = syn::parse(item)?;
        item.attrs.push(syn::parse_quote!(#[c_name_of(#name)]));
        let cfi = cfi_encoding(&name.to_string());
        Ok(quote!(#cfi #item))
    })
}

/// `#[derive(TypeInfo)]`: runtime type information for a struct or union C
/// shares - see typeinfo.rs.
#[proc_macro_derive(TypeInfo, attributes(c_name_of))]
pub fn derive_type_info(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(TypeInfo)]", || typeinfo::derive(input.into()))
}

/// `#[derive(Init)]`: `T::init()`, an initializer that zeroes every field but
/// those with `#[init(EXPR)]` - see init.rs.
#[proc_macro_derive(Init, attributes(init, init_args, placed_at))]
pub fn derive_init(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Init)]", || init::derive(syn::parse(input)?))
}

/// `#[derive(Late)]`: `init_late()` and `exit_late()`, for the fields with
/// `#[late]` - see late.rs.
#[proc_macro_derive(Late, attributes(late))]
pub fn derive_late(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Late)]", || late::derive(syn::parse(input)?))
}

/// `#[derive(Detach)]`: util/rcu.rs's Detach, of every field - see detach.rs.
#[proc_macro_derive(Detach)]
pub fn derive_detach(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Detach)]", || detach::derive(syn::parse(input)?))
}

/// `#[derive(RcuHead)]`: util/rcu.rs's HasRcuHead, at the field marked
/// #[rcu_head] - see rcu_head.rs.
#[proc_macro_derive(RcuHead, attributes(rcu_head))]
pub fn derive_rcu_head(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(RcuHead)]", || rcu_head::derive(syn::parse(input)?))
}

/// `#[derive(Fieldwise)]`: each method `#[fieldwise(...)]` names, applied to
/// every field - see fieldwise.rs.
#[proc_macro_derive(Fieldwise, attributes(fieldwise))]
pub fn derive_fieldwise(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Fieldwise)]", || fieldwise::derive(syn::parse(input)?))
}

/// `#[derive(ToText)]`: a fieldless enum printed as its variant's name, or
/// its `#[label("..")]` - see to_text.rs.
#[proc_macro_derive(ToText, attributes(label))]
pub fn derive_to_text(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(ToText)]", || to_text::derive(syn::parse(input)?))
}

/// `#[derive(Plain)]`: util/vstructs.rs's Plain, checked at compile time -
/// see vstructs.rs.
#[proc_macro_derive(Plain)]
pub fn derive_plain(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(Plain)]", || vstructs::derive_plain(input.into()))
}

/// `#[derive(SizedVal)]`: a key value that's itself in place, any bytes of it
/// a valid one - btree/bkey/types.rs's SizedVal, what a sized key type's
/// value is.
#[proc_macro_derive(SizedVal)]
pub fn derive_sized_val(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(SizedVal)]", || vstructs::derive_sized_val(input.into()))
}

/// `#[derive(View)]`: a projection for each field, on util/vstructs.rs's
/// View and ViewMut of the type - see vstructs.rs.
#[proc_macro_derive(View)]
pub fn derive_view(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(View)]", || vstructs::derive_view(input.into()))
}

fn cstruct_expand(input: TokenStream) -> Result<TokenStream, String> {
    let text = format!("#[derive(CStruct)] {input}");
    let def = parse_cstruct(&text)?;
    if !def.repr_c() {
        return Err(format!("{} must be #[repr(C)]: C shares it", def.name));
    }

    let name = def.rust_ident();
    let inst = tokens(&def.rust_type());
    // A #[c_bitfield] field's members, on the struct: its type's forwarders
    // (#[bitfield]), by the type's own path - a Cell's setting through &self.
    // Not a Volatile one's: that's a word, read and written whole.
    let mut forward = TokenStream::new();
    let (generics, over) = def.impl_generics();
    let (generics, over) = (tokens(&generics), tokens(&over));
    for f in def.fields.iter().filter(|f| f.bitfield) {
        let bt = bitfield_type(&f.ty);
        let suffix = match bitfield_wrapper(&f.ty) {
            None         => "_forward",
            Some("Cell") => "_forward_cell",
            Some(_)      => continue,
        };
        let (path, ty) = bt.rsplit_once("::").map_or(("", bt.as_str()), |(p, t)| (p, t));
        let path = if path.is_empty() { String::new() } else { format!("{path}::") };
        let (fwd, field) = (tokens(&format!("{path}__{ty}{suffix}")), tokens(&f.rust_name()));
        forward.extend(quote!(#fwd!([#generics] #over, #field);));
    }
    let mut nums = size_align(&inst);
    for f in &def.fields {
        let field = tokens(&f.rust_name());
        let proj = quote!(|p: *const #inst| unsafe { ::core::ptr::addr_of!((*p).#field) });
        nums.push(quote!(::core::mem::offset_of!(#inst, #field) as u64));
        nums.push(quote!(crate::types::field_size(#proj)));
        nums.push(quote!(crate::types::field_align(#proj)));
    }
    // a #[c_lock] field's, after every field's: the lock's size, and where
    // in the Locked what it protects and what it publishes are
    for f in def.fields.iter().filter(|f| f.lock.is_some()) {
        let [r, ..] = cstruct::lock_args(&f.ty)?.expect("checked when parsed");
        let (r, ty) = (tokens(&r), tokens(&f.ty));
        nums.push(quote!(::core::mem::size_of::<#r>() as u64));
        nums.push(quote!(<#ty>::PROTECTED_OFFSET as u64));
        nums.push(quote!(<#ty>::PUBLISHED_OFFSET as u64));
    }
    // a pointer field C reads is a thin one: as c_extern!'s parameters
    let thin: TokenStream = def.fields.iter()
        .map(|f| thin_pointers(name, &f.cfg, &[(f.rust_name(), at_default(&f.ty, &def))], None))
        .collect();
    let flex = tokens(&vstructs::flex_array(&def)?);
    let zeroable = maybe_zeroable(&def);
    let record = record(&text, &nums);
    Ok(quote!(#forward #flex #zeroable const _: () = { #thin }; #record))
}

/// Zeroable when every field is - as bindgen's C structs are, deriving
/// pin_init::MaybeZeroable, and the same way: the for<'__dummy> makes each
/// bound a condition on the impl, not an error where it doesn't hold. So a
/// struct holding a Rust type with invariants - a RustOnly - isn't, and
/// #[derive(Init)] can't zero it. core's atomics are, as their integers are,
/// though pin_init has no impl for them, and the orphan rule allows none
/// here: their fields aren't bounds.
fn maybe_zeroable(def: &cstruct::CStructDef) -> TokenStream {
    let bounds: Vec<TokenStream> = def.fields.iter()
        .filter(|f| !is_core_atomic(&f.ty))
        .map(|f| tokens(&f.ty))
        .map(|ty| quote!(#ty: for<'__dummy> ::pin_init::Zeroable))
        .collect();
    let whr = (!bounds.is_empty()).then(|| quote!(where #(#bounds),*));
    let (generics, ty) = def.impl_generics();
    let (generics, ty) = (tokens(&generics), tokens(&ty));
    quote! {
        // SAFETY: every field is Zeroable, and padding may be zero
        #[automatically_derived]
        unsafe impl<#generics> ::pin_init::Zeroable for #ty #whr {}
    }
}

/// One of core::sync::atomic's integer, bool or pointer atomics, by any path
/// - or an array of one - or data_structures/bitmap.rs's AtomicULong, AtomicUsize as C's
/// unsigned long.
pub(crate) fn is_core_atomic(ty: &str) -> bool {
    let ty = ty.trim();
    let ty = ty.strip_prefix('[').and_then(|t| t.split(';').next()).unwrap_or(ty);
    let name = ty.trim().split('<').next().unwrap_or("").rsplit("::").next().unwrap_or("").trim();
    matches!(name, "AtomicBool" | "AtomicPtr" | "AtomicU8" | "AtomicI8" | "AtomicU16" | "AtomicI16" |
                   "AtomicU32" | "AtomicI32" | "AtomicU64" | "AtomicI64" | "AtomicUsize" | "AtomicIsize" |
                   "AtomicULong")
}

/// bitfield_struct::bitfield, recorded - and __NAME_forward!([GENERICS] STRUCT, field),
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
/// an enum - gives its width, #[bits(N)], as bitfield-struct has it; a uN or
/// bool member's #[bits(..)] gives only its options, as #[bits(default = 1)]
/// does. Members are pub, as C's are, unless written otherwise, and their
/// forwarders the same - pub(self), for one only its own module may set, has
/// none. ty is C's declared type of the integer members, which C lays the
/// bitfield out by - the storage type if not given: the generator's, not
/// passed on.
///
/// With #[derive(CStruct)], the bitfield is itself the C struct - C's
/// `struct bch_extent_ptr { __u64 type:1, ...; }` - and C's attributes go on
/// it as on any: #[c_packed], and `align = 8` for __aligned(8), which a u64
/// bitfield needs said on i386 (Align8U64). It's recorded as the struct and
/// #[c_bitfield] member the generator knows - as `NAME { bits: NAME_bits }`.
#[proc_macro_attribute]
pub fn bitfield(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[bitfield]", || bitfield_expand(attr.into(), item.into()))
}

/// `#[vtable]`, on an impl of a trait whose methods are optional - one with
/// `const HAS_FOO: bool = false` for each method foo: `const HAS_FOO: bool =
/// true` for each the impl defines, unless it says otherwise itself. So a
/// table of C function pointers made from the trait leaves out what isn't
/// implemented. The kernel crate's #[vtable] does this too, but from 7.3 its
/// impls name crate::LocalModule, which only a module!() crate has.
#[proc_macro_attribute]
pub fn vtable(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[vtable]", || {
        if !attr.is_empty() {
            return Err(syn::Error::new_spanned(proc_macro2::TokenStream::from(attr), "takes no arguments"));
        }
        let mut imp: syn::ItemImpl = syn::parse(item)?;
        let consts: Vec<String> = imp.items.iter().filter_map(|i| match i {
            syn::ImplItem::Const(c) => Some(c.ident.to_string()),
            _ => None,
        }).collect();
        let has: Vec<syn::Ident> = imp.items.iter().filter_map(|i| match i {
            syn::ImplItem::Fn(f) => Some(quote::format_ident!("HAS_{}", f.sig.ident.to_string().to_uppercase())),
            _ => None,
        }).filter(|c| !consts.contains(&c.to_string())).collect();
        for c in has {
            imp.items.push(syn::parse_quote!(const #c: bool = true;));
        }
        Ok(quote::quote!(#imp))
    })
}

/// `#[args_stored_once]` on an async fn: the same fn, as `fn ->
/// impl Future { async move { body } }`, so an argument live across an
/// await is stored once in its future - an async fn stores it twice
/// (rust-lang/rust#62958). See args_stored_once.rs.
#[proc_macro_attribute]
pub fn args_stored_once(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[args_stored_once]", || args_stored_once::args_stored_once(attr.into(), syn::parse(item)?))
}

/// `#[placed(Base.field)]`, ahead of `#[pin_data]` and the derives, on a
/// struct that lives only at Base's field: its At, the field util/place.rs's
/// Contained is had by, which `#[derive(Init)]` then gives its init() - see
/// place.rs.
#[proc_macro_attribute]
pub fn placed(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[placed]", || place::placed(syn::parse(attr)?, syn::parse(item)?))
}

/// `#[counters(a, b, ..)]` on an accounting key type X: its counters, struct
/// XCounters - accounting.rs's acct_counters! - and its KeyType impl.
#[proc_macro_attribute]
pub fn counters(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[counters]", || {
        let s: syn::ItemStruct = syn::parse(item)?;
        let (name, attr) = (&s.ident, proc_macro2::TokenStream::from(attr));
        let counters = quote::format_ident!("{name}Counters");
        Ok(quote::quote! {
            #s
            crate::accounting::acct_counters!(#counters { #attr });
            impl crate::accounting::KeyType for #name {
                type Delta = #counters<i64>;
                type Read  = #counters<u64>;
            }
        })
    })
}

/// `#[unsized_vstruct]`, on C's definition of a struct ending in a flexible
/// array - or `#[unsized_vstruct(tail = [E])]`, `tail = NAME: [E]`, saying
/// what follows its header: the family of its definitions, one per state -
/// C's, renamed X__head, and each state's, a DST of the header and its tail -
/// and X<S>, a generic struct over the one for S. See vstructs.rs.
#[proc_macro_attribute]
pub fn unsized_vstruct(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[unsized_vstruct]", || vstructs::unsized_vstruct(attr.into(), item.into()))
}

/// `#[grown(from = FIELD)]`, on the definition of a type that grew between
/// format versions, FIELD the first field an older value may lack: X<S>, a
/// generic struct - X<Whole>, X, the whole struct; X<Validated>, the floor
/// and the bytes there are, at the value's length. See vstructs.rs.
#[proc_macro_attribute]
pub fn grown(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[grown]", || vstructs::grown(attr.into(), item.into()))
}

/// `#[counted]`, `#[counted(PATH)]`, `#[counted(PATH, tail = [E])]` or
/// `tail = NAME: [E]`, on C's definition of a header and an array it counts
/// in elements: its family, the count private in each state's definition -
/// see vstructs.rs.
#[proc_macro_attribute]
pub fn counted(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[counted]", || vstructs::counted(attr.into(), item.into()))
}

/// `#[whole]`, on C's definition of a type every valid value of which has
/// all of it - a superblock section or journal entry of fixed fields: its
/// family, every state all of it, at the value's length - see vstructs.rs.
#[proc_macro_attribute]
pub fn whole(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[whole]", || vstructs::whole(attr.into(), item.into()))
}

fn bitfield_expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
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
        format!("#[bitfield({rec_args})] pub struct {name}_bits {}", s.body())
    } else {
        format!("#[bitfield({rec_args})] pub struct {name} {}", s.body())
    };
    let bf = one_item!(&text, Bitfield, "a bitfield struct");
    let mut out = record(&text, &[]);
    let ident = format_ident!("{name}");
    // Each member, not padding: its name, setter and updater, type, and
    // visibility
    let members: Vec<_> = bf.fields.iter().map(|(m, ty, _)| (rust_ident(m), ty)).filter(|(m, _)| !m.starts_with('_'))
        .map(|(m, ty)| {
            let vis = s.vis.iter().find(|(n, _)| *n == m).map_or("pub", |(_, v)| v.as_str());
            (format_ident!("{m}"), format_ident!("set_{m}"), format_ident!("update_{m}"), tokens(ty), tokens(vis))
        })
        .collect();

    // update_x(f): set_x(f(x())), each member's, as visible as its setter -
    // bitfield_struct's, which has the getter and setter but not this.
    // XXX Deprecated when real field projections arrive: x op= v through a
    // projected place instead.
    let update = members.iter().map(|(m, set, update, ty, vis)| quote! {
        #[allow(dead_code)]
        #vis fn #update(&mut self, f: impl FnOnce(#ty) -> #ty) { self.#set(f(self.#m())) }
    });
    out.extend(quote!(impl #ident { #(#update)* }));

    if s.c_struct {
        let repr = if args.align8 { "#[repr(C, align(8))]" } else { "#[repr(C)]" };
        let packed = if s.packed { "#[c_packed]" } else { "" };
        let text = format!("{repr} #[derive(CStruct)] {packed} \
                            pub struct {name} {{ #[c_bitfield] pub bits: {name}_bits, }}");
        one_item!(&text, Struct, "a struct");
        // the member's: the whole struct
        let mut nums = size_align(&ident);
        nums.insert(2, quote!(0));
        nums.extend(size_align(&ident));
        out.extend(record(&text, &nums));
    } else {
        // A member of a C struct: its getters and setters for that struct -
        // not pub(self)'s: its module's, which has the bitfield itself
        let fns = members.iter().filter(|(.., vis)| vis.to_string().replace(' ', "") != "pub(self)")
            .map(|(m, set, update, ty, vis)| quote! {
                #vis fn #m(&self) -> #ty { let b = self.$field; b.#m() }
                #vis fn #set(&mut self, v: #ty) { let mut b = self.$field; b.#set(v); self.$field = b; }
                #vis fn #update(&mut self, f: impl FnOnce(#ty) -> #ty) {
                    let mut b = self.$field; b.#set(f(b.#m())); self.$field = b;
                }
            }).collect::<Vec<_>>();
        let cell_fns = members.iter().filter(|(.., vis)| vis.to_string().replace(' ', "") != "pub(self)")
            .map(|(m, set, update, ty, vis)| quote! {
                #vis fn #m(&self) -> #ty { self.$field.get().#m() }
                #vis fn #set(&self, v: #ty) { let mut b = self.$field.get(); b.#set(v); self.$field.set(b); }
                #vis fn #update(&self, f: impl FnOnce(#ty) -> #ty) {
                    let mut b = self.$field.get(); b.#set(f(b.#m())); self.$field.set(b);
                }
            }).collect::<Vec<_>>();
        let fwd = format_ident!("__{name}_forward");
        let fwd_cell = format_ident!("__{name}_forward_cell");
        out.extend(quote! {
            #[allow(unused_macros)]
            macro_rules! #fwd {
                ([$($g:tt)*] $s:ty, $field:ident) => { impl<$($g)*> $s { #(#fns)* } };
            }
            #[allow(unused_imports)]
            pub(crate) use #fwd;
            #[allow(unused_macros)]
            macro_rules! #fwd_cell {
                ([$($g:tt)*] $s:ty, $field:ident) => { impl<$($g)*> $s { #(#cell_fns)* } };
            }
            #[allow(unused_imports)]
            pub(crate) use #fwd_cell;
        });
    }

    let (storage, rest) = (format_ident!("{}", args.storage), &args.rest);
    let align8 = args.align8.then(|| quote! {
        , repr = crate::types::Align8U64, from = crate::types::Align8U64::from_u64,
        into = crate::types::Align8U64::to_u64
    });
    // What it holds - its repr, else the storage integer - and nothing else:
    // so it's Plain
    let held = if args.align8 {
        "crate::types::Align8U64".to_string()
    } else {
        rest.iter()
            .find_map(|a| key_value(a).filter(|(k, _)| k == "repr").map(|(_, v)| v))
            .unwrap_or_else(|| args.storage.clone())
    };
    let item = &s.item;
    let (info, plain) = (tokens(&typeinfo::info(name, "Struct", "")), tokens(&vstructs::plain(name, &[&held])));
    // The struct as tokens, not text: an error in a member points at it
    Ok(quote! {
        #out
        #[::bitfield_struct::bitfield(#storage #(, #rest)* #align8)]
        #item
        #info
        #plain
        // SAFETY: Plain - its integer, any bytes of which are one
        unsafe impl ::pin_init::Zeroable for #ident {}
    })
}

/// @ts's top-level comma separated items - commas in a type's <..> aren't -
/// less empty ones: a trailing comma's.
fn comma_list(ts: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut items = vec![Vec::new()];
    let mut depth = 0;
    // a '>' of => or -> closes nothing
    let mut arrow = false;
    for t in ts {
        let joint_arrow = matches!(&t, TokenTree::Punct(p)
                                   if matches!(p.as_char(), '=' | '-') && p.spacing() == Spacing::Joint);
        match &t {
            TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '>' && !arrow => depth -= 1,
            TokenTree::Punct(p) if p.as_char() == ',' && depth == 0 => {
                items.push(Vec::new());
                arrow = false;
                continue;
            }
            _ => {}
        }
        arrow = joint_arrow;
        items.last_mut().unwrap().push(t);
    }
    items.retain(|i| !i.is_empty());
    items
}

/// @span's location, as the macro's: an error points there, while what's
/// generated from it - bitfield-struct's with_type__checked() of a member
/// type_ - isn't linted as written code.
fn at(span: Span) -> Span {
    Span::call_site().located_at(span)
}

/// @ts, every token at(@span): for what a macro makes in place of what's
/// written.
fn respan(ts: TokenStream, span: Span) -> TokenStream {
    let span = at(span);
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
    rest:    Vec<TokenStream>,
    /// `ty = T`: C's declared type of the integer members
    ty:      Option<String>,
    /// `align = 8`: C's __aligned(8), on a u64 bitfield that's a C struct
    align8:  bool,
}

/// `KEY = VALUE`: KEY, and VALUE as text.
fn key_value(toks: &TokenStream) -> Option<(String, String)> {
    match &toks.clone().into_iter().collect::<Vec<_>>()[..] {
        [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' && !v.is_empty() =>
            Some((k.to_string(), v.iter().cloned().collect::<TokenStream>().to_string())),
        _ => None,
    }
}

fn bitfield_args(attr: TokenStream) -> Result<BitfieldArgs, String> {
    let mut rest: Vec<TokenStream> = comma_list(attr).into_iter().map(|a| a.into_iter().collect()).collect();
    let storage = match rest.first().map(|a| a.clone().into_iter().collect::<Vec<_>>()).as_deref() {
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
    item:     syn::ItemStruct,
    /// #[derive(CStruct)]: the bitfield is C's struct, not a member of one
    c_struct: bool,
    /// #[c_packed]: C's is __packed
    packed:   bool,
    /// Each member's name and visibility
    vis:      Vec<(String, String)>,
}

impl BitfieldStruct {
    /// Its body, those members - for the record
    fn body(&self) -> TokenStream {
        self.item.fields.to_token_stream()
    }
}

fn bitfield_struct(item: TokenStream) -> Result<BitfieldStruct, String> {
    let mut item: syn::ItemStruct = syn::parse2(item).map_err(|e| format!("expected a struct - {e}"))?;
    let name = item.ident.to_string();
    let (mut c_struct, mut packed) = (false, false);
    let mut attrs = Vec::new();
    for a in std::mem::take(&mut item.attrs) {
        if matches!(&a.meta, syn::Meta::Path(p) if p.is_ident("c_packed")) {
            packed = true;
        } else if a.path().is_ident("derive") {
            let derives = a.parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                .map_err(|e| format!("{name}: {e}"))?;
            let kept: Vec<_> = derives.into_iter()
                .filter(|p| match p.segments.last().map(|s| s.ident.to_string()).as_deref() {
                    Some("CStruct") => { c_struct = true; false }
                    Some("TypeInfo") => false,
                    _ => true,
                })
                .collect();
            if !kept.is_empty() {
                let derive = respan(quote!(derive(#(#kept),*)), a.path().segments[0].ident.span());
                attrs.push(syn::parse_quote!(#[#derive]));
            }
        } else {
            attrs.push(a);
        }
    }
    item.attrs = attrs;
    if packed && !c_struct {
        return Err(format!("{name}: #[c_packed] is a C struct's - #[derive(CStruct)]"));
    }
    let syn::Fields::Named(members) = &mut item.fields else {
        return Err(format!("{name}: expected members, `name: type`"));
    };
    let mut vis = Vec::new();
    for m in members.named.iter_mut() {
        bitfield_member(m)?;
        vis.push((m.ident.as_ref().unwrap().to_string(), m.vis.to_token_stream().to_string()));
    }
    Ok(BitfieldStruct { name, item, c_struct, packed, vis })
}

/// One member: `name: uN` and `name: bool` with their #[bits(N)] and Rust
/// type, at the span of the type they replace, so an error points there - a
/// #[bits(..)] of bitfield-struct's options only, default = .., gets the width
/// put first; one with #[bits(N, ..)] as written. Its visibility as written,
/// pub if none.
fn bitfield_member(m: &mut syn::Field) -> Result<(), String> {
    use syn::parse::Parser as _;
    let name = m.ident.clone().expect("a named member");
    let bits = m.attrs.iter().position(|a| a.path().is_ident("bits"));
    let width_given = bits.is_some_and(|i| matches!(&m.attrs[i].meta, syn::Meta::List(l)
        if matches!(l.tokens.clone().into_iter().next(), Some(TokenTree::Literal(_)))));
    if !width_given {
        let t = match &m.ty {
            syn::Type::Path(p) if p.qself.is_none() => p.path.get_ident().cloned(),
            _ => None,
        }.ok_or_else(|| format!("{name}: a member without #[bits(N)] is uN or bool"))?;
        let (width, rust_ty) = bilge_width(&t.to_string())
            .ok_or_else(|| format!("{name}: {t}: a member without #[bits(N)] is uN or bool"))?;
        // the other attributes, then #[bits(N)] with its options
        let opts = bits.map(|i| m.attrs.remove(i)).and_then(|a| match a.meta {
            syn::Meta::List(l) => Some(l.tokens),
            _ => None,
        }).map(|o| quote!(, #o));
        let width = Literal::u32_unsuffixed(width);
        m.attrs.extend(syn::Attribute::parse_outer.parse2(respan(quote!(#[bits(#width #opts)]), t.span()))
            .expect("an attribute"));
        m.ty = syn::parse2(respan(tokens(&rust_ty), t.span())).expect("a type");
    }
    if matches!(m.vis, syn::Visibility::Inherited) {
        m.vis = syn::parse2(respan(quote!(pub), name.span())).expect("a visibility");
    }
    m.ident.as_mut().unwrap().set_span(at(name.span()));
    Ok(())
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

/// C carried as is: c_verbatim!(r#"..."#).
#[proc_macro]
pub fn c_verbatim(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_verbatim!", || {
        let text = format!("c_verbatim!({input})");
        one_item!(&text, Verbatim, "one string");
        Ok(record(&text, &[]))
    })
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
pub fn c_xmacro(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_xmacro!", || xmacro(input.into()))
}

/// A list's rows, selected: NAME!(xmacro_select [TOKEN => cb]) expands to
/// cb! { (args), ... } of the rows with the identifier TOKEN in a set of
/// theirs, a `[..]` - a flag in a flags column, as ptrs is in bkey_val's
/// (tagged_union!'s #[arm_columns]) - or, in a row without one, among its own
/// tokens, as BTREE_IS_extents is in BCH_BTREE_IDS()'. None selected is an
/// error: an empty mask compiles, and runs no triggers. cb may be a path;
/// [TOKEN => cb { args }] hands it args before the rows, cb! { args (args), ... }.
/// Selecting in a macro_rules! muncher instead nests an expansion per token,
/// which runs into the recursion limit as lists grow.
#[proc_macro]
pub fn xmacro_select(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("xmacro_select!", || {
        let mut input = TokenStream::from(input).into_iter();
        let Some(TokenTree::Group(pre)) = input.next().filter(|t| matches!(t,
            TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket)) else {
            return Err("expected [TOKEN => cb] before the list".into());
        };
        let pre: Vec<TokenTree> = pre.stream().into_iter().collect();
        let (token, cb) = match pre.as_slice() {
            [TokenTree::Ident(token), TokenTree::Punct(eq), TokenTree::Punct(gt), cb @ ..]
                if eq.as_char() == '=' && gt.as_char() == '>' && !cb.is_empty() => (token.to_string(), cb),
            _ => return Err("expected [TOKEN => cb] before the list".into()),
        };
        let (cb, args) = match cb {
            [cb @ .., TokenTree::Group(g)] if g.delimiter() == Delimiter::Brace => (cb, g.stream()),
            cb => (cb, TokenStream::new()),
        };
        let cb = cb.iter().cloned().collect::<TokenStream>();

        let is_token = |t: &TokenTree| matches!(t, TokenTree::Ident(i) if i.to_string() == token);
        let mut rows = Vec::new();
        for t in input {
            match t {
                TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
                    let set = g.stream().into_iter().find_map(|t| match t {
                        TokenTree::Group(s) if s.delimiter() == Delimiter::Bracket => Some(s.stream()),
                        _ => None,
                    });
                    if set.unwrap_or_else(|| g.stream()).into_iter().any(|t| is_token(&t)) {
                        rows.push(g);
                    }
                }
                TokenTree::Punct(p) if p.as_char() == ',' => {}
                t => return Err(format!("expected a list of (args) rows, found {t}")),
            }
        }
        if rows.is_empty() {
            return Err(format!("no row has {token}"));
        }
        Ok(quote!(#cb! { #args #(#rows),* }))
    })
}

/// c_xmacro!'s expansion: the list's record, and NAME!(cb).
fn xmacro(input: TokenStream) -> Result<TokenStream, String> {
    let text = format!("c_xmacro! {{ {input} }}");
    let x = one_item!(&text, XMacro, "NAME(x) { (args), ... }");
    let name = format_ident!("{}", x.name);
    // The list: the brace group after NAME(x), past any attributes
    let list = input.into_iter()
        .filter_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g.stream()),
            _ => None,
        })
        .last()
        .expect("parse_items found the list");
    let entries: Vec<TokenStream> = comma_list(list).into_iter().map(|e| e.into_iter().collect()).collect();

    let mut out = record(&text, &[]);
    let subs: Vec<usize> = x.entries.iter().enumerate()
        .filter(|(_, e)| matches!(e, CXEntry::Sub { .. }))
        .map(|(i, _)| i)
        .collect();

    if subs.is_empty() {
        out.extend(quote! {
            #[allow(unused_macros)]
            macro_rules! #name { ($($cb:ident)::+ $([$($pre:tt)*])?) => { $($cb)::+! { $([$($pre)*])? #(#entries),* } } }
            #[allow(unused_imports)]
            pub(crate) use #name;
        });
        return Ok(out);
    }

    // The plain entries before the first list spliced in, between each and
    // the next, and after the last.
    let lits = |from: usize, to: usize| {
        let e = &entries[from..to];
        quote!(#(#e,)*)
    };
    let path = quote!(crate::cstructs::c);
    let carry = quote!(($($cb)::+) ($($pre)?));
    let step = |n: usize| format_ident!("__{}_{n}", x.name);
    for (n, &i) in subs.iter().enumerate() {
        let CXEntry::Sub { map, .. } = &x.entries[i] else { unreachable!() };
        let (pattern, mapped) = match map {
            None => (quote!(($($e:tt)*)), quote!(($($e)*))),
            Some(m) => {
                let params = m.params.iter().map(|p| format_ident!("{p}"));
                let pattern = quote!((#($#params:tt),*));
                // The template: params are $p, and concat(A, p) is paste!'s
                // [<A $p>] - the mapping is expanded inside paste!.
                let template = spell_concat(&m.template, |p| format!("[<{}>]", p.join(" ")))?;
                let not_word = |c: char| !(c.is_ascii_alphanumeric() || c == '_');
                let arg = |a: &str| -> String {
                    let mut out = String::new();
                    for piece in a.split_inclusive(not_word) {
                        if m.params.iter().any(|p| p == piece.trim_end_matches(not_word)) {
                            out.push('$');
                        }
                        out.push_str(piece);
                    }
                    out.trim().to_string()
                };
                (pattern, tokens(&format!("({})", template.split(',').map(arg).collect::<Vec<_>>().join(", "))))
            }
        };
        let after = lits(i + 1, subs.get(n + 1).copied().unwrap_or(entries.len()));
        let acc = quote!($($acc)* $(#mapped,)* #after);
        let next = match subs.get(n + 1) {
            Some(&j) => {
                let CXEntry::Sub { list: next_list, .. } = &x.entries[j] else { unreachable!() };
                let (next_list, next_step) = (format_ident!("{next_list}"), step(n + 1));
                quote!(#path::#next_list!(#path::#next_step [#carry #acc]);)
            }
            None => quote!($($cb)::+! { $($pre)? #acc }),
        };
        let this = step(n);
        out.extend(quote! {
            #[allow(unused_macros)]
            macro_rules! #this {
                ([($($cb:ident)::+) ($($pre:tt)?) $($acc:tt)*] $(#pattern),* $(,)?) => { ::paste::paste! { #next } };
            }
            #[allow(unused_imports)]
            pub(crate) use #this;
        });
    }
    // The callback's [pre] bracketed, as the plain list's: an optional single
    // tt after a path callback is ambiguous with its `::`.
    let CXEntry::Sub { list: first, .. } = &x.entries[subs[0]] else { unreachable!() };
    let (first, step0, lits0) = (format_ident!("{first}"), step(0), lits(0, subs[0]));
    out.extend(quote! {
        #[allow(unused_macros)]
        macro_rules! #name {
            ($($cb:ident)::+ $([$($pre:tt)*])?) => { #path::#first!(#path::#step0 [($($cb)::+) ($([$($pre)*])?) #lits0]); }
        }
        #[allow(unused_imports)]
        pub(crate) use #name;
    });
    Ok(out)
}

/// An enum, and what its values can be (fs/enum_kind.h):
///
///   c_enum! { #[open] #[prefix(BTREE_ID_)] pub enum Btree: u32 { extents = 0, ... } }
///
/// Each value is also a constant of the module, of the integer type, by C's
/// name for it - `#[prefix(P)]`'s P and its name; a name written with P is
/// C's - as C's enum constants are all one namespace, which C refers to by
/// bare name: in other enums' values, array lengths. The type is built on
/// those:
///
///   #[closed]  a Rust enum
///   #[open]    a newtype over the integer, the values its constants: a value
///              from outside can be anything, so it's Plain, and a word a
///              Volatile can hold
///   #[flags]   a bitflags type
///
/// Any of them can be a #[bitfield]'s member, as C's `enum btree_id
/// btree_id:8`: it has the const into_bits() bitfield-struct converts with,
/// and from_bits() - for #[flags], bitflags' from_bits() is the checked one,
/// so a member names `from = T::from_bits_retain`. A #[closed] enum's member
/// only ever holds one of its values, as its type says; from_bits() of
/// anything else is memory corruption, and panics.
///
/// An open or closed one with a #[prefix] is named, as it prints and parses,
/// by its variants' names past it. `#[nr(NR)]`: C's NR after the last, and
/// NR, a number, not one of its values. A variant with no value is the one
/// before it plus one, as in C. `_` for the name: an anonymous enum, the
/// constants alone.
///
/// A variant is written with its doc, and its columns - `#[arm_columns(..)]`,
/// enum_decl.rs - each made a const fn of the enum:
///
///   c_enum! {
///       #[open] #[c_name(btree_id)] #[prefix(BTREE_ID_)] #[nr(BTREE_ID_NR)]
///       #[arm_columns(flags: BtreeFlags, holds: set(BkeyType))]
///       #[c_list(BCH_BTREE_IDS)]
///       pub enum Btree: u32 {
///           /// File data extent pointers and reservations
///           #[flags(extents, snapshots, data, trans_triggers)]
///           #[holds(whiteout, extent, reservation, ..)]
///           extents = 0,
///           ...
///       }
///   }
///
/// Or from lists, `LIST(params) => template`, a variant for each entry.
#[proc_macro]
pub fn c_enum(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_enum!", || {
        let input = TokenStream::from(input);
        if !from_lists(&input) {
            return enum_decl::c_enum(input);
        }
        let text = format!("c_enum! {{ {input} }}");
        let e = one_item!(&text, Enum, C_ENUM_SYNTAX);
        Ok(match e.from.first() {
            Some((_, f)) => through_list(&f.list, "cstruct_macros::__c_enum", input),
            None => c_enum_gen(&e, EnumOf::C(&text)),
        })
    })
}

const C_ENUM_SYNTAX: &str = "#[open|closed|flags] pub enum NAME: REPR { ... }";

/// Whether c_enum! @input has variants from a list: `LIST(params) => ..`.
fn from_lists(input: &TokenStream) -> bool {
    let Some(TokenTree::Group(body)) = input.clone().into_iter().last() else { return false };
    comma_list(body.stream()).iter().any(|v| v.windows(2).any(|w| matches!(w,
        [TokenTree::Punct(eq), TokenTree::Punct(gt)] if eq.as_char() == '=' && eq.spacing() == Spacing::Joint && gt.as_char() == '>')))
}

/// `atomic_bitfield! { pub struct NAME: uN [in uM] { fields } }`: a
/// bitfield's value, NAME, and AtomicNAME, a word of it that's atomic - see
/// atomic_bitfield.rs.
#[proc_macro]
pub fn atomic_bitfield(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("atomic_bitfield!", || atomic_bitfield::atomic_bitfield(input.into()))
}

/// atomic_bitfield!'s list's callback: { [decl] (row), (row), ... }.
#[doc(hidden)]
#[proc_macro]
pub fn __atomic_bitfield(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("atomic_bitfield!", || atomic_bitfield::rows(input.into()))
}

/// `#[open_enum]` on `#[repr(uN)] pub enum NAME { a = 0, b, ... }`: an enum
/// Rust alone has, whose values can be anything - read off disk, written by
/// another version: c_enum!'s #[open], without the C - no constants of the
/// module, no record. Each variant is named, as Names has it and it prints,
/// by its own name, or its `#[label("..")]`; #[own_to_text] as c_enum!'s.
#[proc_macro_attribute]
pub fn open_enum(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[open_enum]", || {
        use syn::ext::IdentExt as _;
        if !attr.is_empty() {
            return Err(syn::Error::new_spanned(TokenStream::from(attr), "takes no arguments"));
        }
        let item: syn::ItemEnum = syn::parse(item)?;
        let what = |s: &str| format!("{}: {s}", item.ident);
        let mut repr = None;
        let mut own_to_text = false;
        let mut attrs = Vec::new();
        for a in &item.attrs {
            if a.path().is_ident("repr") {
                repr = Some(a.parse_args::<Ident>()?);
            } else if a.path().is_ident("own_to_text") {
                own_to_text = true;
            } else {
                attrs.push(a.clone());
            }
        }
        let repr = repr.filter(|r| cstruct::int_bits(&r.to_string()).is_some())
            .ok_or_else(|| syn::Error::new_spanned(&item.ident, what("#[repr(uN)]: its integer type")))?;
        let mut variants = Vec::new();
        for v in &item.variants {
            if !matches!(v.fields, syn::Fields::Unit) {
                return Err(syn::Error::new_spanned(v, what("its variants are values, without fields")));
            }
            let name = v.ident.unraw().to_string();
            let label = match v.attrs.iter().find(|a| a.path().is_ident("label")) {
                Some(a) => a.parse_args::<syn::LitStr>()?.value(),
                None => name.clone(),
            };
            let value = v.discriminant.as_ref().map(|(_, x)| x.to_token_stream().to_string());
            variants.push(CEnumVariant { name, c_name: None, value, label: Some(label), docs: Vec::new() });
        }
        let e = CEnumDef { cfg: None, kind: CEnumKind::Open, name: Some(item.ident.to_string()), repr: repr.to_string(),
                           variants, from: Vec::new(), own_to_text, c_name: None, prefix: None, nr: None };
        Ok(c_enum_gen(&e, EnumOf::Rust(&item.vis, &attrs)))
    })
}

/// c_enum!'s list's callback: { [decl] (entry), (entry), ... } - the enum
/// with its first list's variants in their place: through the next list, if
/// it has another, or recorded so.
#[doc(hidden)]
#[proc_macro]
pub fn __c_enum(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_enum!", || {
        let (decl, entries) = list_callback(input.into())?;
        let mut e = one_item!(&format!("c_enum! {{ {decl} }}"), Enum, C_ENUM_SYNTAX);
        expand_enum_list(&mut e, &entries)
            .map_err(|err| format!("{}: {err}", e.name.as_deref().unwrap_or("_")))?;
        let decl = enum_text(&e);
        Ok(match e.from.first() {
            Some((_, f)) => through_list(&f.list, "cstruct_macros::__c_enum", tokens(&decl)),
            None => c_enum_gen(&e, EnumOf::C(&format!("c_enum! {{ {decl} }}"))),
        })
    })
}

/// Whose an enum is: C's, a c_enum!'s - its constants the module's, as C's
/// are, and its record, of the text - or Rust's alone, an #[open_enum]'s,
/// with the visibility and attributes it was written with.
#[derive(Clone, Copy)]
enum EnumOf<'a> {
    C(&'a str),
    Rust(&'a syn::Visibility, &'a [syn::Attribute]),
}

/// Enum @e's Rust - and if it's C's, its record.
fn c_enum_gen(e: &CEnumDef, of: EnumOf<'_>) -> TokenStream {
    let repr = format_ident!("{}", e.repr);
    let names: Vec<Ident> = e.variants.iter().map(|v| format_ident!("{}", v.name)).collect();
    // the module's constants are C's, by its names
    let c_names: Vec<Ident> = e.variants.iter().map(|v| format_ident!("{}", e.c_variant(v))).collect();

    // A variant with no value counts from the last that has one - not from
    // the one before it, which would make a const chain as long as the enum:
    // too deep for rustc, at bch_persistent_counters' hundreds.
    let mut out = TokenStream::new();
    let mut values = Vec::new();
    let mut base = None;
    let mut n = 0;
    for ((v, c), CEnumVariant { value, .. }) in names.iter().zip(&c_names).zip(&e.variants) {
        let k = Literal::usize_unsuffixed(n);
        let value = match (value, base) {
            (Some(x), _) => {
                base = Some((v, c));
                n = 0;
                tokens(x)
            }
            (None, Some((_, b))) if matches!(of, EnumOf::C(_)) => quote!(#b + #k),
            (None, Some((b, _))) => quote!(Self::#b.0 + #k),
            (None, None) => quote!(#k),
        };
        match of {
            EnumOf::C(_) => {
                out.extend(quote!(#[allow(non_upper_case_globals, dead_code)] pub const #c: #repr = #value;));
                values.push(quote!(#c));
            }
            EnumOf::Rust(..) => values.push(value),
        }
        n += 1;
    }
    // C's after the last, as C counts it
    let nr = e.nr.as_ref().map(|nr| {
        let nr = format_ident!("{nr}");
        let after = match c_names.last() {
            Some(last) => quote!(#last + 1),
            None => quote!(0),
        };
        out.extend(quote!(#[allow(non_upper_case_globals, dead_code)] pub const #nr: #repr = #after;));
        nr
    });

    let ty = match &e.name {
        Some(n) => format_ident!("{n}"),
        None => repr.clone(),
    };
    if let Some(name) = &e.name {
        let (vis, attrs) = match of {
            EnumOf::C(_) => (quote!(pub), cfi_encoding(e.c_ident().unwrap_or(name))),
            EnumOf::Rust(vis, attrs) => (quote!(#vis), quote!(#(#attrs)*)),
        };
        let name = format_ident!("{name}");
        let docs: Vec<_> = e.variants.iter().map(|v| { let d = &v.docs; quote!(#(#[doc = #d])*) }).collect();
        out.extend(match e.kind {
            CEnumKind::Closed => {
                // what it prints as: its list's name for it, as C's string table has it
                let labels = e.variants.iter().map(|v| e.label(v).map(|l| quote!(#[label(#l)])));
                let msg = format!("{name}::from_bits(): not a {name}");
                // as bit numbers: a set of them takes a bit past the largest -
                // a negative value, unlocked's -1, isn't one
                let nr = quote! {{
                    let mut nr = 0;
                    #(if #name::#names as i64 >= nr { nr = #name::#names as i64 + 1; })*
                    nr as usize
                }};
                quote! {
                    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, crate::util::ToText)]
                    #[repr(#repr)]
                    #attrs
                    #vis enum #name { #(#docs #labels #names = #values,)* }
                    impl #name {
                        pub const fn from_bits(v: #repr) -> Self {
                            match v {
                                #(x if x == Self::#names as #repr => Self::#names,)*
                                _ => panic!(#msg),
                            }
                        }
                        pub const fn into_bits(self) -> #repr { self as #repr }
                    }
                    impl crate::data_structures::bitmap::BitEnum for #name {
                        type Words = crate::data_structures::bitmap::AtomicBitArray<{ crate::types::bits_to_longs(#nr) }>;
                        fn bit(self) -> usize { self as usize }
                        fn from_bit(bit: usize) -> Option<Self> {
                            match bit {
                                #(x if x == Self::#names as usize => Some(Self::#names),)*
                                _ => None,
                            }
                        }
                    }
                }
            }
            CEnumKind::Open => quote! {
                #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
                #[repr(transparent)]
                #attrs
                #vis struct #name(pub #repr);
                #[allow(non_upper_case_globals)]
                impl #name {
                    #(#docs pub const #names: Self = Self(#values);)*
                    pub const fn from_bits(v: #repr) -> Self { Self(v) }
                    pub const fn into_bits(self) -> #repr { self.0 }
                }
                // SAFETY: its integer, every value of which is one
                unsafe impl crate::util::vstructs::Plain for #name {}
                crate::one_word!(#name);
            },
            CEnumKind::Flags => quote! {
                ::bitflags::bitflags! {
                    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
                    #[repr(transparent)]
                    #attrs
                    #vis struct #name: #repr {
                        #(#docs #[allow(non_upper_case_globals)] const #names = #values;)*
                    }
                }
                impl #name {
                    pub const fn into_bits(self) -> #repr { self.bits() }
                }
                unsafe impl crate::util::vstructs::Plain for #name {}
            },
        });
        if let Some(nr) = &nr {
            out.extend(quote!(impl #name { pub const NR: #repr = #nr; }));
        }
    }

    if let Some(name) = &e.name {
        out.extend(names_gen(e, name));
        out.extend(tokens(&typeinfo::info_of(name, e.c_ident().unwrap_or(name), "Enum", "")));

        // Zeroed is a value of it - so a struct holding one can be Zeroable:
        // any integer is an open enum's or flags', a closed enum's only if a
        // variant is 0 - as written, or the first, counting from 0
        let has_zero = e.kind != CEnumKind::Closed || e.variants.iter().enumerate()
            .any(|(i, v)| match &v.value {
                None    => i == 0,
                Some(x) => x.trim() == "0",
            });
        if has_zero {
            out.extend(quote! {
                // SAFETY: zero is a value of it
                unsafe impl ::pin_init::Zeroable for #ty {}
            });
        }
    }

    if let EnumOf::C(text) = of {
        let mut nums = size_align(&ty);
        nums.extend(c_names.iter().map(|v| quote!(#v as u64)));
        out.extend(record(text, &nums));
    }
    out
}

/// An enum from an x-macro list - with labelled variants - is named by the
/// list: util/names.rs's Names, its labelled variants' names and values, a
/// flags type's bits. An open or flags one prints through it - ToText, and
/// Display through that - unless it's #[own_to_text]; a closed one derives
/// ToText.
fn names_gen(e: &CEnumDef, name: &str) -> TokenStream {
    let labelled: Vec<(&str, Ident)> = e.variants.iter()
        .filter_map(|v| e.label(v).map(|l| (l, format_ident!("{}", v.name))))
        .collect();
    if labelled.is_empty() {
        return TokenStream::new();
    }

    let (name, n) = (format_ident!("{name}"), quote!(crate::util::names));
    let entries = labelled.iter().map(|(l, v)| match e.kind {
        CEnumKind::Closed => quote!((#l, Self::#v as u64)),
        CEnumKind::Open   => quote!((#l, Self::#v.0 as u64)),
        CEnumKind::Flags  => quote!((#l, #n::bit(Self::#v.bits() as u64))),
    });
    let mut out = quote! {
        impl #n::Names for #name {
            const ENTRIES: &'static [(&'static str, u64)] = &[#(#entries),*];
        }
    };

    let to_text = match e.kind {
        _ if e.own_to_text => None,
        CEnumKind::Closed  => None,
        CEnumKind::Open    => Some(quote!(#n::prt_name(out, <Self as #n::Names>::ENTRIES, self.0 as u64))),
        CEnumKind::Flags   => Some(quote!(#n::prt_flags(out, <Self as #n::Names>::ENTRIES, self.bits() as u64))),
    };
    // A closed enum's derive gives it name(); an open one's is here
    if e.kind == CEnumKind::Open && !e.own_to_text {
        out.extend(quote! {
            impl #name {
                /// Its name: None for a value this version has none for.
                #[inline]
                pub fn name(self) -> Option<&'static str> { #n::name_of(<Self as #n::Names>::ENTRIES, self.0 as u64) }
            }
        });
    }
    if let Some(body) = to_text {
        out.extend(quote! {
            impl crate::util::ToText for #name {
                #[inline]
                fn to_text(&self, out: &mut crate::util::Printbuf) { #body }
            }
            crate::display_via_to_text!(#name);
        });
    }
    out
}

/// A type alias - C's typedef: c_typedef! { pub type NAME = TYPE; }, with
/// #[c("...")] for the C declarator where TYPE's canonical rendering isn't it.
#[proc_macro]
pub fn c_typedef(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_typedef!", || {
        let text = format!("c_typedef! {{ {input} }}");
        let t = one_item!(&text, Typedef, "pub type NAME = TYPE;");
        let (name, ty, record) = (format_ident!("{}", t.rust_ident()), tokens(&t.ty), record(&text, &[]));
        Ok(quote!(#[allow(non_camel_case_types)] pub type #name = #ty; #record))
    })
}

/// An array published under RCU that C reads:
///
///   c_rcu_array! { pub type DiskGroups = RcuArray<c::bch_disk_group_cpu>
///                  as struct bch_disk_groups_cpu { nr, entries }; }
///
/// Rust gets the alias; C the struct its buffer is - `struct NAME { struct
/// rcu_head rcu; size_t NR; [H's fields;] T ELEMS[]; }`, H's in place, H a
/// #[repr(C)] struct of the same file - checked against RcuArray's layout. A
/// field of the alias's type is, to C, `struct NAME __rcu *`.
#[proc_macro]
pub fn c_rcu_array(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_rcu_array!", || {
        let text = format!("c_rcu_array! {{ {input} }}");
        let a = one_item!(&text, RcuArray, "pub type ALIAS = RcuArray<T[, H]> as struct NAME { NR, ELEMS };");
        let (alias, ty) = (format_ident!("{}", a.alias), tokens(&a.ty));
        let nums: Vec<_> = ["NR", "HEAD", "TAIL"].iter().map(|n| format_ident!("{n}"))
            .map(|n| quote!(<#ty>::#n as u64))
            .collect();
        let record = record(&text, &nums);
        Ok(quote!(#[allow(non_camel_case_types)] pub type #alias = #ty; #record))
    })
}

/// A Rust type with the layout of a C type:
///
///   c_same! { struct bkey_s_c == BkeySC<'static> { k, v } }
///
/// Only a record: the generator asserts the C type has the Rust type's size
/// and alignment, and the listed fields its offsets - so Rust can pass one
/// where C has the other.
#[proc_macro]
pub fn c_same(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_same!", || {
        let text = format!("c_same! {{ {input} }}");
        let s = one_item!(&text, Same, "struct NAME == TYPE { fields }");
        let ty = tokens(&s.rust_ty);
        let mut nums = size_align(&ty);
        nums.extend(s.fields.iter().map(|f| format_ident!("{f}")).map(|f| quote!(::core::mem::offset_of!(#ty, #f) as u64)));
        Ok(record(&text, &nums))
    })
}

/// What Rust calls of a C header:
///
///   c_extern! {
///       pub fn bch2_trans_begin(trans: *mut BtreeTrans<'_>) -> u32;
///       pub static bch2_bkey_ops: [c::bkey_ops; 0];
///   }
///
/// Rust gets the extern "C" block; C the prototypes, generated into the
/// header the C ones are in - see CExtern. A static that isn't mut is C's
/// const, and [T; 0] its unsized array: `extern const struct bkey_ops
/// bch2_bkey_ops[];`.
#[proc_macro]
pub fn c_extern(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_extern!", || {
        let text = format!("c_extern! {{ {input} }}");
        let ex = one_item!(&text, Extern, "fns and statics");

        let mut decls = TokenStream::new();
        let mut asserts = TokenStream::new();
        for f in &ex.fns {
            let mut params = params(&f.params);
            if f.variadic {
                params.push(quote!(...));
            }
            let (cfg, name, ret) = (cfg_attr(&f.cfg), tokens(&f.name), ret(&f.ret));
            let must_use = f.must_use.then(|| quote!(#[must_use]));
            let cold = f.cold.then(|| quote!(#[cold]));
            decls.extend(quote!(#cfg #must_use #cold pub fn #name(#(#params),*) #ret;));
            asserts.extend(thin_pointers(&f.name, &f.cfg, &f.params, f.ret.as_deref()));
        }
        for s in &ex.statics {
            let (cfg, name, ty) = (cfg_attr(&s.cfg), tokens(&s.name), tokens(&s.ty));
            let m = s.mutable.then(|| quote!(mut));
            decls.extend(quote!(#cfg pub static #m #name: #ty;));
        }
        let (cfg, record) = (cfg_attr(&ex.cfg), record(&text, &[]));
        Ok(quote! {
            #cfg unsafe extern "C" { #decls }
            #cfg const _: () = { #asserts };
            #record
        })
    })
}

fn params(params: &[(String, String)]) -> Vec<TokenStream> {
    params.iter().map(|(n, ty)| (tokens(n), tokens(ty))).map(|(n, ty)| quote!(#n: #ty)).collect()
}

fn ret(ret: &Option<String>) -> Option<TokenStream> {
    ret.as_deref().map(tokens).map(|r| quote!(-> #r))
}

/// @ty, a field's type, at its struct's default instance - its lifetimes
/// 'static, its type parameters their defaults: for a check outside the
/// struct's generics, of what neither changes.
fn at_default(ty: &str, def: &cstruct::CStructDef) -> String {
    let mut out = String::new();
    let mut rest = ty;
    while let Some(i) = rest.find(|ch: char| ch == '\'' || ch.is_alphabetic() || ch == '_') {
        out.push_str(&rest[..i]);
        let start = i + usize::from(rest[i..].starts_with('\''));
        let len = rest[start..].find(|ch: char| !(ch.is_alphanumeric() || ch == '_')).unwrap_or(rest.len() - start);
        let word = &rest[i..start + len];
        let default = if start > i {
            def.lifetimes.iter().any(|l| l == word).then_some("'static")
        } else {
            def.type_params.iter().find(|p| p.name == word).map(|p| p.default.as_str())
        };
        out.push_str(default.unwrap_or(word));
        rest = &rest[start + len..];
    }
    out.push_str(rest);
    out
}

/// That each pointer fn @name passes C - parameters @params, return type
/// @ret - is one: a compile error if it's fat, a pointer to an unsized type,
/// whose metadata C has nowhere for. improper_ctypes can't say so: rustc
/// doesn't report it in what a proc macro generates. A pointer to an unsized
/// vstruct is to its header, Head<T>, which is C's struct.
fn thin_pointers(name: &str, cfg: &Option<cstruct::Cfg>, params: &[(String, String)], ret: Option<&str>) -> TokenStream {
    let cfg = cfg_attr(cfg);
    params.iter()
        .map(|(n, ty)| (n.as_str(), ty.as_str()))
        .chain(ret.map(|r| ("its return", r)))
        .filter(|(_, ty)| ty.trim_start().starts_with(['*', '&']))
        .map(|(n, ty)| {
            let msg = format!("{name}: {n}'s {ty} is a fat pointer, which C has no place for - \
                               a pointer to an unsized vstruct T is to Head<T>");
            let ty = tokens(ty);
            quote!(#cfg assert!(::core::mem::size_of::<#ty>() == ::core::mem::size_of::<*const u8>(), #msg);)
        })
        .collect()
}

/// A Rust fn C calls:
///
///   #[c_export]
///   pub fn bch2_xattr_validate(c: &Opaque<Fs>, k: BkeySC<'_>,
///                              from: &c::bkey_validate_context) -> c_int { .. }
///
/// C gets the prototype, as c_extern!'s, from the fn's signature - in
/// foo/bar_gen.h, for a fn in foo/bar.rs - each Rust type as C's
/// (cstruct.rs, c_export_type()): &Opaque<Fs> is struct bch_fs *,
/// BkeySC struct bkey_s_c. An extern "C" fn is the symbol C calls, made
/// #[no_mangle]. Any other is called through a definition with exactly C's
/// signature, made here, which transmutes what isn't C's type: kCFI checks
/// every call through a pointer against a hash of the callee's type, and a fn
/// in Rust's types (&Opaque<Fs>, BkeySC) never hashes as C's prototype
/// does.
///
/// A parameter C doesn't pass is #[lent(from)]: made for the call, by
/// types::Lent, of @from, which C does - a transaction's path table, of the
/// transaction.
///
///   #[c_export]
///   pub fn bch2_trans_unlock(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>) { .. }
///
/// And a static C reads - an ops table:
///
///   #[c_export]
///   pub static bch2_xattr_handlers: [*const c::xattr_handler; 6] = [..];
///
/// C gets its extern declaration, `extern const struct xattr_handler *const
/// bch2_xattr_handlers[6];`; Rust a CStatic<T>, which derefs to the T. A fn
/// pointer in one has to be to an extern "C" fn in C's types, for kCFI.
/// #[c("...")] after #[c_export] is the declaration, where the Rust type
/// doesn't say it - or C's isn't const, as attribute_group's attrs aren't.
#[proc_macro_attribute]
pub fn c_export(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[c_export]", || c_export_expand(attr.into(), item.into()))
}

/// @attrs without their #[@name(..)]s.
fn without_attr<'a>(attrs: &'a [syn::Attribute], name: &str) -> impl Iterator<Item = &'a syn::Attribute> {
    attrs.iter().filter(move |a| !a.path().is_ident(name))
}

fn c_export_expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    if !attr.is_empty() {
        return Err("takes no arguments".into());
    }
    match syn::parse2(item).map_err(|e| format!("on a fn or a static - {e}"))? {
        syn::Item::Fn(f) => c_export_fn(f),
        syn::Item::Static(s) => c_export_static(s),
        _ => Err("on a fn or a static".into()),
    }
}

/// The record's text of a #[c_export] item, @item: $crate, from a
/// macro_rules!, as the crate this is.
fn c_export_text(item: TokenStream) -> String {
    format!("#[c_export] {item}").replace("$crate", "crate")
}

/// A #[c_export] fn's parameters C doesn't pass - #[lent(from)], each made for
/// the call of the one C passes as @from (types/lib.rs, Lent) - their
/// attributes taken off: their index, name, the reference they're had as, and
/// @from.
fn take_lent(sig: &mut syn::Signature) -> Result<Vec<(usize, syn::Ident, syn::TypeReference, syn::Ident)>, String> {
    let mut lent = Vec::new();
    for (i, input) in sig.inputs.iter_mut().enumerate() {
        let syn::FnArg::Typed(p) = input else { continue };
        let Some(at) = p.attrs.iter().position(|a| a.path().is_ident("lent")) else { continue };
        let from = p.attrs.remove(at).parse_args::<syn::Ident>().map_err(|e| format!("#[lent(param)] - {e}"))?;
        let (syn::Pat::Ident(name), syn::Type::Reference(ty)) = (&*p.pat, &*p.ty) else {
            return Err("#[lent] is on a named parameter, of a reference".into());
        };
        lent.push((i, name.ident.clone(), ty.clone(), from));
    }
    Ok(lent)
}

fn c_export_fn(mut item: syn::ItemFn) -> Result<TokenStream, String> {
    let lent = take_lent(&mut item.sig)?;
    let is_lent = |i: usize| lent.iter().any(|l| l.0 == i);

    // What the record has: the signature, the body dropped - C's parameters
    let mut sig = item.sig.clone();
    sig.inputs = sig.inputs.into_iter().enumerate().filter(|(i, _)| !is_lent(*i)).map(|(_, p)| p).collect();
    let vis = &item.vis;
    let text = |attrs: Vec<&syn::Attribute>| c_export_text(quote!(#(#attrs)* #vis #sig {}));
    let e = parse_c_export(&text(item.attrs.iter().collect()))?;
    let f = &e.ex.fns[0];
    let cfg = cfg_attr(&f.cfg);
    // The record is under the fn's #[cfg], so it exists only where that holds,
    // and its text has none - which C would get as an #if: before rustc 1.89,
    // an attribute macro is passed the #[cfg] in front of it, already true,
    // and in C #[cfg(kernel)] is defined(kernel) - never.
    let record = record(&text(without_attr(&item.attrs, "cfg").collect()), &[]);
    let record = quote!(#cfg #record);

    // extern "C": the fn is C's symbol itself
    if e.extern_c {
        if !lent.is_empty() {
            return Err("#[lent] parameters are made by the definition C calls - not an extern \"C\" fn".into());
        }
        return Ok(quote!(#[unsafe(no_mangle)] #item #record));
    }

    let (name, params) = (tokens(&f.name), params(&f.params));
    let converted = f.params.iter().zip(&e.params).filter_map(|((n, c), r)| {
        let (n, c, r) = (tokens(n), tokens(c), tokens(r.as_ref()?));
        Some(quote!(let #n = ::core::mem::transmute::<#c, #r>(#n);))
    });
    let lent_name = |name: &syn::Ident| format_ident!("__lent_{name}");
    let lents = lent.iter().map(|(_, name, ty, from)| {
        let (l, elem, m) = (lent_name(name), &ty.elem, ty.mutability);
        quote!(let #m #l: #elem = crate::types::Lent::lent(#from);)
    });
    let mut c_args = f.params.iter().map(|(n, _)| tokens(n));
    let args = (0..item.sig.inputs.len()).map(|i| match lent.iter().find(|l| l.0 == i) {
        Some((_, name, ty, _)) => {
            let (l, m) = (lent_name(name), ty.mutability);
            quote!(&#m #l)
        }
        None => c_args.next().expect("a parameter for each of C's"),
    }).collect::<Vec<_>>();
    let call = quote!(self::#name(#(#args),*));
    let call = match (&f.ret, &e.ret) {
        (Some(c), Some(r)) => {
            let (c, r) = (tokens(c), tokens(r));
            quote!(::core::mem::transmute::<#r, #c>(#call))
        }
        _ => call,
    };
    let ret = ret(&f.ret);
    let thin = thin_pointers(&f.name, &None, &f.params, f.ret.as_deref());
    Ok(quote! {
        #item
        #cfg const _: () = {
            #[allow(unused_imports)]
            use crate::c;
            #[unsafe(no_mangle)]
            #[allow(unused_unsafe, clippy::transmute_ptr_to_ref)]
            unsafe extern "C" fn #name(#(#params),*) #ret {
                unsafe {
                    #(#converted)*
                    #(#lents)*
                    #call
                }
            }
            const _: () = { #thin };
        };
        #record
    })
}

/// A #[c_export] static: the symbol C reads - #[no_mangle], in a CStatic,
/// Sync for the pointers C's structs hold - and C's extern declaration of it,
/// as a c_extern! static's: `extern const struct foo NAME;`.
fn c_export_static(s: syn::ItemStatic) -> Result<TokenStream, String> {
    if !matches!(s.mutability, syn::StaticMutability::None) {
        return Err("expected static NAME: TYPE = EXPR; - not mut".into());
    }
    let syn::ItemStatic { attrs, vis, ident: name, ty, expr, .. } = &s;
    let text = |attrs: Vec<&syn::Attribute>| c_export_text(quote!(#(#attrs)* #vis static #name: #ty;));
    let e = cstruct::parse_c_export(&text(attrs.iter().collect()))?;
    // As a fn's record: under the static's #[cfg], not with it
    let cfg = cfg_attr(&e.ex.statics[0].cfg);
    let record = record(&text(without_attr(attrs, "cfg").collect()), &[]);

    // #[c("...")], C's declaration where the Rust type doesn't say it, is the
    // record's alone
    let attrs = without_attr(attrs, "c");

    // SAFETY, of the CStatic: a static C reads is immutable, and its only
    // parts that aren't Sync are its pointers - to other statics, and fns
    Ok(quote! {
        #[unsafe(no_mangle)]
        #[allow(non_upper_case_globals)]
        #(#attrs)* #vis static #name: crate::types::CStatic<#ty> = {
            let v = #expr;
            unsafe { crate::types::CStatic::new(v) }
        };
        #cfg #record
    })
}

/// Integer constants - C's #defines: c_const! { pub const NAME: u32 = EXPR; ... }
/// Each #define has its type's C type (`5U`); #[c_int] before one, int's, for
/// a constant C had as a plain number and Rust wants unsigned. The record is
/// the block's, with each constant's value, in order.
#[proc_macro]
pub fn c_const(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_const!", || {
        let text = format!("c_const! {{ {input} }}");
        let consts: Vec<_> = parse_items(&text)?.into_iter()
            .map(|i| match i { CItem::Const(c) => Some(c), _ => None })
            .collect::<Option<_>>()
            .filter(|c: &Vec<_>| !c.is_empty())
            .ok_or("expected pub const NAME: TYPE = EXPR;")?;
        let mut out = TokenStream::new();
        let mut nums = Vec::new();
        for c in &consts {
            let (name, ty, value) = (tokens(&c.name), tokens(&c.ty), tokens(&c.value));
            out.extend(quote!(#[allow(non_upper_case_globals, dead_code)] pub const #name: #ty = #value;));
            nums.push(quote!(#name as u64));
        }
        out.extend(record(&text, &nums));
        Ok(out)
    })
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
/// opcodes, and the generated header asserts C's are the same.
#[proc_macro]
pub fn c_ioctl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_ioctl!", || {
        let text = format!("c_ioctl! {{ {input} }}");
        let io = one_item!(&text, Ioctl, "NAME = _IOW(type, nr, ARG), ...");

        let mut out = TokenStream::new();
        let mut opcodes = Vec::new();
        for e in &io.entries {
            let (name, dir, ty, nr) = (format_ident!("{}", e.name), format_ident!("{}", e.dir), tokens(&e.ty), tokens(&e.nr));
            let (arg, opcode) = match e.arg.as_deref().map(tokens) {
                Some(a) => (a.clone(), quote!(crate::util::ioctl::#dir::<#a>(#ty, #nr))),
                None => (quote!(()), quote!(crate::util::ioctl::_IO(#ty, #nr))),
            };
            // _IOWR -> dir::WR, _IO -> dir::IO
            let dir_ty = format_ident!("{}", e.dir.strip_prefix("_IO").filter(|d| !d.is_empty()).unwrap_or("IO"));
            out.extend(quote! {
                #[allow(non_camel_case_types)]
                pub struct #name;
                impl crate::util::ioctl::Ioctl for #name {
                    const OPCODE: u32 = #opcode;
                    type Arg = #arg;
                    type Dir = crate::util::ioctl::dir::#dir_ty;
                }
            });
            opcodes.push(quote!(<#name as crate::util::ioctl::Ioctl>::OPCODE as u64));
        }
        out.extend(record(&text, &opcodes));
        Ok(out)
    })
}

/// Bitfields in a flags word: c_bitmask! { LE64_BITMASK(struct foo, field),
/// strip PREFIX_ { NAME(start, end), ... } }. Rust gets, on foo, name() and
/// set_name() for each - NAME without PREFIX_, lowercased - as C's NAME() and
/// SET_NAME(): the bits [start, end) of field, as a u64.
#[proc_macro]
pub fn c_bitmask(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_bitmask!", || bitmask_expand(input.into()))
}

fn bitmask_expand(input: TokenStream) -> Result<TokenStream, String> {
    let text = format!("c_bitmask! {{ {input} }}");
    let b = one_item!(&text, Bitmask, "LE64_BITMASK(struct foo, field), strip PREFIX_ { NAME(start, end), ... }");

    // An LE*_BITMASK field is a little-endian integer - Le32, Le64 - read and
    // written with get() and set(); a BITMASK one, native.
    let field = &b.field;
    let f = tokens(field);
    let (get, set) = if b.macro_name == "BITMASK" {
        (quote!(self.#f as u64), quote!(self.#f = new as _))
    } else {
        (quote!(u64::from(self.#f.get())), quote!(self.#f.set(new as _)))
    };

    // The runtime table of them (typeinfo.rs's BitmaskField): this struct
    // field's, as a const named for them - bch_sb_flags_0_BITMASKS - which
    // typeinfo.rs's registry must list, so that a new one can't be left out.
    let c_struct = b.c_type.trim_start_matches("struct ").trim();
    let table = format!("{c_struct}_{}_BITMASKS",
                        field.replace(|c: char| !c.is_alphanumeric(), "_").trim_end_matches('_'));
    // Each entry's struct as typeinfo names it, C's name: its TypeInfo's - a
    // family's type in a state has it as X<Whole>
    let info_ty = tokens(&match b.rust_type.split_once('<') {
        Some((x, _)) => format!("{x}<crate::util::vstructs::Whole>"),
        None         => b.rust_type.clone(),
    });
    let struct_name = quote!(<#info_ty as crate::typeinfo::TypeInfo>::INFO.name);
    let mut table_entries = Vec::new();

    let mut methods = TokenStream::new();
    let mut setters = TokenStream::new();
    for e in &b.entries {
        let m = b.method(&e.name);
        // set_128_bit_macs: the setter's set_ makes it a name already
        let set_m = match m.strip_prefix('_') {
            Some(s) if s.starts_with(|c: char| c.is_ascii_digit()) => s,
            _ => m.as_str(),
        };
        let (start, end) = (tokens(&e.start), tokens(&e.end));
        table_entries.push(quote! {
            crate::typeinfo::BitmaskField { struct_name: #struct_name, field: #field, name: #set_m,
                                            lo: (#start) as u8, hi: (#end) as u8 }
        });
        // C's LE*_BITMASK() defines NAME_MAX: the largest value it holds
        let (max, get_m) = (format_ident!("{}_MAX", m.to_uppercase()), format_ident!("{m}"));
        let (set_fn, update_fn) = (format_ident!("set_{set_m}"), format_ident!("update_{set_m}"));
        // One bit is a flag: bool, as the accessors on bindgen's structs had it.
        let one_bit = matches!((e.start.parse::<u32>(), e.end.parse::<u32>()), (Ok(s), Ok(e)) if e == s + 1);
        let (ty, from, to) = match one_bit {
            true => (quote!(bool), quote!(!= 0), quote!(as u64)),
            false => (quote!(u64), quote!(), quote!()),
        };
        methods.extend(quote! {
            #[allow(dead_code)]
            pub const #max: u64 = u64::MAX >> (64 - ((#end) - (#start)));
            #[allow(dead_code)]
            pub fn #get_m(&self) -> #ty {
                ((#get >> (#start)) & (u64::MAX >> (64 - ((#end) - (#start))))) #from
            }
        });
        // update_x(f): set_x(f(x())). XXX Deprecated when real field
        // projections arrive: x op= v through a projected place instead.
        setters.extend(quote! {
            #[allow(dead_code)]
            pub fn #set_fn(&mut self, v: #ty) {
                let mask = (u64::MAX >> (64 - ((#end) - (#start)))) << (#start);
                let new = (#get & !mask) | (((v #to) << (#start)) & mask);
                #set;
            }
            #[allow(dead_code)]
            pub fn #update_fn(&mut self, f: impl FnOnce(#ty) -> #ty) {
                self.#set_fn(f(self.#get_m()))
            }
        });
    }

    // A family's C struct, X__head: its state definitions, which have C's
    // fields inline and no DerefMut to it, get the setters too - the getters
    // they reach through their Deref
    let state_setters = b.rust_type.strip_suffix("__head").map(|family| {
        let state_impls = format_ident!("__{family}__state_impls");
        quote!(#state_impls! { #setters })
    });

    let (rust_type, table_id) = (tokens(&b.rust_type), format_ident!("{table}"));
    let msg = format!("{table}: not in typeinfo.rs's BITMASKS - add it");
    let record = record(&text, &[]);
    Ok(quote! {
        #record
        impl #rust_type { #methods #setters }
        #state_setters
        #[allow(non_upper_case_globals)]
        pub const #table_id: &[crate::typeinfo::BitmaskField] = &[#(#table_entries),*];
        const _: () = assert!(crate::typeinfo::bitmasks_listed(#table), #msg);
    })
}
