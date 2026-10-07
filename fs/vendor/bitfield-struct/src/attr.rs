use proc_macro2::{Ident, TokenStream};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::Token;

use crate::{type_info, TypeClass};

use super::s_err;

/// The bitfield macro parameters
pub struct Params {
    pub ty: syn::Type,
    pub repr: syn::Type,
    pub into: Option<syn::Path>,
    pub from: Option<syn::Path>,
    pub bits: usize,
    pub binread: Enable,
    pub binwrite: Enable,
    pub new: Enable,
    pub clone: Enable,
    pub debug: Enable,
    pub defmt: Enable,
    pub default: Enable,
    pub hash: Enable,
    pub order: Order,
    pub conversion: bool,
}

impl Parse for Params {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let Ok(ty) = syn::Type::parse(input) else {
            return Err(s_err(input.span(), "unknown type"));
        };
        let (class, bits) = type_info(&ty);
        if class != TypeClass::UInt {
            return Err(s_err(input.span(), "unsupported type"));
        }

        let mut ret = Params {
            repr: ty.clone(),
            ty,
            into: None,
            from: None,
            bits,
            binread: Enable::No,
            binwrite: Enable::No,
            new: Enable::Yes,
            clone: Enable::Yes,
            debug: Enable::Yes,
            defmt: Enable::No,
            default: Enable::Yes,
            hash: Enable::No,
            order: Order::Lsb,
            conversion: true,
        };

        // try parse additional args
        while <Token![,]>::parse(input).is_ok() {
            let ident = Ident::parse(input)?;
            <Token![=]>::parse(input)?;
            match ident.to_string().as_str() {
                "repr" => {
                    ret.repr = input.parse()?;
                }
                "from" => {
                    ret.from = Some(input.parse()?);
                }
                "into" => {
                    ret.into = Some(input.parse()?);
                }
                "debug" => {
                    ret.debug = input.parse()?;
                }
                "defmt" => {
                    ret.defmt = input.parse()?;
                }
                "new" => {
                    ret.new = input.parse()?;
                }
                "clone" => {
                    ret.clone = input.parse()?;
                }
                "default" => {
                    ret.default = input.parse()?;
                }
                "hash" => {
                    ret.hash = input.parse()?;
                }
                "order" => {
                    ret.order = match syn::Ident::parse(input)?.to_string().as_str() {
                        "Msb" | "msb" => Order::Msb,
                        "Lsb" | "lsb" => Order::Lsb,
                        _ => return Err(s_err(ident.span(), "unknown value for order")),
                    };
                }
                "conversion" => {
                    ret.conversion = syn::LitBool::parse(input)?.value;
                }
                "binrw" => {
                    let enable: Enable = input.parse()?;
                    ret.binread = enable.clone();
                    ret.binwrite = enable;
                }
                "binread" => {
                    ret.binread = input.parse()?;
                }
                "binwrite" => {
                    ret.binwrite = input.parse()?;
                }
                _ => return Err(s_err(ident.span(), "unknown argument")),
            };
        }

        if ret.repr != ret.ty && (ret.from.is_none() || ret.into.is_none()) {
            return Err(s_err(
                input.span(),
                "`repr` requires both `from` and `into`",
            ));
        }

        Ok(ret)
    }
}

/// The bits attribute of the fields of a bitfield struct
pub struct BitsAttr {
    pub bits: Option<usize>,
    pub default: Option<syn::Expr>,
    pub into: Option<syn::Path>,
    pub from: Option<syn::Path>,
    pub access: Option<Access>,
}

impl Parse for BitsAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut attr = Self {
            bits: None,
            default: None,
            into: None,
            from: None,
            access: None,
        };
        if let Ok(bits) = syn::LitInt::parse(input) {
            attr.bits = Some(bits.base10_parse()?);
            if !input.is_empty() {
                <Token![,]>::parse(input)?;
            }
        }
        // parse remainder
        if !input.is_empty() {
            loop {
                let ident = syn::Ident::parse(input)?;

                <Token![=]>::parse(input)?;

                if ident == "default" {
                    attr.default = Some(input.parse()?);
                } else if ident == "into" {
                    attr.into = Some(input.parse()?);
                } else if ident == "from" {
                    attr.from = Some(input.parse()?);
                } else if ident == "access" {
                    attr.access = Some(input.parse()?);
                }

                if input.is_empty() {
                    break;
                }

                <Token![,]>::parse(input)?;
            }
        }
        Ok(attr)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
    WriteOnly,
    None,
}

impl Parse for Access {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mode = input.parse::<Ident>()?;

        if mode == "RW" {
            Ok(Self::ReadWrite)
        } else if mode == "RO" {
            Ok(Self::ReadOnly)
        } else if mode == "WO" {
            Ok(Self::WriteOnly)
        } else if mode == "None" {
            Ok(Self::None)
        } else {
            Err(s_err(
                mode.span(),
                "Invalid access mode, only RW, RO, WO, or None are allowed",
            ))
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Order {
    Lsb,
    Msb,
}

#[derive(Debug, Clone)]
pub enum Enable {
    No,
    Yes,
    Cfg(TokenStream),
}
impl Enable {
    pub fn cfg(self) -> Option<Option<TokenStream>> {
        match self {
            Enable::No => None,
            Enable::Yes => Some(None),
            Enable::Cfg(c) => Some(Some(c)),
        }
    }
}
impl Parse for Enable {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(if let Ok(lit_bool) = syn::LitBool::parse(input) {
            if lit_bool.value {
                Enable::Yes
            } else {
                Enable::No
            }
        } else {
            let meta = syn::MetaList::parse(input)?;
            if matches!(meta.delimiter, syn::MacroDelimiter::Paren(_)) && meta.path.is_ident("cfg")
            {
                Enable::Cfg(meta.tokens)
            } else {
                return Err(s_err(meta.span(), "Only `cfg` attributes are allowed"));
            }
        })
    }
}

pub struct EnumParams {
    pub into: Enable,
    pub from: Enable,
    pub all: Enable,
}

impl Parse for EnumParams {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut ret = EnumParams {
            into: Enable::Yes,
            from: Enable::Yes,
            all: Enable::No,
        };

        while !input.is_empty() {
            let ident = syn::Ident::parse(input)?;
            <Token![=]>::parse(input)?;

            if ident == "from" {
                ret.from = input.parse()?;
            } else if ident == "into" {
                ret.into = input.parse()?;
            } else if ident == "all" {
                ret.all = input.parse()?;
            } else {
                return Err(s_err(ident.span(), "unknown argument"));
            }

            if !input.is_empty() {
                <Token![,]>::parse(input)?;
            }
        }

        Ok(ret)
    }
}
