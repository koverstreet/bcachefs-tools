// SPDX-License-Identifier: GPL-2.0
//! The macro side of util/place.rs - a value's place, the field of a parent
//! it lives at.
//!
//! `#[placed(Base.field)]`, or `#[placed(Base.field[], index = NAME: TYPE)]`
//! for an element of that array, ahead of `#[pin_data]` and the derives, is
//! all a placed type says, and all its parent needs - `[..]` for an element
//! of the Container there is below. It gives the struct:
//!
//! - its At field, `#[placed_at]` - `_placed_at`, zero bytes, char[0] to C;
//!   or an element's index, NAME, its AtIndex C's TYPE - for the derives
//!   after it to see as any other field: `#[derive(Init)]` gives its init()
//!   the At to put there;
//! - Placed - its parent, and its At;
//! - InitAt for Base's field - its init(), the At from at(), or for an
//!   array each element's from at_index() - which Base's `#[derive(Init)]`
//!   picks for the field: so an At is made only for the field it names, and
//!   nothing is written on the parent;
//! - its #[cfg]s, on all of these;
//! - no drop of its own: its container() would see a parent partly dropped.
//!   A #[pin_data] struct can't impl Drop, and #[pin_data(PinnedDrop)] is
//!   refused; a struct without #[pin_data] gets an empty Drop, for one of its
//!   own to conflict with.
//!
//! For an element of a Container, it gives the struct its AtIn field, and
//! PlacedIn, and Base's InitAt for the field - the Container, by
//! util/place.rs's container() - but neither Placed nor a container() - an
//! element's place is for the guarantee it never outlives its Base - nor
//! any rule on its drop. The At field's attributes, #[placed_at] and #[c],
//! are #[derive(Init)]'s and #[derive(CStruct)]'s, so it has them only if it
//! has those.
//!
//! And `#[derive(Init)]` (init.rs) makes a Field<N> for each field of the
//! structs it's on, N its name's hash - a field as a type, for the children
//! placed at it - and initializes one without #[init] with field_init(): its
//! InitAt, if a placed type lives there, else zeroed.

use proc_macro::{Delimiter, Group, TokenStream, TokenTree};

use crate::cstruct::CFieldDef;
use crate::tokens;

/// Where #[placed] says a type lives: at Base.field; as an element of the
/// array there, holding its index as @0, of integer type @1; or as an
/// element of the Container there.
enum Kind {
    Field,
    Array(String, String),
    Container,
}

struct PlaceAttr {
    base:  String,
    field: String,
    kind:  Kind,
}

fn parse_attr(attr: TokenStream) -> Result<PlaceAttr, String> {
    let usage = "takes Base.field; Base.field[], index = NAME: TYPE, for an element of the array there, its index \
                 the field NAME of integer TYPE; or Base.field[..], for an element of the Container there";
    let attr: String = attr.to_string().chars().filter(|c| !c.is_whitespace()).collect();
    let (path, index) = match attr.split_once(',') {
        Some((path, rest)) => {
            let (name, ty) = rest.strip_prefix("index=").and_then(|r| r.split_once(':'))
                .ok_or_else(|| format!("{usage} - not {attr:?}"))?;
            (path, Some((name.to_string(), ty.to_string())))
        }
        None => (attr.as_str(), None),
    };
    let (path, kind) = if let Some(p) = path.strip_suffix("[..]") {
        (p, Kind::Container)
    } else if let Some(p) = path.strip_suffix("[]") {
        let (name, ty) = index.clone()
            .ok_or_else(|| format!("{usage}: an array's element names its index - not {attr:?}"))?;
        (p, Kind::Array(name, ty))
    } else {
        (path, Kind::Field)
    };
    if index.is_some() && !matches!(kind, Kind::Array(..)) {
        return Err(format!("{usage}: only an array's element has an index - not {attr:?}"));
    }
    let (base, field) = path.rsplit_once('.')
        .filter(|(b, f)| !b.is_empty() && !f.is_empty() && f.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()))
        .ok_or_else(|| format!("{usage} - not {attr:?}"))?;
    Ok(PlaceAttr { base: base.into(), field: field.into(), kind })
}

