// SPDX-License-Identifier: GPL-2.0
//
// Parsing the items the macros (fs/cstruct-macros) take, which include!()
// this file - so it's plain `//` comments - and build on syn: the kernel
// build's, os/linux_kernel/vendor, with a bare rustc.
//
// The items:
//
//   The derives' struct / union, named fields.
//
//   #[type_name(NAME)] on the struct: its name to TypeInfo, where the item's
//   differs - a family's definition, X__head, whose name the family's type
//   X<S> has (util/vstructs.rs).
//
//   xmacro! { NAME { (args), (args), ... } }: an x-macro list - a
//   macro, NAME!(cb), that hands the whole list to cb!.
//
//   ioctl! { NAME = _IOW(type, nr, ARG), ... }: ioctl numbers - a marker
//   type binding the opcode to ARG, the opcode computed from the target's own
//   _IOC constants (fs/util/ioctl.rs).

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StructKind {
    Struct,
    Union,
}

/// A derive's struct or union: its name and fields, in order.
pub(crate) struct StructDef {
    /// The item's name.
    pub(crate) ident:     syn::Ident,
    /// #[type_name(NAME)]: the type's name, where the item's differs - a
    /// family's definition, X__head, whose name the family's type X<S> has
    /// (util/vstructs.rs).
    pub(crate) type_name: Option<syn::Ident>,
    pub(crate) attrs:     Vec<syn::Attribute>,
    pub(crate) generics:  syn::Generics,
    pub(crate) kind:      StructKind,
    pub(crate) fields:    Vec<FieldDef>,
}

impl StructDef {
    /// The type's name: #[type_name]'s, or the item's.
    pub(crate) fn name(&self) -> &syn::Ident {
        self.type_name.as_ref().unwrap_or(&self.ident)
    }
}

pub(crate) struct FieldDef {
    pub(crate) ident:    syn::Ident,
    pub(crate) ty:       syn::Type,
    pub(crate) vis:      syn::Visibility,
    /// #[members]: its type is a #[bitfield] type, whose members are the
    /// struct's too.
    pub(crate) bitfield: bool,
    /// #[counted_by(FIELD)], on a trailing `[T; 0]`: the field its length
    /// is - C's __counted_by(), as the struct's FlexArray impl.
    pub(crate) counted_by: Option<syn::Ident>,
    /// #[placed_at]: the field #[placed] added, its place's At - which
    /// #[derive(Init)]'s init() takes. (cstruct-macros' place.rs.)
    pub(crate) placed_at: bool,
}

/// An x-macro list.
#[derive(Debug)]
pub(crate) struct XMacro {
    pub(crate) name:     syn::Ident,
    pub(crate) entries:  Vec<XEntry>,
}

/// An entry of an x-macro list.
#[derive(Debug)]
pub(crate) enum XEntry {
    /// (args)
    Args(proc_macro2::Group),
    /// SUB() - another list's entries, here - or SUB((params) => (template)):
    /// SUB's entries, each mapped to the template.
    Sub { list: syn::Ident, map: Option<XMap> },
}

#[derive(Debug)]
pub(crate) struct XMap {
    pub(crate) params:   Vec<syn::Ident>,
    /// An entry's arguments, mapped: params, and concat(A, b) pastes.
    pub(crate) template: proc_macro2::TokenStream,
}

/// ioctl!: ioctl numbers, C's #define NAME _IOW(type, nr, arg).
#[derive(Debug)]
pub(crate) struct IoctlList {
    pub(crate) entries: Vec<IoctlEntry>,
}

/// One ioctl: @dir is its _IO*() macro - _IO, _IOR, _IOW or _IOWR - and
/// @ty and @nr its type and number. @arg is the argument's Rust type, None
/// for _IO.
#[derive(Debug)]
pub(crate) struct IoctlEntry {
    pub(crate) name:  syn::Ident,
    pub(crate) dir:   syn::Ident,
    pub(crate) ty:    syn::Expr,
    pub(crate) nr:    syn::Expr,
    pub(crate) arg:   Option<syn::Type>,
}

/// What an enum's values can be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum EnumKind {
    /// Only ever those listed: a Rust enum.
    Closed,
    /// Anything, from outside: a newtype with the values as constants.
    Open,
    /// Masks, or'd: a bitflags type.
    Flags,
}

