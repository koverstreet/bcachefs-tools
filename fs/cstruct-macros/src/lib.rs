// SPDX-License-Identifier: GPL-2.0
//! The macros our types are written with - see fs/types/lib.rs.
//! Each parses its input with fs/types/cstruct.rs, so a mistake is a compile
//! error at the item, and expands to its Rust side:
//!
//!   #[derive(MaybeZeroable)]
//!                        a struct or union: Zeroable if its fields are
//!   #[derive(BitfieldMembers)]
//!                        a #[members] field's bitfield members, on the
//!                        struct
//!   #[derive(FlexArray)] #[counted_by] on a trailing [T; 0]: its FlexArray
//!                        (vstructs.rs)
//!   #[bitfield(uN)]      bitfield_struct::bitfield, and forwarders for the
//!                        struct that holds one in a #[members] field
//!   xmacro! { .. }       an x-macro list: NAME!(cb), handing cb! the whole
//!                        list
//!   xmacro_select!       NAME!(xmacro_select [TOKEN => cb]): cb! handed
//!                        the rows with TOKEN in them - a flag in a flags
//!                        column
//!   ioctl! { .. }        ioctl numbers: a marker type for each, its opcode
//!                        and argument type
//!   #[bch_enum(..)]      an enum, open, closed or flags: the type that is,
//!                        and its values
//!   atomic_bitfield! { .. }
//!                        a bitfield's value, and an atomic word of it
//!   tagged_union! { .. } a tagged union with a stable representation: its
//!                        storage, the tag a Determinant of it, declared as
//!                        an enum, the arms its variants (tagged_union.rs)
//!   #[derive(TypeInfo)]  runtime type information, for kvdb (typeinfo.rs)
//!   #[derive(Plain)]     util/vstructs.rs's Plain, checked at compile time;
//!                        a #[bitfield] is Plain too (vstructs.rs)
//!   #[derive(View)]      a projection for each field on a View of the type:
//!                        its fields read as zero past a short value's end
//!                        (vstructs.rs)
//!   #[unsized_vstruct]   on a struct ending in a flexible array: a family
//!                        of definitions, one per state, and X<S> picking
//!                        one (vstructs.rs)
//!   #[grown(from = F)]   on the struct of a type that grew: its family - the
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
//! On syn and quote - os/linux_kernel/vendor's: the kernel build compiles this
//! with a bare rustc.

use proc_macro2::{Delimiter, Group, Ident, Literal, Spacing, Span, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens};

// A proc-macro crate may export only its macros: the parser goes in a
// private module.
mod cstruct {
    include!("../../types/cstruct.rs");
}
use cstruct::{generic_arg, int_of, parse_cstruct, parse_ioctl, parse_xmacro, split_commas, EnumDef,
              EnumKind, EnumVariant, XEntry};

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

/// bch_opts' options, a field each: options.rs.
#[proc_macro_derive(Options, attributes(opt))]
pub fn derive_options(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Options)]", || options::derive(syn::parse(input)?))
}

/// `#[derive(MaybeZeroable)]`: a struct or union's Zeroable impl, if every
/// field is - see maybe_zeroable().
/// `#[type_name_of(NAME)]` is what #[type_name] leaves for the derives.
#[proc_macro_derive(MaybeZeroable, attributes(type_name_of))]
pub fn derive_maybe_zeroable(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(MaybeZeroable)]", || Ok(maybe_zeroable(&parse_cstruct(&input.into())?)))
}

/// `#[derive(BitfieldMembers)]`: the members of a `#[members]` field - one of
/// a #[bitfield] type - on the struct, forwarded to it.
#[proc_macro_derive(BitfieldMembers, attributes(members))]
pub fn derive_bitfield_members(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(BitfieldMembers)]", || bitfield_members(input.into()))
}

/// `#[derive(FlexArray)]`: util/vstructs.rs's FlexArray, from
/// `#[counted_by(FIELD)]` on a trailing `[T; 0]`, FIELD its length - see
/// vstructs.rs.
#[proc_macro_derive(FlexArray, attributes(counted_by))]
pub fn derive_flex_array(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(FlexArray)]", || vstructs::flex_array(&parse_cstruct(&input.into())?))
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

