// SPDX-License-Identifier: GPL-2.0
//! `#[derive(TypeInfo)]` — runtime type information for the bindgen-generated
//! C struct bindings.
//!
//! Emits a `crate::typeinfo::TypeInfo` impl whose `INFO` describes every field:
//! name, offset (via `offset_of!`, so the compiler guarantees it) and kind.
//! The kind encodes width, endianness and signedness, recovered from the C
//! typedef *name* (`__le32`, `__u64`, ...). The typedefs resolve to plain
//! integers, so the endianness only exists at the syntax level — that's why
//! this must be a derive macro looking at tokens rather than a generic
//! reflection library looking at resolved types.
//!
//! Injected into the bindings by bindgen's `--with-derive-custom-*` flags (see
//! `codegen.rs`) on structs, unions and enums matching the `bch_*`-family
//! regex. The macro is total over everything bindgen emits for those: field
//! types it can't describe (pointers, function pointers, bitfield units,
//! kernel-internal structs) degrade to `Opaque` rather than failing, so
//! tagging an in-memory monster like `bch_fs` is harmless. Unions and enums
//! get an empty field list (size only). Nested struct fields whose type name
//! matches the same regex are described by reference to that type's own INFO —
//! the regex here and in codegen.rs must stay in sync, or the emitted
//! `<T as TypeInfo>` bound will fail to resolve (a loud, actionable error).
//!
//! Hand-rolled token parsing, zero dependencies (no syn/quote): the kernel and
//! DKMS builds compile this with a plain `rustc --crate-type proc-macro`, the
//! same recipe as the vendored `paste` crate, so it must be self-contained.
//! The input grammar is machine-generated bindgen output, which is far more
//! regular than general Rust — this is much less heroic than it sounds.

use proc_macro::{Delimiter, TokenStream, TokenTree};

/// Must match the `--with-derive-custom-*` regex in codegen.rs: these are the
/// type names we may reference as `<T as TypeInfo>::INFO` from field kinds.
fn derives_type_info(name: &str) -> bool {
    name.starts_with("bch_") || matches!(name, "bpos" | "bkey" | "bversion")
}

#[proc_macro_derive(TypeInfo)]
pub fn derive_type_info(input: TokenStream) -> TokenStream {
    let toks: Vec<TokenTree> = input.into_iter().collect();
    let mut i = 0;

    skip_attrs_and_vis(&toks, &mut i);

    let Some(TokenTree::Ident(kw)) = toks.get(i) else { return TokenStream::new() };
    let kw = kw.to_string();
    i += 1;

    let Some(TokenTree::Ident(name)) = toks.get(i) else { return TokenStream::new() };
    let name = name.to_string();
    i += 1;

    // Generic types can't be described by a single static table; bindgen
    // doesn't emit generics for the types we match, so just skip quietly.
    if matches!(toks.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '<') {
        return TokenStream::new();
    }

    let (shape, fields) = match kw.as_str() {
        "enum" => ("Enum", String::new()),
        "union" => ("Union", String::new()),
        "struct" => match toks.get(i) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => {
                ("Struct", named_fields(&name, g.stream()))
            }
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => {
                ("Struct", tuple_fields(&name, g.stream()))
            }
            _ => ("Struct", String::new()), // unit struct
        },
        _ => return TokenStream::new(),
    };

    format!(
        "#[automatically_derived]\n\
         impl crate::typeinfo::TypeInfo for {name} {{\n\
             const INFO: &'static crate::typeinfo::StructInfo = &crate::typeinfo::StructInfo {{\n\
                 name: \"{name}\",\n\
                 size: ::core::mem::size_of::<{name}>(),\n\
                 shape: crate::typeinfo::Shape::{shape},\n\
                 fields: &[{fields}],\n\
             }};\n\
         }}\n"
    )
    .parse()
    .expect("TypeInfo derive: generated impl failed to parse")
}

