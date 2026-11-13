// SPDX-License-Identifier: GPL-2.0
//
// Parsing the items the macros (fs/cstruct-macros) take, which include!()
// this file - so it's plain `//` comments - and build on syn: the kernel
// build's, vendor/kernel-rust, with a bare rustc.
//
// The items:
//
//   #[derive(CStruct)] struct / union, named fields.
//
//   #[type_name(NAME)] on the struct: its name to TypeInfo, where the item's
//   differs - a family's definition, X__head, whose name the family's type
//   X<S> has (util/vstructs.rs).
//
//   c_xmacro! { NAME { (args), (args), ... } }: an x-macro list - a
//   macro, NAME!(cb), that hands the whole list to cb!.
//
//   c_bitmask! { LE64_BITMASK(struct foo, field), strip PREFIX_ {
//       NAME(start, end), ... } }: bitfields in a flags word - accessors on
//   foo named for NAME without PREFIX_, lowercased.
//
//   c_ioctl! { NAME = _IOW(type, nr, ARG), ... }: ioctl numbers - a marker
//   type binding the opcode to ARG, the opcode computed from the target's own
//   _IOC constants (fs/util/ioctl.rs).

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CKind {
    Struct,
    Union,
}

/// A #[derive(CStruct)] struct or union: its name and fields, in order.
pub struct CStructDef {
    /// The item's name.
    pub ident:     syn::Ident,
    /// #[type_name(NAME)]: the type's name, where the item's differs - a
    /// family's definition, X__head, whose name the family's type X<S> has
    /// (util/vstructs.rs).
    pub type_name: Option<syn::Ident>,
    pub generics:  syn::Generics,
    pub kind:      CKind,
    pub fields:    Vec<CFieldDef>,
}

impl CStructDef {
    /// The type's name: #[type_name]'s, or the item's.
    pub fn name(&self) -> &syn::Ident {
        self.type_name.as_ref().unwrap_or(&self.ident)
    }
}

pub struct CFieldDef {
    pub ident:    syn::Ident,
    pub ty:       syn::Type,
    pub vis:      syn::Visibility,
    /// #[c_bitfield]: its type is a bitfield-struct type, whose members are
    /// the struct's too.
    pub bitfield: bool,
    /// #[counted_by(FIELD)], on a trailing `[T; 0]`: the field its length
    /// is - C's __counted_by(), as the struct's FlexArray impl.
    pub counted_by: Option<syn::Ident>,
    /// #[placed_at]: the field #[placed] added, its place's At - which
    /// #[derive(Init)]'s init() takes. (cstruct-macros' place.rs.)
    pub placed_at: bool,
}

/// An x-macro list.
#[derive(Debug)]
pub struct CXMacro {
    pub name:     String,
    pub entries:  Vec<CXEntry>,
}

/// An entry of an x-macro list.
#[derive(Debug)]
pub enum CXEntry {
    /// (args)
    Args,
    /// SUB() - another list's entries, here - or SUB((params) => (template)):
    /// SUB's entries, each mapped to the template.
    Sub { list: String, map: Option<CXMap> },
}

#[derive(Debug)]
pub struct CXMap {
    pub params:   Vec<String>,
    /// An entry's arguments, mapped: params, and concat(A, b) pastes.
    pub template: String,
}

/// Bitfields in one flags word: @macro_name is LE64_BITMASK, LE32_BITMASK,
/// LE16_BITMASK or BITMASK; @c_type the C struct type, "struct foo"; @field
/// the flags word, as C and Rust both write it: "flags[0]".
#[derive(Debug)]
pub struct CBitmask {
    pub macro_name: String,
    /// The struct as written, "struct foo".
    pub c_type:     String,
    /// The type the accessors are on: "foo" - or for `struct Head<foo>`, a
    /// family's C definition, "foo__head".
    pub rust_type:  String,
    pub field:      String,
    pub strip:      Option<String>,
    pub entries:    Vec<CBitmaskEntry>,
}

#[derive(Debug)]
pub struct CBitmaskEntry {
    pub name:  String,
    pub start: String,
    pub end:   String,
}

impl CBitmask {
    /// The Rust accessor for @name: PREFIX_ stripped, lowercased - and with a
    /// leading _ if that starts with a digit: BCH_SB_128_BIT_MACS.
    pub fn method(&self, name: &str) -> String {
        let n = self.strip.as_deref().and_then(|p| name.strip_prefix(p)).unwrap_or(name);
        let n = n.to_lowercase();
        if n.starts_with(|c: char| c.is_ascii_digit()) {
            format!("_{n}")
        } else {
            rust_ident(&n)
        }
    }
}

/// c_ioctl!: ioctl numbers, C's #define NAME _IOW(type, nr, arg).
#[derive(Debug)]
pub struct CIoctl {
    pub entries: Vec<CIoctlEntry>,
}

/// One ioctl: @dir is its _IO*() macro - _IO, _IOR, _IOW or _IOWR - and
/// @ty and @nr its type and number, as written. @arg is the argument's Rust
/// type, None for _IO.
#[derive(Debug)]
pub struct CIoctlEntry {
    pub name:  String,
    pub dir:   String,
    pub ty:    String,
    pub nr:    String,
    pub arg:   Option<String>,
}