/// `#[derive(TypeInfo)]`: runtime type information for a struct or union, for
/// kvdb - see typeinfo.rs.
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

fn bitfield_members(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input)?;
    let name = &def.ident;
    if !def.fields.iter().any(|f| f.bitfield) {
        return Err(format!("{name}: no #[members] field"));
    }

    let mut forward = TokenStream::new();
    let (generics, over, _) = def.generics.split_for_impl();
    for f in def.fields.iter().filter(|f| f.bitfield) {
        if let Some(fwd) = members_forwarder(name, f)? {
            let field = &f.ident;
            forward.extend(quote!(#fwd!([#generics] #name #over, #field);));
        }
    }
    Ok(forward)
}

/// A #[members] field's members, on the struct: its type's forwarder
/// (#[bitfield]), by the type's own path - a Cell's setting through &self.
/// None for a Volatile one: that's a word, read and written whole.
pub(crate) fn members_forwarder(name: &Ident, f: &cstruct::FieldDef) -> Result<Option<syn::Path>, String> {
    let (bt, suffix) = match (generic_arg(&f.ty, "Cell"), generic_arg(&f.ty, "Volatile")) {
        (Some(t), _)    => (t, "_forward_cell"),
        (None, Some(_)) => return Ok(None),
        (None, None)    => (&f.ty, "_forward"),
    };
    let syn::Type::Path(syn::TypePath { qself: None, path }) = bt else {
        return Err(format!("{name}.{}: #[members] is for a field of a #[bitfield] type", f.ident));
    };
    let mut fwd = path.clone();
    let last = fwd.segments.last_mut().expect("a path has a segment");
    last.ident = format_ident!("__{}{suffix}", last.ident);
    Ok(Some(fwd))
}

/// Zeroable when every field is - pin_init's #[derive(MaybeZeroable)], done
/// the same way: the for<'__dummy> makes each
/// bound a condition on the impl, not an error where it doesn't hold. So a
/// struct holding a Rust type with invariants - a lock - isn't, and
/// #[derive(Init)] can't zero it. core's atomics are, as their integers are,
/// though pin_init has no impl for them, and the orphan rule allows none
/// here: their fields aren't bounds. Nor has it one for Cell, whose zeroes
/// are its T's: a Cell's bound is its T's.
fn maybe_zeroable(def: &cstruct::StructDef) -> TokenStream {
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
/// which #[derive(BitfieldMembers)] invokes for a #[members] field of this type:
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
/// does. Members are pub unless written otherwise, and their
/// forwarders the same - pub(self), for one only its own module may set, has
/// none.
///
/// Storage LeN is a uN, little-endian - an on-disk word: #[bitfield(Le64)].
/// `align = 8` aligns it to 8, which a u64 bitfield needs said on i386
/// (Align8U64).
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

/// `#[unsized_vstruct]`, on the definition of a struct ending in a flexible
/// array - or `#[unsized_vstruct(tail = [E])]`, `tail = NAME: [E]`, saying
/// what follows its header: the family of its definitions, one per state -
/// the one written, renamed X__head, and each state's, a DST of the header
/// and its tail -
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
/// `tail = NAME: [E]`, on the definition of a header and an array it counts
/// in elements: its family, the count private in each state's definition -
/// see vstructs.rs.
#[proc_macro_attribute]
pub fn counted(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[counted]", || vstructs::counted(attr.into(), item.into()))
}

/// `#[whole]`, on the definition of a type every valid value of which has
/// all of it - a superblock section or journal entry of fixed fields: its
/// family, every state all of it, at the value's length - see vstructs.rs.
#[proc_macro_attribute]
pub fn whole(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[whole]", || vstructs::whole(attr.into(), item.into()))
}

fn bitfield_expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let args = bitfield_args(attr)?;
    let item = bitfield_struct(item)?;
    let name = &item.ident;
    let mut out = TokenStream::new();
    // Each member, not padding: its name, setters and updater, type, and
    // visibility
    let members: Vec<_> = item.fields.iter()
        .map(|f| (f.ident.as_ref().expect("named members").to_string(), f))
        .filter(|(m, _)| !m.starts_with('_'))
        .map(|(m, f)| (format_ident!("{m}"), format_ident!("set_{m}"), format_ident!("set_{m}_checked"),
                       format_ident!("update_{m}"), &f.ty, &f.vis))
        .collect();
    // pub(self): its module's, which has the bitfield itself
    let outside = |vis: &syn::Visibility| !matches!(vis, syn::Visibility::Restricted(r)
                                                     if r.in_token.is_none() && r.path.is_ident("self"));

    // update_x(f): set_x(f(x())), each member's, as visible as its setter -
    // bitfield_struct's, which has the getter and setter but not this.
    // XXX Deprecated when real field projections arrive: x op= v through a
    // projected place instead.
    let update = members.iter().map(|(m, set, _, update, ty, vis)| quote! {
        #vis fn #update(&mut self, f: impl FnOnce(#ty) -> #ty) { self.#set(f(self.#m())) }
    });
    // An integer member's NAME_MAX, the largest value it holds
    let is_int = |ty: &syn::Type| matches!(ty, syn::Type::Path(p)
        if p.path.get_ident().is_some_and(|i| matches!(i.to_string().as_str(), "u8" | "u16" | "u32" | "u64")));
    let max = item.fields.iter()
        .filter(|f| !f.ident.as_ref().expect("named members").to_string().starts_with('_') && is_int(&f.ty))
        .map(|f| {
            let (ty, vis) = (&f.ty, &f.vis);
            let max = format_ident!("{}_MAX", f.ident.as_ref().expect("named members").to_string().to_uppercase());
            let width = member_width(f)?;
            Ok(quote!(#vis const #max: #ty = #ty::MAX >> (#ty::BITS - #width);))
        })
        .collect::<Result<Vec<_>, String>>()?;
    out.extend(quote!(impl #name { #(#update)* #(#max)* }));

    // For a struct holding one in a #[members] field: its getters and
    // setters for that struct - not pub(self)'s
    let fns = members.iter().filter(|(.., vis)| outside(vis))
        .map(|(m, set, set_checked, update, ty, vis)| quote! {
            #vis fn #m(&self) -> #ty { let b = self.$field; b.#m() }
            #vis fn #set(&mut self, v: #ty) { let mut b = self.$field; b.#set(v); self.$field = b; }
            // bitfield_struct's name, which for a type_ is set_type__checked
            #[allow(non_snake_case)]
            #vis fn #set_checked(&mut self, v: #ty) -> Result<(), ()> {
                let mut b = self.$field; b.#set_checked(v)?; self.$field = b; Ok(())
            }
            #vis fn #update(&mut self, f: impl FnOnce(#ty) -> #ty) { self.#set(f(self.#m())) }
        }).collect::<Vec<_>>();
    let cell_fns = members.iter().filter(|(.., vis)| outside(vis))
        .map(|(m, set, set_checked, update, ty, vis)| quote! {
            #vis fn #m(&self) -> #ty { self.$field.get().#m() }
            #vis fn #set(&self, v: #ty) { let mut b = self.$field.get(); b.#set(v); self.$field.set(b); }
            #[allow(non_snake_case)]
            #vis fn #set_checked(&self, v: #ty) -> Result<(), ()> {
                let mut b = self.$field.get(); b.#set_checked(v)?; self.$field.set(b); Ok(())
            }
            #vis fn #update(&self, f: impl FnOnce(#ty) -> #ty) { self.#set(f(self.#m())) }
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

    let (storage, rest) = (&args.storage, &args.rest);
    // What it holds - its repr, else the storage integer - and nothing else:
    // so it's Plain
    let held = rest.iter()
        .find_map(|a| key_value(a).filter(|(k, _)| k == "repr").map(|(_, v)| v))
        .unwrap_or_else(|| storage.to_token_stream());
    // TypeInfo: each member but padding, as its bits of the word - the
    // storage integer, in its repr's byte order. A u128's aren't: kvdb's
    // integers are up to 8 bytes
    let bytes = match storage.to_string().as_str() {
        "u8" => Some(1u8),
        "u16" => Some(2),
        "u32" => Some(4),
        "u64" => Some(8),
        _ => None,
    };
    let little = syn::parse2::<syn::Type>(held.clone()).ok()
        .and_then(|t| match t {
            syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        })
        .is_some_and(|r| matches!(r.as_str(), "Le16" | "Le32" | "Le64"));
    let endian = format_ident!("{}", if little { "Little" } else { "Native" });
    let mut lo = 0u32;
    let mut member_info = Vec::new();
    for (f, bytes) in item.fields.iter().filter_map(|f| Some((f, bytes?))) {
        let m = f.ident.as_ref().expect("named members");
        let hi = lo + member_width(f)?;
        if !m.to_string().starts_with('_') {
            let (m, lo, hi) = (m.to_string(), lo as u8, hi as u8);
            member_info.push(quote! {
                crate::typeinfo::FieldInfo {
                    name: #m,
                    offset: 0,
                    kind: crate::typeinfo::FieldKind::Bits {
                        bytes: #bytes, endian: crate::typeinfo::Endian::#endian, lo: #lo, hi: #hi,
                    },
                }
            });
        }
        lo = hi;
    }
    let shape = quote!(Bitfield(crate::typeinfo::Endian::#endian));
    let (info, plain) = (typeinfo::info(name, name, shape, &member_info), vstructs::plain(name, &[held]));
    // The struct as tokens, not text: an error in a member points at it
    Ok(quote! {
        #out
        #[::bitfield_struct::bitfield(#storage #(, #rest)*)]
        #item
        #info
        #plain
        // SAFETY: Plain - its integer, any bytes of which are one
        unsafe impl ::pin_init::Zeroable for #name {}
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
    storage: Ident,
    /// The rest of bitfield-struct's - with the repr our shorthands say
    rest:    Vec<TokenStream>,
}

/// `KEY = VALUE`: KEY, and VALUE.
fn key_value(toks: &TokenStream) -> Option<(Ident, TokenStream)> {
    match &toks.clone().into_iter().collect::<Vec<_>>()[..] {
        [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' && !v.is_empty() =>
            Some((k.clone(), v.iter().cloned().collect())),
        _ => None,
    }
}

fn bitfield_args(attr: TokenStream) -> Result<BitfieldArgs, String> {
    let mut rest: Vec<TokenStream> = comma_list(attr).into_iter().map(|a| a.into_iter().collect()).collect();
    let mut storage = match rest.first().map(|a| a.clone().into_iter().collect::<Vec<_>>()).as_deref() {
        Some([TokenTree::Ident(s)]) => s.clone(),
        _ => return Err("expected the storage type first: #[bitfield(uN, ..)], #[bitfield(LeN, ..)]".into()),
    };
    rest.remove(0);
    let mut align = None;
    rest.retain(|a| match key_value(a) {
        Some((k, v)) if k == "align" => { align = Some(v); false }
        _ => true,
    });
    let mut repr = |ty: TokenStream, from: &str, into: &str| {
        let (from, into) = (format_ident!("{from}"), format_ident!("{into}"));
        rest.extend([quote!(repr = #ty), quote!(from = #ty::#from), quote!(into = #ty::#into)]);
    };
    // LeN: the uN, little-endian - as on disk
    if let Some(bits) = storage.to_string().strip_prefix("Le") {
        repr(quote!(crate::types::#storage), "new", "to_int");
        storage = format_ident!("u{bits}");
    }
    match align {
        None => {}
        Some(n) if storage == "u64" && syn::parse2::<syn::LitInt>(n.clone()).is_ok_and(|n| n.base10_digits() == "8") =>
            repr(quote!(crate::types::Align8U64), "from_u64", "to_u64"),
        Some(n) => return Err(format!("align = {n}: there's only align = 8, on u64 storage")),
    }
    Ok(BitfieldArgs { storage, rest })
}

/// A #[bitfield] struct, for bitfield-struct: its members canonical, and
/// TypeInfo out of its derive - we give the TypeInfo.
fn bitfield_struct(item: TokenStream) -> Result<syn::ItemStruct, String> {
    let mut item: syn::ItemStruct = syn::parse2(item).map_err(|e| format!("expected a struct - {e}"))?;
    let name = item.ident.clone();
    let mut attrs = Vec::new();
    for a in std::mem::take(&mut item.attrs) {
        if a.path().is_ident("derive") {
            let derives = a.parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                .map_err(|e| format!("{name}: {e}"))?;
            let kept: Vec<_> = derives.into_iter()
                .filter(|p| !p.segments.last().is_some_and(|s| s.ident == "TypeInfo"))
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
    for m in members.named.iter_mut() {
        bitfield_member(m)?;
    }
    Ok(item)
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
        let (width, rust_ty) = bilge_width(&t)
            .ok_or_else(|| format!("{name}: {t}: a member without #[bits(N)] is uN or bool"))?;
        // the other attributes, then #[bits(N)] with its options
        let opts = bits.map(|i| m.attrs.remove(i)).and_then(|a| match a.meta {
            syn::Meta::List(l) => Some(l.tokens),
            _ => None,
        }).map(|o| quote!(, #o));
        let width = Literal::u32_unsuffixed(width);
        m.attrs.extend(syn::Attribute::parse_outer.parse2(respan(quote!(#[bits(#width #opts)]), t.span()))
            .expect("an attribute"));
        m.ty = syn::parse2(respan(quote!(#rust_ty), t.span())).expect("a type");
    }
    if matches!(m.vis, syn::Visibility::Inherited) {
        m.vis = syn::parse2(respan(quote!(pub), name.span())).expect("a visibility");
    }
    m.ident.as_mut().unwrap().set_span(at(name.span()));
    Ok(())
}

/// @m's width: its #[bits(N, ..)]'s N, which bitfield_member() gives every
/// member.
fn member_width(m: &syn::Field) -> Result<u32, String> {
    let name = m.ident.as_ref().expect("a named member");
    m.attrs.iter()
        .find(|a| a.path().is_ident("bits"))
        .and_then(|a| match &a.meta {
            syn::Meta::List(l) => l.tokens.clone().into_iter().next(),
            _ => None,
        })
        .and_then(|t| syn::parse2::<syn::LitInt>(t.into()).ok())
        .and_then(|n| n.base10_parse().ok())
        .ok_or_else(|| format!("{name}: #[bits(N, ..)], N an integer literal"))
}

/// A bilge member type's width and the Rust type it's read as: bool, and uN
/// for N bits as N rounded up to an integer.
fn bilge_width(t: &Ident) -> Option<(u32, Ident)> {
    if t == "bool" {
        return Some((1, t.clone()));
    }
    let n: u32 = t.to_string().strip_prefix('u')?.parse().ok().filter(|n| (1..=128).contains(n))?;
    let int = [8, 16, 32, 64, 128].into_iter().find(|w| *w >= n)?;
    Some((n, format_ident!("u{int}")))
}

/// An x-macro list: xmacro! { NAME { (args), ... } }: a macro,
/// NAME!(cb), which expands to cb! { (args), ... } - and NAME!(cb [..]),
/// to cb! { [..] (args), ... }, for a cb that accumulates. cb may be a path.
///
/// An entry can be another list, SUB(), or another list's entries mapped,
/// SUB((params) => (args)). Then NAME!(cb) is a chain of macros, one per
/// list it splices in: each is SUB! handed the entries so far, which adds
/// SUB's, mapped, and those after, and calls the next; the last calls cb!. They're named by path, through
/// crate::cstructs::c - NAME is expanded wherever it's used.
#[proc_macro]
pub fn xmacro(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("xmacro!", || xmacro_expand(input.into()))
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
                if eq.as_char() == '=' && gt.as_char() == '>' && !cb.is_empty() => (token.clone(), cb),
            _ => return Err("expected [TOKEN => cb] before the list".into()),
        };
        let (cb, args) = match cb {
            [cb @ .., TokenTree::Group(g)] if g.delimiter() == Delimiter::Brace => (cb, g.stream()),
            cb => (cb, TokenStream::new()),
        };
        let cb = cb.iter().cloned().collect::<TokenStream>();

        let is_token = |t: &TokenTree| matches!(t, TokenTree::Ident(i) if *i == token);
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

/// xmacro!'s expansion: NAME!(cb), which calls cb! with the list's rows.
fn xmacro_expand(input: TokenStream) -> Result<TokenStream, String> {
    let x = parse_xmacro(input)?;
    let name = &x.name;
    // The plain entries from @from to @to: none of them a list spliced in
    let args = |from: usize, to: usize| x.entries[from..to].iter().map(|e| match e {
        XEntry::Args(g) => g,
        XEntry::Sub { .. } => unreachable!("taken between the lists spliced in"),
    });

    let mut out = TokenStream::new();
    let subs: Vec<usize> = x.entries.iter().enumerate()
        .filter(|(_, e)| matches!(e, XEntry::Sub { .. }))
        .map(|(i, _)| i)
        .collect();

    if subs.is_empty() {
        let entries = args(0, x.entries.len());
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
        let e = args(from, to);
        quote!(#(#e,)*)
    };
    let path = quote!(crate::cstructs::c);
    let carry = quote!(($($cb)::+) ($($pre)?));
    let step = |n: usize| format_ident!("__{name}_{n}");
    for (n, &i) in subs.iter().enumerate() {
        let XEntry::Sub { map, .. } = &x.entries[i] else { unreachable!() };
        let (pattern, mapped) = match map {
            None => (quote!(($($e:tt)*)), quote!(($($e)*))),
            Some(m) => {
                let params = &m.params;
                let template = xmacro_template(m.template.clone(), params);
                (quote!((#($#params:tt),*)), quote!((#template)))
            }
        };
        let after = lits(i + 1, subs.get(n + 1).copied().unwrap_or(x.entries.len()));
        let acc = quote!($($acc)* $(#mapped,)* #after);
        let next = match subs.get(n + 1) {
            Some(&j) => {
                let XEntry::Sub { list: next_list, .. } = &x.entries[j] else { unreachable!() };
                let next_step = step(n + 1);
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
    let XEntry::Sub { list: first, .. } = &x.entries[subs[0]] else { unreachable!() };
    let (step0, lits0) = (step(0), lits(0, subs[0]));
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

/// A mapped list's entry, @template: its @params $p, and concat(A, b, ..)
/// paste!'s [<A b ..>] - the mapping is expanded inside paste!.
fn xmacro_template(template: TokenStream, params: &[Ident]) -> TokenStream {
    let t: Vec<TokenTree> = template.into_iter().collect();
    let mut out = TokenStream::new();
    let mut k = 0;
    while k < t.len() {
        match (&t[k], t.get(k + 1)) {
            (TokenTree::Ident(c), Some(TokenTree::Group(g))) if c == "concat" && g.delimiter() == Delimiter::Parenthesis => {
                let parts = split_commas(g.stream()).into_iter().map(|p| xmacro_template(p, params));
                out.extend(quote!([< #(#parts)* >]));
                k += 2;
                continue;
            }
            (TokenTree::Ident(i), _) if params.contains(i) => out.extend(quote!($#i)),
            (TokenTree::Group(g), _) => {
                let mut inner = Group::new(g.delimiter(), xmacro_template(g.stream(), params));
                inner.set_span(g.span());
                out.extend([TokenTree::Group(inner)]);
            }
            (t, _) => out.extend([t.clone()]),
        }
        k += 1;
    }
    out
}

/// `atomic_bitfield! { pub struct NAME: uN { fields } }`: a
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
/// Any of them can be a #[bitfield]'s member, `#[bits(8)] btree_id: Btree`:
/// it has the const into_bits() bitfield-struct converts with, and
/// from_bits() - for flags, bitflags' from_bits() is the checked one, so a
/// member names `from = T::from_bits_retain`. A closed enum's member only
/// ever holds one of its values, as its type says; from_bits() of anything
/// else is memory corruption, and panics.
///
/// A variant prints and parses as its name, or its #[label("..")].
/// `#[nr]`: Self::NR, past the
/// largest value - a number, not one of its values. `#[own_to_text]`: it
/// prints as more than its name, by its own ToText.
///
/// A variant is written with its doc, and its columns - `#[arm_columns(..)]`,
/// enum_decl.rs - each made a const fn of the enum:
///
///   #[bch_enum(open)]
///   #[nr]
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

/// Enum @e: the type, its values, and their names.
fn enum_gen(e: &EnumDef) -> TokenStream {
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
    for (v, EnumVariant { value, .. }) in names.iter().zip(&e.variants) {
        let k = Literal::usize_unsuffixed(n);
        raw_values.push(match (value, base) {
            (Some(x), _) => quote!(#x),
            (None, Some(b)) => quote!(#b + #k),
            (None, None) => quote!(#k),
        });
        values.push(match (value, base, e.kind) {
            (Some(x), ..) => Some(quote!(#x)),
            (None, _, EnumKind::Closed) => None,
            (None, Some(b), EnumKind::Open) => Some(quote!(Self::#b.0 + #k)),
            (None, Some(b), EnumKind::Flags) => Some(quote!(Self::#b.bits() + #k)),
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
            EnumKind::Closed => quote!(Self::#v as #repr),
            EnumKind::Open   => quote!(Self::#v.0),
            EnumKind::Flags  => quote!(Self::#v.bits()),
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
    // An LeN field of an open enum is stored as its integer or a narrower
    // one - a byte of a u32 enum - and one of flags as theirs
    let uints = ["u8", "u16", "u32", "u64"];
    let n = uints.iter().position(|u| repr == u).map_or(0, |i| i + 1);
    let widths = if e.kind == EnumKind::Flags { &uints[n.saturating_sub(1)..n] } else { &uints[..n] };
    let too_wide = format!("{name}: a value too wide for its field");
    let le_values = widths.iter().map(|w| {
        let w = format_ident!("{w}");
        let (from, into) = match e.kind {
            EnumKind::Flags => (quote!(Self::from_bits_retain(v)), quote!(self.bits())),
            _ => (quote!(Self(v.into())), quote!(#w::try_from(self.0).expect(#too_wide))),
        };
        quote! {
            impl crate::types::LeValue<#w> for #name {
                fn from_int(v: #w) -> Self { #from }
                fn into_int(self) -> #w { #into }
            }
        }
    }).collect::<Vec<_>>();
    out.extend(match e.kind {
        EnumKind::Closed => {
            // what it prints as: its label
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
        EnumKind::Open => quote! {
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
            impl From<#name> for u64 {
                fn from(v: #name) -> u64 { v.0.into() }
            }
            impl TryFrom<u64> for #name {
                type Error = <#repr as TryFrom<u64>>::Error;
                fn try_from(v: u64) -> Result<Self, Self::Error> { #repr::try_from(v).map(Self) }
            }
            // SAFETY: its integer, every value of which is one
            unsafe impl crate::util::vstructs::Plain for #name {}
            crate::one_word!(#name);
            #(#le_values)*
        },
        EnumKind::Flags => quote! {
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
            impl From<#name> for u64 {
                fn from(v: #name) -> u64 { v.bits().into() }
            }
            impl TryFrom<u64> for #name {
                type Error = <#repr as TryFrom<u64>>::Error;
                fn try_from(v: u64) -> Result<Self, Self::Error> { #repr::try_from(v).map(Self::from_bits_retain) }
            }
            unsafe impl crate::util::vstructs::Plain for #name {}
            crate::one_word!(#name);
            #(#le_values)*

            // The flags as plain integers, for a type-level constant: one
            // that calls bitflags' const fns can cycle in rustc's
            // coherence checking (Debian's 1.97.1)
            #[allow(non_snake_case, non_upper_case_globals)]
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
    // An array of a value per variant is indexed by it
    if e.kind == EnumKind::Open && nr.is_some() {
        out.extend(quote! {
            impl<T> core::ops::Index<#name> for [T; #name::NR as usize] {
                type Output = T;
                fn index(&self, i: #name) -> &T { &self[i.0 as usize] }
            }
            impl<T> core::ops::IndexMut<#name> for [T; #name::NR as usize] {
                fn index_mut(&mut self, i: #name) -> &mut T { &mut self[i.0 as usize] }
            }
        });
    }

    out.extend(names_gen(e, name));
    out.extend(typeinfo::info(name, name, quote!(Enum), &[]));

    // Zeroed is a value of it - so a struct holding one can be Zeroable:
    // any integer is an open enum's or flags', a closed enum's only if a
    // variant is 0 - as written, or the first, counting from 0
    let has_zero = e.kind != EnumKind::Closed || e.variants.iter().enumerate()
        .any(|(i, v)| match &v.value {
            None    => i == 0,
            Some(x) => int_of(x) == Some(0),
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
fn names_gen(e: &EnumDef, name: &Ident) -> TokenStream {
    let labelled: Vec<(String, &Ident)> = e.variants.iter()
        .filter_map(|v| e.label(v).map(|l| (l, &v.name)))
        .collect();
    if labelled.is_empty() {
        return TokenStream::new();
    }

    let n = quote!(crate::util::names);
    let entries = labelled.iter().map(|(l, v)| match e.kind {
        EnumKind::Closed => quote!((#l, Self::#v as u64)),
        EnumKind::Open   => quote!((#l, Self::#v.0 as u64)),
        EnumKind::Flags  => quote!((#l, #n::bit(Self::#v.bits() as u64))),
    });
    let mut out = quote! {
        impl #n::Names for #name {
            const ENTRIES: &'static [(&'static str, u64)] = &[#(#entries),*];
        }
    };

    let to_text = match e.kind {
        _ if e.own_to_text => None,
        EnumKind::Closed  => None,
        EnumKind::Open    => Some(quote!(#n::prt_name(out, <Self as #n::Names>::ENTRIES, self.0 as u64))),
        EnumKind::Flags   => Some(quote!(#n::prt_flags(out, <Self as #n::Names>::ENTRIES, self.bits() as u64))),
    };
    // A closed enum's derive gives it name(); an open one's is here
    if e.kind == EnumKind::Open && !e.own_to_text {
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
///   ioctl! {
///       BCH_IOCTL_DISK_ADD = _IOW(0xbc, 4, c::bch_ioctl_disk),
///       BCHFS_IOC_REINHERIT_ATTRS = _IOR(0xbc, 64, *const core::ffi::c_char),
///   }
///
/// Rust gets a marker type for each, named as the #define, implementing
/// util::ioctl::Ioctl: the opcode, made by util::ioctl's _IO*() from the
/// target's own constants, and the argument type.
#[proc_macro]
pub fn ioctl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("ioctl!", || {
        let io = parse_ioctl(input.into())?;

        let mut out = TokenStream::new();
        for e in &io.entries {
            let (name, dir, ty, nr) = (&e.name, &e.dir, &e.ty, &e.nr);
            let (arg, opcode) = match &e.arg {
                Some(a) => (quote!(#a), quote!(crate::util::ioctl::#dir::<#a>(#ty, #nr))),
                None => (quote!(()), quote!(crate::util::ioctl::_IO(#ty, #nr))),
            };
            // _IOWR -> dir::WR, _IO -> dir::IO
            let dir = dir.to_string();
            let dir_ty = format_ident!("{}", dir.strip_prefix("_IO").filter(|d| !d.is_empty()).unwrap_or("IO"));
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
