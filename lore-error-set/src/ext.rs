// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Extension traits on `Result` for ergonomic error mapping.
//!
//! [`ResultExt`] provides `.try_match()` and `.map_matched_err()` for mapping
//! errors between error sets with mandatory context strings. For propagation
//! the strict [`ForwardStrict::forward`] (auto-implemented for every
//! `Result<T, E: ErrorSet>`) enforces at compile time that the target
//! declares every variant of the source — a missing target variant is a
//! `Has<V>` trait-bound error rather than a runtime collapse to
//! `Target::Internal`.

use std::sync::Arc;

use crate::internal::Internal;
use crate::location::Location;
use crate::set::ErrorSet;
use crate::traced::Trace;
use crate::traced::Traced;

// ---------------------------------------------------------------------------
// WrapInternal — wrap any std::error::Error as Internal
// ---------------------------------------------------------------------------

/// Extension trait on `Result<T, E>` for wrapping arbitrary errors as
/// [`Traced<Internal>`].
///
/// Unlike [`ResultExt`] (which requires `E: ErrorSet`), this trait works with
/// any `E: std::error::Error` — useful inside error-set implementations where
/// the source error is not itself an error set.
///
/// The returned `Result<T, Traced<Internal>>` propagates with `?` into any
/// error set via the generated `From<Traced<Internal>>` impl.
///
/// ```
/// use lore_error_set::prelude::*;
///
/// fn parse_port(s: &str) -> Result<u16, Traced<Internal>> {
///     s.parse::<u16>().internal("parsing port number")
/// }
///
/// let err = parse_port("not_a_number").unwrap_err();
/// // The context lives on the trace as a Location::with_context entry.
/// assert_eq!(
///     err.trace().locations().last().and_then(|l| l.context()),
///     Some("parsing port number"),
/// );
/// ```
pub trait WrapInternal<T> {
    /// Maps the error to [`Traced<Internal>`] with the given context string,
    /// capturing the caller location as the first trace entry.
    fn internal(self, context: &str) -> Result<T, Traced<Internal>>;

    /// Like [`internal`](WrapInternal::internal), but with a lazily-evaluated
    /// context string. The closure is only called on the error path.
    fn internal_with<F>(self, f: F) -> Result<T, Traced<Internal>>
    where
        F: FnOnce() -> String;
}

impl<T, E> WrapInternal<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    #[track_caller]
    fn internal(self, context: &str) -> Result<T, Traced<Internal>> {
        let caller = std::panic::Location::caller();
        self.map_err(|e| {
            let mut trace = Trace::new();
            trace.push(Location::with_context(
                caller.file(),
                caller.line(),
                caller.column(),
                Arc::from(context),
            ));
            Traced::new(Internal::new(Arc::new(e)), trace)
        })
    }

    #[track_caller]
    fn internal_with<F>(self, f: F) -> Result<T, Traced<Internal>>
    where
        F: FnOnce() -> String,
    {
        let caller = std::panic::Location::caller();
        self.map_err(|e| {
            let context = f();
            let mut trace = Trace::new();
            trace.push(Location::with_context(
                caller.file(),
                caller.line(),
                caller.column(),
                Arc::from(context.as_str()),
            ));
            Traced::new(Internal::new(Arc::new(e)), trace)
        })
    }
}

// ---------------------------------------------------------------------------
// InternalForbiddenOnErrorSets — poison `.internal()` where it would lose data
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub mod guard {
    /// Unsatisfiable bound carried by the poisoned `.internal()` overloads.
    ///
    /// Nothing implements this trait, and nothing outside this crate can: the
    /// only receivers that would matter are `Result<..>`, so an external impl
    /// is a foreign type and a foreign trait, which the orphan rule rejects.
    /// That makes the methods bounded on it uncallable, including under UFCS.
    #[diagnostic::on_unimplemented(
        message = "`.internal()` is not available on `{Self}`",
        label = "this error already carries its own variants and trace",
        note = "`.internal()` is for foreign errors (io::Error, ParseIntError, ...)",
        note = "for an error set, use `.forward(\"context\")` to preserve its variants",
        note = "to collapse deliberately, use `Target::internal_with_context(err, \"context\")`",
        note = "for a `Traced<_>`, use `.chain_err(\"context\")` or `.chain_err_from(..)`"
    )]
    pub trait Forbidden {
        /// Never callable, since no type implements [`Forbidden`]. It exists so
        /// the poisoned methods can have panic-free bodies.
        fn absurd<T>(self) -> T;
    }
}

