// SPDX-License-Identifier: GPL-2.0
//! What util/vstructs.rs's traits need of a type C shares, from its
//! definition: #[derive(Plain)] - and #[bitfield], every one of which is -
//! #[derive(FlexArray)], from #[counted_by(FIELD)], #[derive(View)], its
//! fields' projections on a View of it, and the families of definitions,
//! one per state: #[unsized_vstruct], an unsized vstruct's; #[grown], a type
//! that grew between format versions.
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

fn vs() -> TokenStream {
    quote!(crate::util::vstructs)
}

/// #[derive(Plain)], on a struct - not a union, whose bytes past a short
/// arm's end are no field's. An unsized vstruct's states aren't: they're its
/// C definition's - #[unsized_vstruct]'s X__head - and then its tail's
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
/// make it `v.field`. Not for fields named `_..`, padding and C's internals,
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

// ---- Families: a type's definitions, one per state (util/vstructs.rs) ----

/// A family attribute's arguments: `#[counted(PATH, tail = ..)]`,
/// `#[unsized_vstruct(tail = ..)]`, `#[grown(from = FIELD)]`; `bkey`, for a
/// key's value.
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
}

/// @attr, a family attribute's arguments: an error for one it doesn't take -
/// @takes, of "count", "tail", "from" and "bkey" - or one not written as its
/// kind is. The caller names the attribute and the struct.
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

/// A field of C's struct as a state's definition has it, inline: pub only
/// where C's is pub and holds no count - not the count, written private or
/// pub(crate), nor an embedded header, Head<..> or an array of them, whose
/// own count it is, nor one of @private, which the family names: a counted
/// type's count. Those read through the definition's Deref to its head, and
/// can't be written: a count changes only through what holds the value.
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

/// The item's #[repr]s, as written.
fn repr_attrs(def: &StructDef) -> TokenStream {
    let repr = def.attrs.iter().filter(|a| a.path().is_ident("repr"));
    quote!(#(#repr)*)
}

/// What a state's definition is laid out as, for dst().
struct Shape<'a> {
    /// The item's #[repr]s, as written.
    repr:    TokenStream,
    /// C's fields it has inline, at @head's offsets.
    fields:  &'a [FieldDef],
    /// Those the family makes private: a counted type's count.
    private: Vec<Ident>,
    /// What precedes the tail: C's struct, or a grown type's floor.
    head:    Ident,
    /// What it derefs to, #[repr(transparent)] over @head if not @head
    /// itself: a #[whole] type's X<Whole>, where X's methods are.
    deref:   Ident,
    /// After the fields: @tail, a slice of @elem.
    tail:    Ident,
    elem:    syn::Type,
    /// C's struct: what as_ffi() points at.
    ffi:     Ident,
    /// Where the tail starts, if not where the head ends: inside it, the
    /// head being C's struct rounded up into the tail's first bytes - a key
    /// value's (#[whole(tail = ..)]).
    tail_at: Option<TokenStream>,
}

/// A state's definition, @ty, laid out as @shape: a DST - its fields inline,
/// those that hold a count private (inline_field()), then its tail.
///
/// Its fields are its head's, at the head's offsets - asserted - so it derefs
/// to the head: reads, and the head's methods, written once there. No
/// DerefMut: a &mut to the head would let a count - its own, or an embedded
/// header's - be rewritten, or the whole head assigned or swapped, from
/// outside what holds the value. Its own fields that hold one are private,
/// and it's unsized, so it can't be assigned or swapped whole either.
///
/// It's Unsized: from_raw() at the length's worth of elements past the head;
/// SelfSized, and its bytes, by its reference's length; and an arm's,
/// ArmRef, cast() from the bytes its storage says are its. @doc is its own;
/// @methods, more of them.
fn dst(ty: &Ident, doc: &str, shape: &Shape<'_>, methods: TokenStream) -> TokenStream {
    let vs = vs();
    let Shape { repr, fields, private, head, deref, tail, elem, ffi: _, tail_at } = shape;
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
        None => quote!(0),
    };
    let tail_msg = format!("{ty}: its tail starts where its fields end, at TAIL");
    quote! {
        #[doc = #doc]
        #repr
        #[allow(non_camel_case_types)]
        pub struct #ty {
            #(#inline)*
            pub #tail: [#elem],
        }

        impl ::core::ops::Deref for #ty {
            type Target = #deref;
            #[inline]
            fn deref(&self) -> &#deref {
                // SAFETY: its first bytes, as many as the head has, are the head's
                // fields, at the head's offsets - asserted below, each Plain - and
                // any of its tail they reach: it's no shorter than the head, as
                // Unsized says; the target is the head, or #[repr(transparent)]
                // over it
                unsafe { &*(self as *const Self).cast::<#deref>() }
            }
        }

        // SAFETY: its head's fields, each Plain, then its tail at TAIL - as the
        // asserts below check - Plain; from_raw() makes the tail as long as
        // @bytes past TAIL
        unsafe impl #vs::Unsized for #ty {
            type Head = #head;
            type Elem = #elem;
            const TAIL: usize = #tail_at;
            #[inline]
            fn from_raw(p: *const u8, bytes: usize) -> *const Self {
                let n = (bytes - Self::TAIL) / ::core::mem::size_of::<#elem>();
                ::core::ptr::slice_from_raw_parts(p.cast::<#elem>(), n) as *const Self
            }
            #[inline]
            fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {
                let n = (bytes - Self::TAIL) / ::core::mem::size_of::<#elem>();
                ::core::ptr::slice_from_raw_parts_mut(p.cast::<#elem>(), n) as *mut Self
            }
        }

        const _: () = {
            #vs::assert_plain::<#elem>();
            #(#plain_fields)*
            #(#offsets)*
            // its tail starts at TAIL - where its head ends, or inside it:
            // from_raw()'s length counts on it
            assert!((#tail_start).next_multiple_of(::core::mem::align_of::<#elem>())
                        == <#ty as #vs::Unsized>::TAIL,
                    #tail_msg);
            assert!(<#ty as #vs::Unsized>::TAIL <= ::core::mem::size_of::<#head>());
        };

        impl #vs::SelfSized for #ty {
            #[inline]
            fn bytes(&self) -> usize {
                ::core::mem::size_of_val(self)
            }
        }

        // as a tagged union's arm, over the bytes its storage says are its
        impl #vs::ArmRef for #ty {
            const MIN_LEN: usize = ::core::mem::size_of::<<Self as #vs::Unsized>::Head>();

            fn arm_ref(b: &[u8]) -> Option<&Self> {
                #vs::cast(b)
            }

            fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
                #vs::cast_mut(b)
            }

            fn whole_len(len: usize) -> Option<usize> {
                #vs::whole_len::<Self>(len)
            }
        }

        impl #ty {
            /// Its bytes, every one.
            pub fn as_bytes(&self) -> &[u8] {
                #vs::bytes_of(self)
            }

            /// Its bytes, to edit: for its own walks - a count may be in them.
            pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
                #vs::bytes_of_mut(self)
            }

            #methods
        }
    }
}

