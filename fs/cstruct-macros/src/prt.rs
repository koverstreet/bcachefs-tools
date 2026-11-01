// SPDX-License-Identifier: GPL-2.0
//! message!()'s format string, written into a printbuf piece by piece: what
//! format_args!() would format, except that an argument that's ToText is
//! to_text()'d into the printbuf itself - not formatted through Display, into
//! a printbuf of its own on the stack, and copied in.
//!
//!   __prt!(CRATE, OUT, "fmt", args..)
//!
//! CRATE is the caller's $crate; OUT an expression that derefs to the
//! Printbuf. The literal text between placeholders is written with write!(),
//! so its escapes and {{ }} mean what they mean in any format string. A
//! placeholder with no spec - {}, {0}, {name}, {x} - goes through
//! printbuf::prt_arg, which picks ToText over Display at compile time; one
//! with a spec - {:x}, {:>10}, {:?} - is write!()'s.
//!
//! As format_args!(): each argument evaluated once, and one never used is an
//! error. Not supported: a spec taking its width or precision from an argument
//! ($ or *).
//!
//! The arguments' tokens go into the expansion as they came: called from
//! message!(), a macro_rules macro, code made from text would resolve names
//! in its expansion, where the caller's locals aren't - so an inline {x}'s
//! ident takes the format string's span, as format_args!()'s does.

use proc_macro2::{Ident, Literal, Span, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens as _};
use syn::parse::{Parse, ParseStream};
use syn::{Error, Expr, LitStr, Token};

/// What a placeholder names: a positional argument - {} numbered in order, as
/// format_args!() numbers them - or a name, a named argument's or the
/// caller's variable.
#[derive(Debug, PartialEq)]
enum ArgRef {
    Index(usize),
    Name(String),
}

#[derive(Debug, PartialEq)]
enum Piece {
    /// Source text, as it was in the literal: escapes and {{ }} as written
    Lit(String),
    Arg { arg: ArgRef, spec: String },
}

/// The format string's literal source, its opening and closing quotes
/// (r#" "# for a raw one), and its pieces.
fn split_fmt(src: &str) -> Result<(String, String, Vec<Piece>), String> {
    let (open, close, body, raw) = if let Some(r) = src.strip_prefix('r') {
        let hashes = r.len() - r.trim_start_matches('#').len();
        let open = format!("r{}\"", "#".repeat(hashes));
        let close = format!("\"{}", "#".repeat(hashes));
        let body = src.strip_prefix(open.as_str()).and_then(|b| b.strip_suffix(close.as_str()))
            .ok_or("the format string isn't a string literal")?;
        (open, close, body, true)
    } else {
        let body = src.strip_prefix('"').and_then(|b| b.strip_suffix('"'))
            .ok_or("the format string isn't a string literal")?;
        ("\"".to_string(), "\"".to_string(), body, false)
    };

    let mut pieces = Vec::new();
    let mut lit = String::new();
    let mut next = 0;
    let mut chars = body.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // an escape, whole: \u{..}'s braces are no placeholder
            '\\' if !raw => {
                lit.push(ch);
                if let Some(next) = chars.next() {
                    lit.push(next);
                    if next == 'u' && chars.peek() == Some(&'{') {
                        for c in chars.by_ref() {
                            lit.push(c);
                            if c == '}' {
                                break;
                            }
                        }
                    }
                }
            }
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push_str("{{");
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push_str("}}");
            }
            '}' => return Err("unmatched `}` in the format string".into()),
            '{' => {
                let mut ph = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c)   => ph.push(c),
                        None      => return Err("unterminated `{` in the format string".into()),
                    }
                }
                let (arg, spec) = ph.split_once(':').unwrap_or((&ph, ""));
                let arg = arg.trim();
                if spec.contains('$') || spec.contains('*') {
                    return Err(format!("{{{ph}}}: a width or precision from an argument isn't supported"));
                }
                let arg = if arg.is_empty() {
                    next += 1;
                    ArgRef::Index(next - 1)
                } else if let Ok(i) = arg.parse() {
                    ArgRef::Index(i)
                } else {
                    ArgRef::Name(arg.to_string())
                };

                if !lit.is_empty() {
                    pieces.push(Piece::Lit(core::mem::take(&mut lit)));
                }
                pieces.push(Piece::Arg { arg, spec: spec.to_string() });
            }
            _ => lit.push(ch),
        }
    }
    if !lit.is_empty() {
        pieces.push(Piece::Lit(lit));
    }
    Ok((open, close, pieces))
}

/// __prt!'s input: CRATE, OUT, "fmt", then the arguments - positional, then
/// `name = expr`.
pub(crate) struct PrtInput {
    krate: TokenTree,
    out:   Expr,
    fmt:   LitStr,
    args:  Vec<(Option<Ident>, Expr)>,
}