/// What an enum's values can be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CEnumKind {
    /// Only ever those listed: a Rust enum.
    Closed,
    /// Anything, from outside: a newtype with the values as constants.
    Open,
    /// Masks, or'd: a bitflags type.
    Flags,
}

impl CEnumKind {
    /// The attribute that says it: #[closed], #[open], #[flags].
    pub fn attr(self) -> &'static str {
        match self {
            CEnumKind::Closed => "closed",
            CEnumKind::Open   => "open",
            CEnumKind::Flags  => "flags",
        }
    }
}

/// Items from an x-macro list: `LIST(params) => template`. Each entry's
/// arguments are bound to the params - `..` the rest - and `concat(A, b)` in the
/// template is pasted into one identifier.
#[derive(Debug, Clone)]
pub struct CFromList {
    pub list:     String,
    pub params:   Vec<String>,
    pub template: String,
}

/// An enum, and what its values can be. @name is None only while a
/// tag_enum() is being parsed.
#[derive(Debug, Clone)]
pub struct CEnumDef {
    pub kind:     CEnumKind,
    pub name:     Option<String>,
    /// Its doc comment.
    pub docs:     Vec<String>,
    /// The integer type: u32, i32, u64.
    pub repr:     String,
    pub variants: Vec<CEnumVariant>,
    /// `LIST(params) => VARIANT [= value]`: variants from lists, each in
    /// place before the written one at its index - expand_enum_list() makes
    /// them, a list at a time.
    pub from:     Vec<(usize, CFromList)>,
    /// #[own_to_text]: it prints as more than its name - an open or flags
    /// enum #[bch_enum] otherwise gives ToText.
    pub own_to_text: bool,
    /// #[prefix(P)]: what C's names for its variants start with, Rust's
    /// without it.
    pub prefix:   Option<String>,
    /// #[nr]: NR, past the largest value - a number, not one of its values.
    pub nr:       bool,
}

impl CEnumDef {
    /// What @v prints as: its #[label], or its name - past its #[prefix], if
    /// it has one. Not a flag's, whose names would be its bits'.
    pub fn label<'a>(&self, v: &'a CEnumVariant) -> Option<&'a str> {
        v.label.as_deref().or_else(|| {
            (self.kind != CEnumKind::Flags).then_some(())?;
            let p = self.prefix.as_deref().unwrap_or_default();
            Some(v.name.strip_prefix(p).unwrap_or(&v.name))
        })
    }
}

/// An enum's variant. Its value is Rust source, or none for the one after the
/// last; the generator replaces them with what they evaluate to. Its label is
/// what it prints as, `#[label("..")]`: a list's variant's is the argument its
/// template pastes into the name - `stripe`, of `concat(BCH_WATERMARK_, name)` - as
/// C's string tables have it; none, its name.
#[derive(Debug, Clone)]
pub struct CEnumVariant {
    pub name:   String,
    pub value:  Option<String>,
    pub label:  Option<String>,
    pub docs:   Vec<String>,
}

/// One arm of a tagged_union!: its name, payload type, and tag value.
#[derive(Debug, Clone)]
pub struct CTaggedArm {
    pub name:  String,
    pub ty:    String,
    pub value: String,
}

/// tagged_union!: a tagged union with a stable representation, as
/// cstruct-macros' tagged_union.rs parses its declaration.
///
/// The storage is what C has: the tag, then a union of the arms, overlaid
/// with the pad, if any. @size says what its size is a function of: nothing,
/// a constant, the pad's; the header's count - the storage another's, a
/// header its arms embed and run past, and what's generated only the typed
/// view of it; or the tag - each value as long as the arm its tag selects,
/// the head the start of every arm.
#[derive(Debug)]
pub struct CTaggedUnion {
    pub name:      String,
    /// `union NAME in STORAGE`: unsized, over another's storage - see
    /// storage; else `struct NAME`, its own sized storage, which this
    /// defines.
    pub union:     bool,
    /// `in STORAGE`: the storage is that type, defined elsewhere, and its C
    /// too; what's generated is the typed view of it.
    pub storage:   Option<String>,
    /// The tag's name: its field, and the accessor.
    pub tag_field: String,
    pub tag_ty:    String,
    /// `= TAG_ENUM`: the tag as handed out, a newtype over an integer the
    /// tag widens to - or empty, the tag as it is.
    pub tag_enum:  String,
    /// `tag NAME: enum TYPE`: the tag is a C enum, closed - stored as it,
    /// the arms' values its variants. Only in memory: no bytes from outside,
    /// which could hold a value that isn't one.
    pub tag_is_enum: bool,
    /// `plain`: every arm is plain data - one not defined here a type of its
    /// own's, not a #[counted] family's head.
    pub plain:     bool,
    pub arms:      Vec<CTaggedArm>,
    pub pad:       Option<String>,
    /// `packed`: C's __packed - for a struct, the payload right after the
    /// tag whatever the arms' alignment, which is what the format says.
    pub packed:    bool,
    /// `size: ..`: what its size is a function of.
    pub size:      CTaggedSize,
    /// Arm types defined in the block, for nestify::nest!.
    pub defs:      Vec<CTaggedDef>,
}