impl EnumKind {
    /// The attribute that says it: #[closed], #[open], #[flags].
    pub(crate) fn attr(self) -> &'static str {
        match self {
            EnumKind::Closed => "closed",
            EnumKind::Open   => "open",
            EnumKind::Flags  => "flags",
        }
    }
}

/// Items from an x-macro list: `LIST(params) => template`. Each entry's
/// arguments are bound to the params - `..` the rest - and `concat(A, b)` in the
/// template is pasted into one identifier.
#[derive(Debug, Clone)]
pub(crate) struct FromList {
    pub(crate) list:     syn::Ident,
    pub(crate) params:   Vec<syn::Ident>,
    pub(crate) template: proc_macro2::TokenStream,
}

/// An enum, and what its values can be.
#[derive(Debug, Clone)]
pub(crate) struct EnumDef {
    pub(crate) kind:     EnumKind,
    pub(crate) name:     syn::Ident,
    /// Its doc comment.
    pub(crate) docs:     Vec<String>,
    /// The integer type: u32, i32, u64.
    pub(crate) repr:     syn::Ident,
    pub(crate) variants: Vec<EnumVariant>,
    /// `LIST(params) => VARIANT [= value]`: variants from lists, each in
    /// place before the written one at its index - expand_enum_list() makes
    /// them, a list at a time.
    pub(crate) from:     Vec<(usize, FromList)>,
    /// #[own_to_text]: it prints as more than its name - an open or flags
    /// enum #[bch_enum] otherwise gives ToText.
    pub(crate) own_to_text: bool,
    /// #[prefix(P)]: what C's names for its variants start with, Rust's
    /// without it.
    pub(crate) prefix:   Option<String>,
    /// #[nr]: NR, past the largest value - a number, not one of its values.
    pub(crate) nr:       bool,
}

impl EnumDef {
    /// What @v prints as: its #[label], or its name - past its #[prefix], if
    /// it has one. Not a flag's, whose names would be its bits'.
    pub(crate) fn label(&self, v: &EnumVariant) -> Option<String> {
        v.label.clone().or_else(|| {
            (self.kind != EnumKind::Flags).then_some(())?;
            let (p, name) = (self.prefix.as_deref().unwrap_or_default(), v.name.to_string());
            Some(name.strip_prefix(p).unwrap_or(&name).to_string())
        })
    }
}

/// An enum's variant. Its value, or none for the one after the last; the
/// generator replaces them with what they evaluate to. Its label is what it
/// prints as, `#[label("..")]`: a list's variant's is the argument its
/// template pastes into the name - `stripe`, of `concat(BCH_WATERMARK_, name)` - as
/// C's string tables have it; none, its name.
#[derive(Debug, Clone)]
pub(crate) struct EnumVariant {
    pub(crate) name:   syn::Ident,
    pub(crate) value:  Option<syn::Expr>,
    pub(crate) label:  Option<String>,
    pub(crate) docs:   Vec<String>,
}

// ---- structs ---------------------------------------------------------------

/// The struct or union @d; None if it's an enum.
fn parse_struct(d: syn::DeriveInput) -> Result<Option<StructDef>, String> {
    let name = &d.ident;
    let (kind, fields) = match d.data {
        syn::Data::Struct(s) => (StructKind::Struct, match s.fields {
            syn::Fields::Named(f) => Some(f.named),
            _ => None,
        }),
        syn::Data::Union(u) => (StructKind::Union, Some(u.fields.named)),
        syn::Data::Enum(_) => return Ok(None),
    };
    let fields_of = fields
        .filter(|_| d.generics.const_params().next().is_none())
        .ok_or_else(|| format!("{name}: named fields, and no const generics - an unsized \
                                vstruct's states are each its own definition: #[unsized_vstruct]"))?;

    let mut fields = Vec::new();
    for f in fields_of {
        let mut counted_by = None;
        let bare = |n: &str| f.attrs.iter().any(|a| matches!(&a.meta, syn::Meta::Path(p) if p.is_ident(n)));
        for attr in &f.attrs {
            if attr.path().is_ident("counted_by") {
                counted_by = Some(attr.parse_args::<syn::Ident>()
                    .map_err(|_| format!("{name}: #[counted_by(FIELD)] takes the field that counts it"))?);
            }
        }
        let (bitfield, placed_at) = (bare("members"), bare("placed_at"));
        let ident = f.ident.expect("named fields");
        fields.push(FieldDef { ident, ty: f.ty, vis: f.vis, bitfield, counted_by, placed_at });
    }

    // #[type_name], or what it expands to - a family's macro sees it
    // unexpanded, the derives expanded
    let type_name = d.attrs.iter()
        .filter(|a| a.path().is_ident("type_name") || a.path().is_ident("type_name_of"))
        .find_map(|a| a.parse_args::<syn::Ident>().ok());
    Ok(Some(StructDef { ident: d.ident, type_name, attrs: d.attrs, generics: d.generics, kind, fields }))
}

