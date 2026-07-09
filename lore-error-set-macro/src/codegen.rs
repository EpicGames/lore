// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Code generation for the `#[error_set]` proc-macro.
//!
//! Parses a simple enum declaration like `pub enum Foo { A, B, C }` and
//! generates the full error set implementation including:
//! - Expanded enum with `Traced<T>` wrapper variants + `Internal` variant
//! - `From<T>` impls for each user variant
//! - `std::error::Error`, `std::fmt::Display`, `Debug` impls
//! - `is_*`, `as_*`, `as_*_traced` accessor methods
//! - `ErrorSet` trait impl for cross-set mapping
//! - `FfiError` trait impl for FFI code delegation

use proc_macro2::TokenStream;
use quote::format_ident;
use quote::quote;
use syn::Ident;
use syn::ItemEnum;
use syn::Visibility;

/// Converts a `PascalCase` identifier to `snake_case`.
///
/// Inserts an underscore before each uppercase letter that follows a lowercase
/// letter or digit, then lowercases everything.
///
/// Examples:
/// - `NotFound` -> `not_found`
/// - `AlreadyExists` -> `already_exists`
/// - `Timeout` -> `timeout`
/// - `IOError` -> `io_error`
fn to_snake_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                let prev = chars[i - 1];
                // Insert underscore before an uppercase letter when:
                // 1. The previous character is lowercase or a digit, OR
                // 2. The previous character is uppercase AND the next character
                //    is lowercase (handles acronyms like "IOError" -> "io_error")
                if prev.is_lowercase() || prev.is_ascii_digit() {
                    result.push('_');
                } else if prev.is_uppercase() {
                    if let Some(&next) = chars.get(i + 1) {
                        if next.is_lowercase() {
                            result.push('_');
                        }
                    }
                }
            }
            for lc in c.to_lowercase() {
                result.push(lc);
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Generates the full error set code from a parsed enum declaration.
///
/// The input enum should have bare ident variants (no fields):
/// ```ignore
/// pub enum Foo {
///     A,
///     B,
/// }
/// ```
///
/// Returns a token stream containing the expanded enum and all trait
/// implementations.
pub fn generate_error_set(input: &ItemEnum, derive_clone: bool) -> TokenStream {
    let vis = &input.vis;
    let enum_name = &input.ident;

    // Collect the variant identifiers (bare idents, no fields).
    let variants: Vec<&Ident> = input.variants.iter().map(|v| &v.ident).collect();

    let expanded_enum = generate_expanded_enum(vis, enum_name, &variants, derive_clone);
    let from_impls = generate_from_impls(enum_name, &variants);
    let error_impl = generate_error_impl(enum_name, &variants);
    let display_impl = generate_display_impl(enum_name, &variants);
    let accessor_methods = generate_accessor_methods(vis, enum_name, &variants);
    let error_set_impl = generate_error_set_impl(enum_name, &variants);
    let ffi_error_impl = generate_ffi_error_impl(enum_name, &variants);
    let has_trace_impl = generate_has_trace_impl(enum_name);
    let matched_enum = generate_matched_enum(vis, enum_name, &variants);
    let has_impls = generate_has_impls(enum_name, &variants);
    let strict_forward = generate_strict_forward(vis, enum_name, &variants);

    quote! {
        #expanded_enum
        #from_impls
        #error_impl
        #display_impl
        #accessor_methods
        #error_set_impl
        #ffi_error_impl
        #has_trace_impl
        #matched_enum
        #has_impls
        #strict_forward
    }
}

/// Generates the expanded enum declaration with `Traced<T>` wrappers and
/// `Internal` variant.
fn generate_expanded_enum(
    vis: &Visibility,
    enum_name: &Ident,
    variants: &[&Ident],
    derive_clone: bool,
) -> TokenStream {
    let variant_defs = variants.iter().map(|v| {
        quote! { #v(lore_error_set::Traced<#v>) }
    });

    let clone_impl = if derive_clone {
        let clone_arms = variants.iter().map(|v| {
            quote! { Self::#v(traced) => Self::#v(traced.clone()) }
        });
        quote! {
            impl Clone for #enum_name {
                fn clone(&self) -> Self {
                    match self {
                        #(#clone_arms,)*
                        Self::Internal(traced) => Self::Internal(traced.clone()),
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    quote! {
        #[derive(Debug)]
        #vis enum #enum_name {
            #(#variant_defs,)*
            Internal(lore_error_set::Traced<lore_error_set::Internal>),
        }

        #clone_impl
    }
}

/// Generates `From<T>` impls for each user variant, plus `From<Internal>`.
fn generate_from_impls(enum_name: &Ident, variants: &[&Ident]) -> TokenStream {
    let impls = variants.iter().map(|v| {
        quote! {
            impl From<#v> for #enum_name {
                #[track_caller]
                fn from(err: #v) -> Self {
                    let caller = ::std::panic::Location::caller();
                    let mut trace = lore_error_set::Trace::new();
                    trace.push(lore_error_set::Location::new(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                    ));
                    #enum_name::#v(lore_error_set::Traced::new(err, trace))
                }
            }
        }
    });

    quote! {
        #(#impls)*

        impl From<lore_error_set::Internal> for #enum_name {
            #[track_caller]
            fn from(internal: lore_error_set::Internal) -> Self {
                let caller = ::std::panic::Location::caller();
                let mut trace = lore_error_set::Trace::new();
                trace.push(lore_error_set::Location::new(
                    caller.file(),
                    caller.line(),
                    caller.column(),
                ));
                #enum_name::Internal(lore_error_set::Traced::new(internal, trace))
            }
        }

        impl From<lore_error_set::Traced<lore_error_set::Internal>> for #enum_name {
            fn from(traced: lore_error_set::Traced<lore_error_set::Internal>) -> Self {
                #enum_name::Internal(traced)
            }
        }
    }
}

/// Generates the `std::error::Error` impl.
fn generate_error_impl(enum_name: &Ident, variants: &[&Ident]) -> TokenStream {
    let source_arms = variants.iter().map(|v| {
        quote! { Self::#v(e) => e.source() }
    });

    quote! {
        impl ::std::error::Error for #enum_name {
            fn source(&self) -> Option<&(dyn ::std::error::Error + 'static)> {
                match self {
                    #(#source_arms,)*
                    Self::Internal(e) => e.source(),
                }
            }
        }
    }
}

/// Generates the `std::fmt::Display` impl.
fn generate_display_impl(enum_name: &Ident, variants: &[&Ident]) -> TokenStream {
    let display_arms = variants.iter().map(|v| {
        quote! { Self::#v(e) => ::std::fmt::Display::fmt(e, f) }
    });

    quote! {
        impl ::std::fmt::Display for #enum_name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match self {
                    #(#display_arms,)*
                    Self::Internal(traced) => {
                        // Internal's "what was being attempted" context lives
                        // on the trace as a Location::with_context entry, not
                        // on the Internal struct itself. Walk the trace
                        // newest-first and prepend the most-recent context to
                        // the source's Display.
                        let ctx = traced
                            .trace()
                            .locations()
                            .iter()
                            .rev()
                            .find_map(|loc| loc.context());
                        match ctx {
                            Some(ctx) => write!(f, "{}: {}", ctx, &**traced),
                            None => ::std::fmt::Display::fmt(&**traced, f),
                        }
                    }
                }
            }
        }
    }
}

/// Generates `is_*`, `as_*`, and `as_*_traced` accessor methods.
fn generate_accessor_methods(
    _vis: &Visibility,
    enum_name: &Ident,
    variants: &[&Ident],
) -> TokenStream {
    let is_methods = variants.iter().map(|v| {
        let snake = to_snake_case(&v.to_string());
        let method_name = format_ident!("is_{}", snake);
        quote! {
            pub fn #method_name(&self) -> bool {
                matches!(self, Self::#v(_))
            }
        }
    });

    let is_internal = quote! {
        pub fn is_internal(&self) -> bool {
            matches!(self, Self::Internal(_))
        }
    };

    let as_methods = variants.iter().map(|v| {
        let snake = to_snake_case(&v.to_string());
        let method_name = format_ident!("as_{}", snake);
        quote! {
            pub fn #method_name(&self) -> Option<&#v> {
                match self {
                    Self::#v(e) => Some(e),
                    _ => None,
                }
            }
        }
    });

    let as_internal = quote! {
        pub fn as_internal(&self) -> Option<&lore_error_set::Internal> {
            match self {
                Self::Internal(traced) => Some(&**traced),
                _ => None,
            }
        }
    };

    let as_internal_traced = quote! {
        pub fn as_internal_traced(&self) -> Option<&lore_error_set::Traced<lore_error_set::Internal>> {
            match self {
                Self::Internal(traced) => Some(traced),
                _ => None,
            }
        }
    };

    let as_traced_methods = variants.iter().map(|v| {
        let snake = to_snake_case(&v.to_string());
        let method_name = format_ident!("as_{}_traced", snake);
        quote! {
            pub fn #method_name(&self) -> Option<&lore_error_set::Traced<#v>> {
                match self {
                    Self::#v(e) => Some(e),
                    _ => None,
                }
            }
        }
    });

    let trace_method = {
        let trace_arms = variants.iter().map(|v| {
            quote! { Self::#v(traced) => traced.trace() }
        });
        quote! {
            /// Returns a reference to the trace for this error.
            pub fn trace(&self) -> &lore_error_set::Trace {
                match self {
                    #(#trace_arms,)*
                    Self::Internal(traced) => traced.trace(),
                }
            }
        }
    };

    let internal_trait_impl = quote! {
        impl lore_error_set::internal::SupportsInternalError for #enum_name {
            #[track_caller]
            fn internal(msg: impl Into<String>) -> Self {
                let caller = ::std::panic::Location::caller();
                let mut trace = lore_error_set::Trace::new();
                trace.push(lore_error_set::Location::new(
                    caller.file(),
                    caller.line(),
                    caller.column(),
                ));
                Self::Internal(lore_error_set::Traced::new(
                    lore_error_set::Internal::msg(msg),
                    trace,
                ))
            }

            #[track_caller]
            fn internal_with_context(
                source: impl ::std::error::Error + Send + Sync + 'static,
                context: &str,
            ) -> Self {
                let caller = ::std::panic::Location::caller();
                let mut trace = lore_error_set::Trace::new();
                trace.push(lore_error_set::Location::with_context(
                    caller.file(),
                    caller.line(),
                    caller.column(),
                    ::std::sync::Arc::from(context),
                ));
                Self::Internal(lore_error_set::Traced::new(
                    lore_error_set::Internal::new(::std::sync::Arc::new(source)),
                    trace,
                ))
            }
        }
    };

    let internal_constructors = quote! {
        /// Reexpose trait method without requiring the trait to be in scope.
        pub fn internal(msg: impl Into<String>) -> Self {
            <Self as lore_error_set::internal::SupportsInternalError>::internal(msg)
        }

        /// Reexpose trait method without requiring the trait to be in scope.
        pub fn internal_with_context(
            source: impl ::std::error::Error + Send + Sync + 'static,
            context: &str,
        ) -> Self {
            <Self as lore_error_set::internal::SupportsInternalError>::internal_with_context(
                source,
                context
            )
        }
    };

    quote! {
        impl #enum_name {
            #(#is_methods)*
            #is_internal
            #(#as_methods)*
            #as_internal
            #(#as_traced_methods)*
            #as_internal_traced
            #trace_method
            #internal_constructors
        }

        #internal_trait_impl
    }
}

