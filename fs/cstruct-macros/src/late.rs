// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Late)]`: what a struct makes when the filesystem goes read-write,
//! declared on its fields - `#[late(EXPR)]` on a `Late<T>`, made by EXPR, a
//! PinInit<T, BchError>, or `#[late(EXPR, ERRCODE)]`, its failure
//! Errcode::ERRCODE; a bare `#[late]` on a struct with its own - as
//! `init_late()`, which makes them under the state lock, and `stop_late()`,
//! which teardown calls before it frees what their work uses: each made one
//! stopped - a workqueue drained - last first. They drop with the struct.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DataStruct, DeriveInput, Error, Meta};

use crate::init::initializer;

pub(crate) fn derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let inst = &input.ident;
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(Error::new_spanned(&input, format!("{inst}: #[derive(Late)] is for a struct")));
    };

    let mut init = Vec::new();
    let mut stop = Vec::new();
    for f in fields {
        let Some(a) = f.attrs.iter().find(|a| a.path().is_ident("late")) else { continue };
        let Some(name) = &f.ident else {
            return Err(Error::new_spanned(f, format!("{inst}: #[derive(Late)] is for named fields")));
        };
        let cfg: Vec<_> = f.attrs.iter().filter(|a| a.path().is_ident("cfg")).collect();
        if let Meta::Path(_) = a.meta {
            init.push(quote!(#(#cfg)* self.#name.init_late(state)?;));
            stop.push(quote!(#(#cfg)* self.#name.stop_late();));
        } else {
            let expr = initializer(a, name)?;
            init.push(quote!(#(#cfg)* self.#name.init(state, || #expr)?;));
            stop.push(quote!(#(#cfg)* self.#name.stop();));
        }
    }
    stop.reverse();

    let (impl_g, ty_g, where_c) = input.generics.split_for_impl();
    Ok(quote! {
        #[automatically_derived]
        impl #impl_g #inst #ty_g #where_c {
            /// Its #[late] fields made, but those that are: see #[derive(Late)].
            #[allow(unused_variables)]
            pub fn init_late(&self, state: &crate::fs::StateLockWrite<'_>) -> Result<(), crate::errcode::BchError> {
                #(#init)*
                Ok(())
            }

            /// Its made #[late] fields stopped, last first: see #[derive(Late)].
            pub fn stop_late(&self) {
                #(#stop)*
            }
        }
    })
}
