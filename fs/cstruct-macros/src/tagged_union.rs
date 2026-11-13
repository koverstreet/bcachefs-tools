// SPDX-License-Identifier: GPL-2.0
//! tagged_union!: a tagged union with a stable representation, declared as
//! an enum - its arms the variants, each with its payload's type and the
//! tag's value as the discriminant:
//!
//!   tagged_union! {
//!       #[derive(Clone, Copy, CStruct, TypeInfo)]*
//!       #[c_name(disk_accounting_pos)]
//!       #[tag(type_: u8)]
//!       #[tag_enum(disk_accounting_type: u32, prefix = BCH_DISK_ACCOUNTING_, nr)]
//!       #[pad(Bpos)]
//!       #[size(pad)]
//!       #[packed]
//!       pub enum DiskAccountingPos {
//!           /* No subtypes - number of inodes in the entire filesystem */
//!           nr_inodes(#[repr(C)] #[c_name(bch_acct_nr_inodes)] pub struct NrInodes {}) = 0,
//!           replicas(Replicas) = 2,
//!           ...
//!       }
//!   }
//!
//!   tagged_union! {
//!       #[storage(c::jset_entry)]
//!       #[tag(type_: u8)]
//!       #[tag_enum(bch_jset_entry_type: u32, prefix = BCH_JSET_ENTRY_, nr)]
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
//! `#[tag_enum([closed,] NAME: REPR, prefix = P[, nr][, c_name = C])]`:
//! the tag's enum, a c_enum! of the arms (enum_decl.rs) - open, or closed and
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
//!    C's struct disk_accounting_pos, by its #[c_name]; Plain, its pad being
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
//! The declaration is parsed into a CTaggedUnion (fs/types/cstruct.rs),
//! which generate() takes.

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};

use crate::cstruct::{rust_text, CEnumDef, CEnumKind, CTaggedArm, CTaggedDef, CTaggedSize,
                     CTaggedUnion};
use crate::enum_decl::{self, arm_path};
use crate::tokens;

pub(crate) fn tagged_union(input: TokenStream) -> Result<TokenStream, String> {
    let (decl, tu, mut out) = enum_form(&input)?;
    out.extend(generate(&decl, &tu)?);
    Ok(out)
}

/// The declaration's parts generate() passes on - the arm types as their
/// tokens, so an error in one is reported where it's written.
struct Decl {
    attrs:     String,
    vis:       String,
    arm_attrs: TokenStream,
    defs:      Vec<TokenStream>,
}