/// @name's family, @name__family - its Unvalidated, Validated and Whole
/// definitions. Every family has @name__unvalidated and @name__validated: a
/// plain alias where a state shares another's definition, so a container's
/// ArmOf impls can name each state's - concrete types, not projections.
///
/// And __@name__state_impls!{ items }: the items, impl'd on each of
/// @with_fields, the definitions with C's fields inline - for c_bitmask!'s
/// setters, which the Deref to C's struct can't reach: there's no DerefMut.
fn family(name: &Ident, unvalidated: &Ident, validated: &Ident, whole: &Ident, with_fields: &[Ident])
    -> TokenStream
{
    let vs = vs();
    let fam = format_ident!("{name}__family");
    let aliases = [("unvalidated", unvalidated), ("validated", validated)].into_iter()
        .map(|(st, def)| (format_ident!("{name}__{st}"), def))
        .filter(|(alias, def)| alias != *def)
        .map(|(alias, def)| quote! {
            #[allow(non_camel_case_types)]
            pub type #alias = #def;
        });
    let state_impls = format_ident!("__{name}__state_impls");
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

        #[doc(hidden)]
        #[allow(unused_macros)]
        macro_rules! #state_impls {
            ($($items:tt)*) => {
                #(impl #with_fields { $($items)* })*
            };
        }
    }
}

/// The methods of a state's definition, of a family whose count is its own -
/// an unsized vstruct's, a counted type's: handed back to C, @ffi. What
/// makes one is X<S>'s (generic_type()).
fn state_methods(ffi: &Ident) -> TokenStream {
    quote! {
        /// For C: a pointer that reaches all of it.
        pub fn as_ffi(&self) -> *const #ffi {
            (self as *const Self).cast()
        }

        /// For C, to edit: a pointer that reaches all of it.
        pub fn as_ffi_mut(&mut self) -> *mut #ffi {
            (self as *mut Self).cast()
        }
    }
}

/// The family of a type whose count is its own - an unsized vstruct's, a
/// #[counted] type's: both states laid out as @shape - Unvalidated, bytes
/// from outside, its count checked against them; Validated, its count
/// trusted as C trusts it - and Validated its Whole, the header never having
/// grown.
fn own_count_family(name: &Ident, shape: &Shape<'_>) -> TokenStream {
    let vs = vs();
    let state = |st: &str, what: &str| -> TokenStream {
        let ty = format_ident!("{name}__{}", st.to_lowercase());
        let doc = format!(" {name}<{st}>: {what}. C's fields - those that hold a count\n \
                           private - then its tail, as long as its count says. It derefs\n \
                           to C's struct {name}, Head<{name}>, to read.");
        let mut out = dst(&ty, &doc, shape, state_methods(&shape.ffi));
        out.extend(quote! {
            // a sized tagged union's arm: as many bytes as its count says
            impl #vs::ArmAt for #ty {
                fn arm_at(b: &[u8]) -> Result<&Self, #vs::VStructError> {
                    #vs::from_bytes::<Self>(b)
                }
            }
        });
        out
    };
    let (unvalidated, validated) = (format_ident!("{name}__unvalidated"), format_ident!("{name}__validated"));
    let unsized_states = [("Unvalidated", unvalidated.clone()), ("Validated", validated.clone()),
                          ("Whole", validated.clone())];
    let doc = format!(" {name}, in state S - Validated unless named: a reference to one\n \
                       reaches every byte of it and none past. C's struct {name} is\n \
                       Head<{name}>.");
    let mut out = state("Unvalidated", "bytes from outside - off a device, from the kernel - its count \
                                        checked against them when it's made");
    out.extend(state("Validated", "what C read and validated, its counts trusted as C trusts them"));
    out.extend(family(name, &unvalidated, &validated, &validated, &[unvalidated.clone(), validated.clone()]));
    out.extend(generic_type(name,
        &Generic { default: "Validated", unsized_states: &unsized_states, arm_of: None,
                   whole: None, ffi: Some(&shape.ffi), typestate: false },
        &doc));
    out
}

