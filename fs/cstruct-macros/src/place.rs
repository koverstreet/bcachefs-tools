// SPDX-License-Identifier: GPL-2.0
//! The macro side of util/place.rs - a value's place, the field of a parent
//! it lives at.
//!
//! `#[placed(Base.field)]`, or `#[placed(Base.field[], index = NAME: TYPE)]`
//! for an element of that array, ahead of `#[pin_data]` and the derives, is
//! all a placed type says, and all its parent needs - `[..]` for an element
//! of the Container there is below. It gives the struct:
//!
//! - its At field, `#[placed_at]` - `_placed_at`, zero bytes, char[0] to C;
//!   or an element's index, NAME, its AtIndex C's TYPE - for the derives
//!   after it to see as any other field: `#[derive(Init)]` gives its init()
//!   the At to put there;
//! - Contained - its parent, and its At;
//! - InitAt for Base's field - its init(), the At from at(), or for an
//!   array each element's from at_index() - which Base's `#[derive(Init)]`
//!   picks for the field: so an At is made only for the field it names, and
//!   nothing is written on the parent;
//! - its #[cfg]s, on all of these;
//! - no drop of its own: its container() would see a parent partly dropped.
//!   A #[pin_data] struct can't impl Drop, and #[pin_data(PinnedDrop)] is
//!   refused; a struct without #[pin_data] gets an empty Drop, for one of its
//!   own to conflict with.
//!
//! For an element of a Container, it gives the struct its AtIn field, and
//! PlacedIn, and Base's InitAt for the field - the Container, by
//! util/place.rs's container() - but neither Contained nor a container() - an
//! element's place is for the guarantee it never outlives its Base - nor
//! any rule on its drop. The At field's attributes, #[placed_at] and #[c],
//! are #[derive(Init)]'s and #[derive(CStruct)]'s, so it has them only if it
//! has those.
//!
//! And `#[derive(Init)]` (init.rs) makes a Field<N> for each field of the
//! structs it's on, N its name's hash - a field as a type, for the children
//! placed at it - and initializes one without #[init] with field_init(): its
//! InitAt, if a placed type lives there, else zeroed.

use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens as _};
use syn::parse::{Parse, ParseStream, Parser as _};
use syn::punctuated::Punctuated;
use syn::{bracketed, token, Error, Field, Fields, Ident, ItemStruct, LitStr, Path, Token, Type};

use crate::cstruct::CFieldDef;

/// Where #[placed] says a type lives: at Base.field; as an element of the
/// array there, holding its index as @0, of integer type @1; or as an
/// element of the Container there.
enum Kind {
    Field,
    Array(Ident, Type),
    Container,
}

pub(crate) struct PlaceAttr {
    base:  Path,
    field: Ident,
    kind:  Kind,
}

const USAGE: &str = "takes Base.field; Base.field[], index = NAME: TYPE, for an element of the array there, its \
                     index the field NAME of integer TYPE; or Base.field[..], for an element of the Container there";

impl Parse for PlaceAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let usage = |e: Error| Error::new(e.span(), format!("{USAGE} - {e}"));
        let base = Path::parse_mod_style(input).map_err(usage)?;
        input.parse::<Token![.]>().map_err(usage)?;
        let field: Ident = input.parse().map_err(usage)?;
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
    let mut cstruct = false;                // whether C has it
    let mut init = false;                   // whether #[derive(Init)]'s
    let mut cfgs = Vec::new();              // for its impls too
    for a in &item.attrs {
        let path = a.path();
        if path.segments.last().is_some_and(|s| s.ident == "pin_data") {
            pin_data = Some(a.meta.to_token_stream().to_string().contains("PinnedDrop"));
        } else if path.is_ident("derive") {
            let derives = a.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)?;
            let has = |d: &str| derives.iter().any(|p| p.segments.last().is_some_and(|s| s.ident == d));
            cstruct |= has("CStruct");
            init |= has("Init");
        } else if path.is_ident("cfg") {
            cfgs.push(a.clone());
        }
    }
    if !item.generics.params.is_empty() {
        return Err(Error::new_spanned(&item.generics, format!("{name}: a placed type has no generics")));
    }

    // Its At, and the field it's in: an element's index, as C names it, or
    // a field of no bytes
    let field_name = LitStr::new(&field.to_string(), field.span());
    let n = quote!({ crate::util::place::name(#field_name) });
    let empty_at = || (format_ident!("{AT_FIELD}"), format!("char {AT_FIELD}[0]"));
    let (at, (at_field, at_decl)) = match &kind {
        Kind::Field => (quote!(crate::util::place::At<#base, #n>), empty_at()),
        Kind::Array(idx, ty) => {
            let c_ty = match crate::cstruct::field_type(&ty.to_token_stream().to_string()) {
                Ok(crate::cstruct::FieldType::Named { c: Some(c), .. }) => c,
                _ => return Err(Error::new_spanned(ty,
                    format!("{name}: its index, {idx}: {}, isn't an integer C has", ty.to_token_stream()))),
            };
            (quote!(crate::util::place::AtIndex<#base, #n, #ty>), (idx.clone(), format!("{c_ty} {idx}")))
        }
        Kind::Container => (quote!(crate::util::place::AtIn<#name>), empty_at()),
    };

    // The At field, last. #[derive(Init)]'s init() takes the At, and C has a
    // declaration of it - if they're there: the attributes are theirs
    let Fields::Named(fields) = &mut item.fields else {
        return Err(Error::new_spanned(&item, format!("{name}: is for a struct with named fields")));
    };
    let init_attr = init.then(|| quote!(#[placed_at]));
    let c_attr = cstruct.then(|| quote!(#[c(#at_decl)]));
    fields.named.push(Field::parse_named.parse2(quote!(#init_attr #c_attr pub(crate) #at_field: #at))?);

    let init_at = |init: TokenStream| quote! {
        #(#cfgs)*
        #[automatically_derived]
        // SAFETY: init() is Base's for the field, which Base's #[derive(Init)]
        // runs it into, the field it's for
        unsafe impl crate::util::place::InitAt<#n> for #base {
            unsafe fn init() -> impl ::pin_init::PinInit<
                <Self as crate::util::place::Field<#n>>::Type,
                crate::errcode::BchError>
            {
                // SAFETY: the caller's
                unsafe { #init }
            }
        }
    };

    // A field's or an array element's: Contained, its InitAt, and no drop of
    // its own. A container's element: PlacedIn, its container's InitAt -
    // and its drop is its own, the container whole while it runs
    let impls = match &kind {
        Kind::Field | Kind::Array(..) => {
            if pin_data == Some(true) {
                return Err(Error::new_spanned(&name, format!(
                    "{name}: a placed type has no drop of its own - its container() would see a parent partly \
                     dropped - so not #[pin_data(PinnedDrop)]")));
            }
            let init = match &kind {
                Kind::Field => quote!(crate::util::place::at(#name::init)),
                _ => quote!(::pin_init::pin_init_array_from_fn(|i| unsafe {
                    crate::util::place::at_index(i, #name::init) })),
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
            let init_at = init_at(init);
            quote! {
                #(#cfgs)*
                #[automatically_derived]
                // SAFETY: its At field, which its init() takes - given by InitAt's,
                // from at() or at_index(), for the place the At names
                unsafe impl crate::util::place::Contained for #name {
                    type Parent = #base;
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

/// Whether @f is an At of no bytes - not an element's index - which
/// TypeInfo doesn't describe.
pub(crate) fn is_empty_at_field(f: &CFieldDef) -> bool {
    f.placed_at && f.name == AT_FIELD
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