impl Parse for PrtInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let krate = input.parse()?;
        input.parse::<Token![,]>()?;
        let out = input.parse()?;
        input.parse::<Token![,]>()?;
        let fmt = input.parse().map_err(|e| Error::new(e.span(), "the format string must be a string literal"))?;
        let mut args = Vec::new();
        while input.parse::<Option<Token![,]>>()?.is_some() && !input.is_empty() {
            let name = match input.peek(syn::Ident) && input.peek2(Token![=]) && !input.peek2(Token![==]) {
                true => {
                    let name = input.parse()?;
                    input.parse::<Token![=]>()?;
                    Some(name)
                }
                false => None,
            };
            args.push((name, input.parse()?));
        }
        if !input.is_empty() {
            return Err(input.error("expected `,` between the arguments"));
        }
        Ok(PrtInput { krate, out, fmt, args })
    }
}

/// A string literal of @src, quotes included, at @span.
fn lit_at(src: &str, span: Span) -> syn::Result<Literal> {
    let mut lit: Literal = src.parse().map_err(|_| Error::new(span, format!("bad format piece {src}")))?;
    lit.set_span(span);
    Ok(lit)
}

pub(crate) fn prt(PrtInput { krate, out, fmt, args }: PrtInput) -> syn::Result<TokenStream> {
    let fmt_span = fmt.span();
    let (open, close, pieces) = split_fmt(&fmt.token().to_string()).map_err(|e| Error::new(fmt_span, e))?;

    // The arguments, positional then named - bound as __bch_argN, N their
    // place here - and whether each is used
    let nr_positional = args.iter().take_while(|(name, _)| name.is_none()).count();
    let mut used = vec![false; args.len()];
    let binding = |i: usize| format_ident!("__bch_arg{i}");

    let mut body = Vec::new();
    for p in pieces {
        match p {
            Piece::Lit(s) => {
                let lit = lit_at(&format!("{open}{s}{close}"), fmt_span)?;
                body.push(quote!(::core::write!(__bch_out, #lit);));
            }
            Piece::Arg { arg, spec } => {
                // the argument, as a reference: an argument's binding, or an
                // inline capture - the caller's name, at the caller's span
                let arg = match arg {
                    ArgRef::Index(i) if i < nr_positional => {
                        used[i] = true;
                        binding(i).into_token_stream()
                    }
                    ArgRef::Index(i) => return Err(Error::new(fmt_span,
                        format!("format string refers to argument {i}, but there are {nr_positional}"))),
                    ArgRef::Name(n) => match args.iter().position(|(name, _)| name.as_ref().is_some_and(|m| *m == n)) {
                        Some(i) => {
                            used[i] = true;
                            binding(i).into_token_stream()
                        }
                        None => {
                            let n = Ident::new(&n, fmt_span);
                            quote!(&#n)
                        }
                    },
                };

                body.push(match spec.is_empty() {
                    true => quote!((&#krate::util::printbuf::prt_arg::Arg(#arg)).prt(__bch_out);),
                    false => {
                        let lit = lit_at(&format!("\"{{:{spec}}}\""), fmt_span)?;
                        quote!(::core::write!(__bch_out, #lit, #arg);)
                    }
                });
            }
        }
    }

    if let Some(i) = used.iter().position(|u| !u) {
        return Err(match &args[i] {
            (Some(name), _) => Error::new_spanned(name, format!("named argument `{name}` is never used")),
            (None, a)       => Error::new_spanned(a, format!("argument {i} is never used")),
        });
    }

    // Each argument evaluated once, as format_args!() does it: borrowed in a
    // match's scrutinee, so the temporaries in it - k.to_text(fs)'s key - live
    // until the writes are done.
    let exprs = args.iter().map(|(_, a)| a);
    let bindings = (0..args.len()).map(binding);
    Ok(quote! {
        match (#(&(#exprs),)*) {
            (#(#bindings,)*) => {
                let __bch_out: &mut #krate::util::Printbuf = &mut *(#out);
                #[allow(unused_imports)]
                use #krate::util::printbuf::prt_arg::{ViaText as _, ViaDisplay as _};
                #(#body)*
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Piece {
        Piece::Lit(s.into())
    }

    fn arg(arg: ArgRef, spec: &str) -> Piece {
        Piece::Arg { arg, spec: spec.into() }
    }

    #[test]
    fn pieces() {
        let (_, _, p) = split_fmt(r#""a {} b {x} c {0:>4}{} {{d}} \u{1F600}\n""#).unwrap();
        assert_eq!(p, vec![lit("a "), arg(ArgRef::Index(0), ""), lit(" b "),
                           arg(ArgRef::Name("x".into()), ""), lit(" c "),
                           arg(ArgRef::Index(0), ">4"), arg(ArgRef::Index(1), ""),
                           lit(r" {{d}} \u{1F600}\n")]);
    }

    #[test]
    fn raw() {
        let (open, close, p) = split_fmt(r###"r#"\{}"#"###).unwrap();
        assert_eq!((open.as_str(), close.as_str()), ("r#\"", "\"#"));
        assert_eq!(p, vec![lit(r"\"), arg(ArgRef::Index(0), "")]);
    }

    #[test]
    fn errors() {
        assert!(split_fmt(r#""a } b""#).is_err());
        assert!(split_fmt(r#""a { b""#).is_err());
        assert!(split_fmt(r#""{:1$}""#).is_err());
    }
}