/// The width of integer type @ty, in bits.
pub(crate) fn int_bits(ty: &str) -> Option<u32> {
    Some(match ty.trim() {
        "u8" | "i8" => 8,
        "u16" | "i16" => 16,
        "u32" | "i32" => 32,
        "u64" | "i64" => 64,
        _ => return None,
    })
}

// ---- xmacro!, ioctl! ---------------------------------------------------

/// xmacro!'s body: NAME { (args), ... }
pub(crate) fn parse_xmacro(input: proc_macro2::TokenStream) -> Result<XMacro, String> {
    use proc_macro2::{Delimiter, TokenTree};
    syn::parse::Parser::parse2(|input: syn::parse::ParseStream<'_>| {
        input.call(syn::Attribute::parse_outer)?;
        let name: syn::Ident = input.parse()?;
        let list;
        syn::braced!(list in input);

        let mut entries = Vec::new();
        while !list.is_empty() {
            entries.push(match list.parse::<TokenTree>()? {
                TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => XEntry::Args(g),
                // SUB(), or SUB((params) => (template))
                TokenTree::Ident(sub) => {
                    let inner;
                    syn::parenthesized!(inner in list);
                    let map = match inner.is_empty() {
                        true => None,
                        false => {
                            let params;
                            syn::parenthesized!(params in inner);
                            let params = syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>
                                ::parse_terminated(&params)?.into_iter().collect();
                            inner.parse::<syn::Token![=>]>()?;
                            let template;
                            syn::parenthesized!(template in inner);
                            Some(XMap { params, template: template.parse()? })
                        }
                    };
                    XEntry::Sub { list: sub, map }
                }
                t => return Err(syn::Error::new(t.span(), "expected (args) or SUB(..)")),
            });
            if !list.is_empty() {
                list.parse::<syn::Token![,]>()?;
            }
        }
        Ok(XMacro { name, entries })
    }, input).map_err(|e| format!("an entry is (args), SUB(), or SUB((params) => (args)) - {e}"))
}

/// @ts split at its commas - its groups' are their own.
pub(crate) fn split_commas(ts: proc_macro2::TokenStream) -> Vec<proc_macro2::TokenStream> {
    let mut out = vec![proc_macro2::TokenStream::new()];
    for t in ts {
        match &t {
            proc_macro2::TokenTree::Punct(p) if p.as_char() == ',' => out.push(proc_macro2::TokenStream::new()),
            _ => out.last_mut().unwrap().extend([t]),
        }
    }
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// ioctl!'s body: NAME = _IOW(type, nr, ARG), ... - _IO without the ARG.
pub(crate) fn parse_ioctl(input: proc_macro2::TokenStream) -> Result<IoctlList, String> {
    let entries = syn::parse::Parser::parse2(|input: syn::parse::ParseStream<'_>| {
        let mut entries = Vec::new();
        while !input.is_empty() {
            input.call(syn::Attribute::parse_outer)?;
            let name: syn::Ident = input.parse()?;
            input.parse::<syn::Token![=]>()?;
            let dir: syn::Ident = input.parse()?;
            let args;
            syn::parenthesized!(args in input);
            let err = |e: String| syn::Error::new(name.span(), format!("{name}: {e}"));
            let args = split_commas(args.parse()?);
            let expr = |t: &proc_macro2::TokenStream| syn::parse2::<syn::Expr>(t.clone());
            let (ty, nr, arg) = match (dir.to_string().as_str(), args.as_slice()) {
                ("_IO", [ty, nr]) => (expr(ty)?, expr(nr)?, None),
                ("_IOR" | "_IOW" | "_IOWR", [ty, nr, arg]) => (expr(ty)?, expr(nr)?, Some(syn::parse2(arg.clone())?)),
                ("_IO", _) => return Err(err("expected _IO(type, nr)".into())),
                ("_IOR" | "_IOW" | "_IOWR", _) => return Err(err(format!("expected {dir}(type, nr, ARG)"))),
                _ => return Err(err(format!("{dir} isn't _IO, _IOR, _IOW or _IOWR"))),
            };
            entries.push(IoctlEntry { name, dir, ty, nr, arg });
            input.parse::<Option<syn::Token![,]>>()?;
        }
        Ok(entries)
    }, input).map_err(|e| format!("an entry is NAME = _IOW(type, nr, ARG) - {e}"))?;
    if entries.is_empty() {
        return Err("expected NAME = _IOW(type, nr, ARG), ...".into());
    }
    Ok(IoctlList { entries })
}

