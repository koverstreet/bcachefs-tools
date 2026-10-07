use super::Member;
use proc_macro2::TokenStream;
use quote::quote;

/// Implements the `core::clone::Clone` trait for the given bitfield struct.
pub fn debug(
    name: &syn::Ident,
    generics: &syn::Generics,
    members: &[Member],
    cfg: Option<TokenStream>,
) -> TokenStream {
    let fields = members.iter().filter_map(|m| {
        let inner = m.inner.as_ref()?;
        if inner.from.is_empty() {
            return None;
        }

        let ident = &inner.ident;
        Some(quote!(.field(stringify!(#ident), &self.#ident())))
    });

    let attr = cfg.map(|cfg| quote!(#[cfg(#cfg)]));

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        #attr
        impl #impl_generics core::fmt::Debug for #name #ty_generics #where_clause {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!(#name))
                    #( #fields )*
                    .finish()
            }
        }
    }
}

/// Implements the `core::fmt::Display` trait for the given bitfield struct.
pub fn defmt(
    name: &syn::Ident,
    generics: &syn::Generics,
    members: &[Member],
    cfg: Option<TokenStream>,
) -> TokenStream {
    // build a part of the format string for each field
    let formats = members.iter().filter_map(|m| {
        let inner = m.inner.as_ref()?;
        if inner.from.is_empty() {
            return None;
        }

        // primitives supported by defmt
        const PRIMITIVES: &[&str] = &[
            "bool", "usize", "isize", //
            "u8", "u16", "u32", "u64", "u128", //
            "i8", "i16", "i32", "i64", "i128", //
            "f32", "f64", //
        ];

        // get the type name so we can use more efficient defmt formats
        // if it's a primitive
        if let syn::Type::Path(syn::TypePath { path, .. }) = &inner.ty {
            if let Some(ident) = path.get_ident() {
                if PRIMITIVES.iter().any(|s| ident == s) {
                    // defmt supports this primitive, use special spec
                    return Some(format!("{}: {{={ident}}}", inner.ident));
                }
            }
        }

        Some(format!("{}: {{:?}}", inner.ident))
    });

    // find the corresponding format argument for each field
    let args = members.iter().filter_map(|m| {
        let inner = m.inner.as_ref()?;
        if inner.from.is_empty() {
            return None;
        }

        let ident = &inner.ident;
        Some(quote!(self.#ident()))
    });

    // build a string like "Foo { field_name: {:?}, ... }"
    // four braces, two to escape *this* format, times two to escape
    // the defmt::write! call below.
    let format_string = format!(
        "{name} {{{{ {} }}}} ",
        formats.collect::<Vec<_>>().join(", ")
    );

    let attr = cfg.map(|cfg| quote!(#[cfg(#cfg)]));

    // note: we use defmt paths here, not ::defmt, because many crates
    // in the embedded space will rename defmt (e.g. to defmt_03) in
    // order to support multiple incompatible defmt versions.
    //
    // defmt itself avoids ::defmt for this reason. For more info, see:
    // https://github.com/knurling-rs/defmt/pull/835

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        #attr
        impl #impl_generics defmt::Format for #name #ty_generics #where_clause {
            fn format(&self, f: defmt::Formatter) {
                defmt::write!(f, #format_string, #( #args, )*)
            }
        }
    }
}

/// Implements the `core::hash::Hash` trait for the given bitfield struct.
pub fn hash(
    name: &syn::Ident,
    generics: &syn::Generics,
    members: &[Member],
    cfg: Option<TokenStream>,
) -> TokenStream {
    let fields = members.iter().filter_map(|m| {
        let inner = m.inner.as_ref()?;
        if inner.from.is_empty() {
            return None;
        }

        let ident = &inner.ident;
        Some(quote!(self.#ident()))
    });

    let attr = cfg.map(|cfg| quote!(#[cfg(#cfg)]));

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        #attr
        impl #impl_generics core::hash::Hash for #name #ty_generics #where_clause {
            fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
                #( #fields.hash(state); )*
            }
        }
    }
}

/// Implements the `binrw::BinWrite` trait for the given bitfield struct.
pub fn binwrite(
    name: &syn::Ident,
    generics: &syn::Generics,
    cfg: Option<TokenStream>,
) -> TokenStream {
    let attr = cfg.map(|cfg| quote!(#[cfg(#cfg)]));

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        #attr
        impl #impl_generics binrw::BinWrite for #name #ty_generics #where_clause {
            type Args<'a> = ();

            fn write_options<W: binrw::io::Write + binrw::io::Seek>(
                &self,
                writer: &mut W,
                endian: binrw::Endian,
                args: Self::Args<'_>,
            ) -> binrw::BinResult<()> {
                let raw = self.into_bits();

                let bytes = match endian {
                    binrw::Endian::Big => raw.to_be_bytes(),
                    binrw::Endian::Little => raw.to_le_bytes(),
                };

                writer.write_all(&bytes)?;

                Ok(())
            }
        }
    }
}

/// Implements the `binrw::BinRead` trait for the given bitfield struct.
pub fn binread(
    name: &syn::Ident,
    generics: &syn::Generics,
    repr: &syn::Type,
    cfg: Option<TokenStream>,
) -> TokenStream {
    let attr = cfg.map(|cfg| quote!(#[cfg(#cfg)]));

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        #attr
        impl #impl_generics binrw::BinRead for #name #ty_generics #where_clause {
            type Args<'a> = ();

            fn read_options<R: binrw::io::Read + binrw::io::Seek>(
                reader: &mut R,
                endian: binrw::Endian,
                args: Self::Args<'_>,
            ) -> binrw::BinResult<Self> {
                let mut buf = [0u8; core::mem::size_of::<#repr>()];

                reader.read_exact(&mut buf)?;

                let raw = match endian {
                    binrw::Endian::Big => <#repr>::from_be_bytes(buf),
                    binrw::Endian::Little => <#repr>::from_le_bytes(buf),
                };

                Ok(Self::from_bits(raw))
            }
        }
    }
}
