// bcachefs: the docs are ../README.md - not include_str!()'d, as upstream does:
// the module build's source (install_dkms) has only the .rs.
//! Struct-like procedural macro for bitfields: see README.md.
#![warn(clippy::unwrap_used)]

use proc_macro as pc;
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote, ToTokens};
use std::{fmt, stringify};
use syn::spanned::Spanned;

mod attr;
use attr::*;
mod bitenum;
mod traits;

/// Creates a bitfield for this struct.
///
/// The arguments first, have to begin with the integer type of the bitfield:
/// For example: `#[bitfield(u64)]`.
///
/// It can contain the following additional parameters, like the `debug` argument
/// for disabling the `Debug` trait generation (`#[bitfield(u64, debug = false)]`).
///
/// Parameters of the `bitfield` attribute:
/// - the bitfield integer type (required)
/// - `repr` specifies the bitfield's representation in memory
/// - `from` to specify a conversion function from repr to the bitfield's integer type
/// - `into` to specify a conversion function from the bitfield's integer type to repr
/// - `new` to disable the `new` function generation
/// - `binread` to enable the `BinRead` trait generation
/// - `binwrite` to enable the `BinWrite` trait generation
/// - `binrw` to enable both `BinRead` and `BinWrite` trait generation
/// - `clone` to disable the `Clone` trait generation
/// - `debug` to disable the `Debug` trait generation
/// - `defmt` to enable the `defmt::Format` trait generation
/// - `default` to disable the `Default` trait generation
/// - `hash` to generate the `Hash` trait
/// - `order` to specify the bit order (Lsb, Msb)
/// - `conversion` to disable the generation of `into_bits` and `from_bits`
///
/// > For `new`, `clone`, `debug`, `defmt` or `default`, you can either use booleans
/// > (`#[bitfield(u8, debug = false)]`) or cfg attributes
/// > (`#[bitfield(u8, debug = cfg(test))]`) to enable/disable them.
///
/// Parameters of the `bits` attribute (for fields):
/// - the number of bits
/// - `access` to specify the access mode (RW, RO, WO, None)
/// - `default` to set a default value
/// - `into` to specify a conversion function from the field type to the bitfield type
/// - `from` to specify a conversion function from the bitfield type to the field type
#[proc_macro_attribute]
pub fn bitfield(args: pc::TokenStream, input: pc::TokenStream) -> pc::TokenStream {
    match bitfield_inner(args.into(), input.into()) {
        Ok(result) => result.into(),
        Err(e) => e.into_compile_error().into(),
    }
}

/// Implements the constant `into_bits` and `from_bits` functions for the given enum.
///
/// # Parameters
///
/// - `into`: Enable/disable the `into_bits` function generation.
/// - `from`: Enable/disable the `from_bits` function generation.
/// - `all`: Enable/disable the `all` function generation.
///
/// > For `into`, `from`, and `all`, you can either use booleans
/// > (`#[bitfield(u8, debug = false)]`) or cfg attributes
/// > (`#[bitfield(u8, debug = cfg(test))]`) to enable/disable them.
///
/// # Examples
///
/// ```rust
/// use bitfield_struct::bitenum;
///
/// #[bitenum(all = false)]
/// #[repr(u8)]
/// #[derive(Debug, PartialEq, Eq)]
/// enum MyEnum {
///     #[fallback]
///     A = 0,
///     B = 1,
///     C = 2,
/// }
///
/// assert_eq!(MyEnum::from_bits(1), MyEnum::B);
/// assert_eq!(MyEnum::C.into_bits(), 2);
/// ```
///
/// Use the `#[fallback]` attribute to specify a fallback variant for invalid bit patterns.
#[proc_macro_attribute]
pub fn bitenum(args: pc::TokenStream, input: pc::TokenStream) -> pc::TokenStream {
    match bitenum::bitenum_inner(args.into(), input.into()) {
        Ok(result) => result.into(),
        Err(e) => e.into_compile_error().into(),
    }
}