/// Poison trait that makes `.internal()` a compile error on errors that
/// already carry their own variants and trace.
///
/// [`WrapInternal`] is blanket-implemented for every
/// `E: std::error::Error + Send + Sync + 'static`, and every `#[error_set]`
/// enum satisfies that bound. Without this trait, `.internal()` on an error set
/// compiles and silently collapses every handleable variant into `Internal`,
/// discarding the accumulated trace — the caller loses the ability to match on
/// `NotFound`, `Conflict`, and the rest, and the FFI boundary reports `-1`.
///
/// This trait declares methods with the same names as [`WrapInternal`]'s, so
/// method resolution finds two candidates and refuses to pick one. Both traits
/// must be in scope for that to happen, which is why they are exported together
/// from [`crate::prelude`] — always import that rather than either trait alone.
///
/// The diagnostics differ by receiver, and both are intentional:
///
/// - `Result<T, E: ErrorSet>` — [`WrapInternal`] also applies, so this is an
///   `E0034` ambiguity naming this trait among the candidates.
/// - `Result<T, Traced<X>>` — [`WrapInternal`] does *not* apply (`Traced` has no
///   `Error` impl), so this resolves here uniquely and reports the
///   [`guard::Forbidden`] message. Without this impl the error would be a
///   confusing "`std::error::Error` is not implemented for `Traced<_>`".
///
/// Use [`ForwardStrict::forward`] instead, or
/// [`SupportsInternalError::internal_with_context`] to collapse on purpose.
///
/// [`SupportsInternalError::internal_with_context`]:
///     crate::internal::SupportsInternalError::internal_with_context
pub trait InternalForbiddenOnErrorSets<T> {
    /// Never callable — see [the trait docs](InternalForbiddenOnErrorSets).
    fn internal(self, context: &str) -> Result<T, Traced<Internal>>
    where
        Self: guard::Forbidden;

    /// Never callable — see [the trait docs](InternalForbiddenOnErrorSets).
    fn internal_with<F>(self, f: F) -> Result<T, Traced<Internal>>
    where
        Self: guard::Forbidden,
        F: FnOnce() -> String;
}

impl<T, E: ErrorSet> InternalForbiddenOnErrorSets<T> for Result<T, E> {
    fn internal(self, _context: &str) -> Result<T, Traced<Internal>>
    where
        Self: guard::Forbidden,
    {
        guard::Forbidden::absurd(self)
    }

    fn internal_with<F>(self, _f: F) -> Result<T, Traced<Internal>>
    where
        Self: guard::Forbidden,
        F: FnOnce() -> String,
    {
        guard::Forbidden::absurd(self)
    }
}

impl<T, X> InternalForbiddenOnErrorSets<T> for Result<T, Traced<X>> {
    fn internal(self, _context: &str) -> Result<T, Traced<Internal>>
    where
        Self: guard::Forbidden,
    {
        guard::Forbidden::absurd(self)
    }

    fn internal_with<F>(self, _f: F) -> Result<T, Traced<Internal>>
    where
        Self: guard::Forbidden,
        F: FnOnce() -> String,
    {
        guard::Forbidden::absurd(self)
    }
}

