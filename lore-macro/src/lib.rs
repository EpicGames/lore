// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod lore_args;
mod lore_command;
mod lore_instrument;
mod position_bitcode;
mod test_pub;
mod validate_text;
mod variant;
mod wire_schema;

use proc_macro::TokenStream;
use syn::DeriveInput;
use syn::parse_macro_input;

#[proc_macro_derive(VariantTypeSize)]
pub fn variant_type_size(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);

    variant::get_variant_type_sizes(&ast)
}

/// Derives bitcode's `Encode` and `Decode` for an enum whose variants each hold one value, as each
/// value's position among the variants followed by the value its variant holds, for fewer than
/// 2^32 variants, and `lore_base::wire_schema::WireSchema` for that encoding. The deriving crate
/// depends on `bitcode` and `lore-base`, and has a `test-util` feature.
#[proc_macro_derive(PositionBitcode)]
pub fn position_bitcode(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);

    position_bitcode::get_position_bitcode_impl(&ast)
}

#[proc_macro_derive(LoreArgs, attributes(handler, requires_service))]
pub fn lore_args(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);

    lore_args::get_lore_args_impl(&ast)
}

#[proc_macro_derive(ValidateText)]
pub fn validate_text(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);

    validate_text::get_validate_text_impl(&ast)
}

#[proc_macro_derive(LoreCommand)]
pub fn lore_command(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);

    lore_command::get_lore_command_impl(&ast)
}

/// Implements `lore_base::wire_schema::WireSchema` for a type that derives `bitcode::Encode` and
/// `bitcode::Decode`, so that a change to its encoding changes the schema a client and the service
/// compare.
///
/// The schema covers the order, names and schemas of the type's fields and variants, and leaves
/// out a field that `#[bitcode(skip)]` leaves out of the encoding. Each type parameter must
/// implement the trait. A type that holds itself, however indirectly, fails to compile, since its
/// schema would be defined by its own.
#[proc_macro_derive(WireSchema)]
pub fn wire_schema(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);

    wire_schema::get_wire_schema_impl(&ast)
}

#[proc_macro_attribute]
pub fn lore_instrument(args: TokenStream, item: TokenStream) -> TokenStream {
    lore_instrument::lore_instrument_impl(args, item)
}

/// Makes an item `pub` when its crate is built with the crate's own `test-util`
/// feature, for the crate's tests in `tests/`, and leaves it as written
/// otherwise. On a struct the fields become `pub` as well.
///
/// A crate using it declares a `test-util` feature and enables it from its own
/// dev-dependencies, so release builds never see the wider visibility. See
/// `docs/developing/code-standards/testing.md`.
#[proc_macro_attribute]
pub fn test_pub(args: TokenStream, item: TokenStream) -> TokenStream {
    test_pub::test_pub_impl(args, item)
}
