// SPDX-License-Identifier: GPL-2.0
//! c_enum!'s declaration: an enum written as Rust writes one, each variant
//! with its doc, its columns and its value - and a key type's, its value's
//! type, `deleted(c::Deleted)`:
//!
//!   #[arm_columns(..)]   columns, each a const fn of the enum, a variant's
//!                        written #[COLUMN(..)]:
//!     NAME: FLAGS          a set of #[flags] FLAGS, by its names - a key
//!                          type's flags
//!     NAME: set(ENUM)      a set of ENUM's values, a u64 a bit each - a
//!                          btree's key types
//!     NAME: enum([closed,] K: REPR, prefix = P[, nr = NR][, c_name = C])
//!                          an enum K of the same variants, valued by the
//!                          column: its fn is K's variant - a recovery pass's
//!                          stable id
//!     NAME: value(T)       a T, any const expression - an inode option's
//!                          width
//!   #[mask(NAME: REPR, prefix = P[, c_name = C])]
//!                        the #[flags] NAME of bit numbers, 1 << each value;
//!                        a variant's #[mask_prefix(P)] is its prefix from
//!                        that variant on
//!   NAME_arms!(cb [pre]) cb! { [pre] (name, value, [Type], [column]..), .. }:
//!                        the variants as a list's rows, for what expands
//!                        them - a table, xmacro_select
//!
//! An open enum's enum and value columns are Options, for the values no
//! variant has, and its sets empty. A value is an integer literal or any
//! const expression - BCH_VERSION(1, 40) - and one not written the last's
//! plus one.
//! tagged_union!'s tag enum is one, its arms the union's.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::punctuated::Punctuated;

use crate::cstruct::{parse_enum_head, parse_variant, rust_text, split_commas, CEnumDef, CEnumKind,
                     CEnumVariant};
use crate::{c_enum_gen, comma_list, tokens, EnumOf};

pub(crate) struct Arm {
    pub docs:  Vec<String>,
    pub name:  String,
    /// What it prints as, `#[label("..")]`, where that isn't its name
    pub label: Option<String>,
    /// Its value's type, `name(Type)`
    pub ty:    Option<String>,
    pub value: String,
    pub cols:  Vec<Vec<String>>,
    /// The #[mask]'s prefix from this arm on, `#[mask_prefix(P)]`
    pub mask_prefix: Option<String>,
}

/// An #[arm_columns] column: a set of a #[flags] c_enum!'s flags, or of
/// another enum's values, a bit each; another enum's variant, of the arm's
/// name; or a value of a type.
pub(crate) enum Column {
    Flags(String),
    Set(String),
    Enum(CEnumDef),
    Value(String),
}

pub(crate) type Columns = Vec<(String, Column)>;

fn is_punct(t: Option<&TokenTree>, c: char) -> bool {
    matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == c)
}

/// #[arm_columns(NAME: FLAGS | NAME: set(ENUM) | NAME: enum(..) | NAME: value(T), ..)]
pub(crate) fn columns(args: TokenStream) -> Result<Columns, String> {
    comma_list(args).into_iter().map(|c| Ok(match c.as_slice() {
        [TokenTree::Ident(n), colon, TokenTree::Ident(kind), TokenTree::Group(g)]
            if is_punct(Some(colon), ':') && g.delimiter() == Delimiter::Parenthesis && kind == "set" =>
            (n.to_string(), Column::Set(rust_text(&g.stream()))),
        [TokenTree::Ident(n), colon, TokenTree::Ident(kind), TokenTree::Group(g)]
            if is_punct(Some(colon), ':') && g.delimiter() == Delimiter::Parenthesis && kind == "enum" =>
            (n.to_string(), Column::Enum(tag_enum(g.stream())?)),
        [TokenTree::Ident(n), colon, TokenTree::Ident(kind), TokenTree::Group(g)]
            if is_punct(Some(colon), ':') && g.delimiter() == Delimiter::Parenthesis && kind == "value" =>
            (n.to_string(), Column::Value(rust_text(&g.stream()))),
        [TokenTree::Ident(n), colon, ty @ ..] if is_punct(Some(colon), ':') && !ty.is_empty() =>
            (n.to_string(), Column::Flags(rust_text(&ty.iter().cloned().collect::<TokenStream>()))),
        _ => return Err("#[arm_columns(NAME: FLAGS | NAME: set(ENUM) | NAME: enum(..) | NAME: value(T), ..)]".to_string()),
    })).collect()
}

