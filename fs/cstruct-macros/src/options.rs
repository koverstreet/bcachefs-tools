// SPDX-License-Identifier: GPL-2.0
//! `#[derive(Options)]`: bch_opts, the options - a field each, described by
//! its `#[opt(...)]` and its doc comment, the option's help:
//!
//! ```text
//! /// Filesystem block size
//! #[opt(uint(512, 1 << 15), flags = fs | format | human_readable, sb = bch_sb::block_size,
//!       default = 4 << 10, hint = "size")]
//! pub block_size: u16,
//! ```
//!
//! The type first: `bool`, `uint(min, max)`, `str(Enum)`, `str_nolimit(Enum)`,
//! `bitfield(Enum[, allowed])`, `with(OPT_FN)`, `str_member(member_field)` -
//! a string or bitfield option's choices are its enum's Names. Then, each
//! optional: `flags`, the OPT_* when it may be set, lowercase; `sb`, its
//! superblock field's getter, `struct::getter`, its setter set_getter, or
//! `struct::getter / setter`; `default`; `hint`.
//!
//! It makes, from the fields in order - each one's id, Opt_name:
//!
//! ```text
//! bch_opt_id               the ids, and bch2_opts_nr - a c_enum!, for C too
//! bch_opts::TABLE          each option's Opt (opts.rs), by id
//! bch_opts::DEFAULTS       each one's default, all defined
//! get(), set(), defined(), apply()   by id; set() sets it defined, in the
//!                          field `defined`, a bch_opts_mask
//! name(), set_name()       each option's value - a bool option's as bool, a
//!                          string option's as its enum - and setting it,
//!                          defined
//! name_or_default()        its value, or its default if it isn't defined
//! PublishedOpts            bch_opts laid out the same, each option a Volatile:
//!                          the filesystem's, c->opts, which anyone reads an
//!                          option at a time and its lock's holder writes -
//!                          the readers above, and load(), a bch_opts copy
//! PublishedOptsWriter      it, as its writer has it: the writers above
//! ```
//!
//! A field without #[opt] isn't an option: `defined`.

use proc_macro2::{Literal, TokenStream};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parenthesized, token, Data, DataStruct, DeriveInput, Error, Expr, ExprLit, Fields, Ident, Lit, LitStr,
          Meta, Token, Type};

/// An #[opt(...)]: its type, `kind(args)`, then its `key = value`s.
struct OptAttr {
    kind:    Ident,
    args:    Vec<Expr>,
    flags:   Vec<Ident>,
    /// host::getter [/ setter]
    sb:      Option<(Ident, Ident, Option<Ident>)>,
    default: Option<Expr>,
    hint:    Option<Expr>,
}

impl Parse for OptAttr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let kind: Ident = input.parse()
            .map_err(|e| Error::new(e.span(), "the type first: bool, uint(min, max), str(Enum), ..."))?;
        let args = match input.peek(token::Paren) {
            true => {
                let inner;
                parenthesized!(inner in input);
                Punctuated::<Expr, Token![,]>::parse_terminated(&inner)?.into_iter().collect()
            }
            false => Vec::new(),
        };
        let mut a = OptAttr { kind, args, flags: Vec::new(), sb: None, default: None, hint: None };
        while input.parse::<Option<Token![,]>>()?.is_some() && !input.is_empty() {
            let k: Ident = input.parse()?;
            input.parse::<Token![=]>().map_err(|e| Error::new(e.span(), format!("{k}: expected key = value")))?;
            match k.to_string().as_str() {
                "flags" => a.flags = Punctuated::<Ident, Token![|]>::parse_separated_nonempty(input)?
                    .into_iter().collect(),
                "sb" => {
                    let host = input.parse()?;
                    input.parse::<Token![::]>()?;
                    let getter = input.parse()?;
                    let setter = match input.parse::<Option<Token![/]>>()? {
                        Some(_) => Some(input.parse()?),
                        None => None,
                    };
                    a.sb = Some((host, getter, setter));
                }
                "default" => a.default = Some(input.parse()?),
                "hint" => a.hint = Some(input.parse()?),
                _ => return Err(Error::new_spanned(&k, format!("{k}: not an #[opt] key"))),
            }
        }
        if !input.is_empty() {
            return Err(input.error("expected `, key = value`"));
        }
        Ok(a)
    }
}

/// A field with an #[opt]: an option.
struct Opt<'a> {
    name: &'a Ident,
    ty:   &'a Type,
    attr: OptAttr,
    doc:  Vec<String>,
}

