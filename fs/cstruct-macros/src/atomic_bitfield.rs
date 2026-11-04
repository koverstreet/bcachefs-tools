// SPDX-License-Identifier: GPL-2.0
//! atomic_bitfield!{}: a bitfield's value, and a word of it that's atomic -
//! one-bit fields set and cleared by one atomic op, as C's set_bit(), and
//! anything else - a wider field, several fields at once - by cmpxchg over
//! the value.
//!
//!   atomic_bitfield! {
//!       pub struct BtreeNodeFlags: u32 in usize {
//!           #[bits(3)] write_type: btree_write_type,
//!           BTREE_FLAGS(flag) => flag: bool,
//!       }
//!   }
//!
//! BtreeNodeFlags is the value: bitfield-struct's, over the u32, padded to
//! it. AtomicBtreeNodeFlags is the word: an AtomicUsize - `in usize`, for
//! C's unsigned long; without it, the value's own integer.
//!
//! `LIST(a, ..) => a: T` is a field per row of x-macro LIST, named by the
//! row's column a - as c_enum!'s rows are: the list stays the one source of
//! the names, C's accessors and name tables made from it too.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, ToTokens};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{braced, parenthesized, Attribute, Ident, LitInt, Token, Type, Visibility};

struct Decl {
    attrs:   Vec<Attribute>,
    vis:     Visibility,
    name:    Ident,
    bits:    Ident,
    word:    Option<Ident>,
    entries: Vec<Entry>,
}

enum Entry {
    Field(Field),
    Rows { list: Ident, cols: Vec<Option<Ident>>, rest: bool, field: Field },
}

#[derive(Clone)]
struct Field {
    attrs: Vec<Attribute>,
    name:  Ident,
    ty:    Type,
}

impl Parse for Field {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let name = input.parse()?;
        input.parse::<Token![:]>()?;
        Ok(Field { attrs, name, ty: input.parse()? })
    }
}

impl Parse for Entry {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if !(input.peek(Ident) && input.peek2(syn::token::Paren)) {
            return Ok(Entry::Field(input.parse()?));
        }
        let list = input.parse()?;
        let inner;
        parenthesized!(inner in input);
        let (mut cols, mut rest) = (Vec::new(), false);
        while !inner.is_empty() {
            if inner.peek(Token![..]) {
                inner.parse::<Token![..]>()?;
                rest = true;
            } else if inner.peek(Token![_]) {
                inner.parse::<Token![_]>()?;
                cols.push(None);
            } else {
                cols.push(Some(inner.parse()?));
            }
            if !inner.is_empty() {
                inner.parse::<Token![,]>()?;
            }
        }
        input.parse::<Token![=>]>()?;
        Ok(Entry::Rows { list, cols, rest, field: input.parse()? })
    }
}

impl Parse for Decl {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let vis = input.parse()?;
        input.parse::<Token![struct]>()?;
        let name = input.parse()?;
        input.parse::<Token![:]>()?;
        let bits = input.parse()?;
        let word = if input.peek(Token![in]) {
            input.parse::<Token![in]>()?;
            Some(input.parse()?)
        } else {
            None
        };
        let body;
        braced!(body in input);
        let entries = Punctuated::<Entry, Token![,]>::parse_terminated(&body)?.into_iter().collect();
        Ok(Decl { attrs, vis, name, bits, word, entries })
    }
}

