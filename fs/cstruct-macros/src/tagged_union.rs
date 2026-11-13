// SPDX-License-Identifier: GPL-2.0
//! tagged_union!: a tagged union with a stable representation, declared as
//! an enum - its arms the variants, each with its payload's type and the
//! tag's value as the discriminant:
//!
//!   tagged_union! {
//!       #[derive(Clone, Copy, CStruct, TypeInfo)]*
//!       #[tag(type_: u8)]
//!       #[tag_enum(DiskAccountingType: u32, prefix = BCH_DISK_ACCOUNTING_, nr)]
//!       #[pad(Bpos)]
//!       #[size(pad)]
//!       #[packed]
//!       pub enum DiskAccountingPos {
//!           /* No subtypes - number of inodes in the entire filesystem */
//!           nr_inodes(#[repr(C)] pub struct NrInodes {}) = 0,
//!           replicas(Replicas) = 2,
//!           ...
//!       }
//!   }
//!
//!   tagged_union! {
//!       #[storage(c::jset_entry)]
//!       #[tag(type_: u8)]
//!       #[tag_enum(JsetEntryType: u32, prefix = BCH_JSET_ENTRY_, nr)]
//!       #[size(header)]
//!       pub enum jset_entry {
//!           /// Btree key updates
//!           btree_keys(c::jset_entry_btree_keys) = 0,
//!           ...
//!       }
//!   }
//!
//! A discriminant not written is the last's plus one, as an enum's. An arm's
//! type may be defined in its variant, `[#[..]] [pub] struct|union NAME {
//! .. }`: it goes to nestify::nest!, so it can nest types of its own, with
//! the declaration's `#[...]*` - nestify's propagating attributes, for each
//! arm type and what it nests, not for the storage. It's reported where it's
//! written: the definition keeps its tokens' spans.
//!
//! `#[tag_enum([closed,] NAME: REPR, prefix = P[, nr])]`:
//! the tag's enum, a #[bch_enum] of the arms (enum_decl.rs) - open, or closed and
//! the tag stored as it, `#[tag(FIELD)]`. NAME_arms! is the arms as a list,
//! (name, value, Type), for what's made from each.
//!
//! A type whose bytes come from outside - disk, or C - with a tag that says
//! which of its arms it holds, and that may be one this version doesn't know.
//! The layout is given, not chosen: the storage is the C, plain #[repr(C)]
//! data - without #[storage], a struct of the tag, then a union of the arms,
//! overlaid with the pad.
//!
//! `#[size(..)]` says what its size is a function of, and what's generated
//! follows from that: nothing, a constant - `size(pad)`, a struct, every
//! value the pad's size, a value of plain data; its header's count -
//! `size(header)`, a union over another's storage, below, a view of bytes as
//! long as the header says; or its tag - `size(arm)`, a value as long as the
//! arm its tag selects, below.
//!
//! `#[packed]` is C's __packed: for an on-disk struct, the payload right after
//! the tag, whatever the arms' alignment - a struct of the tag and the union
//! otherwise puts the union at its alignment, and a new arm could move every
//! payload.
//!
//! The tag is a function of the whole value: a crate::types::Determinant,
//! stabby's idea (docs.rs/stabby), n-way - generated, returning the tag
//! field, `#[tag(FIELD: TYPE)]`. It's handed out as its open #[tag_enum], a
//! newtype over an integer the tag widens to - or with none, as it is.
//!
//! Generated, for the first declaration:
//!
//!  - `union DiskAccountingPos__ffi`: the storage, what C and disk see -
//!    named DiskAccountingPos to TypeInfo; Plain, its pad being
//!    every byte of it;
//!  - `DiskAccountingPos<S>`, the storage in a state, Validated unless
//!    named, as a family's types are (util/vstructs.rs): Unvalidated, bytes
//!    from outside - `from_bytes()`; Validated, what validate passed, or
//!    what's made here from an arm. One generic newtype, not a definition
//!    per state: the states differ only in what get() does with an arm
//!    that doesn't fit.
//!  - `enum DiskAccountingPosArm`, an arm by value - the union's name in
//!    CamelCase, but that's the storage's here - and `enum
//!    DiskAccountingPosRef<'_, St>`, an arm by reference: one of the arm
//!    types the block declares, plain data, a reference to it; another's -
//!    the replicas entry, a header and the devices it counts - its
//!    #[counted] family's type, X<St>, in the storage's state, as long as
//!    its own header says (CountedHead, ArmAt);
//!  - `get()`: the arm the tag selects. Unvalidated, an error if the tag
//!    isn't one of ours or the arm's header says it's longer than the
//!    storage holds (ArmError); Validated, Err(the tag), and an arm that
//!    doesn't fit a bug. Shared references only: through a `&mut` to an
//!    arm, a tag in the arm's own bits could change under whoever holds it.
//!  - `validate()`, Unvalidated to Validated, if get() finds its arm fits;
//!  - `len()` on a Ref, the bytes its arm takes from `ARMS_AT`, where arms
//!    start - a tailed arm's, as many as its header says;
//!  - `new()`, and `from_arm()` by the arm's type, Validated: the payload into
//!    zeroed storage, then Determinant::set() marks it - stabby's order, so a
//!    tag that's bits of the payload is the last thing written, not
//!    overwritten by it. Never a typed copy, which leaves the bytes past a
//!    short payload undefined. `from_arm_tailed()` writes an arm's flexible
//!    array too.
//!  - crate::types::ArmOf for each arm's type, which from_arm() goes by;
//!  - `type_()`, the tag, known or not; `as_bytes()`, `as_ffi()`;
//!  - C: the same storage, its offsets asserted against the Rust's.
//!
//! A struct C hands over in memory - a request, not a record off disk - can
//! say so, with either or both of:
//!
//!  - a closed #[tag_enum]: the tag's a closed C enum, the storage holds it
//!    as one - C keeps its enum type, its switches their -Wswitch - and the
//!    arms' values are its variants, compared as themselves. A value from
//!    outside could hold one that isn't a variant, so there's no Unvalidated
//!    view, no from_bytes(); and the storage isn't Plain. new() starts from
//!    zeroes: a const asserts 0 is a variant.
//!  - `#[plain]`: every arm is plain data, so an arm type not defined in the
//!    block - a typedef of an integer, for C's `.device` where a struct
//!    would make it `.device.dev` - is one of its own, not a #[counted]
//!    family's head.
//!
//! The storage's bytes are always all written: they come from C or disk, from
//! from_bytes(), or from new(), which starts from zeroes - so get() and
//! as_bytes() can read any of them. A literal of the storage could leave
//! some unwritten, as could an arm with padding; nothing makes either.
//!
//! Or, `#[storage(STORAGE)] #[size(header)]`, the storage is another's: a
//! header its arms embed and run past, as far as its count says - a journal
//! entry's, a superblock field's: an unsized vstruct (util/vstructs.rs), each
//! reference to which has its arm's bytes, and its own C. Then what's
//! generated is the Ref enum, ArmOf, and on the storage in each state -
//! X<Unvalidated>, X<Validated> - typed(): the arm its tag selects, over its
//! bytes, in the storage's state - every arm type has a family
//! (util/vstructs.rs), so an Unvalidated storage's arms are Unvalidated. An
//! arm may be shorter than its type - written by an older version, before the
//! type grew - so it's what its definition's ArmRef says: a grown type's at
//! the arm's length, its later fields zero past the end. arm::<A>() is
//! typed() for one arm, if the tag is A's - A in the storage's state,
//! X<Unvalidated> for an Unvalidated one - and arm_mut::<A>() the same, to
//! edit. And for what's done to whichever arm it is - an ops trait's methods
//! - NAME_match!(r, a => ..) on what typed() gave, and NAME_by_tag!(tag, A =>
//! .., _ => ..) by the tag alone: `a: A`, or A, names the arm's type, A<S> in
//! state S.
//!
//! Or, `#[storage(HEAD)] #[size(arm)]`, each value is as long as the arm its
//! tag selects - an ACL's entries: no count to step by, so a tag this version
//! doesn't know is one there's no stepping past. HEAD is what every arm
//! starts with, the tag in it, and the arms are plain data, packed: an
//! on-disk type asks no alignment of where it is, so a value is read at any
//! offset. Arms may share a type, the variant saying which. What's generated
//! is the enum, the arm by value, and on it read(), from bytes, an error if
//! the tag isn't ours or the bytes are fewer than its arm
//! (crate::util::vstructs::ArmSized); bytes(), its arm's size; head(), the
//! fields every arm shares; name(), its arm's; write(), its arm's bytes then
//! its tag; walk(), over a run of them (ArmWalk). No C: HEAD and the arms
//! have their own.
//!
//! The declaration is parsed into a TaggedUnion, which generate() takes.

