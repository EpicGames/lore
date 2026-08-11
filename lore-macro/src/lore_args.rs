// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use proc_macro::TokenStream;
use quote::quote;
use syn::DeriveInput;
use syn::Path;

use crate::validate_text::get_validate_text_tokens;

pub fn get_lore_args_impl(input: &DeriveInput) -> TokenStream {
    let name = &input.ident;
    let validate_text = get_validate_text_tokens(input);

    let handler_attr = input
        .attrs
        .iter()
        .find(|attr| attr.path().segments.iter().next_back().unwrap().ident == "handler")
        .unwrap_or_else(|| panic!("LoreArgs missing a `#[handler()] attribute"));

    let handler_fn_name: Path = handler_attr
        .parse_args()
        .unwrap_or_else(|err| panic!("LoreArgs handler attribute failed to parse: {err}"));

    quote! {
        impl crate::args::LoreArgs for #name {
            fn to_command(self) -> crate::remote::command::LoreCommand {
                self.into()
            }
        }

        #validate_text

        impl crate::args::InvokableLoreArgs for #name {
            async fn invoke_local(
                self,
                globals: LoreGlobalArgs,
                callback: LoreEventCallback,
            ) -> i32 {
                #handler_fn_name (globals, self, callback).await
            }
        }
    }
    .into()
}