/// What a tagged_union!'s size is a function of, `size: pad|header|arm`:
/// the rest follows from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CTaggedSize {
    /// Nothing - a constant, its pad's: a struct, its own storage, which
    /// holds every arm, and a value of plain data.
    Pad,
    /// Its header's count: a union over another's storage, an unsized
    /// vstruct, its arms embedding the header and running past it as far as
    /// the count says - a family (util/vstructs.rs), each state's reference
    /// to which has its arm's bytes.
    Header,
    /// Its tag: the arm the tag selects, whose size it is - a union, its
    /// storage the head every arm starts with, the tag in it; a value read
    /// off bytes and written back to them, no C of its own (ArmSized).
    Arm,
}

impl CTaggedSize {
}

/// An arm's type, defined in its tagged_union!'s block.
#[derive(Debug, Clone)]
pub struct CTaggedDef {
    pub name: String,
}

#[derive(Debug)]
pub enum CItem {
    XMacro(CXMacro),
    Bitmask(CBitmask),
    Ioctl(CIoctl),
    Enum(CEnumDef),
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "union",
    "gen",
];

/// A C name as Rust spells it: a keyword gets a _ - type is type_, as
/// bindgen has it.
pub fn rust_ident(name: &str) -> String {
    if RUST_KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

// ---- tokens ------------------------------------------------------------

/// A token, as the text is written from: a word - an identifier, a literal,
/// a lifetime - or a punctuation character.
enum Flat {
    Word(String),
    Punct(char),
}

/// syn's tokens as the tokenizer has them: a group its delimiters, a
/// lifetime one word, a raw identifier without its r#.
fn flat(ts: &proc_macro2::TokenStream) -> Vec<Flat> {
    use proc_macro2::{Delimiter, TokenTree};
    let mut out = Vec::new();
    let mut it = ts.clone().into_iter().peekable();
    while let Some(t) = it.next() {
        match t {
            TokenTree::Ident(i) => out.push(Flat::Word(i.to_string().trim_start_matches("r#").to_string())),
            TokenTree::Literal(l) => out.push(Flat::Word(l.to_string())),
            TokenTree::Punct(p) if p.as_char() == '\'' && matches!(it.peek(), Some(TokenTree::Ident(_))) => {
                let Some(TokenTree::Ident(i)) = it.next() else { unreachable!() };
                out.push(Flat::Word(format!("'{i}")));
            }
            TokenTree::Punct(p) => out.push(Flat::Punct(p.as_char())),
            TokenTree::Group(g) => {
                let delims = match g.delimiter() {
                    Delimiter::Parenthesis => Some(('(', ')')),
                    Delimiter::Bracket     => Some(('[', ']')),
                    Delimiter::Brace       => Some(('{', '}')),
                    Delimiter::None        => None,
                };
                out.extend(delims.map(|(o, _)| Flat::Punct(o)));
                out.extend(flat(&g.stream()));
                out.extend(delims.map(|(_, c)| Flat::Punct(c)));
            }
        }
    }
    out
}

/// @t's Rust source, as tokens_to_rust() writes it.
pub fn rust_text(t: &impl quote::ToTokens) -> String {
    join_flat(&flat(&t.to_token_stream()))
}

/// A space only between two words: punctuation stays as written, so `<<`
/// and `->` come back whole - the tokenizer splits them.
fn join_flat(toks: &[Flat]) -> String {
    let word = |t: &Flat| matches!(t, Flat::Word(_));
    let mut out = String::new();
    for (n, t) in toks.iter().enumerate() {
        if n > 0 && word(&toks[n - 1]) && word(t) {
            out.push(' ');
        }
        match t {
            Flat::Word(s) => out.push_str(s),
            Flat::Punct(ch) => out.push(*ch),
        }
    }
    out
}

// ---- attributes ------------------------------------------------------------

/// A family attribute's arguments (cstruct-macros' vstructs.rs):
/// `#[counted(PATH, tail = ..)]`, `#[unsized_vstruct(tail = ..)]`,
/// `#[grown(from = FIELD)]`; `bkey`, for a key's value.
#[derive(Debug, Default)]
pub struct FamilyArgs {
    /// The one argument that isn't `key = value`: the count, a field path.
    pub count: Option<String>,
    /// `tail = [E]` or `tail = NAME: [E]`: the tail's name, if it's named,
    /// and E.
    pub tail: Option<(Option<String>, String)>,
    /// `from = FIELD`.
    pub from: Option<String>,
    /// `bkey`: a key's value - its btree/bkey/types.rs impls too.
    pub bkey: bool,
}

/// @src, a family attribute's arguments: an error for one it doesn't take -
/// @takes, of "count", "tail" and "from" - or one not written as its kind is.
/// The caller names the attribute and the struct.
pub fn parse_family_args(src: &str, takes: &[&str]) -> Result<FamilyArgs, String> {
    use proc_macro2::{Delimiter, TokenStream, TokenTree};
    let text = |t: &[TokenTree]| rust_text(&t.iter().cloned().collect::<TokenStream>());
    let mut args = FamilyArgs::default();
    for part in split_commas(src.parse().map_err(|e| format!("({src}): {e}"))?) {
        let part: Vec<TokenTree> = part.into_iter().collect();
        let (kind, set) = match part.as_slice() {
            [] => continue,
            [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' => {
                let bad = || format!("({}) not understood", text(&part));
                match ident(k).as_str() {
                    "tail" => {
                        let (name, slice) = match v {
                            [TokenTree::Ident(n), TokenTree::Punct(c), s @ ..] if c.as_char() == ':' => (Some(ident(n)), s),
                            s => (None, s),
                        };
                        let elem = match slice {
                            [TokenTree::Group(g)] if g.delimiter() == Delimiter::Bracket && !g.stream().is_empty() &&
                                !g.stream().into_iter().any(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ';')) =>
                                rust_text(&g.stream()),
                            _ => return Err(format!("tail = [E] - a slice - not tail = {}", text(v))),
                        };
                        ("tail", args.tail.replace((name, elem)).is_some())
                    }
                    "from" => match v {
                        [TokenTree::Ident(f)] => ("from", args.from.replace(ident(f)).is_some()),
                        _ => return Err(bad()),
                    },
                    k => return Err(format!("no argument {k}")),
                }
            }
            [TokenTree::Ident(k)] if k == "bkey" => ("bkey", core::mem::replace(&mut args.bkey, true)),
            p => ("count", args.count.replace(text(p)).is_some()),
        };
        if !takes.contains(&kind) {
            return Err(format!("doesn't take ({})", text(&part)));
        }
        if set {
            return Err(format!("{kind} given twice"));
        }
    }
    Ok(args)
}

