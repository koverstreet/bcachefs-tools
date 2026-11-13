// SPDX-License-Identifier: GPL-2.0
//! `#[derive(TypeInfo)]`: a `crate::typeinfo::TypeInfo` impl for a struct or
//! union - the runtime type information kvdb introspects values with.
//!
//! INFO describes every field: name, offset (`offset_of!`, so the compiler
//! guarantees it) and kind. The fields are cstruct.rs's parse - every struct
//! deriving TypeInfo is a CStruct - and a field's kind is its type's,
//! cstruct::field_type().
//!
//! Field types it can't describe - pointers, function pointers, generics,
//! kernel-internal structs - are Opaque rather than an error, so deriving it
//! on a large in-memory struct is harmless. Unions get their size and no
//! fields, as do the enums and bitfields the other macros here give TypeInfo.
//! A field whose type is one of derives_type_info()'s names is described by
//! reference to that type's own INFO, so that type must have TypeInfo too, or
//! the `<T as TypeInfo>` bound fails to resolve (a loud, actionable error).

use proc_macro2::TokenStream;

use crate::cstruct::{field_type, parse_cstruct, CKind, FieldType, IntType};
use crate::tokens;

const OPAQUE: &str = "crate::typeinfo::FieldKind::Opaque";

/// The type names a field kind may reference as `<T as TypeInfo>::INFO`:
/// each must have TypeInfo itself, by the name the field's type is written
/// with.
fn derives_type_info(name: &str) -> bool {
    name.starts_with("bch_") ||
        matches!(name, "Bpos" | "bkey" | "Bversion" |
                       "Rbio" | "ReadErrReport")
}

/// By its type's name: a family's definition, X__head, is X.
pub(crate) fn derive(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    let name = &def.name;
    let inst = def.rust_ident();

    let shape = match def.kind {
        CKind::Struct => "Struct",
        CKind::Union  => "Union",
    };

    let mut fields = String::new();
    if def.kind == CKind::Struct {
        // a place's At is zero bytes of nothing: not described
        let described: Vec<_> = def.fields.iter().filter(|f| !crate::place::is_empty_at_field(f)).collect();
        let last = described.len().saturating_sub(1);
        for (i, f) in described.into_iter().enumerate() {
            let field = f.rust_name();
            fields.push_str(&format!(
                "crate::typeinfo::FieldInfo {{ \
                     name: \"{field}\", \
                     offset: ::core::mem::offset_of!({inst}, {field}), \
                     kind: {} \
                 }},\n", kind(&f.ty, i == last)?));
        }
    }

    Ok(tokens(&info_of(inst, name, shape, &fields)))
}

/// The TypeInfo impl for @name: @fields its FieldInfo entries - none for a
/// type described by its size alone.
pub(crate) fn info(name: &str, shape: &str, fields: &str) -> String {
    info_of(name, name, shape, fields)
}

/// info(), for Rust type @ty, named @name.
pub(crate) fn info_of(ty: &str, name: &str, shape: &str, fields: &str) -> String {
    format!(
        "#[automatically_derived]\n\
         impl crate::typeinfo::TypeInfo for {ty} {{\n\
             const INFO: &'static crate::typeinfo::StructInfo = &crate::typeinfo::StructInfo {{\n\
                 name: \"{name}\",\n\
                 size: ::core::mem::size_of::<{ty}>(),\n\
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
        FieldType::Named { int, name } => match int {
            Some(IntType { bytes, endian, signed }) => {
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