/// Generates the `ErrorSet` trait impl.
fn generate_error_set_impl(enum_name: &Ident, variants: &[&Ident]) -> TokenStream {
    let matched_name = format_ident!("Matched{}", enum_name);

    // Build the type-level variant list — Cons<A, Cons<B, ..., Nil>>.
    let variants_list = build_variants_list(variants);

    let extract_arms = variants.iter().map(|v| {
        quote! {
            Self::#v(traced) => lore_error_set::TracedBox::from_traced(traced)
        }
    });

    // Generate the chain of downcasts for try_from_inner.
    // Each variant tries to downcast, passing the error along on failure.
    let try_from_downcasts = variants.iter().map(|v| {
        quote! {
            let inner = match inner.downcast::<#v>() {
                Ok(e) => return Ok(Self::#v(lore_error_set::Traced::new(*e, trace))),
                Err(inner) => inner,
            };
        }
    });

    // Generate push_trace arms for each variant.
    let push_trace_arms = variants.iter().map(|v| {
        quote! {
            Self::#v(traced) => traced.trace_mut().push(location)
        }
    });

    // Generate into_matched arms for handleable variants.
    let into_matched_arms = variants.iter().map(|v| {
        quote! {
            Self::#v(mut traced) => {
                traced.trace_mut().push(caller);
                Ok(#matched_name::#v(traced))
            }
        }
    });

    // Generate into_matched_with arms for handleable variants.
    let into_matched_with_arms = variants.iter().map(|v| {
        quote! {
            Self::#v(mut traced) => {
                traced.trace_mut().push(caller);
                Ok(#matched_name::#v(traced))
            }
        }
    });

    quote! {
        impl lore_error_set::ErrorSet for #enum_name {
            type Matched = #matched_name;
            type Variants = #variants_list;

            fn into_matched(
                self,
                context: &str,
                caller: lore_error_set::Location,
            ) -> Result<#matched_name, lore_error_set::Traced<lore_error_set::Internal>> {
                match self {
                    #(#into_matched_arms,)*
                    Self::Internal(traced) => {
                        // Upstream is already Internal: don't wrap it in a new
                        // Internal (which would nest source chains). Adopt the
                        // upstream Internal as-is and push a context-bearing
                        // trace entry recording this hop.
                        let (internal, mut trace) = traced.into_parts();
                        trace.push(lore_error_set::Location::with_context(
                            caller.file,
                            caller.line,
                            caller.column,
                            ::std::sync::Arc::from(context),
                        ));
                        Err(lore_error_set::Traced::new(internal, trace))
                    }
                }
            }

            fn into_matched_with<F>(
                self,
                f: F,
                caller: lore_error_set::Location,
            ) -> Result<#matched_name, lore_error_set::Traced<lore_error_set::Internal>>
            where
                F: FnOnce() -> String,
            {
                match self {
                    #(#into_matched_with_arms,)*
                    Self::Internal(traced) => {
                        let context_str = f();
                        let (internal, mut trace) = traced.into_parts();
                        trace.push(lore_error_set::Location::with_context(
                            caller.file,
                            caller.line,
                            caller.column,
                            ::std::sync::Arc::from(context_str.as_str()),
                        ));
                        Err(lore_error_set::Traced::new(internal, trace))
                    }
                }
            }

            fn push_trace(&mut self, location: lore_error_set::Location) {
                match self {
                    #(#push_trace_arms,)*
                    Self::Internal(traced) => traced.trace_mut().push(location),
                }
            }

            fn extract_inner(self) -> lore_error_set::TracedBox {
                match self {
                    #(#extract_arms,)*
                    Self::Internal(traced) => lore_error_set::TracedBox::from_traced(traced),
                }
            }

            fn try_from_inner(traced: lore_error_set::TracedBox) -> Result<Self, lore_error_set::TracedBox> {
                let lore_error_set::TracedBox { inner, trace } = traced;
                #(#try_from_downcasts)*
                Err(lore_error_set::TracedBox { inner, trace })
            }

            fn wrap_internal(err: lore_error_set::TracedBox, context: &str) -> Self {
                // The hop's context is expected to have been recorded on the
                // trace by the caller (via Location::with_context). The
                // `context` parameter is unused on the adopt path; the trace
                // is the single source of truth for hop annotations. If the
                // inner is already an Internal, adopt it as-is. Otherwise
                // wrap the inner as the source of a new Internal.
                let _ = context;
                match err.inner.downcast::<lore_error_set::Internal>() {
                    Ok(boxed_internal) => Self::Internal(lore_error_set::Traced::new(
                        *boxed_internal,
                        err.trace,
                    )),
                    Err(inner) => Self::Internal(lore_error_set::Traced::new(
                        lore_error_set::Internal::new(::std::sync::Arc::from(inner)),
                        err.trace,
                    )),
                }
            }
        }
    }
}

