// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Init)]`: `T::init()`, a pin-init initializer for a struct.
//!
//! Every field is zeroed but a field with `#[init(EXPR)]`, which is
//! initialized from EXPR: a type's real initializer, or a struct's own
//! init().
//!
//! An initializer may fail - a percpu rwsem allocates - with a BchError, or
//! an error that converts to one; the infallible ones convert from
//! Infallible. Then the fields initialized so far are dropped, and init()
//! fails with it.
//!
//! Forgetting one is a compile error, not a zeroed lock: a field without
//! `#[init]` is zeroed - by place.rs's field_init(), unless a placed type
//! lives there - which only a Zeroable type takes: plain data is, a type
//! with invariants, or a struct holding one, isn't. core's atomics are
//! zeroed as values: they're as Zeroable as their integers, but pin_init has
//! no impl for them, and the orphan rule allows none here.
//!
//! The struct is dropped whole, after its teardown: so a field's Drop must
//! do nothing to what the teardown already put away.
//!
//! The struct needs `#[pin_data]` too, ahead of the derives - pin_init!()
//! needs its projections - and `#[pin]` on a field whose initializer is a
//! PinInit, not an Init: a lock, a struct's init().
//!
//! `#[init_args(PARAMS)]` on the struct: parameters init() takes, for its
//! fields' #[init] initializers to use - what a field can't be zeroed
//! before, as a BackRef to its owner: `#[init_args(fs: &Fs)]`, init(fs). The
//! initializer borrows them: of the parameters, at most one a reference, so
//! its lifetime is the elided one.
//!
//! And each field is a Field<N>, one without #[init] is zeroed only if no
//! placed type lives there, and a placed struct's init() takes its At: a
//! value's place - place.rs. A placed type with #[init_args] is given them
//! by its field's `#[placed_args(EXPRS)]`. A Placed<C, A> field is made with at(), around
//! its #[init]: the A it names must be its own place, or it won't compile.
//!
//! A field with `#[init_locked(P, "name")]` is a Locked whose data is placed
//! in it, `#[placed(Base.field.*)]`: made here, new_lock!(<- the data's
//! init(), P, "name"), the data given its AtInner. Each struct's Enclosing is
//! here too: itself, or - placed in a Wrapper - its parent, so what's placed
//! in a Locked's data has the struct holding the lock for its parent.
//!
//! A field `w: ReffedWork<f, R>` - a work item, running fn f, holding a ref
//! on R's count - is made here too, with all it needs, as the field is the
//! one place it's declared: f as a type, a braced struct beside the fn f in
//! scope here, and its WorkFunc - the field's At, and run(), the call to f;
//! and the field's initializer, ReffedWork::new() given the At, with a
//! lockdep class of its own - a static in this generated code, one per
//! field, as a static in ReffedWork's generic code wouldn't be.

use proc_macro2::{Literal, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens as _};
use syn::punctuated::Punctuated;
use syn::{Attribute, Data, DataStruct, DeriveInput, Error, FnArg, GenericArgument, Ident, PathArguments, Token,
          Type};

use crate::place;

