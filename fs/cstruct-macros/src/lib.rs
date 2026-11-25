// SPDX-License-Identifier: GPL-2.0
//! The macros our types are written with - see fs/types/lib.rs.
//! Each parses its input with cstruct.rs, so a mistake is a compile error at
//! the item, and expands to its Rust side:
//!
//!   #[derive(MaybeZeroable)]
//!                        a struct or union: Zeroable if its fields are
//!   #[derive(BitfieldMembers)]
//!                        a #[members] field's bitfield members, on the
//!                        struct
//!   #[bitfield(uN)]      bitfield_struct::bitfield, and forwarders for the
//!                        struct that holds one in a #[members] field; with
//!                        `atomic`, an atomic word of it (bitfield_atomic.rs)
//!   xmacro! { .. }       an x-macro list: NAME!(cb), handing cb! the whole
//!                        list
//!   xmacro_select!       NAME!(xmacro_select [TOKEN => cb]): cb! handed
//!                        the rows with TOKEN in them - a flag in a flags
//!                        column
//!   ioctl! { .. }        ioctl numbers: a marker type for each, its opcode
//!                        and argument type
//!   #[bch_enum(..)]      an enum, open, closed or flags: the type that is,
//!                        and its values
//!   tagged_union! { .. } a tagged union with a stable representation: its
//!                        storage, the tag a Determinant of it, declared as
//!                        an enum, the arms its variants (tagged_union.rs)
//!   #[derive(TypeInfo)]  runtime type information, for kvdb (typeinfo.rs)
//!   #[derive(Plain)]     util/vstructs.rs's Plain, checked at compile time;
//!                        a #[bitfield] is Plain too (vstructs.rs)
//!   #[derive(View)]      a projection for each field on a View of the type:
//!                        its fields read as zero past a short value's end
//!                        (vstructs.rs)
//!   #[unsized_vstruct]   on a struct ending in a flexible array, its header:
//!                        X<S>, the header and its tail, in state S
//!                        (vstructs.rs)
//!   #[grown(from = F)]   on the struct of a type that grew: its family - the
//!                        whole struct, and the floor and the bytes there
//!                        are (vstructs.rs)
//!   __prt!(..)           message!()'s and prt!()'s format strings, written
//!                        into a printbuf - a ToText argument to_text()'d
//!                        into it (prt.rs)
//!   #[args_stored_once]  an async fn as fn -> impl Future { async move }, so
//!                        its by-value arguments are stored once in its
//!                        future, not twice (args_stored_once.rs)
//!   #[in_place(Fut)]     an async fn whose future is named Fut, for a task
//!                        held in place, or a box's pool, by type (in_place.rs)
//!
//! On syn and quote - os/linux_kernel/vendor's: the kernel build compiles this
//! with a bare rustc.

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

mod cstruct;
use cstruct::{parse_cstruct, parse_ioctl, EnumKind};

mod args_stored_once;
mod bch_enum;
mod bitfield;
mod bitfield_atomic;
mod in_place;
mod detach;
mod rcu_head;
mod fieldwise;
mod options;
mod init;
mod late;
mod place;
mod prt;
mod tagged_union;
mod to_text;
mod typeinfo;
mod tokens;
mod vstructs;
mod xmacro;
mod zeroable;

/// A tagged union with a stable representation: see tagged_union.rs.
#[proc_macro]
pub fn tagged_union(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("tagged_union!", || tagged_union::tagged_union(input.into()))
}

/// message!()'s format string written into a printbuf, ToText arguments
/// to_text()'d into it: see prt.rs.
#[doc(hidden)]
#[proc_macro]
pub fn __prt(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("message!", || prt::prt(syn::parse(input)?))
}

fn compile_error(msg: &str) -> TokenStream {
    quote!(::core::compile_error!(#msg);)
}

/// A macro's expansion, @f, or its error as compile_error!, @what naming the
/// macro: every macro here is expand("name!", || ..), or expand_syn().
fn expand(what: &str, f: impl FnOnce() -> Result<TokenStream, String>) -> proc_macro::TokenStream {
    f().unwrap_or_else(|e| compile_error(&format!("{what}: {e}"))).into()
}

/// expand() of a macro on syn: its error at the tokens it's about.
fn expand_syn(what: &str, f: impl FnOnce() -> syn::Result<TokenStream>) -> proc_macro::TokenStream {
    f().unwrap_or_else(|e| syn::Error::new(e.span(), format!("{what}: {e}")).into_compile_error()).into()
}

/// bch_opts' options, a field each: options.rs.
#[proc_macro_derive(Options, attributes(opt))]
pub fn derive_options(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Options)]", || options::derive(syn::parse(input)?))
}

