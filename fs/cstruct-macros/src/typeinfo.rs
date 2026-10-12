// SPDX-License-Identifier: GPL-2.0
//! `#[derive(TypeInfo)]`: a `crate::typeinfo::TypeInfo` impl for a type C
//! shares - the runtime type information kvdb introspects values with.
//!
//! INFO describes every field: name, offset (`offset_of!`, so the compiler
//! guarantees it) and kind. The fields are cstruct.rs's parse - every struct
//! deriving TypeInfo is a CStruct - and a field's kind is read from the C it
//! renders as, cstruct::field_type(): a field's TypeInfo can't disagree with
//! its generated C declaration.
//!
//! Field types it can't describe - pointers, function pointers, generics,
//! kernel-internal structs - are Opaque rather than an error, so deriving it
//! on a large in-memory struct is harmless. Unions get their size and no
//! fields, as do the enums and bitfields the other macros here give TypeInfo.
//! A field whose type is one of derives_type_info()'s names is described by
//! reference to that type's own INFO, so that type must have TypeInfo too, or
//! the `<T as TypeInfo>` bound fails to resolve (a loud, actionable error).

use proc_macro::TokenStream;

use crate::cstruct::{field_type, is_phantom, parse_cstruct, CKind, FieldType};
use crate::tokens;

const OPAQUE: &str = "crate::typeinfo::FieldKind::Opaque";

/// The type names a field kind may reference as `<T as TypeInfo>::INFO`:
/// each must have TypeInfo itself.
fn derives_type_info(name: &str) -> bool {
    name.starts_with("bch_") || matches!(name, "bpos" | "bkey" | "bversion")
}

/// An unsized vstruct's (cstruct.rs' CGeneric) is every instance's, sized or
/// not, in every state: the fields C has, laid out as C's instance - its
/// state marker isn't one.
pub(crate) fn derive(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    let name = &def.name;
    let inst = def.instance();

    let shape = match def.kind {
        CKind::Struct => "Struct",
        CKind::Union  => "Union",
    };

    let mut fields = String::new();
    if def.kind == CKind::Struct {
        let last = def.fields.len().saturating_sub(1);
        for (i, f) in def.fields.iter().enumerate().filter(|(_, f)| !is_phantom(&f.ty)) {
            let field = f.rust_name();
            fields.push_str(&format!(
                "crate::typeinfo::FieldInfo {{ \
                     name: \"{field}\", \
                     offset: ::core::mem::offset_of!({inst}, {field}), \
                     kind: {} \
                 }},\n", kind(&f.ty, i == last)?));
        }
    }

    let (params, ty) = def.every_instance();
    Ok(tokens(&info_of(&params, &ty, name, &inst, shape, &fields)))
}

/// The TypeInfo impl for @name: @fields its FieldInfo entries - none for a
/// type described by its size alone.
pub(crate) fn info(name: &str, shape: &str, fields: &str) -> String {
    info_of("", name, name, name, shape, fields)
}

/// info(), for type @ty with impl parameters @params, its size @sized's.
fn info_of(params: &str, ty: &str, name: &str, sized: &str, shape: &str, fields: &str) -> String {
    format!(
        "#[automatically_derived]\n\
         impl{params} crate::typeinfo::TypeInfo for {ty} {{\n\
             const INFO: &'static crate::typeinfo::StructInfo = &crate::typeinfo::StructInfo {{\n\
                 name: \"{name}\",\n\
                 size: ::core::mem::size_of::<{sized}>(),\n\
                 shape: crate::typeinfo::Shape::{shape},\n\
                 fields: &[{fields}],\n\
             }};\n\
         }}\n")
}

/// The `FieldKind` expression for a field of Rust type @ty; @last: it's the
/// struct's last field, where `[T; 0]` is C's flexible array member, x[] -
/// as long as the value.
fn kind(ty: &str, last: bool) -> Result<String, String> {
    Ok(match field_type(ty)? {
        FieldType::Array { elem, n } => {
            let elem_kind = kind(&elem, false)?;
            let stride = format!("::core::mem::size_of::<{elem}>()");
            if last && n == "0" {
                format!("crate::typeinfo::FieldKind::VarTail {{ elem: &{elem_kind}, stride: {stride} }}")
            } else {
                format!("crate::typeinfo::FieldKind::Array {{ elem: &{elem_kind}, n: {n}, stride: {stride} }}")
            }
        }
        FieldType::Opaque => OPAQUE.into(),
        FieldType::Named { c, name } => match c.as_deref().and_then(int) {
            Some((bytes, endian, signed)) => {
                let bytes = match bytes {
                    Some(b) => format!("{b}u8"),
                    None    => format!("::core::mem::size_of::<{ty}>() as u8"),
                };
                format!("crate::typeinfo::FieldKind::Int {{ \
                             bytes: {bytes}, \
                             endian: crate::typeinfo::Endian::{endian}, \
                             signed: {signed} \
                         }}")
            }
            None if derives_type_info(&name) =>
                format!("crate::typeinfo::FieldKind::Struct(<{ty} as crate::typeinfo::TypeInfo>::INFO)"),
            None => OPAQUE.into(),
        },
    })
}

/// A C integer type, as canon_base() names Rust's: its size - None for the
/// platform width, which the compiler supplies - endianness and signedness.
fn int(c: &str) -> Option<(Option<u8>, &'static str, bool)> {
    Some(match c {
        "u8"  | "bool"                => (Some(1), "Native", false),
        "u16" | "unsigned short"      => (Some(2), "Native", false),
        "u32" | "unsigned"            => (Some(4), "Native", false),
        "u64" | "unsigned long long"  => (Some(8), "Native", false),
        "s8"  | "char"                => (Some(1), "Native", true),
        "s16" | "short"               => (Some(2), "Native", true),
        "s32" | "int"                 => (Some(4), "Native", true),
        "s64" | "long long"           => (Some(8), "Native", true),
        "__le16"                      => (Some(2), "Little", false),
        "__le32"                      => (Some(4), "Little", false),
        "__le64"                      => (Some(8), "Little", false),
        "__be16"                      => (Some(2), "Big",    false),
        "__be32"                      => (Some(4), "Big",    false),
        "__be64"                      => (Some(8), "Big",    false),
        "size_t"  | "unsigned long"   => (None,    "Native", false),
        "ssize_t" | "long"            => (None,    "Native", true),
        _ => return None,
    })
}
