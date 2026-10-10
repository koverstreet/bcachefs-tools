// SPDX-License-Identifier: GPL-2.0
//! tagged_union!: a tagged union with a stable representation.
//!
//!   tagged_union! {
//!       /// ...
//!       pub struct disk_accounting_pos {
//!           tag type_: u8 = disk_accounting_type,
//!           arms from BCH_DISK_ACCOUNTING_TYPES(f, nr, ..) => f: bch_acct_ ## f = nr,
//!           pad: c::bpos,
//!           packed,
//!       }
//!   }
//!
//!   tagged_union! {
//!       pub union bch_extent_entry {
//!           tag type_: u32 = bch_extent_entry_type by ExtentEntryType,
//!           arms from BCH_EXTENT_ENTRY_TYPES(f, n) => f: bch_extent_ ## f = n,
//!       }
//!   }
//!
//! A type whose bytes come from outside - disk, or C - with a tag that says
//! which of its arms it holds, and that may be one this version doesn't know.
//! The layout is given, not chosen: the storage is the C, plain #[repr(C)]
//! data - a `struct` is the tag, then a union of the arms, and a `union` is
//! the arms alone. Either is overlaid with the pad, if any. `packed` is C's
//! __packed: for an on-disk struct, the payload right after the tag, whatever
//! the arms' alignment - a struct of the tag and the union otherwise puts the
//! union at its alignment, and a new arm could move every payload.
//!
//! The tag is a function of the whole value: a crate::types::Determinant,
//! stabby's idea (docs.rs/stabby), n-way. A struct's is generated, and
//! returns the field; a union's is written by hand and named with `by` -
//! an extent entry's tag is the lowest set bit of its first word, bits each
//! arm's own type field sets.
//!
//! Generated, for the first declaration:
//!
//!  - `union disk_accounting_pos`: the storage, what C and disk see;
//!  - `enum DiskAccountingPos`, an arm by value, and `enum
//!    DiskAccountingPosRef<'_>`, an arm with the bytes it may extend into -
//!    a crate::util::vstructs::Flex, so an arm ending in a flexible array,
//!    replicas' devs, has its tail checked against the storage's end;
//!  - `get()`: the arm the tag selects, by reference - Err(the tag) if it
//!    isn't one of ours. Shared references only: through a `&mut` to an arm,
//!    a tag in the arm's own bits could change under whoever holds it.
//!  - `new()`, and `from_arm()` by the arm's type: the payload into zeroed
//!    storage, then Determinant::set() marks it - stabby's order, so a tag
//!    that's bits of the payload is the last thing written, not overwritten
//!    by it. Never a typed copy, which leaves the bytes past a short payload
//!    undefined. `from_arm_tailed()` writes an arm's flexible array too.
//!  - crate::types::ArmOf for each arm's type, which from_arm() goes by;
//!  - `type_()`, the tag, known or not; `from_bytes()` and `as_bytes()`;
//!  - C: the same storage, its offsets asserted against the Rust's.
//!
//! The storage's bytes are always all written: they come from C or disk, from
//! from_bytes(), or from new(), which starts from zeroes - so get() and
//! as_bytes() can read any of them. A literal of the storage could leave
//! some unwritten, as could an arm with padding; nothing makes either.
//!
//! Or the storage is another's - `pub struct disk_accounting_pos in
//! c::disk_accounting_pos`, bindgen's while C defines it - and what's
//! generated is only the view of it: no storage and no C, the arms' types
//! another's too. Without the storage's definition, the arms are found where
//! a `packed` struct has them, right after the tag, or for a union where it
//! has them, at its start; the asserts check each fits there.
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

use proc_macro::{Delimiter, Group, Punct, Spacing, TokenStream, TokenTree};

use crate::cstruct::{parse_items, tagged_union_arms, tagged_union_record_text, CItem, CTaggedArm,
                     CTaggedUnion};
use crate::{compile_error, record};

