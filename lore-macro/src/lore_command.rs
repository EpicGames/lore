// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use proc_macro::TokenStream;
use quote::quote;
use syn::Data;
use syn::DeriveInput;
use syn::Fields;
use syn::Variant;

pub fn get_lore_command_impl(input: &DeriveInput) -> TokenStream {
    let name = &input.ident;

    let variants: Vec<&Variant> = match &input.data {
        Data::Enum(enum_data) => enum_data.variants.iter().collect(),
        _ => panic!("LoreCommand should only be used on the LoreCommand enum"),
    };

    let mut inside_match = quote! {};
    let mut conversions = quote! {};
    for variant in variants.iter() {
        let ident = &variant.ident;
        let args_type = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,
            _ => panic!("LoreCommand variant {ident} must hold exactly one arguments type"),
        };
        inside_match = quote! {
            #inside_match
            #name::#ident(args) => { crate::args::invoke_args(args, globals, callback).await }
        };
        conversions = quote! {
            #conversions
            impl From<#args_type> for #name {
                fn from(args: #args_type) -> Self {
                    #name::#ident(args)
                }
            }
        };
    }

    quote! {
        impl #name {
            pub async fn invoke_local(self, globals: crate::interface::LoreGlobalArgs, callback: crate::interface::LoreEventCallback) -> i32 {
                match self {
                    #inside_match
                }
            }
        }

        #conversions
    }.into()
}
