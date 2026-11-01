// SPDX-License-Identifier: GPL-2.0
//! `#[derive(ToText)]`: a fieldless enum, printed as its variant's name -
//! util/printbuf.rs's, here because this is the crate our macros live in.
//!
//! A variant prints as its own name, or as `#[label("..")]` names it: c_enum!
//! labels the variants an x-macro makes with the name the list gives each, as
//! C's string tables have them. It makes
//!
//! ```text
//! pub const fn name(&self) -> &'static str
//! impl ToText, writing name()
//! impl Display, through ToText
//! ```
//!
//! An enum with generics, or a variant with fields, has nothing it could
//! print as a name alone: an error.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Error, Fields, LitStr};

pub(crate) fn derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let Data::Enum(e) = &input.data else {
        return Err(Error::new_spanned(&input, "for an enum"));
    };
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(&input.generics, format!("{name}: a generic enum has no one set of names")));
    }

    let arms = e.variants.iter().map(|v| {
        let ident = &v.ident;
        if !matches!(v.fields, Fields::Unit) {
            return Err(Error::new_spanned(v, format!("{name}::{ident}: a variant with fields has no name to print")));
        }
        let label = match v.attrs.iter().find(|a| a.path().is_ident("label")) {
            Some(a) => a.parse_args::<LitStr>()
                .map_err(|e| Error::new(e.span(), format!("{name}: #[label(\"name\")] takes a string")))?,
            None => LitStr::new(&ident.to_string(), ident.span()),
        };
        Ok(quote!(Self::#ident => #label,))
    }).collect::<syn::Result<Vec<_>>>()?;

    Ok(quote! {
        #[automatically_derived]
        impl #name {
            /// What it prints as.
            pub const fn name(&self) -> &'static str {
                match *self { #(#arms)* }
            }
        }
        #[automatically_derived]
        impl crate::util::ToText for #name {
            fn to_text(&self, out: &mut crate::util::Printbuf) {
                out.write_fmt(::core::format_args!("{}", self.name()))
            }
        }
        crate::display_via_to_text!(#name);
    })
}