// ---- structs ---------------------------------------------------------------

/// The struct or union @d; None if it's an enum.
fn parse_struct(d: syn::DeriveInput) -> Result<Option<CStructDef>, String> {
    let name = &d.ident;
    let (kind, fields) = match d.data {
        syn::Data::Struct(s) => (CKind::Struct, match s.fields {
            syn::Fields::Named(f) => Some(f.named),
            _ => None,
        }),
        syn::Data::Union(u) => (CKind::Union, Some(u.fields.named)),
        syn::Data::Enum(_) => return Ok(None),
    };
    let fields_of = fields
        .filter(|_| d.generics.const_params().next().is_none())
        .ok_or_else(|| format!("{name}: a CStruct has named fields, and no const generics - an unsized \
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
        let (bitfield, placed_at) = (bare("c_bitfield"), bare("placed_at"));
        let ident = f.ident.expect("named fields");
        fields.push(CFieldDef { ident, ty: f.ty, vis: f.vis, bitfield, counted_by, placed_at });
    }

    // #[type_name], or what it expands to - a family's macro sees it
    // unexpanded, the derives expanded
    let type_name = d.attrs.iter()
        .filter(|a| a.path().is_ident("type_name") || a.path().is_ident("type_name_of"))
        .find_map(|a| a.parse_args::<syn::Ident>().ok());
    Ok(Some(CStructDef { ident: d.ident, type_name, generics: d.generics, kind, fields }))
}

/// The width of integer type @ty, in bits.
pub fn int_bits(ty: &str) -> Option<u32> {
    Some(match ty.trim() {
        "u8" | "i8" => 8,
        "u16" | "i16" => 16,
        "u32" | "i32" => 32,
        "u64" | "i64" => 64,
        _ => return None,
    })
}

// ---- c_xmacro!, c_bitmask! -------------------------------------------------

/// c_xmacro!'s body: NAME { (args), ... }
fn parse_xmacro(m: &syn::Macro) -> Result<CXMacro, String> {
    m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        input.call(syn::Attribute::parse_outer)?;
        let name: syn::Ident = input.parse()?;
        let list;
        syn::braced!(list in input);

        let mut entries = Vec::new();
        while !list.is_empty() {
            entries.push(if list.peek(syn::token::Paren) {
                let args;
                syn::parenthesized!(args in list);
                args.parse::<proc_macro2::TokenStream>()?;
                CXEntry::Args
            } else {
                // SUB(), or SUB((params) => (template))
                let sub: syn::Ident = list.parse()?;
                let inner;
                syn::parenthesized!(inner in list);
                let map = match inner.is_empty() {
                    true => None,
                    false => {
                        let params;
                        syn::parenthesized!(params in inner);
                        let params = syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>
                            ::parse_terminated(&params)?.iter().map(ident).collect();
                        inner.parse::<syn::Token![=>]>()?;
                        let template;
                        syn::parenthesized!(template in inner);
                        let template = rust_text(&template.parse::<proc_macro2::TokenStream>()?);
                        Some(CXMap { params, template })
                    }
                };
                CXEntry::Sub { list: ident(&sub), map }
            });
            if !list.is_empty() {
                list.parse::<syn::Token![,]>()?;
            }
        }
        Ok(CXMacro { name: ident(&name), entries })
    }).map_err(|e| format!("c_xmacro!: an entry is (args), SUB(), or SUB((params) => (args)) - {e}"))
}