/// An enum's head, `#[open|closed|flags] pub enum NAME: REPR`, with no
/// variants yet; the attributes that aren't
/// its, and its body.
pub(crate) fn parse_enum_head(input: syn::parse::ParseStream<'_>)
        -> syn::Result<(EnumDef, Vec<syn::Attribute>, proc_macro2::TokenStream)> {
    let mut kind = None;
    let (mut own_to_text, mut prefix, mut nr) = (false, None, false);
    let mut others = Vec::new();
    for a in input.call(syn::Attribute::parse_outer)? {
        let id = |a: &syn::Attribute| a.parse_args::<syn::Ident>().map(|n| ident(&n));
        match a.path().get_ident().map(|i| i.to_string()).as_deref() {
            Some("open")        => kind = Some(EnumKind::Open),
            Some("closed")      => kind = Some(EnumKind::Closed),
            Some("flags")       => kind = Some(EnumKind::Flags),
            Some("own_to_text") => own_to_text = true,
            Some("prefix")      => prefix = Some(id(&a)?),
            Some("nr")          => nr = true,
            _ => others.push(a),
        }
    }
    input.parse::<syn::Visibility>()?;
    input.parse::<syn::Token![enum]>()?;
    let name: syn::Ident = input.parse()?;
    let kind = kind.ok_or_else(|| input.error(format!("{name}: #[open], #[closed] or #[flags]")))?;
    input.parse::<syn::Token![:]>()?;
    let repr: syn::Ident = input.parse()?;
    if int_bits(&repr.to_string()).is_none() {
        return Err(input.error(format!("{name}: REPR is an integer type")));
    }
    let body;
    syn::braced!(body in input);
    let def = EnumDef { kind, name, docs: Vec::new(), repr, variants: Vec::new(), from: Vec::new(),
                         own_to_text, prefix, nr };
    Ok((def, others, body.parse()?))
}

/// @a's text, if it's a doc comment's line.
pub(crate) fn doc_of(a: &syn::Attribute) -> Option<String> {
    match &a.meta {
        syn::Meta::NameValue(syn::MetaNameValue {
            path, value: syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }), ..
        }) if path.is_ident("doc") => Some(s.value()),
        _ => None,
    }
}

/// An enum's variant, `[#[..]] NAME[(Type)] [= value]`: its doc and #[label]
/// read into its EnumVariant; its other attributes, and its type.
pub(crate) fn parse_variant(v: proc_macro2::TokenStream)
        -> syn::Result<(EnumVariant, Vec<syn::Attribute>, Option<syn::Type>)> {
    let v: syn::Variant = syn::parse2(v)?;
    let (mut label, mut docs, mut others) = (None, Vec::new(), Vec::new());
    for a in v.attrs {
        match doc_of(&a) {
            Some(d) => docs.push(d),
            _ if a.path().is_ident("label") => label = Some(a.parse_args::<syn::LitStr>()?.value()),
            _ => others.push(a),
        }
    }
    let ty = match v.fields {
        syn::Fields::Unit => None,
        syn::Fields::Unnamed(f) if f.unnamed.len() == 1 => f.unnamed.into_iter().next().map(|f| f.ty),
        f => return Err(syn::Error::new_spanned(f, format!("{}: a variant's payload is a type, (Type)", v.ident))),
    };
    let value = v.discriminant.map(|(_, x)| x);
    Ok((EnumVariant { name: v.ident, value, label, docs }, others, ty))
}

