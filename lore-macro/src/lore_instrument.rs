// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use proc_macro::TokenStream;
use quote::quote;
use syn::ItemFn;
use syn::parse::Parse;
use syn::parse::ParseStream;

struct LoreInstrumentArgs {
    state_path: Option<syn::Path>,
}

impl Parse for LoreInstrumentArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self { state_path: None });
        }

        let ident: syn::Ident = input.parse()?;
        if ident != "state" {
            return Err(syn::Error::new(ident.span(), "expected `state`"));
        }
        let _eq: syn::Token![=] = input.parse()?;
        let lit: syn::LitStr = input.parse()?;
        let path: syn::Path = lit.parse()?;
        Ok(Self {
            state_path: Some(path),
        })
    }
}

pub fn lore_instrument_impl(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(args as LoreInstrumentArgs);
    let input = syn::parse_macro_input!(item as ItemFn);

    let state_path = args.state_path.unwrap_or_else(|| {
        syn::parse_str("lore_telemetry::execution_state::ServerExecutionState").unwrap()
    });

    let vis = &input.vis;
    let sig = &input.sig;
    let attrs = &input.attrs;
    let block = &input.block;

    if sig.asyncness.is_some() {
        // Async function: wrap body with .instrument()
        quote! {
            #(#attrs)*
            #vis #sig {
                let __lore_body = async move #block;
                if let Some(__lore_state) = lore_revision::runtime::try_execution_context()
                    .and_then(|ctx| ctx.caller_state().cloned())
                    .and_then(|any| ::std::sync::Arc::downcast::<#state_path>(any).ok())
                {
                    tracing::Instrument::instrument(__lore_body, __lore_state.span.clone()).await
                } else {
                    __lore_body.await
                }
            }
        }
        .into()
    } else {
        // Sync function: enter span via guard
        quote! {
            #(#attrs)*
            #vis #sig {
                let __lore_span_guard = lore_revision::runtime::try_execution_context()
                    .and_then(|ctx| ctx.caller_state().cloned())
                    .and_then(|any| ::std::sync::Arc::downcast::<#state_path>(any).ok())
                    .map(|state| state.span.clone().entered());
                let _ = &__lore_span_guard;
                #block
            }
        }
        .into()
    }
}
