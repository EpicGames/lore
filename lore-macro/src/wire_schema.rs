// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use proc_macro2::TokenStream;
use quote::quote;
use syn::Data;
use syn::DeriveInput;
use syn::Fields;
use syn::parse_quote;

/// A `lore_base::wire_schema::WireSchema` implementation that hashes the type's fields, or its
/// variants and theirs, in declaration order, as bitcode's derive encodes them.
pub fn get_wire_schema_impl(input: &DeriveInput) -> proc_macro::TokenStream {
    wire_schema_impl(input, "enum").into()
}

/// A `lore_base::wire_schema::WireSchema` implementation that hashes the type's fields, or its
/// variants and theirs, in declaration order.
///
/// A struct mixes in a tag, its field count, and each field's name and schema. An enum mixes in
/// `enum_tag`, which names how its variants are encoded, its variant count, and for each variant
/// its name, the kind and count of its fields, and each field's name, or its position where the
/// fields are unnamed, and schema. A field bitcode skips is left out, as bitcode leaves it out of
/// the encoding. The type's own name is left out, since the encoding doesn't carry it. A type
/// parameter must implement the trait itself.
pub fn wire_schema_impl(input: &DeriveInput, enum_tag: &str) -> TokenStream {
    let name = &input.ident;
    let mut generics = input.generics.clone();
    for parameter in generics.type_params_mut() {
        parameter
            .bounds
            .push(parse_quote!(::lore_base::wire_schema::WireSchema));
    }
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();

    let body = match &input.data {
        Data::Struct(data) => {
            let fields = fields_schema(&data.fields);
            quote! {
                let hash = ::lore_base::wire_schema::schema_leaf("struct");
                #fields
            }
        }
        Data::Enum(data) => {
            let count = data.variants.len() as u64;
            let variants = data.variants.iter().map(|variant| {
                let text = variant.ident.to_string();
                let fields = fields_schema(&variant.fields);
                quote! {
                    let hash = ::lore_base::wire_schema::schema_text(hash, #text);
                    #fields
                }
            });
            quote! {
                let hash = ::lore_base::wire_schema::schema_leaf(#enum_tag);
                let hash = ::lore_base::wire_schema::schema_u64(hash, #count);
                #(#variants)*
            }
        }
        Data::Union(_) => panic!("WireSchema can't be derived for a union"),
    };

    quote! {
        impl #impl_generics ::lore_base::wire_schema::WireSchema for #name #type_generics
        #where_clause
        {
            const SCHEMA: u64 = {
                #body
                hash
            };
        }
    }
}

/// Statements that mix the kind and count of `fields` into `hash`, then each field's name, or
/// position where the fields are unnamed, and schema.
fn fields_schema(fields: &Fields) -> TokenStream {
    let kind = match fields {
        Fields::Named(_) => "named",
        Fields::Unnamed(_) => "unnamed",
        Fields::Unit => "unit",
    };
    let encoded: Vec<_> = fields
        .iter()
        .enumerate()
        .filter(|(_, field)| !skipped_by_bitcode(field))
        .collect();
    let count = encoded.len() as u64;
    let mixed = encoded.into_iter().map(|(index, field)| {
        let text = field
            .ident
            .as_ref()
            .map_or_else(|| index.to_string(), ToString::to_string);
        let ty = &field.ty;
        quote! {
            let hash = ::lore_base::wire_schema::schema_text(hash, #text);
            let hash = ::lore_base::wire_schema::schema_u64(
                hash,
                <#ty as ::lore_base::wire_schema::WireSchema>::SCHEMA,
            );
        }
    });
    quote! {
        let hash = ::lore_base::wire_schema::schema_text(hash, #kind);
        let hash = ::lore_base::wire_schema::schema_u64(hash, #count);
        #(#mixed)*
    }
}

/// Whether bitcode's derive leaves the field out of the encoding, as `#[bitcode(skip)]` does.
fn skipped_by_bitcode(field: &syn::Field) -> bool {
    let mut skipped = false;
    for attr in field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("bitcode"))
    {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                skipped = true;
            } else if meta.input.peek(syn::Token![=]) {
                // Another of bitcode's options, such as `bound_type = "..."`, which leaves the
                // field in the encoding.
                meta.value()?.parse::<syn::LitStr>()?;
            }
            Ok(())
        })
        .unwrap_or_else(|err| panic!("WireSchema can't read a bitcode attribute: {err}"));
    }
    skipped
}
