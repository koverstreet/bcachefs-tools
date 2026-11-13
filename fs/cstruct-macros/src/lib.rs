// SPDX-License-Identifier: GPL-2.0
//! The macros the types C once shared are written with - see fs/types/lib.rs.
//! Each parses its input with fs/types/cstruct.rs, so a mistake is a compile
//! error at the item, and expands to its Rust side:
//!
//!   #[derive(CStruct)]   a struct or union: Zeroable, and its
//!                        #[c_bitfield] members on it
//!   #[bitfield(uN)]      bitfield_struct::bitfield, and forwarders for the
//!                        struct that holds one in a #[c_bitfield] field
//!   c_xmacro! { .. }     an x-macro list: NAME!(cb), handing cb! the whole
//!                        list
//!   xmacro_select!       NAME!(xmacro_select [TOKEN => cb]): cb! handed
//!                        the rows with TOKEN in them - a flag in a flags
//!                        column
//!   c_bitmask! { .. }    bitfields in a flags word: a getter and a setter
//!                        on the struct for each
//!   c_ioctl! { .. }      ioctl numbers: a marker type for each, its opcode
//!                        and argument type
//!   #[bch_enum(..)]      an enum, open, closed or flags: the type that is,
//!                        and its values
//!   list_enum! { .. }    a #[bch_enum] with variants from lists
//!   atomic_bitfield! { .. }
//!                        a bitfield's value, and an atomic word of it
//!   tagged_union! { .. } a tagged union with a stable representation: C's
//!                        storage, the tag a Determinant of it, declared as
//!                        an enum, the arms its variants (tagged_union.rs)
//!   #[derive(TypeInfo)]  runtime type information, for kvdb (typeinfo.rs)
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
//!                        into it (prt.rs)
//!   #[args_stored_once]  an async fn as fn -> impl Future { async move }, so
//!                        its by-value arguments are stored once in its
//!                        future, not twice (args_stored_once.rs)
//!   #[in_place(Fut)]     an async fn whose future is named Fut, for a task
//!                        held in place, or a box's pool, by type (in_place.rs)
//!
//! On syn and quote - vendor/kernel-rust's: the kernel build compiles this
//! with a bare rustc.

use proc_macro2::{Delimiter, Group, Ident, Literal, Spacing, Span, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens};

// A proc-macro crate may export only its macros: the parser goes in a
// private module.
mod cstruct {
    include!("../../types/cstruct.rs");
}
use cstruct::{enum_tokens, expand_enum_list, generic_arg, parse_cstruct, parse_items, parse_list_enum,
              paste_concat, CEnumDef, CEnumKind, CEnumVariant, CItem, CXEntry};

mod args_stored_once;
mod atomic_bitfield;
mod in_place;
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

/// list_enum! @decl through @list - LIST!(__list_enum [decl]) - which calls
/// __list_enum back with its entries: { [decl] (entry), (entry), ... }.
fn through_list(list: &Ident, decl: TokenStream) -> TokenStream {
    quote!(crate::cstructs::c::#list!(cstruct_macros::__list_enum [#decl]);)
}

/// What a list hands through_list()'s callback - { [decl] (entry), (entry),
/// ... }: the declaration, and each entry's arguments.
fn list_callback(input: TokenStream) -> Result<(TokenStream, Vec<TokenStream>), String> {
    let mut trees = input.into_iter();
    let decl = match trees.next() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket => g.stream(),
        _ => return Err("expected [declaration] then the list's entries".into()),
    };
    let entries = trees
        .filter_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream()),
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

/// bch_opts' options, a field each: options.rs.
#[proc_macro_derive(Options, attributes(opt))]
pub fn derive_options(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Options)]", || options::derive(syn::parse(input)?))
}

/// `#[derive(CStruct)]`: a struct or union's Zeroable impl, the members of
/// a `#[c_bitfield]` field - one of a #[bitfield] type - forwarded to it,
/// and with `#[counted_by(FIELD)]` on a trailing `[T; 0]`, FIELD its length,
/// its FlexArray (vstructs.rs).
/// `#[type_name_of(NAME)]` is what #[type_name] leaves for the derives.
#[proc_macro_derive(CStruct, attributes(c_bitfield, type_name_of, counted_by))]
pub fn derive_cstruct(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(CStruct)]", || cstruct_expand(input.into()))
}