/// Generates the `Matched` enum for an error set.
///
/// For `#[error_set] pub enum Foo { A, B }`, this generates:
///
/// ```ignore
/// pub enum MatchedFoo {
///     A(Traced<A>),
///     B(Traced<B>),
///     // NO Internal variant, NO Ok variant
/// }
/// ```
///
/// Plus `forward` and `forward_with` methods on the enum.
fn generate_matched_enum(vis: &Visibility, enum_name: &Ident, variants: &[&Ident]) -> TokenStream {
    let matched_name = format_ident!("Matched{}", enum_name);

    let variant_defs = variants.iter().map(|v| {
        quote! { #v(lore_error_set::Traced<#v>) }
    });

    // Generate forward match arms for each variant.
    let make_forward_arms = || {
        variants
            .iter()
            .map(|v| {
                quote! {
                    Self::#v(traced) => {
                        let traced_box = lore_error_set::TracedBox::from_traced(traced);
                        match Target::try_from_inner(traced_box) {
                            Ok(mut target) => {
                                target.push_trace(lore_error_set::Location::with_context(
                                    caller.file(),
                                    caller.line(),
                                    caller.column(),
                                    ::std::sync::Arc::from(context),
                                ));
                                target
                            }
                            Err(mut unmatched) => {
                                unmatched.trace.push(lore_error_set::Location::with_context(
                                    caller.file(),
                                    caller.line(),
                                    caller.column(),
                                    ::std::sync::Arc::from(context),
                                ));
                                Target::wrap_internal(unmatched, context)
                            }
                        }
                    }
                }
            })
            .collect::<Vec<_>>()
    };
    let forward_arms_strict = make_forward_arms();

    let make_forward_with_arms = || {
        variants
            .iter()
            .map(|v| {
                quote! {
                    Self::#v(traced) => {
                        let traced_box = lore_error_set::TracedBox::from_traced(traced);
                        match Target::try_from_inner(traced_box) {
                            Ok(mut target) => {
                                target.push_trace(lore_error_set::Location::with_context(
                                    caller.file(),
                                    caller.line(),
                                    caller.column(),
                                    ::std::sync::Arc::from(context_str.as_str()),
                                ));
                                target
                            }
                            Err(mut unmatched) => {
                                unmatched.trace.push(lore_error_set::Location::with_context(
                                    caller.file(),
                                    caller.line(),
                                    caller.column(),
                                    ::std::sync::Arc::from(context_str.as_str()),
                                ));
                                Target::wrap_internal(unmatched, &context_str)
                            }
                        }
                    }
                }
            })
            .collect::<Vec<_>>()
    };
    let forward_with_arms_strict = make_forward_with_arms();

    quote! {
        #[derive(Debug)]
        #vis enum #matched_name {
            #(#variant_defs,)*
        }

        impl #matched_name {
            /// Forward this matched error to a target error set, requiring at
            /// compile time that the target declares every variant of the
            /// source. A missing target variant is a `Has<V>` trait-bound
            /// error rather than a runtime collapse to `Target::Internal`.
            ///
            /// The bound uses the *source* set's full variant list — over-
            /// strict for narrowed catch-all arms (where some variants were
            /// already handled), but always correct: widen the target.
            #[track_caller]
            pub fn forward<Target>(self, context: &str) -> Target
            where
                Target: lore_error_set::ErrorSet
                      + lore_error_set::HasAll<<#enum_name as lore_error_set::ErrorSet>::Variants>,
            {
                let caller = ::std::panic::Location::caller();
                match self {
                    #(#forward_arms_strict)*
                }
            }

            /// Like [`forward`](Self::forward), but with a lazily-evaluated
            /// context string. The closure is only called on the error path.
            #[track_caller]
            pub fn forward_with<Target, F>(self, f: F) -> Target
            where
                Target: lore_error_set::ErrorSet
                      + lore_error_set::HasAll<<#enum_name as lore_error_set::ErrorSet>::Variants>,
                F: FnOnce() -> String,
            {
                let caller = ::std::panic::Location::caller();
                let context_str = f();
                match self {
                    #(#forward_with_arms_strict)*
                }
            }
        }
    }
}

