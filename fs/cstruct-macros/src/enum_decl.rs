// SPDX-License-Identifier: GPL-2.0
//! An enum's declaration, #[bch_enum]'s or list_enum!'s: each variant
//! with its doc, its columns and its value - and a key type's, its value's
//! type, `deleted(c::Deleted)`:
//!
//!   #[arm_columns(..)]   columns, each a const fn of the enum, a variant's
//!                        written #[COLUMN(..)]:
//!     NAME: FLAGS          a set of #[flags] FLAGS, by its names - a key
//!                          type's flags
//!     NAME: set(ENUM)      a set of ENUM's values, a u64 a bit each - a
//!                          btree's key types
//!     NAME: enum([closed,] K: REPR, prefix = P[, nr])
//!                          an enum K of the same variants, valued by the
//!                          column: its fn is K's variant - a recovery pass's
//!                          stable id
//!     NAME: value(T)       a T, any const expression - an inode option's
//!                          width
//!   #[mask(NAME: REPR, prefix = P)]
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

use proc_macro2::{Delimiter, Ident, Literal, TokenStream, TokenTree};
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;

use crate::cstruct::{doc_of, int_of, parse_enum_head, parse_variant, split_commas, EnumDef, EnumKind,
                     EnumVariant};
use crate::{enum_gen, comma_list};

pub(crate) struct Arm {
    pub docs:  Vec<String>,
    pub name:  Ident,
    /// What it prints as, `#[label("..")]`, where that isn't its name
    pub label: Option<String>,
    /// Its value's type, `name(Type)`
    pub ty:    Option<syn::Type>,
    pub value: syn::Expr,
    pub cols:  Vec<Cell>,
    /// The #[mask]'s prefix from this arm on, `#[mask_prefix(P)]`
    pub mask_prefix: Option<String>,
}

/// An #[arm_columns] column: a set of a #[bch_enum(flags)]'s flags, or of
/// another enum's values, a bit each; another enum's variant, of the arm's
/// name; or a value of a type.
pub(crate) enum Column {
    Flags(syn::Type),
    Set(syn::Type),
    Enum(EnumDef),
    Value(syn::Type),
}

pub(crate) type Columns = Vec<(Ident, Column)>;

/// An arm's #[COLUMN(..)]: a set's names, or an enum or value column's value.
#[derive(Clone)]
pub(crate) enum Cell {
    Names(Vec<Ident>),
    Value(syn::Expr),
}

impl Cell {
    fn names(&self) -> &[Ident] {
        match self {
            Cell::Names(n) => n,
            Cell::Value(_) => &[],
        }
    }

    /// An enum or value column's: arms() gives each arm's its value.
    fn value(&self) -> &syn::Expr {
        match self {
            Cell::Value(v) => v,
            Cell::Names(_) => unreachable!("an enum or value column's cell is a value"),
        }
    }
}

fn is_punct(t: Option<&TokenTree>, c: char) -> bool {
    matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == c)
}

/// #[arm_columns(NAME: FLAGS | NAME: set(ENUM) | NAME: enum(..) | NAME: value(T), ..)]
pub(crate) fn columns(args: TokenStream) -> Result<Columns, String> {
    const SYNTAX: &str = "#[arm_columns(NAME: FLAGS | NAME: set(ENUM) | NAME: enum(..) | NAME: value(T), ..)]";
    let ty = |ts: TokenStream| syn::parse2::<syn::Type>(ts).map_err(|e| format!("{SYNTAX}: {e}"));
    comma_list(args).into_iter().map(|c| Ok(match c.as_slice() {
        [TokenTree::Ident(n), colon, TokenTree::Ident(kind), TokenTree::Group(g)]
            if is_punct(Some(colon), ':') && g.delimiter() == Delimiter::Parenthesis && kind == "set" =>
            (n.clone(), Column::Set(ty(g.stream())?)),
        [TokenTree::Ident(n), colon, TokenTree::Ident(kind), TokenTree::Group(g)]
            if is_punct(Some(colon), ':') && g.delimiter() == Delimiter::Parenthesis && kind == "enum" =>
            (n.clone(), Column::Enum(tag_enum(g.stream())?)),
        [TokenTree::Ident(n), colon, TokenTree::Ident(kind), TokenTree::Group(g)]
            if is_punct(Some(colon), ':') && g.delimiter() == Delimiter::Parenthesis && kind == "value" =>
            (n.clone(), Column::Value(ty(g.stream())?)),
        [TokenTree::Ident(n), colon, rest @ ..] if is_punct(Some(colon), ':') && !rest.is_empty() =>
            (n.clone(), Column::Flags(ty(rest.iter().cloned().collect())?)),
        _ => return Err(SYNTAX.to_string()),
    })).collect()
}