/// The arms of @body: `[#[COLUMN(..)]..] name[(Type)] [= value]`, a value not
/// written the last's plus one. With @typed, each has its Type.
pub(crate) fn arms(body: TokenStream, columns: &Columns, typed: bool) -> Result<Vec<Arm>, String> {
    let mut arms = Vec::new();
    // the last value written, and how many arms after it this one is
    let mut base: (Option<String>, u64) = (None, 0);
    for v in split_commas(body) {
        let (v, attrs, ty) = parse_variant(v).map_err(|e| e.to_string())?;
        let name = v.name;
        if typed && ty.is_none() {
            return Err(format!("{name}: an arm is `name(Type) [= value]`"));
        }
        let mut cols: Vec<Vec<String>> = vec![Vec::new(); columns.len()];
        let mut mask_prefix = None;
        for a in attrs {
            if a.path().is_ident("mask_prefix") {
                let p: syn::Ident = a.parse_args().map_err(|e| format!("{name}: #[mask_prefix(P)]: {e}"))?;
                mask_prefix = Some(p.to_string());
                continue;
            }
            let i = columns.iter().position(|(c, _)| a.path().is_ident(c))
                .ok_or_else(|| format!("{name}: #[{}]: not one of its #[arm_columns]", rust_text(a.path())))?;
            let err = |e: syn::Error| format!("{name}: #[{}(..)]: {e}", columns[i].0);
            cols[i] = match columns[i].1 {
                Column::Enum(_) | Column::Value(_) => vec![rust_text(&a.parse_args::<TokenStream>().map_err(err)?)],
                _ => a.parse_args_with(Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated).map_err(err)?
                    .iter().map(|f| f.to_string()).collect(),
            };
        }
        let value = match v.value {
            Some(val) => {
                base = (Some(val.clone()), 0);
                val
            }
            None => match &base.0 {
                None => base.1.to_string(),
                Some(b) => match parse_int(b) {
                    Some(n) => (n + base.1).to_string(),
                    None => format!("({b}) + {}", base.1),
                },
            },
        };
        base.1 += 1;
        if let Some(((c, _), _)) = columns.iter().zip(&cols)
            .find(|((_, c), v)| matches!(c, Column::Enum(_) | Column::Value(_)) && v.is_empty()) {
            return Err(format!("{name}: no #[{c}(..)]"));
        }
        arms.push(Arm { docs: v.docs, name, label: v.label, ty: ty.map(|t| rust_text(&t)), value, cols, mask_prefix });
    }
    // the entries are one shape: every arm's (name, value, Type, ..), or every one's (name, value, ..)
    if arms.iter().any(|a| a.ty.is_some()) && arms.iter().any(|a| a.ty.is_none()) {
        return Err("an arm with a type and one without: every arm has its type, or none does".into());
    }
    Ok(arms)
}

/// c_enum! of a declaration: `[#[..]] pub enum NAME|_: REPR { arms }`.
pub(crate) fn c_enum(input: TokenStream) -> Result<TokenStream, String> {
    let (e, attrs, body) = syn::parse::Parser::parse2(parse_enum_head, input).map_err(|e| e.to_string())?;
    let err = |m: String| format!("{}: {m}", e.name.as_deref().unwrap_or("_"));
    let (mut columns, mut mask) = (Vec::new(), None);
    for a in attrs {
        let args = || a.parse_args::<TokenStream>().map_err(|e| err(e.to_string()));
        match a.path().get_ident().map(|i| i.to_string()).as_deref() {
            Some("arm_columns") => columns = self::columns(args()?).map_err(err)?,
            Some("mask")        => mask = Some(CEnumDef { kind: CEnumKind::Flags, ..tag_enum(args()?).map_err(err)? }),
            Some("doc")         => {}
            _ => return Err(err(format!("#[{}]: not one of c_enum!'s", rust_text(a.path())))),
        }
    }
    if (e.kind == CEnumKind::Flags || e.name.is_none()) && !columns.is_empty() {
        return Err(err("#[arm_columns] are a named enum's, not flags' - bits".into()));
    }
    if e.kind == CEnumKind::Flags && mask.is_some() {
        return Err(err("#[mask] is of bit numbers, not flags".into()));
    }
    let arms = arms(body, &columns, false).map_err(err)?;
    if mask.is_none() && arms.iter().any(|a| a.mask_prefix.is_some()) {
        return Err(err("#[mask_prefix] without a #[mask]".into()));
    }

    let mut out = generate(&e, &arms, &columns);
    if let Some(name) = &e.name {
        out.extend(entries(name, &arms));
    }
    for (i, (_, col)) in columns.iter().enumerate() {
        if let Column::Enum(k) = col {
            out.extend(enum_of(k, arms.iter().map(|a| (a, a.cols[i][0].as_str()))));
        }
    }
    if let Some(m) = &mask {
        out.extend(mask_of(m, &e, &arms));
    }
    Ok(out)
}