/// Builds the type-level variant list for `ErrorSet::Variants`.
///
/// For variants `[A, B, C]`, emits
/// `Cons<A, Cons<B, Cons<C, Nil>>>`. Empty enum yields `Nil`. The list
/// excludes `Internal` because every error set carries it unconditionally.
fn build_variants_list(variants: &[&Ident]) -> TokenStream {
    let mut acc = quote! { lore_error_set::variants::Nil };
    for v in variants.iter().rev() {
        acc = quote! { lore_error_set::variants::Cons<#v, #acc> };
    }
    acc
}

/// Generates per-variant `Has<V>` marker impls.
///
/// For `#[error_set] pub enum Foo { A, B }`, this emits:
///
/// ```ignore
/// impl lore_error_set::Has<A> for Foo {}
/// impl lore_error_set::Has<B> for Foo {}
/// impl lore_error_set::Has<lore_error_set::Internal> for Foo {}
/// ```
///
/// These markers are consumed by the strict `forward` method (and per-source
/// `ResultForwardStrict` trait) on every source set: the source-side `where`
/// clause requires `Target: Has<V>` for each of its variants, turning a
/// missing target variant into a compile error.
fn generate_has_impls(enum_name: &Ident, variants: &[&Ident]) -> TokenStream {
    let variant_impls = variants.iter().map(|v| {
        quote! {
            impl lore_error_set::Has<#v> for #enum_name {}
        }
    });

    quote! {
        #(#variant_impls)*
        impl lore_error_set::Has<lore_error_set::Internal> for #enum_name {}
    }
}

/// Generates the strict `forward` / `forward_with` inherent methods on the
/// source error set.
///
/// For `#[error_set] pub enum Foo { A, B }`, this emits roughly:
///
/// ```ignore
/// impl Foo {
///     #[track_caller]
///     pub fn forward<Target>(self, context: &str) -> Target
///     where
///         Target: lore_error_set::ErrorSet
///               + lore_error_set::HasAll<<Self as lore_error_set::ErrorSet>::Variants>,
///     { ... }
///
///     #[track_caller]
///     pub fn forward_with<Target, F>(self, f: F) -> Target
///     where
///         Target: lore_error_set::ErrorSet
///               + lore_error_set::HasAll<<Self as lore_error_set::ErrorSet>::Variants>,
///         F: FnOnce() -> String,
///     { ... }
/// }
/// ```
///
/// The `HasAll<Self::Variants>` bound expands (via `HasAll`'s recursive
/// blanket impl) to `Target: Has<A> + Has<B>` for `Foo` — a missing target
/// variant is a `Has<V>` trait-bound error rather than a runtime collapse to
/// `Target::Internal`.
///
/// The `Result`-extension counterpart lives in `lore_error_set::ForwardStrict`
/// — a single generic trait shared by all sources, bounded the same way. The
/// inherent methods emitted here are the value-receiver form, useful inside
/// `map_err(|e| e.forward::<Target>("ctx"))` and catch-all match arms.
///
/// The runtime path is unchanged (`extract_inner` / `try_from_inner` /
/// `wrap_internal`); the variant-presence guarantee is type-system-only.
fn generate_strict_forward(
    _vis: &Visibility,
    enum_name: &Ident,
    _variants: &[&Ident],
) -> TokenStream {
    quote! {
        impl #enum_name {
            /// Forward this error to a target error set, requiring at compile
            /// time that the target declares every variant of the source.
            ///
            /// A missing target variant is a `Has<V>` trait-bound error rather
            /// than a runtime collapse to `Target::Internal`.
            #[track_caller]
            pub fn forward<Target>(self, context: &str) -> Target
            where
                Target: lore_error_set::ErrorSet
                      + lore_error_set::HasAll<<Self as lore_error_set::ErrorSet>::Variants>,
            {
                let caller = ::std::panic::Location::caller();
                let traced = lore_error_set::ErrorSet::extract_inner(self);
                match Target::try_from_inner(traced) {
                    Ok(mut target) => {
                        lore_error_set::ErrorSet::push_trace(
                            &mut target,
                            lore_error_set::Location::with_context(
                                caller.file(),
                                caller.line(),
                                caller.column(),
                                ::std::sync::Arc::from(context),
                            ),
                        );
                        target
                    }
                    Err(mut unmatched) => {
                        unmatched.trace.push(lore_error_set::Location::with_context(
                            caller.file(),
                            caller.line(),
                            caller.column(),
                            ::std::sync::Arc::from(context),
                        ));
                        Target::wrap_internal(unmatched, context)
                    }
                }
            }

            /// Like [`forward`](Self::forward), but with a lazily-evaluated
            /// context string. The closure is only called on the error path.
            #[track_caller]
            pub fn forward_with<Target, F>(self, f: F) -> Target
            where
                Target: lore_error_set::ErrorSet
                      + lore_error_set::HasAll<<Self as lore_error_set::ErrorSet>::Variants>,
                F: FnOnce() -> String,
            {
                let caller = ::std::panic::Location::caller();
                let context_str = f();
                let traced = lore_error_set::ErrorSet::extract_inner(self);
                match Target::try_from_inner(traced) {
                    Ok(mut target) => {
                        lore_error_set::ErrorSet::push_trace(
                            &mut target,
                            lore_error_set::Location::with_context(
                                caller.file(),
                                caller.line(),
                                caller.column(),
                                ::std::sync::Arc::from(context_str.as_str()),
                            ),
                        );
                        target
                    }
                    Err(mut unmatched) => {
                        unmatched.trace.push(lore_error_set::Location::with_context(
                            caller.file(),
                            caller.line(),
                            caller.column(),
                            ::std::sync::Arc::from(context_str.as_str()),
                        ));
                        Target::wrap_internal(unmatched, &context_str)
                    }
                }
            }
        }
    }
}

