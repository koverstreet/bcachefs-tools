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

use proc_macro::{Delimiter, Group, Ident, Literal, Spacing, TokenStream, TokenTree};

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

/// @ts split at its top level commas - a turbofish's included, whose angle
/// brackets aren't a group. A trailing comma makes no empty last piece.
fn split_commas(ts: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut out = vec![Vec::new()];
    let mut angle = 0usize;
    let mut prev_colons = false;

    for tt in ts {
        let colons = matches!(&tt, TokenTree::Punct(p) if p.as_char() == ':');
        match &tt {
            TokenTree::Punct(p) if p.as_char() == ',' && angle == 0 => {
                out.push(Vec::new());
                prev_colons = false;
                continue;
            }
            TokenTree::Punct(p) if p.as_char() == '<' && (prev_colons || angle > 0) => angle += 1,
            TokenTree::Punct(p) if p.as_char() == '>' && angle > 0 => angle -= 1,
            _ => {}
        }
        prev_colons = colons;
        out.last_mut().unwrap().push(tt);
    }
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// A macro_rules fragment ($x:literal, $x:expr) comes as a group of its own,
/// with no delimiters: what's in it.
fn unwrap_none(tt: &TokenTree) -> Option<TokenTree> {
    match tt {
        TokenTree::Group(g) if g.delimiter() == Delimiter::None => {
            let mut inner = g.stream().into_iter();
            match (inner.next(), inner.next()) {
                (Some(t), None) => unwrap_none(&t).or(Some(t)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn code(s: &str) -> TokenStream {
    s.parse().unwrap()
}

fn paren(ts: TokenStream) -> TokenTree {
    TokenTree::Group(Group::new(Delimiter::Parenthesis, ts))
}

/// ::core::write!(__bch_out, @lit, @arg);
fn write_call(lit: Literal, arg: Option<TokenStream>) -> TokenStream {
    let mut args = code("__bch_out,");
    args.extend([TokenTree::Literal(lit)]);
    if let Some(arg) = arg {
        args.extend(code(","));
        args.extend(arg);
    }
    let mut ts = code("::core::write!");
    ts.extend([paren(args)]);
    ts.extend(code(";"));
    ts
}

pub(crate) fn prt(input: TokenStream) -> Result<TokenStream, String> {
    let mut parts = split_commas(input).into_iter();
    let krate: TokenStream = parts.next().ok_or("no crate")?.into_iter().collect();
    let out: TokenStream = parts.next().ok_or("no printbuf")?.into_iter().collect();

    let fmt_part = parts.next().ok_or("no format string")?;
    let fmt_tt = match fmt_part.as_slice() {
        [tt] => unwrap_none(tt).unwrap_or_else(|| tt.clone()),
        _    => return Err("the format string must be a string literal".into()),
    };
    let TokenTree::Literal(fmt_lit) = &fmt_tt else {
        return Err("the format string must be a string literal".into());
    };
    let fmt_span = fmt_lit.span();
    let (open, close, pieces) = split_fmt(&fmt_lit.to_string())?;

    // The arguments, positional then named - bound as __bch_argN, N their
    // place here - and whether each is used
    let mut args: Vec<(Option<String>, TokenStream)> = Vec::new();
    for a in parts {
        match a.as_slice() {
            [TokenTree::Ident(name), TokenTree::Punct(eq), rest @ ..]
                if eq.as_char() == '=' && eq.spacing() == Spacing::Alone && !rest.is_empty() =>
                args.push((Some(name.to_string()), rest.iter().cloned().collect())),
            _ => args.push((None, a.into_iter().collect())),
        }
    }
    let nr_positional = args.iter().take_while(|(name, _)| name.is_none()).count();
    let mut used = vec![false; args.len()];

    let path = |rest: &str| -> TokenStream {
        let mut ts = krate.clone();
        ts.extend(code(rest));
        ts
    };

    let mut body = TokenStream::new();

    // let __bch_out: &mut CRATE::util::Printbuf = &mut *(OUT);
    body.extend(code("let __bch_out: &mut"));
    body.extend(path("::util::Printbuf"));
    body.extend(code("= &mut *"));
    body.extend([paren(out)]);
    body.extend(code(";"));

    body.extend(code("#[allow(unused_imports)] use"));
    body.extend(path("::util::printbuf::prt_arg::{ViaText as _, ViaDisplay as _};"));

    for p in pieces {
        match p {
            Piece::Lit(s) => {
                let mut lit: Literal = format!("{open}{s}{close}").parse().unwrap();
                lit.set_span(fmt_span);
                body.extend(write_call(lit, None));
            }
            Piece::Arg { arg, spec } => {
                // the argument, as a reference: an argument's binding, or an
                // inline capture - the caller's name, at the caller's span
                let mut bound = |i: usize| {
                    used[i] = true;
                    code(&format!("__bch_arg{i}"))
                };
                let arg_ts = match arg {
                    ArgRef::Index(i) if i < nr_positional => bound(i),
                    ArgRef::Index(i) => return Err(format!("format string refers to argument {i}, \
                                                            but there are {nr_positional}")),
                    ArgRef::Name(n) => match args.iter().position(|(name, _)| name.as_ref() == Some(&n)) {
                        Some(i) => bound(i),
                        None => {
                            let mut ts = code("&");
                            ts.extend([TokenTree::Ident(Ident::new(&n, fmt_span))]);
                            ts
                        }
                    },
                };

                if spec.is_empty() {
                    // (&CRATE::util::printbuf::prt_arg::Arg(ARG)).prt(__bch_out);
                    let mut call = code("&");
                    call.extend(path("::util::printbuf::prt_arg::Arg"));
                    call.extend([paren(arg_ts)]);
                    body.extend([paren(call)]);
                    body.extend(code(".prt(__bch_out);"));
                } else {
                    let mut lit: Literal = format!("\"{{:{spec}}}\"").parse()
                        .map_err(|_| format!("bad format spec `{spec}`"))?;
                    lit.set_span(fmt_span);
                    body.extend(write_call(lit, Some(arg_ts)));
                }
            }
        }
    }

    if let Some(i) = used.iter().position(|u| !u) {
        return Err(match &args[i].0 {
            Some(name) => format!("named argument `{name}` is never used"),
            None       => format!("argument {i} is never used"),
        });
    }

    // Each argument evaluated once, as format_args!() does it: borrowed in a
    // match's scrutinee, so the temporaries in it - k.to_text(fs)'s key - live
    // until the writes are done.
    //   match (&(A0), &(A1), ..) { (__bch_arg0, __bch_arg1, ..) => { body } }
    let mut scrutinee = TokenStream::new();
    let mut pattern = TokenStream::new();
    for (i, (_, a)) in args.into_iter().enumerate() {
        scrutinee.extend(code("&"));
        scrutinee.extend([paren(a)]);
        scrutinee.extend(code(","));
        pattern.extend(code(&format!("__bch_arg{i},")));
    }

    let mut ts = code("match");
    ts.extend([paren(scrutinee)]);
    let mut arm = TokenStream::new();
    arm.extend([paren(pattern)]);
    arm.extend(code("=>"));
    arm.extend([TokenTree::Group(Group::new(Delimiter::Brace, body))]);
    ts.extend([TokenTree::Group(Group::new(Delimiter::Brace, arm))]);
    Ok(ts)
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
