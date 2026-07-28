// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Feature-gated tracing types for error location tracking.
//!
//! When the `track-locations` feature is enabled, [`Trace`] stores a bounded
//! stack of [`Location`]s and [`Traced<E>`] wraps an error with its trace.
//!
//! When the feature is disabled, both types are zero-sized, providing no
//! runtime overhead.

use std::fmt;
use std::ops::Deref;

use crate::location::Location;

/// Maximum number of trace entries before overflow truncation.
pub const MAX_TRACE_DEPTH: usize = 16;

// ---------------------------------------------------------------------------
// Trace — feature-gated
// ---------------------------------------------------------------------------

/// A bounded stack of source locations captured during error creation and
/// forwarding.
///
/// When `track-locations` is enabled this stores up to [`MAX_TRACE_DEPTH`]
/// locations in a heap-allocated `Vec` (pre-allocated with capacity 8).
/// When the depth limit is exceeded, the oldest entries are dropped and the
/// `overflow` flag is set.
///
/// When `track-locations` is disabled this is a zero-sized type.
#[cfg(feature = "track-locations")]
#[derive(Debug, Clone)]
pub struct Trace {
    locations: Vec<Location>,
    overflow: bool,
}

#[cfg(feature = "track-locations")]
impl Trace {
    /// Creates an empty trace.
    #[inline]
    pub fn new() -> Self {
        Self {
            locations: Vec::with_capacity(8),
            overflow: false,
        }
    }

    /// Pushes a location onto the trace. If the trace has reached
    /// [`MAX_TRACE_DEPTH`], the oldest entry is removed and the overflow
    /// flag is set.
    #[inline]
    pub fn push(&mut self, location: Location) {
        if self.locations.len() >= MAX_TRACE_DEPTH {
            self.locations.remove(0);
            self.overflow = true;
        }
        self.locations.push(location);
    }

    /// Returns `true` if the trace has overflowed (oldest entries were
    /// dropped).
    #[inline]
    pub fn has_overflow(&self) -> bool {
        self.overflow
    }

    /// Returns the recorded locations.
    #[inline]
    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    /// Returns the number of recorded locations.
    #[inline]
    pub fn len(&self) -> usize {
        self.locations.len()
    }

    /// Returns `true` if no locations have been recorded.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.locations.is_empty()
    }
}

#[cfg(feature = "track-locations")]
impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "track-locations")]
impl fmt::Display for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.overflow {
            writeln!(f, "  ... (earlier entries truncated)")?;
        }
        for loc in &self.locations {
            writeln!(f, "  at {loc}")?;
        }
        Ok(())
    }
}

// -- Disabled variant -------------------------------------------------------

/// A zero-sized trace type used when `track-locations` is disabled.
#[cfg(not(feature = "track-locations"))]
#[derive(Debug, Clone, Default)]
pub struct Trace;

#[cfg(not(feature = "track-locations"))]
impl Trace {
    /// Creates an empty (zero-sized) trace.
    #[inline]
    pub fn new() -> Self {
        Self
    }

    /// No-op when tracing is disabled.
    #[inline]
    pub fn push(&mut self, _location: Location) {}

    /// Always returns `false` when tracing is disabled.
    #[inline]
    pub fn has_overflow(&self) -> bool {
        false
    }

    /// Always returns an empty slice when tracing is disabled.
    #[inline]
    pub fn locations(&self) -> &[Location] {
        &[]
    }

    /// Always returns 0 when tracing is disabled.
    #[inline]
    pub fn len(&self) -> usize {
        0
    }

    /// Always returns `true` when tracing is disabled.
    #[inline]
    pub fn is_empty(&self) -> bool {
        true
    }
}

#[cfg(not(feature = "track-locations"))]
impl fmt::Display for Trace {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HasTrace — trait access to an error's trace
// ---------------------------------------------------------------------------

/// Trait exposing an error's [`Trace`] through a trait bound.
///
/// Each `#[error_set]` enum has an inherent `trace()` method, but no shared
/// trait bound exposes it. Generic code that holds an error behind a bound
/// (rather than a concrete type) needs trait access to the trace. `#[error_set]`
/// implements this trait for every generated enum, delegating to that inherent
/// method, so a generic function can bound on `HasTrace` and read the trace.
pub trait HasTrace {
    /// Returns a reference to this error's trace.
    fn trace(&self) -> &Trace;
}

// ---------------------------------------------------------------------------
// Send + Sync assertions for Trace
// ---------------------------------------------------------------------------
fn _assert_trace_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<Trace>();
}

// ---------------------------------------------------------------------------
// Traced<E> — feature-gated
// ---------------------------------------------------------------------------

/// A wrapper that pairs an error with its [`Trace`].
///
/// When `track-locations` is enabled, this stores both the inner error and a
/// trace of source locations. When disabled, it is a zero-cost newtype.
#[cfg(feature = "track-locations")]
#[derive(Debug, Clone)]
pub struct Traced<E> {
    inner: E,
    trace: Trace,
}

