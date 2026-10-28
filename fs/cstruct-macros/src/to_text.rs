// SPDX-License-Identifier: GPL-2.0
//! `#[derive(ToText)]`: a fieldless enum, printed as its variant's name -
//! util/printbuf.rs's, here because this is the crate our macros live in.
//!
//! A variant prints as its own name, or as `#[label("..")]` names it: c_enum!
//! labels the variants an x-macro makes with the name the list gives each, as
//! C's string tables have them. It makes
//!
//! ```text
//! pub const fn name(&self) -> &'static str
//! impl ToText, writing name()
//! impl Display, through ToText
//! ```
//!
//! An enum with generics, or a variant with fields, has nothing it could
//! print as a name alone: an error.

use proc_macro::{Delimiter, Punct, Spacing, TokenStream, TokenTree};

use crate::tokens;

pub(crate) fn derive(input: TokenStream) -> Result<TokenStream, String> {
    let mut trees = input.into_iter();

    // past its attributes and visibility, to `enum NAME`
    let name = loop {
        match trees.next() {
            Some(TokenTree::Ident(i)) if i.to_string() == "enum" => match trees.next() {
                Some(TokenTree::Ident(n)) => break n.to_string(),
                _ => return Err("expected the enum's name".into()),
            },
            Some(TokenTree::Ident(i)) if ["struct", "union"].contains(&i.to_string().as_str()) =>
                return Err("for an enum".into()),
            Some(_) => {}
            None => return Err("expected an enum".into()),
        }
    };
    let body = match trees.next() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => g.stream(),
        _ => return Err(format!("{name}: a generic enum has no one set of names")),
    };

    let mut arms = String::new();
    let mut variant: Vec<TokenTree> = Vec::new();
    for t in body.into_iter().chain([TokenTree::Punct(Punct::new(',', Spacing::Alone))]) {
        match t {
            TokenTree::Punct(p) if p.as_char() == ',' => {
                if !variant.is_empty() {
                    arms.push_str(&arm(&name, &variant)?);
                }
                variant.clear();
            }
            t => variant.push(t),
        }
    }

    Ok(tokens(&format!(
        "#[automatically_derived]\n\
         impl {name} {{\n\
         \x20   /// What it prints as.\n\
         \x20   pub const fn name(&self) -> &'static str {{\n\
         \x20       match *self {{\n{arms}        }}\n\
         \x20   }}\n\
         }}\n\
         #[automatically_derived]\n\
         impl crate::util::ToText for {name} {{\n\
         \x20   fn to_text(&self, out: &mut crate::util::Printbuf) {{\n\
         \x20       out.write_fmt(::core::format_args!(\"{{}}\", self.name()))\n\
         \x20   }}\n\
         }}\n\
         crate::display_via_to_text!({name});\n")))
}

/// One variant's match arm: its tokens, attributes first.
fn arm(enum_name: &str, toks: &[TokenTree]) -> Result<String, String> {
    let mut label = None;
    let mut i = 0;
    while let (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g))) = (toks.get(i), toks.get(i + 1)) {
        if p.as_char() != '#' || g.delimiter() != Delimiter::Bracket {
            break;
        }
        let attr: Vec<TokenTree> = g.stream().into_iter().collect();
        if let [TokenTree::Ident(n), TokenTree::Group(args)] = &attr[..] {
            if n.to_string() == "label" {
                match &args.stream().into_iter().collect::<Vec<_>>()[..] {
                    [TokenTree::Literal(l)] if l.to_string().starts_with('"') => label = Some(l.to_string()),
                    _ => return Err(format!("{enum_name}: #[label(\"name\")] takes a string")),
                }
            }
        }
        i += 2;
    }

    let Some(TokenTree::Ident(v)) = toks.get(i) else {
        return Err(format!("{enum_name}: expected a variant name"));
    };
    match toks.get(i + 1) {
        None => {}
        Some(TokenTree::Punct(p)) if p.as_char() == '=' => {}
        Some(_) => return Err(format!("{enum_name}::{v}: a variant with fields has no name to print")),
    }
    let label = label.unwrap_or_else(|| format!("\"{v}\""));
    Ok(format!("            Self::{v} => {label},\n"))
}