fn bitfield_inner(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let input = syn::parse2::<syn::ItemStruct>(input)?;
    let Params {
        ty,
        repr,
        into,
        from,
        bits,
        binread,
        binwrite,
        new,
        clone,
        debug,
        defmt,
        default,
        hash,
        order,
        conversion,
    } = syn::parse2(args)?;

    let span = input.fields.span();
    let name = input.ident;
    let vis = input.vis;

    // We currently only support const generics and where clauses,
    // typed generics and lifetimes are not supported, as they had to be
    // used (e.g., by `PhantomData`) in the generated code as well.
    // This could be added in the future if there is demand for it.
    if input.generics.type_params().next().is_some() || input.generics.lifetimes().next().is_some()
    {
        return Err(s_err(
            span,
            "type parameters and lifetimes are not supported",
        ));
    }
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let attrs: TokenStream = input.attrs.iter().map(ToTokens::to_token_stream).collect();
    let derive = match clone {
        Enable::No => None,
        Enable::Yes => Some(quote! { #[derive(Copy, Clone)] }),
        Enable::Cfg(cfg) => Some(quote! { #[cfg_attr(#cfg, derive(Copy, Clone))] }),
    };

    let syn::Fields::Named(fields) = input.fields else {
        return Err(s_err(span, "only named fields are supported"));
    };

    let mut offset = 0;
    let mut members = Vec::with_capacity(fields.named.len());
    for field in fields.named {
        let f = Member::new(
            ty.clone(),
            bits,
            into.clone(),
            from.clone(),
            field,
            offset,
            order,
        )?;
        offset += f.bits;
        members.push(f);
    }

    if offset < bits {
        return Err(s_err(
            span,
            format!(
                "The bitfield size ({bits} bits) has to be equal to the sum of its fields ({offset} bits). \
                You might have to add padding (a {} bits large field prefixed with \"_\").",
                bits - offset
            ),
        ));
    }
    if offset > bits {
        return Err(s_err(
            span,
            format!(
                "The size of the fields ({offset} bits) is larger than the type ({bits} bits)."
            ),
        ));
    }

    let mut impl_debug = TokenStream::new();
    if let Some(cfg) = debug.cfg() {
        impl_debug.extend(traits::debug(&name, &input.generics, &members, cfg));
    }
    if let Some(cfg) = defmt.cfg() {
        impl_debug.extend(traits::defmt(&name, &input.generics, &members, cfg));
    }
    if let Some(cfg) = hash.cfg() {
        impl_debug.extend(traits::hash(&name, &input.generics, &members, cfg));
    }
    if let Some(cfg) = binread.cfg() {
        impl_debug.extend(traits::binread(&name, &input.generics, &repr, cfg));
    }
    if let Some(cfg) = binwrite.cfg() {
        impl_debug.extend(traits::binwrite(&name, &input.generics, cfg));
    }

    let defaults = members.iter().map(Member::default).collect::<Vec<_>>();

    let impl_new = new.cfg().map(|cfg| {
        let attr = cfg.map(|cfg| quote!(#[cfg(#cfg)]));
        quote! {
            /// Creates a new default initialized bitfield.
            #attr
            #vis const fn new() -> Self {
                let mut this = Self(#from(0));
                #( #defaults )*
                this
            }
        }
    });

    let impl_default = default.cfg().map(|cfg| {
        let attr = cfg.map(|cfg| quote!(#[cfg(#cfg)]));
        quote! {
            #attr
            impl #impl_generics Default for #name #ty_generics #where_clause {
                fn default() -> Self {
                    let mut this = Self(#from(0));
                    #( #defaults )*
                    this
                }
            }
        }
    });

    let conversion = conversion.then(|| {
        quote! {
            /// Convert from bits.
            #vis const fn from_bits(bits: #repr) -> Self {
                Self(bits)
            }
            /// Convert into bits.
            #vis const fn into_bits(self) -> #repr {
                self.0
            }
        }
    });

    Ok(quote! {
        #attrs
        #derive
        #[repr(transparent)]
        #vis struct #name #impl_generics (#repr) #where_clause;

        #[allow(unused_comparisons)]
        #[allow(clippy::unnecessary_cast)]
        #[allow(clippy::assign_op_pattern)]
        #[allow(clippy::double_parens)]
        impl #impl_generics #name #ty_generics #where_clause {
            #impl_new

            #conversion

            #( #members )*
        }

        #[allow(unused_comparisons)]
        #[allow(clippy::unnecessary_cast)]
        #[allow(clippy::assign_op_pattern)]
        #[allow(clippy::double_parens)]
        #impl_default

        impl #impl_generics From<#repr> for #name #ty_generics #where_clause {
            fn from(v: #repr) -> Self {
                Self(v)
            }
        }
        impl #impl_generics From<#name #ty_generics> for #repr #where_clause {
            fn from(v: #name #ty_generics) -> Self {
                v.0
            }
        }

        #impl_debug
    })
}

/// Represents a member where accessor functions should be generated for.
struct Member {
    offset: usize,
    bits: usize,
    base_ty: syn::Type,
    repr_into: Option<syn::Path>,
    repr_from: Option<syn::Path>,
    default: TokenStream,
    inner: Option<MemberInner>,
}

struct MemberInner {
    ident: syn::Ident,
    ty: syn::Type,
    attrs: Vec<syn::Attribute>,
    vis: syn::Visibility,
    into: TokenStream,
    from: TokenStream,
}

impl Member {
    fn new(
        base_ty: syn::Type,
        base_bits: usize,
        repr_into: Option<syn::Path>,
        repr_from: Option<syn::Path>,
        field: syn::Field,
        offset: usize,
        order: Order,
    ) -> syn::Result<Self> {
        let span = field.span();

        let syn::Field {
            mut attrs,
            vis,
            ident,
            ty,
            ..
        } = field;

        let ident = ident.ok_or_else(|| s_err(span, "Not supported"))?;
        let ignore = ident.to_string().starts_with('_');

        let Field {
            bits,
            ty,
            mut default,
            into,
            from,
            access,
        } = parse_field(&base_ty, &attrs, &ty, ignore)?;

        let ignore = ignore || access == Access::None;

        // compute the offset
        let offset = if order == Order::Lsb {
            offset
        } else {
            base_bits - offset - bits
        };

        if bits > 0 && !ignore {
            // overflow check
            if offset + bits > base_bits {
                return Err(s_err(
                    ty.span(),
                    "The sum of the members overflows the type size",
                ));
            };

            // clear conversion expr if not needed
            let (from, into) = match access {
                Access::ReadWrite => (from, into),
                Access::ReadOnly => (from, quote!()),
                Access::WriteOnly => (from, into),
                Access::None => (quote!(), quote!()),
            };

            // auto-conversion from zero
            if default.is_empty() {
                if !from.is_empty() {
                    default = quote!({ let this = 0; #from });
                } else {
                    default = quote!(0);
                }
            }

            // remove our attribute
            attrs.retain(|a| !a.path().is_ident("bits"));

            Ok(Self {
                offset,
                bits,
                base_ty,
                repr_into,
                repr_from,
                default,
                inner: Some(MemberInner {
                    ident,
                    ty,
                    attrs,
                    vis,
                    into,
                    from,
                }),
            })
        } else {
            if default.is_empty() {
                default = quote!(0);
            }

            Ok(Self {
                offset,
                bits,
                base_ty,
                repr_into,
                repr_from,
                default,
                inner: None,
            })
        }
    }

    fn default(&self) -> TokenStream {
        let default = &self.default;

        if let Some(inner) = &self.inner {
            if !inner.into.is_empty() {
                let ident = &inner.ident;
                let with_ident = format_ident!("with_{}", ident);
                return quote!(this = this.#with_ident(#default););
            }
        }

        // fallback when there is no setter
        let offset = self.offset;
        let base_ty = &self.base_ty;
        let repr_into = &self.repr_into;
        let repr_from = &self.repr_from;
        let bits = self.bits as u32;

        quote! {
            let mask = #base_ty::MAX >> (#base_ty::BITS - #bits);
            this.0 = #repr_from(#repr_into(this.0) | (((#default as #base_ty) & mask) << #offset));
        }
    }
}

impl ToTokens for Member {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let Self {
            offset,
            bits,
            base_ty,
            repr_into,
            repr_from,
            default: _,
            inner:
                Some(MemberInner {
                    ident,
                    ty,
                    attrs,
                    vis,
                    into,
                    from,
                }),
        } = self
        else {
            return Default::default();
        };

        let ident_str = ident.to_string().to_uppercase();
        let ident_upper = Ident::new(
            ident_str.strip_prefix("R#").unwrap_or(&ident_str),
            ident.span(),
        );

        let with_ident = format_ident!("with_{}", ident);
        let with_ident_checked = format_ident!("with_{}_checked", ident);
        let set_ident = format_ident!("set_{}", ident);
        let set_ident_checked = format_ident!("set_{}_checked", ident);
        let bits_ident = format_ident!("{}_BITS", ident_upper);
        let offset_ident = format_ident!("{}_OFFSET", ident_upper);

        let location = format!("\n\nBits: {offset}..{}", offset + bits);

        let doc: TokenStream = attrs
            .iter()
            .filter(|a| !a.path().is_ident("bits"))
            .map(ToTokens::to_token_stream)
            .collect();

        tokens.extend(quote! {
            const #bits_ident: usize = #bits;
            const #offset_ident: usize = #offset;
        });

        if !from.is_empty() {
            tokens.extend(quote! {
                #doc
                #[doc = #location]
                #vis const fn #ident(&self) -> #ty {
                    let mask = #base_ty::MAX >> (#base_ty::BITS - Self::#bits_ident as u32);
                    let this = (#repr_into(self.0) >> Self::#offset_ident) & mask;
                    #from
                }
            });
        }

        if !into.is_empty() {
            let (class, _) = type_info(ty);
            // generate static strings for the error messages (due to const)
            let bounds = if class == TypeClass::SInt {
                let min = -((u128::MAX >> (128 - (bits - 1))) as i128) - 1;
                let max = u128::MAX >> (128 - (bits - 1));
                format!("[{}, {}]", min, max)
            } else {
                format!("[0, {}]", u128::MAX >> (128 - bits))
            };
            let bounds_error = format!("value out of bounds {bounds}");

            tokens.extend(quote! {
                #doc
                #[doc = #location]
                #vis const fn #with_ident_checked(mut self, value: #ty) -> core::result::Result<Self, ()> {
                    match self.#set_ident_checked(value) {
                        Ok(_) => Ok(self),
                        Err(_) => Err(()),
                    }
                }
                #doc
                #[doc = #location]
                #[cfg_attr(debug_assertions, track_caller)]
                #vis const fn #with_ident(mut self, value: #ty) -> Self {
                    self.#set_ident(value);
                    self
                }

                #doc
                #[doc = #location]
                #vis const fn #set_ident(&mut self, value: #ty) {
                    if let Err(_) = self.#set_ident_checked(value) {
                        panic!(#bounds_error)
                    }
                }
                #doc
                #[doc = #location]
                #vis const fn #set_ident_checked(&mut self, value: #ty) -> core::result::Result<(), ()> {
                    let this = value;
                    let value: #base_ty = #into;
                    let mask = #base_ty::MAX >> (#base_ty::BITS - Self::#bits_ident as u32);
                    if value > mask {
                        return Err(());
                    }
                    let bits = #repr_into(self.0) & !(mask << Self::#offset_ident) | (value & mask) << Self::#offset_ident;
                    self.0 = #repr_from(bits);
                    Ok(())
                }
            });
        }
    }
}