/// c_bitmask!'s body: LE64_BITMASK(struct foo, field), strip PREFIX_ { NAME(start, end), ... }
/// @ts split at its commas - its groups' are their own.
pub fn split_commas(ts: proc_macro2::TokenStream) -> Vec<proc_macro2::TokenStream> {
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

/// A parse's error, as the parsers' are: a String.
fn syn_err(what: &str) -> impl Fn(syn::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

fn parse_bitmask(m: &syn::Macro) -> Result<CBitmask, String> {
    m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        input.call(syn::Attribute::parse_outer)?;
        let macro_name: syn::Ident = input.parse()?;
        if !matches!(ident(&macro_name).as_str(), "LE64_BITMASK" | "LE32_BITMASK" | "LE16_BITMASK" | "BITMASK") {
            return Err(syn::Error::new(macro_name.span(), format!("{macro_name}: not a bitmask macro")));
        }
        let args;
        syn::parenthesized!(args in input);
        args.parse::<syn::Token![struct]>()?;
        let ty: syn::Path = args.parse()?;
        args.parse::<syn::Token![,]>()?;
        let field = rust_text(&args.parse::<proc_macro2::TokenStream>()?);

        // NAME, or NAME<ARG>
        let bad = || syn::Error::new_spanned(&ty, format!(
            "{macro_name}: expected a struct type - struct foo, struct Head<foo> for a family's, or struct \
             foo<STATE> for a family's type in a state"));
        let (n, arg) = match ty.segments.iter().collect::<Vec<_>>().as_slice() {
            [s] => (ident(&s.ident), match &s.arguments {
                syn::PathArguments::None => None,
                syn::PathArguments::AngleBracketed(a) => match a.args.iter().collect::<Vec<_>>().as_slice() {
                    [syn::GenericArgument::Type(syn::Type::Path(t))] => Some(t.path.get_ident().map(ident).ok_or_else(bad)?),
                    _ => return Err(bad()),
                },
                _ => return Err(bad()),
            }),
            _ => return Err(bad()),
        };
        let (rust_type, c_type) = match (n.as_str(), arg.as_deref()) {
            (_, None) => (n.clone(), format!("struct {n}")),
            // a family's C definition, X__head (util/vstructs.rs): its header's
            // fields, which each state's definition derefs to
            ("Head", Some(f)) => (format!("{f}__head"), format!("struct {f}")),
            // a family's type in a state: a #[grown] type's value in place, for
            // its floor's fields - every view of it reaches X<Validated>
            (_, Some(st @ ("Whole" | "Validated" | "Unvalidated"))) =>
                (format!("{n}<crate::util::vstructs::{st}>"), format!("struct {n}")),
            _ => return Err(bad()),
        };

        input.parse::<Option<syn::Token![,]>>()?;
        let strip = match input.peek(syn::Ident) && input.fork().parse::<syn::Ident>()? == "strip" {
            true => {
                input.parse::<syn::Ident>()?;
                Some(ident(&input.parse()?))
            }
            false => None,
        };

        let list;
        syn::braced!(list in input);
        let mut entries = Vec::new();
        while !list.is_empty() {
            let name: syn::Ident = list.parse()?;
            let range;
            syn::parenthesized!(range in list);
            let [start, end] = split_commas(range.parse()?).try_into().map_err(|_|
                syn::Error::new(name.span(), format!("{name}: expected NAME(start, end)")))?;
            entries.push(CBitmaskEntry { name: ident(&name), start: rust_text(&start), end: rust_text(&end) });
            list.parse::<Option<syn::Token![,]>>()?;
        }

        Ok(CBitmask { macro_name: ident(&macro_name), c_type, rust_type, field, strip, entries })
    }).map_err(syn_err("c_bitmask!"))
}

/// c_ioctl!'s body: NAME = _IOW(type, nr, ARG), ... - _IO without the ARG.
fn parse_ioctl(m: &syn::Macro) -> Result<CIoctl, String> {
    let entries = m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
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
            let (ty, nr, arg) = match (ident(&dir).as_str(), args.as_slice()) {
                ("_IO", [ty, nr]) => (rust_text(ty), rust_text(nr), None),
                ("_IOR" | "_IOW" | "_IOWR", [ty, nr, arg]) => (rust_text(ty), rust_text(nr), Some(rust_text(arg))),
                ("_IO", _) => return Err(err("expected _IO(type, nr)".into())),
                ("_IOR" | "_IOW" | "_IOWR", _) => return Err(err(format!("expected {dir}(type, nr, ARG)"))),
                _ => return Err(err(format!("{dir} isn't _IO, _IOR, _IOW or _IOWR"))),
            };
            entries.push(CIoctlEntry { name: ident(&name), dir: ident(&dir), ty, nr, arg });
            input.parse::<Option<syn::Token![,]>>()?;
        }
        Ok(entries)
    }).map_err(syn_err("c_ioctl!: an entry is NAME = _IOW(type, nr, ARG)"))?;
    if entries.is_empty() {
        return Err("c_ioctl!: expected NAME = _IOW(type, nr, ARG), ...".into());
    }
    Ok(CIoctl { entries })
}