/// `#[derive(MaybeZeroable)]`: a struct or union's Zeroable impl, if every
/// field is - see zeroable.rs.
/// `#[type_name_of(NAME)]` is what #[type_name] leaves for the derives.
#[proc_macro_derive(MaybeZeroable, attributes(type_name_of))]
pub fn derive_maybe_zeroable(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(MaybeZeroable)]", || Ok(zeroable::maybe_zeroable(&parse_cstruct(&input.into())?)))
}

/// `#[derive(BitfieldMembers)]`: the members of a `#[members]` field - one of
/// a #[bitfield] type - on the struct, forwarded to it.
#[proc_macro_derive(BitfieldMembers, attributes(members))]
pub fn derive_bitfield_members(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(BitfieldMembers)]", || bitfield::bitfield_members(input.into()))
}

/// `#[type_name(NAME)]`, before the derives, on a struct whose name isn't
/// the type's: a family's definition, X_head, whose type is X<S>
/// (vstructs.rs), or a tagged union's storage. The derives get it as
/// #[type_name_of(NAME)] - after the item's other attributes, as a derive's
/// helper comes after the derive - and TypeInfo names it so.
#[proc_macro_attribute]
pub fn type_name(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[type_name]", || {
        let name: syn::Ident = syn::parse(attr)
            .map_err(|e| syn::Error::new(e.span(), "takes the type's name"))?;
        let mut item: syn::DeriveInput = syn::parse(item)?;
        item.attrs.push(syn::parse_quote!(#[type_name_of(#name)]));
        Ok(quote!(#item))
    })
}

/// `#[derive(TypeInfo)]`: runtime type information for a struct or union, for
/// kvdb - see typeinfo.rs.
#[proc_macro_derive(TypeInfo, attributes(type_name_of))]
pub fn derive_type_info(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(TypeInfo)]", || typeinfo::derive(input.into()))
}

/// `#[derive(Init)]`: `T::init()`, an initializer that zeroes every field but
/// those with `#[init(EXPR)]` - see init.rs.
#[proc_macro_derive(Init, attributes(init, init_args, init_locked, placed_args, placed_at, field))]
pub fn derive_init(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Init)]", || init::derive(syn::parse(input)?))
}

/// `#[derive(Fields)]`: util/place.rs's Field<N> for each field, as
/// #[derive(Init)] makes them - for a struct made some other way. A private
/// field marked `#[field]` is read by path too, as a visible one is.
#[proc_macro_derive(Fields, attributes(field))]
pub fn derive_fields(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Fields)]", || place::derive_fields(&syn::parse(input)?))
}

/// `#[derive(Late)]`: `init_late()` and `stop_late()`, for the fields with
/// `#[late]` - see late.rs.
#[proc_macro_derive(Late, attributes(late))]
pub fn derive_late(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Late)]", || late::derive(syn::parse(input)?))
}

/// `#[derive(Detach)]`: util/rcu.rs's Detach, of every field - see detach.rs.
#[proc_macro_derive(Detach)]
pub fn derive_detach(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Detach)]", || detach::derive(syn::parse(input)?))
}

/// `#[derive(RcuHead)]`: util/rcu.rs's HasRcuHead, at the field marked
/// #[rcu_head] - see rcu_head.rs.
#[proc_macro_derive(RcuHead, attributes(rcu_head))]
pub fn derive_rcu_head(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(RcuHead)]", || rcu_head::derive(syn::parse(input)?))
}

/// `#[derive(Fieldwise)]`: each method `#[fieldwise(...)]` names, applied to
/// every field - see fieldwise.rs.
#[proc_macro_derive(Fieldwise, attributes(fieldwise))]
pub fn derive_fieldwise(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(Fieldwise)]", || fieldwise::derive(syn::parse(input)?))
}

/// `#[derive(ToText)]`: a fieldless enum printed as its variant's name, or
/// its `#[label("..")]` - see to_text.rs.
#[proc_macro_derive(ToText, attributes(label))]
pub fn derive_to_text(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[derive(ToText)]", || to_text::derive(syn::parse(input)?))
}

/// `#[derive(Plain)]`: util/vstructs.rs's Plain, checked at compile time -
/// see vstructs.rs.
#[proc_macro_derive(Plain, attributes(plain))]
pub fn derive_plain(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(Plain)]", || vstructs::derive_plain(input.into()))
}

