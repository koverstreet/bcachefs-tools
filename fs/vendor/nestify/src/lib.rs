use crate::special_data::Special;
use crate::unpack::Unpack;
use crate::unpack_context::UnpackContext;
use syn::parse_macro_input;

#[cfg(test)]
mod tests;
pub(crate) mod attributes;
pub(crate) mod discriminant;
pub(crate) mod fish;
pub(crate) mod special_data;
pub(crate) mod ty;
pub(crate) mod unpack_context;

/// Provides functionality for unpacking special data structures.
///
/// This module defines traits and implementations for recursively unpacking
/// data structures annotated with custom attributes, facilitating a form of
/// metaprogramming within Rust macros.
pub(crate) mod unpack;
mod attribute_removal;

/// Allows for the expansion of "nested" items
///
/// # Learn
/// See [Guide](https://github.com/snowfoxsh/nestify)
///
/// # Examples
///
/// Define a user profile with nested address and preferences structures
/// ```
/// nestify::nest! {
///     struct UserProfile {
///         name: String,
///         address: struct Address {
///             street: String,
///             city: String,
///         },
///         preferences: struct Preferences {
///             newsletter: bool,
///         },
///     }
/// }
/// ```
///
/// Define a word with a nested view of its halves, as a union
/// ```
/// nestify::nest! {
///     #[repr(C)]
///     #[derive(Clone, Copy)]*
///     union Word {
///         raw: u64,
///         halves: struct Halves {
///             lo: u32,
///             hi: u32,
///         },
///     }
/// }
/// ```
///
/// Define a task with a nested status enum
/// ```
/// nestify::nest! {
///     struct Task {
///         id: i32,
///         description: String,
///         status: enum Status {
///             Pending,
///             InProgress,
///             Completed,
///         },
///     }
/// }
/// ```
#[proc_macro]
pub fn nest(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    if input.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "Nest! macro expansion failed: The input is empty.\n\
             note: The nest! macro expects a non-empty `struct`, `enum` or `union` definition to function properly.\n\
             help: Please ensure that you are using the nest! macro with a valid `struct`, `enum` or `union`. \
             Refer to documentation for information on how to use this macro and more examples",
        )
        .to_compile_error()
        .into();
    }

    let def = parse_macro_input!(input as Special);

    def.unpack(UnpackContext::default(), Vec::default(), None, false).into()
}