/// An enum's head, `#[open|closed|flags] pub enum NAME: REPR`, with no
/// variants yet; the attributes that aren't
/// its, and its body.
pub fn parse_enum_head(input: syn::parse::ParseStream<'_>)
        -> syn::Result<(CEnumDef, Vec<syn::Attribute>, proc_macro2::TokenStream)> {
    let mut kind = None;
    let (mut own_to_text, mut prefix, mut nr) = (false, None, false);
    let mut others = Vec::new();
    for a in input.call(syn::Attribute::parse_outer)? {
        let id = |a: &syn::Attribute| a.parse_args::<syn::Ident>().map(|n| ident(&n));
        match a.path().get_ident().map(|i| i.to_string()).as_deref() {
            Some("open")        => kind = Some(CEnumKind::Open),
            Some("closed")      => kind = Some(CEnumKind::Closed),
            Some("flags")       => kind = Some(CEnumKind::Flags),
            Some("own_to_text") => own_to_text = true,
            Some("prefix")      => prefix = Some(id(&a)?),
            Some("nr")          => nr = true,
            _ => others.push(a),
        }
    }
    input.parse::<syn::Visibility>()?;
    input.parse::<syn::Token![enum]>()?;
    let name = ident(&input.parse()?);
    let what = name.clone();
    let kind = kind.ok_or_else(|| input.error(format!("{what}: #[open], #[closed] or #[flags]")))?;
    input.parse::<syn::Token![:]>()?;
    let repr = ident(&input.parse()?);
    if int_bits(&repr).is_none() {
        return Err(input.error(format!("{what}: REPR is an integer type")));
    }
    let body;
    syn::braced!(body in input);
    let def = CEnumDef { kind, name: Some(name), docs: Vec::new(), repr, variants: Vec::new(), from: Vec::new(),
                         own_to_text, prefix, nr };
    Ok((def, others, body.parse()?))
}

/// @a's text, if it's a doc comment's line.
pub fn doc_of(a: &syn::Attribute) -> Option<String> {
    match &a.meta {
        syn::Meta::NameValue(syn::MetaNameValue {
            path, value: syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }), ..
        }) if path.is_ident("doc") => Some(s.value()),
        _ => None,
    }
}

/// An enum's variant, `[#[..]] NAME[(Type)] [= value]`: its doc and #[label]
/// read into its CEnumVariant; its other attributes, and its type.
pub fn parse_variant(v: proc_macro2::TokenStream)
        -> syn::Result<(CEnumVariant, Vec<syn::Attribute>, Option<syn::Type>)> {
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
    let value = v.discriminant.map(|(_, x)| rust_text(&x));
    Ok((CEnumVariant { name: ident(&v.ident), value, label, docs }, others, ty))
}

/// list_enum!'s body: its head, then { A = expr, B, ... },
/// or `LIST(params) => template` for each of a list's entries.
fn parse_enum(m: &syn::Macro) -> Result<CEnumDef, String> {
    m.parse_body_with(|input: syn::parse::ParseStream<'_>| {
        let (mut e, _, body) = parse_enum_head(input)?;
        for v in split_commas(body) {
            match parse_from_list(&v) {
                Some(f) => e.from.push((e.variants.len(), f)),
                None => e.variants.push(parse_variant(v)?.0),
            }
        }
        Ok(e)
    }).map_err(syn_err("list_enum!: expected #[open|closed|flags] pub enum NAME: REPR { ... }"))
}

/// @i as the tokenizer has an identifier: a raw one without its r#.
fn ident(i: &syn::Ident) -> String {
    i.to_string().trim_start_matches("r#").to_string()
}

impl CTaggedUnion {
    pub fn check(&self) -> Result<(), String> {
        let tu = self;
        // a constant size is a struct's, its own storage, which holds its arms; a
        // header's count or the arm's own size, a union's over another's storage,
        // the head its arms start with
        let ok = match tu.size {
            CTaggedSize::Pad                       => !tu.union && tu.storage.is_none() && tu.pad.is_some(),
            CTaggedSize::Header | CTaggedSize::Arm => tu.union && tu.storage.is_some() && tu.pad.is_none(),
        };
        if !ok {
            return Err(format!("tagged_union! {}: `struct NAME {{ .., pad: TYPE, size: pad }}` - its \
                                own storage, which holds its arms, the pad's size - or `union NAME \
                                in STORAGE {{ .., size: header }}` - over another's storage, whose \
                                arms run past its header as far as its count says - or `union NAME \
                                in HEAD {{ .., size: arm }}` - the head every arm starts with, each \
                                as long as its arm", tu.name));
        }
        if (tu.plain || tu.tag_is_enum) && tu.size != CTaggedSize::Pad {
            return Err(format!("tagged_union! {}: `plain` and `tag NAME: enum TYPE` are for a \
                                struct's own storage, `size: pad`", tu.name));
        }
        if tu.storage.is_some() {
            if let Some(d) = tu.defs.first() {
                return Err(format!("tagged_union! {}: {} is defined here, but the storage is \
                                    another's - `in STORAGE` - and so are its arms' types",
                                   tu.name, d.name));
            }
        }
        Ok(())
    }
}

