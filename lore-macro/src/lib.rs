// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod lore_args;
mod lore_command;
mod lore_instrument;
mod validate_text;
mod variant;

use proc_macro::TokenStream;
use syn::DeriveInput;
use syn::parse_macro_input;

#[proc_macro_derive(VariantTypeSize)]
pub fn variant_type_size(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);

    variant::get_variant_type_sizes(&ast)
}

#[proc_macro_derive(LoreArgs, attributes(handler))]
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

    lore_command::get_invoke_impl(&ast)
}

#[proc_macro_attribute]
pub fn lore_instrument(args: TokenStream, item: TokenStream) -> TokenStream {
    lore_instrument::lore_instrument_impl(args, item)
}