pub(crate) fn derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let args = match input.attrs.iter().find(|a| a.path().is_ident("init_args")) {
        Some(a) => Some(a.parse_args_with(Punctuated::<FnArg, Token![,]>::parse_terminated)?),
        None => None,
    };
    // the name C has it by, as the lockdep classes are named
    let parent = match input.attrs.iter().find(|a| a.path().is_ident("type_name_of")) {
        Some(a) => a.parse_args::<Ident>()?.to_string(),
        None => input.ident.to_string(),
    };
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(Error::new_spanned(&input, format!("{parent}: #[derive(Init)] is for a struct")));
    };
    let inst = &input.ident;
    let (impl_g, ty_g, where_c) = input.generics.split_for_impl();
    let inst_ty = quote!(#inst #ty_g);
    let has_lifetimes = input.generics.lifetimes().next().is_some();

    let mut inits = Vec::new();
    let mut field_impls = Vec::new();
    let mut at: Option<&Type> = None;
    for f in fields {
        let Some(name) = &f.ident else {
            return Err(Error::new_spanned(f, format!("{parent}: #[derive(Init)] is for named fields")));
        };
        let (ty, has) = (&f.ty, |n: &str| f.attrs.iter().any(|a| a.path().is_ident(n)));
        let cfg: Vec<_> = f.attrs.iter().filter(|a| a.path().is_ident("cfg")).collect();
        let cfg = quote!(#(#cfg)*);
        field_impls.push(place::field_impl(&cfg, &impl_g, &inst_ty, where_c, name, ty));

        let init = match f.attrs.iter().find(|a| a.path().is_ident("init")) {
            Some(a) => Some(initializer(a, name)?),
            None => None,
        };
        inits.push(cfg.clone());
        if has("placed_at") {
            at = Some(ty);
            inits.push(quote!(#name: at,));
            continue;
        }
        if let Some(a) = f.attrs.iter().find(|a| a.path().is_ident("placed_args")) {
            let args = a.parse_args_with(Punctuated::<syn::Expr, Token![,]>::parse_terminated)?;
            let args = args.iter();
            let n = name.to_string();
            inits.push(quote!(#name <- unsafe {
                <Self as crate::util::place::InitAt<{ crate::util::place::name(#n) }, _>>::init((#(#args,)*))
            },));
            continue;
        }
        if let Some(a) = f.attrs.iter().find(|a| a.path().is_ident("init_locked")) {
            let args: TokenStream = a.parse_args()?;
            let n = name.to_string();
            /* a placed lock: its At around it, its data's AtInner in it */
            let field = if is_placed(ty) {
                quote!(crate::util::place::at::<Self, { crate::util::place::name(#n) }, _, crate::errcode::BchError>(
                    |at| crate::util::place::Placed::new(at, lock(inner))))
            } else {
                quote!(lock(inner))
            };
            inits.push(quote!(#name <- {
                let lock = |inner| crate::new_lock!(
                    <- <<<Self as crate::util::place::Field<{ crate::util::place::name(#n) }>>::Type
                        as crate::util::place::Wrapper>::Inner>::init(inner), #args);
                unsafe {
                    crate::util::place::at_inner::<Self, { crate::util::place::name(#n) }, _, crate::errcode::BchError>(
                        |inner| #field)
                }
            },));
            continue;
        }
        if is_placed(ty) {
            let value = init.unwrap_or_else(|| quote!(::pin_init::zeroed()));
            let n = name.to_string();
            inits.push(quote!(#name <- {
                let value = #value;
                unsafe {
                    crate::util::place::at::<Self, { crate::util::place::name(#n) }, _, crate::errcode::BchError>(
                        |at| crate::util::place::Placed::new(at, value))
                }
            },));
            continue;
        }
        if let (None, Some(func)) = (&init, reffed_work_fn(&parent, name, ty)?) {
            if has_lifetimes {
                return Err(Error::new_spanned(name, format!("{parent}.{name}: a ReffedWork's parent has no lifetimes")));
            }
            field_impls.push(work_func(&cfg, &inst_ty, name, func));
            let w = work_init(&parent, name)?;
            inits.push(quote!(#name <- #w,));
            continue;
        }
        inits.push(match init {
            Some(init) => quote!(#name <- #init,),
            // SAFETY: core's atomics are their integers, and zero is one -
            // Zeroable, but for the impl the orphan rule doesn't allow us
            None if crate::is_core_atomic(ty) =>
                quote!(#name: unsafe { ::core::mem::zeroed() },),
            None => {
                let init = place::field_init(name);
                quote!(#name <- #init,)
            }
        });
    }

    // What's placed in it has it for its parent - or, it placed in a Wrapper,
    // the struct holding that
    let enclosing = match at {
        Some(ty) if is_at_inner(ty) => quote! {
            #[automatically_derived]
            unsafe impl #impl_g crate::util::place::Enclosing for #inst_ty #where_c {
                type Visible = <Self as crate::util::place::Contained>::Parent;
                unsafe fn visible(p: *mut Self) -> *mut Self::Visible {
                    unsafe { crate::util::place::container_of_raw(p) }
                }
            }
        },
        _ => quote! {
            #[automatically_derived]
            unsafe impl #impl_g crate::util::place::Enclosing for #inst_ty #where_c {
                type Visible = Self;
                unsafe fn visible(p: *mut Self) -> *mut Self {
                    p
                }
            }
        },
    };

    // An At is crate-private, as place.rs is - so is an init() taking one
    let (vis, at_param) = match at {
        Some(ty) => (quote!(pub(crate)), Some(quote!(at: #ty))),
        None => (quote!(pub), None),
    };
    let args = args.iter().flatten();
    let borrows = args.clone().any(|a| a.to_token_stream().to_string().contains('&')).then(|| quote!(+ '_));
    let params = at_param.into_iter().chain(args.map(|a| a.to_token_stream()));

    Ok(quote! {
        #[automatically_derived]
        impl #impl_g #inst_ty #where_c {
            /// Zeroed, but for its #[init] fields: see #[derive(Init)].
            #vis fn init(#(#params),*) -> impl ::pin_init::PinInit<Self, crate::errcode::BchError> #borrows {
                ::pin_init::pin_init!(#inst {
                    #(#inits)*
                }? crate::errcode::BchError)
            }
        }
        #(#field_impls)*
        #enclosing
    })
}

/// #[ATTR(EXPR)] or #[ATTR(EXPR, ERRCODE)]'s initializer: EXPR, or EXPR
/// failing with BCH_ERR_ERRCODE - by init_or(). #[late]'s too.
pub(crate) fn initializer(a: &Attribute, name: &Ident) -> syn::Result<TokenStream> {
    let toks: Vec<TokenTree> = a.parse_args::<TokenStream>()?.into_iter().collect();
    // ERRCODE is an ident after the last comma outside brackets - a comma in
    // EXPR's turbofish is followed by more than that
    let errcode = match toks.iter().rposition(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ',')) {
        Some(i) if i > 0 && i + 2 == toks.len() => match &toks[i + 1] {
            TokenTree::Ident(err) => Some((i, format_ident!("BCH_ERR_{err}"))),
            _ => None,
        },
        _ => None,
    };
    match errcode {
        Some((i, err)) => {
            let init: TokenStream = toks[..i].iter().cloned().collect();
            Ok(quote!(crate::util::alloc::init_or(#init, crate::errcode::Errcode::#err)))
        }
        None if !toks.is_empty() => Ok(toks.into_iter().collect()),
        None => {
            let attr = a.path().to_token_stream();
            Err(Error::new_spanned(a, format!("{name}: #[{attr}(EXPR)] or #[{attr}(EXPR, ERRCODE)]: the field's \
                                               initializer, and the Errcode its failure is")))
        }
    }
}

/// Whether field type @ty is a Placed<C, A>.
fn is_placed(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Placed"))
}

/// Whether At type @ty is an AtInner: its struct placed in a Wrapper.
fn is_at_inner(ty: &Type) -> bool {
    matches!(ty, Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "AtInner"))
}

// ── ReffedWork fields ───────────────────────────────────────────────────────

/// f, if field type @ty is a ReffedWork<f, R>: f a bare name, the fn in
/// scope here that it runs - its type is made here, beside it.
fn reffed_work_fn<'a>(parent: &str, name: &Ident, ty: &'a Type) -> syn::Result<Option<&'a Ident>> {
    let Type::Path(p) = ty else { return Ok(None) };
    let Some(last) = p.path.segments.last().filter(|s| s.ident == "ReffedWork") else { return Ok(None) };
    let PathArguments::AngleBracketed(args) = &last.arguments else { return Ok(None) };
    match args.args.first() {
        Some(GenericArgument::Type(Type::Path(f))) if f.qself.is_none() && f.path.get_ident().is_some() =>
            Ok(f.path.get_ident()),
        f => Err(Error::new_spanned(f, format!("{parent}.{name}: a ReffedWork's function is named bare, as the \
                                                fn in scope here - its type is made beside it: `use` it"))),
    }
}

/// Field @name's work function f, as a type: f's struct, and its WorkFunc -
/// the field's At, and the call.
fn work_func(cfg: &TokenStream, inst_ty: &TokenStream, name: &Ident, f: &Ident) -> TokenStream {
    let n = name.to_string();
    quote! {
        #cfg
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub(crate) struct #f {}
        #cfg
        #[automatically_derived]
        // SAFETY: the field's - the one of type ReffedWork<f, _>, its
        // initializer init()'s, below
        unsafe impl crate::util::workqueue::WorkFunc for #f {
            type At = crate::util::place::At<#inst_ty, { crate::util::place::name(#n) }>;
            fn run(p: &#inst_ty) {
                #f(p)
            }
        }
    }
}

/// Field @name's initializer, of @parent: the work, given its At, its
/// lockdep class named for the field.
fn work_init(parent: &str, name: &Ident) -> syn::Result<TokenStream> {
    let n = name.to_string();
    let class: Literal = format!("c\"{parent}.{name}\"").parse()
        .map_err(|_| Error::new_spanned(name, format!("{parent}.{name}: no C string of the name")))?;
    Ok(quote! {
        {
            #[cfg(kernel)]
            let w = |at| crate::util::workqueue::ReffedWork::new(at, #class, ::kernel::static_lock_class!());
            #[cfg(not(kernel))]
            let w = |at| crate::util::workqueue::ReffedWork::new(at);
            // SAFETY: run into the field, by pin_init!()
            unsafe {
                crate::util::place::at::<Self, { crate::util::place::name(#n) }, _, crate::errcode::BchError>(w)
            }
        }
    })
}