impl ToTokens for Field {
    fn to_tokens(&self, out: &mut TokenStream) {
        let Field { attrs, name, ty } = self;
        out.extend(quote!(#(#attrs)* #name: #ty));
    }
}

impl ToTokens for Entry {
    fn to_tokens(&self, out: &mut TokenStream) {
        out.extend(match self {
            Entry::Field(f) => quote!(#f),
            Entry::Rows { list, cols, rest, field } => {
                let cols = cols.iter().map(|c| c.as_ref().map_or(quote!(_), |c| quote!(#c)));
                let rest = rest.then(|| quote!(..));
                quote!(#list(#(#cols,)* #rest) => #field)
            }
        });
    }
}

impl ToTokens for Decl {
    fn to_tokens(&self, out: &mut TokenStream) {
        let Decl { attrs, vis, name, bits, word, entries } = self;
        let word = word.as_ref().map(|w| quote!(in #w));
        out.extend(quote!(#(#attrs)* #vis struct #name: #bits #word { #(#entries,)* }));
    }
}

/// The declaration with its first list's rows in place, or generated, if it
/// has no list left.
fn next(decl: Decl) -> syn::Result<TokenStream> {
    match decl.entries.iter().find_map(|e| match e { Entry::Rows { list, .. } => Some(list), _ => None }) {
        Some(list) => Ok(super::through_list(&list.to_string(), "cstruct_macros::__atomic_bitfield", quote!(#decl))),
        None => generate(&decl),
    }
}

pub(crate) fn atomic_bitfield(input: TokenStream) -> syn::Result<TokenStream> {
    next(syn::parse2(input)?)
}

/// The list's callback: [decl] then its rows, each a field.
pub(crate) fn rows(input: TokenStream) -> syn::Result<TokenStream> {
    let (decl, rows) = super::list_callback(input).map_err(|e| syn::Error::new(Span::call_site(), e))?;
    let mut decl: Decl = syn::parse2(decl)?;
    let at = decl.entries.iter().position(|e| matches!(e, Entry::Rows { .. })).expect("a list to expand");
    let Entry::Rows { cols, rest, field, .. } = decl.entries.remove(at) else { unreachable!() };
    let fields = rows.iter().map(|row| {
        let row: Vec<String> = row.split(',').map(|c| c.trim().to_string()).collect();
        if row.len() < cols.len() || (!rest && row.len() > cols.len()) {
            return Err(syn::Error::new(field.name.span(), format!("a row of {} columns: ({})", row.len(), row.join(", "))));
        }
        let col = cols.iter().position(|c| c.as_ref() == Some(&field.name))
            .ok_or_else(|| syn::Error::new(field.name.span(), "the field is named by one of the row's columns"))?;
        Ok(Entry::Field(Field { name: Ident::new(&row[col], field.name.span()), ..field.clone() }))
    }).collect::<syn::Result<Vec<_>>>()?;
    decl.entries.splice(at..at, fields);
    next(decl)
}

fn int_bits(ty: &str) -> Option<usize> {
    match ty {
        "u8" => Some(8), "u16" => Some(16), "u32" => Some(32), "u64" => Some(64),
        _ => None,
    }
}

/// A field's width: one for a bool, its integer's, or what #[bits(N)] says.
fn width(f: &Field) -> syn::Result<usize> {
    if let Some(a) = f.attrs.iter().find(|a| a.path().is_ident("bits")) {
        return a.parse_args::<LitInt>()?.base10_parse();
    }
    let ty = f.ty.to_token_stream().to_string();
    match ty.as_str() {
        "bool" => Ok(1),
        t => int_bits(t).ok_or_else(|| syn::Error::new_spanned(&f.ty, "a field of this type gives its width: #[bits(N)]")),
    }
}

fn is_bool(f: &Field) -> bool {
    f.ty.to_token_stream().to_string() == "bool"
}

fn generate(d: &Decl) -> syn::Result<TokenStream> {
    let Decl { attrs, vis, name, bits, word, entries } = d;
    let fields: Vec<&Field> = entries.iter().map(|e| match e { Entry::Field(f) => f, Entry::Rows { .. } => unreachable!() }).collect();
    let size = int_bits(&bits.to_string()).ok_or_else(|| syn::Error::new(bits.span(), "the value's integer: u8, u16, u32 or u64"))?;
    let used: usize = fields.iter().map(|f| width(f)).sum::<syn::Result<_>>()?;
    if used > size {
        return Err(syn::Error::new(name.span(), format!("{used} bits of fields in a {bits}")));
    }
    let pad = (used < size).then(|| {
        let n = LitInt::new(&(size - used).to_string(), Span::call_site());
        quote!(#[bits(#n)] __pad: #bits,)
    });

    // as visible as the struct: bitfield-struct's accessors take a field's
    let value_fields = fields.iter().map(|Field { attrs, name, ty }| quote!(#(#attrs)* #vis #name: #ty));

    let atomic = format_ident!("Atomic{name}");
    let word = word.as_ref().unwrap_or(bits);
    let atomic_word = format_ident!("Atomic{}", {
        let w = word.to_string();
        let mut c = w.chars();
        c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
    });

    let ordering = quote!(::core::sync::atomic::Ordering);
    let (relaxed, seqcst) = (quote!(#ordering::Relaxed), quote!(#ordering::SeqCst));
    let mask = |m: &Ident| {
        let offset = format_ident!("{}_OFFSET", m.to_string().to_uppercase());
        quote!(((1 as #word) << #name::#offset))
    };

    let accessors = fields.iter().map(|f| {
        let (m, ty) = (&f.name, &f.ty);
        let set = format_ident!("set_{m}");
        let swap = format_ident!("swap_{m}");
        let with = format_ident!("with_{m}");
        let get = quote! { #vis fn #m(&self) -> #ty { self.load().#m() } };
        if is_bool(f) {
            let mask = mask(m);
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
            quote! {
                #get
                #vis fn #set(&self, v: #ty) {
                    let _ = self.fetch_update(#relaxed, #relaxed, |o| Some(o.#with(v)));
                }
            }
        }
    });

    let prt = fields.iter().map(|f| {
        let (m, label) = (&f.name, f.name.to_string());
        match f.ty.to_token_stream().to_string().as_str() {
            "bool" => quote! { if self.#m() { sep(out); crate::prt!(out, #label); } },
            t if int_bits(t).is_some() => quote! { sep(out); crate::prt!(out, "{}={}", #label, self.#m()); },
            _ => quote! { sep(out); crate::prt!(out, "{}=", #label); crate::util::ToText::to_text(&self.#m(), out); },
        }
    });

    // on the futex's words: waits on the value, and a one-bit field as a lock
    let bit_locks = fields.iter().copied().filter(|f| is_bool(f)).map(|f| {
        let m = &f.name;
        let (lock, unlock, swap) = (format_ident!("lock_{m}"), format_ident!("unlock_{m}"), format_ident!("swap_{m}"));
        let mask = mask(m);
        quote! {
            /// Set, by whoever finds it clear - waiting while it's set; with
            /// the unlock, a lock.
            #vis fn #lock<E: crate::util::sleep::SleepEnd>(&self, how: crate::util::sleep::Sleep<E>) -> Result<(), E> {
                while self.#swap(true) {
                    self.wait_until(how, |v| !v.#m())?;
                }
                Ok(())
            }

            /// Cleared, releasing what it locked, and its waiters woken.
            #vis fn #unlock(&self) {
                self.0.fetch_and(!#mask, #ordering::Release);
                self.wake();
            }
        }
    });
    let waits = matches!(word.to_string().as_str(), "u8" | "u32" | "usize").then(|| quote! {
        /// Sleep until @cond of the value holds; woken by wake().
        #vis fn wait_until<E: crate::util::sleep::SleepEnd>(&self, how: crate::util::sleep::Sleep<E>,
                                                           mut cond: impl FnMut(#name) -> bool) -> Result<(), E> {
            crate::locking::bitlock::wait_until(&self.0, how, |w| cond(#name::from_bits(w as #bits)))
        }

        /// wait_until(), given up after @timeout jiffies: whether @cond held.
        #vis fn wait_until_timeout(&self, how: crate::util::sleep::Sleep, timeout: ::core::ffi::c_ulong,
                                   mut cond: impl FnMut(#name) -> bool) -> bool {
            crate::locking::bitlock::wait_until_timeout(&self.0, how, timeout, |w| cond(#name::from_bits(w as #bits)))
        }

        /// Wake wait_until()'s waiters, after the change they wait for.
        #vis fn wake(&self) {
            crate::locking::bitlock::wake(&self.0)
        }

        #(#bit_locks)*
    });

    let doc = format!("{name}, atomic: one-bit fields by one atomic op each, the rest - and any \
                       change of several - by cmpxchg over the value.");
    Ok(quote! {
        #[::bitfield_struct::bitfield(#bits)]
        #(#attrs)*
        #vis struct #name {
            #(#value_fields,)*
            #pad
        }

        unsafe impl ::pin_init::Zeroable for #name {}

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
        #vis struct #atomic(::core::sync::atomic::#atomic_word);

        unsafe impl ::pin_init::Zeroable for #atomic {}

        #[allow(dead_code)]
        impl #atomic {
            #vis const fn new(v: #name) -> Self {
                Self(::core::sync::atomic::#atomic_word::new(v.into_bits() as #word))
            }

            #vis fn load(&self) -> #name {
                #name::from_bits(self.0.load(#relaxed) as #bits)
            }

            #vis fn store(&self, v: #name) {
                self.0.store(v.into_bits() as #word, #relaxed)
            }

            #vis fn compare_exchange(&self, current: #name, new: #name, success: #ordering, failure: #ordering)
                -> Result<#name, #name>
            {
                self.0.compare_exchange(current.into_bits() as #word, new.into_bits() as #word, success, failure)
                    .map(|w| #name::from_bits(w as #bits))
                    .map_err(|w| #name::from_bits(w as #bits))
            }

            #vis fn fetch_update(&self, set: #ordering, fetch: #ordering, mut f: impl FnMut(#name) -> Option<#name>)
                -> Result<#name, #name>
            {
                self.0.fetch_update(set, fetch, |w| f(#name::from_bits(w as #bits)).map(|v| v.into_bits() as #word))
                    .map(|w| #name::from_bits(w as #bits))
                    .map_err(|w| #name::from_bits(w as #bits))
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
    })
}
