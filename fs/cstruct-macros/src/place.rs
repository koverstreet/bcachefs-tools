// SPDX-License-Identifier: GPL-2.0
//! The macro side of util/place.rs - a value's place, the field of a parent
//! it lives at.
//!
//! `#[placed(Base.field)]`, `#[placed(Base.field.*)]` for the Inner of the
//! Wrapper there - a Locked's data - or `#[placed(Base.field[], index =
//! NAME: TYPE)]` for an element of the array there, or of the Array there,
//! ahead of `#[pin_data]` and the derives, is all a placed type says, and
//! all its parent needs - `[..]` for an element of the Container there is
//! below. It gives the struct:
//!
//! - its At field, `#[placed_at]` - `_placed_at`, zero bytes; or an
//!   element's index, NAME, its AtIndex TYPE - for the derives
//!   after it to see as any other field: `#[derive(Init)]` gives its init()
//!   the At to put there;
//! - Contained - its parent, and its At;
//! - InitAt for Base's field - its init(), the At from at(), or for an
//!   array each element's from at_index() - which Base's `#[derive(Init)]`
//!   picks for the field: so an At is made only for the field it names, and
//!   nothing is written on the parent. An Inner has none: Base's
//!   #[init_locked] field makes it, its AtInner from at_inner();
//! - its #[cfg]s, on all of these;
//! - no drop of its own: its container() would see a parent partly dropped.
//!   A #[pin_data] struct can't impl Drop, and #[pin_data(PinnedDrop)] is
//!   refused; a struct without #[pin_data] gets an empty Drop, for one of its
//!   own to conflict with. Its teardown is util/place.rs's PlacedDrop.
//!
//! For an element of a Container, it gives the struct its AtIn field, and
//! PlacedIn, and Base's InitAt for the field - the Container, by
//! util/place.rs's container() - but neither Contained nor a container() - an
//! element's place is for the guarantee it never outlives its Base - nor
//! any rule on its drop. The At field's #[placed_at] is #[derive(Init)]'s,
//! so it has it only if it has that.
//!
//! And `#[derive(Init)]` (init.rs) makes a Field<N> for each field of the
//! structs it's on, N its name's hash - a field as a type, for the children
//! placed at it - and initializes one without #[init] with field_init(): its
//! InitAt, if a placed type lives there, else zeroed.

use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens as _};
use syn::parse::{Parse, ParseStream, Parser as _};
use syn::punctuated::Punctuated;
use syn::{bracketed, token, Error, Field, Fields, FnArg, Ident, ItemStruct, Lifetime, LitStr, Pat, PatType, Path,
          Token, Type};

use crate::cstruct::CFieldDef;

/// Where #[placed] says a type lives: at Base.field; as an element of the
/// array there, holding its index as @0, of integer type @1; or as an
/// element of the Container there.
enum Kind {
    Field,
    Inner,
    Array(Ident, Type),
    Container,
}

pub(crate) struct PlaceAttr {
    base:  Path,
    field: Ident,
    kind:  Kind,
}

const USAGE: &str = "takes Base.field; Base.field.*, for the Inner of the Wrapper there; Base.field[], index = \
                     NAME: TYPE, for an element of the array there, its index the field NAME of integer TYPE; or \
                     Base.field[..], for an element of the Container there";

impl Parse for PlaceAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let usage = |e: Error| Error::new(e.span(), format!("{USAGE} - {e}"));
        let base = Path::parse_mod_style(input).map_err(usage)?;
        input.parse::<Token![.]>().map_err(usage)?;
        let field: Ident = input.parse().map_err(usage)?;
        if input.peek(Token![.]) {
            input.parse::<Token![.]>()?;
            input.parse::<Token![*]>().map_err(usage)?;
            if !input.is_empty() {
                return Err(usage(input.error("unexpected tokens")));
            }
            return Ok(PlaceAttr { base, field, kind: Kind::Inner });
        }
        // Some(container): [..] or []
        let elem = match input.peek(token::Bracket) {
            true => {
                let inner;
                bracketed!(inner in input);
                let container = !inner.is_empty();
                if container {
                    inner.parse::<Token![..]>().map_err(usage)?;
                }
                Some(container)
            }
            false => None,
        };
        let index = match input.parse::<Option<Token![,]>>()? {
            Some(_) => {
                let kw: Ident = input.parse().map_err(usage)?;
                if kw != "index" {
                    return Err(usage(Error::new_spanned(kw, "expected index")));
                }
                input.parse::<Token![=]>().map_err(usage)?;
                let name: Ident = input.parse().map_err(usage)?;
                input.parse::<Token![:]>().map_err(usage)?;
                Some((name, input.parse::<Type>().map_err(usage)?))
            }
            None => None,
        };
        if !input.is_empty() {
            return Err(usage(input.error("unexpected tokens")));
        }
        let kind = match (elem, index) {
            (None, None) => Kind::Field,
            (Some(true), None) => Kind::Container,
            (Some(false), Some((name, ty))) => Kind::Array(name, ty),
            (Some(false), None) => return Err(Error::new(field.span(),
                format!("{USAGE}: an array's element names its index"))),
            (_, Some((name, _))) => return Err(Error::new(name.span(),
                format!("{USAGE}: only an array's element has an index"))),
        };
        Ok(PlaceAttr { base, field, kind })
    }
}