impl Opt<'_> {
    fn id(&self) -> Ident {
        format_ident!("Opt_{}", self.name)
    }
}

/// An option's Opt, and what its set and get are.
fn option(f: &Opt<'_>) -> syn::Result<TokenStream> {
    let name = f.name;
    let a = &f.attr;
    let o = quote!(crate::opts);
    let names = |t: &Expr| quote!(crate::names_by_value!(c::#t));

    let (ty, min, max) = match (a.kind.to_string().as_str(), a.args.as_slice()) {
        ("bool", []) => (quote!(#o::OptType::Bool), quote!(0), quote!(1)),
        ("uint", [min, max]) => (quote!(#o::OptType::Uint), quote!((#min) as u64), quote!((#max) as u64)),
        ("str", [t]) => {
            let n = names(t);
            (quote!(#o::OptType::Str(#n)), quote!(0), quote!((#n.len() - 1) as u64))
        }
        ("str_nolimit", [t]) => {
            let n = names(t);
            (quote!(#o::OptType::Str(#n)), quote!(0), quote!(u64::MAX))
        }
        ("bitfield", [t]) => {
            let n = names(t);
            (quote!(#o::OptType::Bitfield { choices: #n, allowed: u64::MAX }), quote!(0), quote!(0))
        }
        ("bitfield", [t, allowed]) => {
            let n = names(t);
            (quote!(#o::OptType::Bitfield { choices: #n, allowed: (#allowed) as u64 }), quote!(0), quote!(0))
        }
        ("with", [f]) => (quote!(#o::OptType::Fn(&#f)), quote!(0), quote!(0)),
        ("str_member", [m]) => (quote!(#o::OptType::StrMember {
            get: |m| &m.#m,
            get_mut: |m| &mut m.#m,
            size: #o::size_of_field::<c::bch_member, _>(|m| &m.#m),
        }), quote!(0), quote!(0)),
        (kind, _) => return Err(Error::new_spanned(&a.kind, format!("{name}: #[opt]: {kind}: not an option type"))),
    };

    let flags = match a.flags.is_empty() {
        true => quote!(0),
        false => {
            let f = a.flags.iter().map(|f| format_ident!("OPT_{}", f.to_string().to_uppercase()));
            quote!(#(#f)|*)
        }
    };
    let sb = match &a.sb {
        None => quote!(None),
        Some((host, getter, setter)) => {
            // the head: what either state of a superblock has its fields on
            let host = match host == "bch_sb" {
                true => format_ident!("bch_sb__head"),
                false => host.clone(),
            };
            let setter = setter.clone().unwrap_or_else(|| format_ident!("set_{getter}"));
            quote!(Some(&#o::Field::<c::#host, _> { get: |t| t.#getter(), set: |t, v| t.#setter(v) }
                        as &dyn #o::SbOpt))
        }
    };
    let hint = match &a.hint {
        Some(h) => quote!(Some(#h)),
        None => quote!(None),
    };
    let help = match f.doc.is_empty() {
        true  => quote!(None),
        false => {
            let lines: Vec<&str> = f.doc.iter().map(|l| l.strip_prefix(' ').unwrap_or(l)).collect();
            let help = lines.join("\n");
            quote!(Some(#help))
        }
    };

    let id = f.id();
    let name = name.to_string();
    Ok(quote! {
        #o::Opt {
            id:    bch_opt_id::#id,
            name:  #name,
            flags: c::opt_flags::from_bits_retain(#flags),
            ty:    #ty,
            min:   #min,
            max:   #max,
            sb:    #sb,
            hint:  #hint,
            help:  #help,
        }
    })
}

enum Accessor {
    Bool,
    /// A string option's choices' enum, through c_enum!'s from_bits() and
    /// into_bits(): a closed enum's from_bits() panics on a value that isn't
    /// one, and a string option's value is an index into its choices - so
    /// one, unless something set it around the parser.
    Enum(Expr),
    Field,
}

/// What an option's accessors take and give: a bool option's a bool, a
/// string option's its enum, the rest the field's own type.
fn accessor_type(a: &OptAttr) -> Accessor {
    match (a.kind.to_string().as_str(), a.args.as_slice()) {
        ("bool", []) => Accessor::Bool,
        ("str" | "str_nolimit", [e]) => Accessor::Enum(e.clone()),
        _ => Accessor::Field,
    }
}

type Conv = Box<dyn Fn(TokenStream) -> TokenStream>;

/// What option @f's accessors take and give; that from the field's value, an
/// expression; and the field's value from one of it, `v`.
fn accessor(f: &Opt<'_>) -> (TokenStream, Conv, TokenStream) {
    let ty = f.ty;
    match accessor_type(&f.attr) {
        Accessor::Bool    => (quote!(bool), Box::new(|x| quote!(#x != 0)), quote!(v as #ty)),
        Accessor::Enum(e) => (quote!(c::#e), Box::new(move |x| quote!(c::#e::from_bits(#x as _))),
                              quote!(v.into_bits() as #ty)),
        Accessor::Field   => (quote!(#ty), Box::new(|x| x), quote!(v)),
    }
}

/// The option's default, from its #[opt]: 0 without one.
fn default(f: &Opt<'_>) -> TokenStream {
    let ty = f.ty;
    match &f.attr.default {
        Some(d) => quote!((#d) as #ty),
        None => quote!(0),
    }
}

/// A field's doc comment, a line per attribute.
fn doc_lines(f: &syn::Field) -> Vec<String> {
    f.attrs.iter().filter_map(|a| match &a.meta {
        Meta::NameValue(nv) if nv.path.is_ident("doc") => match &nv.value {
            Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    }).collect()
}

pub(crate) fn derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let Data::Struct(DataStruct { fields: Fields::Named(fields), .. }) = &input.data else {
        return Err(Error::new_spanned(&input, "for a struct with named fields"));
    };
    let mut opts = Vec::new();
    for f in &fields.named {
        let Some(a) = f.attrs.iter().rev().find(|a| a.path().is_ident("opt")) else { continue };
        let ident = f.ident.as_ref().expect("named fields");
        let attr = a.parse_args().map_err(|e| Error::new(e.span(), format!("{ident}: #[opt]: {e}")))?;
        opts.push(Opt { name: ident, ty: &f.ty, attr, doc: doc_lines(f) });
    }

    let labels = opts.iter().map(|f| LitStr::new(&f.name.to_string(), f.name.span()));
    let ids: Vec<Ident> = opts.iter().map(Opt::id).collect();
    let table = opts.iter().map(option).collect::<syn::Result<Vec<_>>>()?;
    let defaults = opts.iter().map(|f| {
        let (n, d) = (f.name, default(f));
        quote!(opts.#n = #d;)
    });

    // What reads and writes a value of each option, generic over how a
    // field's loaded and stored: the plain struct's fields, or its published
    // mirror's Volatiles. @load(n) is field n's value; @store(n, v) sets it;
    // @defined(id) and @define(id) test and set the defined bit of option id,
    // an expression.
    struct Place<'a> {
        load:    &'a dyn Fn(&Ident) -> TokenStream,
        store:   &'a dyn Fn(&Ident, TokenStream) -> TokenStream,
        defined: &'a dyn Fn(TokenStream) -> TokenStream,
        define:  &'a dyn Fn(TokenStream) -> TokenStream,
        recv:    TokenStream,
    }

    let readers = |p: &Place<'_>| -> TokenStream {
        let get = opts.iter().map(|f| {
            let (id, v) = (f.id(), (p.load)(f.name));
            quote!(bch_opt_id::#id => #v as u64,)
        });
        let accessors = opts.iter().map(|f| {
            let (ty, conv, _) = accessor(f);
            let (n, id) = (f.name, f.id());
            let or_default = format_ident!("{n}_or_default");
            let (value, dflt) = (conv((p.load)(n)), conv(quote!(#name::DEFAULTS.#n)));
            quote! {
                pub fn #n(&self) -> #ty { #value }
                pub fn #or_default(&self) -> #ty {
                    if self.defined(bch_opt_id::#id) { self.#n() } else { #dflt }
                }
            }
        });
        let defined = (p.defined)(quote!(id));
        quote! {
            /// Its value for @id - defined or not.
            pub fn get(&self, id: bch_opt_id) -> u64 {
                match id {
                    #(#get)*
                    _ => panic!("not an option: {}", id.0),
                }
            }

            pub fn defined(&self, id: bch_opt_id) -> bool {
                #defined
            }

            #(#accessors)*
        }
    };

    let writers = |p: &Place<'_>| -> TokenStream {
        let recv = &p.recv;
        let set = opts.iter().map(|f| {
            let (id, ty) = (f.id(), f.ty);
            let s = (p.store)(f.name, quote!(v as #ty));
            quote!(bch_opt_id::#id => #s,)
        });
        let apply = opts.iter().map(|f| {
            let (n, id) = (f.name, f.id());
            let (s, d) = ((p.store)(n, quote!(src.#n)), (p.define)(quote!(bch_opt_id::#id)));
            quote! {
                if src.defined.test(bch_opt_id::#id) {
                    #s;
                    #d;
                }
            }
        });
        let setters = opts.iter().map(|f| {
            let (ty, _, back) = accessor(f);
            let (n, id) = (f.name, f.id());
            let set_n = format_ident!("set_{n}");
            let (s, d) = ((p.store)(n, back), (p.define)(quote!(bch_opt_id::#id)));
            quote! {
                pub fn #set_n(#recv, v: #ty) {
                    #s;
                    #d;
                }
            }
        });
        let define = (p.define)(quote!(id));
        quote! {
            /// Set, and defined.
            pub fn set(#recv, id: bch_opt_id, v: u64) {
                match id {
                    #(#set)*
                    _ => panic!("not an option: {}", id.0),
                }
                #define;
            }

            /// Those @src defines, set: bch2_opts_apply().
            pub fn apply(#recv, src: &#name) {
                #(#apply)*
            }

            #(#setters)*
        }
    };

    let plain = Place {
        load:    &|n| quote!(self.#n),
        store:   &|n, v| quote!(self.#n = #v),
        defined: &|id| quote!(self.defined.test(#id)),
        define:  &|id| quote!(self.defined.set(#id)),
        recv:    quote!(&mut self),
    };
    let published = Place {
        load:    &|n| quote!(self.#n.get()),
        store:   &|n, v| quote!(self.#n.set(#v)),
        defined: &|id| quote!(self.defined.test(#id.0 as usize)),
        define:  &|id| quote!(self.defined.set(#id.0 as usize)),
        recv:    quote!(&self),
    };
    let (plain_readers, plain_writers) = (readers(&plain), writers(&plain));
    let (pub_readers, pub_writers) = (readers(&published), writers(&published));

    let (names, tys): (Vec<_>, Vec<_>) = opts.iter().map(|f| (f.name, f.ty)).unzip();
    let nr = Literal::usize_unsuffixed(opts.len());
    let pub_doc = [format!(" {name}, read an option at a time while its lock's holder writes it,"),
                   format!(" through PublishedOptsWriter. Laid out as {name} is: to C, one.")];

    Ok(quote! {
        c_enum! {
            #[open]
            pub enum bch_opt_id: u32 {
                #(#[label(#labels)] #ids,)*
                bch2_opts_nr,
            }
        }

        const _: () = assert!(#nr <= BCH_OPTS_MAX as usize, "more options than BCH_OPTS_MAX");

        impl #name {
            /// Each option, by bch_opt_id.
            pub const TABLE: [crate::opts::Opt; #nr] = [#(#table),*];

            /// Each option's default, all defined.
            pub const DEFAULTS: Self = {
                // SAFETY: integers, and the defined bits'
                let mut opts: Self = unsafe { core::mem::zeroed() };
                #(#defaults)*
                let mut i = 0;
                while i < #nr {
                    opts.defined.set(bch_opt_id(i as u32));
                    i += 1;
                }
                opts
            };

            #plain_readers
            #plain_writers
        }

        crate::published! {
            #(#[doc = #pub_doc])*
            pub struct PublishedOpts {
                #(pub #names: crate::util::projections::Volatile<#tys>,)*
                pub defined: AtomicOptsMask,
            }
        }

        // SAFETY: Volatile words: zeroes are options, none defined
        unsafe impl ::pin_init::Zeroable for PublishedOpts {}

        // Field for field as the options struct, each Volatile its type's
        // size: checked
        const _: () = {
            assert!(core::mem::size_of::<#name>() == core::mem::size_of::<PublishedOpts>());
            assert!(core::mem::align_of::<#name>() == core::mem::align_of::<PublishedOpts>());
        };

        impl PublishedOpts {
            /// A copy: each option as it was when it was read, not all as of one
            /// moment.
            pub fn load(&self) -> #name {
                #name {
                    #(#names: self.#names.get(),)*
                    defined: bch_opts_mask::load(&self.defined),
                }
            }

            #pub_readers
        }

        impl PublishedOptsWriter {
            #pub_writers
        }
    })
}
