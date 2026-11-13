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

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

use crate::cstruct::{field_type, flex_elem, parse_cstruct, CKind, FieldType, IntType};

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
    let def = parse_cstruct(&input)?;
    let inst = &def.ident;

    let shape = match def.kind {
        CKind::Struct => "Struct",
        CKind::Union  => "Union",
    };

    let mut fields = Vec::new();
    if def.kind == CKind::Struct {
        // a place's At is zero bytes of nothing: not described
        let described: Vec<_> = def.fields.iter().filter(|f| !crate::place::is_empty_at_field(f)).collect();
        let last = described.len().saturating_sub(1);
        for (i, f) in described.into_iter().enumerate() {
            let (field, kind) = (&f.ident, kind(&f.ty, i == last));
            let name = field.to_string();
            fields.push(quote! {
                crate::typeinfo::FieldInfo {
                    name: #name,
                    offset: ::core::mem::offset_of!(#inst, #field),
                    kind: #kind,
                }
            });
        }
    }

    Ok(info(inst, def.name(), shape, &fields))
}

/// The TypeInfo impl for @ty, named @name: @fields its FieldInfo entries -
/// none for a type described by its size alone.
pub(crate) fn info(ty: &Ident, name: &Ident, shape: &str, fields: &[TokenStream]) -> TokenStream {
    let (name, shape) = (name.to_string(), format_ident!("{shape}"));
    quote! {
        #[automatically_derived]
        impl crate::typeinfo::TypeInfo for #ty {
            const INFO: &'static crate::typeinfo::StructInfo = &crate::typeinfo::StructInfo {
                name: #name,
                size: ::core::mem::size_of::<#ty>(),
                shape: crate::typeinfo::Shape::#shape,
                fields: &[#(#fields),*],
            };
        }
    }
}

/// The `FieldKind` expression for a field of Rust type @ty; @last: it's the
/// struct's last field, where `[T; 0]` is C's flexible array member, x[] -
/// as long as the value.
fn kind(ty: &syn::Type, last: bool) -> TokenStream {
    match field_type(ty) {
        FieldType::Array { elem, n } => {
            let elem_kind = kind(elem, false);
            let stride = quote!(::core::mem::size_of::<#elem>());
            if last && flex_elem(ty).is_some() {
                quote!(crate::typeinfo::FieldKind::VarTail { elem: &#elem_kind, stride: #stride })
            } else {
                quote!(crate::typeinfo::FieldKind::Array { elem: &#elem_kind, n: #n, stride: #stride })
            }
        }
        FieldType::Named { int: Some(IntType { bytes, endian, signed }), .. } => {
            let bytes = match bytes {
                Some(b) => quote!(#b),
                None    => quote!(::core::mem::size_of::<#ty>() as u8),
            };
            let endian = format_ident!("{endian}");
            quote! {
                crate::typeinfo::FieldKind::Int {
                    bytes: #bytes,
                    endian: crate::typeinfo::Endian::#endian,
                    signed: #signed,
                }
            }
        }
        FieldType::Named { name, .. } if derives_type_info(&name) =>
            quote!(crate::typeinfo::FieldKind::Struct(<#ty as crate::typeinfo::TypeInfo>::INFO)),
        FieldType::Named { .. } | FieldType::Opaque => quote!(crate::typeinfo::FieldKind::Opaque),
    }
}