/// The union the declaration says, and what's generated from its arms besides
/// it: the tag enum and NAME_arms!.
fn enum_form(input: &TokenStream) -> Result<(Decl, CTaggedUnion, TokenStream), String> {
    let trees: Vec<TokenTree> = input.clone().into_iter().collect();
    let is_ident = |t: Option<&TokenTree>, s: &str| matches!(t, Some(TokenTree::Ident(i)) if i == s);
    let is_punct = |t: Option<&TokenTree>, c: char| matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == c);

    let mut attrs = String::new();
    let mut arm_attrs = TokenStream::new();
    let mut c_name = None;
    let mut ours: Vec<(String, TokenStream)> = Vec::new();
    let mut k = 0;
    while is_punct(trees.get(k), '#') {
        let Some(TokenTree::Group(g)) = trees.get(k + 1) else { break };
        let star = is_punct(trees.get(k + 2), '*');
        let mut inner = g.stream().into_iter();
        let head = match inner.next() {
            Some(TokenTree::Ident(i)) => i.to_string(),
            _ => String::new(),
        };
        let args = match inner.next() {
            Some(TokenTree::Group(a)) => a.stream(),
            _ => TokenStream::new(),
        };
        let n = 2 + usize::from(star);
        if star {
            arm_attrs.extend(trees[k..k + n].iter().cloned());
        } else if head == "c_name" {
            c_name = Some(rust_text(&args));
        } else if matches!(head.as_str(), "tag" | "tag_enum" | "storage" | "pad" | "size" | "packed" | "plain") {
            ours.push((head, args));
        } else {
            attrs.push_str(&format!("#{g}\n"));
        }
        k += n;
    }
    let vis_start = k;
    if is_ident(trees.get(k), "pub") {
        k += 1 + usize::from(matches!(trees.get(k + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis));
    }
    if !is_ident(trees.get(k), "enum") {
        return Err("tagged_union!: expected [#[..]] pub enum NAME { .. }".into());
    }
    let vis = rust_text(&trees[vis_start..k].iter().cloned().collect::<TokenStream>());
    let (Some(TokenTree::Ident(name)), Some(TokenTree::Group(body))) = (trees.get(k + 1), trees.get(k + 2)) else {
        return Err("tagged_union!: expected pub enum NAME { .. }".into());
    };
    let name = name.to_string();
    let err = |m: &str| format!("tagged_union! {name}: {m}");

    let text = |ts: &TokenStream| rust_text(ts);
    let mut tag: Option<(String, Option<String>)> = None;
    let mut tag_enum: Option<CEnumDef> = None;
    let (mut storage, mut pad, mut size, mut packed, mut plain) = (None, None, None, false, false);
    for (head, args) in &ours {
        match head.as_str() {
            "tag" => {
                let a: Vec<TokenTree> = args.clone().into_iter().collect();
                tag = Some(match a.as_slice() {
                    [TokenTree::Ident(f)] => (f.to_string(), None),
                    [TokenTree::Ident(f), c, ty @ ..] if is_punct(Some(c), ':') && !ty.is_empty() =>
                        (f.to_string(), Some(text(&ty.iter().cloned().collect()))),
                    _ => return Err(err("#[tag(FIELD: TYPE)], or #[tag(FIELD)] for a closed tag enum")),
                });
            }
            "tag_enum" => tag_enum = Some(enum_decl::tag_enum(args.clone()).map_err(|m| err(&format!("#[tag_enum(..)]: {m}")))?),
            "storage" => storage = Some(text(args)),
            "pad"     => pad = Some(text(args)),
            "size"    => size = Some(match text(args).as_str() {
                "pad"    => CTaggedSize::Pad,
                "header" => CTaggedSize::Header,
                "arm"    => CTaggedSize::Arm,
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

    let (body, defs) = arm_defs(body.stream());
    let arms = enum_decl::arms(body, &Vec::new(), true).map_err(|m| err(&m))?;
    let mut out = TokenStream::new();
    if let Some(e) = &tag_enum {
        out.extend(enum_decl::generate(e, &arms, &Vec::new()));
    }
    out.extend(enum_decl::entries(&name, &arms));

    let tag_enum_name = |e: &CEnumDef| e.name.clone().unwrap_or_default();
    let closed = |e: &CEnumDef| e.kind == CEnumKind::Closed;
    let (tag_ty, tag_enum_ty, tag_is_enum) = match (&tag_enum, tag_ty) {
        (Some(e), None) if closed(e) => (format!("c::{}", tag_enum_name(e)), String::new(), true),
        (Some(e), Some(ty)) if !closed(e) => (ty, format!("c::{}", tag_enum_name(e)), false),
        (None, Some(ty)) => (ty, String::new(), false),
        _ => return Err(err("#[tag(FIELD: TYPE)] with an open #[tag_enum], #[tag(FIELD)] with a closed one")),
    };

    let arm_value = |a: &enum_decl::Arm| match &tag_enum {
        Some(e) if closed(e) => format!("c::{}::{}", tag_enum_name(e), a.name),
        _ => a.value.clone(),
    };
    let tu = CTaggedUnion {
        name: name.clone(),
        c_name,
        union: storage.is_some(),
        storage,
        tag_field,
        tag_ty,
        tag_enum: tag_enum_ty,
        tag_is_enum,
        plain,
        arms: arms.iter().map(|a| CTaggedArm { name: a.name.clone(), ty: a.ty.clone().unwrap(), value: arm_value(a) }).collect(),
        pad,
        packed,
        size: size.ok_or_else(|| err("no #[size(..)] - what it's a function of, pad, header or arm"))?,
        defs: defs.iter().map(|(name, _)| CTaggedDef { name: name.clone() }).collect(),
    };
    tu.check()?;
    let defs = defs.into_iter().map(|(_, def)| def).collect();
    Ok((Decl { attrs, vis, arm_attrs, defs }, tu, out))
}

/// The arm types defined in their arms, `name(pub struct T { .. })`: @body
/// with `name(T)` for each, and each one's name and definition.
fn arm_defs(body: TokenStream) -> (TokenStream, Vec<(String, TokenStream)>) {
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
                defs.push((n.to_string(), g.stream()));
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

fn generate(d: &Decl, tu: &CTaggedUnion) -> Result<TokenStream, String> {
    let arms = &tu.arms;
    if arms.is_empty() {
        return Err(format!("{}: no arms", tu.name));
    }
    if let Some(d) = tu.defs.iter().find(|d| !arms.iter().any(|a| a.ty.trim() == d.name)) {
        return Err(format!("{}: {} is defined here but isn't an arm's type", tu.name, d.name));
    }
    // The arm types defined in the block: nestify::nest!, given the
    // declaration's `#[...]*` - propagating, for any types nested in them -
    // as the tokens they were written as.
    let mut defs = TokenStream::new();
    for def in &d.defs {
        let mut body = d.arm_attrs.clone();
        body.extend(def.clone());
        defs.extend(tokens("::nestify::nest!"));
        defs.extend([TokenTree::Group(Group::new(Delimiter::Brace, body))]);
    }
    let (attrs, vis) = (&d.attrs, &d.vis);
    let name = &tu.name;
    let c_name = tu.c_name.as_deref().unwrap_or(name);
    let en = camel(name);
    // the arms by value: named for the union, but where that's the storage's
    // own name - DiskAccountingPos, C's disk_accounting_pos - its Arm
    let ev = if en == *name { format!("{en}Arm") } else { en.clone() };
    let (tag, tag_ty) = (&tu.tag_field, &tu.tag_ty);
    // The tag as handed out: its enum, a newtype over an integer the tag
    // widens to - or with none, the tag as it is
    let tag_enum = if tu.tag_enum.is_empty() { tag_ty } else { &tu.tag_enum };
    let wrap_tag = |x: &str| if tu.tag_enum.is_empty() {
        x.to_string()
    } else {
        format!("{tag_enum}({x} as _)")
    };
    let t_tag = wrap_tag("t");
    // A value as the tag: an enum tag's values are its variants, as they are
    let as_tag = |v: &str| if tu.tag_is_enum { format!("({v})") } else { format!("({v}) as {tag_ty}") };
    let det_trait = "crate::types::Determinant";
    let arm_of = "crate::types::ArmOf";
    let vs = "crate::util::vstructs";
    // The storage: the type the views are of
    // A sized storage's is C's struct, {name}__ffi: {name} is its state's
    let s_ty = tu.storage.clone().unwrap_or_else(|| format!("{name}__ffi"));
    let s_size = format!("::core::mem::size_of::<{s_ty}>()");

    // The storage, as C has it: the tag then the arms' union (with types of
    // their own, C's anonymous struct and union), overlaid with the pad. Or
    // none, the storage being another's - an unsized union's.
    let arm_fields: String = arms.iter()
        .map(|a| format!("    {vis} {}: {},\n", a.name, a.ty))
        .collect();
    let pad_field = tu.pad.as_ref().map(|p| format!("    {vis} _pad: {p},\n")).unwrap_or_default();
    let storage_attrs = "#[allow(non_camel_case_types)]\n#[repr(C)]\n#[derive(Clone, Copy)]\n";
    // `packed`: where C has __packed - the arms' union and the tag's struct
    let packed_attrs = if tu.packed {
        "#[allow(non_camel_case_types)]\n#[repr(C, packed)]\n#[derive(Clone, Copy)]\n"
    } else {
        storage_attrs
    };
    // An unsized vstruct's storage is a family: each state its own
    // definition, named - what its tag is read from
    let states: Vec<(&str, String)> = if tu.size == CTaggedSize::Header {
        ["Unvalidated", "Validated"].iter()
            .map(|st| (*st, format!("{s_ty}__{}", st.to_lowercase())))
            .collect()
    } else {
        vec![("", s_ty.clone())]
    };
    let (storage, payload_offset, determinant) = if tu.storage.is_some() {
        // another's storage, a union: its tag is its own field, by value, as
        // a packed struct's must be
        let det = format!("{name}__tag");
        let impls: String = states.iter()
            .map(|(_, s)| format!(
                "impl {det_trait}<{s}> for {det} {{\n\
                     type Tag = {tag_ty};\n\
                     fn get(u: &{s}) -> {tag_ty} {{\n\
                         <{tag_ty}>::from({{ u.{tag} }})\n\
                     }}\n\
                     fn set(u: &mut {s}, tag: {tag_ty}) {{\n\
                         u.{tag} = tag.into();\n\
                     }}\n\
                 }}\n"))
            .collect();
        (format!("/// {name}'s tag: the storage's {tag}.\n\
                  #[doc(hidden)]\n\
                  #[allow(non_camel_case_types)]\n\
                  {vis} struct {det};\n\
                  \n\
                  {impls}"),
         "0".to_string(),
         det)
    } else {
        let det = format!("{name}__tag");
        (format!("#[doc(hidden)]\n{packed_attrs}{vis} union {name}__arms {{\n{arm_fields}}}\n\
                  \n\
                  #[doc(hidden)]\n{packed_attrs}{vis} struct {name}__tagged {{\n\
                      {vis} {tag}: {tag_ty},\n\
                      {vis} u: {name}__arms,\n\
                  }}\n\
                  \n\
                  #[::cstruct_macros::c_name({c_name})]\n{attrs}{storage_attrs}{vis} union {s_ty} {{\n\
                      {vis} t: {name}__tagged,\n\
                      {pad_field}\
                  }}\n\
                  \n\
                  /// {name}'s tag: its field.\n\
                  #[doc(hidden)]\n\
                  #[allow(non_camel_case_types)]\n\
                  {vis} struct {det};\n\
                  \n\
                  impl {det_trait}<{s_ty}> for {det} {{\n\
                      type Tag = {tag_ty};\n\
                      fn get(u: &{s_ty}) -> {tag_ty} {{\n\
                          // SAFETY: the tag is plain data, the struct's first field\n\
                          unsafe {{ u.t.{tag} }}\n\
                      }}\n\
                      fn set(u: &mut {s_ty}, tag: {tag_ty}) {{\n\
                          #[allow(unused_unsafe)]\n\
                          // SAFETY: a write of plain data\n\
                          unsafe {{ u.t.{tag} = tag }}\n\
                      }}\n\
                  }}\n"),
         format!("::core::mem::offset_of!({name}__tagged, u)"),
         det)
    };
    let det = format!("<{determinant} as {det_trait}<{s_ty}>>");

    let by_value: String = arms.iter().map(|a| format!("    {}({}),\n", a.name, a.ty)).collect();
    // An arm type this declares is plain data: a reference to it, whatever
    // the storage's state - and with `plain`, every arm's. Another's is a
    // #[counted] family's head - a replicas entry's, as C's union holds it -
    // the family's type in the storage's state, as long as its own header
    // says (CountedHead, ArmAt).
    let declared = |a: &CTaggedArm| tu.plain || tu.defs.iter().any(|d| d.name == a.ty.trim());
    let stated = arms.iter().any(|a| !declared(a));
    let st_param = if stated { format!(", St: {vs}::State = {vs}::Validated") } else { String::new() };
    let st_arg = |st: &str| if stated { format!(", {vs}::{st}") } else { String::new() };
    let by_ref: String = arms.iter()
        .map(|a| if declared(a) {
            format!("    {}(&'a {}),\n", a.name, a.ty)
        } else {
            format!("    {}(&'a <{} as {vs}::CountedHead>::Of<St>),\n", a.name, a.ty.trim())
        })
        .collect();
    let lens: String = arms.iter()
        .map(|a| format!("{en}Ref::{}(a) => ::core::mem::size_of_val(a),\n", a.name))
        .collect();
    let (st_impl, st_use) = if stated { (format!(", St: {vs}::State"), ", St") } else { (String::new(), "") };
    // get()'s arms, in state @st: @bad, what an arm whose header says it's
    // longer than the storage holds is
    let gets = |st: &str, bad: &str| -> String {
        arms.iter()
            .map(|a| {
                let arm = if declared(a) {
                    format!("// SAFETY: an arm this declares is plain data, in the storage\n\
                             // and aligned there: asserted below\n\
                             unsafe {{ &*b.as_ptr().cast::<{}>() }}", a.ty)
                } else {
                    format!("match <<{} as {vs}::CountedHead>::Of<{vs}::{st}> as {vs}::ArmAt>::arm_at(b) {{\n\
                                 Ok(arm) => arm,\n\
                                 Err(err) => {bad},\n\
                             }}", a.ty.trim())
                };
                format!("if t == {} {{\n\
                             return Ok({en}Ref::{}({arm}));\n\
                         }}\n", as_tag(&a.value), a.name)
            })
            .collect()
    };
    let news: String = arms.iter()
        .map(|a| format!("{ev}::{}(x) => Self::from_arm(x),\n", a.name))
        .collect();
    // An arm that's the storage itself - an ACL's short entry - isn't a type
    // of its own to go by
    let arms_of: String = arms.iter()
        .filter(|a| a.ty.trim() != s_ty.trim())
        .map(|a| format!("impl {arm_of}<{s_ty}> for {} {{\n\
                              type Tag = {tag_ty};\n\
                              const TAG: {tag_ty} = {};\n\
                          }}\n",
                         a.ty, as_tag(&a.value)))
        .collect();
    // Where get() finds an arm: in the storage, aligned for it
    let arm_asserts: String = arms.iter()
        .map(|a| format!("const _: () = assert!({payload_offset} + ::core::mem::size_of::<{ty}>() <= {s_size},\n\
                              \"tagged_union! {name}: arm {arm} doesn't fit in the storage\");\n\
                          const _: () = assert!(({payload_offset}) % ::core::mem::align_of::<{ty}>() == 0 &&\n\
                                                ::core::mem::align_of::<{s_ty}>() >= ::core::mem::align_of::<{ty}>(),\n\
                              \"tagged_union! {name}: arm {arm} isn't aligned in the storage\");\n",
                         ty = a.ty, arm = a.name))
        .collect();
    // packed: an arm aligned more than 1 would be an unaligned reference -
    // say so by name, not as rustc's E0793 at the list
    let packed_asserts: String = arms.iter()
        .filter(|_| tu.packed)
        .map(|a| format!("const _: () = assert!(::core::mem::align_of::<{}>() == 1,\n\
                          \"tagged_union! {name}: arm {} is aligned more than 1, so in a packed \
                          union get() can't hand out a reference to it - pack the arm\");\n",
                         a.ty, a.name))
        .collect();
    // Plain - C's struct of a family is: its pad is every byte of it, and
    // Plain, and the arms are plain data. Not with an enum tag, which isn't
    // any bytes: but new() starts from zeroes, so zero must be one of its
    // variants - a const that isn't fails to evaluate.
    let pad_assert = tu.pad.as_ref()
        .map(|p| {
            let plain = if tu.tag_is_enum {
                format!("// SAFETY: 0 is one of its variants - or this, a const, fails to evaluate\n\
                         const _: {tag_ty} = unsafe {{ ::core::mem::zeroed() }};\n")
            } else {
                format!("// SAFETY: its pad is every byte of it - asserted above - and Plain,\n\
                         // and its arms are plain data, any bytes one\n\
                         unsafe impl {vs}::Plain for {s_ty} {{}}\n")
            };
            format!("const _: () = assert!({s_size} == ::core::mem::size_of::<{p}>(),\n\
                     \"tagged_union! {name}: bigger than its pad\");\n\
                     {plain}\
                     const _: () = {vs}::assert_plain::<{p}>();\n")
        })
        .unwrap_or_default();

    if tu.size == CTaggedSize::Arm {
        // Each value as long as the arm its tag selects - the variant, by
        // value - read off bytes and written back to them: ArmSized, and
        // ArmWalk over a run of them. The tag is the head's, so every arm
        // starts with the head, its tag where the head has it; and every arm
        // is plain data, read and written by copying, at any alignment.
        // No ArmOf: arms may share a type, the variant saying which.
        let head = s_ty.trim();
        let tag_of_raw = wrap_tag("self.tag_raw()");
        let reads: String = arms.iter()
            .map(|a| format!(
                "if t == ({value}) as {tag_ty} {{\n\
                     return {vs}::cast_plain::<{ty}>(b).copied().map(Self::{arm})\n\
                         .ok_or({vs}::ArmError::Bad {{\n\
                             tag: {t_tag},\n\
                             err: {vs}::VStructError::Overrun {{\n\
                                 ty:   ::core::any::type_name::<{ty}>(),\n\
                                 want: ::core::mem::size_of::<{ty}>(),\n\
                                 have: b.len(),\n\
                             }},\n\
                         }});\n\
                 }}\n",
                value = a.value, ty = a.ty, arm = a.name))
            .collect();
        let tags: String = arms.iter()
            .map(|a| format!("Self::{}(_) => ({}) as {tag_ty},\n", a.name, a.value))
            .collect();
        let sizes: String = arms.iter()
            .map(|a| format!("Self::{}(_) => ::core::mem::size_of::<{}>(),\n", a.name, a.ty))
            .collect();
        let arm_bytes: String = arms.iter()
            .map(|a| format!("Self::{}(x) => {vs}::plain_bytes(x),\n", a.name))
            .collect();
        let names: String = arms.iter()
            .map(|a| format!("Self::{0}(_) => \"{0}\",\n", a.name))
            .collect();
        let asserts: String = arms.iter()
            .map(|a| format!(
                "const _: () = {vs}::assert_plain::<{ty}>();\n\
                 const _: () = assert!(::core::mem::size_of::<{ty}>() >= ::core::mem::size_of::<{head}>() &&\n\
                                       ::core::mem::offset_of!({ty}, {tag}) == ::core::mem::offset_of!({head}, {tag}),\n\
                     \"tagged_union! {name}: arm {arm} doesn't start with the head - its {tag} \
                     where the head has it\");\n\
                 const _: () = assert!(::core::mem::align_of::<{ty}>() == 1,\n\
                     \"tagged_union! {name}: arm {arm} has an alignment - it's read where it \
                     is, at any offset: #[repr(C, packed)]\");\n",
                ty = a.ty, arm = a.name))
            .collect();
        return with_defs(defs, name, &format!(
            "{storage}\
             const _: () = {vs}::assert_plain::<{head}>();\n\
             const _: () = assert!(::core::mem::align_of::<{head}>() == 1,\n\
                 \"tagged_union! {name}: its head has an alignment - it's read where it is, \
                 at any offset: #[repr(C, packed)]\");\n\
             {asserts}\
             \n\
             /// One of {name}'s arms, by value, as long as it is - the variant is\n\
             /// the tag: what read() gives, and write() writes, its arm's own\n\
             /// {tag} with it.\n\
             #[allow(non_camel_case_types, dead_code)]\n\
             #[derive(Clone, Copy)]\n\
             {vis} enum {en} {{\n{by_value}}}\n\
             \n\
             impl {en} {{\n\
                 fn tag_raw(&self) -> {tag_ty} {{\n\
                     match self {{\n{tags}}}\n\
                 }}\n\
                 \n\
                 fn arm_bytes(&self) -> &[u8] {{\n\
                     match self {{\n{arm_bytes}}}\n\
                 }}\n\
                 \n\
                 /// Its tag: its variant's.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn {tag}(&self) -> {tag_enum} {{\n\
                     {tag_of_raw}\n\
                 }}\n\
                 \n\
                 /// Its head, which every arm starts with: the fields they share.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn head(&self) -> {head} {{\n\
                     *{vs}::cast_plain(self.arm_bytes()).expect(\"an arm starts with the head\")\n\
                 }}\n\
                 \n\
                 /// Its arm's name.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn name(&self) -> &'static str {{\n\
                     match self {{\n{names}}}\n\
                 }}\n\
                 \n\
                 /// Write it at the start of @out - its arm's bytes, then its tag,\n\
                 /// last, as a sized union's new() marks it - and give back the rest\n\
                 /// of @out. Panics if @out is shorter than it: bytes() sizes @out.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn write<'o>(&self, out: &'o mut [u8]) -> &'o mut [u8] {{\n\
                     let b = self.arm_bytes();\n\
                     let (to, rest) = out.split_at_mut(b.len());\n\
                     to.copy_from_slice(b);\n\
                     {det}::set({vs}::cast_plain_mut(to).expect(\"an arm starts with the head\"),\n\
                                self.tag_raw());\n\
                     rest\n\
                 }}\n\
                 \n\
                 /// The values @b holds, one after another: ArmWalk.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn walk(b: &[u8]) -> {vs}::ArmWalk<'_, Self> {{\n\
                     {vs}::ArmWalk::new(b)\n\
                 }}\n\
             }}\n\
             \n\
             impl {vs}::ArmSized for {en} {{\n\
                 type Tag = {tag_enum};\n\
                 \n\
                 fn read(b: &[u8]) -> Result<Self, {vs}::ArmError<{tag_enum}>> {{\n\
                     let Some(h) = {vs}::cast_plain::<{head}>(b) else {{\n\
                         return Err({vs}::ArmError::Short({vs}::VStructError::Overrun {{\n\
                             ty:   ::core::any::type_name::<{head}>(),\n\
                             want: ::core::mem::size_of::<{head}>(),\n\
                             have: b.len(),\n\
                         }}));\n\
                     }};\n\
                     let t: {tag_ty} = {det}::get(h);\n\
                     {reads}\
                     Err({vs}::ArmError::Unknown({t_tag}))\n\
                 }}\n\
                 \n\
                 fn bytes(&self) -> usize {{\n\
                     match self {{\n{sizes}}}\n\
                 }}\n\
             }}\n"));
    }

    if tu.size == CTaggedSize::Header {
        // An unsized vstruct, its arms running past its header as far as its
        // count says - each reference to one has them: typed() and arm(), on
        // each state's definition of the storage. An arm there may be an
        // older, shorter version of its type: as its ArmRef has it, over the
        // storage's bytes.
        //
        // Every arm has a family - #[unsized_vstruct], #[grown], #[whole] -
        // so each is named in the storage's state: a member of an Unvalidated
        // one is Unvalidated, all the way down.
        let vs = "crate::util::vstructs";
        let by_ref: String = arms.iter()
            .map(|a| format!("    {}(&'a {}<St>),\n", a.name, a.ty.trim()))
            .collect();
        // ArmOf by state - the storage's, X<St>, for the arm's definition in
        // that state, which the arm's X<St> has it by: an arm of an
        // Unvalidated storage is Unvalidated
        let arms_of_by_state: String = states.iter()
            .flat_map(|(st, _)| {
                let (lower, s_ty) = (st.to_lowercase(), &s_ty);
                arms.iter().map(move |a| format!(
                    "impl {arm_of}<{s_ty}<{vs}::{st}>> for {}__{lower} {{\n\
                         type Tag = {tag_ty};\n\
                         const TAG: {tag_ty} = ({}) as {tag_ty};\n\
                     }}\n",
                    a.ty.trim(), a.value))
            })
            .collect();
        let impls: String = states.iter()
            .map(|(st, s)| {
                let det = format!("<{determinant} as {det_trait}<{s}>>");
                let tag_of_self = wrap_tag(&format!("{det}::get(&self.repr)"));
                let selects: String = arms.iter()
                    .map(|a| format!("if t == ({}) as {tag_ty} {{\n\
                                          return {};\n\
                                      }}\n",
                                     a.value,
                                     format!("<{}<{vs}::{st}> as {vs}::ArmRef>::arm_ref(self.as_bytes())\n\
                                                  .map({en}Ref::{}).ok_or({t_tag})",
                                             a.ty.trim(), a.name)))
                    .collect();
                let ref_args = format!("<'_, {vs}::{st}>");
                format!(
                    "impl {s_ty}<{vs}::{st}> {{\n\
                         /// The tag, whether this version knows it or not.\n\
                         #[allow(dead_code)]\n\
                         {vis} fn {tag}(&self) -> {tag_enum} {{\n\
                             {tag_of_self}\n\
                         }}\n\
                         \n\
                         /// The arm its tag selects, over its bytes - as many as its count\n\
                         /// says, as its ArmRef has it: Err(the tag) if it isn't one of\n\
                         /// ours, or the bytes are fewer than any valid one's. One shorter\n\
                         /// than its type is an older version, its missing fields zero.\n\
                         #[allow(dead_code)]\n\
                         {vis} fn typed(&self) -> Result<{en}Ref{ref_args}, {tag_enum}> {{\n\
                             let t: {tag_ty} = {det}::get(&self.repr);\n\
                             {selects}\
                             Err({t_tag})\n\
                         }}\n\
                         \n\
                         /// The arm, if the tag is @A's: typed(), for one arm.\n\
                         #[allow(dead_code)]\n\
                         {vis} fn arm<A>(&self) -> Option<&A>\n\
                         where\n\
                             A: {arm_of}<Self, Tag = {tag_ty}> + {vs}::ArmRef + ?Sized,\n\
                         {{\n\
                             if {det}::get(&self.repr) != A::TAG {{\n\
                                 return None;\n\
                             }}\n\
                             A::arm_ref(self.as_bytes())\n\
                         }}\n\
                         \n\
                         /// arm(), to edit: as its ArmRef has it.\n\
                         #[allow(dead_code)]\n\
                         {vis} fn arm_mut<A>(&mut self) -> Option<&mut A>\n\
                         where\n\
                             A: {arm_of}<Self, Tag = {tag_ty}> + {vs}::ArmRef + ?Sized,\n\
                         {{\n\
                             if {det}::get(&self.repr) != A::TAG {{\n\
                                 return None;\n\
                             }}\n\
                             A::arm_mut(self.as_bytes_mut())\n\
                         }}\n\
                     }}\n\
                     \n")
            })
            .collect();
        let match_arms: String = arms.iter()
            .map(|a| format!("crate::c::{en}Ref::{}($a) => {{ $(type $A<S> = {}<S>;)? $body }}\n",
                             a.name, arm_path(&a.ty)))
            .collect();
        let tag_arms: String = arms.iter()
            .map(|a| format!("if t == ({}) as {tag_ty} {{ type $A<S> = {}<S>; $body }} else ",
                             a.value, arm_path(&a.ty)))
            .collect();
        let dispatch = format!(
            "#[allow(unused_macros)]\n\
             macro_rules! {name}_match {{\n\
                 ($r:expr, $a:ident $(: $A:ident)? => $body:expr) => {{\n\
                     match $r {{\n{match_arms}}}\n\
                 }};\n\
             }}\n\
             #[allow(unused_imports)]\n\
             pub(crate) use {name}_match;\n\
             \n\
             #[allow(unused_macros)]\n\
             macro_rules! {name}_by_tag {{\n\
                 ($t:expr, $A:ident => $body:expr, _ => $other:expr) => {{{{\n\
                     let t: {tag_ty} = $t;\n\
                     {tag_arms}{{ $other }}\n\
                 }}}};\n\
             }}\n\
             #[allow(unused_imports)]\n\
             pub(crate) use {name}_by_tag;\n");
        return with_defs(defs, name, &format!(
            "{storage}\
             {dispatch}\
             /// One of {name}'s arms, with the bytes its header says are its - as\n\
             /// many as there are, for one written before its type grew: what\n\
             /// {name}'s typed() gives.\n\
             #[allow(non_camel_case_types, dead_code)]\n\
             #[derive(Clone, Copy)]\n\
             {vis} enum {en}Ref<'a, St: {vs}::State = {vs}::Validated> {{\n{by_ref}}}\n\
             \n\
             {impls}\
             {arms_of_by_state}"));
    }

    let tag_of_storage = wrap_tag(&format!("{det}::get(&self.0)"));
    let ref_doc = if stated {
        format!("/// One of {name}'s arms, by reference, in its state: what {name}::get()\n\
                 /// gives - an arm that's another's type as long as its own header says.\n")
    } else {
        format!("/// One of {name}'s arms, by reference: what {name}::get() gives.\n")
    };
    // Bytes from outside: not with an enum tag, which they could hold a value
    // of that isn't one of its variants
    let unvalidated_impl = if tu.tag_is_enum {
        String::new()
    } else {
        format!(
            "impl {name}<{vs}::Unvalidated> {{\n\
                 /// From its bytes, as C and disk have them.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn from_bytes(b: &[u8; {s_size}]) -> Self {{\n\
                     // SAFETY: the storage is plain data: any bytes are a valid {name}\n\
                     Self(unsafe {{ b.as_ptr().cast::<{s_ty}>().read_unaligned() }}, ::core::marker::PhantomData)\n\
                 }}\n\
                 \n\
                 /// The arm the tag selects: Err if the tag isn't one of ours, or the\n\
                 /// arm's header says it's longer than the storage holds.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn get(&self) -> Result<{en}Ref<'_{unvalidated}>, {vs}::ArmError<{tag_enum}>> {{\n\
                     let t: {tag_ty} = {det}::get(&self.0);\n\
                     let b = &self.as_bytes()[{payload_offset}..];\n\
                     {gets_unvalidated}\
                     Err({vs}::ArmError::Unknown({t_tag}))\n\
                 }}\n\
                 \n\
                 /// Validated, if its arm fits in the storage: get()'s check, kept. A\n\
                 /// tag this version doesn't know passes - a Validated get() says so.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn validate(self) -> Result<{name}, {vs}::ArmError<{tag_enum}>> {{\n\
                     self.validated().copied()\n\
                 }}\n\
                 \n\
                 /// validate(), by reference: the same bytes, Validated.\n\
                 #[allow(dead_code)]\n\
                 {vis} fn validated(&self) -> Result<&{name}, {vs}::ArmError<{tag_enum}>> {{\n\
                     match self.get() {{\n\
                         Err(e @ ({vs}::ArmError::Bad {{ .. }} | {vs}::ArmError::Short(_))) => Err(e),\n\
                         _ => Ok(unsafe {{ &*::core::ptr::from_ref(self).cast::<{name}>() }}),\n\
                     }}\n\
                 }}\n\
             }}\n\
             \n",
            unvalidated = st_arg("Unvalidated"),
            gets_unvalidated = gets("Unvalidated",
                &format!("return Err({vs}::ArmError::Bad {{ tag: {t_tag}, err }})")))
    };
    with_defs(defs, name, &format!(
        "{storage}\
         \n\
         /// {name}, in state S - Validated unless named: over C's struct, {s_ty}.\n\
         /// Unvalidated, bytes from outside - from the kernel, off a device - each\n\
         /// arm checked against the storage as get() hands it out; Validated,\n\
         /// what validate passed, or what's made here from an arm.\n\
         #[allow(non_camel_case_types)]\n\
         #[repr(transparent)]\n\
         {vis} struct {name}<S = {vs}::Validated>({s_ty}, ::core::marker::PhantomData<S>);\n\
         \n\
         impl<S> Clone for {name}<S> {{\n\
             fn clone(&self) -> Self {{\n\
                 *self\n\
             }}\n\
         }}\n\
         \n\
         impl<S> Copy for {name}<S> {{}}\n\
         \n\
         /// One of {name}'s arms, by value: what {name}::new() takes.\n\
         #[allow(non_camel_case_types, dead_code)]\n\
         {vis} enum {ev} {{\n{by_value}}}\n\
         \n\
         {ref_doc}\
         #[allow(non_camel_case_types, dead_code)]\n\
         #[derive(Clone, Copy)]\n\
         {vis} enum {en}Ref<'a{st_param}> {{\n{by_ref}}}\n\
         \n\
         impl<'a{st_impl}> {en}Ref<'a{st_use}> {{\n\
             /// The bytes it takes, from {name}::ARMS_AT - a tailed arm's, as\n\
             /// many as its header says.\n\
             #[allow(dead_code)]\n\
             {vis} fn len(&self) -> usize {{\n\
                 match *self {{\n{lens}}}\n\
             }}\n\
         }}\n\
         \n\
         impl<S> {name}<S> {{\n\
             /// The tag, whether this version knows it or not.\n\
             #[allow(dead_code)]\n\
             {vis} fn {tag}(&self) -> {tag_enum} {{\n\
                 {tag_of_storage}\n\
             }}\n\
             \n\
             /// Its bytes, as C and disk have them.\n\
             #[allow(dead_code)]\n\
             {vis} fn as_bytes(&self) -> &[u8; {s_size}] {{\n\
                 // SAFETY: plain data, every byte of it written: it came from C or disk,\n\
                 // from_bytes() or new(), which starts from zeroes\n\
                 unsafe {{ &*(self as *const Self).cast::<[u8; {s_size}]>() }}\n\
             }}\n\
             \n\
             /// For C.\n\
             #[allow(dead_code)]\n\
             {vis} fn as_ffi(&self) -> *const {s_ty} {{\n\
                 &self.0\n\
             }}\n\
             \n\
             /// For C, to edit.\n\
             #[allow(dead_code)]\n\
             {vis} fn as_ffi_mut(&mut self) -> *mut {s_ty} {{\n\
                 &mut self.0\n\
             }}\n\
         }}\n\
         \n\
         {unvalidated_impl}\
         impl {name}<{vs}::Validated> {{\n\
             /// Where its arms start, in any state: after the tag.\n\
             #[allow(dead_code)]\n\
             {vis} const ARMS_AT: usize = {payload_offset};\n\
             \n\
             /// The arm the tag selects - Err(the tag) if it isn't one of ours.\n\
             /// Validated, every arm fits in the storage.\n\
             #[allow(dead_code)]\n\
             {vis} fn get(&self) -> Result<{en}Ref<'_{validated}>, {tag_enum}> {{\n\
                 let t: {tag_ty} = {det}::get(&self.0);\n\
                 let b = &self.as_bytes()[{payload_offset}..];\n\
                 {gets_validated}\
                 Err({t_tag})\n\
             }}\n\
             \n\
             /// From an arm: new(), by the arm's type.\n\
             #[allow(dead_code)]\n\
             {vis} fn from_arm<A: {arm_of}<{s_ty}, Tag = {tag_ty}>>(arm: A) -> Self {{\n\
                 let mut s = Self::with_payload(arm);\n\
                 {det}::set(&mut s, A::TAG);\n\
                 Self(s, ::core::marker::PhantomData)\n\
             }}\n\
             \n\
             /// From an arm ending in a flexible array, and that array - as many\n\
             /// elements as the arm's header says: an error if the storage can't\n\
             /// hold them.\n\
             #[allow(dead_code)]\n\
             {vis} fn from_arm_tailed<A>(arm: A, tail: &[A::Elem]) -> Result<Self, {vs}::VStructError>\n\
             where A: {arm_of}<{s_ty}, Tag = {tag_ty}> + {vs}::FlexArray\n\
             {{\n\
                 assert_eq!(arm.nr(), tail.len(), \"{{}}: its header's count, and the elements given\",\n\
                            ::core::any::type_name::<A>());\n\
                 let at = {payload_offset} + A::TAIL;\n\
                 let len = ::core::mem::size_of_val(tail);\n\
                 if at + len > {s_size} {{\n\
                     return Err({vs}::VStructError::Overrun {{ ty: ::core::any::type_name::<A>(),\n\
                                                               want: A::TAIL + len,\n\
                                                               have: {s_size} - {payload_offset} }});\n\
                 }}\n\
                 let mut s = Self::with_payload(arm);\n\
                 // SAFETY: in bounds, as checked above, and the elements are plain data\n\
                 unsafe {{\n\
                     ::core::ptr::copy_nonoverlapping(tail.as_ptr().cast::<u8>(),\n\
                         (&mut s as *mut {s_ty}).cast::<u8>().add(at), len);\n\
                 }}\n\
                 {det}::set(&mut s, A::TAG);\n\
                 Ok(Self(s, ::core::marker::PhantomData))\n\
             }}\n\
             \n\
             /// From an arm: its payload into zeroed storage, then the determinant marks\n\
             /// it - stabby's order, so a tag that's bits of the payload is written last.\n\
             #[allow(dead_code)]\n\
             {vis} fn new(v: {ev}) -> Self {{\n\
                 match v {{\n{news}}}\n\
             }}\n\
             \n\
             /// Zeroed storage holding @arm's payload, not yet marked as holding it.\n\
             fn with_payload<A: {arm_of}<{s_ty}>>(arm: A) -> {s_ty} {{\n\
                 // SAFETY: zeroes are a valid {name}: plain data, or its tag an enum\n\
                 // with a zero variant - asserted below\n\
                 let mut s: {s_ty} = unsafe {{ ::core::mem::zeroed() }};\n\
                 // SAFETY: only arms are ArmOf, and each is plain data, in bounds - asserted\n\
                 // below\n\
                 unsafe {{\n\
                     (&mut s as *mut {s_ty}).cast::<u8>().add({payload_offset}).cast::<A>()\n\
                         .write_unaligned(arm);\n\
                 }}\n\
                 s\n\
             }}\n\
         }}\n\
         \n\
         {arms_of}\
         {pad_assert}\
         {arm_asserts}\
         {packed_asserts}",
        validated = st_arg("Validated"),
        gets_validated = gets("Validated",
            &format!("panic!(\"{name}<Validated>: {{err}} - an arm that doesn't fit, in one \\
                      validate passed or made from an arm\")"))))
}

/// @code, after @defs: the arm types defined in the block, then the rest.
fn with_defs(mut defs: TokenStream, name: &str, code: &str) -> Result<TokenStream, String> {
    let rest = code.parse::<TokenStream>()
        .map_err(|e| format!("{name}: generated code doesn't parse: {e}"))?;
    defs.extend(rest);
    Ok(defs)
}
