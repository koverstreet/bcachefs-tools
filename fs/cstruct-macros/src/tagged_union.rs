// SPDX-License-Identifier: GPL-2.0
//! tagged_union!: a tagged union with a stable representation, declared as
//! an enum - its arms the variants, each with its payload's type and the
//! tag's value as the discriminant:
//!
//!   tagged_union! {
//!       in crate::alloc::accounting_format;
//!       #[derive(Clone, Copy, MaybeZeroable, TypeInfo)]*
//!       #[tag(type_: u8)]
//!       #[tag_enum(DiskAccountingType: u32, nr)]
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
//!       in crate::format;
//!       #[storage(crate::format::JsetEntry)]
//!       #[tag(type_: u8)]
//!       #[tag_enum(JsetEntryType: u32, nr)]
//!       #[size(header)]
//!       pub enum JsetEntry {
//!           /// Btree key updates
//!           btree_keys(c::jset_entry_btree_keys) = 0,
//!           ...
//!       }
//!   }
//!
//! `in MODULE;` is where it's declared: NAME_match! expands where it's used,
//! and names what's generated here by it. The arm types it pastes are as
//! written, so are full paths.
//!
//! A discriminant not written is the last's plus one, as an enum's. An arm's
//! type may be defined in its variant, `[#[..]] [pub] struct|union NAME {
//! .. }`: it goes to nestify::nest!, so it can nest types of its own, with
//! the declaration's `#[...]*` - nestify's propagating attributes, for each
//! arm type and what it nests, not for the storage. It's reported where it's
//! written: the definition keeps its tokens' spans.
//!
//! `#[tag_enum(NAME: REPR[, nr])]`:
//! the tag's enum, an open #[bch_enum] of the arms (bch_enum.rs). NAME_arms!
//! is the arms as a list, (name, value, Type), for what's made from each.
//!
//! A type whose bytes come from outside - off a device, from the kernel -
//! with a tag that says which of its arms it holds, and that may be one this
//! version doesn't know. The layout is given, not chosen: the storage is
//! plain #[repr(C)] data - without #[storage], a struct of the tag, then a
//! union of the arms, overlaid with the pad.
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
//!  - `union DiskAccountingPosStorage`: the storage, as on disk -
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
//!    its own header says (CountedHead, from_bytes());
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
//!  - `type_()`, the tag, known or not; `as_bytes()`.
//!
//! The storage's bytes are always all written: they come from outside, by
//! from_bytes(), or from new(), which starts from zeroes - so get() and
//! as_bytes() can read any of them. A literal of the storage could leave
//! some unwritten, as could an arm with padding; nothing makes either.
//!
//! Or, `#[storage(STORAGE)] #[size(header)]`, the storage is another's: a
//! header, then its arm, as far as the header's count says - a journal
//! entry's, a superblock field's: an unsized vstruct (util/vstructs.rs), each
//! reference to which has its bytes. An arm is what's past the header, its
//! payload, as a key value is what's past the key; the header is the
//! storage's alone. Then what's generated is the Ref enum, ArmOf, and on the
//! storage in each state - X<Unvalidated>, X<Validated> - payload(), its
//! arm's bytes, and typed(): the arm its tag selects, over those bytes, in
//! the storage's state - every arm type has a family
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
//! its tag; walk(), over a run of them (ArmWalk). No storage is made: HEAD
//! and the arms have their own definitions.
//!
//! The declaration is parsed into a TaggedUnion, which generate() takes.

use proc_macro2::{Delimiter, Group, Ident, TokenStream, TokenTree};
use quote::{format_ident, quote};
use syn::parse::ParseStream;
use syn::{parse_quote, Token};

use crate::cstruct::{EnumDef, EnumKind};
use crate::bch_enum::{self, arm_path};
use crate::tokens::vs;

