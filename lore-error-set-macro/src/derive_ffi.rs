// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Derive macro implementation for `FfiError`.
//!
//! Parses `#[ffi_code(N)]` on a struct or enum and generates an
//! `impl FfiError` that returns the given integer constant.

use proc_macro2::TokenStream;
use quote::quote;
use syn::DeriveInput;

pub fn derive_ffi_error(input: &DeriveInput) -> TokenStream {
    let name = &input.ident;

    let code = match extract_ffi_code(input) {
        Ok(lit) => lit,
        Err(err) => return err.to_compile_error(),
    };

    quote! {
        // Scopes the attribute to the FFI code registry. This is a bare
        // identifier, so it resolves in the module the derive expands into
        // rather than against this crate: a `#[ffi_code(N)]` written anywhere
        // that does not declare `__ffi_code_registry_marker` fails to compile.
        // Codes share one numeric space and one process-exit-status space, so
        // they are allocated in exactly one place.
        const _: fn() = __ffi_code_registry_marker;

        impl #name {
            /// This type's FFI error code, usable in const position.
            ///
            /// Lets another declaration that must spell the same code — a
            /// `#[repr(C)]` enum discriminant, say — assert equality at compile
            /// time instead of duplicating the literal unchecked.
            pub const FFI_CODE: i32 = #code;
        }

        impl lore_error_set::FfiError for #name {
            fn ffi_code(&self) -> i32 { Self::FFI_CODE }
        }
    }
}

fn extract_ffi_code(input: &DeriveInput) -> syn::Result<syn::LitInt> {
    for attr in &input.attrs {
        if attr.path().is_ident("ffi_code") {
            let lit: syn::LitInt = attr.parse_args()?;
            // Validate it parses as i32.
            lit.base10_parse::<i32>().map_err(|_err| {
                syn::Error::new_spanned(&lit, "ffi_code must be an integer literal")
            })?;
            return Ok(lit);
        }
    }

    Err(syn::Error::new_spanned(
        &input.ident,
        "#[derive(FfiError)] requires #[ffi_code(N)] attribute",
    ))
}
