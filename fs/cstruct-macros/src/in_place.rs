// SPDX-License-Identifier: GPL-2.0
//! #[in_place(Fut)]: an async fn whose future is named Fut, for what has to
//! say its task's type: the struct that holds the task in place -
//! InPlace<'q, Fut> - or the pool a task's box comes from, sized for it. Fut
//! is a type alias impl Trait, the fn its defining use, so a call is any call.
//!
//! By type, not by a size: a struct whose field's type holds the future's
//! size can't be asked whether it's Freeze without that size, and a read of
//! one of its fields in the fn asks, while the fn's MIR - the future's layout,
//! the size - is being optimized: a cycle. A type is answered without its
//! layout.
//!
//! The args are stored once, as #[args_stored_once] stores them: the task's
//! size is its struct's. Fut's generics are the fn's.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{parse_quote, Ident, ItemFn};

pub(crate) fn in_place(fut: Ident, mut f: ItemFn) -> syn::Result<TokenStream> {
    let ret = crate::args_stored_once::store_args_once(&mut f)?;
    let vis = &f.vis;
    let (params, args, where_clause) = f.sig.generics.split_for_impl();
    let alias = quote! {
        #vis type #fut #params = impl ::core::future::Future<Output = #ret> + Send #where_clause;
    };
    f.sig.output = parse_quote!(-> #fut #args);
    f.attrs.push(parse_quote!(#[define_opaque(#fut)]));
    Ok(quote!(#alias #f))
}