/// A tagged_union!'s declaration, as generate() takes it.
///
/// The storage is its own: the tag, then a union of the arms, overlaid
/// with the pad - or another's, #[storage]. @size says what its size is a
/// function of: nothing, a constant, the pad's; the header's count - the
/// storage another's, a header and then its arm, and what's generated only
/// the typed view of it; or the tag - each value as long as
/// the arm its tag selects, the head the start of every arm.
struct TaggedUnion {
    name:      Ident,
    /// `in MODULE;`: where it's declared, which NAME_match! - expanded where
    /// it's used - names the arms' enum by.
    module:    syn::Path,
    /// #[storage(TYPE)]: the storage is that type, another's, and what's
    /// generated a view of it; else it's this one's own, which it defines.
    storage:   Option<syn::Type>,
    /// #[tag(FIELD..)]: the tag's field, and its accessor.
    tag_field: Ident,
    tag_ty:    syn::Type,
    /// An open #[tag_enum]: the tag as handed out, a newtype over an integer
    /// the tag widens to - or none, the tag as it is.
    tag_enum:  Option<syn::Type>,
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
    /// vstruct - a family (util/vstructs.rs), each state's reference to which
    /// has its bytes - its arms what's past the header, as far as the count
    /// says: the header the storage's alone.
    Header,
    /// Its tag: the arm the tag selects, whose size it is - a union, its
    /// storage the head every arm starts with, the tag in it; a value read
    /// off bytes and written back to them, no storage of its own (ArmSized).
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
                                #[size(header)] - over another's storage, whose arms are what's past \
                                its header, as far as its count says - or #[storage(HEAD)] #[size(arm)] - \
                                the head every arm starts with, each as long as its arm"));
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
const OURS: &[&str] = &["tag", "tag_enum", "storage", "pad", "size", "packed"];

/// The union the declaration says, and what's generated from its arms besides
/// it: the tag enum and NAME_arms!.
fn enum_form(input: TokenStream) -> Result<(Decl, TaggedUnion, TokenStream), String> {
    let (module, attrs, arm_attrs, ours, vis, name, body) = syn::parse::Parser::parse2(|input: ParseStream<'_>| {
        let module = crate::tokens::parse_module(input)?;
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
        Ok((module, attrs, arm_attrs, ours, vis, name, body.parse::<TokenStream>()?))
    }, input).map_err(|e| format!("expected in MODULE; [#[..]] pub enum NAME {{ .. }}: {e}"))?;
    let err = |m: &str| format!("tagged_union! {name}: {m}");
    let module = module.ok_or_else(|| err("no `in MODULE;` - NAME_match!, which expands where it's used, \
                                            names the arms' enum by it"))?;

    let mut tag: Option<(Ident, syn::Type)> = None;
    let mut tag_enum: Option<EnumDef> = None;
    let (mut storage, mut pad, mut size, mut packed) = (None, None, None, false);
    for a in &ours {
        let kind = a.path().get_ident().map(ToString::to_string).unwrap_or_default();
        let bad = |e: syn::Error| err(&format!("#[{kind}(..)]: {e}"));
        match kind.as_str() {
            "tag" => tag = Some(a.parse_args_with(|i: ParseStream<'_>| {
                let field: Ident = i.parse()?;
                i.parse::<Token![:]>()?;
                Ok((field, i.parse()?))
            }).map_err(|_| err("#[tag(FIELD: TYPE)]"))?),
            "tag_enum" => tag_enum = Some(bch_enum::tag_enum(a.parse_args().map_err(bad)?)
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
            _ => unreachable!(),
        }
    }
    let (tag_field, tag_ty) = tag.ok_or_else(|| err("no #[tag(..)]"))?;

    let (body, defs) = arm_defs(body);
    let arms = bch_enum::arms(body, &Vec::new(), true).map_err(|m| err(&m))?;
    let mut out = TokenStream::new();
    if let Some(e) = &tag_enum {
        out.extend(bch_enum::generate(e, &arms, &Vec::new()));
    }
    out.extend(bch_enum::entries(&name, &arms));