/// tagged_union!: the arms from a list go through the list; written-out
/// arms are generated directly. The declaration goes to the list as tokens,
/// not text: an arm type defined in it keeps its spans, so an error in one
/// is reported where it is.
pub fn tagged_union(input: TokenStream) -> TokenStream {
    let tu = match parse(&input) {
        Ok(tu) => tu,
        Err(e) => return compile_error(&format!("tagged_union!: {e}")),
    };
    match &tu.from {
        Some((list, _, _)) => {
            let mut callback: TokenStream = "cstruct_macros::__tagged_union".parse().unwrap();
            callback.extend([TokenTree::Group(Group::new(Delimiter::Bracket, input))]);
            let mut out: TokenStream = format!("crate::cstructs::c::{list}!").parse().unwrap();
            out.extend([TokenTree::Group(Group::new(Delimiter::Parenthesis, callback)),
                        TokenTree::Punct(Punct::new(';', Spacing::Alone))]);
            out
        }
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
    let mut items: Vec<Vec<TokenTree>> = vec![Vec::new()];
    for t in body.into_iter().flatten() {
        match &t {
            TokenTree::Punct(p) if p.as_char() == ',' => items.push(Vec::new()),
            _ => items.last_mut().unwrap().push(t),
        }
    }
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

/// The list's callback: { [decl] (entry), (entry), ... }.
pub fn expand(input: TokenStream) -> TokenStream {
    let mut trees = input.into_iter();
    let decl = match trees.next() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket => g.stream(),
        _ => return compile_error("__tagged_union!: expected [declaration] then the list's entries"),
    };
    let entries: Vec<String> = trees
        .filter_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream().to_string()),
            _ => None,
        })
        .collect();
    let tu = match parse(&decl) {
        Ok(tu) => tu,
        Err(e) => return compile_error(&format!("tagged_union!: {e}")),
    };
    match tagged_union_arms(&tu, &entries) {
        Ok(arms) => generate(&decl, &tu, &arms),
        Err(e) => compile_error(&format!("tagged_union! {}: {e}", tu.name)),
    }
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

