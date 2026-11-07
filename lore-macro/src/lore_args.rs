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

    // Opts a struct with a `vfs: LoreVfsType` field into overriding `requires_service`, for the
    // clone and create arguments: the only two calls that need the service before an instance
    // exists to say so, where their VFS resolves to FUSE, which only the service mounts.
    // `command_requires_service_given` in `lore` matches the same two commands. Every other
    // `#[derive(LoreArgs)]` struct keeps the trait's default (`false`).
    let requires_service =
        if input.attrs.iter().any(|attr| {
            attr.path().segments.iter().next_back().unwrap().ident == "requires_service"
        }) {
            quote! {
                fn requires_service(&self) -> bool {
                    self.vfs.to_config().vfs_type == ::lore_revision::repository::VfsType::Fuse
                }
            }
        } else {
            quote! {}
        };

    quote! {
        impl crate::args::LoreArgs for #name {
            fn to_command(self) -> crate::remote::command::LoreCommand {
                self.into()
            }

            #requires_service
        }

        #validate_text

        impl crate::args::InvokableLoreArgs for #name {
            fn invoke_local(
                self,
                globals: LoreGlobalArgs,
                callback: LoreEventCallback,
            ) -> impl ::core::future::Future<Output = i32> + Send {
                #handler_fn_name (globals, self, callback)
            }
        }
    }
    .into()
}
