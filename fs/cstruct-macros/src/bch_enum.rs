// SPDX-License-Identifier: GPL-2.0
//! An enum's declaration, #[bch_enum]'s: each variant
//! with its doc, its columns and its value - and a key type's, its value's
//! type, `deleted(c::Deleted)`:
//!
//!   #[arm_columns(..)]   columns, each a const fn of the enum, a variant's
//!                        written #[COLUMN(..)]:
//!     NAME: FLAGS          a set of #[flags] FLAGS, by its names - a key
//!                          type's flags
//!     NAME: set(ENUM)      a set of ENUM's values, a u64 a bit each - a
//!                          btree's key types
//!     NAME: enum([closed,] K: REPR[, nr])
//!                          an enum K of the same variants, valued by the
//!                          column: its fn is K's variant - a recovery pass's
//!                          stable id
//!     NAME: value(T)       a T, any const expression - an inode option's
//!                          width
//!   #[mask(NAME: REPR)]  the #[flags] NAME of bit numbers, 1 << each value,
//!                        named as their variants
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
use quote::{format_ident, quote, ToTokens};
use syn::punctuated::Punctuated;

use crate::cstruct::{doc_of, int_bits, int_of, parse_variant, split_commas, EnumDef, EnumKind, EnumVariant};
use crate::tokens::comma_list;
use crate::typeinfo;

pub(crate) struct Arm {
    pub docs:  Vec<String>,
    pub name:  Ident,
    /// What it prints as, `#[label("..")]`, where that isn't its name
    pub label: Option<String>,
    /// Its value's type, `name(Type)`
    pub ty:    Option<syn::Type>,
    pub value: syn::Expr,
    pub cols:  Vec<Cell>,
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
        for a in attrs {
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
        arms.push(Arm { docs: v.docs, name, label: v.label, ty, value, cols });
    }
    // the entries are one shape: every arm's (name, value, Type, ..), or every one's (name, value, ..)
    if arms.iter().any(|a| a.ty.is_some()) && arms.iter().any(|a| a.ty.is_none()) {
        return Err("an arm with a type and one without: every arm has its type, or none does".into());
    }
    Ok(arms)
}

