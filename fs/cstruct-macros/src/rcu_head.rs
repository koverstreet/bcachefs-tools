// SPDX-License-Identifier: GPL-2.0
//! `#[derive(RcuHead)]`: util/rcu.rs's HasRcuHead, at the one field marked
//! #[rcu_head], whose type has a head itself.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DataStruct, DeriveInput, Error, Index};

pub(crate) fn derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(Error::new_spanned(&input, format!("{name}: #[derive(RcuHead)] is for a struct")));
    };

    let mut heads = fields.iter().enumerate()
        .filter(|(_, f)| f.attrs.iter().any(|a| a.path().is_ident("rcu_head")));
    let Some((i, f)) = heads.next() else {
        return Err(Error::new_spanned(&input, format!("{name}: #[derive(RcuHead)] needs a field marked #[rcu_head]")));
    };
    if let Some((_, g)) = heads.next() {
        return Err(Error::new_spanned(g, format!("{name}: #[derive(RcuHead)]: a second field marked #[rcu_head]")));
    }

    let member = match &f.ident {
        Some(id) => quote!(#id),
        None => {
            let i = Index::from(i);
            quote!(#i)
        }
    };
    let ty = &f.ty;

    let (impl_g, ty_g, where_c) = input.generics.split_for_impl();
    Ok(quote! {
        unsafe impl #impl_g crate::util::rcu::HasRcuHead for #name #ty_g #where_c {
            const HEAD: usize = ::core::mem::offset_of!(Self, #member)
                + <#ty as crate::util::rcu::HasRcuHead>::HEAD;
            const WORK: bool = <#ty as crate::util::rcu::HasRcuHead>::WORK;
        }
    })
}