/// Extension trait on `Result<T, E>` for cross-set error mapping.
///
/// This trait provides:
///
/// - **`try_match`**: Separates `Internal` errors (propagated via `?`) from
///   handleable errors (returned in a `Matched` wrapper for pattern matching).
///
/// - **`map_matched_err`**: Combines `try_match` and `map_err` into one
///   operation — propagates `Internal` and hands handleable variants to a
///   closure that maps them to the target set. Use this for deliberate
///   cross-discrete-type translation (e.g. `NotFound → InvalidPath`) where
///   there is no superset relationship between source and target.
///
/// Each operation requires a context string describing the mapping site;
/// lazy `_with` variants accept closures for deferred context creation.
///
/// For variant-preserving propagation use [`ForwardStrict::forward`] — it
/// requires at compile time that the target declares every variant of the
/// source.
pub trait ResultExt<T, E: ErrorSet> {
    /// Filters the result: propagates `Internal` errors (with context added)
    /// and returns handleable errors in a `Matched` wrapper.
    ///
    /// Use `?` on the returned `Result` to propagate `Traced<Internal>`:
    /// ```
    /// use lore_error_set::prelude::*;
    /// use std::fmt;
    ///
    /// #[derive(Debug, Clone)]
    /// struct NotFound;
    /// impl fmt::Display for NotFound {
    ///     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "not found") }
    /// }
    /// impl std::error::Error for NotFound {}
    /// impl FfiError for NotFound { fn ffi_code(&self) -> i32 { 1 } }
    ///
    /// #[error_set]
    /// enum Errors { NotFound }
    ///
    /// fn fallible_op() -> Result<String, Errors> {
    ///     Err(NotFound.into())
    /// }
    ///
    /// fn caller() -> Result<String, Traced<Internal>> {
    ///     match fallible_op().try_match("loading data")? {
    ///         Ok(v) => Ok(v),
    ///         Err(_matched) => Ok("handled".into()),
    ///     }
    /// }
    ///
    /// assert_eq!(caller().unwrap(), "handled");
    /// ```
    fn try_match(self, context: &str) -> Result<Result<T, E::Matched>, Traced<Internal>>;

    /// Like [`try_match`](ResultExt::try_match), but with a lazily-evaluated
    /// context string. The closure is only called on the error path.
    fn try_match_with<F>(self, f: F) -> Result<Result<T, E::Matched>, Traced<Internal>>
    where
        F: FnOnce() -> String;

    /// Combines [`try_match`](ResultExt::try_match) and `map_err` into a
    /// single operation.
    ///
    /// `Internal` errors are propagated with the context string added to
    /// the trace. Handleable errors are passed to the closure, which maps
    /// them to the target error set.
    fn map_matched_err<Target, F>(self, context: &str, f: F) -> Result<T, Target>
    where
        Target: ErrorSet,
        F: FnOnce(E::Matched) -> Target;
}

impl<T, E: ErrorSet> ResultExt<T, E> for Result<T, E> {
    #[track_caller]
    fn try_match(self, context: &str) -> Result<Result<T, E::Matched>, Traced<Internal>> {
        match self {
            Ok(val) => Ok(Ok(val)),
            Err(err) => {
                let caller = ::std::panic::Location::caller();
                let loc = Location::with_context(
                    caller.file(),
                    caller.line(),
                    caller.column(),
                    Arc::from(context),
                );
                err.into_matched(context, loc).map(Err)
            }
        }
    }

    #[track_caller]
    fn try_match_with<F>(self, f: F) -> Result<Result<T, E::Matched>, Traced<Internal>>
    where
        F: FnOnce() -> String,
    {
        match self {
            Ok(val) => Ok(Ok(val)),
            Err(err) => {
                let caller = ::std::panic::Location::caller();
                let context_str = f();
                let loc = Location::with_context(
                    caller.file(),
                    caller.line(),
                    caller.column(),
                    Arc::from(context_str.as_str()),
                );
                err.into_matched(&context_str, loc).map(Err)
            }
        }
    }

