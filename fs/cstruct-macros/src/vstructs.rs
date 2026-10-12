// SPDX-License-Identifier: GPL-2.0
//! What util/vstructs.rs's traits need of a type C shares, from its
//! definition: #[derive(Plain)] - and #[bitfield], every one of which is -
//! and #[counted_by(FIELD)], a CStruct's FlexArray.
//!
//! Plain is a promise: every bit pattern of the type's bytes is a value of
//! it, and every byte is a field's. These impls check it at compile time,
//! rather than take it on trust: each field's type is Plain, and the fields'
//! sizes add up to the type's, so there's no padding - zerocopy's FromBytes,
//! for the types we define.

use proc_macro::TokenStream;

use crate::cstruct::{field_type, parse_cstruct, CKind, CStructDef, FieldType};
use crate::tokens;

/// #[derive(Plain)], on a struct - not a union, whose bytes past a short
/// arm's end are no field's.
pub(crate) fn derive_plain(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    if def.kind != CKind::Struct {
        return Err(format!("{}: Plain is for structs", def.name));
    }
    let tys: Vec<&str> = def.fields.iter().map(|f| f.ty.as_str()).collect();
    Ok(tokens(&plain(&def.name, &tys)))
}

/// Plain for @name, whose fields are of types @tys: the impl, and the
/// checks that make it sound - a compile error if it isn't.
pub(crate) fn plain(name: &str, tys: &[&str]) -> String {
    let fields: String = tys.iter()
        .map(|ty| format!("crate::util::vstructs::assert_plain::<{ty}>();\n"))
        .collect();
    let sizes: String = tys.iter()
        .map(|ty| format!(" + ::core::mem::size_of::<{ty}>()"))
        .collect();
    format!(
        "unsafe impl crate::util::vstructs::Plain for {name} {{}}\n\
         const _: () = {{\n\
             {fields}\
             assert!(::core::mem::size_of::<{name}>() == 0{sizes},\n\
                     \"{name}: padding between its fields - it isn't Plain\");\n\
         }};\n")
}

/// A CStruct's FlexArray, from #[counted_by(FIELD)] on its trailing `[T; 0]`
/// - empty if it has none.
pub(crate) fn flex_array(def: &CStructDef) -> Result<String, String> {
    let name = &def.name;
    let Some((i, f)) = def.fields.iter().enumerate().find(|(_, f)| f.counted_by.is_some()) else {
        return Ok(String::new());
    };
    let count = f.counted_by.as_deref().unwrap_or_default();
    let elem = match field_type(&f.ty)? {
        FieldType::Array { elem, n } if n == "0" && i + 1 == def.fields.len() => elem,
        _ => return Err(format!("{name}.{}: #[counted_by] is for the last field, a [T; 0]", f.name)),
    };
    if !def.fields.iter().any(|g| g.rust_name() == count) {
        return Err(format!("{name}.{}: #[counted_by({count})]: {name} has no field {count}", f.name));
    }
    Ok(flex_impl(name, &f.rust_name(), &elem, count))
}

/// FlexArray for @name: @field, [@elem; 0], as long as @count says.
fn flex_impl(name: &str, field: &str, elem: &str, count: &str) -> String {
    // the count by value: a reference to a packed struct's field may be
    // unaligned
    format!(
        "impl crate::util::vstructs::FlexArray for {name} {{\n\
             type Elem = {elem};\n\
             const TAIL: usize = ::core::mem::offset_of!({name}, {field});\n\
             fn nr(&self) -> usize {{ crate::util::vstructs::Count::count(&{{ self.{count} }}) }}\n\
         }}\n")
}
