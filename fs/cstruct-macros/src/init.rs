// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Init)]`: `T::init()`, a pin-init initializer for a struct C
//! shares, for Rust to allocate it with.
//!
//! Every field is zeroed - as kvzalloc() left the struct, for C's own init to
//! fill in after - but a field with `#[init(EXPR)]`, which is initialized from
//! EXPR: a Rust type's real initializer, or a struct's own init(). So a C
//! field costs nothing here, now or as it stays C.
//!
//! An initializer may fail - a percpu rwsem allocates - with a BchError, or
//! an error that converts to one; the infallible ones convert from
//! Infallible. Then the fields initialized so far are dropped, and init()
//! fails with it.
//!
//! Forgetting one is a compile error, not a zeroed lock: a field without
//! `#[init]` is zeroed - by place.rs's field_init(), unless a placed type
//! lives there - which only a Zeroable type
//! takes - C's are, a Rust type with invariants, or a struct holding one,
//! isn't. core's atomics are zeroed as values: they're as Zeroable as their
//! integers, but pin_init has no impl for them, and the orphan rule allows
//! none here.
//!
//! The struct is dropped whole, after C's teardown: so a C type's Rust Drop
//! must do nothing to what C's teardown left - printbuf's does, as
//! printbuf_exit() leaves a printbuf empty.
//!
//! The struct needs `#[pin_data]` too, ahead of the derives - pin_init!()
//! needs its projections - and `#[pin]` on a field whose initializer is a
//! PinInit, not an Init: a lock, a struct's init().
//!
//! And each field is a Field<N>, one without #[init] is zeroed only if no
//! placed type lives there, and a placed struct's init() takes its At: a
//! value's place - place.rs.

use proc_macro::TokenStream;

use crate::cstruct::{parse_cstruct, CKind};
use crate::place;
use crate::tokens;

pub(crate) fn derive(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input.to_string())?;
    if def.kind != CKind::Struct {
        return Err(format!("{}: #[derive(Init)] is for a struct", def.name));
    }
    let inst = def.rust_ident();
    let (impl_lts, inst_ty) = if def.lifetimes.is_empty() {
        (String::new(), inst.to_string())
    } else {
        let l = def.lifetimes.join(", ");
        (format!("<{l}>"), format!("{inst}<{l}>"))
    };

    let mut fields = String::new();
    let mut field_impls = String::new();
    let mut at: Option<&str> = None;
    for f in &def.fields {
        let cfg = f.cfg.as_ref().map(|c| format!("#[cfg({})] ", c.to_rust())).unwrap_or_default();
        let name = f.rust_name();
        fields.push_str(&cfg);
        field_impls.push_str(&place::field_impl(&cfg, &impl_lts, &inst_ty, &name, &f.ty));

        if place::is_at_field(f) {
            at = Some(&f.ty);
            fields.push_str(&format!("{name}: at,\n"));
            continue;
        }
        match f.init.as_deref() {
            Some(init) => fields.push_str(&format!("{name} <- {init},\n")),
            // SAFETY: core's atomics are their integers, and zero is one -
            // Zeroable, but for the impl the orphan rule doesn't allow us
            None if crate::is_core_atomic(&f.ty) =>
                fields.push_str(&format!("{name}: unsafe {{ ::core::mem::zeroed() }},\n")),
            None => fields.push_str(&format!("{name} <- {},\n", place::field_init(&name))),
        }
    }

    // An At is crate-private, as place.rs is - so is an init() taking one
    let (vis, at_param) = match at {
        Some(ty) => ("pub(crate)", format!("at: {ty}")),
        None => ("pub", String::new()),
    };

    Ok(tokens(&format!(
        "#[automatically_derived]\n\
         impl {inst} {{\n\
             /// Zeroed, but for its #[init] fields: see #[derive(Init)].\n\
             {vis} fn init({at_param}) -> impl ::pin_init::PinInit<Self, crate::errcode::BchError> {{\n\
                 ::pin_init::pin_init!({inst} {{\n{fields}}}? crate::errcode::BchError)\n\
             }}\n\
         }}\n\
         {field_impls}")))
}