#[cfg(feature = "track-locations")]
impl<E> Traced<E> {
    /// Creates a new `Traced` wrapper with the given error and trace.
    #[inline]
    pub fn new(inner: E, trace: Trace) -> Self {
        Self { inner, trace }
    }

    /// Returns a reference to the trace.
    #[inline]
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// Returns a mutable reference to the trace.
    #[inline]
    pub fn trace_mut(&mut self) -> &mut Trace {
        &mut self.trace
    }

    /// Consumes the wrapper and returns the inner error.
    #[inline]
    pub fn into_inner(self) -> E {
        self.inner
    }

    /// Consumes the wrapper and returns both the inner error and the trace.
    #[inline]
    pub fn into_parts(self) -> (E, Trace) {
        (self.inner, self.trace)
    }
}

#[cfg(feature = "track-locations")]
impl<E> Deref for Traced<E> {
    type Target = E;

    #[inline]
    fn deref(&self) -> &E {
        &self.inner
    }
}

#[cfg(feature = "track-locations")]
impl<E: fmt::Display> fmt::Display for Traced<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

// -- Disabled variant -------------------------------------------------------

/// A zero-cost newtype wrapper used when `track-locations` is disabled.
#[cfg(not(feature = "track-locations"))]
#[derive(Debug, Clone)]
pub struct Traced<E>(pub E);

#[cfg(not(feature = "track-locations"))]
impl<E> Traced<E> {
    /// Creates a new `Traced` wrapper. The trace argument is ignored when
    /// tracing is disabled.
    #[inline]
    pub fn new(inner: E, _trace: Trace) -> Self {
        Self(inner)
    }

    /// Returns a reference to the (empty) trace.
    #[inline]
    pub fn trace(&self) -> &Trace {
        &Trace
    }

    /// Returns a mutable reference to a no-op trace.
    ///
    /// Note: When tracing is disabled, mutations to the returned reference
    /// have no effect. This method exists for API compatibility.
    ///
    /// Uses a leaked allocation for a stable `&mut Trace`. Since `Trace` is a
    /// ZST when tracing is disabled, this performs no actual allocation.
    #[inline]
    pub fn trace_mut(&mut self) -> &mut Trace {
        // Trace is a ZST, so Box::leak allocates nothing (zero-sized).
        Box::leak(Box::new(Trace))
    }

    /// Consumes the wrapper and returns the inner error.
    #[inline]
    pub fn into_inner(self) -> E {
        self.0
    }

    /// Consumes the wrapper and returns both the inner error and the trace.
    #[inline]
    pub fn into_parts(self) -> (E, Trace) {
        (self.0, Trace)
    }
}

#[cfg(not(feature = "track-locations"))]
impl<E> Deref for Traced<E> {
    type Target = E;

    #[inline]
    fn deref(&self) -> &E {
        &self.0
    }
}

#[cfg(not(feature = "track-locations"))]
impl<E: fmt::Display> fmt::Display for Traced<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// ChainError — convert between discrete error types preserving trace
// ---------------------------------------------------------------------------

/// Chain a new discrete error type onto an existing traced error, preserving
/// the trace from the source and adding a new location entry with context.
///
/// Use this when mapping between different discrete error types across error
/// set boundaries (e.g., `NotFound` → `AddressNotFound`) without losing the
/// trace of where the original error came from.
///
/// ```ignore
/// Err(MatchedProtocolError::NotFound(traced)) => {
///     return Err(AddressNotFound { address }
///         .chain_err(traced, "fragment not found on remote")
///         .into());
/// }
/// ```
pub trait ChainError: Sized {
    /// Chains `self` onto the trace from `source`, adding `context` and the
    /// current caller location to the trace.
    #[track_caller]
    fn chain_err<E>(self, source: Traced<E>, context: &str) -> Traced<Self>;

    /// Like [`chain_err`](ChainError::chain_err), but with a lazily evaluated
    /// context string (only called when `track-locations` is enabled).
    #[track_caller]
    fn chain_err_with<E>(self, source: Traced<E>, context: impl FnOnce() -> String)
        -> Traced<Self>;
}

#[cfg(feature = "track-locations")]
impl<T> ChainError for T {
    #[track_caller]
    fn chain_err<E>(self, source: Traced<E>, context: &str) -> Traced<Self> {
        let (_, mut trace) = source.into_parts();
        let caller = std::panic::Location::caller();
        trace.push(Location::with_context(
            caller.file(),
            caller.line(),
            caller.column(),
            context.into(),
        ));
        Traced::new(self, trace)
    }

    #[track_caller]
    fn chain_err_with<E>(
        self,
        source: Traced<E>,
        context: impl FnOnce() -> String,
    ) -> Traced<Self> {
        let (_, mut trace) = source.into_parts();
        let caller = std::panic::Location::caller();
        trace.push(Location::with_context(
            caller.file(),
            caller.line(),
            caller.column(),
            context().into(),
        ));
        Traced::new(self, trace)
    }
}