/// The arms of @body: `[#[COLUMN(..)]..] name[(Type)] [= value]`, a value not
/// written the last's plus one. With @typed, each has its Type.
pub(crate) fn arms(body: TokenStream, columns: &Columns, typed: bool) -> Result<Vec<Arm>, String> {
    let mut arms = Vec::new();
    // the last value written, and how many arms after it this one is
    let mut base: (Option<syn::Expr>, u64) = (None, 0);
    for v in split_commas(body) {
        let (v, attrs, ty) = parse_variant(v).map_err(|e| e.to_string())?;
        let name = v.name;
        if typed && ty.is_none() {
            return Err(format!("{name}: an arm is `name(Type) [= value]`"));
        }
        let mut cols: Vec<Option<Cell>> = vec![None; columns.len()];
        let mut mask_prefix = None;
        for a in attrs {
            if a.path().is_ident("mask_prefix") {
                let p: Ident = a.parse_args().map_err(|e| format!("{name}: #[mask_prefix(P)]: {e}"))?;
                mask_prefix = Some(p.to_string());
                continue;
            }
            let path = a.path();
            let i = columns.iter().position(|(c, _)| path.is_ident(c))
                .ok_or_else(|| format!("{name}: #[{}]: not one of its #[arm_columns]", quote!(#path)))?;
            let err = |e: syn::Error| format!("{name}: #[{}(..)]: {e}", columns[i].0);
            cols[i] = Some(match columns[i].1 {
                Column::Enum(_) | Column::Value(_) => Cell::Value(a.parse_args().map_err(err)?),
                _ => Cell::Names(a.parse_args_with(Punctuated::<Ident, syn::Token![,]>::parse_terminated)
                    .map_err(err)?.into_iter().collect()),
            });
        }
        let value = match v.value {
            Some(val) => {
                base = (Some(val.clone()), 0);
                val
            }
            None => {
                let k = Literal::u64_unsuffixed(base.1);
                match &base.0 {
                    None => syn::parse_quote!(#k),
                    Some(b) => match int_of(b) {
                        Some(n) => {
                            let n = Literal::u64_unsuffixed(n + base.1);
                            syn::parse_quote!(#n)
                        }
                        None => syn::parse_quote!((#b) + #k),
                    },
                }
            }
        };
        base.1 += 1;
        let cols = columns.iter().zip(cols).map(|((c, col), cell)| match (col, cell) {
            (_, Some(cell)) => Ok(cell),
            (Column::Enum(_) | Column::Value(_), None) => Err(format!("{name}: no #[{c}(..)]")),
            (Column::Flags(_) | Column::Set(_), None) => Ok(Cell::Names(Vec::new())),
        }).collect::<Result<_, _>>()?;
        arms.push(Arm { docs: v.docs, name, label: v.label, ty, value, cols, mask_prefix });
    }
    // the entries are one shape: every arm's (name, value, Type, ..), or every one's (name, value, ..)
    if arms.iter().any(|a| a.ty.is_some()) && arms.iter().any(|a| a.ty.is_none()) {
        return Err("an arm with a type and one without: every arm has its type, or none does".into());
    }
    Ok(arms)
}

/// An enum of a declaration: `[#[..]] pub enum NAME: REPR { arms }`.
pub(crate) fn declaration(input: TokenStream) -> Result<TokenStream, String> {
    let (mut e, attrs, body) = syn::parse::Parser::parse2(parse_enum_head, input).map_err(|e| e.to_string())?;
    let name = e.name.clone();
    let err = |m: String| format!("{name}: {m}");
    let (mut columns, mut mask) = (Vec::new(), None);
    for a in attrs {
        let args = || a.parse_args::<TokenStream>().map_err(|e| err(e.to_string()));
        match a.path().get_ident().map(|i| i.to_string()).as_deref() {
            Some("arm_columns") => columns = self::columns(args()?).map_err(err)?,
            Some("mask")        => mask = Some(EnumDef { kind: EnumKind::Flags, ..tag_enum(args()?).map_err(err)? }),
            Some("doc")         => e.docs.extend(doc_of(&a)),
            _ => {
                let path = a.path();
                return Err(err(format!("#[{}]: not one of list_enum!'s", quote!(#path))));
            }
        }
    }
    if e.kind == EnumKind::Flags && !columns.is_empty() {
        return Err(err("#[arm_columns] are an enum's, not flags' - bits".into()));
    }
    if e.kind == EnumKind::Flags && mask.is_some() {
        return Err(err("#[mask] is of bit numbers, not flags".into()));
    }
    let arms = arms(body, &columns, false).map_err(err)?;
    if mask.is_none() && arms.iter().any(|a| a.mask_prefix.is_some()) {
        return Err(err("#[mask_prefix] without a #[mask]".into()));
    }

    let mut out = generate(&e, &arms, &columns);
    out.extend(entries(&name, &arms));
    for (i, (_, col)) in columns.iter().enumerate() {
        if let Column::Enum(k) = col {
            out.extend(enum_of(k, arms.iter().map(|a| (a, a.cols[i].value()))));
        }
    }
    if let Some(m) = &mask {
        out.extend(mask_of(m, &e, &arms));
    }
    Ok(out)
}

/// `[closed,] NAME: REPR, prefix = P[, nr]`: a tag enum's,
/// or an enum column's.
pub(crate) fn tag_enum(args: TokenStream) -> Result<EnumDef, String> {
    let (mut kind, mut name, mut prefix, mut nr) = (EnumKind::Open, None, None, false);
    for part in comma_list(args) {
        match part.as_slice() {
            [TokenTree::Ident(c)] if c == "closed" => kind = EnumKind::Closed,
            [TokenTree::Ident(c)] if c == "open" => {}
            [TokenTree::Ident(c)] if c == "nr" => nr = true,
            [TokenTree::Ident(n), c, TokenTree::Ident(r)] if is_punct(Some(c), ':') =>
                name = Some((n.clone(), r.clone())),
            [TokenTree::Ident(k), eq, TokenTree::Ident(v)] if is_punct(Some(eq), '=') => match k.to_string().as_str() {
                "prefix" => prefix = Some(v.to_string()),
                _ => return Err(format!("{k} = ..: not prefix")),
            },
            _ => return Err("expected [closed,] NAME: REPR, prefix = PREFIX[, nr]".into()),
        }
    }
    let (Some((name, repr)), Some(prefix)) = (name, prefix) else {
        return Err("an enum needs NAME: REPR and prefix = PREFIX".into());
    };
    Ok(EnumDef { kind, name, docs: Vec::new(), repr, variants: Vec::new(), from: Vec::new(),
                  own_to_text: false, prefix: Some(prefix), nr })
}

/// Enum @e of @arms, each valued by its expression.
fn enum_of<'a>(e: &EnumDef, arms: impl Iterator<Item = (&'a Arm, &'a syn::Expr)>) -> TokenStream {
    let repr = &e.repr;
    let def = EnumDef {
        variants: arms.map(|(a, v)| EnumVariant {
            name:   a.name.clone(),
            // an expression's type may not be the enum's, BCH_VERSION()'s
            // u64: as a list's value is, it's cast
            value:  Some(match int_of(v) {
                Some(_) => v.clone(),
                None => syn::parse_quote!((#v) as #repr),
            }),
            label:  a.label.clone(),
            docs:   a.docs.clone(),
        }).collect(),
        ..e.clone()
    };
    enum_gen(&def)
}

/// #[mask(NAME: REPR, prefix = P)]: flags @m of bit numbers
/// @bits, a flag per arm, 1 << its value - named P and the arm's name, or the
/// #[mask_prefix] last written on an arm up to it and the name. A flag prints
/// as its arm's name, as a list's does.
fn mask_of(m: &EnumDef, bits: &EnumDef, arms: &[Arm]) -> TokenStream {
    let (repr, bits_ty) = (&m.repr, &bits.name);
    let mut prefix = m.prefix.clone();
    let def = EnumDef {
        variants: arms.iter().map(|a| {
            prefix = a.mask_prefix.clone().or(prefix.take());
            let p = prefix.as_deref().unwrap_or_default();
            // an arm written with its prefix, one not an identifier past it,
            // is a flag written with the mask's
            let (arm, written) = (&a.name, a.name.to_string());
            let past = bits.prefix.as_deref().and_then(|b| written.strip_prefix(b));
            let bit = match bits.kind {
                EnumKind::Closed => quote!((#bits_ty::#arm as u32)),
                _ => quote!(#bits_ty::#arm.0),
            };
            EnumVariant {
                name:   past.map_or_else(|| arm.clone(), |n| format_ident!("{p}{n}")),
                value:  Some(syn::parse_quote!((1 as #repr) << #bit)),
                label:  Some(a.label.clone().unwrap_or_else(|| past.unwrap_or(&written).to_string())),
                docs:   a.docs.clone(),
            }
        }).collect(),
        ..m.clone()
    };
    enum_gen(&def)
}

/// Enum @e of @arms, and a const fn of it per column.
pub(crate) fn generate(e: &EnumDef, arms: &[Arm], columns: &Columns) -> TokenStream {
    let mut out = enum_of(e, arms.iter().map(|a| (a, &a.value)));

    let name = &e.name;
    // a tag no arm has: none of a set; no value, so a value's is an Option
    let other = e.kind != EnumKind::Closed;
    let fns: Vec<TokenStream> = columns.iter().enumerate()
        .map(|(i, (col_name, col))| {
            let arm_names = arms.iter().map(|a| &a.name);
            let values = arms.iter().map(|a| col.value(&a.name, &a.cols[i], other));
            let none = other.then(|| col.none()).map(|n| quote!(_ => #n,));
            let ty = col.ty(other);
            quote! {
                pub const fn #col_name(self) -> #ty {
                    match self {
                        #(Self::#arm_names => #values,)*
                        #none
                    }
                }
            }
        })
        .collect();
    if !fns.is_empty() {
        out.extend(quote!(impl #name { #(#fns)* }));
    }
    out
}

/// NAME_arms!(cb [pre]): cb! { [pre] (name, value, [Type], [column]..), .. },
/// an entry per arm - a list's form, so xmacro_select can select from it.
pub(crate) fn entries(name: &Ident, arms: &[Arm]) -> TokenStream {
    let macro_name = format_ident!("{name}_arms");
    let rows = arms.iter().map(|a| {
        let (name, value) = (&a.name, &a.value);
        let ty = a.ty.as_ref().map(arm_path).map(|t| quote!(, #t));
        let cols = a.cols.iter().map(|c| match c {
            Cell::Names(n) => quote!([#(#n),*]),
            Cell::Value(v) => quote!([#v]),
        });
        quote!((#name, #value #ty #(, #cols)*),)
    });
    quote! {
        #[allow(unused_macros)]
        macro_rules! #macro_name {
            ($($cb:ident)::+ $([$($pre:tt)*])?) => {
                $($cb)::+ ! { $([$($pre)*])? #(#rows)* }
            };
        }
        #[allow(unused_imports)]
        pub(crate) use #macro_name;
    }
}

impl Column {
    /// The const fn's type: an enum or value column's an Option, if @other -
    /// a tag no arm has.
    fn ty(&self, other: bool) -> TokenStream {
        let ty = match self {
            Column::Flags(ty) | Column::Value(ty) => quote!(#ty),
            Column::Set(_)    => quote!(u64),
            Column::Enum(e)   => { let n = &e.name; quote!(#n) }
        };
        match self {
            Column::Enum(_) | Column::Value(_) if other => quote!(Option<#ty>),
            _ => ty,
        }
    }

    /// Arm @arm's: its set of @cell's names, its variant of the column's
    /// enum, or its value - Some, if @other makes it an Option.
    fn value(&self, arm: &Ident, cell: &Cell, other: bool) -> TokenStream {
        let v = match self {
            Column::Flags(ty) => cell.names().iter().fold(self.none(), |v, f| quote!(#v.union(#ty::#f))),
            Column::Set(ty)   => cell.names().iter().fold(self.none(), |v, f| quote!(#v | 1u64 << #ty::#f.into_bits())),
            Column::Enum(e)   => { let n = &e.name; quote!(#n::#arm) }
            Column::Value(ty) => { let x = cell.value(); quote!(((#x) as #ty)) }
        };
        match self {
            Column::Enum(_) | Column::Value(_) if other => quote!(Some(#v)),
            _ => v,
        }
    }

    /// A tag no arm has: an empty set, no variant or value.
    fn none(&self) -> TokenStream {
        match self {
            Column::Flags(ty) => quote!(#ty::empty()),
            Column::Set(_)    => quote!(0u64),
            Column::Enum(_) | Column::Value(_) => quote!(None),
        }
    }
}

/// An arm's type as a path from anywhere in the crate, for macros that expand
/// at their callers.
pub(crate) fn arm_path(ty: &syn::Type) -> TokenStream {
    match ty {
        syn::Type::Path(p) if p.qself.is_none() && p.path.leading_colon.is_none() &&
                              p.path.segments.first().is_some_and(|s| s.ident == "c") => quote!(crate::#ty),
        _ => quote!(#ty),
    }
}