fn generate(decl: &TokenStream, tu: &CTaggedUnion, arms: &[CTaggedArm]) -> TokenStream {
    if arms.is_empty() {
        return compile_error(&format!("tagged_union! {}: no arms", tu.name));
    }
    if let Some(d) = tu.defs.iter().find(|d| !arms.iter().any(|a| a.ty.trim() == d.name)) {
        return compile_error(&format!("tagged_union! {}: {} is defined here but isn't an arm's type",
                                      tu.name, d.name));
    }
    // The arm types defined in the block: nestify::nest!, given the
    // declaration's `#[...]*` - propagating, for any types nested in them -
    // as the tokens they were written as.
    let (arm_attrs, def_tokens) = arm_attrs_and_defs(decl);
    let mut defs = TokenStream::new();
    for d in def_tokens {
        let mut body = arm_attrs.clone();
        body.extend(d);
        defs.extend("::nestify::nest!".parse::<TokenStream>().unwrap());
        defs.extend([TokenTree::Group(Group::new(Delimiter::Brace, body))]);
    }
    let (attrs, vis) = attrs_and_vis(decl);
    let name = &tu.name;
    let en = camel(name);
    let (tag, tag_ty, tag_enum) = (&tu.tag_field, &tu.tag_ty, &tu.tag_enum);
    let det_trait = "crate::types::Determinant";
    let arm_of = "crate::types::ArmOf";
    let flex = "crate::util::vstructs::Flex";
    let flex_error = "crate::util::vstructs::FlexError";
    // The storage: the type the views are of
    let s_ty = tu.storage.clone().unwrap_or_else(|| name.clone());
    let s_size = format!("::core::mem::size_of::<{s_ty}>()");

    // The storage, as C has it: for a struct, the tag then the arms' union
    // (with types of their own, C's anonymous struct and union); for a union,
    // the arms. Or none, the storage being another's: a struct's tag is then
    // at its start, and its arms right after - `packed`, as the parser
    // requires.
    let arm_fields: String = arms.iter()
        .map(|a| format!("    {vis} {}: {},\n", a.name, a.ty))
        .collect();
    let pad_field = tu.pad.as_ref().map(|p| format!("    {vis} _pad: {p},\n")).unwrap_or_default();
    let storage_attrs = "#[allow(non_camel_case_types)]\n#[repr(C)]\n#[derive(Clone, Copy)]\n";
    // `packed`: where C has __packed - the arms' union and the tag's struct,
    // or the union
    let packed_attrs = if tu.packed {
        "#[allow(non_camel_case_types)]\n#[repr(C, packed)]\n#[derive(Clone, Copy)]\n"
    } else {
        storage_attrs
    };
    let (storage, payload_offset, determinant) = if tu.storage.is_some() && tu.union {
        (String::new(), "0".to_string(), tu.by.clone().unwrap_or_default())
    } else if tu.storage.is_some() {
        let det = format!("{name}__tag");
        (format!("/// {name}'s tag: the storage's first bytes.\n\
                  #[doc(hidden)]\n\
                  #[allow(non_camel_case_types)]\n\
                  {vis} struct {det};\n\
                  \n\
                  impl {det_trait}<{s_ty}> for {det} {{\n\
                      type Tag = {tag_ty};\n\
                      fn get(u: &{s_ty}) -> {tag_ty} {{\n\
                          // SAFETY: the tag is plain data, at the storage's start\n\
                          unsafe {{ u.as_bytes().as_ptr().cast::<{tag_ty}>().read_unaligned() }}\n\
                      }}\n\
                      fn set(u: &mut {s_ty}, tag: {tag_ty}) {{\n\
                          // SAFETY: a write of plain data, where get() reads it\n\
                          unsafe {{ (u as *mut {s_ty}).cast::<{tag_ty}>().write_unaligned(tag) }}\n\
                      }}\n\
                  }}\n"),
         format!("::core::mem::size_of::<{tag_ty}>()"),
         det)
    } else if tu.union {
        (format!("{attrs}{packed_attrs}{vis} union {name} {{\n{arm_fields}{pad_field}}}\n"),
         "0".to_string(),
         tu.by.clone().unwrap_or_default())
    } else {
        let det = format!("{name}__tag");
        (format!("#[doc(hidden)]\n{packed_attrs}{vis} union {name}__arms {{\n{arm_fields}}}\n\
                  \n\
                  #[doc(hidden)]\n{packed_attrs}{vis} struct {name}__tagged {{\n\
                      {vis} {tag}: {tag_ty},\n\
                      {vis} u: {name}__arms,\n\
                  }}\n\
                  \n\
                  {attrs}{storage_attrs}{vis} union {name} {{\n\
                      {vis} t: {name}__tagged,\n\
                      {pad_field}\
                  }}\n\
                  \n\
                  /// {name}'s tag: its field.\n\
                  #[doc(hidden)]\n\
                  #[allow(non_camel_case_types)]\n\
                  {vis} struct {det};\n\
                  \n\
                  impl {det_trait}<{name}> for {det} {{\n\
                      type Tag = {tag_ty};\n\
                      fn get(u: &{name}) -> {tag_ty} {{\n\
                          // SAFETY: the tag is plain data, the struct's first field\n\
                          unsafe {{ u.t.{tag} }}\n\
                      }}\n\
                      fn set(u: &mut {name}, tag: {tag_ty}) {{\n\
                          #[allow(unused_unsafe)]\n\
                          // SAFETY: a write of plain data\n\
                          unsafe {{ u.t.{tag} = tag }}\n\
                      }}\n\
                  }}\n"),
         format!("::core::mem::offset_of!({name}__tagged, u)"),
         det)
    };
    let det = format!("<{determinant} as {det_trait}<Self>>");

    let by_value: String = arms.iter().map(|a| format!("    {}({}),\n", a.name, a.ty)).collect();
    let by_ref: String = arms.iter()
        .map(|a| format!("    {}({flex}<'a, {}>),\n", a.name, a.ty))
        .collect();
    let gets: String = arms.iter()
        .map(|a| format!("if t == ({}) as {tag_ty} {{ return Ok({en}Ref::{}({flex}::new_unchecked(b))); }}\n",
                         a.value, a.name))
        .collect();
    let news: String = arms.iter()
        .map(|a| format!("{en}::{}(x) => Self::from_arm(x),\n", a.name))
        .collect();
    let arms_of: String = arms.iter()
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
    let pad_assert = tu.pad.as_ref()
        .map(|p| format!("const _: () = assert!({s_size} == ::core::mem::size_of::<{p}>(),\n\
                          \"tagged_union! {name}: bigger than its pad\");\n"))
        .unwrap_or_default();
    // The C: another's storage has its own
    let c_record = if tu.storage.is_none() {
        let nums = [
            format!("::core::mem::size_of::<{name}>() as u64"),
            format!("::core::mem::align_of::<{name}>() as u64"),
            format!("{payload_offset} as u64"),
        ];
        record(&tagged_union_record_text(tu, arms), &nums)
    } else {
        String::new()
    };

    format!(
        "{storage}\
         \n\
         /// One of {name}'s arms, by value: what {name}::new() takes.\n\
         #[allow(non_camel_case_types, dead_code)]\n\
         {vis} enum {en} {{\n{by_value}}}\n\
         \n\
         /// One of {name}'s arms, with the bytes it may extend into - the storage's,\n\
         /// to its end: what {name}::get() gives.\n\
         #[allow(non_camel_case_types, dead_code)]\n\
         #[derive(Clone, Copy)]\n\
         {vis} enum {en}Ref<'a> {{\n{by_ref}}}\n\
         \n\
         impl {s_ty} {{\n\
             /// The tag, whether this version knows it or not.\n\
             #[allow(dead_code)]\n\
             {vis} fn {tag}(&self) -> {tag_enum} {{\n\
                 {tag_enum}({det}::get(self) as _)\n\
             }}\n\
             \n\
             /// From its bytes, as C and disk have them.\n\
             #[allow(dead_code)]\n\
             {vis} fn from_bytes(b: &[u8; {s_size}]) -> Self {{\n\
                 // SAFETY: the storage is plain data: any bytes are a valid {name}\n\
                 unsafe {{ b.as_ptr().cast::<Self>().read_unaligned() }}\n\
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
             /// The arm the tag selects, with the storage's bytes from it to the end -\n\
             /// Err(the tag) if it isn't one of ours.\n\
             #[allow(dead_code)]\n\
             {vis} fn get(&self) -> Result<{en}Ref<'_>, {tag_enum}> {{\n\
                 let t: {tag_ty} = {det}::get(self);\n\
                 let b = &self.as_bytes()[{payload_offset}..];\n\
                 // SAFETY: the tag says these are that arm's bytes, an arm is plain data,\n\
                 // and each is in bounds and aligned there: asserted below\n\
                 unsafe {{\n{gets}}}\n\
                 Err({tag_enum}(t as _))\n\
             }}\n\
             \n\
             /// From an arm: new(), by the arm's type.\n\
             #[allow(dead_code)]\n\
             {vis} fn from_arm<A: {arm_of}<Self, Tag = {tag_ty}>>(arm: A) -> Self {{\n\
                 let mut s = Self::with_payload(arm);\n\
                 {det}::set(&mut s, A::TAG);\n\
                 s\n\
             }}\n\
             \n\
             /// From an arm ending in a flexible array, and that array: as many elements\n\
             /// as the arm's header says, if the storage holds them.\n\
             #[allow(dead_code)]\n\
             {vis} fn from_arm_tailed<A>(arm: A, tail: &[A::Elem]) -> Result<Self, {flex_error}>\n\
             where A: {arm_of}<Self, Tag = {tag_ty}> + crate::util::vstructs::FlexArray\n\
             {{\n\
                 let ty = ::core::any::type_name::<A>();\n\
                 if arm.nr() != tail.len() {{\n\
                     return Err({flex_error}::Count {{ ty, nr: arm.nr(), given: tail.len() }});\n\
                 }}\n\
                 let at = {payload_offset} + A::TAIL;\n\
                 let len = ::core::mem::size_of_val(tail);\n\
                 if at + len > {s_size} {{\n\
                     return Err({flex_error}::Overrun {{ ty, want: A::TAIL + len,\n\
                                                       have: {s_size} - {payload_offset} }});\n\
                 }}\n\
                 let mut s = Self::with_payload(arm);\n\
                 // SAFETY: in bounds, as checked above, and the elements are plain data\n\
                 unsafe {{\n\
                     ::core::ptr::copy_nonoverlapping(tail.as_ptr().cast::<u8>(),\n\
                         (&mut s as *mut Self).cast::<u8>().add(at), len);\n\
                 }}\n\
                 {det}::set(&mut s, A::TAG);\n\
                 Ok(s)\n\
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
             fn with_payload<A: {arm_of}<Self>>(arm: A) -> Self {{\n\
                 // SAFETY: the storage is plain data: any bytes are a valid {name}\n\
                 let mut s: Self = unsafe {{ ::core::mem::zeroed() }};\n\
                 // SAFETY: only arms are ArmOf, and each is plain data, in bounds - asserted\n\
                 // below\n\
                 unsafe {{\n\
                     (&mut s as *mut Self).cast::<u8>().add({payload_offset}).cast::<A>()\n\
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
         {c_record}")
        .parse::<TokenStream>()
        .map(|rest| {
            defs.extend(rest);
            defs
        })
        .unwrap_or_else(|e| compile_error(&format!("tagged_union! {name}: generated code doesn't parse: {e}")))
}