/// list_enum!'s body: its head, then { A = expr, B, ... },
/// or `LIST(params) => template` for each of a list's entries.
pub(crate) fn parse_list_enum(input: proc_macro2::TokenStream) -> Result<EnumDef, String> {
    syn::parse::Parser::parse2(|input: syn::parse::ParseStream<'_>| {
        let (mut e, _, body) = parse_enum_head(input)?;
        for v in split_commas(body) {
            match parse_from_list(&v)? {
                Some(f) => e.from.push((e.variants.len(), f)),
                None => e.variants.push(parse_variant(v)?.0),
            }
        }
        Ok(e)
    }, input).map_err(|e| format!("expected #[open|closed|flags] pub enum NAME: REPR {{ ... }} - {e}"))
}

/// @i as the tokenizer has an identifier: a raw one without its r#.
fn ident(i: &syn::Ident) -> String {
    i.to_string().trim_start_matches("r#").to_string()
}

/// `LIST(params) => template` at @item - a list_enum! variant - if it is one:
/// params, each bound to an entry's argument in turn, ending at `..`.
fn parse_from_list(item: &proc_macro2::TokenStream) -> syn::Result<Option<FromList>> {
    use proc_macro2::{Delimiter, TokenTree};
    let t: Vec<TokenTree> = item.clone().into_iter().collect();
    let [TokenTree::Ident(list), TokenTree::Group(group), TokenTree::Punct(eq), TokenTree::Punct(gt), template @ ..]
        = t.as_slice() else { return Ok(None) };
    if group.delimiter() != Delimiter::Parenthesis || eq.as_char() != '=' || gt.as_char() != '>' || template.is_empty() {
        return Ok(None);
    }
    let mut params = Vec::new();
    for p in split_commas(group.stream()) {
        match p.into_iter().collect::<Vec<_>>().as_slice() {
            [TokenTree::Ident(i)] => params.push(i.clone()),
            [TokenTree::Punct(a), TokenTree::Punct(b)] if a.as_char() == '.' && b.as_char() == '.' => break,
            _ => return Err(syn::Error::new(group.span(), format!("{list}(params): each a name, or .. for the rest"))),
        }
    }
    Ok(Some(FromList { list: list.clone(), params, template: template.iter().cloned().collect() }))
}

/// What @f's template makes of one entry of its list, @entry: its parameters
/// substituted, then its camel(n)s and concat(A, b, ..)s pasted.
fn from_list_entry(f: &FromList, entry: &proc_macro2::TokenStream) -> Result<Vec<proc_macro2::TokenTree>, String> {
    use proc_macro2::{Delimiter, Group, Ident, Span, TokenStream, TokenTree};
    let list = &f.list;
    let args = split_commas(entry.clone());
    let mut bound: Vec<(&Ident, &TokenStream)> = Vec::new();
    for (n, p) in f.params.iter().enumerate() {
        let arg = args.get(n).ok_or_else(|| format!("{list}: entry ({entry}) has no {p}"))?;
        bound.push((p, arg));
    }

    fn subst(ts: TokenStream, bound: &[(&Ident, &TokenStream)]) -> TokenStream {
        ts.into_iter().flat_map(|t| match t {
            TokenTree::Ident(i) => match bound.iter().find(|(p, _)| i == **p) {
                Some((_, arg)) => (*arg).clone(),
                None => TokenStream::from(TokenTree::Ident(i)),
            },
            TokenTree::Group(g) => TokenStream::from(TokenTree::Group(Group::new(g.delimiter(), subst(g.stream(), bound)))),
            t => TokenStream::from(t),
        }).collect()
    }

    let bad = || format!("{list}: concat() in the template pastes identifiers and literals only");
    let name = |s: String| match s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') &&
                                 s.chars().all(|c| c.is_alphanumeric() || c == '_') {
        true => Ok(TokenTree::Ident(Ident::new(&s, Span::call_site()))),
        false => Err(format!("{list}: pastes {s}, not an identifier")),
    };
    fn paste(ts: TokenStream, name: &dyn Fn(String) -> Result<TokenTree, String>, bad: &dyn Fn() -> String)
        -> Result<Vec<TokenTree>, String>
    {
        let t: Vec<TokenTree> = ts.into_iter().collect();
        let mut out = Vec::new();
        let mut k = 0;
        while k < t.len() {
            match (&t[k], t.get(k + 1)) {
                (TokenTree::Ident(c), Some(TokenTree::Group(g)))
                    if g.delimiter() == Delimiter::Parenthesis && (c == "camel" || c == "concat") =>
                {
                    let parts = split_commas(g.stream()).into_iter().map(|p| {
                        match p.into_iter().collect::<Vec<_>>().as_slice() {
                            [TokenTree::Ident(i)] => Ok(i.to_string()),
                            [TokenTree::Literal(l)] => Ok(l.to_string()),
                            _ => Err(bad()),
                        }
                    }).collect::<Result<Vec<_>, _>>()?;
                    out.push(match (c == "camel", parts.as_slice()) {
                        (true, [n]) => name(n.split('_').map(|w| w[..1].to_uppercase() + &w[1..]).collect())?,
                        (true, _) => return Err(bad()),
                        (false, _) => name(parts.concat())?,
                    });
                    k += 2;
                }
                (TokenTree::Group(g), _) => {
                    let mut inner = Group::new(g.delimiter(), paste(g.stream(), name, bad)?.into_iter().collect());
                    inner.set_span(g.span());
                    out.push(TokenTree::Group(inner));
                    k += 1;
                }
                (t, _) => {
                    out.push(t.clone());
                    k += 1;
                }
            }
        }
        Ok(out)
    }
    paste(subst(f.template.clone(), &bound), &name, &bad)
}

