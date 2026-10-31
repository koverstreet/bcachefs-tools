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

use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};

fn is_ident(t: &TokenTree, s: &str) -> bool {
    matches!(t, TokenTree::Ident(i) if i.to_string() == s)
}

fn is_punct(t: &TokenTree, c: char) -> bool {
    matches!(t, TokenTree::Punct(p) if p.as_char() == c)
}

/// @ts split at its top-level commas: commas outside <> - a group's are its own.
fn split_params(ts: &[TokenTree]) -> Vec<&[TokenTree]> {
    let (mut out, mut start, mut depth) = (Vec::new(), 0, 0i32);
    for (i, t) in ts.iter().enumerate() {
        match t {
            TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '>' && !(i > 0 && is_punct(&ts[i - 1], '-')) => depth -= 1,
            TokenTree::Punct(p) if p.as_char() == ',' && depth == 0 => {
                out.push(&ts[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < ts.len() {
        out.push(&ts[start..]);
    }
    out
}

/// A parameter the rewrite keeps as it is: `[mut] name: T`, or a receiver -
/// `[&['a]] [mut] self`.
fn check_param(p: &[TokenTree]) -> Result<(), String> {
    let p = match p {
        [amp, apos, _, rest @ ..] if is_punct(amp, '&') && is_punct(apos, '\'') => rest,
        [amp, rest @ ..] if is_punct(amp, '&') => rest,
        p => p,
    };
    let p = if p.first().is_some_and(|t| is_ident(t, "mut")) { &p[1..] } else { p };
    match p {
        [t] if is_ident(t, "self") => Ok(()),
        [TokenTree::Ident(name), colon, ..] if is_punct(colon, ':') && name.to_string() != "_" => Ok(()),
        _ => Err("a pattern or `_` parameter: the lowering's move is how those bind and drop - name it".into()),
    }
}

pub(crate) fn args_stored_once(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    if !attr.is_empty() {
        return Err("takes no arguments".into());
    }
    let tts: Vec<TokenTree> = item.into_iter().collect();
    let fn_at = tts.iter().position(|t| is_ident(t, "fn")).ok_or("expected an async fn")?;
    let async_at = tts[..fn_at].iter().position(|t| is_ident(t, "async")).ok_or("expected an async fn")?;
    let params_at = fn_at + tts[fn_at..].iter()
        .position(|t| matches!(t, TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis))
        .ok_or("expected its parameters")?;
    let Some(TokenTree::Group(body)) = tts.last() else { return Err("expected a body".into()) };
    if body.delimiter() != Delimiter::Brace {
        return Err("expected a body".into());
    }

    let TokenTree::Group(params) = &tts[params_at] else { unreachable!() };
    let params: Vec<TokenTree> = params.stream().into_iter().collect();
    for p in split_params(&params) {
        check_param(p)?;
    }

    let rest = &tts[params_at + 1..tts.len() - 1];
    let where_at = rest.iter().position(|t| is_ident(t, "where")).unwrap_or(rest.len());
    let ret: TokenStream = match &rest[..where_at] {
        [] => "()".parse().unwrap(),
        [a, b, ret @ ..] if is_punct(a, '-') && is_punct(b, '>') && !ret.is_empty() => ret.iter().cloned().collect(),
        _ => return Err("expected `-> T`, a where clause or the body after its parameters".into()),
    };

    // tokens moved, not re-parsed: errors in the body keep their spans
    let mut out: TokenStream = tts[..async_at].iter().chain(&tts[async_at + 1..=params_at]).cloned().collect();
    out.extend("-> impl ::core::future::Future<Output =".parse::<TokenStream>().unwrap());
    out.extend(ret);
    out.extend(">".parse::<TokenStream>().unwrap());
    out.extend(rest[where_at..].iter().cloned());
    let block: TokenStream = [TokenTree::Ident(Ident::new("async", body.span())),
                              TokenTree::Ident(Ident::new("move", body.span())),
                              TokenTree::Group(body.clone())].into_iter().collect();
    out.extend([TokenTree::Group(Group::new(Delimiter::Brace, block))]);
    Ok(out)
}