fn skip_attrs_and_vis(toks: &[TokenTree], i: &mut usize) {
    loop {
        match toks.get(*i) {
            Some(TokenTree::Punct(p)) if p.as_char() == '#' => {
                *i += 2; // '#' + [...] group (doc comments arrive this way too)
            }
            Some(TokenTree::Ident(id)) if id.to_string() == "pub" => {
                *i += 1;
                if matches!(toks.get(*i), Some(TokenTree::Group(g))
                            if g.delimiter() == Delimiter::Parenthesis)
                {
                    *i += 1; // pub(crate) etc.
                }
            }
            _ => return,
        }
    }
}

/// Split a token stream at top-level occurrences of `sep`. Groups are atomic
/// `TokenTree`s, so no bracket-depth tracking is needed — but generic argument
/// lists aren't groups, so track `<`/`>` depth for types like `Option<fn(A, B)>`.
fn split_top_level(ts: TokenStream, sep: char) -> Vec<Vec<TokenTree>> {
    let mut out = vec![Vec::new()];
    let mut angle = 0i32;
    let mut prev_dash = false;
    for t in ts {
        if let TokenTree::Punct(p) = &t {
            match p.as_char() {
                '<' => angle += 1,
                '>' if !prev_dash => angle -= 1, // ignore `->`
                c if c == sep && angle == 0 => {
                    out.push(Vec::new());
                    prev_dash = false;
                    continue;
                }
                _ => {}
            }
            prev_dash = p.as_char() == '-';
        } else {
            prev_dash = false;
        }
        out.last_mut().unwrap().push(t);
    }
    if out.last().is_some_and(|v| v.is_empty()) {
        out.pop(); // trailing separator
    }
    out
}

fn named_fields(struct_name: &str, body: TokenStream) -> String {
    let mut out = String::new();
    for field in split_top_level(body, ',') {
        let mut i = 0;
        skip_attrs_and_vis(&field, &mut i);
        let Some(TokenTree::Ident(fname)) = field.get(i) else { continue };
        let fname = fname.to_string();
        i += 1;
        // expect ':'
        if !matches!(field.get(i), Some(TokenTree::Punct(p)) if p.as_char() == ':') {
            continue;
        }
        i += 1;
        push_field(&mut out, struct_name, &fname, &fname, &field[i..]);
    }
    out
}

fn tuple_fields(struct_name: &str, body: TokenStream) -> String {
    let mut out = String::new();
    for (idx, field) in split_top_level(body, ',').into_iter().enumerate() {
        let mut i = 0;
        skip_attrs_and_vis(&field, &mut i);
        let idx = idx.to_string();
        push_field(&mut out, struct_name, &idx, &idx, &field[i..]);
    }
    out
}

fn push_field(out: &mut String, struct_name: &str, fname: &str, faccess: &str, ty: &[TokenTree]) {
    let kind = classify(ty);
    out.push_str(&format!(
        "crate::typeinfo::FieldInfo {{ \
             name: \"{fname}\", \
             offset: ::core::mem::offset_of!({struct_name}, {faccess}), \
             kind: {kind} \
         }},\n"
    ));
}

fn tokens_to_string(ts: &[TokenTree]) -> String {
    // Collect into a TokenStream and let its Display do the formatting: it
    // respects Punct spacing, so `::` round-trips as one path separator
    // rather than two lone colons (which wouldn't re-parse).
    ts.iter().cloned().collect::<TokenStream>().to_string()
}

fn int_kind(bytes: &str, endian: &str, signed: bool) -> String {
    format!(
        "crate::typeinfo::FieldKind::Int {{ \
             bytes: {bytes}, \
             endian: crate::typeinfo::Endian::{endian}, \
             signed: {signed} \
         }}"
    )
}

const OPAQUE: &str = "crate::typeinfo::FieldKind::Opaque";

