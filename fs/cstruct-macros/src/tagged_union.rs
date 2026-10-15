// SPDX-License-Identifier: GPL-2.0
//! tagged_union!: a tagged union with a stable representation.
//!
//!   tagged_union! {
//!       /// ...
//!       pub struct disk_accounting_pos {
//!           tag type_: u8 = disk_accounting_type,
//!           arms from BCH_DISK_ACCOUNTING_TYPES(f, nr, ..) => f: bch_acct_ ## f = nr,
//!           pad: c::bpos,
//!           size: pad,
//!           packed,
//!       }
//!   }
//!
//!   tagged_union! {
//!       pub union jset_entry in c::jset_entry {
//!           tag type_: u8 = c::bch_jset_entry_type,
//!           arm btree_keys: c::jset_entry = ..,
//!           ...
//!           size: header,
//!       }
//!   }
//!
//! A type whose bytes come from outside - disk, or C - with a tag that says
//! which of its arms it holds, and that may be one this version doesn't know.
//! The layout is given, not chosen: the storage is the C, plain #[repr(C)]
//! data - a `struct` is the tag, then a union of the arms, overlaid with the
//! pad.
//!
//! `size:` says what its size is a function of, and what's generated follows
//! from that: nothing, a constant - `size: pad`, a struct, every value the
//! pad's size, a value of plain data; its header's count - `size: header`,
//! a union over another's storage, below, a view of bytes as long as the
//! header says; or its tag - `size: arm`, a value as long as the arm its tag
//! selects, below.
//!
//! `packed` is C's __packed: for an on-disk struct, the payload right after
//! the tag, whatever the arms' alignment - a struct of the tag and the union
//! otherwise puts the union at its alignment, and a new arm could move every
//! payload.
//!
//! The tag is a function of the whole value: a crate::types::Determinant,
//! stabby's idea (docs.rs/stabby), n-way - generated, returning the tag
//! field, `tag NAME`. It's handed out as TAG_ENUM, `tag NAME: TYPE =
//! TAG_ENUM`, a newtype over an integer the tag widens to - or with none,
//! `tag NAME: TYPE`, as it is.
//!
//! Generated, for the first declaration:
//!
//!  - `union disk_accounting_pos__ffi`: the storage, what C and disk see -
//!    C's struct disk_accounting_pos, Ffi<disk_accounting_pos>; Plain, its
//!    pad being every byte of it;
//!  - `disk_accounting_pos<S>`, the storage in a state, Validated unless
//!    named, as a family's types are (util/vstructs.rs): Unvalidated, bytes
//!    from outside - `from_bytes()`; Validated, what validate passed, or
//!    what's made here from an arm. One generic newtype, not a definition
//!    per state: the states differ only in what get() does with an arm
//!    that doesn't fit.
//!  - `enum DiskAccountingPos`, an arm by value, and `enum
//!    DiskAccountingPosRef<'_, St>`, an arm by reference: one of the arm
//!    types the block declares, plain data, a reference to it; another's -
//!    the replicas entry, a header and the devices it counts - its
//!    family's, NAME__family, in the storage's state, as long as its own
//!    header says (ArmAt);
//!  - `get()`: the arm the tag selects. Unvalidated, an error if the tag
//!    isn't one of ours or the arm's header says it's longer than the
//!    storage holds (ArmError); Validated, Err(the tag), and an arm that
//!    doesn't fit a bug. Shared references only: through a `&mut` to an
//!    arm, a tag in the arm's own bits could change under whoever holds it.
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
//! The storage's bytes are always all written: they come from C or disk, from
//! from_bytes(), or from new(), which starts from zeroes - so get() and
//! as_bytes() can read any of them. A literal of the storage could leave
//! some unwritten, as could an arm with padding; nothing makes either.
//!
//! Or, `union NAME in STORAGE { .., size: header }`, the storage is
//! another's: a header its arms embed and run past, as far as its count
//! says - a journal entry's, a superblock field's: an unsized vstruct
//! (util/vstructs.rs), each reference to which has its arm's bytes, and its
//! own C. Then what's generated is the Ref enum, ArmOf, and on each state's
//! definition of the storage - X__unvalidated, X__validated - typed(): the
//! arm its tag selects, over its bytes, in the storage's state - every arm
//! type has a family (util/vstructs.rs), so an Unvalidated storage's arms are
//! Unvalidated. An arm may be shorter than its type - written by an older
//! version, before the type grew - so it's what its definition's ArmRef says:
//! a grown type's at the arm's length, its later fields zero
//! past the end. An arm that's the storage itself - a journal entry of keys -
//! is the storage, in its state. arm::<A>() is typed() for one arm, if the
//! tag is A's - A in the storage's state, X<Unvalidated> for an Unvalidated
//! one - and arm_mut::<A>() the same, to edit.
//!
//! Or, `union NAME in HEAD { .., size: arm }`, each value is as long as the
//! arm its tag selects - an ACL's entries: no count to step by, so a tag this
//! version doesn't know is one there's no stepping past. HEAD is what every
//! arm starts with, the tag in it, and the arms are plain data, packed: an
//! on-disk type asks no alignment of where it is, so a value is read at any
//! offset. Arms may share a type, the variant saying which. What's generated
//! is the enum, the arm by value, and on it read(),
//! from bytes, an error if the tag isn't ours or the bytes are fewer than
//! its arm (crate::util::vstructs::ArmSized); bytes(), its arm's size;
//! head(), the fields every arm shares; name(), its arm's; write(), its
//! arm's bytes then its tag; walk(), over a run of them (ArmWalk). No C:
//! HEAD and the arms have their own.
//!
//! The arms come from an x-macro list: tagged_union! hands the declaration to
//! it - LIST!(cstruct_macros::__tagged_union [decl]) - and the list calls
//! back with its entries, each bound to the params, `..` the rest, and
//! `A ## B` pasting - spaced, as Rust 2021 lexes `a##b` as a reserved prefix.
//!
//! The arms' types can be defined in the block, each next to the others, as
//! a Rust enum's variants are - comments and all:
//!
//!   tagged_union! {
//!       #[derive(Clone, Copy, CStruct, TypeInfo)]*
//!       #[c_typedef]*
//!       pub struct disk_accounting_pos {
//!           tag type_: u8 = disk_accounting_type,
//!           arms from BCH_DISK_ACCOUNTING_TYPES(f, nr, ..) => f: bch_acct_ ## f = nr,
//!           ...
//!           /* Metadata accounting per btree id: ... */
//!           #[repr(C, packed)]
//!           pub struct bch_acct_btree {
//!               pub id: u32,
//!           },
//!       }
//!   }
//!
//! Each goes to nestify::nest!, so it can nest types of its own, with the
//! declaration's `#[...]*` - nestify's propagating attributes, for each arm
//! type and what it nests, not for the storage. The list still says what the
//! arms are; a type defined here that isn't one's is an error. The
//! declaration reaches the list as tokens, so an error in a definition is
//! reported where it's written.

