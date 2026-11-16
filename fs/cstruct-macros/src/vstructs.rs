// SPDX-License-Identifier: GPL-2.0
//! What util/vstructs.rs's traits need of a type, from its
//! definition: #[derive(Plain)] - and #[bitfield], every one of which is -
//! #[derive(FlexArray)], from #[counted_by(FIELD)], #[derive(View)], its
//! fields' projections on a View of it, and the families, a type's X<S> in
//! each state: #[unsized_vstruct], an unsized vstruct's, and #[counted], a
//! header and the array it counts - X<S> its own definition in every state;
//! #[whole] and #[grown], a sized type's, which in place is a definition of
//! its own.
//!
//! Plain is a promise: every bit pattern of the type's bytes is a value of
//! it, and every byte is a field's. These impls check it at compile time,
//! rather than take it on trust: each field's type is Plain, and the fields'
//! sizes add up to the type's, so there's no padding - zerocopy's FromBytes,
//! for the types we define.

use proc_macro2::{Ident, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens};
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::{parse_quote, Token};

use crate::cstruct::{flex_elem, generic_arg, parse_cstruct, split_commas, FieldDef, StructKind, StructDef};
use crate::tokens::vs;

/// #[derive(Plain)], on a struct - not a union, whose bytes past a short
/// arm's end are no field's. An unsized vstruct's states aren't: they're its
/// definition's as written - #[unsized_vstruct]'s X_head - and then its tail's
/// elements, Plain too, as the attribute checks.
pub(crate) fn derive_plain(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input)?;
    if def.kind != StructKind::Struct {
        return Err(format!("{}: Plain is for structs", def.name()));
    }
    let tys: Vec<&syn::Type> = def.fields.iter().map(|f| &f.ty).collect();
    Ok(plain(&def.ident, &tys))
}

/// #[derive(SizedVal)]: the marker, for a key value - as an on-disk value,
/// any bytes of it are one.
pub(crate) fn derive_sized_val(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input)?;
    let (name, vs) = (&def.ident, vs());
    // the same struct in every state, but from outside: OfSized
    Ok(quote! {
        // SAFETY: an on-disk value, any bytes of which are one
        unsafe impl crate::btree::bkey::SizedVal for #name {}
        impl #vs::InState for #name {
            type In<S: #vs::State> = S::OfSized<Self>;
            type State = #vs::Validated;
        }
    })
}

/// Plain for @name, whose fields are of types @tys: the impl, and the
/// checks that make it sound - a compile error if it isn't. Native too, as
/// itself.
pub(crate) fn plain(name: &Ident, tys: &[impl ToTokens]) -> TokenStream {
    let msg = format!("{name}: padding between its fields - it isn't Plain");
    // and Native as itself: a struct reads as a struct. In-kernel, the
    // kernel's own FromBytes and AsBytes - what uaccess's UserSlice reads and
    // writes - whose conditions are Plain's: any bytes are one, every byte is
    // a field's, and no field is a cell
    quote! {
        unsafe impl crate::util::vstructs::Plain for #name {}
        #[cfg(kernel)]
        unsafe impl ::kernel::transmute::FromBytes for #name {}
        #[cfg(kernel)]
        unsafe impl ::kernel::transmute::AsBytes for #name {}
        impl crate::util::vstructs::Native for #name {
            type Native = Self;
            fn to_native(self) -> Self { self }
            fn from_native(n: Self) -> Self { n }
        }
        const _: () = {
            #(crate::util::vstructs::assert_plain::<#tys>();)*
            assert!(::core::mem::size_of::<#name>() == 0 #(+ ::core::mem::size_of::<#tys>())*, #msg);
        };
    }
}

/// A View's own methods, which a field's projection can't be named.
const VIEW_METHODS: &[&str] = &["new", "get", "set", "whole", "whole_mut", "project", "as_view", "as_bytes"];

/// #[derive(View)], on a Plain struct: a projection for each field, on View
/// and ViewMut - `v.field()`, a View of the field, until field projections
/// make it `v.field`. Not for fields named `_..`, padding and internals,
/// or a trailing `[T; 0]`: a flexible array is View::as_bytes()'s.
pub(crate) fn derive_view(input: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&input)?;
    if def.kind != StructKind::Struct {
        return Err(format!("{}: a View is of a struct", def.name()));
    }
    let name = &def.ident;
    let (mut fields, mut tys) = (Vec::new(), Vec::new());
    for f in &def.fields {
        let field = &f.ident;
        if field.to_string().starts_with('_') || flex_elem(&f.ty).is_some() {
            continue;
        }
        if VIEW_METHODS.iter().any(|m| field == m) {
            return Err(format!("{name}.{field}: a View's own method is named {field} - \
                                its projection would be hidden"));
        }
        fields.push(field);
        tys.push(&f.ty);
    }
    let names: Vec<String> = fields.iter().map(ToString::to_string).collect();
    Ok(quote! {
        impl<'a> crate::util::projections::View<'a, #name> {
            #(pub fn #fields(&self) -> crate::util::projections::View<'a, #tys> {
                self.project(::core::mem::offset_of!(#name, #fields), #names)
            })*
        }
        impl<'a> crate::util::projections::ViewMut<'a, #name> {
            #(pub fn #fields(&mut self) -> crate::util::projections::ViewMut<'_, #tys> {
                self.project(::core::mem::offset_of!(#name, #fields), #names)
            })*
        }
    })
}

// ---- Families: a type in each state, X<S> (util/vstructs.rs' Family) ----

