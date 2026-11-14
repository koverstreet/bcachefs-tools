// SPDX-License-Identifier: GPL-2.0
//! #[bitfield] and #[derive(BitfieldMembers)]: bitfield_struct::bitfield, and the
//! members on the struct that holds one in a #[members] field.

use proc_macro2::{Ident, Literal, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens};

use crate::cstruct::{self, generic_arg, parse_cstruct};
use crate::tokens::{at, comma_list, key_value, respan};
use crate::{bitfield_atomic, typeinfo, vstructs};

pub(crate) fn bitfield_expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
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
    let atomic = args.atomic.then(|| bitfield_atomic::atomic(&item, storage));
    // The struct as tokens, not text: an error in a member points at it
    Ok(quote! {
        #out
        #[::bitfield_struct::bitfield(#storage #(, #rest)*)]
        #item
        #info
        #plain
        #atomic
        // SAFETY: Plain - its integer, any bytes of which are one
        unsafe impl ::pin_init::Zeroable for #name {}
    })
}

/// #[bitfield]'s arguments: bitfield-struct's, its storage first, and ours.
struct BitfieldArgs {
    storage: Ident,
    /// The rest of bitfield-struct's - with the repr our shorthands say
    rest:    Vec<TokenStream>,
    /// `atomic`: AtomicNAME too (bitfield_atomic.rs)
    atomic:  bool,
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
    let atomic = rest.iter().position(|a| a.to_string() == "atomic").map(|i| rest.remove(i)).is_some();
    if atomic && (storage.to_string().starts_with("Le") || align.is_some()) {
        return Err("atomic is of a uN, a native word: not LeN, nor aligned".into());
    }
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
    Ok(BitfieldArgs { storage, rest, atomic })
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

pub(crate) fn bitfield_members(input: TokenStream) -> Result<TokenStream, String> {
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

