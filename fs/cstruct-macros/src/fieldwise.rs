// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Fieldwise)]`: methods on a struct that apply the method of the
//! same name to each of its fields.
//!
//! `#[fieldwise(clamp_nonnegative, wrapping_add(rhs))]` makes
//!
//! ```text
//! fn clamp_nonnegative(self) -> Self
//! fn wrapping_add(self, rhs: Self) -> Self
//! ```
//!
//! each field becoming `self.f.clamp_nonnegative()`, `self.f.wrapping_add(rhs.f)`.
//! A bare name is unary, one with a parameter binary: a derive sees no types,
//! so the attribute says which. `Trait::method` makes it that trait's - one
//! impl per trait - for generic code to call. Arrays are mapped element by
//! element; a field of another such struct gets the method it was given - so
//! the methods take and return by value, the struct Copy, as integers' own are.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parenthesized, token, Data, DataStruct, DeriveInput, Error, Ident, Token, Type};

/// A method #[derive(Fieldwise)] lifts: unary, or binary with a parameter of
/// this name - #[fieldwise(method)] or #[fieldwise(method(rhs))] - and an
/// inherent one, or Trait::method, of that trait's impl.
struct Method {
    tr:     Option<Ident>,
    method: Ident,
    rhs:    Option<Ident>,
}

impl Parse for Method {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let first: Ident = input.parse()?;
        let (tr, method) = match input.parse::<Option<Token![::]>>()? {
            Some(_) => (Some(first), input.parse()?),
            None => (None, first),
        };
        let rhs = match input.peek(token::Paren) {
            true => {
                let inner;
                parenthesized!(inner in input);
                Some(inner.parse()?)
            }
            false => None,
        };
        Ok(Method { tr, method, rhs })
    }
}

pub(crate) fn derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(Error::new_spanned(&input, format!("{name}: #[derive(Fieldwise)] is for a struct")));
    };
    let mut methods = Vec::new();
    for a in input.attrs.iter().filter(|a| a.path().is_ident("fieldwise")) {
        methods.extend(a.parse_args_with(Punctuated::<Method, Token![,]>::parse_terminated)?);
    }
    if methods.is_empty() {
        return Err(Error::new_spanned(name, format!("{name}: #[fieldwise(...)] names no methods")));
    }

    // Inherent methods under None, a trait's under its name: one impl each.
    let mut impls: Vec<(Option<&Ident>, Vec<TokenStream>)> = Vec::new();
    for m in &methods {
        let mut inits = Vec::new();
        for f in fields {
            let Some(field) = &f.ident else {
                return Err(Error::new_spanned(f, format!("{name}: #[derive(Fieldwise)] is for named fields")));
            };
            if f.attrs.iter().any(|a| a.path().is_ident("c_bitfield")) {
                return Err(Error::new_spanned(field,
                    format!("{name}.{field}: #[derive(Fieldwise)] can't see into a bitfield")));
            }
            let cfg = f.attrs.iter().filter(|a| a.path().is_ident("cfg"));
            let rhs = m.rhs.as_ref().map(|r| quote!(#r.#field));
            let e = apply(&m.method, &f.ty, quote!(self.#field), rhs, 0);
            inits.push(quote!(#(#cfg)* #field: #e,));
        }

        let params = match &m.rhs {
            Some(r) => quote!(self, #r: Self),
            None    => quote!(self),
        };
        let vis = m.tr.is_none().then(|| quote!(pub));
        let method = &m.method;
        let method = quote! {
            #[inline]
            #vis fn #method(#params) -> Self {
                Self {
                    #(#inits)*
                }
            }
        };

        match impls.iter_mut().find(|(t, _)| *t == m.tr.as_ref()) {
            Some((_, body)) => body.push(method),
            None            => impls.push((m.tr.as_ref(), vec![method])),
        }
    }

    // its lifetimes elided, as an impl that doesn't name them has it
    let lts: Vec<_> = input.generics.lifetimes().map(|_| quote!('_)).collect();
    let ty = match lts.is_empty() {
        true => quote!(#name),
        false => quote!(#name<#(#lts),*>),
    };
    Ok(impls.iter().map(|(tr, body)| match tr {
        Some(tr) => quote!(#[automatically_derived] impl #tr for #ty { #(#body)* }),
        None     => quote!(#[automatically_derived] impl #ty { #(#body)* }),
    }).collect())
}

/// @method on @val, of type @ty - with @rhs, for a binary one - an array's
/// elements each, at @depth arrays in.
fn apply(method: &Ident, ty: &Type, val: TokenStream, rhs: Option<TokenStream>, depth: usize) -> TokenStream {
    if let Type::Array(a) = ty {
        let i = format_ident!("__i{depth}");
        let rhs = rhs.map(|r| quote!(#r[#i]));
        let e = apply(method, &a.elem, quote!(#val[#i]), rhs, depth + 1);
        return quote!(::core::array::from_fn(|#i| #e));
    }
    match rhs {
        Some(r) => quote!(#val.#method(#r)),
        None    => quote!(#val.#method()),
    }
}
