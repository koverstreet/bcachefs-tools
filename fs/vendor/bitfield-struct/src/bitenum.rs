use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::{spanned::Spanned, Attribute};

use crate::{s_err, EnumParams};

struct Variant {
    ident: syn::Ident,
    cfg: Vec<Attribute>,
}

impl ToTokens for Variant {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let ident = &self.ident;
        let cfg = &self.cfg;
        tokens.extend(quote! {
            #( #cfg )*
            Self::#ident
        });
    }
}
struct VariantMatch<'a> {
    variant: &'a Variant,
    repr: &'a syn::Type,
}
impl<'a> ToTokens for VariantMatch<'a> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let Variant { ident, cfg } = &self.variant;
        let repr = &self.repr;
        tokens.extend(quote! {
            #( #cfg )*
            x if x == Self::#ident as #repr => Self::#ident,
        });
    }
}

pub fn bitenum_inner(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let params = syn::parse2::<EnumParams>(args)?;
    let span = input.span();
    let mut input = syn::parse2::<syn::ItemEnum>(input)?;

    let Some(repr) = input.attrs.iter().find_map(|attr| {
        if attr.style == syn::AttrStyle::Outer && attr.path().is_ident("repr") {
            if let syn::Meta::List(syn::MetaList { tokens, .. }) = &attr.meta {
                let ty = syn::parse2::<syn::Type>(tokens.clone()).ok()?;
                return Some(ty);
            }
        }
        None
    }) else {
        return Err(s_err(span, "missing #[repr(...)] attribute"));
    };

    let mut fallback = None;
    let mut variants = Vec::new();
    for variant in &mut input.variants {
        let len = variant.attrs.len();
        variant.attrs.retain(|a| !a.path().is_ident("fallback"));
        if len != variant.attrs.len() {
            if fallback.is_some() {
                return Err(s_err(
                    variant.span(),
                    "only one #[fallback] attribute is allowed",
                ));
            }
            fallback = Some(variant.ident.clone());
        }
        let cfg = variant
            .attrs
            .iter()
            .filter(|a| a.style == syn::AttrStyle::Outer && a.path().is_ident("cfg"))
            .cloned()
            .collect::<Vec<_>>();
        variants.push(Variant {
            ident: variant.ident.clone(),
            cfg,
        });
    }
    if fallback.is_none() {
        return Err(s_err(span, "missing #[fallback] attribute on one variant"));
    }
    let ident = &input.ident;
    let vis = &input.vis;
    let variant_len = variants.iter().map(|v| {
        let cfg = &v.cfg;
        quote! { #( #cfg )* { x += 1; } }
    });

    let match_cases = variants.iter().map(|v| VariantMatch {
        variant: &v,
        repr: &repr,
    });

    let mut impls = TokenStream::new();
    if let Some(cfg) = params.from.cfg() {
        impls.extend(quote! {
            /// Convert from bits.
            #cfg
            #vis const fn from_bits(bits: #repr) -> Self {
                match bits {
                    #( #match_cases )*
                    _ => Self::#fallback,
                }
            }
        });
    }
    if let Some(cfg) = params.into.cfg() {
        impls.extend(quote! {
            /// Convert into bits.
            #cfg
            #vis const fn into_bits(self) -> #repr {
                self as #repr
            }
        });
    }
    if let Some(cfg) = params.all.cfg() {
        impls.extend(quote! {
            /// Returns an array of all enum variants.
            #cfg
            #vis const fn all() -> [Self; const { let mut x = 0; #( #variant_len )* x } ] {
                [ #( #variants, )* ]
            }
        });
    }
    Ok(quote! {
        #input
        impl #ident { #impls }
    })
}
