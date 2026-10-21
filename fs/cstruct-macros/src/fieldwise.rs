// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Fieldwise)]`: methods on a struct that apply the method of the
//! same name to each of its fields.
//!
//! `#[fieldwise(clamp_nonnegative, wrapping_add(rhs))]` makes
//!
//! ```text
//! fn clamp_nonnegative(self) -> Self
//! fn wrapping_add(self, rhs: Self) -> Self
//! ```
//!
//! each field becoming `self.f.clamp_nonnegative()`, `self.f.wrapping_add(rhs.f)`.
//! A bare name is unary, one with a parameter binary: a derive sees no types,
//! so the attribute says which. `Trait::method` makes it that trait's - one
//! impl per trait - for generic code to call. Arrays are mapped element by
//! element; a field of another such struct gets the method it was given - so
//! the methods take and return by value, the struct Copy, as integers' own are.

use proc_macro::TokenStream;

use crate::cstruct::{parse_cstruct, CKind};
use crate::tokens;

pub(crate) fn derive(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    if def.kind != CKind::Struct {
        return Err(format!("{}: #[derive(Fieldwise)] is for a struct", def.name));
    }
    if def.fieldwise.is_empty() {
        return Err(format!("{}: #[fieldwise(...)] names no methods", def.name));
    }

    // Inherent methods under None, a trait's under its name: one impl each.
    let mut impls: Vec<(Option<&str>, String)> = Vec::new();
    for m in &def.fieldwise {
        let mut fields = String::new();
        for f in &def.fields {
            if f.bitfield {
                return Err(format!("{}.{}: #[derive(Fieldwise)] can't see into a bitfield",
                                   def.name, f.name));
            }
            if let Some(cfg) = &f.cfg {
                fields.push_str(&format!("#[cfg({})] ", cfg.to_rust()));
            }
            let name = f.rust_name();
            let rhs = m.rhs.as_ref().map(|r| format!("{r}.{name}"));
            fields.push_str(&format!("{name}: {},\n",
                                     apply(&m.method, &f.ty, &format!("self.{name}"), rhs.as_deref(), 0)?));
        }

        let params = match &m.rhs {
            Some(r) => format!("self, {r}: Self"),
            None    => "self".to_string(),
        };
        let vis = if m.tr.is_none() { "pub " } else { "" };
        let method = format!("#[inline]\n{vis}fn {}({params}) -> Self {{\nSelf {{\n{fields}}}\n}}\n",
                             m.method);

        let tr = m.tr.as_deref();
        match impls.iter_mut().find(|(t, _)| *t == tr) {
            Some((_, body)) => body.push_str(&method),
            None            => impls.push((tr, method)),
        }
    }

    let ty = def.rust_type();
    Ok(tokens(&impls.iter().map(|(tr, body)| match tr {
        Some(tr) => format!("#[automatically_derived]\nimpl {tr} for {ty} {{\n{body}}}\n"),
        None     => format!("#[automatically_derived]\nimpl {ty} {{\n{body}}}\n"),
    }).collect::<String>()))
}

/// @method on @val, of type @ty - with @rhs, for a binary one - an array's
/// elements each, at @depth arrays in.
fn apply(method: &str, ty: &str, val: &str, rhs: Option<&str>, depth: usize) -> Result<String, String> {
    if let Some(elem) = array_elem(ty) {
        let i = format!("__i{depth}");
        let rhs = rhs.map(|r| format!("{r}[{i}]"));
        let e = apply(method, elem, &format!("{val}[{i}]"), rhs.as_deref(), depth + 1)?;
        return Ok(format!("::core::array::from_fn(|{i}| {e})"));
    }
    Ok(match rhs {
        Some(r) => format!("{val}.{method}({r})"),
        None    => format!("{val}.{method}()"),
    })
}

/// The element type of an array type, `[T; N]`.
fn array_elem(ty: &str) -> Option<&str> {
    let inner = ty.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut depth = 0i32;
    for (i, c) in inner.char_indices() {
        match c {
            '[' | '(' | '<' => depth += 1,
            ']' | ')' | '>' => depth -= 1,
            ';' if depth == 0 => return Some(inner[..i].trim()),
            _ => {}
        }
    }
    None
}