#[cfg(not(feature = "track-locations"))]
impl<T> ChainError for T {
    #[track_caller]
    fn chain_err<E>(self, _source: Traced<E>, _context: &str) -> Traced<Self> {
        Traced::new(self, Trace)
    }

    #[track_caller]
    fn chain_err_with<E>(
        self,
        _source: Traced<E>,
        _context: impl FnOnce() -> String,
    ) -> Traced<Self> {
        Traced::new(self, Trace)
    }
}

// ---------------------------------------------------------------------------
// Send + Sync assertions for Traced
// ---------------------------------------------------------------------------
fn _assert_traced_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<Traced<String>>();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // -- Trace tests (feature-gated behavior) --------------------------------

    #[test]
    fn trace_push_and_locations() {
        let mut trace = Trace::new();
        trace.push(Location::new("test.rs", 1, 1));

        #[cfg(feature = "track-locations")]
        {
            assert_eq!(trace.len(), 1);
            assert!(!trace.is_empty());
            assert!(!trace.has_overflow());
            assert_eq!(trace.locations()[0], Location::new("test.rs", 1, 1));
        }

        #[cfg(not(feature = "track-locations"))]
        {
            assert_eq!(trace.len(), 0);
            assert!(trace.is_empty());
            assert!(!trace.has_overflow());
        }
    }

    #[cfg(feature = "track-locations")]
    #[test]
    fn trace_overflow_at_max_depth() {
        let mut trace = Trace::new();

        // Push exactly MAX_TRACE_DEPTH locations — should not overflow.
        for i in 0..MAX_TRACE_DEPTH {
            trace.push(Location::new("file.rs", i as u32, 0));
        }
        assert_eq!(trace.len(), MAX_TRACE_DEPTH);
        assert!(!trace.has_overflow());

        // Push one more — should trigger overflow and remain at MAX_TRACE_DEPTH.
        trace.push(Location::new("file.rs", 999, 0));
        assert_eq!(trace.len(), MAX_TRACE_DEPTH);
        assert!(trace.has_overflow());

        // The oldest entry (line 0) should have been removed.
        assert_eq!(trace.locations()[0].line, 1);
        // The newest entry should be at the end.
        assert_eq!(trace.locations()[MAX_TRACE_DEPTH - 1].line, 999);
    }

    #[cfg(feature = "track-locations")]
    #[test]
    fn trace_overflow_continues_after_multiple_pushes() {
        let mut trace = Trace::new();

        // Push MAX_TRACE_DEPTH + 5 locations.
        for i in 0..(MAX_TRACE_DEPTH + 5) {
            trace.push(Location::new("file.rs", i as u32, 0));
        }

        assert_eq!(trace.len(), MAX_TRACE_DEPTH);
        assert!(trace.has_overflow());

        // First remaining entry should be line 5 (entries 0-4 dropped).
        assert_eq!(trace.locations()[0].line, 5);
    }

    // -- Traced tests -------------------------------------------------------

    #[test]
    fn traced_deref() {
        let traced = Traced::new(42_i32, Trace::new());
        // Deref should give access to the inner value.
        assert_eq!(*traced, 42);
    }

    #[test]
    fn traced_into_inner() {
        let traced = Traced::new(String::from("hello"), Trace::new());
        let inner = traced.into_inner();
        assert_eq!(inner, "hello");
    }

    #[test]
    fn traced_into_parts() {
        let mut trace = Trace::new();
        trace.push(Location::new("test.rs", 10, 5));

        let traced = Traced::new(100_u32, trace);
        let (val, _trace) = traced.into_parts();
        assert_eq!(val, 100);

        #[cfg(feature = "track-locations")]
        {
            assert_eq!(_trace.len(), 1);
        }
    }

    #[test]
    fn traced_display() {
        let traced = Traced::new(String::from("some error"), Trace::new());
        assert_eq!(traced.to_string(), "some error");
    }

    // -- ChainError tests ---------------------------------------------------

    #[test]
    fn chain_err_preserves_inner() {
        let source = Traced::new(42_i32, Trace::new());
        let chained = String::from("new error").chain_err(source, "converting");
        assert_eq!(*chained, "new error");
    }

    #[cfg(feature = "track-locations")]
    #[test]
    fn chain_err_preserves_and_extends_trace() {
        let mut trace = Trace::new();
        trace.push(Location::new("origin.rs", 10, 1));
        let source = Traced::new(42_i32, trace);

        let chained = String::from("new error").chain_err(source, "converting type");

        // Should have the original location plus the chain point
        assert_eq!(chained.trace().len(), 2);
        assert_eq!(chained.trace().locations()[0].line, 10);
        assert_eq!(
            chained.trace().locations()[1].context(),
            Some("converting type")
        );
    }

    #[test]
    fn chain_err_with_lazy_context() {
        let source = Traced::new(42_i32, Trace::new());
        let chained =
            String::from("new error").chain_err_with(source, || format!("lazy context {}", 123));
        assert_eq!(*chained, "new error");
    }
}
