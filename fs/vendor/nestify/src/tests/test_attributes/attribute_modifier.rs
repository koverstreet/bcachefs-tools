use syn::parse_str;
use quote::quote;
use crate::attributes::AttributeModifier;

#[test]
fn parse_star_modifier() {
    let input = quote!{*};
    let parsed = parse_str::<>(&input.to_string());

    assert!(matches!(parsed, Ok(AttributeModifier::Star)));
}

#[test]
fn parse_slash_modifier() {
    let input = quote!{/};
    let parsed = parse_str::<AttributeModifier>(&input.to_string());

    assert!(matches!(parsed, Ok(AttributeModifier::Slash)));
}

#[test]
fn parse_minus_modifier() {
    let input = quote!{-};
    let parsed = parse_str::<AttributeModifier>(&input.to_string());

    assert!(matches!(parsed, Ok(AttributeModifier::Minus)));
}