/// `#[type_name(NAME)]`, before the derives, on a struct whose name isn't
/// the type's: a family's definition, X__head, whose type is X<S>
/// (vstructs.rs), or a tagged union's storage. The derives get it as
/// #[type_name_of(NAME)] - after the item's other attributes, as a derive's
/// helper comes after the derive - and TypeInfo names it so.
#[proc_macro_attribute]
pub fn type_name(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[type_name]", || {
        let name: syn::Ident = syn::parse(attr)
            .map_err(|e| syn::Error::new(e.span(), "takes the type's name"))?;
        let mut item: syn::DeriveInput = syn::parse(item)?;
        item.attrs.push(syn::parse_quote!(#[type_name_of(#name)]));
        Ok(quote!(#item))
    })
}

/// `#[derive(TypeInfo)]`: runtime type information for a struct or union C
/// shares - see typeinfo.rs.
#[proc_macro_derive(TypeInfo, attributes(type_name_of))]
pub fn derive_type_info(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(TypeInfo)]", || typeinfo::derive(input.into()))
}

/// `#[derive(Init)]`: `T::init()`, an initializer that zeroes every field but
/// those with `#[init(EXPR)]` - see init.rs.
#[proc_macro_derive(Init, attributes(init, init_args, init_locked, placed_args, placed_at))]
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
    let def = parse_cstruct(&input)?;
    let name = &def.ident;

    // A #[c_bitfield] field's members, on the struct: its type's forwarders
    // (#[bitfield]), by the type's own path - a Cell's setting through &self.
    // Not a Volatile one's: that's a word, read and written whole.
    let mut forward = TokenStream::new();
    let (generics, over, _) = def.generics.split_for_impl();
    for f in def.fields.iter().filter(|f| f.bitfield) {
        let (bt, suffix) = match (generic_arg(&f.ty, "Cell"), generic_arg(&f.ty, "Volatile")) {
            (Some(t), _)    => (t, "_forward_cell"),
            (None, Some(_)) => continue,
            (None, None)    => (&f.ty, "_forward"),
        };
        let syn::Type::Path(syn::TypePath { qself: None, path }) = bt else {
            return Err(format!("{name}.{}: #[c_bitfield] is for a field of a #[bitfield] type", f.ident));
        };
        let mut fwd = path.clone();
        let last = fwd.segments.last_mut().expect("a path has a segment");
        last.ident = format_ident!("__{}{suffix}", last.ident);
        let field = &f.ident;
        forward.extend(quote!(#fwd!([#generics] #name #over, #field);));
    }
    let flex = vstructs::flex_array(&def)?;
    let zeroable = maybe_zeroable(&def);
    Ok(quote!(#forward #flex #zeroable))
}

/// Zeroable when every field is - as bindgen's C structs are, deriving
/// pin_init::MaybeZeroable, and the same way: the for<'__dummy> makes each
/// bound a condition on the impl, not an error where it doesn't hold. So a
/// struct holding a Rust type with invariants - a lock - isn't, and
/// #[derive(Init)] can't zero it. core's atomics are, as their integers are,
/// though pin_init has no impl for them, and the orphan rule allows none
/// here: their fields aren't bounds. Nor has it one for Cell, whose zeroes
/// are its T's: a Cell's bound is its T's.
fn maybe_zeroable(def: &cstruct::CStructDef) -> TokenStream {
    let mut generics = def.generics.clone();
    generics.make_where_clause().predicates.extend(def.fields.iter()
        .filter(|f| !is_core_atomic(&f.ty))
        .map(|f| generic_arg(&f.ty, "Cell").unwrap_or(&f.ty))
        .map(|ty| -> syn::WherePredicate { syn::parse_quote!(#ty: for<'__dummy> ::pin_init::Zeroable) }));
    let (generics, over, whr) = generics.split_for_impl();
    let name = &def.ident;
    quote! {
        // SAFETY: every field is Zeroable, and padding may be zero
        #[automatically_derived]
        unsafe impl #generics ::pin_init::Zeroable for #name #over #whr {}
    }
}

/// One of core::sync::atomic's integer, bool or pointer atomics, by any path
/// - or an array of one, of any depth - or data_structures/bitmap.rs's
/// AtomicULong, AtomicUsize as C's unsigned long.
pub(crate) fn is_core_atomic(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Array(a) => is_core_atomic(&a.elem),
        syn::Type::Path(p) => p.path.segments.last().is_some_and(|s| matches!(s.ident.to_string().as_str(),
            "AtomicBool" | "AtomicPtr" | "AtomicU8" | "AtomicI8" | "AtomicU16" | "AtomicI16" |
            "AtomicU32" | "AtomicI32" | "AtomicU64" | "AtomicI64" | "AtomicUsize" | "AtomicIsize" |
            "AtomicULong")),
        _ => false,
    }
}

/// bitfield_struct::bitfield - and __NAME_forward!([GENERICS] STRUCT, field),
/// which #[derive(CStruct)] invokes for a #[c_bitfield] field of this type:
/// each member's getter and setter on the struct that holds it, as C had a
/// bitfield's members on its struct. Not `_` members: those are padding.
///
/// Members are written as bilge has them, the type giving the width:
///
///   #[bitfield(u64)]
///   pub struct foo_bits { type_: u2, offset: u40, cached: bool, __pad: u3, .. }
///
/// `name: uN` is N bits, read and written as N rounded up to an integer -
/// offset above is a u64; `name: bool` is one bit. A member of any other type -
/// an enum - gives its width, #[bits(N)], as bitfield-struct has it; a uN or
/// bool member's #[bits(..)] gives only its options, as #[bits(default = 1)]
/// does. Members are pub, as C's are, unless written otherwise, and their
/// forwarders the same - pub(self), for one only its own module may set, has
/// none.
///
/// With #[derive(CStruct)], the bitfield is itself the struct - C's `struct
/// bch_extent_ptr { __u64 type:1, ...; }`. `align = 8` aligns it to 8, which a
/// u64 bitfield needs said on i386 (Align8U64).
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

/// `#[in_place(Fut)]` on an async fn: the same fn, its future named Fut - a
/// type alias impl Trait the fn defines - for a pool to be sized for its
/// task's box; its args stored
/// once, as #[args_stored_once] stores them. See in_place.rs.
#[proc_macro_attribute]
pub fn in_place(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[in_place]", || in_place::in_place(syn::parse(attr)?, syn::parse(item)?))
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
    let mut out = TokenStream::new();
    let ident = format_ident!("{name}");
    // Each member, not padding: its name, setter and updater, type, and
    // visibility
    let members: Vec<_> = s.item.fields.iter()
        .map(|f| (f.ident.as_ref().expect("named members").to_string(), &f.ty))
        .filter(|(m, _)| !m.starts_with('_'))
        .map(|(m, ty)| {
            let vis = s.vis.iter().find(|(n, _)| *n == m).map_or("pub", |(_, v)| v.as_str());
            (format_ident!("{m}"), format_ident!("set_{m}"), format_ident!("update_{m}"), ty.to_token_stream(),
             tokens(vis))
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

    if !s.c_struct {
        // A member of a struct: its getters and setters for that struct -
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
                ([$($g:tt)*] $s:ty, $field:ident) => { impl $($g)* $s { #(#fns)* } };
            }
            #[allow(unused_imports)]
            pub(crate) use #fwd;
            #[allow(unused_macros)]
            macro_rules! #fwd_cell {
                ([$($g:tt)*] $s:ty, $field:ident) => { impl $($g)* $s { #(#cell_fns)* } };
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
    let (info, plain) = (typeinfo::info(&ident, &ident, "Struct", &[]), vstructs::plain(&ident, &[tokens(&held)]));
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
    /// `align = 8`: aligned to 8, as a u64 isn't on i386
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
    let mut align = None;
    rest.retain(|a| match key_value(a) {
        Some((k, v)) if k == "align" => { align = Some(v); false }
        _ => true,
    });
    let align8 = match align.as_deref() {
        None => false,
        Some("8") if storage == "u64" => true,
        Some(n) => return Err(format!("align = {n}: there's only align = 8, on u64 storage")),
    };
    Ok(BitfieldArgs { storage, rest, align8 })
}

/// A #[bitfield] struct, its attributes that are ours taken out: CStruct and
/// TypeInfo from its derive - C's struct, and we give the TypeInfo.
struct BitfieldStruct {
    name:     String,
    /// The struct, for bitfield-struct: its members canonical
    item:     syn::ItemStruct,
    /// #[derive(CStruct)]: the bitfield is C's struct, not a member of one
    c_struct: bool,
    /// Each member's name and visibility
    vis:      Vec<(String, String)>,
}

fn bitfield_struct(item: TokenStream) -> Result<BitfieldStruct, String> {
    let mut item: syn::ItemStruct = syn::parse2(item).map_err(|e| format!("expected a struct - {e}"))?;
    let name = item.ident.to_string();
    let mut c_struct = false;
    let mut attrs = Vec::new();
    for a in std::mem::take(&mut item.attrs) {
        if a.path().is_ident("derive") {
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
    let syn::Fields::Named(members) = &mut item.fields else {
        return Err(format!("{name}: expected members, `name: type`"));
    };
    let mut vis = Vec::new();
    for m in members.named.iter_mut() {
        bitfield_member(m)?;
        vis.push((m.ident.as_ref().unwrap().to_string(), m.vis.to_token_stream().to_string()));
    }
    Ok(BitfieldStruct { name, item, c_struct, vis })
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

/// An x-macro list: c_xmacro! { NAME { (args), ... } }: a macro,
/// NAME!(cb), which expands to cb! { (args), ... } - and NAME!(cb [..]),
/// to cb! { [..] (args), ... }: a list_enum! built from lists passes on the
/// variants so far. cb may be a path.
///
/// An entry can be another list, SUB(), or another list's entries mapped,
/// SUB((params) => (args)). Then NAME!(cb) is a chain of macros, one per
/// list it splices in: each is SUB! handed the entries so far, which adds
/// SUB's, mapped, and those after, and calls the next; the last calls cb!. They're named by path, through
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

/// c_xmacro!'s expansion: NAME!(cb), which calls cb! with the list's rows.
fn xmacro(input: TokenStream) -> Result<TokenStream, String> {
    let text = format!("c_xmacro! {{ {input} }}");
    let x = one_item!(&text, XMacro, "NAME { (args), ... }");
    let name = format_ident!("{}", x.name);
    // The list: the brace group after NAME, past any attributes
    let list = input.into_iter()
        .filter_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g.stream()),
            _ => None,
        })
        .last()
        .expect("parse_items found the list");
    let entries: Vec<TokenStream> = comma_list(list).into_iter().map(|e| e.into_iter().collect()).collect();

    let mut out = TokenStream::new();
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
                let template = paste_concat(&m.template)?;
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

/// An enum some of whose variants are built from lists - `LIST(params) =>
/// template`, a variant for each entry - which an enum declaration can't
/// say: #[bch_enum]'s, written as
///
///   list_enum! { #[open] pub enum NAME: REPR { .., LIST(params) => template, .. } }
#[proc_macro]
pub fn list_enum(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("list_enum!", || {
        let input = TokenStream::from(input);
        if !from_lists(&input) {
            return enum_decl::declaration(input);
        }
        let e = parse_list_enum(input.clone())?;
        Ok(match e.from.first() {
            Some((_, f)) => through_list(&f.list, input),
            None => enum_gen(&e),
        })
    })
}

/// Whether list_enum! @input has variants from a list: `LIST(params) => ..`.
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

/// `#[bch_enum(closed|open|flags)]` on `#[repr(uN)] pub enum NAME { .. }`:
/// what its values can be, and the type that is.
///
///   closed  a Rust enum
///   open    a newtype over the integer, the values its constants: a value
///           from outside can be anything, so it's Plain, and a word a
///           Volatile can hold
///   flags   a bitflags type
///
/// Any of them can be a #[bitfield]'s member, as C's `enum btree_id
/// btree_id:8`: it has the const into_bits() bitfield-struct converts with,
/// and from_bits() - for flags, bitflags' from_bits() is the checked one, so
/// a member names `from = T::from_bits_retain`. A closed enum's member only
/// ever holds one of its values, as its type says; from_bits() of anything
/// else is memory corruption, and panics.
///
/// A variant prints and parses as its name, or its #[label("..")] - past
/// `#[prefix(P)]`, for names written with it. `#[nr]`: Self::NR, past the
/// largest value - a number, not one of its values. `#[own_to_text]`: it
/// prints as more than its name, by its own ToText.
///
/// A variant is written with its doc, and its columns - `#[arm_columns(..)]`,
/// enum_decl.rs - each made a const fn of the enum:
///
///   #[bch_enum(open)]
///   #[prefix(BTREE_ID_)] #[nr]
///   #[arm_columns(flags: BtreeFlags, holds: set(BkeyType))]
///   #[repr(u32)]
///   pub enum Btree {
///       /// File data extent pointers and reservations
///       #[flags(extents, snapshots, data, trans_triggers)]
///       #[holds(whiteout, extent, reservation, ..)]
///       extents = 0,
///       ...
///   }
#[proc_macro_attribute]
pub fn bch_enum(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[bch_enum]", || {
        let kind: Ident = syn::parse(attr).map_err(|_| "takes closed, open or flags")?;
        let item: syn::ItemEnum = syn::parse(item).map_err(|e| e.to_string())?;
        let (mut repr, mut attrs) = (None, Vec::new());
        for a in &item.attrs {
            match a.path().is_ident("repr") {
                true  => repr = Some(a.parse_args::<Ident>().map_err(|e| format!("{}: {e}", item.ident))?),
                false => attrs.push(a),
            }
        }
        let repr = repr.ok_or_else(|| format!("{}: #[repr(uN)]: its integer type", item.ident))?;
        let (vis, name, variants) = (&item.vis, &item.ident, &item.variants);
        enum_decl::declaration(quote!(#[#kind] #(#attrs)* #vis enum #name: #repr { #variants }))
    })
}

/// list_enum!'s list's callback: { [decl] (entry), (entry), ... } - the enum
/// with its first list's variants in their place: through the next list, if
/// it has another, or generated.
#[doc(hidden)]
#[proc_macro]
pub fn __list_enum(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("list_enum!", || {
        let (decl, entries) = list_callback(input.into())?;
        let mut e = parse_list_enum(decl)?;
        expand_enum_list(&mut e, &entries).map_err(|err| format!("{}: {err}", e.name))?;
        Ok(match e.from.first() {
            Some((_, f)) => through_list(&f.list, enum_tokens(&e)),
            None => enum_gen(&e),
        })
    })
}

/// Enum @e: the type, its values, and their names.
fn enum_gen(e: &CEnumDef) -> TokenStream {
    let repr = &e.repr;
    let names: Vec<&Ident> = e.variants.iter().map(|v| &v.name).collect();

    // A variant with no value counts from the last that has one - not from
    // the one before it, which would make a const chain as long as the enum:
    // too deep for rustc, at PersistentCounter's hundreds. A closed
    // enum's is Rust's own, the one before it plus one.
    let mut out = TokenStream::new();
    let mut values = Vec::new();
    // as plain integers, each by its name: the flags' bits module
    let mut raw_values = Vec::new();
    let mut base = None;
    let mut n = 0;
    for (v, CEnumVariant { value, .. }) in names.iter().zip(&e.variants) {
        let k = Literal::usize_unsuffixed(n);
        raw_values.push(match (value, base) {
            (Some(x), _) => quote!(#x),
            (None, Some(b)) => quote!(#b + #k),
            (None, None) => quote!(#k),
        });
        values.push(match (value, base, e.kind) {
            (Some(x), ..) => Some(quote!(#x)),
            (None, _, CEnumKind::Closed) => None,
            (None, Some(b), CEnumKind::Open) => Some(quote!(Self::#b.0 + #k)),
            (None, Some(b), CEnumKind::Flags) => Some(quote!(Self::#b.bits() + #k)),
            (None, None, _) => Some(quote!(#k)),
        });
        if value.is_some() {
            base = Some(v);
            n = 0;
        }
        n += 1;
    }
    // Past the largest, not the last: the values needn't be in order - the
    // counters' stable ids aren't
    let nr = e.nr.then(|| {
        let int = names.iter().map(|v| match e.kind {
            CEnumKind::Closed => quote!(Self::#v as #repr),
            CEnumKind::Open   => quote!(Self::#v.0),
            CEnumKind::Flags  => quote!(Self::#v.bits()),
        });
        quote!({ let mut n = 0; #(if #int + 1 > n { n = #int + 1; })* n })
    });

    let name = &e.name;
    let vis = quote!(pub);
    let attrs = e.docs.iter().map(|d| quote!(#[doc = #d]));
    let attrs = quote!(#(#attrs)*);
    let bits_mod = format_ident!("{name}_bits");
    let docs: Vec<_> = e.variants.iter().map(|v| { let d = &v.docs; quote!(#(#[doc = #d])*) }).collect();
    let discs = values.iter().map(|v| v.as_ref().map(|x| quote!(= #x)));
    let values = values.iter().map(|v| v.as_ref().expect("only a closed enum's goes unwritten"));
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
                #[allow(non_camel_case_types)]
                #attrs
                #vis enum #name { #(#docs #labels #names #discs,)* }
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

            // The flags as plain integers, for a type-level constant: one
            // that calls bitflags' const fns can cycle in rustc's
            // coherence checking (Debian's 1.97.1)
            #[allow(non_snake_case, non_upper_case_globals, dead_code)]
            pub mod #bits_mod {
                #[allow(unused_imports)]
                use {super::*, crate::c::*};
                #(pub const #names: #repr = #raw_values;)*
            }
        },
    });
    if let Some(nr) = &nr {
        out.extend(quote!(impl #name { pub const NR: #repr = #nr; }));
    }

    out.extend(names_gen(e, name));
    out.extend(typeinfo::info(name, name, "Enum", &[]));

    // Zeroed is a value of it - so a struct holding one can be Zeroable:
    // any integer is an open enum's or flags', a closed enum's only if a
    // variant is 0 - as written, or the first, counting from 0
    let has_zero = e.kind != CEnumKind::Closed || e.variants.iter().enumerate()
        .any(|(i, v)| match &v.value {
            None    => i == 0,
            Some(x) => enum_decl::int_of(x) == Some(0),
        });
    if has_zero {
        out.extend(quote! {
            // SAFETY: zero is a value of it
            unsafe impl ::pin_init::Zeroable for #name {}
        });
    }

    out
}

/// An enum from an x-macro list - with labelled variants - is named by the
/// list: util/names.rs's Names, its labelled variants' names and values, a
/// flags type's bits. An open or flags one prints through it - ToText, and
/// Display through that - unless it's #[own_to_text]; a closed one derives
/// ToText.
fn names_gen(e: &CEnumDef, name: &Ident) -> TokenStream {
    let labelled: Vec<(String, &Ident)> = e.variants.iter()
        .filter_map(|v| e.label(v).map(|l| (l, &v.name)))
        .collect();
    if labelled.is_empty() {
        return TokenStream::new();
    }

    let n = quote!(crate::util::names);
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

/// ioctl numbers, C's _IO*() #defines:
///
///   c_ioctl! {
///       BCH_IOCTL_DISK_ADD = _IOW(0xbc, 4, c::bch_ioctl_disk),
///       BCHFS_IOC_REINHERIT_ATTRS = _IOR(0xbc, 64, *const core::ffi::c_char),
///   }
///
/// Rust gets a marker type for each, named as the #define, implementing
/// util::ioctl::Ioctl: the opcode, made by util::ioctl's _IO*() from the
/// target's own constants, and the argument type.
#[proc_macro]
pub fn c_ioctl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("c_ioctl!", || {
        let text = format!("c_ioctl! {{ {input} }}");
        let io = one_item!(&text, Ioctl, "NAME = _IOW(type, nr, ARG), ...");

        let mut out = TokenStream::new();
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
        }
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
    // Each entry's struct as typeinfo names it, its TypeInfo's - a
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
    Ok(quote! {
        impl #rust_type { #methods #setters }
        #state_setters
        #[allow(non_upper_case_globals)]
        pub const #table_id: &[crate::typeinfo::BitmaskField] = &[#(#table_entries),*];
        const _: () = assert!(crate::typeinfo::bitmasks_listed(#table), #msg);
    })
}