/// Map a field type (as tokens) to a `FieldKind` expression. Total: anything
/// unrecognized is `Opaque`, never an error.
fn classify(ty: &[TokenTree]) -> String {
    if ty.is_empty() {
        return OPAQUE.into();
    }

    // Raw pointers (and references, should bindgen ever emit one).
    if matches!(&ty[0], TokenTree::Punct(p) if matches!(p.as_char(), '*' | '&')) {
        return OPAQUE.into();
    }

    // Arrays: a single bracket group `[Elem; N]`.
    if ty.len() == 1 {
        if let TokenTree::Group(g) = &ty[0] {
            if g.delimiter() == Delimiter::Bracket {
                let parts = split_top_level(g.stream(), ';');
                if parts.len() == 2 {
                    let elem = classify(&parts[0]);
                    let elem_ty = tokens_to_string(&parts[0]);
                    let n = tokens_to_string(&parts[1]);
                    return format!(
                        "crate::typeinfo::FieldKind::Array {{ \
                             elem: &{elem}, \
                             n: {n}, \
                             stride: ::core::mem::size_of::<{elem_ty}>() \
                         }}"
                    );
                }
                return OPAQUE.into();
            }
        }
    }

    // A (possibly `::`-qualified) path, possibly with generic arguments.
    // Find the last path-segment ident before any `<`.
    let mut last_ident = None;
    let mut generic_start = None;
    for (i, t) in ty.iter().enumerate() {
        match t {
            TokenTree::Ident(id) => last_ident = Some(id.to_string()),
            TokenTree::Punct(p) if p.as_char() == '<' => {
                generic_start = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(last_ident) = last_ident else { return OPAQUE.into() };

    if let Some(gs) = generic_start {
        // `__IncompleteArrayField<T>`: C flexible array member — a variable
        // tail whose length comes from the value's size at runtime.
        if last_ident == "__IncompleteArrayField" && ty.len() >= gs + 3 {
            let inner = &ty[gs + 1..ty.len() - 1];
            let elem = classify(inner);
            let elem_ty = tokens_to_string(inner);
            return format!(
                "crate::typeinfo::FieldKind::VarTail {{ \
                     elem: &{elem}, \
                     stride: ::core::mem::size_of::<{elem_ty}>() \
                 }}"
            );
        }
        // Option<fn...>, __BindgenUnionField<T>, ...
        return OPAQUE.into();
    }

    let full = tokens_to_string(ty);

    // Fixed-width integer typedefs. Endianness lives in the *name*.
    let (bytes, endian, signed) = match last_ident.as_str() {
        "__le16" => ("2u8", "Little", false),
        "__le32" => ("4u8", "Little", false),
        "__le64" => ("8u8", "Little", false),
        "__be16" => ("2u8", "Big", false),
        "__be32" => ("4u8", "Big", false),
        "__be64" => ("8u8", "Big", false),
        "u8" | "u8_" | "__u8" | "bool" => ("1u8", "Native", false),
        "u16" | "u16_" | "__u16" => ("2u8", "Native", false),
        "u32" | "u32_" | "__u32" => ("4u8", "Native", false),
        "u64" | "u64_" | "__u64" => ("8u8", "Native", false),
        "i8" | "s8" | "__s8" => ("1u8", "Native", true),
        "i16" | "s16" | "__s16" => ("2u8", "Native", true),
        "i32" | "s32" | "__s32" => ("4u8", "Native", true),
        "i64" | "s64" | "__s64" => ("8u8", "Native", true),
        "c_uchar" => ("1u8", "Native", false),
        "c_char" | "c_schar" => ("1u8", "Native", true),
        "c_ushort" => ("2u8", "Native", false),
        "c_short" => ("2u8", "Native", true),
        "c_uint" => ("4u8", "Native", false),
        "c_int" => ("4u8", "Native", true),
        "c_ulonglong" => ("8u8", "Native", false),
        "c_longlong" => ("8u8", "Native", true),
        // Platform-width types: let the compiler supply the size.
        "usize" | "c_ulong" => ("", "Native", false),
        "isize" | "c_long" => ("", "Native", true),
        other => {
            // Nested struct/union/enum we also derive on?
            if derives_type_info(other) {
                return format!(
                    "crate::typeinfo::FieldKind::Struct(\
                         <{full} as crate::typeinfo::TypeInfo>::INFO)"
                );
            }
            return OPAQUE.into();
        }
    };

    let bytes = if bytes.is_empty() {
        format!("::core::mem::size_of::<{full}>() as u8")
    } else {
        bytes.into()
    };
    int_kind(&bytes, endian, signed)
}