/// `[closed,] NAME: REPR, prefix = P[, nr = NR][, c_name = C]`: a tag enum's,
/// or an enum column's.
pub(crate) fn tag_enum(args: TokenStream) -> Result<CEnumDef, String> {
    let mut e = CEnumDef { kind: CEnumKind::Open, name: None, repr: String::new(), variants: Vec::new(),
                           from: Vec::new(), own_to_text: false, c_name: None, prefix: None, nr: None };
    for part in comma_list(args) {
        match part.as_slice() {
            [TokenTree::Ident(c)] if c == "closed" => e.kind = CEnumKind::Closed,
            [TokenTree::Ident(c)] if c == "open" => {}
            [TokenTree::Ident(n), c, TokenTree::Ident(r)] if is_punct(Some(c), ':') =>
                (e.name, e.repr) = (Some(n.to_string()), r.to_string()),
            [TokenTree::Ident(k), eq, TokenTree::Ident(v)] if is_punct(Some(eq), '=') => match k.to_string().as_str() {
                "prefix" => e.prefix = Some(v.to_string()),
                "nr"     => e.nr = Some(v.to_string()),
                "c_name" => e.c_name = Some(v.to_string()),
                _ => return Err(format!("{k} = ..: not prefix, nr or c_name")),
            },
            _ => return Err("expected [closed,] NAME: REPR, prefix = PREFIX[, nr = NR][, c_name = C]".into()),
        }
    }
    if e.name.is_none() || e.prefix.is_none() {
        return Err("an enum needs NAME: REPR and prefix = PREFIX".into());
    }
    Ok(e)
}

/// Enum @e of @arms, each valued by its &str: the c_enum! it is, as its Rust
/// and its record.
fn enum_of<'a>(e: &CEnumDef, arms: impl Iterator<Item = (&'a Arm, &'a str)>) -> TokenStream {
    let def = CEnumDef {
        variants: arms.map(|(a, v)| CEnumVariant {
            name:   a.name.clone(),
            c_name: None,
            // an expression's type may not be the enum's, BCH_VERSION()'s
            // u64: as a list's value is, it's cast
            value:  Some(match parse_int(v) {
                Some(_) => v.to_string(),
                None => format!("(({v}) as {})", e.repr),
            }),
            label:  a.label.clone(),
            docs:   a.docs.clone(),
        }).collect(),
        ..e.clone()
    };
    c_enum_gen(&def, EnumOf::C)
}

/// #[mask(NAME: REPR, prefix = P[, c_name = C])]: flags @m of bit numbers
/// @bits, a flag per arm, 1 << its value - named P and the arm's name, or the
/// #[mask_prefix] last written on an arm up to it and the name. A flag prints
/// as its arm's name, as a list's does.
fn mask_of(m: &CEnumDef, bits: &CEnumDef, arms: &[Arm]) -> TokenStream {
    let mut prefix = m.prefix.clone();
    let def = CEnumDef {
        variants: arms.iter().map(|a| {
            prefix = a.mask_prefix.clone().or(prefix.take());
            let p = prefix.as_deref().unwrap_or_default();
            // an arm written with its prefix, one not an identifier past it,
            // is a flag written with the mask's
            let past = bits.prefix.as_deref().and_then(|b| a.name.strip_prefix(b));
            let name = past.unwrap_or(&a.name);
            CEnumVariant {
                name:   past.map_or_else(|| a.name.clone(), |n| format!("{p}{n}")),
                c_name: (prefix != m.prefix).then(|| format!("{p}{name}")),
                value:  Some(format!("1{} << {}", m.repr, bits.c_name_of(&a.name))),
                label:  Some(a.label.clone().unwrap_or_else(|| name.to_string())),
                docs:   a.docs.clone(),
            }
        }).collect(),
        ..m.clone()
    };
    c_enum_gen(&def, EnumOf::C)
}