use proc_macro::{Delimiter, Group, TokenStream, TokenTree};

use crate::cstruct::{parse_items, tagged_union_arms, tagged_union_record_text, CItem, CTaggedArm,
                     CTaggedSize, CTaggedUnion};
use crate::{comma_list, list_callback, record, through_list, tokens};

/// tagged_union!: the arms from a list go through the list; written-out
/// arms are generated directly. The declaration goes to the list as tokens,
/// not text: an arm type defined in it keeps its spans, so an error in one
/// is reported where it is.
pub(crate) fn tagged_union(input: TokenStream) -> Result<TokenStream, String> {
    let tu = parse(&input)?;
    match &tu.from {
        Some(f) => Ok(through_list(&f.list, "cstruct_macros::__tagged_union", input)),
        None => generate(&input, &tu, &tu.arms),
    }
}

/// The declaration's `#[...]*`s, and the arm types defined in its block -
/// as the tokens they were written as, spans and all.
fn arm_attrs_and_defs(decl: &TokenStream) -> (TokenStream, Vec<TokenStream>) {
    let trees: Vec<TokenTree> = decl.clone().into_iter().collect();
    let mut attrs = TokenStream::new();
    let mut k = 0;
    while let (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g))) = (trees.get(k), trees.get(k + 1)) {
        if p.as_char() != '#' || g.delimiter() != Delimiter::Bracket {
            break;
        }
        if let Some(TokenTree::Punct(star)) = trees.get(k + 2).filter(|t| matches!(t, TokenTree::Punct(s) if s.as_char() == '*')) {
            attrs.extend(trees[k..k + 2].iter().cloned());
            attrs.extend([TokenTree::Punct(star.clone())]);
            k += 3;
        } else {
            k += 2;
        }
    }
    let body = trees.iter().rev().find_map(|t| match t {
        TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g.stream()),
        _ => None,
    });
    let items = body.map(comma_list).unwrap_or_default();
    // [#[..]]... [pub [(..)]] struct|union NAME { .. }
    let is_def = |item: &[TokenTree]| {
        let mut j = 0;
        while matches!(item.get(j), Some(TokenTree::Punct(p)) if p.as_char() == '#') {
            j += 2;
        }
        if matches!(item.get(j), Some(TokenTree::Ident(i)) if i.to_string() == "pub") {
            j += 1;
            if matches!(item.get(j), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis) {
                j += 1;
            }
        }
        matches!((item.get(j), item.get(j + 1), item.get(j + 2)),
                 (Some(TokenTree::Ident(kw)), Some(TokenTree::Ident(_)), Some(TokenTree::Group(g)))
                 if (kw.to_string() == "struct" || kw.to_string() == "union")
                    && g.delimiter() == Delimiter::Brace)
    };
    let defs = items.into_iter()
        .filter(|item| is_def(item))
        .map(|item| item.into_iter().collect())
        .collect();
    (attrs, defs)
}

