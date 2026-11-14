// SPDX-License-Identifier: GPL-2.0
//! Token helpers the macros share.

use proc_macro2::{Group, Ident, Spacing, Span, TokenStream, TokenTree};
use quote::{format_ident, quote};

/// util/vstructs.rs, where what the generated code uses is.
pub(crate) fn vs() -> TokenStream {
    quote!(crate::util::vstructs)
}

/// The name of @name's generated @part - its head, its storage - cased as
/// @name is: bch_sb_head, DiskAccountingPosStorage.
pub(crate) fn derived(name: &Ident, part: &str) -> Ident {
    match name.to_string().contains(|c: char| c.is_ascii_uppercase()) {
        true => {
            let (first, rest) = part.split_at(1);
            format_ident!("{name}{}{rest}", first.to_ascii_uppercase())
        }
        false => format_ident!("{name}_{part}"),
    }
}

/// @ts's top-level comma separated items - commas in a type's <..> aren't -
/// less empty ones: a trailing comma's.
pub(crate) fn comma_list(ts: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut items = vec![Vec::new()];
    let mut depth = 0;
    // a '>' of => or -> closes nothing
    let mut arrow = false;
    for t in ts {
        let joint_arrow = matches!(&t, TokenTree::Punct(p)
                                   if matches!(p.as_char(), '=' | '-') && p.spacing() == Spacing::Joint);
        match &t {
            TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '>' && !arrow => depth -= 1,
            TokenTree::Punct(p) if p.as_char() == ',' && depth == 0 => {
                items.push(Vec::new());
                arrow = false;
                continue;
            }
            _ => {}
        }
        arrow = joint_arrow;
        items.last_mut().unwrap().push(t);
    }
    items.retain(|i| !i.is_empty());
    items
}

/// @span's location, as the macro's: an error points there, while what's
/// generated from it - bitfield-struct's with_type__checked() of a member
/// type_ - isn't linted as written code.
pub(crate) fn at(span: Span) -> Span {
    Span::call_site().located_at(span)
}

/// @ts, every token at(@span): for what a macro makes in place of what's
/// written.
pub(crate) fn respan(ts: TokenStream, span: Span) -> TokenStream {
    let span = at(span);
    ts.into_iter().map(|t| match t {
        TokenTree::Group(g) => {
            let mut n = Group::new(g.delimiter(), respan(g.stream(), span));
            n.set_span(span);
            n.into()
        }
        mut t => { t.set_span(span); t }
    }).collect()
}

/// `KEY = VALUE`: KEY, and VALUE.
pub(crate) fn key_value(toks: &TokenStream) -> Option<(Ident, TokenStream)> {
    match &toks.clone().into_iter().collect::<Vec<_>>()[..] {
        [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' && !v.is_empty() =>
            Some((k.clone(), v.iter().cloned().collect())),
        _ => None,
    }
}