/// Distinguish between different types for code generation.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TypeClass {
    /// Booleans with 1 bit size
    Bool,
    /// Unsigned ints with fixes sizes: u8, u64, ...
    UInt,
    /// Signed ints with fixes sizes: i8, i64, ...
    SInt,
    /// Custom types
    Other,
}

/// Field information, including the `bits` attribute
struct Field {
    bits: usize,
    ty: syn::Type,

    default: TokenStream,
    into: TokenStream,
    from: TokenStream,

    access: Access,
}

/// Parses the `bits` attribute that allows specifying a custom number of bits.
fn parse_field(
    base_ty: &syn::Type,
    attrs: &[syn::Attribute],
    ty: &syn::Type,
    ignore: bool,
) -> syn::Result<Field> {
    fn malformed(mut e: syn::Error, attr: &syn::Attribute) -> syn::Error {
        e.combine(s_err(attr.span(), "malformed #[bits] attribute"));
        e
    }

    let access = if ignore {
        Access::None
    } else {
        Access::ReadWrite
    };

    // Defaults for the different types
    let (class, ty_bits) = type_info(ty);
    let mut ret = match class {
        TypeClass::Bool => Field {
            bits: ty_bits,
            ty: ty.clone(),
            default: quote!(false),
            into: quote!(this as _),
            from: quote!(this != 0),
            access,
        },
        TypeClass::SInt => Field {
            bits: ty_bits,
            ty: ty.clone(),
            default: quote!(0),
            into: quote!(),
            from: quote!(),
            access,
        },
        TypeClass::UInt => Field {
            bits: ty_bits,
            ty: ty.clone(),
            default: quote!(0),
            into: quote!(this as _),
            from: quote!(this as _),
            access,
        },
        TypeClass::Other => Field {
            bits: ty_bits,
            ty: ty.clone(),
            default: quote!(),
            into: quote!(<#ty>::into_bits(this) as _),
            from: quote!(<#ty>::from_bits(this as _)),
            access,
        },
    };

    // Find and parse the bits attribute
    for attr in attrs {
        let syn::Attribute {
            style: syn::AttrStyle::Outer,
            meta: syn::Meta::List(syn::MetaList { path, tokens, .. }),
            ..
        } = attr
        else {
            continue;
        };
        if !path.is_ident("bits") {
            continue;
        }

        let span = tokens.span();
        let BitsAttr {
            bits,
            default,
            into,
            from,
            access,
        } = syn::parse2(tokens.clone()).map_err(|e| malformed(e, attr))?;

        // bit size
        if let Some(bits) = bits {
            if bits == 0 {
                return Err(s_err(span, "bits cannot bit 0"));
            }
            if ty_bits != 0 && bits > ty_bits {
                return Err(s_err(span, "overflowing field type"));
            }
            ret.bits = bits;
        }

        // read/write access
        if let Some(access) = access {
            if ignore {
                return Err(s_err(
                    tokens.span(),
                    "'access' is not supported for padding",
                ));
            }
            ret.access = access;
        }

        // conversion
        if let Some(into) = into {
            if ret.access == Access::None {
                return Err(s_err(into.span(), "'into' is not supported on padding"));
            }
            ret.into = quote!(#into(this) as _);
        }
        if let Some(from) = from {
            if ret.access == Access::None {
                return Err(s_err(from.span(), "'from' is not supported on padding"));
            }
            ret.from = quote!(#from(this as _));
        }
        if let Some(default) = default {
            ret.default = default.into_token_stream();
        }
    }

    if ret.bits == 0 {
        return Err(s_err(
            ty.span(),
            "Custom types and isize/usize require an explicit bit size",
        ));
    }

    // Signed integers need some special handling...
    if !ignore && ret.access != Access::None && class == TypeClass::SInt {
        let bits = ret.bits as u32;
        if ret.into.is_empty() {
            // Bounds check and remove leading ones from negative values
            ret.into = quote! {{
                let m = #ty::MIN >> (#ty::BITS - #bits);
                if !(m <= this && this <= -(m + 1)) {
                    return Err(())
                }
                let mask = #base_ty::MAX >> (#base_ty::BITS - #bits);
                (this as #base_ty & mask)
            }};
        }
        if ret.from.is_empty() {
            // Sign extend negative values
            ret.from = quote! {{
                let shift = #ty::BITS - #bits;
                ((this as #ty) << shift) >> shift
            }};
        }
    }

    Ok(ret)
}

/// Returns the number of bits for a given type
fn type_info(ty: &syn::Type) -> (TypeClass, usize) {
    let syn::Type::Path(syn::TypePath { path, .. }) = ty else {
        return (TypeClass::Other, 0);
    };
    let Some(ident) = path.get_ident() else {
        return (TypeClass::Other, 0);
    };
    if ident == "bool" {
        return (TypeClass::Bool, 1);
    }
    if ident == "isize" || ident == "usize" {
        return (TypeClass::UInt, 0); // they have architecture dependend sizes
    }
    macro_rules! integer {
        ($ident:ident => $($uint:ident),* ; $($sint:ident),*) => {
            match ident {
                $(_ if ident == stringify!($uint) => (TypeClass::UInt, $uint::BITS as _),)*
                $(_ if ident == stringify!($sint) => (TypeClass::SInt, $sint::BITS as _),)*
                _ => (TypeClass::Other, 0)
            }
        };
    }
    integer!(ident => u8, u16, u32, u64, u128 ; i8, i16, i32, i64, i128)
}

fn s_err(span: proc_macro2::Span, msg: impl fmt::Display) -> syn::Error {
    syn::Error::new(span, msg)
}

#[cfg(test)]
mod test {
    #![allow(clippy::unwrap_used)]
    use quote::quote;

    use crate::{Access, BitsAttr, Enable, Order, Params};

    #[test]
    fn parse_args() {
        let args = quote!(u64);
        let params = syn::parse2::<Params>(args).unwrap();
        assert_eq!(params.bits, u64::BITS as usize);
        assert!(matches!(params.debug, Enable::Yes));
        assert!(matches!(params.defmt, Enable::No));

        let args = quote!(u32, debug = false);
        let params = syn::parse2::<Params>(args).unwrap();
        assert_eq!(params.bits, u32::BITS as usize);
        assert!(matches!(params.debug, Enable::No));
        assert!(matches!(params.defmt, Enable::No));

        let args = quote!(u32, defmt = true);
        let params = syn::parse2::<Params>(args).unwrap();
        assert_eq!(params.bits, u32::BITS as usize);
        assert!(matches!(params.debug, Enable::Yes));
        assert!(matches!(params.defmt, Enable::Yes));

        let args = quote!(u32, defmt = cfg(test), debug = cfg(feature = "foo"));
        let params = syn::parse2::<Params>(args).unwrap();
        assert_eq!(params.bits, u32::BITS as usize);
        assert!(matches!(params.debug, Enable::Cfg(_)));
        assert!(matches!(params.defmt, Enable::Cfg(_)));

        let args = quote!(u32, order = Msb);
        let params = syn::parse2::<Params>(args).unwrap();
        assert!(params.bits == u32::BITS as usize && params.order == Order::Msb);
    }

    #[test]
    fn parse_bits() {
        let args = quote!(8);
        let attr = syn::parse2::<BitsAttr>(args).unwrap();
        assert_eq!(attr.bits, Some(8));
        assert!(attr.default.is_none());
        assert!(attr.into.is_none());
        assert!(attr.from.is_none());
        assert!(attr.access.is_none());

        let args = quote!(8, default = 8, access = RW);
        let attr = syn::parse2::<BitsAttr>(args).unwrap();
        assert_eq!(attr.bits, Some(8));
        assert!(attr.default.is_some());
        assert!(attr.into.is_none());
        assert!(attr.from.is_none());
        assert_eq!(attr.access, Some(Access::ReadWrite));

        let args = quote!(access = RO);
        let attr = syn::parse2::<BitsAttr>(args).unwrap();
        assert_eq!(attr.bits, None);
        assert!(attr.default.is_none());
        assert!(attr.into.is_none());
        assert!(attr.from.is_none());
        assert_eq!(attr.access, Some(Access::ReadOnly));

        let args = quote!(default = 8, access = WO);
        let attr = syn::parse2::<BitsAttr>(args).unwrap();
        assert_eq!(attr.bits, None);
        assert!(attr.default.is_some());
        assert!(attr.into.is_none());
        assert!(attr.from.is_none());
        assert_eq!(attr.access, Some(Access::WriteOnly));

        let args = quote!(
            3,
            into = into_something,
            default = 1,
            from = from_something,
            access = None
        );
        let attr = syn::parse2::<BitsAttr>(args).unwrap();
        assert_eq!(attr.bits, Some(3));
        assert!(attr.default.is_some());
        assert!(attr.into.is_some());
        assert!(attr.from.is_some());
        assert_eq!(attr.access, Some(Access::None));
    }

    #[test]
    fn parse_access_mode() {
        let args = quote!(RW);
        let mode = syn::parse2::<Access>(args).unwrap();
        assert_eq!(mode, Access::ReadWrite);

        let args = quote!(RO);
        let mode = syn::parse2::<Access>(args).unwrap();
        assert_eq!(mode, Access::ReadOnly);

        let args = quote!(WO);
        let mode = syn::parse2::<Access>(args).unwrap();
        assert_eq!(mode, Access::WriteOnly);

        let args = quote!(None);
        let mode = syn::parse2::<Access>(args).unwrap();
        assert_eq!(mode, Access::None);

        let args = quote!(garbage);
        let mode = syn::parse2::<Access>(args);
        assert!(mode.is_err());
    }
}
