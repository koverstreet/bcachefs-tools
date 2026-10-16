// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Init)]`: `T::init()`, a pin-init initializer for a struct C
//! shares, for Rust to allocate it with.
//!
//! Every field is zeroed - as kvzalloc() left the struct, for C's own init to
//! fill in after - but a field with `#[init(EXPR)]`, which is initialized from
//! EXPR: a Rust type's real initializer, or a struct's own init(). So a C
//! field costs nothing here, now or as it stays C.
//!
//! Forgetting one is a compile error, not a zeroed Mutex: a field without
//! `#[init]` gets `pin_init::init_zeroed()`, which only a Zeroable type
//! takes - C's are, a Rust type with invariants, or a struct holding one,
//! isn't.
//!
//! The struct is dropped whole, after C's teardown: so a C type's Rust Drop
//! must do nothing to what C's teardown left - printbuf's does, as
//! printbuf_exit() leaves a printbuf empty.
//!
//! The struct needs `#[pin_data]` too, ahead of the derives - pin_init!()
//! needs its projections - and `#[pin]` on a field whose initializer is a
//! PinInit, not an Init: a kernel Mutex, a struct's init().

use proc_macro::TokenStream;

use crate::cstruct::{parse_cstruct, CKind};
use crate::tokens;

pub(crate) fn derive(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    if def.kind != CKind::Struct {
        return Err(format!("{}: #[derive(Init)] is for a struct", def.name));
    }
    let inst = def.rust_ident();

    let mut fields = String::new();
    for f in &def.fields {
        if let Some(cfg) = &f.cfg {
            fields.push_str(&format!("#[cfg({})] ", cfg.to_rust()));
        }
        let init = f.init.as_deref().unwrap_or("::pin_init::init_zeroed()");
        fields.push_str(&format!("{} <- {init},\n", f.rust_name()));
    }

    Ok(tokens(&format!(
        "#[automatically_derived]\n\
         impl {inst} {{\n\
             /// Zeroed, but for its #[init] fields: see #[derive(Init)].\n\
             pub fn init() -> impl ::pin_init::PinInit<Self> {{\n\
                 ::pin_init::pin_init!({inst} {{\n{fields}}})\n\
             }}\n\
         }}\n")))
}