/// A family attribute's arguments: `#[counted(PATH, tail = ..)]`,
/// `#[unsized_vstruct(tail = ..)]`, `#[grown(from = FIELD)]`; `bkey`, for a
/// key's value; and a vstruct's `u64s = ..`, `start = ..`, `entries = ..`.
#[derive(Default)]
struct FamilyArgs {
    /// The one argument that isn't `key = value`: the count, a field or a
    /// field's, FIELD.COUNT.
    count: Option<Vec<Ident>>,
    /// `tail = [E]` or `tail = NAME: [E]`: the tail's name, if it's named,
    /// and E.
    tail: Option<(Option<Ident>, syn::Type)>,
    /// `from = FIELD`.
    from: Option<Ident>,
    /// `bkey`: a key's value - its btree/bkey/types.rs impls too.
    bkey: bool,
    /// `u64s = PATH[, start = EXPR]`: a vstruct's count of u64s, and where
    /// they start if not past its header - vstruct!()'s.
    u64s: Option<TokenStream>,
    start: Option<TokenStream>,
    /// `entries = [thin] E`: what its entries are - vstruct_container!()'s.
    entries: Option<TokenStream>,
}

/// @attr, a family attribute's arguments: an error for one it doesn't take -
/// @takes, of "count", "tail", "from", "bkey", "u64s", "start" and "entries" -
/// or one not written as its kind is. The caller names the attribute and the
/// struct.
fn family_args(attr: TokenStream, takes: &[&str]) -> Result<FamilyArgs, String> {
    let mut args = FamilyArgs::default();
    for part in split_commas(attr) {
        let toks: Vec<TokenTree> = part.clone().into_iter().collect();
        let (kind, set) = match toks.as_slice() {
            [] => continue,
            [TokenTree::Ident(k), TokenTree::Punct(eq), v @ ..] if eq.as_char() == '=' => {
                let v: TokenStream = v.iter().cloned().collect();
                match k.to_string().as_str() {
                    "tail" => {
                        let tail = syn::parse::Parser::parse2(|i: ParseStream<'_>| {
                            let name = match i.peek2(Token![:]) {
                                true => {
                                    let n: Ident = i.parse()?;
                                    i.parse::<Token![:]>()?;
                                    Some(n)
                                }
                                false => None,
                            };
                            let slice: syn::TypeSlice = i.parse()?;
                            Ok((name, *slice.elem))
                        }, v.clone()).map_err(|_| format!("tail = [E] - a slice - not tail = {v}"))?;
                        ("tail", args.tail.replace(tail).is_some())
                    }
                    "from" => {
                        let from = syn::parse2(v).map_err(|_| format!("({part}): from = FIELD"))?;
                        ("from", args.from.replace(from).is_some())
                    }
                    "u64s"    => ("u64s", args.u64s.replace(v).is_some()),
                    "start"   => ("start", args.start.replace(v).is_some()),
                    "entries" => ("entries", args.entries.replace(v).is_some()),
                    k => return Err(format!("no argument {k}")),
                }
            }
            [TokenTree::Ident(k)] if k == "bkey" => ("bkey", core::mem::replace(&mut args.bkey, true)),
            _ => {
                let path = syn::parse::Parser::parse2(Punctuated::<Ident, Token![.]>::parse_separated_nonempty,
                                                      part.clone())
                    .map_err(|_| format!("({part}): the count is a field, or a field's, FIELD.COUNT"))?;
                ("count", args.count.replace(path.into_iter().collect()).is_some())
            }
        };
        if !takes.contains(&kind) {
            return Err(format!("doesn't take ({part})"));
        }
        if set {
            return Err(format!("{kind} given twice"));
        }
    }
    Ok(args)
}

