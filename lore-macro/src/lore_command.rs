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
    let mut run_match = quote! {};
    let mut validate_match = quote! {};
    let mut conversions = quote! {};
    for variant in variants.iter() {
        let ident = &variant.ident;
        let args_type = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,
            _ => panic!("LoreCommand variant {ident} must hold exactly one arguments type"),
        };
        inside_match = quote! {
            #inside_match
            #name::#ident(args) => {
                let handler = ::core::pin::pin!(crate::args::InvokableLoreArgs::invoke_local(args, globals, callback));
                handler.await
            }
        };
        run_match = quote! {
            #run_match
            #name::#ident(_) => crate::args::run_handler(self, globals, callback, |command| match command {
                #name::#ident(args) => args,
                _ => unreachable!(),
            }),
        };
        validate_match = quote! {
            #validate_match
            #name::#ident(args) => ::lore_base::text::ValidateText::validate_text(args),
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
            /// Runs this command's handler as a future.
            ///
            /// Each arm pins the handler's future in a local that lives across the await, so the
            /// future is built in place in this future's state. Awaited by value, it would pass
            /// through two temporaries joined by a full copy. LLVM merges each such pair and drops
            /// its lifetime markers, and the poll frame would then hold one for every command.
            pub async fn invoke_local(self, globals: crate::interface::LoreGlobalArgs, callback: crate::interface::LoreEventCallback) -> i32 {
                match self {
                    #inside_match
                }
            }

            /// Runs this command's handler to completion on the calling thread, which must be
            /// outside the runtime.
            ///
            /// Each arm binds nothing and passes the whole command to `run_handler`, which moves the
            /// arguments out, so in optimized builds this function's frame holds no command's
            /// arguments.
            pub(crate) fn run_local(self, globals: crate::interface::LoreGlobalArgs, callback: crate::interface::LoreEventCallback) -> i32 {
                match self {
                    #run_match
                }
            }
        }

        impl ::lore_base::text::ValidateText for #name {
            fn validate_text(&self) -> ::std::result::Result<(), ::lore_base::text::TextNotUtf8> {
                match self {
                    #validate_match
                }
            }
        }

        #conversions
    }.into()
}
