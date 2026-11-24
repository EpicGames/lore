// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use proc_macro::TokenStream;
use proc_macro2::Ident;
use proc_macro2::Span;
use quote::quote;
use syn::Data;
use syn::DeriveInput;
use syn::Fields;

/// `lore_base::encoding::Positioned` for an enum whose variants each hold one value, and
/// bitcode's `Encode` and `Decode` through it. A variant's position is its index in the enum's
/// declaration, so a variant appended last leaves every other variant's encoding as it is.
///
/// The enum also gets `lore_base::wire_schema::WireSchema`, whose schema names the encoding by
/// position, so that it differs from the schema of the same enum under bitcode's derive. With the
/// `test-util` feature of the crate that derives it, the enum also gets `VARIANTS`, the number of
/// its variants, for tests that visit every variant.
pub fn get_position_bitcode_impl(input: &DeriveInput) -> TokenStream {
    let name = &input.ident;
    let Data::Enum(data) = &input.data else {
        panic!("PositionBitcode input {name} is not an enum");
    };
    let payload = Ident::new(&format!("{name}Payload"), Span::call_site());

    let mut payload_variants = quote! {};
    let mut encode_arms = quote! {};
    let mut collect_arms = quote! {};
    let mut decode_arms = quote! {};
    for (position, variant) in data.variants.iter().enumerate() {
        let ident = &variant.ident;
        let ty = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,
            _ => panic!("PositionBitcode variant {name}::{ident} must hold exactly one value"),
        };
        let position = u32::try_from(position).expect("an enum has fewer than 2^32 variants");
        payload_variants = quote! {
            #payload_variants
            #ident(<#ty as ::bitcode::Encode>::Encoder),
        };
        encode_arms = quote! {
            #encode_arms
            #name::#ident(value) => #payload::#ident(::lore_base::encoding::payload_encoder(value)),
        };
        collect_arms = quote! {
            #collect_arms
            #payload::#ident(mut encoder) => {
                ::lore_base::encoding::collect_payload(#position, &mut encoder, out)
            }
        };
        decode_arms = quote! {
            #decode_arms
            #position => ::lore_base::encoding::decode_payload(input).map(#name::#ident),
        };
    }
    let variants =
        u32::try_from(data.variants.len()).expect("an enum has fewer than 2^32 variants");
    let schema = crate::wire_schema::wire_schema_impl(input, "enum by position");

    quote! {
        const _: () = {
            #[allow(clippy::large_enum_variant)]
            pub enum #payload {
                #payload_variants
            }

            impl ::lore_base::encoding::Positioned for #name {
                type Payload = #payload;

                #[inline]
                fn encode_payload(&self) -> #payload {
                    match self {
                        #encode_arms
                    }
                }

                #[inline]
                fn collect_payload(payload: #payload, out: &mut ::std::vec::Vec<u8>) {
                    match payload {
                        #collect_arms
                    }
                }

                fn decode_positioned(
                    position: u32,
                    input: &mut &[u8],
                ) -> ::bitcode::__private::Result<Self> {
                    match position {
                        #decode_arms
                        _ => ::lore_base::encoding::invalid_position(),
                    }
                }
            }

            impl ::bitcode::Encode for #name {
                type Encoder = ::lore_base::encoding::PositionEncoder<#name>;
            }

            impl<'de> ::bitcode::Decode<'de> for #name {
                type Decoder = ::lore_base::encoding::PositionDecoder<#name>;
            }
        };

        #schema

        #[cfg(feature = "test-util")]
        impl #name {
            /// The number of variants.
            pub const VARIANTS: u32 = #variants;
        }
    }
    .into()
}