/// `LIST(params) => template` at @item - a list_enum! variant - if it is one.
fn parse_from_list(item: &proc_macro2::TokenStream) -> Option<CFromList> {
    use proc_macro2::{Delimiter, TokenTree};
    let t: Vec<TokenTree> = item.clone().into_iter().collect();
    match t.as_slice() {
        [TokenTree::Ident(list), TokenTree::Group(params), TokenTree::Punct(eq), TokenTree::Punct(gt), template @ ..]
            if params.delimiter() == Delimiter::Parenthesis && eq.as_char() == '=' && gt.as_char() == '>' &&
               !template.is_empty() =>
            Some(CFromList {
                list:     ident(list),
                params:   split_commas(params.stream()).iter().map(rust_text).collect(),
                template: rust_text(&template.iter().cloned().collect::<proc_macro2::TokenStream>()),
            }),
        _ => None,
    }
}

/// @template, its concat(A, b, ..)s spelled as paste!'s [<A b ..>].
pub fn paste_concat(template: &str) -> Result<String, String> {
    let mut out = template.to_string();
    while let Some(i) = out.find("concat(") {
        let j = i + out[i..].find(')').ok_or_else(|| format!("an unclosed concat( in the template {template}"))?;
        let parts: Vec<&str> = out[i + "concat(".len()..j].split(',').map(str::trim).collect();
        let pasted = format!("[<{}>]", parts.join(" "));
        out.replace_range(i..=j, &pasted);
    }
    Ok(out)
}

