// SPDX-License-Identifier: GPL-2.0
//! #[args_stored_once]: an async fn as `fn -> impl Future { async move }`.
//!
//! An async fn's lowering moves each by-value argument into a local of its
//! body, and the coroutine layout never reuses a captured argument's storage
//! once it's moved out: an argument live across an await is stored twice
//! (rust-lang/rust#62958; design-docs/bcachefs-rust/language-and-writeups.md
//! §8). An async block uses its captures in place, and drops them where the
//! async fn would have - at completion, after its locals - so for a plain
//! identifier parameter the rewrite changes only the layout.
//!
//! The `impl Future` is edition 2024's, as a macro's tokens are its crate's
//! edition: it captures every lifetime in scope, elided ones included, as
//! the async fn's future does. Patterns and `_` are refused: there the
//! lowering's move is how they bind and drop.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{parse_quote, Error, FnArg, ItemFn, Pat, ReturnType};

pub(crate) fn args_stored_once(attr: TokenStream, mut f: ItemFn) -> syn::Result<TokenStream> {
    if !attr.is_empty() {
        return Err(Error::new_spanned(attr, "takes no arguments"));
    }
    let ret = store_args_once(&mut f)?;
    f.sig.output = parse_quote!(-> impl ::core::future::Future<Output = #ret>);
    Ok(quote!(#f))
}

/// @f, an async fn, as a fn whose body is an async block: the output of its
/// future, for the caller to give @f the return type of.
pub(crate) fn store_args_once(f: &mut ItemFn) -> syn::Result<TokenStream> {
    if f.sig.asyncness.take().is_none() {
        return Err(Error::new_spanned(&f.sig, "expected an async fn"));
    }
    for p in &f.sig.inputs {
        match p {
            FnArg::Typed(t) if !matches!(&*t.pat, Pat::Ident(i) if i.by_ref.is_none() && i.subpat.is_none()) =>
                return Err(Error::new_spanned(&t.pat,
                    "a pattern or `_` parameter: the lowering's move is how those bind and drop - name it")),
            _ => {}
        }
    }

    let ret = match &f.sig.output {
        ReturnType::Default => quote!(()),
        ReturnType::Type(_, t) => quote!(#t),
    };
    let block = &f.block;
    f.block = parse_quote!({ async move #block });
    Ok(ret)
}