/// `#[placed(...)]`: the struct, its At field added, and its impls.
pub(crate) fn placed(PlaceAttr { base, field, kind }: PlaceAttr, mut item: ItemStruct) -> syn::Result<TokenStream> {
    let name = item.ident.clone();

    // Its outer attributes: #[pin_data], its derives, its #[cfg]s
    let mut pin_data: Option<bool> = None;  // whether PinnedDrop
    let mut init = false;                   // whether #[derive(Init)]'s
    let mut cfgs = Vec::new();              // for its impls too
    for a in &item.attrs {
        let path = a.path();
        if path.segments.last().is_some_and(|s| s.ident == "pin_data") {
            pin_data = Some(a.meta.to_token_stream().to_string().contains("PinnedDrop"));
        } else if path.is_ident("derive") {
            let derives = a.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)?;
            let has = |d: &str| derives.iter().any(|p| p.segments.last().is_some_and(|s| s.ident == d));
            init |= has("Init");
        } else if path.is_ident("cfg") {
            cfgs.push(a.clone());
        }
    }
    if !item.generics.params.is_empty() {
        return Err(Error::new_spanned(&item.generics, format!("{name}: a placed type has no generics")));
    }

    // Its #[init_args], which its init() takes after the At: its InitAt takes
    // them, a tuple, from its Base's #[placed_args] - a reference's lifetime
    // the impl's 'a
    let mut arg_pats = Vec::new();
    let mut arg_tys = Vec::new();
    let mut arg_lifetime = false;
    if let Some(a) = item.attrs.iter().find(|a| a.path().is_ident("init_args")) {
        for arg in a.parse_args_with(Punctuated::<FnArg, Token![,]>::parse_terminated)? {
            let FnArg::Typed(PatType { pat, ty, .. }) = &arg else {
                return Err(Error::new_spanned(&arg, format!("{name}: an init arg is a name and a type")));
            };
            let Pat::Ident(p) = &**pat else {
                return Err(Error::new_spanned(pat, format!("{name}: an init arg is a name and a type")));
            };
            let mut ty = (**ty).clone();
            if let Type::Reference(r) = &mut ty {
                if r.lifetime.is_none() {
                    r.lifetime = Some(Lifetime::new("'a", r.and_token.span));
                    arg_lifetime = true;
                }
            }
            arg_pats.push(p.ident.clone());
            arg_tys.push(ty);
        }
    }
    if !arg_pats.is_empty() && !matches!(kind, Kind::Field) {
        return Err(Error::new_spanned(&name, format!("{name}: only a placed field's type takes init args")));
    }

    // Its At, and the field it's in: an element's index, as C names it, or
    // a field of no bytes
    let field_name = LitStr::new(&field.to_string(), field.span());
    let n = quote!({ crate::util::place::name(#field_name) });
    let (at, at_field) = match &kind {
        Kind::Field => (quote!(crate::util::place::At<#base, #n>), format_ident!("{AT_FIELD}")),
        /* named for its field: a Locked's data has the parent's members */
        Kind::Inner => (quote!(crate::util::place::AtInner<#base, #n>), format_ident!("{INNER_AT_FIELD}{field}")),
        Kind::Array(idx, ty) => (quote!(crate::util::place::AtIndex<#base, #n, #ty>), idx.clone()),
        Kind::Container => (quote!(crate::util::place::AtIn<#name>), format_ident!("{AT_FIELD}")),
    };

    // The At, last. #[derive(Init)]'s init() takes the At - if it's there:
    // the attribute is its
    let Fields::Named(fields) = &mut item.fields else {
        return Err(Error::new_spanned(&item, format!("{name}: is for a struct with named fields")));
    };
    let init_attr = init.then(|| quote!(#[placed_at]));
    fields.named.push(Field::parse_named.parse2(quote!(#init_attr pub(crate) #at_field: #at))?);

    let lifetime = arg_lifetime.then(|| quote!(<'a>));
    let init_at = |init: TokenStream| quote! {
        #(#cfgs)*
        #[automatically_derived]
        // SAFETY: init() is Base's for the field, which Base's #[derive(Init)]
        // runs it into, the field it's for
        unsafe impl #lifetime crate::util::place::InitAt<#n, (#(#arg_tys,)*)> for #base {
            unsafe fn init((#(#arg_pats,)*): (#(#arg_tys,)*)) -> impl ::pin_init::PinInit<
                <Self as crate::util::place::Field<#n>>::Type,
                crate::errcode::BchError>
            {
                // SAFETY: the caller's
                unsafe { #init }
            }
        }
    };

    // A field's, a Wrapper's Inner or an array element's: Contained, its
    // InitAt - but an Inner's, which its Base's #[init_locked] makes - and no
    // drop of its own. A container's element: PlacedIn, its container's
    // InitAt - and its drop is its own, the container whole while it runs
    let impls = match &kind {
        Kind::Field | Kind::Inner | Kind::Array(..) => {
            if pin_data == Some(true) {
                return Err(Error::new_spanned(&name, format!(
                    "{name}: a placed type has no drop of its own - its container() would see a parent partly \
                     dropped - so not #[pin_data(PinnedDrop)]: its teardown is a PlacedDrop")));
            }
            let init = match &kind {
                Kind::Field => Some(quote!(crate::util::place::at(|at| #name::init(at, #(#arg_pats),*)))),
                Kind::Inner => None,
                _ => Some(quote!(<<Self as crate::util::place::Field<#n>>::Type as crate::util::place::Array>::init_each(
                    |i| unsafe { crate::util::place::at_index(i, #name::init) }))),
            };
            // #[pin_data]'s own check refuses a Drop; without it, this is the
            // one a Drop of its own would conflict with
            let no_drop = pin_data.is_none().then(|| quote! {
                #(#cfgs)*
                #[automatically_derived]
                impl Drop for #name {
                    fn drop(&mut self) {}
                }
            });
            let init_at = init.map(init_at);
            quote! {
                #(#cfgs)*
                #[automatically_derived]
                // SAFETY: its At field, which its init() takes - given by InitAt's,
                // from at() or at_index(), for the place the At names
                unsafe impl crate::util::place::Contained for #name {
                    type Parent = <#base as crate::util::place::Enclosing>::Visible;
                    type At = #at;
                    unsafe fn placed_at_raw(p: *const Self) -> *const Self::At {
                        // SAFETY: the caller's
                        unsafe { &raw const (*p).#at_field }
                    }
                }
                #init_at
                #no_drop
            }
        }
        Kind::Container => {
            let init_at = init_at(quote!(crate::util::place::container::<Self, #n, #name, _>()));
            quote! {
                #(#cfgs)*
                #[automatically_derived]
                // SAFETY: its AtIn field, which only its container's insert() makes
                unsafe impl crate::util::place::PlacedIn for #name {
                    type Base = #base;
                }
                #init_at
            }
        }
    };

    Ok(quote!(#item #impls))
}

/// The At field #[placed] adds, where there's no index for it to be.
const AT_FIELD: &str = "_placed_at";

/// An Inner's, then its field's name.
const INNER_AT_FIELD: &str = "_placed_in_";

/// Whether @f is an At of no bytes - not an element's index - which
/// TypeInfo doesn't describe.
pub(crate) fn is_empty_at_field(f: &CFieldDef) -> bool {
    f.placed_at && (f.ident == AT_FIELD || f.ident.to_string().starts_with(INNER_AT_FIELD))
}

/// Field<N> for field @name, of type @ty, of @inst_ty - @impl_g and @where_c
/// its generics, @cfg the field's #[cfg].
pub(crate) fn field_impl(cfg: &TokenStream, impl_g: &impl quote::ToTokens, inst_ty: &TokenStream,
                         where_c: Option<&syn::WhereClause>, name: &Ident, ty: &Type) -> TokenStream {
    let n = name.to_string();
    quote! {
        #cfg
        #[automatically_derived]
        // SAFETY: its field, by offset_of!()
        unsafe impl #impl_g crate::util::place::Field<{ crate::util::place::name(#n) }> for #inst_ty #where_c {
            type Type = #ty;
            const OFFSET: usize = ::core::mem::offset_of!(#inst_ty, #name);
        }
    }
}

/// The initializer for field @name, without #[init]: InitAt's, if a placed
/// type lives there, else zeroed - util/place.rs's Pick.
pub(crate) fn field_init(name: &Ident) -> TokenStream {
    let n = name.to_string();
    quote! {
        {
            use crate::util::place::{PickPlaced as _, PickZeroed as _};
            // SAFETY: run into the field, by pin_init!()
            unsafe { (&crate::util::place::Pick::<Self, { crate::util::place::name(#n) }>::NEW).pick() }
        }
    }
}
