// SPDX-License-Identifier: GPL-2.0
//! #[bitfield(uN, atomic)]: the bitfield, and a word of it that's atomic -
//! one-bit fields set and cleared by one atomic op, and anything else - a
//! wider field, several fields at once - by cmpxchg over the value.
//!
//!   #[bitfield(u32, atomic)]
//!   pub struct BtreeNodeFlags {
//!       #[bits(3)] write_type: BtreeWriteType,
//!       read_in_flight: bool,
//!       ...
//!   }
//!
//! BtreeNodeFlags is the value, as any #[bitfield] is. AtomicBtreeNodeFlags is
//! the word: an AtomicU32, the value's own integer.

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote, ToTokens};

/// The atomic word of #[bitfield] @item, stored as @storage, a uN - its
/// members as #[bitfield] has made them - and the value's ToText.
pub(crate) fn atomic(item: &syn::ItemStruct, storage: &Ident) -> TokenStream {
    let (vis, name) = (&item.vis, &item.ident);
    let members: Vec<_> = item.fields.iter()
        .map(|f| (f.ident.as_ref().expect("named members"), &f.ty, &f.vis))
        .filter(|(m, ..)| !m.to_string().starts_with('_'))
        .collect();
    let ty_name = |ty: &syn::Type| ty.to_token_stream().to_string();

    let atomic = format_ident!("Atomic{name}");
    let word = format_ident!("Atomic{}", storage.to_string().to_uppercase());
    let ordering = quote!(::core::sync::atomic::Ordering);
    let (relaxed, seqcst) = (quote!(#ordering::Relaxed), quote!(#ordering::SeqCst));

    let accessors = members.iter().map(|(m, ty, vis)| {
        let set = format_ident!("set_{m}");
        let get = quote! { #vis fn #m(&self) -> #ty { self.load().#m() } };
        if ty_name(ty) == "bool" {
            let swap = format_ident!("swap_{m}");
            let offset = format_ident!("{}_OFFSET", m.to_string().to_uppercase());
            let mask = quote!(((1 as #storage) << #name::#offset));
            quote! {
                #get
                #vis fn #set(&self, v: bool) {
                    if v { self.0.fetch_or(#mask, #relaxed); } else { self.0.fetch_and(!#mask, #relaxed); }
                }
                /// Set to @v, and what it was.
                #vis fn #swap(&self, v: bool) -> bool {
                    (if v { self.0.fetch_or(#mask, #seqcst) } else { self.0.fetch_and(!#mask, #seqcst) }) & #mask != 0
                }
            }
        } else {
            let with = format_ident!("with_{m}");
            quote! {
                #get
                #vis fn #set(&self, v: #ty) {
                    let _ = self.fetch_update(#relaxed, #relaxed, |o| Some(o.#with(v)));
                }
            }
        }
    });

    let prt = members.iter().map(|(m, ty, _)| {
        let label = m.to_string();
        match ty_name(ty).as_str() {
            "bool" => quote! { if self.#m() { sep(out); crate::prt!(out, #label); } },
            "u8" | "u16" | "u32" | "u64" => quote! { sep(out); crate::prt!(out, "{}={}", #label, self.#m()); },
            _ => quote! { sep(out); crate::prt!(out, "{}=", #label); crate::util::ToText::to_text(&self.#m(), out); },
        }
    });

    // on the futex's words: waits on the value
    let waits = matches!(storage.to_string().as_str(), "u8" | "u32").then(|| quote! {
        /// Sleep until @cond of the value holds; woken by wake().
        #vis fn wait_until<E: crate::util::sleep::SleepEnd>(&self, how: crate::util::sleep::Sleep<E>,
                                                           mut cond: impl FnMut(#name) -> bool) -> Result<(), E> {
            crate::locking::bitlock::wait_until(&self.0, how, |w| cond(#name::from_bits(w)))
        }

        /// wait_until(), given up after @timeout jiffies: whether @cond held.
        #vis fn wait_until_timeout(&self, how: crate::util::sleep::Sleep, timeout: ::core::ffi::c_ulong,
                                   mut cond: impl FnMut(#name) -> bool) -> bool {
            crate::locking::bitlock::wait_until_timeout(&self.0, how, timeout, |w| cond(#name::from_bits(w)))
        }

        /// Wake wait_until()'s waiters, after the change they wait for.
        #vis fn wake(&self) {
            crate::locking::bitlock::wake(&self.0)
        }
    });

    let doc = format!("{name}, atomic: one-bit fields by one atomic op each, the rest - and any \
                       change of several - by cmpxchg over the value.");
    quote! {
        impl crate::util::ToText for #name {
            /// The one-bit fields set, by name, and the others' values.
            fn to_text(&self, out: &mut crate::util::Printbuf) {
                let mut first = true;
                let mut sep = |out: &mut crate::util::Printbuf| {
                    if !::core::mem::take(&mut first) { crate::prt!(out, ","); }
                };
                #(#prt)*
            }
        }

        #[doc = #doc]
        #[repr(transparent)]
        #vis struct #atomic(::core::sync::atomic::#word);

        unsafe impl ::pin_init::Zeroable for #atomic {}

        impl #atomic {
            #vis const fn new(v: #name) -> Self {
                Self(::core::sync::atomic::#word::new(v.into_bits()))
            }

            #vis fn load(&self) -> #name {
                #name::from_bits(self.0.load(#relaxed))
            }

            #vis fn store(&self, v: #name) {
                self.0.store(v.into_bits(), #relaxed)
            }

            #vis fn fetch_update(&self, set: #ordering, fetch: #ordering, mut f: impl FnMut(#name) -> Option<#name>)
                -> Result<#name, #name>
            {
                self.0.fetch_update(set, fetch, |w| f(#name::from_bits(w)).map(#name::into_bits))
                    .map(#name::from_bits)
                    .map_err(#name::from_bits)
            }

            #(#accessors)*

            #waits
        }

        impl crate::util::ToText for #atomic {
            fn to_text(&self, out: &mut crate::util::Printbuf) {
                self.load().to_text(out)
            }
        }

        crate::display_via_to_text!(#name);
        crate::display_via_to_text!(#atomic);
    }
}