/// `#[placed(...)]`: the struct, its At field added, and its impls.
pub(crate) fn placed(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let PlaceAttr { base, field, kind } = parse_attr(attr)?;

    let mut trees: Vec<TokenTree> = item.into_iter().collect();

    // Its outer attributes - #[pin_data], its derives, its #[cfg]s - then the
    // struct keyword and its name
    let mut pin_data: Option<bool> = None;  // whether PinnedDrop
    let mut cstruct = false;                // whether C has it
    let mut init = false;                   // whether #[derive(Init)]'s
    let mut cfgs = String::new();           // for its impls too
    let mut i = 0;
    while let (Some(TokenTree::Punct(p)), Some(TokenTree::Group(g))) = (trees.get(i), trees.get(i + 1)) {
        if p.as_char() != '#' || g.delimiter() != Delimiter::Bracket {
            break;
        }
        let a: String = g.stream().to_string().chars().filter(|c| !c.is_whitespace()).collect();
        let path = a.split('(').next().unwrap_or_default();
        if path == "pin_data" || path.ends_with("::pin_data") {
            pin_data = Some(a.contains("PinnedDrop"));
        }
        if path == "derive" {
            cstruct |= a.contains("CStruct");
            init |= a.contains("Init");
        }
        if path == "cfg" {
            cfgs.push_str(&format!("#[{}]\n", g.stream()));
        }
        i += 2;
    }
    let kw = trees[i..].iter()
        .position(|t| matches!(t, TokenTree::Ident(id) if id.to_string() == "struct"))
        .map(|k| k + i)
        .ok_or("is for a struct with named fields")?;
    let name = match trees.get(kw + 1) {
        Some(TokenTree::Ident(id)) => id.to_string(),
        _ => return Err("is for a struct with named fields".into()),
    };
    if matches!(trees.get(kw + 2), Some(TokenTree::Punct(p)) if p.as_char() == '<') {
        return Err(format!("{name}: a placed type has no generics"));
    }

    // Its At, and the field it's in: an element's index, as C names it, or
    // a field of no bytes
    let n = format!("{{ crate::util::place::name({field:?}) }}");
    let (at, at_field, at_decl) = match &kind {
        Kind::Field => (
            format!("crate::util::place::At<{base}, {n}>"),
            AT_FIELD.to_string(),
            format!("char {AT_FIELD}[0]"),
        ),
        Kind::Array(idx, ty) => {
            let c_ty = match crate::cstruct::field_type(ty)? {
                crate::cstruct::FieldType::Named { c: Some(c), .. } => c,
                _ => return Err(format!("{name}: its index, {idx}: {ty}, isn't an integer C has")),
            };
            (format!("crate::util::place::AtIndex<{base}, {n}, {ty}>"), idx.clone(), format!("{c_ty} {idx}"))
        }
        Kind::Container => (
            format!("crate::util::place::AtIn<{name}>"),
            AT_FIELD.to_string(),
            format!("char {AT_FIELD}[0]"),
        ),
    };

    // The At field, last
    let body = match trees.last() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => g.stream(),
        _ => return Err(format!("{name}: is for a struct with named fields")),
    };
    let sep = match body.clone().into_iter().last() {
        None => "",
        Some(TokenTree::Punct(p)) if p.as_char() == ',' => "",
        _ => ",",
    };
    // #[derive(Init)]'s init() takes the At, and C has a declaration of it -
    // if they're there: the attributes are theirs
    let init_attr = if init { "#[placed_at]" } else { "" };
    let c_attr = if cstruct { format!("#[c(\"{at_decl}\")]") } else { String::new() };
    let body = tokens(&format!("{body}{sep} {init_attr} {c_attr} pub(crate) {at_field}: {at},"));
    *trees.last_mut().unwrap() = TokenTree::Group(Group::new(Delimiter::Brace, body));
    let item: TokenStream = trees.into_iter().collect();

    let init_at = |init: &str| format!(
        "{cfgs}#[automatically_derived]\n\
         // SAFETY: init() is Base's for {field}, which {base}'s #[derive(Init)]\n\
         // runs it into, the field it's for\n\
         unsafe impl crate::util::place::InitAt<{n}> for {base} {{\n\
             unsafe fn init() -> impl ::pin_init::PinInit<\n\
                 <Self as crate::util::place::Field<{n}>>::Type,\n\
                 crate::errcode::BchError>\n\
             {{\n\
                 // SAFETY: the caller's\n\
                 unsafe {{ {init} }}\n\
             }}\n\
         }}\n");

    // A field's or an array element's: Placed, its InitAt, and no drop of
    // its own. A container's element: PlacedIn, its container's InitAt -
    // and its drop is its own, the container whole while it runs
    let impls = match &kind {
        Kind::Field | Kind::Array(..) => {
            if pin_data == Some(true) {
                return Err(format!("{name}: a placed type has no drop of its own - its container() would see a \
                                    parent partly dropped - so not #[pin_data(PinnedDrop)]"));
            }
            let init = match &kind {
                Kind::Field => format!("crate::util::place::at({name}::init)"),
                _ => format!("::pin_init::pin_init_array_from_fn(|i| unsafe {{ \
                              crate::util::place::at_index(i, {name}::init) }})"),
            };
            // #[pin_data]'s own check refuses a Drop; without it, this is the
            // one a Drop of its own would conflict with
            let no_drop = match pin_data {
                Some(_) => String::new(),
                None => format!("{cfgs}#[automatically_derived]\n\
                                 impl Drop for {name} {{\n\
                                     fn drop(&mut self) {{}}\n\
                                 }}\n"),
            };
            format!(
                "{cfgs}#[automatically_derived]\n\
                 // SAFETY: its At field, which its init() takes - given by InitAt's,\n\
                 // from at() or at_index(), for the place the At names\n\
                 unsafe impl crate::util::place::Placed for {name} {{\n\
                     type Parent = {base};\n\
                     type At = {at};\n\
                     unsafe fn placed_at_raw(p: *const Self) -> *const Self::At {{\n\
                         // SAFETY: the caller's\n\
                         unsafe {{ &raw const (*p).{at_field} }}\n\
                     }}\n\
                 }}\n\
                 {}{no_drop}", init_at(&init))
        }
        Kind::Container => format!(
            "{cfgs}#[automatically_derived]\n\
             // SAFETY: its AtIn field, which only its container's insert() makes\n\
             unsafe impl crate::util::place::PlacedIn for {name} {{\n\
                 type Base = {base};\n\
             }}\n\
             {}", init_at(&format!("crate::util::place::container::<Self, {n}, {name}, _>()"))),
    };

    Ok(tokens(&format!("{item}\n{impls}")))
}