use proc_macro2::{Delimiter, Group, Ident, TokenStream, TokenTree};
use quote::{format_ident, quote};
use syn::parse::ParseStream;
use syn::{parse_quote, Token};

use crate::cstruct::{CEnumDef, CEnumKind};
use crate::enum_decl::{self, arm_path};

/// A tagged_union!'s declaration, as generate() takes it.
///
/// The storage is what C has: the tag, then a union of the arms, overlaid
/// with the pad - or another's, #[storage]. @size says what its size is a
/// function of: nothing, a constant, the pad's; the header's count - the
/// storage another's, a header its arms embed and run past, and what's
/// generated only the typed view of it; or the tag - each value as long as
/// the arm its tag selects, the head the start of every arm.
struct TaggedUnion {
    name:      Ident,
    /// #[storage(TYPE)]: the storage is that type, another's, and what's
    /// generated a view of it; else it's this one's own, which it defines.
    storage:   Option<syn::Type>,
    /// #[tag(FIELD..)]: the tag's field, and its accessor.
    tag_field: Ident,
    tag_ty:    syn::Type,
    /// An open #[tag_enum]: the tag as handed out, a newtype over an integer
    /// the tag widens to - or none, the tag as it is.
    tag_enum:  Option<syn::Type>,
    /// A closed #[tag_enum]: the tag is a C enum, stored as it, the arms'
    /// values its variants. Only in memory: no bytes from outside, which
    /// could hold a value that isn't one.
    tag_is_enum: bool,
    /// #[plain]: every arm is plain data - one not defined here a type of its
    /// own's, not a #[counted] family's head.
    plain:     bool,
    arms:      Vec<TaggedArm>,
    pad:       Option<syn::Type>,
    /// #[packed]: C's __packed - for a struct, the payload right after the
    /// tag whatever the arms' alignment, which is what the format says.
    packed:    bool,
    size:      Size,
    /// The arm types defined in the block, for nestify::nest!.
    defs:      Vec<Ident>,
}

/// One arm: its name, payload type, and tag value.
struct TaggedArm {
    name:  Ident,
    ty:    syn::Type,
    value: syn::Expr,
}

/// What its size is a function of, #[size(pad|header|arm)]: the rest
/// follows from it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Size {
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

impl TaggedUnion {
    fn check(&self) -> Result<(), String> {
        let name = &self.name;
        // a constant size is a struct's, its own storage, which holds its arms; a
        // header's count or the arm's own size, a union's over another's storage,
        // the head its arms start with
        let ok = match self.size {
            Size::Pad                => self.storage.is_none() && self.pad.is_some(),
            Size::Header | Size::Arm => self.storage.is_some() && self.pad.is_none(),
        };
        if !ok {
            return Err(format!("tagged_union! {name}: #[pad(TYPE)] #[size(pad)] - its own storage, \
                                which holds its arms, the pad's size - or #[storage(STORAGE)] \
                                #[size(header)] - over another's storage, whose arms run past its \
                                header as far as its count says - or #[storage(HEAD)] #[size(arm)] - \
                                the head every arm starts with, each as long as its arm"));
        }
        if (self.plain || self.tag_is_enum) && self.size != Size::Pad {
            return Err(format!("tagged_union! {name}: #[plain] and a closed #[tag_enum] are for a \
                                struct's own storage, #[size(pad)]"));
        }
        if let (Some(_), Some(d)) = (&self.storage, self.defs.first()) {
            return Err(format!("tagged_union! {name}: {d} is defined here, but the storage is \
                                another's - #[storage(..)] - and so are its arms' types"));
        }
        Ok(())
    }
}

pub(crate) fn tagged_union(input: TokenStream) -> Result<TokenStream, String> {
    let (decl, tu, mut out) = enum_form(input)?;
    out.extend(generate(&decl, &tu)?);
    Ok(out)
}