/// The list's callback: its arms are the list's entries.
pub(crate) fn from_list(input: TokenStream) -> Result<TokenStream, String> {
    let (decl, entries) = list_callback(input)?;
    let tu = parse(&decl)?;
    let arms = tagged_union_arms(&tu, &entries).map_err(|e| format!("{}: {e}", tu.name))?;
    generate(&decl, &tu, &arms)
}

fn parse(input: &TokenStream) -> Result<CTaggedUnion, String> {
    let mut items = parse_items(&format!("tagged_union! {{ {input} }}"))?;
    match (items.pop(), items.is_empty()) {
        (Some(CItem::TaggedUnion(tu)), true) => Ok(tu),
        _ => Err("expected struct NAME { tag ..., arms ..., pad ... }".into()),
    }
}

/// The declaration's attributes - its doc comment, a cfg - and visibility, as
/// source, for what's generated.
fn attrs_and_vis(decl: &TokenStream) -> (String, String) {
    let trees: Vec<TokenTree> = decl.clone().into_iter().collect();
    let mut attrs = String::new();
    let mut k = 0;
    while let (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g))) = (trees.get(k), trees.get(k + 1)) {
        if p.as_char() != '#' || g.delimiter() != Delimiter::Bracket {
            break;
        }
        k += 2;
        // `#[...]*` is the arm types', not the storage's
        if let Some(TokenTree::Punct(star)) = trees.get(k) {
            if star.as_char() == '*' {
                k += 1;
                continue;
            }
        }
        attrs.push_str(&format!("#{g}\n"));
    }
    let mut vis = String::new();
    if let Some(TokenTree::Ident(i)) = trees.get(k) {
        if i.to_string() == "pub" {
            vis.push_str("pub");
            if let Some(TokenTree::Group(g)) = trees.get(k + 1) {
                if g.delimiter() == Delimiter::Parenthesis {
                    vis.push_str(&g.to_string());
                }
            }
        }
    }
    (attrs, vis)
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