/// list_enum! @e's variants from its first list, @entries each one entry's
/// arguments, in their place: each `NAME [= value]`, a value cast to the
/// enum's integer type, as a list's can be any integer type.
pub(crate) fn expand_enum_list(e: &mut EnumDef, entries: &[proc_macro2::TokenStream]) -> Result<(), String> {
    use proc_macro2::{Ident, TokenStream, TokenTree};
    if e.from.is_empty() {
        return Ok(());
    }
    let (at, f) = e.from.remove(0);
    let (list, repr) = (&f.list, &e.repr);

    // the parameter the template pastes into the name: its argument, the label
    fn idents(ts: TokenStream, out: &mut Vec<Ident>) {
        for t in ts {
            match t {
                TokenTree::Ident(i) => out.push(i),
                TokenTree::Group(g) => idents(g.stream(), out),
                _ => {}
            }
        }
    }
    let before_eq: TokenStream = f.template.clone().into_iter()
        .take_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == '='))
        .collect();
    let mut name_idents = Vec::new();
    idents(before_eq, &mut name_idents);
    let label_param = name_idents.iter().find_map(|s| f.params.iter().position(|p| p == s));

    let mut variants = Vec::new();
    for entry in entries {
        let toks = from_list_entry(&f, entry)?;
        let (name, value) = match &toks[..] {
            [TokenTree::Ident(name)] => (name.clone(), None),
            [TokenTree::Ident(name), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' && !v.is_empty() => {
                let v: TokenStream = v.iter().cloned().collect();
                let value = syn::parse2(quote::quote!((#v) as #repr))
                    .map_err(|err| format!("{list}: {name} = {v}: {err}"))?;
                (name.clone(), Some(value))
            }
            _ => return Err(format!("{list}: the template gives {} - not NAME [= value]",
                                    toks.into_iter().collect::<TokenStream>())),
        };
        let label = label_param.and_then(|n| split_commas(entry.clone()).into_iter().nth(n))
            .and_then(|a| match a.into_iter().collect::<Vec<_>>().as_slice() {
                [TokenTree::Ident(l)] => Some(ident(l)),
                _ => None,
            });
        variants.push(EnumVariant { name, value, label, docs: Vec::new() });
    }
    let n = variants.len();
    e.variants.splice(at..at, variants);
    for (later, _) in &mut e.from {
        *later += n;
    }
    Ok(())
}

/// list_enum! @e, for its next list's callback: its variants, and the lists
/// not yet expanded in their place.
pub(crate) fn enum_tokens(e: &EnumDef) -> proc_macro2::TokenStream {
    use quote::{format_ident, quote};
    let kind = format_ident!("{}", e.kind.attr());
    let own_to_text = e.own_to_text.then(|| quote!(#[own_to_text]));
    let prefix = e.prefix.as_ref().map(|p| format_ident!("{p}")).map(|p| quote!(#[prefix(#p)]));
    let nr = e.nr.then(|| quote!(#[nr]));
    let (name, repr) = (&e.name, &e.repr);
    let lists = |i: usize| -> proc_macro2::TokenStream {
        e.from.iter().filter(|(at, _)| *at == i).map(|(_, f)| {
            let (list, params, template) = (&f.list, &f.params, &f.template);
            quote!(#list(#(#params,)* ..) => #template,)
        }).collect()
    };
    let mut body = proc_macro2::TokenStream::new();
    for (i, v) in e.variants.iter().enumerate() {
        body.extend(lists(i));
        let (docs, name, value) = (&v.docs, &v.name, v.value.as_ref().map(|x| quote!(= #x)));
        let label = v.label.as_ref().map(|l| quote!(#[label(#l)]));
        body.extend(quote!(#(#[doc = #docs])* #label #name #value,));
    }
    body.extend(lists(e.variants.len()));
    quote!(#[#kind] #own_to_text #prefix #nr pub enum #name: #repr { #body })
}

/// @ty's generic arguments, if it's @name<..> by any path.
fn generic_args<'a>(ty: &'a syn::Type, name: &str) -> Option<Vec<&'a syn::GenericArgument>> {
    let syn::Type::Path(p) = ty else { return None };
    let (last, path) = p.path.segments.iter().collect::<Vec<_>>().split_last().map(|(l, p)| (*l, p.to_vec()))?;
    if p.qself.is_some() || last.ident != name || path.iter().any(|s| !s.arguments.is_none()) {
        return None;
    }
    match &last.arguments {
        syn::PathArguments::AngleBracketed(a) => Some(a.args.iter().collect()),
        _ => None,
    }
}

/// @ty's path's names, if it's a plain path - no generics but lifetimes, which
/// C doesn't see.
fn path_names(ty: &syn::Type) -> Option<Vec<String>> {
    let lifetimes_only = |a: &syn::PathArguments| match a {
        syn::PathArguments::None => true,
        syn::PathArguments::AngleBracketed(a) => a.args.iter().all(|a| matches!(a, syn::GenericArgument::Lifetime(_))),
        syn::PathArguments::Parenthesized(_) => false,
    };
    match ty {
        syn::Type::Path(p) if p.qself.is_none() && p.path.segments.iter().all(|s| lifetimes_only(&s.arguments)) =>
            Some(p.path.segments.iter().map(|s| ident(&s.ident)).collect()),
        _ => None,
    }
}

// ---- field types --------------------------------------------------------------

/// An integer, as TypeInfo reads one: its size in bytes - None for the
/// platform's, which the compiler supplies - its endianness, Native, Little
/// or Big, and whether it's signed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IntType {
    pub(crate) bytes:  Option<u8>,
    pub(crate) endian: &'static str,
    pub(crate) signed: bool,
}

/// The integer type @ty is, if it's one TypeInfo reads as an integer: Rust's
/// and C's integers, the endian ones - Le8..Le64, whatever value they hold,
/// zerocopy's le:: and be:: - and the unsigned atomics.
fn int_type(ty: &syn::Type) -> Option<IntType> {
    let syn::Type::Path(p) = ty else { return None };
    let le = p.path.segments.last()
        .is_some_and(|s| matches!(s.ident.to_string().as_str(), "Le8" | "Le16" | "Le32" | "Le64"));
    if p.qself.is_some() || p.path.segments.iter().rev().skip(le as usize).any(|s| !s.arguments.is_none()) {
        return None;
    }
    let path: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
    let int = |bytes, endian, signed| Some(IntType { bytes, endian, signed });
    match path.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["u8"] | ["bool"]                       => int(Some(1), "Native", false),
        ["u16"]                                 => int(Some(2), "Native", false),
        ["u32"]                                 => int(Some(4), "Native", false),
        ["u64"]                                 => int(Some(8), "Native", false),
        ["i8"]                                  => int(Some(1), "Native", true),
        ["i16"]                                 => int(Some(2), "Native", true),
        ["i32"]                                 => int(Some(4), "Native", true),
        ["i64"]                                 => int(Some(8), "Native", true),
        ["usize"]                               => int(None,    "Native", false),
        ["isize"]                               => int(None,    "Native", true),
        [.., "c_char"]                          => int(Some(1), "Native", true),
        [.., "c_short"]                         => int(Some(2), "Native", true),
        [.., "c_ushort"]                        => int(Some(2), "Native", false),
        [.., "c_int"]                           => int(Some(4), "Native", true),
        [.., "c_uint"]                          => int(Some(4), "Native", false),
        [.., "c_long"]                          => int(None,    "Native", true),
        [.., "c_ulong"]                         => int(None,    "Native", false),
        [.., "c_longlong"]                      => int(Some(8), "Native", true),
        [.., "c_ulonglong"]                     => int(Some(8), "Native", false),
        [.., "AtomicU8"]                        => int(Some(1), "Native", false),
        [.., "AtomicU16"]                       => int(Some(2), "Native", false),
        [.., "AtomicU32"]                       => int(Some(4), "Native", false),
        [.., "AtomicU64"]                       => int(Some(8), "Native", false),
        [.., "AtomicUsize" | "AtomicULong"]     => int(None,    "Native", false),
        [.., "Le8"]                             => int(Some(1), "Little", false),
        [.., "Le16"] | ["le", "U16"]            => int(Some(2), "Little", false),
        [.., "Le32"] | ["le", "U32"]            => int(Some(4), "Little", false),
        [.., "Le64"] | ["le", "U64"]            => int(Some(8), "Little", false),
        ["be", "U16"]                           => int(Some(2), "Big",    false),
        ["be", "U32"]                           => int(Some(4), "Big",    false),
        ["be", "U64"]                           => int(Some(8), "Big",    false),
        _ => None,
    }
}

/// A field's Rust type, as #[derive(TypeInfo)] describes it.
pub(crate) enum FieldType<'a> {
    /// `[elem; n]`
    Array { elem: &'a syn::Type, n: &'a syn::Expr },
    /// A pointer, reference, function or generic: nothing to describe
    Opaque,
    /// Anything else: the integer it is, if it's one, and the last segment
    /// of its path.
    Named { int: Option<IntType>, name: String },
}

/// What @ty is, for #[derive(TypeInfo)].
pub(crate) fn field_type(ty: &syn::Type) -> FieldType<'_> {
    match ty {
        syn::Type::Array(a) => FieldType::Array { elem: &a.elem, n: &a.len },
        t => match (int_type(t), path_names(t)) {
            (None, None) => FieldType::Opaque,
            (int, p) => FieldType::Named { int, name: p.and_then(|p| p.last().cloned()).unwrap_or_default() },
        },
    }
}

/// E, if @ty is `[E; 0]`: C's flexible array member.
pub(crate) fn flex_elem(ty: &syn::Type) -> Option<&syn::Type> {
    match ty {
        syn::Type::Array(syn::TypeArray {
            elem, len: syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. }), ..
        }) if n.base10_digits() == "0" => Some(elem),
        _ => None,
    }
}

/// An integer literal's value, decimal or 0x hex: a discriminant, a bit
/// number.
pub(crate) fn int_of(x: &syn::Expr) -> Option<u64> {
    match x {
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(i), .. }) => i.base10_parse().ok(),
        _ => None,
    }
}

/// @ty's T, if it's @name<T> by any path.
pub(crate) fn generic_arg<'a>(ty: &'a syn::Type, name: &str) -> Option<&'a syn::Type> {
    match generic_args(ty, name)?.as_slice() {
        [syn::GenericArgument::Type(t)] => Some(t),
        _ => None,
    }
}

// ---- entry points ----------------------------------------------------------

/// Parse a derive's input: exactly one struct or union.
pub(crate) fn parse_cstruct(item: &proc_macro2::TokenStream) -> Result<StructDef, String> {
    let expected = "expected a struct or union with named fields";
    let d = syn::parse2::<syn::DeriveInput>(item.clone()).map_err(|e| format!("{expected} - {e}"))?;
    parse_struct(d)?.ok_or_else(|| expected.to_string())
}