/// What @f's template makes of one entry of its list, @entry its source: its
/// parameters substituted, then its camel(n)s and concat(A, b, ..)s pasted.
fn from_list_entry(f: &CFromList, entry: &str) -> Result<Vec<proc_macro2::TokenTree>, String> {
    use proc_macro2::{Delimiter, Group, Ident, Span, TokenStream, TokenTree};
    let list = &f.list;
    let lex = |s: &str| s.parse::<TokenStream>().map_err(|e| format!("{list}: {s}: {e}"));
    let args = split_commas(lex(entry)?);
    let mut bound: Vec<(&str, &TokenStream)> = Vec::new();
    for (n, p) in f.params.iter().enumerate() {
        if p == ".." {
            break;
        }
        let arg = args.get(n).ok_or_else(|| format!("{list}: entry ({entry}) has no {p}"))?;
        bound.push((p, arg));
    }

    fn subst(ts: TokenStream, bound: &[(&str, &TokenStream)]) -> TokenStream {
        ts.into_iter().flat_map(|t| match t {
            TokenTree::Ident(i) => match bound.iter().find(|(p, _)| i == p) {
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
    paste(subst(lex(&f.template)?, &bound), &name, &bad)
}

/// list_enum! @e's variants from its first list, @entries each the source of one
/// entry's arguments, in their place: each `NAME [= value]`, a value cast to
/// the enum's integer type, as a list's can be any integer type.
pub fn expand_enum_list(e: &mut CEnumDef, entries: &[String]) -> Result<(), String> {
    if e.from.is_empty() {
        return Ok(());
    }
    let (at, f) = e.from.remove(0);
    let mut variants = Vec::new();
    use proc_macro2::{TokenStream, TokenTree};
    let lex = |s: &str| s.parse::<TokenStream>().map_err(|err| format!("{}: {s}: {err}", f.list));
    let text = |t: &[TokenTree]| rust_text(&t.iter().cloned().collect::<TokenStream>());

    // the parameter the template pastes into the name: its argument, the label
    fn idents(ts: TokenStream, out: &mut Vec<String>) {
        for t in ts {
            match t {
                TokenTree::Ident(i) => out.push(ident(&i)),
                TokenTree::Group(g) => idents(g.stream(), out),
                _ => {}
            }
        }
    }
    let before_eq: TokenStream = lex(&f.template)?.into_iter()
        .take_while(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == '='))
        .collect();
    let mut name_idents = Vec::new();
    idents(before_eq, &mut name_idents);
    let label_param = name_idents.iter().find_map(|s| f.params.iter().position(|p| p == s));

    for entry in entries {
        let toks = from_list_entry(&f, entry)?;
        let (name, value) = match &toks[..] {
            [TokenTree::Ident(name)] => (ident(name), None),
            [TokenTree::Ident(name), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' && !v.is_empty() =>
                (ident(name), Some(format!("(({}) as {})", text(v), e.repr))),
            _ => return Err(format!("{}: the template gives {} - not NAME [= value]", f.list, text(&toks))),
        };
        let label = label_param.and_then(|n| split_commas(lex(entry).ok()?).into_iter().nth(n))
            .and_then(|a| match a.into_iter().collect::<Vec<_>>().as_slice() {
                [TokenTree::Ident(l)] => Some(ident(l)),
                _ => None,
            });
        variants.push(CEnumVariant { name, value, label, docs: Vec::new() });
    }
    let n = variants.len();
    e.variants.splice(at..at, variants);
    for (later, _) in &mut e.from {
        *later += n;
    }
    Ok(())
}

/// list_enum! @e as source, for its next list's callback: its variants, and
/// the lists not yet expanded in their place.
pub fn enum_text(e: &CEnumDef) -> String {
    let mut t = format!("#[{}] {}{}pub enum {}: {} {{ ", e.kind.attr(),
                        if e.own_to_text { "#[own_to_text] " } else { "" },
                        e.prefix.as_deref().map(|p| format!("#[prefix({p})] ")).unwrap_or_default()
                            + if e.nr { "#[nr] " } else { "" },
                        e.name.as_deref().unwrap_or("_"), e.repr);
    let lists = |t: &mut String, i: usize| for (at, f) in &e.from {
        if *at == i {
            t.push_str(&format!("{}({}) => {}, ", f.list, f.params.join(", "), f.template));
        }
    };
    for (i, v) in e.variants.iter().enumerate() {
        lists(&mut t, i);
        if let Some(l) = &v.label {
            t.push_str(&format!("#[label({l:?})] "));
        }
        match &v.value {
            Some(x) => t.push_str(&format!("{} = {x}, ", v.name)),
            None => t.push_str(&format!("{}, ", v.name)),
        }
    }
    lists(&mut t, e.variants.len());
    t + "}"
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
pub struct IntType {
    pub bytes:  Option<u8>,
    pub endian: &'static str,
    pub signed: bool,
}

/// The integer type @ty is, if it's one TypeInfo reads as an integer: Rust's
/// and C's integers, the endian ones - Le16..Le64, zerocopy's le:: and be:: -
/// and the unsigned atomics.
fn int_type(ty: &syn::Type) -> Option<IntType> {
    let syn::Type::Path(p) = ty else { return None };
    if p.qself.is_some() || p.path.segments.iter().any(|s| !s.arguments.is_none()) {
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
pub enum FieldType<'a> {
    /// `[elem; n]`
    Array { elem: &'a syn::Type, n: &'a syn::Expr },
    /// A pointer, reference, function or generic: nothing to describe
    Opaque,
    /// Anything else: the integer it is, if it's one, and the last segment
    /// of its path.
    Named { int: Option<IntType>, name: String },
}

/// What @ty is, for #[derive(TypeInfo)].
pub fn field_type(ty: &syn::Type) -> FieldType<'_> {
    match ty {
        syn::Type::Array(a) => FieldType::Array { elem: &a.elem, n: &a.len },
        t => match path_names(t) {
            Some(p) => FieldType::Named { int: int_type(t), name: p.last().cloned().unwrap_or_default() },
            None => FieldType::Opaque,
        },
    }
}

/// E, if @ty is `[E; 0]`: C's flexible array member.
pub fn flex_elem(ty: &syn::Type) -> Option<&syn::Type> {
    match ty {
        syn::Type::Array(syn::TypeArray {
            elem, len: syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. }), ..
        }) if n.base10_digits() == "0" => Some(elem),
        _ => None,
    }
}

/// @ty's T, if it's @name<T> by any path.
pub fn generic_arg<'a>(ty: &'a syn::Type, name: &str) -> Option<&'a syn::Type> {
    match generic_args(ty, name)?.as_slice() {
        [syn::GenericArgument::Type(t)] => Some(t),
        _ => None,
    }
}

// ---- entry points ----------------------------------------------------------

/// Parse a derive's input: exactly one struct or union.
pub fn parse_cstruct(item: &proc_macro2::TokenStream) -> Result<CStructDef, String> {
    let expected = "#[derive(CStruct)] is for structs and unions with named fields";
    let d = syn::parse2::<syn::DeriveInput>(item.clone()).map_err(|e| format!("{expected} - {e}"))?;
    parse_struct(d)?.ok_or_else(|| expected.to_string())
}

/// The item a macro's text is, as its macro: c_xmacro!, list_enum!, c_bitmask!
/// or c_ioctl!.
pub fn parse_items(src: &str) -> Result<Vec<CItem>, String> {
    let m = syn::parse_str::<syn::Macro>(src).map_err(|e| format!("{src}: {e}"))?;
    Ok(vec![match m.path.get_ident().map(ident).as_deref() {
        Some("c_xmacro")  => CItem::XMacro(parse_xmacro(&m)?),
        Some("list_enum") => CItem::Enum(parse_enum(&m)?),
        Some("c_bitmask") => CItem::Bitmask(parse_bitmask(&m)?),
        Some("c_ioctl")   => CItem::Ioctl(parse_ioctl(&m)?),
        _ => return Err(format!("{src}: not one of the macros")),
    }])
}