/// @name's vstruct!() and vstruct_container!(), from its attribute's `u64s =
/// PATH[, start = EXPR]` and `entries = [thin] E`: an unsized type's, with
/// @head, counted by its header. @written is the struct as written, its
/// entries from its start[].
fn vstruct_of(name: &Ident, head: Option<&Ident>, written: &Ident, args: &FamilyArgs)
    -> Result<TokenStream, String>
{
    let vs = vs();
    if args.start.is_some() && args.u64s.is_none() {
        return Err(format!("{name}: start = without u64s = - it's where the u64s count from"));
    }
    let counted = match head {
        Some(head) => quote!(unsized #head),
        None => quote!(#name),
    };
    let start = args.start.as_ref().map(|s| quote!(, start = #s));
    let u64s = args.u64s.as_ref().map(|c| quote!(#vs::vstruct!(#counted, #c #start);));
    let entries = args.entries.as_ref().map(|e| quote!(#vs::vstruct_container!(#name, #written, #e);));
    Ok(quote!(#u64s #entries))
}

/// A field of the struct as written, as a state's definition has it, inline:
/// pub only where it's pub and holds no count - not the count, written
/// private or pub(crate), nor an embedded header, Head<..> or an array of
/// them, whose own count it is, nor one of @private, which the family names:
/// a counted type's count. Those read through the definition's Deref to its
/// head, and can't be written: a count changes only through what holds the
/// value.
fn inline_field(f: &FieldDef, private: &[Ident]) -> TokenStream {
    let public = matches!(f.vis, syn::Visibility::Public(_));
    let elem = match &f.ty {
        syn::Type::Array(a) => &*a.elem,
        t => t,
    };
    let vis = (public && generic_arg(elem, "Head").is_none() && !private.contains(&f.ident)).then(|| quote!(pub));
    let (field, ty) = (&f.ident, &f.ty);
    quote!(#vis #field: #ty,)
}

/// The tail of @def, the struct as written of a type whose count is its own,
/// and the fields before it: its trailing [E; 0], or what #[@attr(tail = ..)]
/// says - `[E]`, the states' `_tail`, or `NAME: [E]` - after all of them.
fn own_tail<'a>(def: &'a StructDef, tail: Option<(Option<Ident>, syn::Type)>, attr: &str)
    -> Result<(Ident, syn::Type, &'a [FieldDef]), String>
{
    let name = &def.ident;
    if let Some((tail, elem)) = tail {
        return Ok((tail.unwrap_or_else(|| format_ident!("_tail")), elem, &def.fields[..]));
    }
    let (last, fields) = def.fields.split_last().ok_or_else(|| format!("{name}: #[{attr}] is for a struct with fields"))?;
    let elem = flex_elem(&last.ty).ok_or_else(|| format!("{name}: its last field, {}, isn't a flexible array, \
                                                         [E; 0] - #[{attr}(tail = [E])] says what its tail is",
                                                        last.ident))?;
    Ok((last.ident.clone(), elem.clone(), fields))
}

/// The item's #[repr]s, as written.
fn repr_attrs(def: &StructDef) -> TokenStream {
    let repr = def.attrs.iter().filter(|a| a.path().is_ident("repr"));
    quote!(#(#repr)*)
}

/// What a state's definition is laid out as, for dst().
struct Shape<'a> {
    /// The item's #[repr]s, as written.
    repr:    TokenStream,
    /// The fields it has inline, at @head's offsets.
    fields:  &'a [FieldDef],
    /// Those the family makes private: a counted type's count.
    private: Vec<Ident>,
    /// What precedes the tail: the struct as written, or a grown type's floor.
    head:    Ident,
    /// What it derefs to, #[repr(transparent)] over @head if not @head
    /// itself: a #[whole] type's X<Whole>, where X's methods are.
    deref:   Ident,
    /// After the fields: @tail, a slice of @elem.
    tail:    Ident,
    elem:    syn::Type,
    /// Where the tail starts, if not where the head ends: inside it, the
    /// head being the struct as written, rounded up into the tail's first
    /// bytes - a key value's (#[whole(tail = ..)]).
    tail_at: Option<TokenStream>,
}

/// A state's definition, @ty - or with @generic, @ty<S>, an unsized type's in
/// every state, laid out alike - as @shape: a DST, its fields inline, those
/// that hold a count private (inline_field()), then its tail.
///
/// Its fields are its head's, at the head's offsets - asserted - so it derefs
/// to the head: reads, and the head's methods, written once there. Its own
/// fields that hold a count are private, and it's unsized, so it can't be
/// assigned or swapped whole. Its impls are vstruct_family!(dst ..)'s. @doc
/// is its own; @methods, more of them.
fn dst(ty: &Ident, generic: bool, doc: &str, shape: &Shape<'_>, methods: TokenStream) -> TokenStream {
    let vs = vs();
    let (decl, params, args, state) = match generic {
        true => (quote!(<S: #vs::State = #vs::Validated>), quote!(S: #vs::State), quote!(<S>),
                 quote!(_state: ::core::marker::PhantomData<S>,)),
        false => Default::default(),
    };
    let Shape { repr, fields, private, head, deref, tail, elem, tail_at } = shape;
    let tail_at = tail_at.clone().unwrap_or_else(|| quote!(::core::mem::size_of::<#head>()));
    let plain_fields = fields.iter().map(|f| {
        let ty = &f.ty;
        quote!(#vs::assert_plain::<#ty>();)
    });
    let inline = fields.iter().map(|f| inline_field(f, private));
    let offsets = fields.iter().map(|f| {
        let field = &f.ident;
        let msg = format!("{ty}.{field}: not at {head}'s offset");
        quote!(assert!(::core::mem::offset_of!(#ty, #field) == ::core::mem::offset_of!(#head, #field), #msg);)
    });
    let tail_start = match fields.last() {
        Some(l) => {
            let (field, fty) = (&l.ident, &l.ty);
            quote!(::core::mem::offset_of!(#ty, #field) + ::core::mem::size_of::<#fty>())
        }
        None => quote!(0usize),
    };
    let tail_msg = format!("{ty}: its tail starts where its fields end, at TAIL");
    quote! {
        #[doc = #doc]
        #repr
        #[allow(non_camel_case_types)]
        pub struct #ty #decl {
            #state
            #(#inline)*
            pub #tail: [#elem],
        }

        crate::vstruct_family!(dst [#params] #ty #args, #head, #deref, #elem, #tail_at);

        const _: () = {
            #(#plain_fields)*
            #(#offsets)*
            // its tail starts at TAIL - where its head ends, or inside it:
            // from_raw()'s length counts on it
            assert!((#tail_start).next_multiple_of(::core::mem::align_of::<#elem>())
                        == <#ty as #vs::Unsized>::TAIL,
                    #tail_msg);
        };

        #methods
    }
}

/// @name's family, @name_family - its Unvalidated, Validated and Whole
/// definitions. Every family has @name_unvalidated and @name_validated: a
/// plain alias where a state shares another's definition, so a container's
/// ArmOf impls can name each state's - concrete types, not projections.
fn family(name: &Ident, unvalidated: &TokenStream, validated: &TokenStream, whole: &TokenStream) -> TokenStream {
    let vs = vs();
    let fam = crate::tokens::derived(name, "family");
    let aliases = [("unvalidated", unvalidated), ("validated", validated)].into_iter()
        .map(|(st, def)| (crate::tokens::derived(name, st), def))
        .filter(|(alias, def)| alias.to_string() != def.to_string())
        .map(|(alias, def)| quote! {
            #[allow(non_camel_case_types)]
            pub type #alias = #def;
        });
    quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #fam;

        impl #vs::Family for #fam {
            type Unvalidated = #unvalidated;
            type Validated = #validated;
            type Whole = #whole;
        }

        #(#aliases)*
    }
}

/// A #[members] field's members on each of @defs too - a family's definitions
/// in place, with its fields inline, each `[impl generics] type`: their Deref
/// to the struct as written can't reach its setters.
fn members_on(name: &Ident, fields: &[FieldDef], defs: &[TokenStream]) -> Result<TokenStream, String> {
    let mut out = TokenStream::new();
    for f in fields.iter().filter(|f| f.bitfield) {
        if let Some(fwd) = crate::bitfield::members_forwarder(name, f)? {
            let field = &f.ident;
            out.extend(defs.iter().map(|d| quote!(#fwd!(#d, #field);)));
        }
    }
    Ok(out)
}

/// The family of a type whose count is its own - an unsized vstruct's, a
/// #[counted] type's: its states are laid out alike, as @shape, so X<S> is
/// its definition in each - Unvalidated, bytes from outside, its count
/// checked against them; Validated, its count trusted, validate's; and
/// Validated its Whole, the header never having grown.
fn own_count_family(name: &Ident, shape: &Shape<'_>) -> Result<TokenStream, String> {
    let vs = vs();
    let doc = format!(" {name}, in state S - Validated unless named: its fields - those that\n \
                       hold a count private - then its tail, as long as its count says, so\n \
                       a reference to one reaches every byte of it and none past. It\n \
                       derefs to its head, Head<{name}>, to read. Unvalidated, it's bytes\n \
                       from outside - off a device, from the kernel - its count checked\n \
                       against them when it's made.");
    let mut out = dst(name, true, &doc, shape, TokenStream::new());
    let head = &shape.head;
    out.extend(quote!(crate::vstruct_family!(unsized #name, #head);));
    let (unvalidated, validated) = (quote!(#name<#vs::Unvalidated>), quote!(#name<#vs::Validated>));
    out.extend(members_on(name, shape.fields, &[quote!([<S: #vs::State>] #name<S>)])?);
    out.extend(family(name, &unvalidated, &validated, &validated));
    Ok(out)
}

/// #[unsized_vstruct]: an unsized vstruct's family (util/vstructs.rs'
/// Family), from the struct as written.
///
/// The item is the header, renamed X_head, with #[type_name(X)]: Head<X>.
/// X<S> is the type in state S, Validated unless named: a DST, the header
/// and then its tail, as long as its count says - which derefs to the
/// header, where its fields and methods are, written once. Its header never
/// grew: Validated is whole.
///
/// The tail is the item's trailing flexible array, `NAME: [E; 0]` - X's is
/// `NAME: [E]` - or, where the header doesn't end in one, what the attribute
/// says: `tail = [E]`, X's `_tail`, the raw rest - a superblock field's, past
/// its header; a btree node's, its bset's keys - or `tail = NAME: [E]`, an
/// array X calls NAME.
///
/// `u64s = PATH[, start = EXPR]` is its count, vstruct!()'s; `entries = [thin]
/// E`, what its entries are, vstruct_container!()'s.
pub(crate) fn unsized_vstruct(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item)?;
    let name = &def.ident;
    let last = def.fields.last().ok_or_else(|| format!("{name}: an unsized vstruct has fields"))?;
    let args = family_args(attr, &["tail", "u64s", "start", "entries"]).map_err(|e| format!("{name}: {e}"))?;
    let written = crate::tokens::derived(name, "head");
    let vstruct = vstruct_of(name, Some(&written), &written, &args)?;
    let (tail, elem, fields) = own_tail(&def, args.tail, "unsized_vstruct")?;
    let last = &last.ident;
    let shape = Shape { repr: repr_attrs(&def), fields, private: Vec::new(), head: written.clone(),
                        deref: written.clone(), tail, elem, tail_at: None };

    let mut out = rename_struct(item, &def, &written)?;
    let msg = format!("{name}: its tail starts where its header ends");
    out.extend(quote! {
        const _: () = {
            // the struct as written ends at its last field - its flexible
            // array, or an embedded vstruct's header, which ends where that
            // one's tail starts
            assert!(::core::mem::offset_of!(#written, #last) + crate::types::field_size(
                        |p: #written| p.#last) as usize
                    == ::core::mem::size_of::<#written>(),
                    #msg);
        };
    });
    out.extend(own_count_family(name, &shape)?);
    out.extend(vstruct);
    Ok(out)
}

/// #[counted], #[counted(PATH)], #[counted(PATH, tail = ..)]: the family of a
/// type that's a header and an array it counts in elements - an ioctl's
/// reply, a replicas usage record. Not a vstruct: no u64s, no entries to
/// walk; its tail is just the array.
///
/// PATH is the count, as vstruct!'s is - a field, or a field of one it
/// embeds (r.nr_devs) - and nr_entries if not named, the convention. Its
/// first field holds the count, so that's private in each state's
/// definition; the head keeps it as written, so a header can still be
/// built standalone. The array is the item's trailing [E; 0], or what the
/// attribute says - as #[unsized_vstruct]'s, `tail = [E]`, the states'
/// `_tail`, or `tail = NAME: [E]`, a field of a field the states call NAME.
///
/// The head gets FlexArray from the count - its size, TAIL and the
/// elements past it - and CountedHead, the way to its Unvalidated definition
/// from what holds its bytes.
pub(crate) fn counted(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let vs = vs();
    let def = parse_cstruct(&item)?;
    let name = &def.ident;
    let written = crate::tokens::derived(name, "head");

    let args = family_args(attr, &["count", "tail"]).map_err(|e| format!("{name}: {e}"))?;
    let count = args.count.unwrap_or_else(|| vec![format_ident!("nr_entries")]);
    let count_field = &count[0];
    if !def.fields.iter().any(|f| f.ident == *count_field) {
        let count = quote!(#(#count).*);
        return Err(format!("{name}: #[counted({count})], but {name} has no field {count_field} - \
                            name its count, #[counted(FIELD)] or #[counted(FIELD.COUNT)]"));
    }

    // the array: a trailing [E; 0], or a field of a field - after the head,
    // where dst() asserts its tail starts
    let named = args.tail.is_some();
    let (tail, elem, fields) = own_tail(&def, args.tail, "counted")?;
    let tail_at = match named {
        true => quote!(::core::mem::size_of::<#written>()),
        false => quote!(::core::mem::offset_of!(#written, #tail)),
    };
    let shape = Shape { repr: repr_attrs(&def), fields, private: vec![count_field.clone()], head: written.clone(),
                        deref: written.clone(), tail, elem: elem.clone(), tail_at: None };

    let mut out = rename_struct(item, &def, &written)?;
    let msg = format!("{name}: its elements aren't a whole number of its alignment");
    out.extend(flex_impl(&written, &tail_at, &elem, &count));
    out.extend(quote! {
        impl #vs::CountedHead for #written {
            type Of<S: #vs::State> = #name<S>;
        }

        // a reference as long as its count says is a whole number of its
        // alignment only if its elements are: else it claims bytes past it
        const _: () = assert!(::core::mem::size_of::<#elem>() % ::core::mem::align_of::<#written>() == 0, #msg);
    });
    out.extend(own_count_family(name, &shape)?);
    Ok(out)
}

/// #[grown(from = FIELD)]: the family of a type that grew between format
/// versions, from the struct as written - FIELD the first field a valid
/// value may lack, written by a version from before it.
///
/// X<S> is a generic struct over its definition in state S. X<Whole> - X -
/// is the whole struct, every field a field: the item, renamed X_whole with
/// #[type_name(X)]. X_floor is the fields before FIELD, every valid
/// value's. X<Validated> - Unvalidated too, as the cast checks the floor - is
/// a value in place, a DST: the floor's fields, then the bytes there are, at
/// exactly the value's length; it derefs to the floor, to read. A field from
/// FIELD on is a getter, zero past the end, and whole() is all of it, X, if
/// it's all there.
pub(crate) fn grown(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item)?;
    let name = def.name();
    let args = family_args(attr, &["from", "bkey", "u64s", "start"]).map_err(|e| format!("{name}: {e}"))?;
    let from = args.from.as_ref()
        .ok_or_else(|| format!("{name}: #[grown(from = FIELD)], FIELD the first field an older \
                                version's value may lack"))?;
    let at = def.fields.iter().position(|f| f.ident == *from)
        .ok_or_else(|| format!("{name}: #[grown(from = {from})], but {name} has no field {from}"))?;
    if at == 0 {
        return Err(format!("{name}: #[grown(from = {from})]: every valid value has at least one field"));
    }
    let mut out = sized_family(&def, item, Some((from, at)), None, args.bkey)?;
    out.extend(vstruct_of(&def.ident, None, &crate::tokens::derived(&def.ident, "whole"), &args)?);
    Ok(out)
}

/// #[whole]: the family of a type every valid value of which has all of it -
/// validate rejects one shorter - though it may be longer, written by a
/// version since: a superblock section or journal entry of fixed fields.
///
/// X<S> is a generic struct over its definition in state S. X<Whole> - X -
/// is the struct: the item, renamed X_whole with #[type_name(X)].
/// X<Validated> and X<Unvalidated> are a value in place, a DST: X's fields,
/// those that hold a count private, then the bytes past them there are, at
/// exactly the value's length; it derefs to X, to read, and X's methods.
///
/// Or `#[whole(tail = NAME: [E])]`: the struct ends in NAME: [E; 0] - and
/// perhaps other zero-length arrays, other names for where it starts - a
/// header and the array after it: a key value, its length the key's. The
/// tail is NAME, from where the struct has it: inside the struct, if align
/// rounds the struct up into the array's first bytes - bch_inode's fields end
/// at 14, its size is 16 - which the deref to the struct reaches, a value
/// being no shorter than that.
///
/// Or `tail = NAME: [u8]`, over any zero-length array: the bytes from where
/// it starts, for a tail that's more than that array - bch_stripe's
/// pointers, then checksums and sector counts its header sizes.
///
/// With a tail, `entries = [thin] E`: what its entries are, from its start[]
/// to the value's end - vstruct_container!()'s.
pub(crate) fn whole(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item)?;
    let name = def.name();
    let args = family_args(attr, &["tail", "bkey", "u64s", "start", "entries"]).map_err(|e| format!("{name}: {e}"))?;
    if args.entries.is_some() && args.tail.is_none() {
        return Err(format!("{name}: #[whole(entries = ..)] without tail = - its entries are in its tail"));
    }
    let vstruct = vstruct_of(&def.ident, None, &crate::tokens::derived(&def.ident, "whole"), &args)?;
    let tail = match args.tail {
        None => None,
        Some((None, _)) => return Err(format!("{name}: #[whole(tail = NAME: [E])] - the tail is the struct's last \
                                               field, NAME: [E; 0], named")),
        Some((Some(tail), elem)) => {
            let at = def.fields.iter().position(|f| f.ident == tail)
                .ok_or_else(|| format!("{name}: #[whole(tail = {tail}: ..)], but it has no field {tail}"))?;
            // [E; 0] - or any zero-length array, for a tail of bytes
            let bytes: syn::Type = parse_quote!(u8);
            match flex_elem(&def.fields[at].ty) {
                Some(e) if *e == elem || elem == bytes => {}
                _ => {
                    let elem = quote!(#elem);
                    return Err(format!("{name}: #[whole(tail = {tail}: [{elem}])], but its field {tail} \
                                        isn't [{elem}; 0]"));
                }
            }
            // after it, only other names for where it starts
            for f in &def.fields[at + 1..] {
                if flex_elem(&f.ty).is_none() {
                    return Err(format!("{name}.{}: after the tail, {tail}, a field that isn't a \
                                        zero-length array - another name for its start", f.ident));
                }
            }
            Some((tail, elem))
        }
    };
    let mut out = sized_family(&def, item, None, tail, args.bkey)?;
    out.extend(vstruct);
    Ok(out)
}

/// #[grown] and #[whole]: the family of sized struct @def, written @item -
/// @grew, the first field an older version's value may lack and its index,
/// if it grew.
///
/// Both have a definition of all of it, every field inline: X_whole, if it
/// grew - its Whole; else X_validated, its every state.
fn sized_family(def: &StructDef, item: TokenStream, grew: Option<(&Ident, usize)>,
                tail: Option<(Ident, syn::Type)>, bkey: bool)
    -> Result<TokenStream, String>
{
    let vs = vs();
    let name = &def.ident;
    // A key's value: what a mutable copy of a key read as one is - and a
    // grown type's struct, had by value
    let bkey_impls = match (bkey, grew) {
        (false, _) => quote!(),
        (true, None) => quote! {
            // SAFETY: every valid value has its head
            unsafe impl crate::btree::bkey::MutBkey for #name<#vs::Validated> {
                type Mut = Self;
                const MIN_BYTES: usize = 0;
            }
        },
        (true, Some(_)) => quote! {
            // SAFETY: Plain, as #[grown] requires: no padding, any bytes a value
            unsafe impl crate::btree::bkey::BkeyWhole for #name {}

            // SAFETY: every key is one of AllTypes'
            unsafe impl crate::btree::bkey::InFamily<crate::btree::bkey::AllTypes> for #name {}

            // SAFETY: a BkeyWhole, its key whole
            unsafe impl crate::btree::bkey::MutBkey for #name<#vs::Validated> {
                type Mut = #name;
                const MIN_BYTES: usize = ::core::mem::size_of::<crate::btree::bkey::BkeyI<#name>>();
            }
        },
    };
    // The struct as written, renamed X_whole: X<Whole>'s representation
    let written = crate::tokens::derived(name, "whole");
    // A value in place, as long as what holds it says: for a grown type the
    // floor and the bytes there are; else all of its fields, then the bytes
    // past them
    let ty = crate::tokens::derived(name, "validated");
    let derives = derive_names(def);
    // X is generated: a derive vstruct_family! doesn't forward would be
    // X_whole's alone, out of X's reach. Ours of the struct as written are
    // X_whole's
    let of_whole = ["MaybeZeroable", "BitfieldMembers", "FlexArray"];
    if let Some(d) = derives.iter()
        .find(|d| !of_whole.iter().chain(WHOLE_DERIVES).any(|w| d == w)) {
        return Err(format!("{name}: #[derive({d})], but {name} - {name}<Whole> - is generated, and has \
                            only the derives it forwards to X<Whole>: {}", WHOLE_DERIVES.join(", ")));
    }
    let is_plain = derives.iter().any(|d| d == "Plain");
    let fl = crate::tokens::derived(name, "floor");

    let mut out = rename_struct(item, def, &written)?;
    let (shape, methods, doc, generic_doc) = match grew {
        None => whole_in_place(def, &written, tail),
        Some((from, at)) => {
            if !is_plain {
                return Err(format!("{name}: #[grown] without Plain - the struct is a value in place, every \
                                    byte of it read, so it can't have padding"));
            }
            let (floor, later) = def.fields.split_at(at);
            out.extend(floor_struct(name, &fl, &written, floor, from));
            grown_in_place(name, &fl, &ty, &written, floor, later)?
        }
    };
    out.extend(dst(&ty, false, &doc, &shape, methods));
    // a grown type's in place definition derefs to the floor: a floor
    // #[members] field's getters there, its setters on the definition
    let defs = [quote!([] #fl), quote!([] #ty)];
    out.extend(members_on(name, shape.fields, if grew.is_some() { &defs } else { &defs[1..] })?);
    // the cast checks its head - all of the struct, or a grown type's floor:
    // an unvalidated one is as good
    out.extend(family(name, &quote!(#ty), &quote!(#ty), &quote!(#written)));
    out.extend(quote! {
        // SAFETY: its fields are each Plain, as its value in place asserts; edited
        // in place only if it's Plain - padding would be written over its tail
        unsafe impl crate::util::vstructs::SizedArm for #written {
            const WRITABLE: bool = #is_plain;
        }
    });
    // X<S>: Whole, the struct, with the value traits it derives that are
    // X<Whole>'s too; in place, the DST - a typestate if the type grew
    let fam = crate::tokens::derived(name, "family");
    let x_derives = derives.iter().filter(|d| WHOLE_DERIVES.iter().any(|w| d == w));
    let typestate = grew.map(|_| quote!(, typestate));
    out.extend(quote! {
        crate::vstruct_family!(sized #name, #fam, #ty, #written, #generic_doc, [#(#x_derives),*] #typestate);
    });
    if grew.is_some() {
        // all of it is a value in place, every field there: X's methods, on
        // X<Validated>, are the struct's too
        out.extend(quote! {
            impl ::core::ops::Deref for #written {
                type Target = #name<#vs::Validated>;
                #[inline]
                fn deref(&self) -> &Self::Target {
                    // SAFETY: all of its bytes - Plain, each a field's - which a value in
                    // place is as long as
                    unsafe { &*<Self::Target as #vs::Unsized>::from_raw(
                        (self as *const Self).cast(), ::core::mem::size_of::<Self>()) }
                }
            }

            impl ::core::ops::DerefMut for #written {
                #[inline]
                fn deref_mut(&mut self) -> &mut Self::Target {
                    // SAFETY: as deref()'s
                    unsafe { &mut *<Self::Target as #vs::Unsized>::from_raw_mut(
                        (self as *mut Self).cast(), ::core::mem::size_of::<Self>()) }
                }
            }
        });
    }
    out.extend(bkey_impls);
    Ok(out)
}

/// A #[whole] type's value in place: every valid value has all of it - its
/// fields, then @tail, from where the struct has it, or the bytes past them.
/// Its Shape, its methods, its doc and X<S>'s.
fn whole_in_place<'a>(def: &'a StructDef, written: &Ident, tail: Option<(Ident, syn::Type)>)
    -> (Shape<'a>, TokenStream, String, String)
{
    let name = &def.ident;
    let (fields, tail_name, elem, tail_at, doc) = match tail {
        Some((t, e)) => {
            let at = def.fields.iter().position(|f| f.ident == t).unwrap_or(def.fields.len());
            let doc = format!(" {name} in place: its fields - those that hold a count private -\n \
                               then {t}, from where the struct has it, as long as the value is. It\n \
                               derefs to the struct, to read.");
            (&def.fields[..at], t.clone(), e, Some(quote!(::core::mem::offset_of!(#written, #t))), doc)
        }
        None => {
            let doc = format!(" {name} in place: its fields - those that hold a count private - then\n \
                               the bytes past them there are, written by a version since, at\n \
                               exactly the value's length. It derefs to the struct, to read.");
            (&def.fields[..], format_ident!("_rest"), parse_quote!(u8), None, doc)
        }
    };
    let shape = Shape { repr: repr_attrs(def), fields, private: Vec::new(), head: written.clone(),
                        deref: name.clone(), tail: tail_name, elem, tail_at };
    let generic_doc = format!(" {name}, in state S - Whole unless named: the struct, every field a field.\n \
                               Validated or Unvalidated, a value in place - all its fields, then\n \
                               the bytes past them a version since wrote, at the value's length.");
    (shape, TokenStream::new(), doc, generic_doc)
}

/// A #[grown] type's floor, @fl: @floor, the fields before @from, at
/// @written's offsets.
fn floor_struct(name: &Ident, fl: &Ident, written: &Ident, floor: &[FieldDef], from: &Ident) -> TokenStream {
    let floor_fields = floor.iter().map(|f| {
        let (field, fty) = (&f.ident, &f.ty);
        quote!(pub #field: #fty,)
    });
    let floor_tys: Vec<&syn::Type> = floor.iter().map(|f| &f.ty).collect();
    let floor_offsets = floor.iter().map(|f| {
        let field = &f.ident;
        quote!(assert!(::core::mem::offset_of!(#fl, #field) == ::core::mem::offset_of!(#written, #field));)
    });
    let floor_doc = format!(" {name}'s floor: the fields every valid one has - all it had before\n \
                             it grew, from {from} on.");
    let floor_msg = format!("{name}: its floor ends where {from} starts");
    let mut out = quote! {
        #[doc = #floor_doc]
        #[repr(C)]
        #[derive(Clone, Copy)]
        #[allow(non_camel_case_types)]
        pub struct #fl {
            #(#floor_fields)*
        }
    };
    out.extend(plain(fl, &floor_tys));
    out.extend(quote! {
        const _: () = {
            #(#floor_offsets)*
            assert!(::core::mem::size_of::<#fl>() == ::core::mem::offset_of!(#written, #from), #floor_msg);
        };
    });
    out
}

/// A #[grown] type's value in place, @ty: @floor's fields, then the bytes
/// there are - @later's fields getters, zero past the end. Its Shape, its
/// methods, its doc and X<S>'s.
fn grown_in_place<'a>(name: &Ident, fl: &Ident, ty: &Ident, written: &Ident, floor: &'a [FieldDef],
                      later: &[FieldDef])
    -> Result<(Shape<'a>, TokenStream, String, String), String>
{
    let vs = vs();
    const METHODS: &[&str] = &["whole", "whole_mut", "as_bytes", "as_bytes_mut"];
    let mut getters = TokenStream::new();
    for f in later {
        let (field, fty) = (&f.ident, &f.ty);
        if METHODS.iter().any(|m| field == m) {
            return Err(format!("{name}.{field}: {ty}'s own method is named {field} - its getter \
                                would be hidden"));
        }
        let doc = format!(" {field}, or zero where the value ends before it: one written\n before {name} had it.");
        getters.extend(quote! {
            #[doc = #doc]
            pub fn #field(&self) -> #fty {
                #vs::read_past::<#fty>(&self._rest,
                    ::core::mem::offset_of!(#written, #field) - ::core::mem::size_of::<#fl>())
            }
        });
    }

    let methods = quote! {
        impl #ty {
            /// All of it, if it's all there - every field a field.
            pub fn whole(&self) -> Option<&#name> {
                // SAFETY: X is #[repr(transparent)] over X_whole
                #vs::cast_plain::<#written>(self.as_bytes())
                    .map(|w| unsafe { &*(w as *const #written as *const #name) })
            }

            /// whole(), to edit.
            pub fn whole_mut(&mut self) -> Option<&mut #name> {
                // SAFETY: as whole()'s
                #vs::cast_plain_mut::<#written>(self.as_bytes_mut())
                    .map(|w| unsafe { &mut *(w as *mut #written as *mut #name) })
            }

            #getters
        }
    };

    let dst_doc = format!(" A {name} as it was written, by any version since it had its floor:\n \
                           the floor's fields, those that hold a count private, then the\n \
                           bytes there are, at exactly the value's length. It derefs to the\n \
                           floor, to read. A field it gained later is a getter, zero where\n \
                           the value ends before it; whole() is all of it, if it's all there.");
    let generic_doc = format!(" {name}, in state S - Whole unless named: the struct, every field a field.\n \
                               Validated or Unvalidated, a value in place, from any version since\n \
                               it had its floor: the floor's fields, then the bytes there are; a\n \
                               field it gained later is a getter, zero where the value ends.");
    let shape = Shape { repr: quote!(#[repr(C)]), fields: floor, private: Vec::new(), head: fl.clone(),
                        deref: fl.clone(), tail: format_ident!("_rest"), elem: parse_quote!(u8), tail_at: None };
    Ok((shape, methods, dst_doc, generic_doc))
}

/// The traits @def derives.
fn derive_names(def: &StructDef) -> Vec<Ident> {
    def.attrs.iter()
        .filter(|a| a.path().is_ident("derive"))
        .filter_map(|a| a.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated).ok())
        .flatten()
        .filter_map(|p| p.segments.last().map(|s| s.ident.clone()))
        .collect()
}

/// The derives vstruct_family! gives X<Whole> as the struct has them.
const WHOLE_DERIVES: &[&str] = &["Clone", "Copy", "Default", "Plain", "TypeInfo"];

/// @item, a struct, as @to - with #[type_name], its own name, before its
/// other attributes, so it expands before the derives among them.
fn rename_struct(item: TokenStream, def: &StructDef, to: &Ident) -> Result<TokenStream, String> {
    let name = &def.ident;
    let mut tts: Vec<TokenTree> = item.into_iter().collect();
    let at = tts.windows(2)
        .position(|w| matches!((&w[0], &w[1]), (TokenTree::Ident(s), TokenTree::Ident(n))
                               if s == "struct" && n == name))
        .ok_or_else(|| format!("{name}: a family is made from a struct"))?;
    let mut to = to.clone();
    to.set_span(tts[at + 1].span());
    tts[at + 1] = TokenTree::Ident(to);
    let mut out = TokenStream::new();
    if def.type_name.is_none() {
        out.extend(quote!(#[::cstruct_macros::type_name(#name)]));
    }
    out.extend(tts);
    Ok(out)
}

/// #[derive(FlexArray)], from #[counted_by(FIELD)] on the trailing `[T; 0]`.
pub(crate) fn flex_array(def: &StructDef) -> Result<TokenStream, String> {
    let name = &def.ident;
    let Some((i, f, count)) = def.fields.iter().enumerate().find_map(|(i, f)| Some((i, f, f.counted_by.as_ref()?)))
    else {
        return Err(format!("{name}: no #[counted_by(FIELD)] field"));
    };
    let elem = match flex_elem(&f.ty) {
        Some(elem) if i + 1 == def.fields.len() => elem,
        _ => return Err(format!("{name}.{}: #[counted_by] is for the last field, a [T; 0]", f.ident)),
    };
    if !def.fields.iter().any(|g| g.ident == *count) {
        return Err(format!("{name}.{}: #[counted_by({count})]: {name} has no field {count}", f.ident));
    }
    let field = &f.ident;
    Ok(flex_impl(name, &quote!(::core::mem::offset_of!(#name, #field)), elem, std::slice::from_ref(count)))
}

/// FlexArray for @name: @elem from @tail_at, as many as @count, a field's
/// path, says.
fn flex_impl(name: &Ident, tail_at: &TokenStream, elem: &syn::Type, count: &[Ident]) -> TokenStream {
    let vs = vs();
    // the count by value: a reference to a packed struct's field may be
    // unaligned
    quote! {
        impl #vs::FlexArray for #name {
            type Elem = #elem;
            const TAIL: usize = #tail_at;
            #[inline]
            fn nr(&self) -> usize { #vs::Count::count(&{ self.#(#count).* }) }
        }
    }
}
