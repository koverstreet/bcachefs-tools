// SPDX-License-Identifier: GPL-2.0
//! Proc-macros for the userspace kernel-compat shim (`bcachefs-shim`).
//!
//! These are userspace stand-ins for the in-kernel `pin-init` attribute macros,
//! so that fs/ code carrying `#[pin_data]`/`#[pin]` compiles unchanged in
//! userspace — selected by cfg in the importing crate (kernel → real
//! `kernel`/`pin-init`; userspace → here).
//!
//! Why a no-op is correct, not a dodge: pinning in the kernel exists to keep an
//! embedded `work_struct` (and similar address-sensitive types) at a stable
//! address — the workqueue lists it by pointer and recovers the container via
//! `container_of`. In userspace there is no real `work_struct`, and the heap box
//! behind `Arc::new` never moves, so address stability is already guaranteed and
//! there is nothing to pin. `pin_data` here therefore just strips the `#[pin]`
//! field markers the kernel macro would consume and re-emits the struct as a
//! plain, normally-constructible type.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemStruct};

/// No-op userspace mirror of `kernel`'s `#[pin_data]`. Strips the `#[pin]` field
/// markers (so a bare `#[pin]` doesn't fail to resolve as a standalone
/// attribute) and re-emits the struct untouched.
#[proc_macro_attribute]
pub fn pin_data(_args: TokenStream, input: TokenStream) -> TokenStream {
    let mut item = parse_macro_input!(input as ItemStruct);
    for field in item.fields.iter_mut() {
        field.attrs.retain(|attr| !attr.path().is_ident("pin"));
    }
    quote!(#item).into()
}
