// SPDX-License-Identifier: GPL-2.0
//! #[derive(MaybeZeroable)]: Zeroable when every field is.

use proc_macro2::TokenStream;
use quote::quote;

use crate::cstruct::{self, generic_arg};

/// Zeroable when every field is - pin_init's #[derive(MaybeZeroable)], done
/// the same way: the for<'__dummy> makes each
/// bound a condition on the impl, not an error where it doesn't hold. So a
/// struct holding a Rust type with invariants - a lock - isn't, and
/// #[derive(Init)] can't zero it. core's atomics are, as their integers are,
/// though pin_init has no impl for them, and the orphan rule allows none
/// here: their fields aren't bounds. Nor has it one for Cell, whose zeroes
/// are its T's: a Cell's bound is its T's.
pub(crate) fn maybe_zeroable(def: &cstruct::StructDef) -> TokenStream {
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