fn generate(decl: &TokenStream, tu: &CTaggedUnion, arms: &[CTaggedArm]) -> Result<TokenStream, String> {
    if arms.is_empty() {
        return Err(format!("{}: no arms", tu.name));
    }
    if let Some(d) = tu.defs.iter().find(|d| !arms.iter().any(|a| a.ty.trim() == d.name)) {
        return Err(format!("{}: {} is defined here but isn't an arm's type", tu.name, d.name));
    }
    // The arm types defined in the block: nestify::nest!, given the
    // declaration's `#[...]*` - propagating, for any types nested in them -
    // as the tokens they were written as.
    let (arm_attrs, def_tokens) = arm_attrs_and_defs(decl);
    let mut defs = TokenStream::new();
    for d in def_tokens {
        let mut body = arm_attrs.clone();
        body.extend(d);
        defs.extend(tokens("::nestify::nest!"));
        defs.extend([TokenTree::Group(Group::new(Delimiter::Brace, body))]);
    }
    let (attrs, vis) = attrs_and_vis(decl);
    let name = &tu.name;
    let en = camel(name);
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
    // definition, named - an impl for the alias X<St> would be for a
    // projection (util/vstructs.rs)
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
                  {attrs}#[c_name({name})]\n{storage_attrs}{vis} union {s_ty} {{\n\
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
    // the storage's state. Another's is a family's - a replicas entry - in the
    // storage's state, as long as its own header says (ArmAt).
    let declared = |a: &CTaggedArm| tu.defs.iter().any(|d| d.name == a.ty.trim());
    let stated = arms.iter().any(|a| !declared(a));
    let st_param = if stated { format!(", St: {vs}::State = {vs}::Validated") } else { String::new() };
    let st_arg = |st: &str| if stated { format!(", {vs}::{st}") } else { String::new() };
    let by_ref: String = arms.iter()
        .map(|a| if declared(a) {
            format!("    {}(&'a {}),\n", a.name, a.ty)
        } else {
            format!("    {}(<<St as {vs}::State>::Of<{}__family> as {vs}::ArmRef>::Ref<'a>),\n",
                    a.name, a.ty.trim())
        })
        .collect();
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
                    format!("match <<{vs}::{st} as {vs}::State>::Of<{}__family> as {vs}::ArmAt>::arm_at(b) {{\n\
                                 Ok(arm) => arm,\n\
                                 Err(err) => {bad},\n\
                             }}", a.ty.trim())
                };
                format!("if t == ({}) as {tag_ty} {{\n\
                             return Ok({en}Ref::{}({arm}));\n\
                         }}\n", a.value, a.name)
            })
            .collect()
    };
    let news: String = arms.iter()
        .map(|a| format!("{en}::{}(x) => Self::from_arm(x),\n", a.name))
        .collect();
    // An arm that's the storage itself - a journal entry of keys - isn't a
    // type of its own to go by
    let arms_of: String = arms.iter()
        .filter(|a| a.ty.trim() != s_ty.trim())
        .map(|a| format!("impl {arm_of}<{s_ty}> for {} {{\n\
                              type Tag = {tag_ty};\n\
                              const TAG: {tag_ty} = ({}) as {tag_ty};\n\
                          }}\n",
                         a.ty, a.value))
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
    // Plain, and the arms are plain data
    let pad_assert = tu.pad.as_ref()
        .map(|p| format!("const _: () = assert!({s_size} == ::core::mem::size_of::<{p}>(),\n\
                          \"tagged_union! {name}: bigger than its pad\");\n\
                          // SAFETY: its pad is every byte of it - asserted above - and Plain,\n\
                          // and its arms are plain data, any bytes one\n\
                          unsafe impl {vs}::Plain for {s_ty} {{}}\n\
                          const _: () = {vs}::assert_plain::<{p}>();\n"))
        .unwrap_or_default();
    // The C: another's storage has its own
    let c_record = if tu.storage.is_none() {
        let nums = [
            format!("::core::mem::size_of::<{s_ty}>() as u64"),
            format!("::core::mem::align_of::<{s_ty}>() as u64"),
            format!("{payload_offset} as u64"),
        ];
        record(&tagged_union_record_text(tu, arms), &nums)
    } else {
        String::new()
    };

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
        // storage's bytes. Or the storage itself - a journal entry of keys -
        // in its state.
        //
        // Every arm has a family - #[unsized_vstruct], #[grown], #[whole] -
        // so each is named in the storage's state: a member of an Unvalidated
        // one is Unvalidated, all the way down.
        let vs = "crate::util::vstructs";
        let own = |a: &CTaggedArm| a.ty.trim() == s_ty.trim();
        let by_ref: String = arms.iter()
            .map(|a| if own(a) {
                format!("    {}(&'a {s_ty}<St>),\n", a.name)
            } else {
                format!("    {}(<<St as {vs}::State>::Of<{}__family> as {vs}::ArmRef>::Ref<'a>),\n",
                        a.name, a.ty.trim())
            })
            .collect();
        let det = format!("<{determinant} as {det_trait}<Self>>");
        let tag_of_self = wrap_tag(&format!("{det}::get(self)"));
        // ArmOf by each state's definitions, named - the storage's and the
        // arm's: an arm of an Unvalidated storage is its Unvalidated definition
        let arms_of_by_state: String = states.iter()
            .flat_map(|(st, s)| {
                let st = st.to_lowercase();
                arms.iter().filter(|a| !own(a)).map(move |a| format!(
                    "impl {arm_of}<{s}> for {}__{st} {{\n\
                         type Tag = {tag_ty};\n\
                         const TAG: {tag_ty} = ({}) as {tag_ty};\n\
                     }}\n",
                    a.ty.trim(), a.value))
            })
            .collect();
        let impls: String = states.iter()
            .map(|(st, s)| {
                let lower = st.to_lowercase();
                let selects: String = arms.iter()
                    .map(|a| format!("if t == ({}) as {tag_ty} {{\n\
                                          return {};\n\
                                      }}\n",
                                     a.value,
                                     if own(a) {
                                         format!("Ok({en}Ref::{}(self))", a.name)
                                     } else {
                                         format!("<{}__{lower} as {vs}::ArmRef>::arm_ref(self.as_bytes())\n\
                                                      .map({en}Ref::{}).ok_or({t_tag})",
                                                 a.ty.trim(), a.name)
                                     }))
                    .collect();
                let ref_args = format!("<'_, {vs}::{st}>");
                format!(
                    "impl {s} {{\n\
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
                             let t: {tag_ty} = {det}::get(self);\n\
                             {selects}\
                             Err({t_tag})\n\
                         }}\n\
                         \n\
                         /// The arm, if the tag is @A's: typed(), for one arm.\n\
                         #[allow(dead_code)]\n\
                         {vis} fn arm<A>(&self) -> Option<A::Ref<'_>>\n\
                         where\n\
                             A: {arm_of}<{s}, Tag = {tag_ty}> + {vs}::ArmRef + ?Sized,\n\
                         {{\n\
                             if {det}::get(self) != A::TAG {{\n\
                                 return None;\n\
                             }}\n\
                             A::arm_ref(self.as_bytes())\n\
                         }}\n\
                         \n\
                         /// arm(), to edit: as its ArmRef has it.\n\
                         #[allow(dead_code)]\n\
                         {vis} fn arm_mut<A>(&mut self) -> Option<A::Mut<'_>>\n\
                         where\n\
                             A: {arm_of}<{s}, Tag = {tag_ty}> + {vs}::ArmRef + ?Sized,\n\
                         {{\n\
                             if {det}::get(self) != A::TAG {{\n\
                                 return None;\n\
                             }}\n\
                             A::arm_mut(self.as_bytes_mut())\n\
                         }}\n\
                     }}\n\
                     \n")
            })
            .collect();
        return with_defs(defs, name, &format!(
            "{storage}\
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
    with_defs(defs, name, &format!(
        "{storage}\
         \n\
         /// {name}, in state S - Validated unless named: C's struct, Ffi<{name}>.\n\
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
         impl<S> {vs}::HasFfi for {name}<S> {{\n\
             type Ffi = {s_ty};\n\
         }}\n\
         \n\
         /// One of {name}'s arms, by value: what {name}::new() takes.\n\
         #[allow(non_camel_case_types, dead_code)]\n\
         {vis} enum {en} {{\n{by_value}}}\n\
         \n\
         {ref_doc}\
         #[allow(non_camel_case_types, dead_code)]\n\
         #[derive(Clone, Copy)]\n\
         {vis} enum {en}Ref<'a{st_param}> {{\n{by_ref}}}\n\
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
         impl {name}<{vs}::Unvalidated> {{\n\
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
         }}\n\
         \n\
         impl {name}<{vs}::Validated> {{\n\
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
             {vis} fn new(v: {en}) -> Self {{\n\
                 match v {{\n{news}}}\n\
             }}\n\
             \n\
             /// Zeroed storage holding @arm's payload, not yet marked as holding it.\n\
             fn with_payload<A: {arm_of}<{s_ty}>>(arm: A) -> {s_ty} {{\n\
                 // SAFETY: the storage is plain data: any bytes are a valid {name}\n\
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
         {packed_asserts}\
         {c_record}",
        unvalidated = st_arg("Unvalidated"),
        validated = st_arg("Validated"),
        gets_unvalidated = gets("Unvalidated",
            &format!("return Err({vs}::ArmError::Bad {{ tag: {t_tag}, err }})")),
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