/// Enum @e of @arms, and a const fn of it per column.
pub(crate) fn generate(e: &CEnumDef, arms: &[Arm], columns: &Columns) -> TokenStream {
    let mut out = enum_of(e, arms.iter().map(|a| (a, a.value.as_str())));

    let name = e.name.as_deref().unwrap_or("_");
    // a tag no arm has: none of a set; no value, so a value's is an Option
    let other = e.kind != CEnumKind::Closed;
    let fns: String = columns.iter().enumerate()
        .map(|(i, (col_name, col))| {
            let fn_arms: String = arms.iter()
                .map(|a| format!("Self::{} => {},\n", a.name, col.value(&a.name, &a.cols[i], other)))
                .chain(other.then(|| format!("_ => {},\n", col.none())))
                .collect();
            format!("pub const fn {col_name}(self) -> {} {{\n match self {{\n{fn_arms}}}\n}}\n", col.ty(other))
        })
        .collect();
    if !fns.is_empty() {
        out.extend(tokens(&format!("#[allow(dead_code)] impl {name} {{\n{fns}}}")));
    }
    out
}

/// NAME_arms!(cb [pre]): cb! { [pre] (name, value, [Type], [column]..), .. },
/// an entry per arm - a list's form, so xmacro_select can select from it.
pub(crate) fn entries(name: &str, arms: &[Arm]) -> TokenStream {
    let entries: String = arms.iter()
        .map(|a| [a.name.clone(), a.value.clone()].into_iter().chain(a.ty.as_deref().map(arm_path))
             .chain(a.cols.iter().map(|c| format!("[{}]", c.join(", "))))
             .collect::<Vec<_>>().join(", "))
        .map(|row| format!("({row}),"))
        .collect();
    tokens(&format!("#[allow(unused_macros)] macro_rules! {name}_arms {{ \
                         ($($cb:ident)::+ $([$($pre:tt)*])?) => {{ \
                             $($cb)::+ ! {{ $([$($pre)*])? {entries} }} \
                         }}; \
                     }} \
                     #[allow(unused_imports)] pub(crate) use {name}_arms;"))
}

impl Column {
    /// The const fn's type: an enum or value column's an Option, if @other -
    /// a tag no arm has.
    fn ty(&self, other: bool) -> String {
        let ty = match self {
            Column::Flags(ty) | Column::Value(ty) => ty.clone(),
            Column::Set(_)    => "u64".into(),
            Column::Enum(e)   => e.name.clone().unwrap_or_default(),
        };
        match self {
            Column::Enum(_) | Column::Value(_) if other => format!("Option<{ty}>"),
            _ => ty,
        }
    }

    /// Arm @arm's: its set of @cell's names, its variant of the column's
    /// enum, or its value - Some, if @other makes it an Option.
    fn value(&self, arm: &str, cell: &[String], other: bool) -> String {
        let v = match self {
            Column::Flags(ty) => cell.iter().fold(format!("{ty}::empty()"), |v, f| format!("{v}.union({ty}::{f})")),
            Column::Set(ty)   => cell.iter().fold("0u64".into(), |v, f| format!("{v} | 1u64 << {ty}::{f}.into_bits()")),
            Column::Enum(e)   => format!("{}::{arm}", e.name.as_deref().unwrap_or_default()),
            Column::Value(ty) => format!("(({}) as {ty})", cell[0]),
        };
        match self {
            Column::Enum(_) | Column::Value(_) if other => format!("Some({v})"),
            _ => v,
        }
    }

    /// A tag no arm has: an empty set, no variant or value.
    fn none(&self) -> String {
        match self {
            Column::Enum(_) | Column::Value(_) => "None".into(),
            _ => self.value("", &[], false),
        }
    }
}

/// An arm's type as a path from anywhere in the crate, for macros that expand
/// at their callers.
pub(crate) fn arm_path(ty: &str) -> String {
    match ty.trim().strip_prefix("c::") {
        Some(rest) => format!("crate::c::{rest}"),
        None => ty.trim().to_string(),
    }
}

/// An integer literal, decimal or 0x hex, as a discriminant is written.
pub(crate) fn parse_int(s: &str) -> Option<u64> {
    let s = s.replace('_', "");
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}