/// The #[bch_enum(@kind)] @item: `#[repr(uN)] pub enum NAME { arms }`.
pub(crate) fn declaration(kind: EnumKind, item: syn::ItemEnum) -> Result<TokenStream, String> {
    let name = item.ident.clone();
    let err = |m: String| format!("{name}: {m}");
    let (mut repr, mut docs, mut own_to_text, mut nr) = (None, Vec::new(), false, false);
    let (mut columns, mut mask) = (Vec::new(), None);
    for a in &item.attrs {
        let args = || a.parse_args::<TokenStream>().map_err(|e| err(e.to_string()));
        match a.path().get_ident().map(|i| i.to_string()).as_deref() {
            Some("repr")        => repr = Some(a.parse_args::<Ident>().map_err(|e| err(e.to_string()))?),
            Some("own_to_text") => own_to_text = true,
            Some("nr")          => nr = true,
            Some("arm_columns") => columns = self::columns(args()?).map_err(err)?,
            Some("mask")        => mask = Some(EnumDef { kind: EnumKind::Flags, ..tag_enum(args()?).map_err(err)? }),
            Some("doc")         => docs.extend(doc_of(a)),
            _ => {
                let path = a.path();
                return Err(err(format!("#[{}]: not one of #[bch_enum]'s", quote!(#path))));
            }
        }
    }
    let repr = repr.filter(|r| int_bits(&r.to_string()).is_some())
        .ok_or_else(|| err("#[repr(uN)]: its integer type".into()))?;
    let e = EnumDef { kind, name: name.clone(), docs, repr, variants: Vec::new(), own_to_text, nr };
    if e.kind == EnumKind::Flags && !columns.is_empty() {
        return Err(err("#[arm_columns] are an enum's, not flags' - bits".into()));
    }
    if e.kind == EnumKind::Flags && mask.is_some() {
        return Err(err("#[mask] is of bit numbers, not flags".into()));
    }
    let arms = arms(item.variants.to_token_stream(), &columns, false).map_err(err)?;

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

/// `[closed,] NAME: REPR[, nr]`: a tag enum's, or an enum column's.
pub(crate) fn tag_enum(args: TokenStream) -> Result<EnumDef, String> {
    let (mut kind, mut name, mut nr) = (EnumKind::Open, None, false);
    for part in comma_list(args) {
        match part.as_slice() {
            [TokenTree::Ident(c)] if c == "closed" => kind = EnumKind::Closed,
            [TokenTree::Ident(c)] if c == "open" => {}
            [TokenTree::Ident(c)] if c == "nr" => nr = true,
            [TokenTree::Ident(n), c, TokenTree::Ident(r)] if is_punct(Some(c), ':') =>
                name = Some((n.clone(), r.clone())),
            _ => return Err("expected [closed,] NAME: REPR[, nr]".into()),
        }
    }
    let Some((name, repr)) = name else {
        return Err("an enum needs NAME: REPR".into());
    };
    Ok(EnumDef { kind, name, docs: Vec::new(), repr, variants: Vec::new(),
                  own_to_text: false, nr })
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

/// #[mask(NAME: REPR)]: flags @m of bit numbers @bits, a flag per arm, 1 <<
/// its value, named and printed as the arm.
fn mask_of(m: &EnumDef, bits: &EnumDef, arms: &[Arm]) -> TokenStream {
    let (repr, bits_ty) = (&m.repr, &bits.name);
    let def = EnumDef {
        variants: arms.iter().map(|a| {
            let arm = &a.name;
            let bit = match bits.kind {
                EnumKind::Closed => quote!((#bits_ty::#arm as u32)),
                _ => quote!(#bits_ty::#arm.0),
            };
            EnumVariant {
                name:   arm.clone(),
                value:  Some(syn::parse_quote!((1 as #repr) << #bit)),
                label:  Some(a.label.clone().unwrap_or_else(|| arm.to_string())),
                docs:   a.docs.clone(),
            }
        }).collect(),
        ..m.clone()
    };
    let mut out = enum_gen(&def);
    let (name, past) = (&m.name, format!("{bits_ty}: a value past {}'s bits", m.name));
    out.extend(quote! {
        impl From<#bits_ty> for #name {
            fn from(v: #bits_ty) -> Self {
                Self::from_bits_retain((1 as #repr).checked_shl(v.into_bits() as u32).expect(#past))
            }
        }
    });
    out
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

/// Enum @e: the type, its values, and their names.
fn enum_gen(e: &EnumDef) -> TokenStream {
    let repr = &e.repr;
    let names: Vec<&Ident> = e.variants.iter().map(|v| &v.name).collect();

    // A variant with no value counts from the last that has one - not from
    // the one before it, which would make a const chain as long as the enum:
    // too deep for rustc, at PersistentCounter's hundreds. A closed
    // enum's is Rust's own, the one before it plus one; flags' are their
    // bits module's.
    let mut out = TokenStream::new();
    let mut values = Vec::new();
    // as plain integers, each by its name: the flags' bits module
    let mut raw_values = Vec::new();
    let mut base = None;
    let mut n = 0;
    for (v, EnumVariant { value, .. }) in names.iter().zip(&e.variants) {
        let k = Literal::usize_unsuffixed(n);
        raw_values.push(match (value, base) {
            (Some(x), _) => quote!(#x),
            (None, Some(b)) => quote!(#b + #k),
            (None, None) => quote!(#k),
        });
        values.push(match (value, base, e.kind) {
            (Some(x), ..) => Some(quote!(#x)),
            (None, _, EnumKind::Closed | EnumKind::Flags) => None,
            (None, Some(b), EnumKind::Open) => Some(quote!(Self::#b.0 + #k)),
            (None, None, EnumKind::Open) => Some(quote!(#k)),
        });
        if value.is_some() {
            base = Some(v);
            n = 0;
        }
        n += 1;
    }
    let nr = e.nr.then(|| past_largest(names.iter().map(|v| variant_int(e, v)), quote!(#repr)));

    let name = &e.name;
    let attrs = e.docs.iter().map(|d| quote!(#[doc = #d]));
    let attrs = quote!(#(#attrs)*);
    let bits_mod = format_ident!("{name}_bits");
    let docs: Vec<_> = e.variants.iter().map(|v| { let d = &v.docs; quote!(#(#[doc = #d])*) }).collect();
    let discs = values.iter().map(|v| v.as_ref().map(|x| quote!(= #x)));
    let values = values.iter().map(|v| v.as_ref().expect("an open enum's values are all made"));
    // An open enum's or flags' integer, in and out
    let (from_int, to_int) = match e.kind {
        EnumKind::Flags => (quote!(Self::from_bits_retain), quote!(bits())),
        _ => (quote!(Self), quote!(0)),
    };
    // An LeN field of an open enum is stored as its integer or a narrower
    // one - a byte of a u32 enum - and one of flags as theirs
    let uints = ["u8", "u16", "u32", "u64"];
    let n = uints.iter().position(|u| repr == u).map_or(0, |i| i + 1);
    let widths = if e.kind == EnumKind::Flags { &uints[n.saturating_sub(1)..n] } else { &uints[..n] };
    let too_wide = format!("{name}: a value too wide for its field");
    let le_values = widths.iter().map(|w| {
        let w = format_ident!("{w}");
        quote! {
            impl crate::types::LeValue<#w> for #name {
                fn from_int(v: #w) -> Self { #from_int(v.into()) }
                fn into_int(self) -> #w { #w::try_from(self.#to_int).expect(#too_wide) }
            }
        }
    });
    let int_impls = (e.kind != EnumKind::Closed).then(|| quote! {
        impl #name {
            pub const fn into_bits(self) -> #repr { self.#to_int }
        }
        impl From<#name> for u64 {
            fn from(v: #name) -> u64 { v.#to_int.into() }
        }
        impl TryFrom<u64> for #name {
            type Error = <#repr as TryFrom<u64>>::Error;
            fn try_from(v: u64) -> Result<Self, Self::Error> { #repr::try_from(v).map(#from_int) }
        }
        // SAFETY: its integer, every value of which is one
        unsafe impl crate::util::vstructs::Plain for #name {}
        crate::one_word!(#name);
        #(#le_values)*
    });
    out.extend(match e.kind {
        EnumKind::Closed => {
            // what it prints as: its label
            let labels = e.variants.iter().map(|v| e.label(v).map(|l| quote!(#[label(#l)])));
            let msg = format!("{name}::from_bits(): not a {name}");
            // as bit numbers: a set of them takes a bit past the largest -
            // a negative value, unlocked's -1, isn't one
            let nr = past_largest(names.iter().map(|v| { let i = variant_int(e, v); quote!(#i as i64) }), quote!(i64));
            quote! {
                #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, crate::util::ToText)]
                #[repr(#repr)]
                #[allow(non_camel_case_types)]
                #attrs
                pub enum #name { #(#docs #labels #names #discs,)* }
                impl #name {
                    /// The variant @v is, if it's one.
                    pub const fn try_from_bits(v: #repr) -> Option<Self> {
                        match v {
                            #(x if x == Self::#names as #repr => Some(Self::#names),)*
                            _ => None,
                        }
                    }
                    pub const fn from_bits(v: #repr) -> Self {
                        match Self::try_from_bits(v) { Some(x) => x, None => panic!(#msg) }
                    }
                    pub const fn into_bits(self) -> #repr { self as #repr }
                }
                impl crate::data_structures::bitmap::BitEnum for #name {
                    type Words = crate::data_structures::bitmap::AtomicBitArray<{ crate::types::bits_to_longs(#nr as usize) }>;
                    fn bit(self) -> usize { self as usize }
                    fn from_bit(bit: usize) -> Option<Self> {
                        #repr::try_from(bit).ok().and_then(Self::try_from_bits)
                    }
                }
            }
        }
        EnumKind::Open => quote! {
            #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
            #[repr(transparent)]
            #attrs
            pub struct #name(pub #repr);
            #[allow(non_upper_case_globals)]
            impl #name {
                #(#docs pub const #names: Self = Self(#values);)*
                pub const fn from_bits(v: #repr) -> Self { Self(v) }
            }
            #int_impls
        },
        EnumKind::Flags => quote! {
            ::bitflags::bitflags! {
                #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
                #[repr(transparent)]
                #attrs
                pub struct #name: #repr {
                    #(#docs #[allow(non_upper_case_globals)] const #names = #bits_mod::#names;)*
                }
            }
            #int_impls

            // The flags as plain integers, for a type-level constant: one
            // that calls bitflags' const fns can cycle in rustc's
            // coherence checking (Debian's 1.97.1)
            #[allow(non_snake_case, non_upper_case_globals)]
            pub mod #bits_mod {
                #[allow(unused_imports)]
                use {super::*, crate::c::*};
                #(pub const #names: #repr = #raw_values;)*
            }
        },
    });
    if let Some(nr) = &nr {
        out.extend(quote!(impl #name { pub const NR: #repr = #nr; }));
    }
    // An array of a value per variant is indexed by it
    if e.kind == EnumKind::Open && nr.is_some() {
        out.extend(quote! {
            impl<T> core::ops::Index<#name> for [T; #name::NR as usize] {
                type Output = T;
                fn index(&self, i: #name) -> &T { &self[i.0 as usize] }
            }
            impl<T> core::ops::IndexMut<#name> for [T; #name::NR as usize] {
                fn index_mut(&mut self, i: #name) -> &mut T { &mut self[i.0 as usize] }
            }
        });
    }

    out.extend(names_gen(e, name));
    out.extend(typeinfo::info(name, name, quote!(Enum), &[]));

    // Zeroed is a value of it - so a struct holding one can be Zeroable:
    // any integer is an open enum's or flags', a closed enum's only if a
    // variant is 0 - as written, or the first, counting from 0
    let has_zero = e.kind != EnumKind::Closed || e.variants.iter().enumerate()
        .any(|(i, v)| match &v.value {
            None    => i == 0,
            Some(x) => int_of(x) == Some(0),
        });
    if has_zero {
        out.extend(quote! {
            // SAFETY: zero is a value of it
            unsafe impl ::pin_init::Zeroable for #name {}
        });
    }

    out
}

/// Variant @v of enum @e's integer, as its repr - by the enum's name, so a
/// type-level constant can say it.
fn variant_int(e: &EnumDef, v: &Ident) -> TokenStream {
    let (name, repr) = (&e.name, &e.repr);
    match e.kind {
        EnumKind::Closed => quote!((#name::#v as #repr)),
        EnumKind::Open   => quote!(#name::#v.0),
        EnumKind::Flags  => quote!(#name::#v.bits()),
    }
}

/// One past the largest of @ints, computed in @ty - not the last's: the
/// values needn't be in order, the counters' stable ids aren't. 0 if none
/// is 0 or more.
fn past_largest(ints: impl Iterator<Item = TokenStream>, ty: TokenStream) -> TokenStream {
    let ints: Vec<_> = ints.collect();
    quote!({ let mut n: #ty = 0; #(if (#ints) >= n { n = (#ints) + 1; })* n })
}

/// An enum from an x-macro list - with labelled variants - is named by the
/// list: util/names.rs's Names, its labelled variants' names and values, a
/// flags type's bits. An open or flags one prints through it - ToText, and
/// Display through that - unless it's #[own_to_text]; a closed one derives
/// ToText.
fn names_gen(e: &EnumDef, name: &Ident) -> TokenStream {
    let labelled: Vec<(String, &Ident)> = e.variants.iter()
        .filter_map(|v| e.label(v).map(|l| (l, &v.name)))
        .collect();
    if labelled.is_empty() {
        return TokenStream::new();
    }

    let n = quote!(crate::util::names);
    let entries = labelled.iter().map(|(l, v)| {
        let i = variant_int(e, v);
        match e.kind {
            EnumKind::Flags => quote!((#l, #n::bit(#i as u64))),
            _ => quote!((#l, #i as u64)),
        }
    });
    let mut out = quote! {
        impl #n::Names for #name {
            const ENTRIES: &'static [(&'static str, u64)] = &[#(#entries),*];
        }
    };

    let to_text = match e.kind {
        _ if e.own_to_text => None,
        EnumKind::Closed  => None,
        EnumKind::Open    => Some(quote!(#n::prt_name(out, <Self as #n::Names>::ENTRIES, self.0 as u64))),
        EnumKind::Flags   => Some(quote!(#n::prt_flags(out, <Self as #n::Names>::ENTRIES, self.bits() as u64))),
    };
    // A closed enum's derive gives it name(); an open one's is here
    if e.kind == EnumKind::Open && !e.own_to_text {
        out.extend(quote! {
            impl #name {
                /// Its name: None for a value this version has none for.
                #[inline]
                pub fn name(self) -> Option<&'static str> { #n::name_of(<Self as #n::Names>::ENTRIES, self.0 as u64) }
            }
        });
    }
    if let Some(body) = to_text {
        out.extend(quote! {
            impl crate::util::ToText for #name {
                #[inline]
                fn to_text(&self, out: &mut crate::util::Printbuf) { #body }
            }
            crate::display_via_to_text!(#name);
        });
    }
    out
}