    if tag_enum.as_ref().is_some_and(|e| e.kind == EnumKind::Closed) {
        return Err(err("#[tag_enum(..)]: an open enum - a tag from outside may be one it doesn't know"));
    }
    let tag_enum_ty = tag_enum.as_ref().map(|e| -> syn::Type {
        let n = &e.name;
        parse_quote!(#module::#n)
    });

    let tu = TaggedUnion {
        name: name.clone(),
        module,
        storage,
        tag_field,
        tag_ty,
        tag_enum: tag_enum_ty,
        arms: arms.iter().map(|a| TaggedArm {
            name: a.name.clone(), ty: a.ty.clone().expect("typed arms"), value: a.value.clone(),
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

/// Path type @ty, its last segment's name its generated @part: a state's
/// definition, X_validated.
fn derived_type(ty: &syn::Type, part: &str) -> syn::Type {
    let mut ty = ty.clone();
    if let syn::Type::Path(p) = &mut ty {
        let last = p.path.segments.last_mut().expect("a path has a segment");
        last.ident = crate::tokens::derived(&last.ident, part);
    }
    ty
}
/// Whether @ty is @def, a type the block defines.
fn is_def(ty: &syn::Type, def: &Ident) -> bool {
    matches!(ty, syn::Type::Path(p) if p.qself.is_none() && p.path.is_ident(def))
}

fn generate(d: &Decl, tu: &TaggedUnion) -> Result<TokenStream, String> {
    let (arms, name) = (&tu.arms, &tu.name);
    if arms.is_empty() {
        return Err(format!("{name}: no arms"));
    }
    if let Some(def) = tu.defs.iter().find(|def| !arms.iter().any(|a| is_def(&a.ty, def))) {
        return Err(format!("{name}: {def} is defined here but isn't an arm's type"));
    }
    // The arm types defined in the block: nestify::nest!, given the
    // declaration's `#[...]*` - propagating, for any types nested in them -
    // as the tokens they were written as.
    let arm_attrs = &d.arm_attrs;
    let defs: TokenStream = d.defs.iter().map(|def| quote!(::nestify::nest! { #arm_attrs #def })).collect();
    let en = format_ident!("{}", camel(&name.to_string()));
    let g = Gen {
        tu,
        vis:         &d.vis,
        en_ref:      format_ident!("{en}Ref"),
        en,
        // The tag as handed out: its enum, a newtype over an integer the tag
        // widens to - or with none, the tag as it is
        tag_enum:    tu.tag_enum.as_ref().unwrap_or(&tu.tag_ty),
        // The storage: the type the views are of. A sized one's is its own,
        // NAME_storage: NAME is its state's
        s_ty:        tu.storage.clone().unwrap_or_else(|| {
            let own = crate::tokens::derived(name, "storage");
            parse_quote!(#own)
        }),
        determinant: crate::tokens::derived(name, "tag"),
    };
    let body = match tu.size {
        Size::Pad    => g.pad(&d.attrs),
        Size::Header => g.header()?,
        Size::Arm    => g.arm(),
    };
    Ok(quote!(#defs #body))
}

/// What the three sizes' generation shares: the union's names, its tag, and
/// its storage.
struct Gen<'a> {
    tu:          &'a TaggedUnion,
    vis:         &'a syn::Visibility,
    /// The union's name in CamelCase: its arms' enum by value, and by
    /// reference, en_ref
    en:          Ident,
    en_ref:      Ident,
    tag_enum:    &'a syn::Type,
    s_ty:        syn::Type,
    determinant: Ident,
}

impl Gen<'_> {
    /// The tag, @x, as handed out.
    fn wrap_tag(&self, x: TokenStream) -> TokenStream {
        match &self.tu.tag_enum {
            Some(e) => quote!(#e(#x as _)),
            None => x,
        }
    }

    fn t_tag(&self) -> TokenStream {
        self.wrap_tag(quote!(t))
    }

    fn as_tag(&self, v: &syn::Expr) -> TokenStream {
        let tag_ty = &self.tu.tag_ty;
        quote!((#v) as #tag_ty)
    }

    /// The determinant over the storage.
    fn det(&self) -> TokenStream {
        let (determinant, s_ty) = (&self.determinant, &self.s_ty);
        quote!(<#determinant as crate::types::Determinant<#s_ty>>)
    }

    // ---- The core, what every size is made of: the determinant, the arms'
    // enums, the arm the tag selects, tag(), and ArmOf

    /// The determinant, over each of @storages: @get reads the tag from `u`,
    /// @set writes `tag` to it.
    fn determinant(&self, storages: &[syn::Type], get: TokenStream, set: TokenStream) -> TokenStream {
        let (name, vis, determinant) = (&self.tu.name, self.vis, &self.determinant);
        let (tag, tag_ty) = (&self.tu.tag_field, &self.tu.tag_ty);
        let doc = format!(" {name}'s tag: its {tag}.");
        let impls = storages.iter().map(|s| quote! {
            impl crate::types::Determinant<#s> for #determinant {
                type Tag = #tag_ty;
                fn get(u: &#s) -> #tag_ty {
                    #get
                }
                fn set(u: &mut #s, tag: #tag_ty) {
                    #set
                }
            }
        });
        quote! {
            #[doc = #doc]
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            #vis struct #determinant;

            #(#impls)*
        }
    }

    /// The determinant of another's storage, in each of @storages: its tag is
    /// its own field, read by value, as a packed struct's must be.
    fn given_storage(&self, storages: &[syn::Type]) -> TokenStream {
        let (tag, tag_ty) = (&self.tu.tag_field, &self.tu.tag_ty);
        self.determinant(storages, quote!(<#tag_ty>::from({ u.#tag })), quote!(u.#tag = tag.into();))
    }

    /// An enum of the arms, @name, each variant its arm's @field: by value,
    /// the arm's type; by reference, a reference to it.
    fn arms_enum(&self, name: &Ident, doc: &str, attrs: TokenStream, generics: TokenStream,
                 field: impl Fn(&TaggedArm) -> TokenStream) -> TokenStream {
        let vis = self.vis;
        let variants = self.tu.arms.iter().map(|a| {
            let (arm, field) = (&a.name, field(a));
            quote!(#arm(#field),)
        });
        quote! {
            #[doc = #doc]
            #[allow(non_camel_case_types)]
            #attrs
            #vis enum #name #generics {
                #(#variants)*
            }
        }
    }

    /// The arm the tag, `t`, selects: @get of it, returned.
    fn select(&self, get: impl Fn(&TaggedArm) -> TokenStream) -> TokenStream {
        self.tu.arms.iter().map(|a| {
            let (v, got) = (self.as_tag(&a.value), get(a));
            quote!(if t == #v { return #got; })
        }).collect()
    }

    /// tag(), from @raw: the tag as handed out.
    fn tag_fn(&self, doc: &str, raw: TokenStream) -> TokenStream {
        let (vis, tag, tag_enum) = (self.vis, &self.tu.tag_field, self.tag_enum);
        let tag_of = self.wrap_tag(raw);
        quote! {
            #[doc = #doc]
            #vis fn #tag(&self) -> #tag_enum {
                #tag_of
            }
        }
    }

    /// ArmOf storage @of, for each arm's type that @ty gives.
    fn arms_of(&self, of: TokenStream, ty: impl Fn(&TaggedArm) -> Option<TokenStream>) -> TokenStream {
        let tag_ty = &self.tu.tag_ty;
        self.tu.arms.iter().filter_map(|a| {
            let (ty, v) = (ty(a)?, self.as_tag(&a.value));
            Some(quote! {
                impl crate::types::ArmOf<#of> for #ty {
                    type Tag = #tag_ty;
                    const TAG: #tag_ty = #v;
                }
            })
        }).collect()
    }

    // ---- The sizes

    /// #[size(arm)]: each value as long as the arm its tag selects - the
    /// variant, by value - read off bytes and written back to them:
    /// ArmSized, and ArmWalk over a run of them. The tag is the head's, so
    /// every arm starts with the head, its tag where the head has it; and
    /// every arm is plain data, read and written by copying, at any
    /// alignment. No ArmOf: arms may share a type, the variant saying which.
    fn arm(&self) -> TokenStream {
        let (tu, vs, vis, en, tag_enum) = (self.tu, vs(), self.vis, &self.en, self.tag_enum);
        let (arms, name, tag, tag_ty) = (&tu.arms, &tu.name, &tu.tag_field, &tu.tag_ty);
        let (head, det, t_tag, determinant) = (&self.s_ty, self.det(), self.t_tag(), &self.determinant);
        let each = |f: &dyn Fn(&TaggedArm) -> TokenStream| {
            let arms = arms.iter().map(f);
            quote!(match self { #(#arms)* })
        };
        let reads = self.select(|a| {
            let (arm, ty) = (&a.name, &a.ty);
            quote!(#vs::read_plain::<#ty>(b).map(Self::#arm).map_err(|err| #vs::ArmError::Bad { tag: #t_tag, err }))
        });
        let tags = each(&|a| {
            let (arm, v) = (&a.name, self.as_tag(&a.value));
            quote!(Self::#arm(_) => #v,)
        });
        let arm_bytes = each(&|a| {
            let arm = &a.name;
            quote!(Self::#arm(x) => #vs::plain_bytes(x),)
        });
        let names = each(&|a| {
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
        let storage = self.given_storage(std::slice::from_ref(head));
        let by_value = self.arms_enum(en, &doc, quote!(#[derive(Clone, Copy)]), quote!(), |a| {
            let ty = &a.ty;
            quote!(#ty)
        });
        let tag_fn = self.tag_fn(" Its tag: its variant's.", quote!(#vs::ArmSized::tag_raw(self)));
        quote! {
            #storage
            const _: () = #vs::assert_plain::<#head>();
            const _: () = assert!(::core::mem::align_of::<#head>() == 1, #head_aligned);
            #(#asserts)*

            #by_value

            impl #en {
                #tag_fn

                /// Its arm's name.
                #vis fn name(&self) -> &'static str {
                    #names
                }
            }

            impl #vs::ArmSized for #en {
                type Tag = #tag_enum;
                type Head = #head;
                type Det = #determinant;

                fn read(b: &[u8]) -> Result<Self, #vs::ArmError<#tag_enum>> {
                    let h: #head = #vs::read_plain(b).map_err(#vs::ArmError::Short)?;
                    let t: #tag_ty = #det::get(&h);
                    #reads
                    Err(#vs::ArmError::Unknown(#t_tag))
                }

                fn arm_bytes(&self) -> &[u8] {
                    #arm_bytes
                }

                fn tag_raw(&self) -> #tag_ty {
                    #tags
                }
            }
        }
    }

    /// #[size(header)]: an unsized vstruct, its arm what's past its header -
    /// its payload() - as far as its count says, each reference to one has
    /// them: typed() and arm(), on the storage in each state. An arm there
    /// may be an older, shorter version of its type: as its ArmRef has it,
    /// over the payload's bytes.
    ///
    /// Its storage is a family, X<S> its definition in each state - what its
    /// tag is read from. Every arm has a family too - #[whole], #[grown] - so
    /// each is named in the storage's state: a member of an Unvalidated one
    /// is Unvalidated, all the way down.
    fn header(&self) -> Result<TokenStream, String> {
        let (tu, vs, vis, s_ty, tag_enum) = (self.tu, vs(), self.vis, &self.s_ty, self.tag_enum);
        let (arms, name, tag_ty, module) = (&tu.arms, &tu.name, &tu.tag_ty, &tu.module);
        let (en_ref, determinant, t_tag) = (&self.en_ref, &self.determinant, self.t_tag());
        let not_path = std::iter::once(s_ty).chain(arms.iter().map(|a| &a.ty))
            .find(|t| !matches!(t, syn::Type::Path(_)));
        if let Some(t) = not_path {
            return Err(format!("{name}: {}: #[size(header)]'s storage and arms are each a family's \
                                type, named", quote!(#t)));
        }
        let states = ["Unvalidated", "Validated"];
        let storages: Vec<syn::Type> = states.iter().map(|st| derived_type(s_ty, &st.to_lowercase())).collect();
        let doc = format!(" One of {name}'s arms, with the bytes its header says are its - as\n \
                           many as there are, for one written before its type grew: what\n \
                           {name}'s typed() gives.");
        let head = quote!(::core::mem::size_of::<<Self as #vs::DstHead>::Head>());
        let mut out = self.given_storage(&storages);
        out.extend(self.arms_enum(en_ref, &doc, quote!(#[derive(Clone, Copy)]),
                                  quote!(<'a, St: #vs::State = #vs::Validated>), |a| {
            let ty = &a.ty;
            quote!(&'a #ty<St>)
        }));
        for (st, s) in states.iter().zip(&storages) {
            let (lower, st) = (st.to_lowercase(), format_ident!("{st}"));
            // ArmOf the storage in a state, X<St>, for the arm's definition in
            // that state, which the arm's X<St> is or has it by: an arm of an
            // Unvalidated storage is Unvalidated
            out.extend(self.arms_of(quote!(#s_ty<#vs::#st>), |a| {
                let def = derived_type(&a.ty, &lower);
                Some(quote!(#def))
            }));
            let det = quote!(<#determinant as crate::types::Determinant<#s>>);
            let tag_fn = self.tag_fn(" The tag, whether this version knows it or not.", quote!(#det::get(self)));
            let selects = self.select(|a| {
                let (arm, ty) = (&a.name, &a.ty);
                quote!(<#ty<#vs::#st> as #vs::ArmRef>::arm_ref(self.payload()).map(#en_ref::#arm).ok_or(#t_tag))
            });
            out.extend(quote! {
                impl #s_ty<#vs::#st> {
                    #tag_fn

                    /// Its arm's bytes: what's past its header.
                    #vis fn payload(&self) -> &[u8] {
                        &self.as_bytes()[#head..]
                    }

                    /// The arm its tag selects, over its bytes - as many as its count
                    /// says, as its ArmRef has it: Err(the tag) if it isn't one of
                    /// ours, or the bytes are fewer than any valid one's. One shorter
                    /// than its type is an older version, its missing fields zero.
                    #vis fn typed(&self) -> Result<#en_ref<'_, #vs::#st>, #tag_enum> {
                        let t: #tag_ty = #det::get(self);
                        #selects
                        Err(#t_tag)
                    }

                    /// The arm, if the tag is @A's: typed(), for one arm.
                    #vis fn arm<A>(&self) -> Option<&A>
                    where
                        A: crate::types::ArmOf<Self, Tag = #tag_ty> + #vs::ArmRef + ?Sized,
                    {
                        if #det::get(self) != A::TAG {
                            return None;
                        }
                        A::arm_ref(self.payload())
                    }

                    /// arm(), to edit: as its ArmRef has it.
                    #vis fn arm_mut<A>(&mut self) -> Option<&mut A>
                    where
                        A: crate::types::ArmOf<Self, Tag = #tag_ty> + #vs::ArmRef + ?Sized,
                    {
                        if #det::get(self) != A::TAG {
                            return None;
                        }
                        A::arm_mut(&mut self.as_bytes_mut()[#head..])
                    }
                }
            });
        }
        let match_arms = arms.iter().map(|a| {
            let (arm, path) = (&a.name, arm_path(&a.ty));
            quote!(#module::#en_ref::#arm($a) => { $(type $A<S> = #path<S>;)? $body })
        });
        let tag_arms = arms.iter().map(|a| {
            let (v, path) = (self.as_tag(&a.value), arm_path(&a.ty));
            quote!(if t == #v { type $A<S> = #path<S>; $body } else)
        });
        let (match_mac, by_tag_mac) = (format_ident!("{name}_match"), format_ident!("{name}_by_tag"));
        out.extend(quote! {
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
        });
        Ok(out)
    }

    /// #[size(pad)]: its own storage, a struct of the tag then a union of the
    /// arms - with types of their own, Rust having no anonymous struct or
    /// union - overlaid with the pad; NAME<S> over it, the storage in a
    /// state; and from an arm, or from bytes.
    fn pad(&self, attrs: &[syn::Attribute]) -> TokenStream {
        let (tu, vs, vis, s_ty, tag_enum) = (self.tu, vs(), self.vis, &self.s_ty, self.tag_enum);
        let (arms, name, tag, tag_ty) = (&tu.arms, &tu.name, &tu.tag_field, &tu.tag_ty);
        let (en, en_ref, determinant) = (&self.en, &self.en_ref, &self.determinant);
        let (det, t_tag) = (self.det(), self.t_tag());
        // the arms by value: named for the union, but where that's the storage's
        // own name - DiskAccountingPos - its Arm
        let ev = if en == name { format_ident!("{en}Arm") } else { en.clone() };
        let own_storage = crate::tokens::derived(name, "storage");
        let s_size = quote!(::core::mem::size_of::<#s_ty>());

        let arm_fields = arms.iter().map(|a| {
            let (arm, ty) = (&a.name, &a.ty);
            quote!(#vis #arm: #ty,)
        });
        let pad_field = tu.pad.as_ref().map(|p| quote!(#vis _pad: #p,));
        let storage_attrs = quote!(#[allow(non_camel_case_types)] #[repr(C)] #[derive(Clone, Copy)]);
        // `packed`, a packed layout - the arms' union and the tag's struct
        let packed_attrs = if tu.packed {
            quote!(#[allow(non_camel_case_types)] #[repr(C, packed)] #[derive(Clone, Copy)])
        } else {
            storage_attrs.clone()
        };
        let (arms_union, tagged) = (crate::tokens::derived(name, "arms"), crate::tokens::derived(name, "tagged"));
        let payload_offset = quote!(::core::mem::offset_of!(#tagged, u));
        let storage = self.determinant(std::slice::from_ref(s_ty),
            // the tag is plain data, the struct's first field
            quote!(unsafe { u.t.#tag }),
            quote!(#[allow(unused_unsafe)] unsafe { u.t.#tag = tag }));

        // An arm type this declares is plain data: a reference to it, whatever
        // the storage's state. Another's is a #[counted] family's head - a
        // replicas entry's - the family's type in the
        // storage's state, as long as its own header says (CountedHead, from_bytes()).
        let declared = |a: &TaggedArm| tu.defs.iter().any(|def| is_def(&a.ty, def));
        let stated = arms.iter().any(|a| !declared(a));
        let st_param = stated.then(|| quote!(, St: #vs::State = #vs::Validated));
        let st_arg = |st: &str| {
            let st = format_ident!("{st}");
            stated.then(|| quote!(, #vs::#st))
        };
        let ref_doc = if stated {
            format!(" One of {name}'s arms, by reference, in its state: what {name}::get()\n \
                     gives - an arm that's another's type as long as its own header says.")
        } else {
            format!(" One of {name}'s arms, by reference: what {name}::get() gives.")
        };
        let by_ref = self.arms_enum(en_ref, &ref_doc, quote!(#[derive(Clone, Copy)]), quote!(<'a #st_param>), |a| {
            let ty = &a.ty;
            match declared(a) {
                true => quote!(&'a #ty),
                false => quote!(&'a <#ty as #vs::CountedHead>::Of<St>),
            }
        });
        let value_doc = format!(" One of {name}'s arms, by value: what {name}::new() takes.");
        let by_value = self.arms_enum(&ev, &value_doc, quote!(), quote!(), |a| {
            let ty = &a.ty;
            quote!(#ty)
        });
        let lens = arms.iter().map(|a| {
            let arm = &a.name;
            quote!(#en_ref::#arm(a) => ::core::mem::size_of_val(a),)
        });
        let (st_impl, st_use) = if stated { (quote!(, St: #vs::State), quote!(, St)) } else { (quote!(), quote!()) };
        // get()'s arms, in state @st: @bad, what an arm whose header says it's
        // longer than the storage holds is
        // get()'s arms, in state St: an error if an arm's header says it's
        // longer than the storage holds
        let selects = self.select(|a| {
            let (arm, ty) = (&a.name, &a.ty);
            let got = if declared(a) {
                quote! {
                    // SAFETY: an arm this declares is plain data, in the storage
                    // and aligned there: asserted below
                    unsafe { &*b.as_ptr().cast::<#ty>() }
                }
            } else {
                quote! {
                    #vs::from_bytes::<<#ty as #vs::CountedHead>::Of<St>>(b)
                        .map_err(|err| #vs::ArmError::Bad { tag: #t_tag, err })?
                }
            };
            quote!(Ok(#en_ref::#arm(#got)))
        });
        // a family is SelfSized in each state, not over every one
        let counted_in_st = arms.iter().filter(|a| !declared(a)).map(|a| {
            let ty = &a.ty;
            quote!(<#ty as #vs::CountedHead>::Of<St>: #vs::SelfSized,)
        });
        let news = arms.iter().map(|a| {
            let arm = &a.name;
            quote!(#ev::#arm(x) => Self::from_arm(x),)
        });
        // An arm that's the storage itself - an ACL's short entry - isn't a type
        // of its own to go by
        let arms_of = self.arms_of(quote!(#s_ty), |a| {
            let ty = &a.ty;
            (a.ty != *s_ty).then(|| quote!(#ty))
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
        // Plain, as a family's head is: its pad is every byte of it, and
        // Plain, and the arms are plain data.
        let pad_assert = tu.pad.as_ref().map(|p| {
            let msg = format!("tagged_union! {name}: bigger than its pad");
            quote! {
                const _: () = assert!(#s_size == ::core::mem::size_of::<#p>(), #msg);
                // SAFETY: its pad is every byte of it - asserted above - and Plain,
                // and its arms are plain data, any bytes one
                unsafe impl #vs::Plain for #s_ty {}
                const _: () = #vs::assert_plain::<#p>();
            }
        });

        let tag_fn = self.tag_fn(" The tag, whether this version knows it or not.", quote!(#det::get(&self.0)));
        // Bytes from outside
        let unvalidated_impl = {
            let unvalidated = st_arg("Unvalidated");
            quote! {
                impl #name<#vs::Unvalidated> {
                    /// From its bytes, as on disk.
                    #vis fn from_bytes(b: &[u8; #s_size]) -> Self {
                        // SAFETY: the storage is plain data: any bytes are a valid one
                        Self(unsafe { b.as_ptr().cast::<#s_ty>().read_unaligned() }, ::core::marker::PhantomData)
                    }

                    /// The arm the tag selects: Err if the tag isn't one of ours, or the
                    /// arm's header says it's longer than the storage holds.
                    #vis fn get(&self) -> Result<#en_ref<'_ #unvalidated>, #vs::ArmError<#tag_enum>> {
                        self.arm_in::<#vs::Unvalidated>()
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
        };
        let validated = st_arg("Validated");
        let doesnt_fit = format!("{name}<Validated>: {{err}} - an arm that doesn't fit, in one validate passed \
                                  or made from an arm");
        let state_doc = format!(" {name}, in state S - Validated unless named: over its storage, {own_storage}.\n \
                                 Unvalidated, bytes from outside - from the kernel, off a device - each\n \
                                 arm checked against the storage as get() hands it out; Validated,\n \
                                 what validate passed, or what's made here from an arm.");
        let len_doc = format!(" The bytes it takes, from {name}::ARMS_AT - a tailed arm's, as\n \
                               many as its header says.");
        quote! {
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
            #vis union #own_storage {
                #vis t: #tagged,
                #pad_field
            }

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

            #by_value
            #by_ref

            impl<'a #st_impl> #en_ref<'a #st_use> {
                #[doc = #len_doc]
                #vis fn len(&self) -> usize {
                    match *self {
                        #(#lens)*
                    }
                }
            }

            impl<S> #name<S> {
                #tag_fn

                /// Its bytes, as on disk.
                #vis fn as_bytes(&self) -> &[u8; #s_size] {
                    // SAFETY: plain data, every byte of it written: it came from
                    // from_bytes() or new(), which starts from zeroes
                    unsafe { &*(self as *const Self).cast::<[u8; #s_size]>() }
                }

                /// The arm the tag selects, in state St: get()'s, in either.
                fn arm_in<St: #vs::State>(&self) -> Result<#en_ref<'_ #st_use>, #vs::ArmError<#tag_enum>>
                where
                    #(#counted_in_st)*
                {
                    let t: #tag_ty = #det::get(&self.0);
                    let b = &self.as_bytes()[#payload_offset..];
                    #selects
                    Err(#vs::ArmError::Unknown(#t_tag))
                }
            }

            #unvalidated_impl

            impl #name<#vs::Validated> {
                /// Where its arms start, in any state: after the tag.
                #vis const ARMS_AT: usize = #payload_offset;

                /// The arm the tag selects - Err(the tag) if it isn't one of ours.
                /// Validated, every arm fits in the storage.
                #vis fn get(&self) -> Result<#en_ref<'_ #validated>, #tag_enum> {
                    self.arm_in::<#vs::Validated>().map_err(|e| match e {
                        #vs::ArmError::Unknown(t) => t,
                        err => panic!(#doesnt_fit),
                    })
                }

                /// From an arm: new(), by the arm's type.
                #vis fn from_arm<A: crate::types::ArmOf<#s_ty, Tag = #tag_ty>>(arm: A) -> Self {
                    // SAFETY: only arms are ArmOf, each plain data, in bounds - asserted
                    // below
                    Self(unsafe { #vs::from_arm::<#s_ty, #determinant, A>(arm, #payload_offset) },
                         ::core::marker::PhantomData)
                }

                /// From an arm ending in a flexible array, and that array - as many
                /// elements as the arm's header says: an error if the storage can't
                /// hold them.
                #vis fn from_arm_tailed<A>(arm: A, tail: &[<A::Of<#vs::Validated> as #vs::Unsized>::Elem])
                    -> Result<Self, #vs::VStructError>
                where A: crate::types::ArmOf<#s_ty, Tag = #tag_ty> + #vs::CountedHead
                {
                    // SAFETY: as from_arm()'s
                    unsafe { #vs::from_arm_tailed::<#s_ty, #determinant, A>(arm, tail, #payload_offset) }
                        .map(|s| Self(s, ::core::marker::PhantomData))
                }

                /// From an arm: from_arm(), whichever it is.
                #vis fn new(v: #ev) -> Self {
                    match v {
                        #(#news)*
                    }
                }
            }

            #arms_of
            #pad_assert
            #(#arm_asserts)*
            #(#packed_asserts)*
        }
    }
}