/// `#[derive(NoUninit)]`: util/vstructs.rs's NoUninit, checked at compile
/// time - see vstructs.rs.
#[proc_macro_derive(NoUninit)]
pub fn derive_no_uninit(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(NoUninit)]", || vstructs::derive_no_uninit(input.into()))
}

/// `#[derive(VolatileValue)]`: util/projections.rs's VolatileValue, a
/// Volatile of it loaded field by field - see vstructs.rs.
#[proc_macro_derive(VolatileValue)]
pub fn derive_volatile_value(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(VolatileValue)]", || vstructs::derive_volatile_value(input.into()))
}

/// `#[derive(NonZeroWord)]`: util/vstructs.rs's NonZeroWord, checked at
/// compile time - see vstructs.rs.
#[proc_macro_derive(NonZeroWord)]
pub fn derive_non_zero_word(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(NonZeroWord)]", || vstructs::derive_non_zero_word(input.into()))
}

/// `#[derive(SelfSized)]`: util/vstructs.rs's SelfSized, for a struct ending in
/// a DST that is - see vstructs.rs.
#[proc_macro_derive(SelfSized)]
pub fn derive_self_sized(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(SelfSized)]", || vstructs::derive_self_sized(input.into()))
}

/// `#[derive(SizedVal)]`: a key value that's itself in place, any bytes of it
/// a valid one - btree/bkey/types.rs's SizedVal, what a sized key type's
/// value is.
#[proc_macro_derive(SizedVal)]
pub fn derive_sized_val(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(SizedVal)]", || vstructs::derive_sized_val(input.into()))
}

/// `#[derive(View)]`: a projection for each field, on util/vstructs.rs's
/// View and ViewMut of the type - see vstructs.rs.
#[proc_macro_derive(View)]
pub fn derive_view(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[derive(View)]", || vstructs::derive_view(input.into()))
}

/// bitfield_struct::bitfield - and __NAME_forward!([GENERICS] STRUCT, field),
/// which #[derive(BitfieldMembers)] invokes for a #[members] field of this type:
/// each member's getter and setter on the struct that holds it, as C had a
/// bitfield's members on its struct. Not `_` members: those are padding.
///
/// Members are written as bilge has them, the type giving the width:
///
///   #[bitfield(u64)]
///   pub struct foo_bits { type_: u2, offset: u40, cached: bool, __pad: u3, .. }
///
/// `name: uN` is N bits, read and written as N rounded up to an integer -
/// offset above is a u64; `name: bool` is one bit. A member of any other type -
/// an enum - gives its width, #[bits(N)], as bitfield-struct has it; a uN or
/// bool member's #[bits(..)] gives only its options, as #[bits(default = 1)]
/// does. Members are pub unless written otherwise, and their
/// forwarders the same - pub(self), for one only its own module may set, has
/// none.
///
/// Storage LeN is a uN, little-endian - an on-disk word: #[bitfield(Le64)].
/// `align = 8` aligns it to 8, which a u64 bitfield needs said on i386
/// (Align8U64). `atomic`, on a uN: AtomicNAME too, a word of it that's
/// atomic - see bitfield_atomic.rs.
#[proc_macro_attribute]
pub fn bitfield(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[bitfield]", || bitfield::bitfield_expand(attr.into(), item.into()))
}

