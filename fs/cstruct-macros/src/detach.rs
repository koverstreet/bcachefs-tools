// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Detach)]`: util/rcu.rs's Detach, each field detached in turn - a
//! field with nothing to detach a no-op, by Detach's own impls, and one with
//! something but no impl a build error, as for the struct itself.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DataStruct, DeriveInput, Error, Index};

pub(crate) fn derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(Error::new_spanned(&input, format!("{name}: #[derive(Detach)] is for a struct")));
    };

    let detaches = fields.iter().enumerate().map(|(i, f)| {
        let cfg = f.attrs.iter().filter(|a| a.path().is_ident("cfg"));
        let member = match &f.ident {
            Some(id) => quote!(#id),
            None => {
                let i = Index::from(i);
                quote!(#i)
            }
        };
        quote!(#(#cfg)* crate::util::rcu::Detach::detach(&self.#member);)
    });

    let (impl_g, ty_g, where_c) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_g crate::util::rcu::Detach for #name #ty_g #where_c {
            fn detach(&self) {
                #(#detaches)*
            }
        }
    })
}