/// The declaration's parts generate() passes on - the arm types as their
/// tokens, so an error in one is reported where it's written.
struct Decl {
    attrs:     Vec<syn::Attribute>,
    vis:       syn::Visibility,
    arm_attrs: TokenStream,
    defs:      Vec<TokenStream>,
}

/// The attributes tagged_union! reads. The rest are the storage's - or with
/// a `*` after, each arm type's the block defines.
const OURS: &[&str] = &["tag", "tag_enum", "storage", "pad", "size", "packed", "plain"];

/// The union the declaration says, and what's generated from its arms besides
/// it: the tag enum and NAME_arms!.
fn enum_form(input: TokenStream) -> Result<(Decl, TaggedUnion, TokenStream), String> {
    let (attrs, arm_attrs, ours, vis, name, body) = syn::parse::Parser::parse2(|input: ParseStream<'_>| {
        let (mut attrs, mut arm_attrs, mut ours) = (Vec::new(), TokenStream::new(), Vec::new());
        while input.peek(Token![#]) {
            let content;
            let attr = syn::Attribute {
                pound_token:   input.parse()?,
                style:         syn::AttrStyle::Outer,
                bracket_token: syn::bracketed!(content in input),
                meta:          content.parse()?,
            };
            if input.peek(Token![*]) {
                let star: Token![*] = input.parse()?;
                arm_attrs.extend(quote!(#attr #star));
            } else if OURS.iter().any(|o| attr.path().is_ident(o)) {
                ours.push(attr);
            } else {
                attrs.push(attr);
            }
        }
        let vis: syn::Visibility = input.parse()?;
        input.parse::<Token![enum]>()?;
        let name: Ident = input.parse()?;
        let body;
        syn::braced!(body in input);
        Ok((attrs, arm_attrs, ours, vis, name, body.parse::<TokenStream>()?))
    }, input).map_err(|e| format!("expected [#[..]] pub enum NAME {{ .. }}: {e}"))?;
    let err = |m: &str| format!("tagged_union! {name}: {m}");

    let mut tag: Option<(Ident, Option<syn::Type>)> = None;
    let mut tag_enum: Option<CEnumDef> = None;
    let (mut storage, mut pad, mut size, mut packed, mut plain) = (None, None, None, false, false);
    for a in &ours {
        let kind = a.path().get_ident().map(ToString::to_string).unwrap_or_default();
        let bad = |e: syn::Error| err(&format!("#[{kind}(..)]: {e}"));
        match kind.as_str() {
            "tag" => tag = Some(a.parse_args_with(|i: ParseStream<'_>| {
                let field: Ident = i.parse()?;
                let ty = match i.is_empty() {
                    true => None,
                    false => {
                        i.parse::<Token![:]>()?;
                        Some(i.parse()?)
                    }
                };
                Ok((field, ty))
            }).map_err(|_| err("#[tag(FIELD: TYPE)], or #[tag(FIELD)] for a closed tag enum"))?),
            "tag_enum" => tag_enum = Some(enum_decl::tag_enum(a.parse_args().map_err(bad)?)
                .map_err(|m| err(&format!("#[tag_enum(..)]: {m}")))?),
            "storage" => storage = Some(a.parse_args().map_err(bad)?),
            "pad"     => pad = Some(a.parse_args().map_err(bad)?),
            "size"    => size = Some(match a.parse_args::<Ident>().map_err(bad)?.to_string().as_str() {
                "pad"    => Size::Pad,
                "header" => Size::Header,
                "arm"    => Size::Arm,
                of => return Err(err(&format!("#[size({of})] - its size is a function of nothing, \
                                               its pad's, size(pad); of its header's count, \
                                               size(header); or of its tag, its arm's, size(arm)"))),
            }),
            "packed"  => packed = true,
            "plain"   => plain = true,
            _ => unreachable!(),
        }
    }
    let (tag_field, tag_ty) = tag.ok_or_else(|| err("no #[tag(..)]"))?;

    let (body, defs) = arm_defs(body);
    let arms = enum_decl::arms(body, &Vec::new(), true).map_err(|m| err(&m))?;
    let mut out = TokenStream::new();
    if let Some(e) = &tag_enum {
        out.extend(enum_decl::generate(e, &arms, &Vec::new()));
    }
    out.extend(enum_decl::entries(&name, &arms));

    let closed = |e: &CEnumDef| e.kind == CEnumKind::Closed;
    let c_path = |e: &CEnumDef| -> syn::Type {
        let n = &e.name;
        parse_quote!(c::#n)
    };
    let (tag_ty, tag_enum_ty, tag_is_enum) = match (&tag_enum, tag_ty) {
        (Some(e), None) if closed(e) => (c_path(e), None, true),
        (Some(e), Some(ty)) if !closed(e) => (ty, Some(c_path(e)), false),
        (None, Some(ty)) => (ty, None, false),
        _ => return Err(err("#[tag(FIELD: TYPE)] with an open #[tag_enum], #[tag(FIELD)] with a closed one")),
    };

    let arm_value = |a: &enum_decl::Arm| -> syn::Expr {
        match &tag_enum {
            Some(e) if closed(e) => {
                let (e, arm) = (&e.name, &a.name);
                parse_quote!(c::#e::#arm)
            }
            _ => a.value.clone(),
        }
    };
    let tu = TaggedUnion {
        name: name.clone(),
        storage,
        tag_field,
        tag_ty,
        tag_enum: tag_enum_ty,
        tag_is_enum,
        plain,
        arms: arms.iter().map(|a| TaggedArm {
            name: a.name.clone(), ty: a.ty.clone().expect("typed arms"), value: arm_value(a),
        }).collect(),
        pad,
        packed,
        size: size.ok_or_else(|| err("no #[size(..)] - what it's a function of, pad, header or arm"))?,
        defs: defs.iter().map(|(name, _)| name.clone()).collect(),
    };
    tu.check()?;
    let defs = defs.into_iter().map(|(_, def)| def).collect();
    Ok((Decl { attrs, vis, arm_attrs, defs }, tu, out))
}

/// The arm types defined in their arms, `name(pub struct T { .. })`: @body
/// with `name(T)` for each, and each one's name and definition.
fn arm_defs(body: TokenStream) -> (TokenStream, Vec<(Ident, TokenStream)>) {
    let mut defs = Vec::new();
    let body = body.into_iter().map(|t| {
        let TokenTree::Group(g) = &t else { return t };
        let p: Vec<TokenTree> = g.stream().into_iter().collect();
        // past its attributes and visibility, `struct|union NAME { .. }`
        let mut d = 0;
        while matches!(p.get(d), Some(TokenTree::Punct(h)) if h.as_char() == '#') {
            d += 2;
        }
        if matches!(p.get(d), Some(TokenTree::Ident(i)) if i == "pub") {
            d += 1 + usize::from(matches!(p.get(d + 1), Some(TokenTree::Group(v)) if v.delimiter() == Delimiter::Parenthesis));
        }
        match (g.delimiter(), p.get(d), p.get(d + 1), p.get(d + 2)) {
            (Delimiter::Parenthesis, Some(TokenTree::Ident(kw)), Some(TokenTree::Ident(n)), Some(TokenTree::Group(b)))
                if (kw == "struct" || kw == "union") && b.delimiter() == Delimiter::Brace => {
                defs.push((n.clone(), g.stream()));
                TokenTree::Group(Group::new(Delimiter::Parenthesis, TokenTree::Ident(n.clone()).into()))
            }
            _ => t,
        }
    }).collect();
    (body, defs)
}

/// foo_bar -> FooBar.
fn camel(name: &str) -> String {
    name.split('_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut c = s.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect()
}

/// Path type @ty, its last segment's name suffixed: a state's definition,
/// X__validated.
fn suffixed(ty: &syn::Type, suffix: &str) -> syn::Type {
    let mut ty = ty.clone();
    if let syn::Type::Path(p) = &mut ty {
        let last = p.path.segments.last_mut().expect("a path has a segment");
        last.ident = format_ident!("{}{suffix}", last.ident);
    }
    ty
}

fn generate(d: &Decl, tu: &TaggedUnion) -> Result<TokenStream, String> {
    let arms = &tu.arms;
    let name = &tu.name;
    if arms.is_empty() {
        return Err(format!("{name}: no arms"));
    }
    // whether @ty is @def, a type the block defines
    let is = |ty: &syn::Type, def: &Ident| matches!(ty, syn::Type::Path(p) if p.qself.is_none() && p.path.is_ident(def));
    if let Some(def) = tu.defs.iter().find(|def| !arms.iter().any(|a| is(&a.ty, def))) {
        return Err(format!("{name}: {def} is defined here but isn't an arm's type"));
    }
    // The arm types defined in the block: nestify::nest!, given the
    // declaration's `#[...]*` - propagating, for any types nested in them -
    // as the tokens they were written as.
    let arm_attrs = &d.arm_attrs;
    let defs: TokenStream = d.defs.iter().map(|def| quote!(::nestify::nest! { #arm_attrs #def })).collect();
    let (attrs, vis) = (&d.attrs, &d.vis);
    let en = format_ident!("{}", camel(&name.to_string()));
    // the arms by value: named for the union, but where that's the storage's
    // own name - DiskAccountingPos, C's disk_accounting_pos - its Arm
    let ev = if en == *name { format_ident!("{en}Arm") } else { en.clone() };
    let en_ref = format_ident!("{en}Ref");
    let (tag, tag_ty) = (&tu.tag_field, &tu.tag_ty);
    // The tag as handed out: its enum, a newtype over an integer the tag
    // widens to - or with none, the tag as it is
    let tag_enum = tu.tag_enum.as_ref().unwrap_or(tag_ty);
    let wrap_tag = |x: TokenStream| match &tu.tag_enum {
        Some(e) => quote!(#e(#x as _)),
        None => x,
    };
    let t_tag = wrap_tag(quote!(t));
    // A value as the tag: an enum tag's values are its variants, as they are
    let as_tag = |v: &syn::Expr| if tu.tag_is_enum { quote!((#v)) } else { quote!((#v) as #tag_ty) };
    let det_trait = quote!(crate::types::Determinant);
    let arm_of = quote!(crate::types::ArmOf);
    let vs = quote!(crate::util::vstructs);
    // The storage: the type the views are of
    // A sized storage's is C's struct, {name}__ffi: {name} is its state's
    let ffi = format_ident!("{name}__ffi");
    let s_ty: syn::Type = tu.storage.clone().unwrap_or_else(|| parse_quote!(#ffi));
    let s_size = quote!(::core::mem::size_of::<#s_ty>());

    // The storage, as C has it: the tag then the arms' union (with types of
    // their own, C's anonymous struct and union), overlaid with the pad. Or
    // none, the storage being another's - an unsized union's.
    let arm_fields = arms.iter().map(|a| {
        let (arm, ty) = (&a.name, &a.ty);
        quote!(#vis #arm: #ty,)
    });
    let pad_field = tu.pad.as_ref().map(|p| quote!(#vis _pad: #p,));
    let storage_attrs = quote!(#[allow(non_camel_case_types)] #[repr(C)] #[derive(Clone, Copy)]);
    // `packed`: where C has __packed - the arms' union and the tag's struct
    let packed_attrs = if tu.packed {
        quote!(#[allow(non_camel_case_types)] #[repr(C, packed)] #[derive(Clone, Copy)])
    } else {
        storage_attrs.clone()
    };
    // An unsized vstruct's storage is a family: each state its own
    // definition, named - what its tag is read from. Each arm is a family's
    // too, named in the storage's state.
    if tu.size == Size::Header {
        let not_path = std::iter::once(&s_ty).chain(arms.iter().map(|a| &a.ty))
            .find(|t| !matches!(t, syn::Type::Path(_)));
        if let Some(t) = not_path {
            return Err(format!("{name}: {}: #[size(header)]'s storage and arms are each a family's \
                                type, named", quote!(#t)));
        }
    }
    let states: Vec<(&str, syn::Type)> = if tu.size == Size::Header {
        ["Unvalidated", "Validated"].iter()
            .map(|st| (*st, suffixed(&s_ty, &format!("__{}", st.to_lowercase()))))
            .collect()
    } else {
        vec![("", s_ty.clone())]
    };
    let determinant = format_ident!("{name}__tag");
    let (storage, payload_offset) = if tu.storage.is_some() {
        // another's storage, a union: its tag is its own field, by value, as
        // a packed struct's must be
        let doc = format!(" {name}'s tag: the storage's {tag}.");
        let impls = states.iter().map(|(_, s)| quote! {
            impl #det_trait<#s> for #determinant {
                type Tag = #tag_ty;
                fn get(u: &#s) -> #tag_ty {
                    <#tag_ty>::from({ u.#tag })
                }
                fn set(u: &mut #s, tag: #tag_ty) {
                    u.#tag = tag.into();
                }
            }
        });
        (quote! {
            #[doc = #doc]
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            #vis struct #determinant;

            #(#impls)*
        }, quote!(0))
    } else {
        let (arms_union, tagged) = (format_ident!("{name}__arms"), format_ident!("{name}__tagged"));
        let doc = format!(" {name}'s tag: its field.");
        (quote! {
            #[doc(hidden)]
            #packed_attrs
            #vis union #arms_union {
                #(#arm_fields)*
            }

            #[doc(hidden)]
            #packed_attrs
            #vis struct #tagged {
                #vis #tag: #tag_ty,
                #vis u: #arms_union,
            }

            #[::cstruct_macros::type_name(#name)]
            #(#attrs)*
            #storage_attrs
            #vis union #ffi {
                #vis t: #tagged,
                #pad_field
            }

            #[doc = #doc]
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            #vis struct #determinant;

            impl #det_trait<#ffi> for #determinant {
                type Tag = #tag_ty;
                fn get(u: &#ffi) -> #tag_ty {
                    // SAFETY: the tag is plain data, the struct's first field
                    unsafe { u.t.#tag }
                }
                fn set(u: &mut #ffi, tag: #tag_ty) {
                    #[allow(unused_unsafe)]
                    // SAFETY: a write of plain data
                    unsafe { u.t.#tag = tag }
                }
            }
        }, quote!(::core::mem::offset_of!(#tagged, u)))
    };
    let det = quote!(<#determinant as #det_trait<#s_ty>>);

    let by_value = arms.iter().map(|a| {
        let (arm, ty) = (&a.name, &a.ty);
        quote!(#arm(#ty),)
    });
    // An arm type this declares is plain data: a reference to it, whatever
    // the storage's state - and with `plain`, every arm's. Another's is a
    // #[counted] family's head - a replicas entry's, as C's union holds it -
    // the family's type in the storage's state, as long as its own header
    // says (CountedHead, ArmAt).
    let declared = |a: &TaggedArm| tu.plain || tu.defs.iter().any(|def| is(&a.ty, def));
    let stated = arms.iter().any(|a| !declared(a));
    let st_param = stated.then(|| quote!(, St: #vs::State = #vs::Validated));
    let st_arg = |st: &str| {
        let st = format_ident!("{st}");
        stated.then(|| quote!(, #vs::#st))
    };
    let by_ref = arms.iter().map(|a| {
        let (arm, ty) = (&a.name, &a.ty);
        if declared(a) {
            quote!(#arm(&'a #ty),)
        } else {
            quote!(#arm(&'a <#ty as #vs::CountedHead>::Of<St>),)
        }
    });
    let lens = arms.iter().map(|a| {
        let arm = &a.name;
        quote!(#en_ref::#arm(a) => ::core::mem::size_of_val(a),)
    });
    let (st_impl, st_use) = if stated { (quote!(, St: #vs::State), quote!(, St)) } else { (quote!(), quote!()) };
    // get()'s arms, in state @st: @bad, what an arm whose header says it's
    // longer than the storage holds is
    let gets = |st: &str, bad: TokenStream| -> TokenStream {
        let st = format_ident!("{st}");
        arms.iter()
            .map(|a| {
                let (arm, ty, v) = (&a.name, &a.ty, as_tag(&a.value));
                let got = if declared(a) {
                    quote! {
                        // SAFETY: an arm this declares is plain data, in the storage
                        // and aligned there: asserted below
                        unsafe { &*b.as_ptr().cast::<#ty>() }
                    }
                } else {
                    quote! {
                        match <<#ty as #vs::CountedHead>::Of<#vs::#st> as #vs::ArmAt>::arm_at(b) {
                            Ok(arm) => arm,
                            Err(err) => #bad,
                        }
                    }
                };
                quote! {
                    if t == #v {
                        return Ok(#en_ref::#arm(#got));
                    }
                }
            })
            .collect()
    };
    let news = arms.iter().map(|a| {
        let arm = &a.name;
        quote!(#ev::#arm(x) => Self::from_arm(x),)
    });
    // An arm that's the storage itself - an ACL's short entry - isn't a type
    // of its own to go by
    let arms_of = arms.iter()
        .filter(|a| a.ty != s_ty)
        .map(|a| {
            let (ty, v) = (&a.ty, as_tag(&a.value));
            quote! {
                impl #arm_of<#s_ty> for #ty {
                    type Tag = #tag_ty;
                    const TAG: #tag_ty = #v;
                }
            }
        });
    // Where get() finds an arm: in the storage, aligned for it
    let arm_asserts = arms.iter().map(|a| {
        let (arm, ty) = (&a.name, &a.ty);
        let fits = format!("tagged_union! {name}: arm {arm} doesn't fit in the storage");
        let aligned = format!("tagged_union! {name}: arm {arm} isn't aligned in the storage");
        quote! {
            const _: () = assert!(#payload_offset + ::core::mem::size_of::<#ty>() <= #s_size, #fits);
            const _: () = assert!((#payload_offset) % ::core::mem::align_of::<#ty>() == 0 &&
                                  ::core::mem::align_of::<#s_ty>() >= ::core::mem::align_of::<#ty>(),
                                  #aligned);
        }
    });
    // packed: an arm aligned more than 1 would be an unaligned reference -
    // say so by name, not as rustc's E0793 at the list
    let packed_asserts = arms.iter()
        .filter(|_| tu.packed)
        .map(|a| {
            let (arm, ty) = (&a.name, &a.ty);
            let msg = format!("tagged_union! {name}: arm {arm} is aligned more than 1, so in a packed \
                               union get() can't hand out a reference to it - pack the arm");
            quote!(const _: () = assert!(::core::mem::align_of::<#ty>() == 1, #msg);)
        });
    // Plain - C's struct of a family is: its pad is every byte of it, and
    // Plain, and the arms are plain data. Not with an enum tag, which isn't
    // any bytes: but new() starts from zeroes, so zero must be one of its
    // variants - a const that isn't fails to evaluate.
    let pad_assert = tu.pad.as_ref().map(|p| {
        let plain = if tu.tag_is_enum {
            quote! {
                // SAFETY: 0 is one of its variants - or this, a const, fails to evaluate
                const _: #tag_ty = unsafe { ::core::mem::zeroed() };
            }
        } else {
            quote! {
                // SAFETY: its pad is every byte of it - asserted above - and Plain,
                // and its arms are plain data, any bytes one
                unsafe impl #vs::Plain for #s_ty {}
            }
        };
        let msg = format!("tagged_union! {name}: bigger than its pad");
        quote! {
            const _: () = assert!(#s_size == ::core::mem::size_of::<#p>(), #msg);
            #plain
            const _: () = #vs::assert_plain::<#p>();
        }
    });

    if tu.size == Size::Arm {
        // Each value as long as the arm its tag selects - the variant, by
        // value - read off bytes and written back to them: ArmSized, and
        // ArmWalk over a run of them. The tag is the head's, so every arm
        // starts with the head, its tag where the head has it; and every arm
        // is plain data, read and written by copying, at any alignment.
        // No ArmOf: arms may share a type, the variant saying which.
        let head = &s_ty;
        let tag_of_raw = wrap_tag(quote!(self.tag_raw()));
        let reads = arms.iter().map(|a| {
            let (arm, ty, value) = (&a.name, &a.ty, &a.value);
            quote! {
                if t == (#value) as #tag_ty {
                    return #vs::cast_plain::<#ty>(b).copied().map(Self::#arm)
                        .ok_or(#vs::ArmError::Bad {
                            tag: #t_tag,
                            err: #vs::VStructError::Overrun {
                                ty:   ::core::any::type_name::<#ty>(),
                                want: ::core::mem::size_of::<#ty>(),
                                have: b.len(),
                            },
                        });
                }
            }
        });
        let tags = arms.iter().map(|a| {
            let (arm, value) = (&a.name, &a.value);
            quote!(Self::#arm(_) => (#value) as #tag_ty,)
        });
        let sizes = arms.iter().map(|a| {
            let (arm, ty) = (&a.name, &a.ty);
            quote!(Self::#arm(_) => ::core::mem::size_of::<#ty>(),)
        });
        let arm_bytes = arms.iter().map(|a| {
            let arm = &a.name;
            quote!(Self::#arm(x) => #vs::plain_bytes(x),)
        });
        let names = arms.iter().map(|a| {
            let (arm, s) = (&a.name, a.name.to_string());
            quote!(Self::#arm(_) => #s,)
        });
        let asserts = arms.iter().map(|a| {
            let (arm, ty) = (&a.name, &a.ty);
            let starts = format!("tagged_union! {name}: arm {arm} doesn't start with the head - its {tag} \
                                  where the head has it");
            let aligned = format!("tagged_union! {name}: arm {arm} has an alignment - it's read where it \
                                   is, at any offset: #[repr(C, packed)]");
            quote! {
                const _: () = #vs::assert_plain::<#ty>();
                const _: () = assert!(::core::mem::size_of::<#ty>() >= ::core::mem::size_of::<#head>() &&
                                      ::core::mem::offset_of!(#ty, #tag) == ::core::mem::offset_of!(#head, #tag),
                                      #starts);
                const _: () = assert!(::core::mem::align_of::<#ty>() == 1, #aligned);
            }
        });
        let head_aligned = format!("tagged_union! {name}: its head has an alignment - it's read where it is, \
                                    at any offset: #[repr(C, packed)]");
        let doc = format!(" One of {name}'s arms, by value, as long as it is - the variant is\n \
                           the tag: what read() gives, and write() writes, its arm's own\n \
                           {tag} with it.");
        return Ok(quote! {
            #defs
            #storage
            const _: () = #vs::assert_plain::<#head>();
            const _: () = assert!(::core::mem::align_of::<#head>() == 1, #head_aligned);
            #(#asserts)*

            #[doc = #doc]
            #[allow(non_camel_case_types)]
            #[derive(Clone, Copy)]
            #vis enum #en {
                #(#by_value)*
            }

            impl #en {
                fn tag_raw(&self) -> #tag_ty {
                    match self {
                        #(#tags)*
                    }
                }

                fn arm_bytes(&self) -> &[u8] {
                    match self {
                        #(#arm_bytes)*
                    }
                }

                /// Its tag: its variant's.
                #vis fn #tag(&self) -> #tag_enum {
                    #tag_of_raw
                }

                /// Its head, which every arm starts with: the fields they share.
                #vis fn head(&self) -> #head {
                    *#vs::cast_plain(self.arm_bytes()).expect("an arm starts with the head")
                }

                /// Its arm's name.
                #vis fn name(&self) -> &'static str {
                    match self {
                        #(#names)*
                    }
                }

                /// Write it at the start of @out - its arm's bytes, then its tag,
                /// last, as a sized union's new() marks it - and give back the rest
                /// of @out. Panics if @out is shorter than it: bytes() sizes @out.
                #vis fn write<'o>(&self, out: &'o mut [u8]) -> &'o mut [u8] {
                    let b = self.arm_bytes();
                    let (to, rest) = out.split_at_mut(b.len());
                    to.copy_from_slice(b);
                    #det::set(#vs::cast_plain_mut(to).expect("an arm starts with the head"),
                              self.tag_raw());
                    rest
                }

                /// The values @b holds, one after another: ArmWalk.
                #vis fn walk(b: &[u8]) -> #vs::ArmWalk<'_, Self> {
                    #vs::ArmWalk::new(b)
                }
            }

            impl #vs::ArmSized for #en {
                type Tag = #tag_enum;

                fn read(b: &[u8]) -> Result<Self, #vs::ArmError<#tag_enum>> {
                    let Some(h) = #vs::cast_plain::<#head>(b) else {
                        return Err(#vs::ArmError::Short(#vs::VStructError::Overrun {
                            ty:   ::core::any::type_name::<#head>(),
                            want: ::core::mem::size_of::<#head>(),
                            have: b.len(),
                        }));
                    };
                    let t: #tag_ty = #det::get(h);
                    #(#reads)*
                    Err(#vs::ArmError::Unknown(#t_tag))
                }

                fn bytes(&self) -> usize {
                    match self {
                        #(#sizes)*
                    }
                }
            }
        });
    }

    if tu.size == Size::Header {
        // An unsized vstruct, its arms running past its header as far as its
        // count says - each reference to one has them: typed() and arm(), on
        // each state's definition of the storage. An arm there may be an
        // older, shorter version of its type: as its ArmRef has it, over the
        // storage's bytes.
        //
        // Every arm has a family - #[unsized_vstruct], #[grown], #[whole] -
        // so each is named in the storage's state: a member of an Unvalidated
        // one is Unvalidated, all the way down.
        let by_ref = arms.iter().map(|a| {
            let (arm, ty) = (&a.name, &a.ty);
            quote!(#arm(&'a #ty<St>),)
        });
        // ArmOf by state - the storage's, X<St>, for the arm's definition in
        // that state, which the arm's X<St> has it by: an arm of an
        // Unvalidated storage is Unvalidated
        let mut arms_of_by_state = TokenStream::new();
        for (st, _) in &states {
            let (lower, st) = (format!("__{}", st.to_lowercase()), format_ident!("{st}"));
            for a in arms {
                let (arm_def, v) = (suffixed(&a.ty, &lower), &a.value);
                arms_of_by_state.extend(quote! {
                    impl #arm_of<#s_ty<#vs::#st>> for #arm_def {
                        type Tag = #tag_ty;
                        const TAG: #tag_ty = (#v) as #tag_ty;
                    }
                });
            }
        }
        let impls = states.iter().map(|(st, s)| {
            let st = format_ident!("{st}");
            let det = quote!(<#determinant as #det_trait<#s>>);
            let tag_of_self = wrap_tag(quote!(#det::get(&self.repr)));
            let selects = arms.iter().map(|a| {
                let (arm, ty, v) = (&a.name, &a.ty, &a.value);
                quote! {
                    if t == (#v) as #tag_ty {
                        return <#ty<#vs::#st> as #vs::ArmRef>::arm_ref(self.as_bytes())
                            .map(#en_ref::#arm).ok_or(#t_tag);
                    }
                }
            });
            quote! {
                impl #s_ty<#vs::#st> {
                    /// The tag, whether this version knows it or not.
                    #vis fn #tag(&self) -> #tag_enum {
                        #tag_of_self
                    }

                    /// The arm its tag selects, over its bytes - as many as its count
                    /// says, as its ArmRef has it: Err(the tag) if it isn't one of
                    /// ours, or the bytes are fewer than any valid one's. One shorter
                    /// than its type is an older version, its missing fields zero.
                    #vis fn typed(&self) -> Result<#en_ref<'_, #vs::#st>, #tag_enum> {
                        let t: #tag_ty = #det::get(&self.repr);
                        #(#selects)*
                        Err(#t_tag)
                    }

                    /// The arm, if the tag is @A's: typed(), for one arm.
                    #vis fn arm<A>(&self) -> Option<&A>
                    where
                        A: #arm_of<Self, Tag = #tag_ty> + #vs::ArmRef + ?Sized,
                    {
                        if #det::get(&self.repr) != A::TAG {
                            return None;
                        }
                        A::arm_ref(self.as_bytes())
                    }

                    /// arm(), to edit: as its ArmRef has it.
                    #vis fn arm_mut<A>(&mut self) -> Option<&mut A>
                    where
                        A: #arm_of<Self, Tag = #tag_ty> + #vs::ArmRef + ?Sized,
                    {
                        if #det::get(&self.repr) != A::TAG {
                            return None;
                        }
                        A::arm_mut(self.as_bytes_mut())
                    }
                }
            }
        });
        let match_arms = arms.iter().map(|a| {
            let (arm, path) = (&a.name, arm_path(&a.ty));
            quote!(crate::c::#en_ref::#arm($a) => { $(type $A<S> = #path<S>;)? $body })
        });
        let tag_arms = arms.iter().map(|a| {
            let (v, path) = (&a.value, arm_path(&a.ty));
            quote!(if t == (#v) as #tag_ty { type $A<S> = #path<S>; $body } else)
        });
        let (match_mac, by_tag_mac) = (format_ident!("{name}_match"), format_ident!("{name}_by_tag"));
        let doc = format!(" One of {name}'s arms, with the bytes its header says are its - as\n \
                           many as there are, for one written before its type grew: what\n \
                           {name}'s typed() gives.");
        return Ok(quote! {
            #defs
            #storage

            #[allow(unused_macros)]
            macro_rules! #match_mac {
                ($r:expr, $a:ident $(: $A:ident)? => $body:expr) => {
                    match $r {
                        #(#match_arms)*
                    }
                };
            }
            #[allow(unused_imports)]
            pub(crate) use #match_mac;

            #[allow(unused_macros)]
            macro_rules! #by_tag_mac {
                ($t:expr, $A:ident => $body:expr, _ => $other:expr) => {{
                    let t: #tag_ty = $t;
                    #(#tag_arms)* { $other }
                }};
            }
            #[allow(unused_imports)]
            pub(crate) use #by_tag_mac;

            #[doc = #doc]
            #[allow(non_camel_case_types)]
            #[derive(Clone, Copy)]
            #vis enum #en_ref<'a, St: #vs::State = #vs::Validated> {
                #(#by_ref)*
            }

            #(#impls)*
            #arms_of_by_state
        });
    }

    let tag_of_storage = wrap_tag(quote!(#det::get(&self.0)));
    let ref_doc = if stated {
        format!(" One of {name}'s arms, by reference, in its state: what {name}::get()\n \
                 gives - an arm that's another's type as long as its own header says.")
    } else {
        format!(" One of {name}'s arms, by reference: what {name}::get() gives.")
    };
    // Bytes from outside: not with an enum tag, which they could hold a value
    // of that isn't one of its variants
    let unvalidated_impl = (!tu.tag_is_enum).then(|| {
        let unvalidated = st_arg("Unvalidated");
        let gets = gets("Unvalidated", quote!(return Err(#vs::ArmError::Bad { tag: #t_tag, err })));
        quote! {
            impl #name<#vs::Unvalidated> {
                /// From its bytes, as C and disk have them.
                #vis fn from_bytes(b: &[u8; #s_size]) -> Self {
                    // SAFETY: the storage is plain data: any bytes are a valid one
                    Self(unsafe { b.as_ptr().cast::<#s_ty>().read_unaligned() }, ::core::marker::PhantomData)
                }

                /// The arm the tag selects: Err if the tag isn't one of ours, or the
                /// arm's header says it's longer than the storage holds.
                #vis fn get(&self) -> Result<#en_ref<'_ #unvalidated>, #vs::ArmError<#tag_enum>> {
                    let t: #tag_ty = #det::get(&self.0);
                    let b = &self.as_bytes()[#payload_offset..];
                    #gets
                    Err(#vs::ArmError::Unknown(#t_tag))
                }

                /// Validated, if its arm fits in the storage: get()'s check, kept. A
                /// tag this version doesn't know passes - a Validated get() says so.
                #vis fn validate(self) -> Result<#name, #vs::ArmError<#tag_enum>> {
                    self.validated().copied()
                }

                /// validate(), by reference: the same bytes, Validated.
                #vis fn validated(&self) -> Result<&#name, #vs::ArmError<#tag_enum>> {
                    match self.get() {
                        Err(e @ (#vs::ArmError::Bad { .. } | #vs::ArmError::Short(_))) => Err(e),
                        _ => Ok(unsafe { &*::core::ptr::from_ref(self).cast::<#name>() }),
                    }
                }
            }
        }
    });
    let validated = st_arg("Validated");
    let doesnt_fit = format!("{name}<Validated>: {{err}} - an arm that doesn't fit, in one validate passed \
                              or made from an arm");
    let gets_validated = gets("Validated", quote!(panic!(#doesnt_fit)));
    let state_doc = format!(" {name}, in state S - Validated unless named: over C's struct, {ffi}.\n \
                             Unvalidated, bytes from outside - from the kernel, off a device - each\n \
                             arm checked against the storage as get() hands it out; Validated,\n \
                             what validate passed, or what's made here from an arm.");
    let value_doc = format!(" One of {name}'s arms, by value: what {name}::new() takes.");
    let len_doc = format!(" The bytes it takes, from {name}::ARMS_AT - a tailed arm's, as\n \
                           many as its header says.");
    Ok(quote! {
        #defs
        #storage

        #[doc = #state_doc]
        #[allow(non_camel_case_types)]
        #[repr(transparent)]
        #vis struct #name<S = #vs::Validated>(#s_ty, ::core::marker::PhantomData<S>);

        impl<S> Clone for #name<S> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<S> Copy for #name<S> {}

        #[doc = #value_doc]
        #[allow(non_camel_case_types)]
        #vis enum #ev {
            #(#by_value)*
        }

        #[doc = #ref_doc]
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy)]
        #vis enum #en_ref<'a #st_param> {
            #(#by_ref)*
        }

        impl<'a #st_impl> #en_ref<'a #st_use> {
            #[doc = #len_doc]
            #vis fn len(&self) -> usize {
                match *self {
                    #(#lens)*
                }
            }
        }

        impl<S> #name<S> {
            /// The tag, whether this version knows it or not.
            #vis fn #tag(&self) -> #tag_enum {
                #tag_of_storage
            }

            /// Its bytes, as C and disk have them.
            #vis fn as_bytes(&self) -> &[u8; #s_size] {
                // SAFETY: plain data, every byte of it written: it came from C or disk,
                // from_bytes() or new(), which starts from zeroes
                unsafe { &*(self as *const Self).cast::<[u8; #s_size]>() }
            }

            /// For C.
            #vis fn as_ffi(&self) -> *const #s_ty {
                &self.0
            }

            /// For C, to edit.
            #vis fn as_ffi_mut(&mut self) -> *mut #s_ty {
                &mut self.0
            }
        }

        #unvalidated_impl

        impl #name<#vs::Validated> {
            /// Where its arms start, in any state: after the tag.
            #vis const ARMS_AT: usize = #payload_offset;

            /// The arm the tag selects - Err(the tag) if it isn't one of ours.
            /// Validated, every arm fits in the storage.
            #vis fn get(&self) -> Result<#en_ref<'_ #validated>, #tag_enum> {
                let t: #tag_ty = #det::get(&self.0);
                let b = &self.as_bytes()[#payload_offset..];
                #gets_validated
                Err(#t_tag)
            }

            /// From an arm: new(), by the arm's type.
            #vis fn from_arm<A: #arm_of<#s_ty, Tag = #tag_ty>>(arm: A) -> Self {
                let mut s = Self::with_payload(arm);
                #det::set(&mut s, A::TAG);
                Self(s, ::core::marker::PhantomData)
            }

            /// From an arm ending in a flexible array, and that array - as many
            /// elements as the arm's header says: an error if the storage can't
            /// hold them.
            #vis fn from_arm_tailed<A>(arm: A, tail: &[A::Elem]) -> Result<Self, #vs::VStructError>
            where A: #arm_of<#s_ty, Tag = #tag_ty> + #vs::FlexArray
            {
                assert_eq!(arm.nr(), tail.len(), "{}: its header's count, and the elements given",
                           ::core::any::type_name::<A>());
                let at = #payload_offset + A::TAIL;
                let len = ::core::mem::size_of_val(tail);
                if at + len > #s_size {
                    return Err(#vs::VStructError::Overrun { ty: ::core::any::type_name::<A>(),
                                                            want: A::TAIL + len,
                                                            have: #s_size - #payload_offset });
                }
                let mut s = Self::with_payload(arm);
                // SAFETY: in bounds, as checked above, and the elements are plain data
                unsafe {
                    ::core::ptr::copy_nonoverlapping(tail.as_ptr().cast::<u8>(),
                        (&mut s as *mut #s_ty).cast::<u8>().add(at), len);
                }
                #det::set(&mut s, A::TAG);
                Ok(Self(s, ::core::marker::PhantomData))
            }

            /// From an arm: its payload into zeroed storage, then the determinant marks
            /// it - stabby's order, so a tag that's bits of the payload is written last.
            #vis fn new(v: #ev) -> Self {
                match v {
                    #(#news)*
                }
            }

            /// Zeroed storage holding @arm's payload, not yet marked as holding it.
            fn with_payload<A: #arm_of<#s_ty>>(arm: A) -> #s_ty {
                // SAFETY: zeroes are a valid one: plain data, or its tag an enum
                // with a zero variant - asserted below
                let mut s: #s_ty = unsafe { ::core::mem::zeroed() };
                // SAFETY: only arms are ArmOf, and each is plain data, in bounds - asserted
                // below
                unsafe {
                    (&mut s as *mut #s_ty).cast::<u8>().add(#payload_offset).cast::<A>()
                        .write_unaligned(arm);
                }
                s
            }
        }

        #(#arms_of)*
        #pad_assert
        #(#arm_asserts)*
        #(#packed_asserts)*
    })
}
