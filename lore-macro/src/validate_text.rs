// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use proc_macro2::TokenStream;
use quote::quote;
use syn::Data;
use syn::DeriveInput;
use syn::Fields;

/// Body of a `ValidateText` implementation that checks every field in turn.
///
/// Every field is walked, so a field whose type has no `ValidateText`
/// implementation fails to compile instead of going unchecked. Field names are
/// string literals and only the failure path builds the path to the field, so
/// checking costs nothing but the UTF-8 scan.
pub fn get_validate_text_body(input: &DeriveInput) -> TokenStream {
    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            Fields::Unnamed(_) | Fields::Unit => return quote! { Ok(()) },
        },
        Data::Enum(_) | Data::Union(_) => return quote! { Ok(()) },
    };

    let checks = fields.iter().map(|item| {
        let name = item.ident.as_ref().expect("named field has an identifier");
        let text = name.to_string();
        quote! {
            ::lore_base::text::ValidateText::validate_text(&self.#name)
                .map_err(|error| error.inside(#text))?;
        }
    });

    quote! {
        #(#checks)*
        Ok(())
    }
}

/// The whole `ValidateText` implementation for a type, so the standalone derive
/// and the `LoreArgs` derive emit the same thing.
pub fn get_validate_text_tokens(input: &DeriveInput) -> TokenStream {
    let name = &input.ident;
    let body = get_validate_text_body(input);

    quote! {
        impl ::lore_base::text::ValidateText for #name {
            fn validate_text(
                &self,
            ) -> ::std::result::Result<(), ::lore_base::text::TextNotUtf8> {
                #body
            }
        }
    }
}

/// A standalone `ValidateText` implementation, for a struct that appears as an
/// argument field or as an array element rather than as a whole argument set.
pub fn get_validate_text_impl(input: &DeriveInput) -> proc_macro::TokenStream {
    get_validate_text_tokens(input).into()
}