    #[track_caller]
    fn map_matched_err<Target, F>(self, context: &str, f: F) -> Result<T, Target>
    where
        Target: ErrorSet,
        F: FnOnce(E::Matched) -> Target,
    {
        match self {
            Ok(val) => Ok(val),
            Err(err) => {
                // Built here rather than above the match: `Arc::from(context)`
                // copies the string onto the heap, and the success path has no
                // trace to put it on.
                let caller = ::std::panic::Location::caller();
                let loc = Location::with_context(
                    caller.file(),
                    caller.line(),
                    caller.column(),
                    Arc::from(context),
                );
                match err.into_matched(context, loc) {
                    Ok(matched) => Err(f(matched)),
                    Err(traced_internal) => {
                        let (internal, trace) = traced_internal.into_parts();
                        Err(Target::wrap_internal(
                            crate::set::TracedBox::new(
                                Box::new(internal)
                                    as Box<dyn std::error::Error + Send + Sync + 'static>,
                                trace,
                            ),
                            context,
                        ))
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ForwardStrict — single generic strict-forward via HasAll<E::Variants>
// ---------------------------------------------------------------------------

/// Extension trait on `Result<T, E: ErrorSet>` providing the strict
/// [`forward`](ForwardStrict::forward) — requires at compile time that the
/// target declares every variant of the source.
///
/// The variant set is read from [`ErrorSet::Variants`] (auto-emitted by
/// `#[error_set]` as a `Cons`/`Nil` list of inner types). The bound
/// `Target: HasAll<E::Variants>` expands to `Target: Has<V>` for every `V`
/// in the source — a missing target variant is a trait-bound error with the
/// per-variant `#[diagnostic::on_unimplemented]` message attached to [`Has`].
///
/// Auto-implemented for every `Result<T, E: ErrorSet>`. Reach via the
/// prelude:
///
/// ```ignore
/// use lore_error_set::prelude::*;
///
/// fn f() -> Result<(), SyncErrors> {
///     load_branch().forward("loading branch")?;  // compile-time superset check
///     Ok(())
/// }
/// ```
///
/// [`Has`]: crate::set::Has
pub trait ForwardStrict<T, E: ErrorSet> {
    /// Forward the error to `Target`, requiring at compile time that `Target`
    /// declares every variant of the source.
    fn forward<Target>(self, context: &str) -> Result<T, Target>
    where
        Target: ErrorSet + crate::set::HasAll<E::Variants>;

    /// Like [`forward`](Self::forward), but with a lazily-evaluated context
    /// string. The closure is only called on the error path.
    fn forward_with<Target, F>(self, f: F) -> Result<T, Target>
    where
        Target: ErrorSet + crate::set::HasAll<E::Variants>,
        F: FnOnce() -> String;
}

impl<T, E: ErrorSet> ForwardStrict<T, E> for Result<T, E> {
    #[track_caller]
    fn forward<Target>(self, context: &str) -> Result<T, Target>
    where
        Target: ErrorSet + crate::set::HasAll<E::Variants>,
    {
        let caller = ::std::panic::Location::caller();
        self.map_err(move |source| {
            let traced = source.extract_inner();
            match Target::try_from_inner(traced) {
                Ok(mut target) => {
                    target.push_trace(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context),
                    ));
                    target
                }
                Err(mut unmatched) => {
                    unmatched.trace.push(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context),
                    ));
                    Target::wrap_internal(unmatched, context)
                }
            }
        })
    }

    #[track_caller]
    fn forward_with<Target, F>(self, f: F) -> Result<T, Target>
    where
        Target: ErrorSet + crate::set::HasAll<E::Variants>,
        F: FnOnce() -> String,
    {
        let caller = ::std::panic::Location::caller();
        self.map_err(move |source| {
            let context_str = f();
            let traced = source.extract_inner();
            match Target::try_from_inner(traced) {
                Ok(mut target) => {
                    target.push_trace(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context_str.as_str()),
                    ));
                    target
                }
                Err(mut unmatched) => {
                    unmatched.trace.push(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context_str.as_str()),
                    ));
                    Target::wrap_internal(unmatched, &context_str)
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ForwardAny — lenient forward for sources that over-declare
// ---------------------------------------------------------------------------

/// Extension trait providing the lenient
/// [`forward_any`](ForwardAny::forward_any).
///
/// **Prefer [`ForwardStrict::forward`].** Reach for this only when the source
/// set declares variants the call site cannot actually produce, so the strict
/// `HasAll` bound cannot be satisfied without widening the target with
/// unreachable variants.
///
/// The runtime behaviour is identical to [`ForwardStrict::forward`]: every
/// variant the target declares is preserved, and only unmatched ones collapse
/// to `Target::Internal`. The sole difference is the missing
/// `Target: HasAll<E::Variants>` bound, so the guarantee moves from
/// compile time to best effort — which is why the strict form stays the
/// default.
///
/// # When this is the right tool
///
/// `HasAll` is keyed on the source's *declared* variants, not on what a given
/// call site can reach. A function returning a broad set (say one covering
/// every failure in a subsystem) forces every target to declare all of them,
/// even where only a handful are reachable. Widening targets to satisfy that
/// is how a codebase converges on one universal error set: the strict form's
/// only remedy is "widen the target", and under mutual forwarding the sole
/// fixpoint is the union of everything.
///
/// This is the escape valve. It keeps whatever the target genuinely declares
/// without pushing the source's imprecision into it.
///
/// The real fix, where it is affordable, is to narrow the source so
/// [`ForwardStrict::forward`] applies again.
///
/// ```ignore
/// use lore_error_set::prelude::*;
///
/// fn f() -> Result<(), NarrowErrors> {
///     // WideErrors declares 44 variants; this call reaches ~8 of them, and
///     // NarrowErrors declares only what its callers can act on.
///     load_state().forward_any("loading state")?;
///     Ok(())
/// }
/// ```
pub trait ForwardAny<T, E: ErrorSet> {
    /// Forwards every variant `Target` declares, collapsing the rest to
    /// `Target::Internal`. See [the trait docs](ForwardAny).
    fn forward_any<Target>(self, context: &str) -> Result<T, Target>
    where
        Target: ErrorSet;

    /// Like [`forward_any`](Self::forward_any), but with a lazily-evaluated
    /// context string. The closure is only called on the error path.
    fn forward_any_with<Target, F>(self, f: F) -> Result<T, Target>
    where
        Target: ErrorSet,
        F: FnOnce() -> String;
}

impl<T, E: ErrorSet> ForwardAny<T, E> for Result<T, E> {
    #[track_caller]
    fn forward_any<Target>(self, context: &str) -> Result<T, Target>
    where
        Target: ErrorSet,
    {
        let caller = ::std::panic::Location::caller();
        self.map_err(move |source| {
            let traced = source.extract_inner();
            match Target::try_from_inner(traced) {
                Ok(mut target) => {
                    target.push_trace(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context),
                    ));
                    target
                }
                Err(mut unmatched) => {
                    unmatched.trace.push(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context),
                    ));
                    Target::wrap_internal(unmatched, context)
                }
            }
        })
    }

    #[track_caller]
    fn forward_any_with<Target, F>(self, f: F) -> Result<T, Target>
    where
        Target: ErrorSet,
        F: FnOnce() -> String,
    {
        let caller = ::std::panic::Location::caller();
        self.map_err(move |source| {
            let context_str = f();
            let traced = source.extract_inner();
            match Target::try_from_inner(traced) {
                Ok(mut target) => {
                    target.push_trace(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context_str.as_str()),
                    ));
                    target
                }
                Err(mut unmatched) => {
                    unmatched.trace.push(Location::with_context(
                        caller.file(),
                        caller.line(),
                        caller.column(),
                        Arc::from(context_str.as_str()),
                    ));
                    Target::wrap_internal(unmatched, &context_str)
                }
            }
        })
    }
}