/// Generates the `FfiError` trait impl.
///
/// Every variant type must implement `FfiError`. If a variant type does not
/// implement `FfiError`, the generated code will not compile. This is
/// intentional per the design (TD-7) — all discrete error types in a set
/// should define their FFI codes.
fn generate_ffi_error_impl(enum_name: &Ident, variants: &[&Ident]) -> TokenStream {
    let ffi_arms = variants.iter().map(|v| {
        quote! { Self::#v(e) => lore_error_set::FfiError::ffi_code(&**e) }
    });

    quote! {
        impl lore_error_set::FfiError for #enum_name {
            fn ffi_code(&self) -> i32 {
                match self {
                    #(#ffi_arms,)*
                    Self::Internal(_) => lore_error_set::Internal::FFI_CODE,
                }
            }
        }
    }
}

/// Generates the `HasTrace` trait impl.
///
/// The inherent `trace()` method (in the `impl #enum_name` block) already
/// returns the trace; this impl exposes it through the `HasTrace` bound so
/// generic code can read the trace without naming the concrete enum.
fn generate_has_trace_impl(enum_name: &Ident) -> TokenStream {
    quote! {
        impl lore_error_set::HasTrace for #enum_name {
            fn trace(&self) -> &lore_error_set::Trace {
                Self::trace(self)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_single_word() {
        assert_eq!(to_snake_case("Timeout"), "timeout");
    }

    #[test]
    fn snake_case_two_words() {
        assert_eq!(to_snake_case("NotFound"), "not_found");
    }

    #[test]
    fn snake_case_three_words() {
        assert_eq!(to_snake_case("AlreadyExists"), "already_exists");
    }

    #[test]
    fn snake_case_acronym() {
        assert_eq!(to_snake_case("IOError"), "io_error");
    }

    #[test]
    fn snake_case_lower_start() {
        assert_eq!(to_snake_case("timeout"), "timeout");
    }
}