/// `#[vtable]`, on an impl of a trait whose methods are optional - one with
/// `const HAS_FOO: bool = false` for each method foo: `const HAS_FOO: bool =
/// true` for each the impl defines, unless it says otherwise itself. So a
/// table of C function pointers made from the trait leaves out what isn't
/// implemented. The kernel crate's #[vtable] does this too, but from 7.3 its
/// impls name crate::LocalModule, which only a module!() crate has.
#[proc_macro_attribute]
pub fn vtable(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[vtable]", || {
        if !attr.is_empty() {
            return Err(syn::Error::new_spanned(proc_macro2::TokenStream::from(attr), "takes no arguments"));
        }
        let mut imp: syn::ItemImpl = syn::parse(item)?;
        let consts: Vec<String> = imp.items.iter().filter_map(|i| match i {
            syn::ImplItem::Const(c) => Some(c.ident.to_string()),
            _ => None,
        }).collect();
        let has: Vec<syn::Ident> = imp.items.iter().filter_map(|i| match i {
            syn::ImplItem::Fn(f) => Some(quote::format_ident!("HAS_{}", f.sig.ident.to_string().to_uppercase())),
            _ => None,
        }).filter(|c| !consts.contains(&c.to_string())).collect();
        for c in has {
            imp.items.push(syn::parse_quote!(const #c: bool = true;));
        }
        Ok(quote::quote!(#imp))
    })
}

/// `#[args_stored_once]` on an async fn: the same fn, as `fn ->
/// impl Future { async move { body } }`, so an argument live across an
/// await is stored once in its future - an async fn stores it twice
/// (rust-lang/rust#62958). See args_stored_once.rs.
#[proc_macro_attribute]
pub fn args_stored_once(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[args_stored_once]", || args_stored_once::args_stored_once(attr.into(), syn::parse(item)?))
}

/// `#[in_place(Fut)]` on an async fn: the same fn, its future named Fut - a
/// type alias impl Trait the fn defines - for a pool to be sized for its
/// task's box; its args stored
/// once, as #[args_stored_once] stores them. See in_place.rs.
#[proc_macro_attribute]
pub fn in_place(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[in_place]", || in_place::in_place(syn::parse(attr)?, syn::parse(item)?))
}

/// `#[placed(Base.field)]`, ahead of `#[pin_data]` and the derives, on a
/// struct that lives only at Base's field: its At, the field util/place.rs's
/// Contained is had by, which `#[derive(Init)]` then gives its init() - see
/// place.rs.
#[proc_macro_attribute]
pub fn placed(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[placed]", || place::placed(syn::parse(attr)?, syn::parse(item)?))
}

/// `#[counters(a, b, ..)]` on an accounting key type X: its counters, struct
/// XCounters - accounting.rs's acct_counters! - and its KeyType impl.
#[proc_macro_attribute]
pub fn counters(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand_syn("#[counters]", || {
        let s: syn::ItemStruct = syn::parse(item)?;
        let (name, attr) = (&s.ident, proc_macro2::TokenStream::from(attr));
        let counters = quote::format_ident!("{name}Counters");
        Ok(quote::quote! {
            #s
            crate::accounting::acct_counters!(#counters { #attr });
            impl crate::accounting::KeyType for #name {
                type Delta = #counters<i64>;
                type Read  = #counters<u64>;
            }
        })
    })
}

/// `#[unsized_vstruct]`, on the definition of a struct ending in a flexible
/// array - or `#[unsized_vstruct(tail = [E])]`, `tail = NAME: [E]`, saying
/// what follows its header: the one written, renamed X_head, and X<S>, a DST
/// of the header and its tail, generic over its state. See vstructs.rs.
#[proc_macro_attribute]
pub fn unsized_vstruct(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[unsized_vstruct]", || vstructs::unsized_vstruct(attr.into(), item.into()))
}

/// `#[grown(from = FIELD)]`, on the definition of a type that grew between
/// format versions, FIELD the first field an older value may lack: X<S>, a
/// generic struct - X<Whole>, X, the whole struct; X<Validated>, the floor
/// and the bytes there are, at the value's length. See vstructs.rs.
#[proc_macro_attribute]
pub fn grown(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[grown]", || vstructs::grown(attr.into(), item.into()))
}

/// `#[counted]`, `#[counted(PATH)]`, `#[counted(PATH, tail = [E])]` or
/// `tail = NAME: [E]`, on the definition of a header and an array it counts
/// in elements: its family, the count private in each state's definition -
/// see vstructs.rs.
#[proc_macro_attribute]
pub fn counted(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[counted]", || vstructs::counted(attr.into(), item.into()))
}

/// `#[whole]`, on the definition of a type every valid value of which has
/// all of it - a superblock section or journal entry of fixed fields: its
/// family, every state all of it, at the value's length - see vstructs.rs.
#[proc_macro_attribute]
pub fn whole(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[whole]", || vstructs::whole(attr.into(), item.into()))
}

/// An x-macro list: xmacro! { NAME { (args), ... } }: a macro,
/// NAME!(cb), which expands to cb! { (args), ... } - and NAME!(cb [..]),
/// to cb! { [..] (args), ... }, for a cb that accumulates. cb may be a path.
///
/// An entry can be another list, SUB(), or another list's entries mapped,
/// SUB((params) => (args)). Then NAME!(cb) is a chain of macros, one per
/// list it splices in: each is SUB! handed the entries so far, which adds
/// SUB's, mapped, and those after, and calls the next; the last calls cb!. They're named by path, through
/// crate::cstructs::c - NAME is expanded wherever it's used.
#[proc_macro]
pub fn xmacro(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("xmacro!", || xmacro::xmacro_expand(input.into()))
}

/// A list's rows, selected: NAME!(xmacro_select [TOKEN => cb]) expands to
/// cb! { (args), ... } of the rows with the identifier TOKEN in a set of
/// theirs, a `[..]` - a flag in a flags column, as ptrs is in bkey_val's
/// (tagged_union!'s #[arm_columns]) - or, in a row without one, among its own
/// tokens, as BTREE_IS_extents is in BCH_BTREE_IDS()'. None selected is an
/// error: an empty mask compiles, and runs no triggers. cb may be a path;
/// [TOKEN => cb { args }] hands it args before the rows, cb! { args (args), ... }.
/// Selecting in a macro_rules! muncher instead nests an expansion per token,
/// which runs into the recursion limit as lists grow.
#[proc_macro]
pub fn xmacro_select(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("xmacro_select!", || xmacro::select(input.into()))
}

/// `#[bch_enum(closed|open|flags)]` on `#[repr(uN)] pub enum NAME { .. }`:
/// what its values can be, and the type that is.
///
///   closed  a Rust enum
///   open    a newtype over the integer, the values its constants: a value
///           from outside can be anything, so it's Plain, and a word a
///           Volatile can hold
///   flags   a bitflags type
///
/// Any of them can be a #[bitfield]'s member, `#[bits(8)] btree_id: Btree`:
/// it has the const into_bits() bitfield-struct converts with, and
/// from_bits() - for flags, bitflags' from_bits() is the checked one, so a
/// member names `from = T::from_bits_retain`. A closed enum's member only
/// ever holds one of its values, as its type says; from_bits() of anything
/// else is memory corruption, and panics.
///
/// A variant prints and parses as its name, or its #[label("..")].
/// `#[nr]`: Self::NR, past the
/// largest value - a number, not one of its values. `#[own_to_text]`: it
/// prints as more than its name, by its own ToText.
///
/// A variant is written with its doc, and its columns - `#[arm_columns(..)]`,
/// bch_enum.rs - each made a const fn of the enum:
///
///   #[bch_enum(open)]
///   #[nr]
///   #[arm_columns(flags: BtreeFlags, holds: set(BkeyType))]
///   #[repr(u32)]
///   pub enum Btree {
///       /// File data extent pointers and reservations
///       #[flags(extents, snapshots, data, trans_triggers)]
///       #[holds(whiteout, extent, reservation, ..)]
///       extents = 0,
///       ...
///   }
#[proc_macro_attribute]
pub fn bch_enum(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("#[bch_enum]", || {
        let kind = match syn::parse::<Ident>(attr).map(|k| k.to_string()).as_deref() {
            Ok("closed") => EnumKind::Closed,
            Ok("open")   => EnumKind::Open,
            Ok("flags")  => EnumKind::Flags,
            _ => return Err("takes closed, open or flags".into()),
        };
        bch_enum::declaration(kind, syn::parse(item).map_err(|e| e.to_string())?)
    })
}

/// ioctl numbers, C's _IO*() #defines:
///
///   ioctl! {
///       BCH_IOCTL_DISK_ADD = _IOW(0xbc, 4, c::bch_ioctl_disk),
///       BCHFS_IOC_REINHERIT_ATTRS = _IOR(0xbc, 64, *const core::ffi::c_char),
///   }
///
/// Rust gets a marker type for each, named as the #define, implementing
/// util::ioctl::Ioctl: the opcode, made by util::ioctl's _IO*() from the
/// target's own constants, and the argument type.
#[proc_macro]
pub fn ioctl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    expand("ioctl!", || {
        let io = parse_ioctl(input.into())?;

        let mut out = TokenStream::new();
        for e in &io.entries {
            let (name, dir, ty, nr) = (&e.name, &e.dir, &e.ty, &e.nr);
            let (arg, opcode) = match &e.arg {
                Some(a) => (quote!(#a), quote!(crate::util::ioctl::#dir::<#a>(#ty, #nr))),
                None => (quote!(()), quote!(crate::util::ioctl::_IO(#ty, #nr))),
            };
            // _IOWR -> dir::WR, _IO -> dir::IO
            let dir = dir.to_string();
            let dir_ty = format_ident!("{}", dir.strip_prefix("_IO").filter(|d| !d.is_empty()).unwrap_or("IO"));
            out.extend(quote! {
                #[allow(non_camel_case_types)]
                pub struct #name;
                impl crate::util::ioctl::Ioctl for #name {
                    const OPCODE: u32 = #opcode;
                    type Arg = #arg;
                    type Dir = crate::util::ioctl::dir::#dir_ty;
                }
            });
        }
        Ok(out)
    })
}