/// The At field #[placed] adds, where there's no index for it to be.
const AT_FIELD: &str = "_placed_at";

/// Whether @f is a place's At: the field #[placed] added.
pub(crate) fn is_at_field(f: &CFieldDef) -> bool {
    f.placed_at
}

/// Whether @f is an At of no bytes - not an element's index - which
/// TypeInfo doesn't describe.
pub(crate) fn is_empty_at_field(f: &CFieldDef) -> bool {
    f.placed_at && f.name == AT_FIELD
}

/// Field<N> for field @name, of type @ty, of @inst_ty - @impl_lts its
/// lifetimes, @cfg the field's #[cfg].
pub(crate) fn field_impl(cfg: &str, impl_lts: &str, inst_ty: &str, name: &str, ty: &str) -> String {
    format!(
        "{cfg}#[automatically_derived]\n\
         // SAFETY: its field {name}, by offset_of!()\n\
         unsafe impl{impl_lts} crate::util::place::Field<{{ crate::util::place::name({name:?}) }}> for {inst_ty} {{\n\
             type Type = {ty};\n\
             const OFFSET: usize = ::core::mem::offset_of!({inst_ty}, {name});\n\
         }}\n")
}

/// The initializer for field @name, without #[init]: InitAt's, if a placed
/// type lives there, else zeroed - util/place.rs's Pick.
pub(crate) fn field_init(name: &str) -> String {
    format!(
        "{{\n\
             use crate::util::place::{{PickPlaced as _, PickZeroed as _}};\n\
             // SAFETY: run into field {name}, by pin_init!()\n\
             unsafe {{ (&crate::util::place::Pick::<Self, {{ crate::util::place::name({name:?}) }}>::new()).pick() }}\n\
         }}")
}