/// #[unsized_vstruct]: the family of an unsized vstruct's definitions, one
/// per state (util/vstructs.rs' Family), from C's struct as written.
///
/// The item is the definition, renamed X__head, with #[type_name(X)]: Head<X>
/// - X<S> is the family's type in state S,
/// Validated unless named (generic_type()). Each state's definition,
/// X__unvalidated and X__validated, is Rust's alone: a DST, the header and
/// then its tail, as long as its count says - which derefs to the header,
/// where its fields and methods are, written once. Its header never grew:
/// Validated is whole.
///
/// The tail is the item's trailing flexible array, `NAME: [E; 0]` - the
/// states' is `NAME: [E]` - or, where the header doesn't end in one, what
/// the attribute says: `tail = [E]`, the states' `_tail`, the raw rest - a
/// superblock field's, past its header; a btree node's, its bset's keys - or
/// `tail = NAME: [E]`, an array the states call NAME.
pub(crate) fn unsized_vstruct(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item)?;
    let name = &def.ident;
    let last = def.fields.last().ok_or_else(|| format!("{name}: an unsized vstruct has fields"))?;
    let args = family_args(attr, &["tail"]).map_err(|e| format!("{name}: {e}"))?;
    let (tail, elem) = match args.tail {
        None => match flex_elem(&last.ty) {
            Some(elem) => (last.ident.clone(), elem.clone()),
            None => return Err(format!("{name}: its last field, {}, isn't a flexible array, [E; 0] - \
                                        #[unsized_vstruct(tail = [E])] says what its tail holds",
                                       last.ident)),
        },
        Some((tail, elem)) => (tail.unwrap_or_else(|| format_ident!("_tail")), elem),
    };
    let ffi = format_ident!("{name}__head");
    let last = &last.ident;
    // C's fields, inline in each state's definition - but its flexible array,
    // where that's the tail
    let fields = if tail == *last { &def.fields[..def.fields.len() - 1] } else { &def.fields[..] };
    let shape = Shape { repr: repr_attrs(&def), fields, private: Vec::new(), head: ffi.clone(),
                        deref: ffi.clone(), tail, elem, ffi: ffi.clone(), tail_at: None };

    let mut out = rename_struct(item, &def, &ffi)?;
    let msg = format!("{name}: its tail starts where its header ends");
    out.extend(quote! {
        const _: () = {
            // C's struct ends at its last field - its flexible array, or an
            // embedded vstruct's header, which ends where that one's tail starts
            assert!(::core::mem::offset_of!(#ffi, #last) + crate::types::field_size(
                        |p: *const #ffi| unsafe { ::core::ptr::addr_of!((*p).#last) }) as usize
                    == ::core::mem::size_of::<#ffi>(),
                    #msg);
        };
    });
    out.extend(own_count_family(name, &shape));
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
/// definition; C's struct keeps it as written, so a header can still be
/// built standalone. The array is the item's trailing [E; 0], or what the
/// attribute says - as #[unsized_vstruct]'s, `tail = [E]`, the states'
/// `_tail`, or `tail = NAME: [E]`, a field of a field the states call NAME.
///
/// C's struct gets FlexArray from the count - its size, TAIL and the
/// elements past it - and CountedHead, the way to its Unvalidated definition
/// from what holds its bytes.
pub(crate) fn counted(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let vs = vs();
    let def = parse_cstruct(&item)?;
    let name = &def.ident;
    let ffi = format_ident!("{name}__head");

    let args = family_args(attr, &["count", "tail"]).map_err(|e| format!("{name}: {e}"))?;
    let count = args.count.unwrap_or_else(|| vec![format_ident!("nr_entries")]);
    let count_field = &count[0];
    if !def.fields.iter().any(|f| f.ident == *count_field) {
        let count = quote!(#(#count).*);
        return Err(format!("{name}: #[counted({count})], but {name} has no field {count_field} - \
                            name its count, #[counted(FIELD)] or #[counted(FIELD.COUNT)]"));
    }

    // the array: a trailing [E; 0], or a field of a field - after C's struct,
    // where dst() asserts its tail starts
    let (tail, elem, fields, tail_at) = match args.tail {
        Some((tail, elem)) => (tail.unwrap_or_else(|| format_ident!("_tail")), elem, &def.fields[..],
                               quote!(::core::mem::size_of::<#ffi>())),
        None => {
            let last = def.fields.last().ok_or_else(|| format!("{name}: a counted type has fields"))?;
            let Some(elem) = flex_elem(&last.ty) else {
                return Err(format!("{name}: its last field, {}, isn't a flexible array, [E; 0] - \
                                    #[counted(.., tail = [E])] says where its array is", last.ident));
            };
            let field = &last.ident;
            (field.clone(), elem.clone(), &def.fields[..def.fields.len() - 1],
             quote!(::core::mem::offset_of!(#ffi, #field)))
        }
    };
    let shape = Shape { repr: repr_attrs(&def), fields, private: vec![count_field.clone()], head: ffi.clone(),
                        deref: ffi.clone(), tail, elem: elem.clone(), ffi: ffi.clone(), tail_at: None };

    let mut out = rename_struct(item, &def, &ffi)?;
    let msg = format!("{name}: its elements aren't a whole number of its alignment");
    out.extend(flex_impl(&ffi, &tail_at, &elem, &count));
    out.extend(quote! {
        impl #vs::CountedHead for #ffi {
            type Of<S: #vs::State> = #name<S>;
        }

        // a reference as long as its count says is a whole number of its
        // alignment only if its elements are: else it claims bytes past it
        const _: () = assert!(::core::mem::size_of::<#elem>() % ::core::mem::align_of::<#ffi>() == 0, #msg);
    });
    out.extend(own_count_family(name, &shape));
    Ok(out)
}

/// #[grown(from = FIELD)]: the family of a type that grew between format
/// versions, from C's struct as written - FIELD the first field a valid
/// value may lack, written by a version from before it.
///
/// X<S> is a generic struct over its definition in state S. X<Whole> - X -
/// is the whole struct, every field a field: the item, renamed X__whole with
/// #[type_name(X)]. X__floor is the fields before FIELD, every valid
/// value's. X<Validated> - Unvalidated too, as the cast checks the floor - is
/// a value in place, a DST: the floor's fields, then the bytes there are, at
/// exactly the value's length; it derefs to the floor, to read. A field from
/// FIELD on is a getter, zero past the end, and whole() is all of it, X, if
/// it's all there.
pub(crate) fn grown(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item)?;
    let name = def.name();
    let args = family_args(attr, &["from", "bkey"]).map_err(|e| format!("{name}: {e}"))?;
    let from = args.from.as_ref()
        .ok_or_else(|| format!("{name}: #[grown(from = FIELD)], FIELD the first field an older \
                                version's value may lack"))?;
    let at = def.fields.iter().position(|f| f.ident == *from)
        .ok_or_else(|| format!("{name}: #[grown(from = {from})], but {name} has no field {from}"))?;
    if at == 0 {
        return Err(format!("{name}: #[grown(from = {from})]: every valid value has at least one field"));
    }
    sized_family(&def, item, Some((from, at)), None, args.bkey)
}

/// #[whole]: the family of a type every valid value of which has all of it -
/// C's validate rejects one shorter - though it may be longer, written by a
/// version since: a superblock section or journal entry of fixed fields.
///
/// X<S> is a generic struct over its definition in state S. X<Whole> - X -
/// is the struct: the item, renamed X__whole with #[type_name(X)].
/// X<Validated> and X<Unvalidated> are a value in place, a DST: X's fields,
/// those that hold a count private, then the bytes past them there are, at
/// exactly the value's length; it derefs to X, to read, and X's methods.
///
/// Or `#[whole(tail = NAME: [E])]`: C's struct ends in NAME: [E; 0] - and
/// perhaps other zero-length arrays, C's other names for where it starts - a
/// header and the array after it: a key value, its length the key's. The
/// tail is NAME, from where C has it: inside C's struct, if align rounds the
/// struct up into the array's first bytes - bch_inode's fields end at 14, its
/// size is 16 - which the deref to C's struct reaches, a value being no
/// shorter than that.
///
/// Or `tail = NAME: [u8]`, over any zero-length array: the bytes from where
/// it starts, for a tail that's more than that array - bch_stripe's
/// pointers, then checksums and sector counts its header sizes.
pub(crate) fn whole(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let def = parse_cstruct(&item)?;
    let name = def.name();
    let args = family_args(attr, &["tail", "bkey"]).map_err(|e| format!("{name}: {e}"))?;
    let tail = match args.tail {
        None => None,
        Some((None, _)) => return Err(format!("{name}: #[whole(tail = NAME: [E])] - the tail is C's last \
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
            // after it, only C's other names for where it starts
            for f in &def.fields[at + 1..] {
                if flex_elem(&f.ty).is_none() {
                    return Err(format!("{name}.{}: after the tail, {tail}, a field that isn't a \
                                        zero-length array - C's other name for its start", f.ident));
                }
            }
            Some((tail, elem))
        }
    };
    sized_family(&def, item, None, tail, args.bkey)
}

/// #[grown] and #[whole]: the family of sized C struct @def, written @item -
/// @grew, the first field an older version's value may lack and its index,
/// if it grew.
///
/// Both have a definition of all of it, every field inline: X__whole, if it
/// grew - its Whole; else X__validated, its every state. Only that one is
/// handed to C, as_ffi(): it's the one sure to hold all of C's struct.
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
    // The struct as written, renamed X__whole: X<Whole>'s representation,
    // and C's struct X
    let ffi = format_ident!("{name}__whole");
    // A value in place, as long as what holds it says: for a grown type the
    // floor and the bytes there are, below; else all of its fields, then
    // the bytes past them
    let all = format_ident!("{name}__validated");
    // in place, the states but Whole: a DST
    let states = [("Validated", all.clone()), ("Unvalidated", all.clone())];
    let derives = derive_names(def);
    // X is generated: a derive generic_type() doesn't forward would be
    // X__whole's alone, out of X's reach. Ours of the struct as written are
    // X__whole's, and are what C's struct is
    let of_whole = ["MaybeZeroable", "BitfieldMembers", "FlexArray"];
    if let Some(d) = derives.iter()
        .find(|d| !of_whole.iter().chain(WHOLE_DERIVES).any(|w| d == w)) {
        return Err(format!("{name}: #[derive({d})], but {name} - {name}<Whole> - is generated, and has \
                            only the derives generic_type() forwards to it: {}", WHOLE_DERIVES.join(", ")));
    }
    let is_plain = derives.iter().any(|d| d == "Plain");
    // the tail, C's last field - or the bytes past C's fields
    let (fields, tail_name, elem, tail_at) = match &tail {
        Some((t, e)) => {
            let at = def.fields.iter().position(|f| f.ident == *t).unwrap_or(def.fields.len());
            (&def.fields[..at], t.clone(), e.clone(), Some(quote!(::core::mem::offset_of!(#ffi, #t))))
        }
        None => (&def.fields[..], format_ident!("_rest"), parse_quote!(u8), None),
    };
    let Some((from, at)) = grew else {
        // Every valid value has all of it: in place, all its fields and the
        // bytes past them, in every state but Whole, the struct itself
        let doc = match &tail {
            Some((t, _)) => format!(" {name} in place: its fields - those that hold a count private -\n \
                                     then {t}, from where C has it, as long as the value is. It\n \
                                     derefs to the struct, to read."),
            None => format!(" {name} in place: its fields - those that hold a count private - then\n \
                             the bytes past them there are, written by a version since, at\n \
                             exactly the value's length. It derefs to the struct, to read."),
        };
        let shape = Shape { repr: repr_attrs(def), fields, private: Vec::new(), head: ffi.clone(),
                            deref: name.clone(), tail: tail_name, elem, ffi: ffi.clone(), tail_at };
        let generic_doc = format!(" {name}, in state S - Whole unless named: the struct, every field a field.\n \
                                   Validated or Unvalidated, a value in place - all its fields, then\n \
                                   the bytes past them a version since wrote, at the value's length.");
        let mut out = rename_struct(item, def, &ffi)?;
        out.extend(dst(&all, &doc, &shape, quote! {
            /// For C: a pointer to all of it.
            pub fn as_ffi(&self) -> *const #ffi {
                (self as *const Self).cast()
            }

            /// For C, to edit: a pointer to all of it.
            pub fn as_ffi_mut(&mut self) -> *mut #ffi {
                (self as *mut Self).cast()
            }
        }));
        out.extend(family(name, &all, &all, &ffi, &[]));
        out.extend(whole_arm_ref(&ffi, is_plain));
        out.extend(generic_type(name, &sized_generic(&all, &ffi, &derives, &states, false), &generic_doc));
        out.extend(bkey_impls);
        return Ok(out);
    };
    if !is_plain {
        return Err(format!("{name}: #[grown] without Plain - the struct is a value in place, every \
                            byte of it read, so it can't have padding"));
    }
    let (floor, later) = def.fields.split_at(at);

    let fl = format_ident!("{name}__floor");
    let ty = format_ident!("{name}__validated");

    let floor_fields = floor.iter().map(|f| {
        let (field, fty) = (&f.ident, &f.ty);
        quote!(pub #field: #fty,)
    });
    let floor_tys: Vec<&syn::Type> = floor.iter().map(|f| &f.ty).collect();
    let floor_offsets = floor.iter().map(|f| {
        let field = &f.ident;
        quote!(assert!(::core::mem::offset_of!(#fl, #field) == ::core::mem::offset_of!(#ffi, #field));)
    });

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
                    ::core::mem::offset_of!(#ffi, #field) - ::core::mem::size_of::<#fl>())
            }
        });
    }

    let methods = quote! {
        /// All of it, if it's all there - every field a field.
        pub fn whole(&self) -> Option<&#name> {
            // SAFETY: X is #[repr(transparent)] over X__whole
            #vs::cast_plain::<#ffi>(self.as_bytes())
                .map(|w| unsafe { &*(w as *const #ffi as *const #name) })
        }

        /// whole(), to edit.
        pub fn whole_mut(&mut self) -> Option<&mut #name> {
            // SAFETY: as whole()'s
            #vs::cast_plain_mut::<#ffi>(self.as_bytes_mut())
                .map(|w| unsafe { &mut *(w as *mut #ffi as *mut #name) })
        }

        #getters
    };

    let floor_doc = format!(" {name}'s floor: the fields every valid one has - all it had before\n \
                             it grew, from {from} on.");
    let floor_msg = format!("{name}: its floor ends where {from} starts");
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
                        deref: fl.clone(), tail: format_ident!("_rest"), elem: parse_quote!(u8), ffi: ffi.clone(),
                        tail_at: None };

    let mut out = rename_struct(item, def, &ffi)?;
    out.extend(quote! {
        #[doc = #floor_doc]
        #[repr(C)]
        #[derive(Clone, Copy)]
        #[allow(non_camel_case_types)]
        pub struct #fl {
            #(#floor_fields)*
        }
    });
    out.extend(plain(&fl, &floor_tys));
    out.extend(quote! {
        const _: () = {
            #(#floor_offsets)*
            assert!(::core::mem::size_of::<#fl>() == ::core::mem::offset_of!(#ffi, #from), #floor_msg);
        };
    });
    out.extend(dst(&ty, &dst_doc, &shape, methods));
    // the cast checks the floor: an unvalidated one is as good
    out.extend(family(name, &ty, &ty, &ffi, &[]));
    out.extend(whole_arm_ref(&ffi, is_plain));
    out.extend(generic_type(name, &sized_generic(&ty, &ffi, &derives, &states, true), &generic_doc));
    // all of it is a value in place, every field there: X's methods, on
    // X<Validated>, are the struct's too
    out.extend(quote! {
        impl ::core::ops::Deref for #ffi {
            type Target = #name<#vs::Validated>;
            #[inline]
            fn deref(&self) -> &Self::Target {
                // SAFETY: all of its bytes - Plain, each a field's - which a value in
                // place is as long as
                unsafe { &*<Self::Target as #vs::Unsized>::from_raw(
                    (self as *const Self).cast(), ::core::mem::size_of::<Self>()) }
            }
        }

        impl ::core::ops::DerefMut for #ffi {
            #[inline]
            fn deref_mut(&mut self) -> &mut Self::Target {
                // SAFETY: as deref()'s
                unsafe { &mut *<Self::Target as #vs::Unsized>::from_raw_mut(
                    (self as *mut Self).cast(), ::core::mem::size_of::<Self>()) }
            }
        }
    });
    out.extend(bkey_impls);
    Ok(out)
}

/// A sized type's X<S>: Whole unless named, the struct @whole, with the value
/// traits @derives names; in place, @inplace, an arm wherever that is - and
/// if @typestate, Unvalidated a typestate over Validated.
fn sized_generic<'a>(inplace: &'a Ident, whole: &'a Ident, derives: &'a [Ident],
                     unsized_states: &'a [(&'a str, Ident)], typestate: bool) -> Generic<'a> {
    Generic { default: "Whole", unsized_states, arm_of: Some(inplace), whole: Some((whole, derives)),
              ffi: None, typestate }
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

/// @whole, a sized struct, from the bytes its storage says are its: all of
/// it, aligned - a value longer than it, written by a version since, is
/// still all of it. To edit, only if it's Plain (@plain): one with padding -
/// a key value's header, rounded up into its tail - would write its padding
/// over the tail.
fn whole_arm_ref(whole: &Ident, plain: bool) -> TokenStream {
    let vs = vs();
    let (get, get_mut) = if plain {
        (quote!(#vs::cast_plain(b)), quote!(#vs::cast_plain_mut(b)))
    } else {
        (quote! {
            // SAFETY: that many bytes, aligned, and its fields - each Plain, as its
            // value in place asserts - any bytes are; its padding is never read
            (b.len() >= ::core::mem::size_of::<Self>() &&
             b.as_ptr() as usize % ::core::mem::align_of::<Self>() == 0)
                .then(|| unsafe { &*b.as_ptr().cast::<Self>() })
        }, quote! {
            let _ = b;
            None
        })
    };
    quote! {
        impl #vs::ArmRef for #whole {
            const MIN_LEN: usize = ::core::mem::size_of::<Self>();

            fn arm_ref(b: &[u8]) -> Option<&Self> {
                #get
            }

            fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
                #get_mut
            }
        }
    }
}

/// The derives generic_type() gives X<Whole> as the struct has them.
const WHOLE_DERIVES: &[&str] = &["Clone", "Copy", "Default", "Plain", "TypeInfo"];

/// What a family's X<S> is made from: which states' definitions are DSTs,
/// what it's an arm of, and what it's made by.
struct Generic<'a> {
    /// The state X alone names: Whole for a sized type, the struct;
    /// Validated for an unsized one, whose Whole is its Validated.
    default: &'a str,
    /// The states whose definition is a DST, and each one's: these have
    /// Unsized and SelfSized as it does.
    unsized_states: &'a [(&'a str, Ident)],
    /// Whose ArmOf it has: a sized type's value in place, in every state -
    /// the struct, X<Whole>, is an arm wherever its value in place is - or,
    /// None, its definition in each state's: an unsized type's, as a
    /// container in that state has it.
    arm_of: Option<&'a Ident>,
    /// The sized struct, if Whole is one, and what it derives: X<Whole> has
    /// the value traits among them that it has.
    whole: Option<(&'a Ident, &'a [Ident])>,
    /// C's struct, if X is made from C's pointer or from bytes - an unsized
    /// type, whose count is its own: from_ffi() and from_bytes() on X<S>.
    ffi: Option<&'a Ident>,
    /// Whether Unvalidated is a typestate over Validated, X<Unvalidated>
    /// dereffing to X<Validated> - where X's methods are - not to their
    /// definition: a #[grown] type's, which is one for both, and whose length
    /// validate checks before anything reads it.
    typestate: bool,
}

/// @name<S>, documented @doc: a generic struct over its family's definition
/// in state S, as @g says. #[repr(transparent)], so a reference to one is a
/// reference to the other, and impls attach to @name<S> for the state
/// they're for. What generic code goes by - ArmOf, ArmRef, Unsized - it has
/// as its definition does.
fn generic_type(name: &Ident, g: &Generic<'_>, doc: &str) -> TokenStream {
    let vs = vs();
    let fam = format_ident!("{name}__family");
    let default = format_ident!("{}", g.default);
    let mut out = quote! {
        #[doc = #doc]
        #[repr(transparent)]
        #[allow(non_camel_case_types)]
        pub struct #name<S: #vs::State = #vs::#default> {
            _state:   ::core::marker::PhantomData<S>,
            /// Its definition in state S - for offset_of!(), repr.field.
            pub repr: <S as #vs::State>::Of<#fam>,
        }

        impl<S: #vs::State> #vs::InState for #name<S> {
            type In<T: #vs::State> = #name<T>;
            type State = S;
        }
    };

    // To its definition in S - or, a typestate, to X<Validated>
    let deref = |params: TokenStream, ty: TokenStream, target: TokenStream, get: TokenStream,
                 get_mut: TokenStream| quote! {
        impl #params ::core::ops::Deref for #ty {
            type Target = #target;
            #[inline]
            fn deref(&self) -> &Self::Target {
                #get
            }
        }

        impl #params ::core::ops::DerefMut for #ty {
            #[inline]
            fn deref_mut(&mut self) -> &mut Self::Target {
                #get_mut
            }
        }
    };
    if g.typestate {
        for st in ["Whole", "Validated"] {
            let st = format_ident!("{st}");
            out.extend(deref(quote!(), quote!(#name<#vs::#st>), quote!(<#vs::#st as #vs::State>::Of<#fam>),
                             quote!(&self.repr), quote!(&mut self.repr)));
        }
        out.extend(deref(quote!(), quote!(#name<#vs::Unvalidated>), quote!(#name<#vs::Validated>),
            quote! {
                // SAFETY: #[repr(transparent)], both, over the one definition
                unsafe { &*(self as *const Self as *const Self::Target) }
            },
            quote! {
                // SAFETY: as deref()'s
                unsafe { &mut *(self as *mut Self as *mut Self::Target) }
            }));
    } else {
        out.extend(deref(quote!(<S: #vs::State>), quote!(#name<S>), quote!(<S as #vs::State>::Of<#fam>),
                         quote!(&self.repr), quote!(&mut self.repr)));
    }

    // its definition in S
    let def_in_s = quote!(<S as #vs::State>::Of<#fam>);
    let arm_of_def = match g.arm_of {
        Some(def) => quote!(#def),
        None => def_in_s.clone(),
    };
    out.extend(quote! {
        // An arm of what its definition is an arm of: the same tag
        impl<U: ?Sized, S: #vs::State> crate::types::ArmOf<U> for #name<S>
        where
            #arm_of_def: crate::types::ArmOf<U>,
        {
            type Tag = <#arm_of_def as crate::types::ArmOf<U>>::Tag;
            const TAG: Self::Tag = <#arm_of_def as crate::types::ArmOf<U>>::TAG;
        }

        // In every state, as its definition in that state is had: the pointer
        // cast keeps the definition's metadata, its last field being it
        impl<S: #vs::State> #vs::ArmRef for #name<S> {
            const MIN_LEN: usize = <#def_in_s as #vs::ArmRef>::MIN_LEN;

            fn arm_ref(b: &[u8]) -> Option<&Self> {
                // SAFETY: #[repr(transparent)] over its definition in S
                <#def_in_s as #vs::ArmRef>::arm_ref(b)
                    .map(|d| unsafe { &*(d as *const #def_in_s as *const Self) })
            }

            fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
                // SAFETY: as arm_ref()'s
                <#def_in_s as #vs::ArmRef>::arm_mut(b)
                    .map(|d| unsafe { &mut *(d as *mut #def_in_s as *mut Self) })
            }

            fn whole_len(len: usize) -> Option<usize> {
                <#def_in_s as #vs::ArmRef>::whole_len(len)
            }
        }

        impl<S: #vs::State> #vs::ArmAt for #name<S>
        where
            #def_in_s: #vs::ArmAt,
        {
            fn arm_at(b: &[u8]) -> Result<&Self, #vs::VStructError> {
                // SAFETY: as arm_ref()'s
                <#def_in_s as #vs::ArmAt>::arm_at(b)
                    .map(|d| unsafe { &*(d as *const #def_in_s as *const Self) })
            }
        }
    });

    for (st, def) in g.unsized_states {
        let st_ty = format_ident!("{st}");
        out.extend(quote! {
            // SAFETY: #[repr(transparent)] over its definition, whose Unsized it is
            unsafe impl #vs::Unsized for #name<#vs::#st_ty> {
                type Head = <#def as #vs::Unsized>::Head;
                type Elem = <#def as #vs::Unsized>::Elem;
                const TAIL: usize = <#def as #vs::Unsized>::TAIL;
                #[inline]
                fn from_raw(p: *const u8, bytes: usize) -> *const Self {
                    <#def as #vs::Unsized>::from_raw(p, bytes) as *const Self
                }
                #[inline]
                fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {
                    <#def as #vs::Unsized>::from_raw_mut(p, bytes) as *mut Self
                }
            }

            impl #vs::SelfSized for #name<#vs::#st_ty> {
                #[inline]
                fn bytes(&self) -> usize {
                    ::core::mem::size_of_val(self)
                }
            }
        });

        // made in the state the caller names: Whole, an unsized type's
        // Validated, is never one made
        let Some(ffi) = g.ffi.filter(|_| *st != "Whole") else { continue };
        let from_bytes = match *st {
            "Unvalidated" => {
                let doc = format!(" The {name} at the start of @bytes - off a device, from the\n \
                                   kernel - as many as its count says: an error if they don't hold\n \
                                   its header, aligned, or that many.");
                quote! {
                    #[doc = #doc]
                    pub fn from_bytes(bytes: &[u8]) -> Result<&Self, #vs::VStructError> {
                        #vs::from_bytes::<Self>(bytes)
                    }

                    /// from_bytes(), mutably.
                    pub fn from_bytes_mut(bytes: &mut [u8]) -> Result<&mut Self, #vs::VStructError> {
                        #vs::from_bytes_mut::<Self>(bytes)
                    }

                    /// It, Validated - the unsafe way up: both states are laid out
                    /// alike, so it's the same bytes, at the same length.
                    ///
                    /// # Safety
                    /// What Validated says of it is true: validate passed.
                    pub unsafe fn assume_validated(&self) -> &#name<#vs::Validated> {
                        let p = <#name<#vs::Validated> as #vs::Unsized>::from_raw(
                            (self as *const Self).cast(), ::core::mem::size_of_val(self));
                        // SAFETY: its bytes, as long as they are; the caller's, that
                        // they're Validated
                        unsafe { &*p }
                    }

                    /// assume_validated(), mutably.
                    ///
                    /// # Safety
                    /// As assume_validated()'s.
                    pub unsafe fn assume_validated_mut(&mut self) -> &mut #name<#vs::Validated> {
                        let p = <#name<#vs::Validated> as #vs::Unsized>::from_raw_mut(
                            (self as *mut Self).cast(), ::core::mem::size_of_val(self));
                        // SAFETY: as assume_validated()'s
                        unsafe { &mut *p }
                    }
                }
            }
            "Validated" => quote! {
                /// It, Unvalidated - the safe way down: both states are laid out
                /// alike, and an Unvalidated one trusts nothing - for what reads
                /// one that may not be valid, a printer.
                pub fn as_unvalidated(&self) -> &#name<#vs::Unvalidated> {
                    let p = <#name<#vs::Unvalidated> as #vs::Unsized>::from_raw(
                        (self as *const Self).cast(), ::core::mem::size_of_val(self));
                    // SAFETY: its bytes, as long as they are
                    unsafe { &*p }
                }
            },
            _ => quote!(),
        };
        let from_ffi_doc = format!(" The {name} C's pointer @p is to, as long as its count says: not\n \
                                    widened from a reference to its header, which reaches only that.\n \
                                    In the state the caller names - {name}::<Validated>, for what C\n \
                                    read and validated - never one inferred.\n\n \
                                    # Safety\n \
                                    @p points to a {name} whose count is right, and what its state\n \
                                    says of its entries' is true - for Validated, that C checked\n \
                                    them; and those bytes stay valid, and unwritten, for 'a.");
        out.extend(quote! {
            impl #name<#vs::#st_ty> {
                #[doc = #from_ffi_doc]
                pub unsafe fn from_ffi<'a>(p: *const #ffi) -> &'a Self {
                    // SAFETY: the caller's
                    unsafe { #vs::from_ffi::<Self>(p) }
                }

                /// from_ffi(), mutably.
                ///
                /// # Safety
                /// As from_ffi()'s, and nothing else touches those bytes for 'a.
                pub unsafe fn from_ffi_mut<'a>(p: *mut #ffi) -> &'a mut Self {
                    // SAFETY: the caller's
                    unsafe { #vs::from_ffi_mut::<Self>(p) }
                }

                #from_bytes
            }
        });
    }

    let Some((whole, derives)) = g.whole else { return out };
    let whole_ty = quote!(#name<#vs::Whole>);

    // Sized: in place, its first size_of bytes - for what holds a value it
    // knows is whole, as a key in state Whole does, to make one from them
    out.extend(quote! {
        // SAFETY: a reference from it claims size_of::<Self>() of @bytes, the
        // first: the struct, every field Plain
        unsafe impl #vs::Unsized for #whole_ty {
            type Head = #whole;
            type Elem = u8;
            const TAIL: usize = ::core::mem::size_of::<Self>();
            #[inline]
            fn from_raw(p: *const u8, _bytes: usize) -> *const Self {
                p.cast()
            }
            #[inline]
            fn from_raw_mut(p: *mut u8, _bytes: usize) -> *mut Self {
                p.cast()
            }
        }
    });
    let has = |d: &str| derives.iter().any(|x| x == d);
    if has("Clone") {
        out.extend(quote! {
            impl ::core::clone::Clone for #whole_ty {
                fn clone(&self) -> Self {
                    #name { _state: ::core::marker::PhantomData, repr: self.repr.clone() }
                }
            }
        });
    }
    if has("Copy") {
        out.extend(quote!(impl ::core::marker::Copy for #whole_ty {}));
    }
    if has("Default") {
        out.extend(quote! {
            impl ::core::default::Default for #whole_ty {
                fn default() -> Self {
                    #name { _state: ::core::marker::PhantomData, repr: ::core::default::Default::default() }
                }
            }
        });
    }
    if has("Plain") {
        out.extend(quote! {
            // SAFETY: #[repr(transparent)] over the struct, which is Plain
            unsafe impl #vs::Plain for #whole_ty {}
        });
    }
    if has("TypeInfo") {
        out.extend(quote! {
            impl crate::typeinfo::TypeInfo for #whole_ty {
                const INFO: &'static crate::typeinfo::StructInfo =
                    <#whole as crate::typeinfo::TypeInfo>::INFO;
            }
        });
    }
    out
}

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
