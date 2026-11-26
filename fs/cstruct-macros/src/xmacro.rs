// SPDX-License-Identifier: GPL-2.0
//! xmacro! and xmacro_select!: x-macro lists, and the rows of one with a token.

use proc_macro2::{Delimiter, Group, Ident, TokenStream, TokenTree};
use quote::{format_ident, quote};

use crate::cstruct::{parse_xmacro, split_commas, XEntry};

/// xmacro_select!: NAME!(xmacro_select [TOKEN => cb]) - cb! handed the rows
/// with TOKEN in them.
pub(crate) fn select(input: TokenStream) -> Result<TokenStream, String> {
    let mut input = TokenStream::from(input).into_iter();
    let Some(TokenTree::Group(pre)) = input.next().filter(|t| matches!(t,
        TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket)) else {
        return Err("expected [TOKEN => cb] before the list".into());
    };
    let pre: Vec<TokenTree> = pre.stream().into_iter().collect();
    let (token, cb) = match pre.as_slice() {
        [TokenTree::Ident(token), TokenTree::Punct(eq), TokenTree::Punct(gt), cb @ ..]
            if eq.as_char() == '=' && gt.as_char() == '>' && !cb.is_empty() => (token.clone(), cb),
        _ => return Err("expected [TOKEN => cb] before the list".into()),
    };
    let (cb, args) = match cb {
        [cb @ .., TokenTree::Group(g)] if g.delimiter() == Delimiter::Brace => (cb, g.stream()),
        cb => (cb, TokenStream::new()),
    };
    let cb = cb.iter().cloned().collect::<TokenStream>();

    let is_token = |t: &TokenTree| matches!(t, TokenTree::Ident(i) if *i == token);
    let mut rows = Vec::new();
    for t in input {
        match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
                let set = g.stream().into_iter().find_map(|t| match t {
                    TokenTree::Group(s) if s.delimiter() == Delimiter::Bracket => Some(s.stream()),
                    _ => None,
                });
                if set.unwrap_or_else(|| g.stream()).into_iter().any(|t| is_token(&t)) {
                    rows.push(g);
                }
            }
            TokenTree::Punct(p) if p.as_char() == ',' => {}
            t => return Err(format!("expected a list of (args) rows, found {t}")),
        }
    }
    if rows.is_empty() {
        return Err(format!("no row has {token}"));
    }
    Ok(quote!(#cb! { #args #(#rows),* }))
}

/// xmacro!'s expansion: NAME!(cb), which calls cb! with the list's rows.
pub(crate) fn xmacro_expand(input: TokenStream) -> Result<TokenStream, String> {
    let x = parse_xmacro(input)?;
    let name = &x.name;
    // The plain entries from @from to @to: none of them a list spliced in
    let args = |from: usize, to: usize| x.entries[from..to].iter().map(|e| match e {
        XEntry::Args(g) => g,
        XEntry::Sub { .. } => unreachable!("taken between the lists spliced in"),
    });

    let mut out = TokenStream::new();
    let subs: Vec<usize> = x.entries.iter().enumerate()
        .filter(|(_, e)| matches!(e, XEntry::Sub { .. }))
        .map(|(i, _)| i)
        .collect();

    if subs.is_empty() {
        let entries = args(0, x.entries.len());
        out.extend(quote! {
            #[allow(unused_macros)]
            macro_rules! #name { ($($cb:ident)::+ $([$($pre:tt)*])?) => { $($cb)::+! { $([$($pre)*])? #(#entries),* } } }
            #[allow(unused_imports)]
            pub(crate) use #name;
        });
        return Ok(out);
    }

    // The plain entries before the first list spliced in, between each and
    // the next, and after the last.
    let lits = |from: usize, to: usize| {
        let e = args(from, to);
        quote!(#(#e,)*)
    };
    let path = x.module.as_ref().ok_or_else(|| format!(
        "xmacro! {name}: no `in MODULE;` - it splices in other lists, a chain of macros that name each other by it"))?;
    let carry = quote!(($($cb)::+) ($($pre)?));
    let step = |n: usize| format_ident!("__{name}_{n}");
    for (n, &i) in subs.iter().enumerate() {
        let XEntry::Sub { map, .. } = &x.entries[i] else { unreachable!() };
        let (pattern, mapped) = match map {
            None => (quote!(($($e:tt)*)), quote!(($($e)*))),
            Some(m) => {
                let params = &m.params;
                let template = xmacro_template(m.template.clone(), params);
                (quote!((#($#params:tt),*)), quote!((#template)))
            }
        };
        let after = lits(i + 1, subs.get(n + 1).copied().unwrap_or(x.entries.len()));
        let acc = quote!($($acc)* $(#mapped,)* #after);
        let next = match subs.get(n + 1) {
            Some(&j) => {
                let XEntry::Sub { list: next_list, .. } = &x.entries[j] else { unreachable!() };
                let next_step = step(n + 1);
                quote!(#path::#next_list!(#path::#next_step [#carry #acc]);)
            }
            None => quote!($($cb)::+! { $($pre)? #acc }),
        };
        let this = step(n);
        out.extend(quote! {
            #[allow(unused_macros)]
            macro_rules! #this {
                ([($($cb:ident)::+) ($($pre:tt)?) $($acc:tt)*] $(#pattern),* $(,)?) => { ::paste::paste! { #next } };
            }
            #[allow(unused_imports)]
            pub(crate) use #this;
        });
    }
    // The callback's [pre] bracketed, as the plain list's: an optional single
    // tt after a path callback is ambiguous with its `::`.
    let XEntry::Sub { list: first, .. } = &x.entries[subs[0]] else { unreachable!() };
    let (step0, lits0) = (step(0), lits(0, subs[0]));
    out.extend(quote! {
        #[allow(unused_macros)]
        macro_rules! #name {
            ($($cb:ident)::+ $([$($pre:tt)*])?) => { #path::#first!(#path::#step0 [($($cb)::+) ($([$($pre)*])?) #lits0]); }
        }
        #[allow(unused_imports)]
        pub(crate) use #name;
    });
    Ok(out)
}

/// A mapped list's entry, @template: its @params $p, and concat(A, b, ..)
/// paste!'s [<A b ..>] - the mapping is expanded inside paste!.
fn xmacro_template(template: TokenStream, params: &[Ident]) -> TokenStream {
    let t: Vec<TokenTree> = template.into_iter().collect();
    let mut out = TokenStream::new();
    let mut k = 0;
    while k < t.len() {
        match (&t[k], t.get(k + 1)) {
            (TokenTree::Ident(c), Some(TokenTree::Group(g))) if c == "concat" && g.delimiter() == Delimiter::Parenthesis => {
                let parts = split_commas(g.stream()).into_iter().map(|p| xmacro_template(p, params));
                out.extend(quote!([< #(#parts)* >]));
                k += 2;
                continue;
            }
            (TokenTree::Ident(i), _) if params.contains(i) => out.extend(quote!($#i)),
            (TokenTree::Group(g), _) => {
                let mut inner = Group::new(g.delimiter(), xmacro_template(g.stream(), params));
                inner.set_span(g.span());
                out.extend([TokenTree::Group(inner)]);
            }
            (t, _) => out.extend([t.clone()]),
        }
        k += 1;
    }
    out
}

