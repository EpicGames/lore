// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![cfg(feature = "track-locations")]
//! Integration tests for trace capture with `track-locations` feature enabled.
//!
//! Covers spec acceptance tests:
//! - #6:  Trace capture across forwards
//! - #14 (enabled): Feature-gated tracing — trace is non-empty
//! - #16: `as_*_traced()` methods return `Traced` with accessible trace
//! - #18: Trace preservation across direct mapping

use std::error::Error;
use std::fmt;

use lore_error_set::error_set;
use lore_error_set::prelude::*;
use lore_error_set::FfiError;
use lore_error_set::Location;
use lore_error_set::Traced;

// ---------------------------------------------------------------------------
// Discrete error types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NotFound {
    resource: String,
}

impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not found: {}", self.resource)
    }
}

impl Error for NotFound {}

impl FfiError for NotFound {
    fn ffi_code(&self) -> i32 {
        10
    }
}

#[derive(Debug, Clone)]
pub struct Timeout {
    duration_ms: u64,
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "timeout after {}ms", self.duration_ms)
    }
}

impl Error for Timeout {}

impl FfiError for Timeout {
    fn ffi_code(&self) -> i32 {
        11
    }
}

// ---------------------------------------------------------------------------
// Error sets: SetA { NotFound, Timeout }, SetB { NotFound }
// ---------------------------------------------------------------------------

#[error_set]
pub enum SetA {
    NotFound,
    Timeout,
}

#[error_set]
pub enum SetB {
    NotFound,
    Timeout,
}

// Third set for multi-hop testing
#[error_set]
pub enum SetC {
    NotFound,
    Timeout,
}

// ---------------------------------------------------------------------------
// Spec test #6: Trace capture across forwards
// ---------------------------------------------------------------------------

#[test]
fn trace_capture_across_forwards() {
    // Create NotFound error in SetA (captures creation site).
    let result_a: Result<(), SetA> = Err(NotFound {
        resource: "doc".into(),
    }
    .into());

    // Forward to SetB via ResultExt::forward.
    let result_b: Result<(), SetB> = result_a.forward("forwarding");

    let err_b = result_b.unwrap_err();
    assert!(err_b.is_not_found(), "should be NotFound in SetB");

    // Access the traced wrapper and check the trace.
    let traced = err_b
        .as_not_found_traced()
        .expect("as_not_found_traced should return Some");
    let trace = traced.trace();

    // The trace should have 2 locations: creation site + forward site.
    assert_eq!(
        trace.len(),
        2,
        "trace should have 2 locations (creation + forward), got {}",
        trace.len()
    );

    // Verify trace locations have non-zero line numbers.
    for loc in trace.locations() {
        assert!(
            loc.line > 0,
            "trace location should have non-zero line number"
        );
    }

    // Verify trace locations point to THIS file, not ext.rs internals.
    for loc in trace.locations() {
        assert!(
            loc.file.contains("tracing.rs"),
            "trace location should point to caller file (tracing.rs), got: {}:{}",
            loc.file,
            loc.line
        );
    }

    // The forward hop should have context.
    let forward_loc = &trace.locations()[1];
    assert_eq!(
        forward_loc.context(),
        Some("forwarding"),
        "forward hop should carry context"
    );
}

// ---------------------------------------------------------------------------
// Spec test #14 (enabled): Feature-gated tracing — trace is non-empty
// ---------------------------------------------------------------------------

#[test]
fn trace_is_non_empty_when_feature_enabled() {
    // Create an error and check trace is NOT empty.
    let err: SetA = NotFound {
        resource: "item".into(),
    }
    .into();

    let traced = err
        .as_not_found_traced()
        .expect("as_not_found_traced should return Some");
    let trace = traced.trace();

    // Trace length should be at least 1.
    assert!(
        !trace.is_empty(),
        "trace should not be empty when track-locations is enabled"
    );
    assert!(
        !trace.is_empty(),
        "trace len should be >= 1, got {}",
        trace.len()
    );

    // Verify Traced::trace() returns a Trace with len() >= 1.
    let locations = trace.locations();
    assert!(!locations.is_empty(), "trace locations should not be empty");
}

// ---------------------------------------------------------------------------
// Spec test #16: `as_*_traced()` methods
// ---------------------------------------------------------------------------

#[test]
fn as_variant_traced_returns_traced_with_accessible_trace() {
    // Construct a NotFound error in SetA.
    let err: SetA = NotFound {
        resource: "file.txt".into(),
    }
    .into();

    // Call .as_not_found_traced() - should return Some(&Traced<NotFound>).
    let traced: &Traced<NotFound> = err
        .as_not_found_traced()
        .expect("as_not_found_traced should return Some");

    // Access .trace() on the returned Traced - should be non-empty.
    let trace = traced.trace();
    assert!(
        !trace.is_empty(),
        "trace from as_not_found_traced() should be non-empty"
    );

    // Verify the inner value is accessible via Deref.
    assert_eq!(traced.resource, "file.txt");

    // Also verify .as_not_found() returns Some(&NotFound) (inner only, no trace access).
    let inner: &NotFound = err.as_not_found().expect("as_not_found should return Some");
    assert_eq!(inner.resource, "file.txt");
}

#[test]
fn as_variant_traced_returns_none_for_wrong_variant() {
    let err: SetA = Timeout { duration_ms: 100 }.into();

    // as_not_found_traced on a Timeout variant should return None.
    assert!(
        err.as_not_found_traced().is_none(),
        "as_not_found_traced should return None for Timeout variant"
    );

    // But as_timeout_traced should return Some.
    let traced = err
        .as_timeout_traced()
        .expect("as_timeout_traced should return Some");
    assert!(!traced.trace().is_empty());
}

// ---------------------------------------------------------------------------
// Spec test #18: Trace preservation across direct mapping
// ---------------------------------------------------------------------------

#[test]
fn trace_preserved_across_direct_mapping() {
    // Create NotFound error in SetA (which captures creation trace).
    let err_a: SetA = NotFound {
        resource: "record".into(),
    }
    .into();

    // Verify the trace is captured in SetA.
    let traced_a = err_a
        .as_not_found_traced()
        .expect("should have traced in SetA");
    let trace_len_a = traced_a.trace().len();
    assert!(
        trace_len_a >= 1,
        "SetA trace should have at least 1 location"
    );

    // Forward to SetB (which also has NotFound - direct mapping).
    let result_a: Result<(), SetA> = Err(err_a);
    let result_b: Result<(), SetB> = result_a.forward("direct mapping test");

    let err_b = result_b.unwrap_err();
    assert!(err_b.is_not_found(), "should be NotFound in SetB");

    // Verify the trace in SetB's NotFound variant has 2 entries.
    let traced_b = err_b
        .as_not_found_traced()
        .expect("should have traced in SetB");
    let trace = traced_b.trace();

    assert_eq!(
        trace.len(),
        2,
        "trace should have 2 locations (creation + forward), got {}",
        trace.len()
    );

    // Verify trace locations point to THIS file, not ext.rs internals.
    for loc in trace.locations() {
        assert!(
            loc.file.contains("tracing.rs"),
            "trace location should point to caller file (tracing.rs), got: {}:{}",
            loc.file,
            loc.line
        );
    }

    // The forward hop should have context.
    let forward_loc = &trace.locations()[1];
    assert_eq!(
        forward_loc.context(),
        Some("direct mapping test"),
        "forward hop should carry context"
    );
}

#[test]
fn trace_preserved_when_timeout_maps_directly() {
    // Create Timeout error in SetA.
    let result_a: Result<(), SetA> = Err(Timeout { duration_ms: 5000 }.into());

    // SetB declares Timeout, so the strict forward maps it directly.
    let result_b: Result<(), SetB> = result_a.forward("timeout forwarding");

    let err_b = result_b.unwrap_err();
    assert!(err_b.is_timeout(), "Timeout should map directly in SetB");

    let traced = err_b
        .as_timeout_traced()
        .expect("should have traced timeout in SetB");
    let trace = traced.trace();
    assert_eq!(
        trace.locations().last().and_then(|loc| loc.context()),
        Some("timeout forwarding"),
        "forward hop should carry context"
    );
}

// ---------------------------------------------------------------------------
// Multi-hop trace test (spec acceptance test #6)
// ---------------------------------------------------------------------------

#[test]
fn trace_three_hops() {
    // Create NotFound error in SetA.
    let result_a: Result<(), SetA> = Err(NotFound {
        resource: "multi".into(),
    }
    .into());

    // Forward SetA -> SetB ("hop 1")
    let result_b: Result<(), SetB> = result_a.forward("hop 1");

    // Forward SetB -> SetC ("hop 2")
    let result_c: Result<(), SetC> = result_b.forward("hop 2");

    let err_c = result_c.unwrap_err();
    assert!(err_c.is_not_found(), "should be NotFound in SetC");

    let traced = err_c
        .as_not_found_traced()
        .expect("should have traced in SetC");
    let trace = traced.trace();

    // 3 entries: creation + hop 1 + hop 2
    assert_eq!(
        trace.len(),
        3,
        "trace should have 3 locations (creation + 2 forwards), got {}",
        trace.len()
    );

    // Verify all locations point to THIS file, not ext.rs internals.
    for (i, loc) in trace.locations().iter().enumerate() {
        assert!(
            loc.file.contains("tracing.rs"),
            "hop {i} location should point to caller file (tracing.rs), got: {}:{}",
            loc.file,
            loc.line
        );
    }

    // Verify contexts on each hop.
    assert_eq!(
        trace.locations()[0].context(),
        None,
        "creation has no context"
    );
    assert_eq!(
        trace.locations()[1].context(),
        Some("hop 1"),
        "first forward has context"
    );
    assert_eq!(
        trace.locations()[2].context(),
        Some("hop 2"),
        "second forward has context"
    );
}

// ---------------------------------------------------------------------------
// Caller location test: forward() of an Internal source must NOT point to ext.rs
// ---------------------------------------------------------------------------

#[test]
fn forward_internal_traces_caller_not_ext() {
    // Construct an Internal source in SetA and forward it through SetB.
    let io_err = std::io::Error::other("disk failure");
    let traced_box = lore_error_set::TracedBox::new(Box::new(io_err), lore_error_set::Trace::new());
    let result_a: Result<(), SetA> = Err(SetA::wrap_internal(traced_box, "source internal"));
    let result_b: Result<(), SetB> = result_a.forward("forwarding internal");

    let err_b = result_b.unwrap_err();
    assert!(err_b.is_internal());

    let trace = err_b.trace();
    assert!(
        !trace.is_empty(),
        "Internal from forward should have trace entries"
    );

    // Every trace location must point to THIS test file, never to ext.rs.
    for loc in trace.locations() {
        assert!(
            !loc.file.contains("ext.rs"),
            "trace should not point to ext.rs internals, got: {}:{}",
            loc.file,
            loc.line
        );
        assert!(
            loc.file.contains("tracing.rs"),
            "trace should point to caller file (tracing.rs), got: {}:{}",
            loc.file,
            loc.line
        );
    }
}

// ---------------------------------------------------------------------------
// .trace() accessor on error-set enum
// ---------------------------------------------------------------------------

#[test]
fn trace_on_error_set_user_variant() {
    // Create NotFound error in SetA.
    let result_a: Result<(), SetA> = Err(NotFound {
        resource: "doc".into(),
    }
    .into());

    // Forward to SetB to add a second trace hop.
    let result_b: Result<(), SetB> = result_a.forward("hop");
    let err_b = result_b.unwrap_err();

    // Call .trace() directly on the error-set enum value.
    let trace = err_b.trace();
    assert!(!trace.is_empty(), "trace should be non-empty");
    assert!(
        !trace.locations().is_empty(),
        "trace locations should be non-empty"
    );
}

#[test]
fn trace_on_error_set_internal_variant() {
    // Construct an Internal source in SetA, forward to SetB.
    let io_err = std::io::Error::other("disk failure");
    let traced_box = lore_error_set::TracedBox::new(Box::new(io_err), lore_error_set::Trace::new());
    let result_a: Result<(), SetA> = Err(SetA::wrap_internal(traced_box, "source internal"));
    let result_b: Result<(), SetB> = result_a.forward("wrapping");
    let err_b = result_b.unwrap_err();

    assert!(err_b.is_internal());

    // Call .trace() directly on the Internal variant.
    let trace = err_b.trace();
    assert!(
        !trace.is_empty(),
        "Internal variant trace should be non-empty"
    );
}

// ---------------------------------------------------------------------------
// Trace Display format test
// ---------------------------------------------------------------------------

#[test]
fn trace_display_format_with_context() {
    let result_a: Result<(), SetA> = Err(NotFound {
        resource: "fmt".into(),
    }
    .into());
    let result_b: Result<(), SetB> = result_a.forward("loading config");

    let err_b = result_b.unwrap_err();
    let traced = err_b.as_not_found_traced().unwrap();
    let display = format!("{}", traced.trace());

    // The display output should contain the context pattern.
    assert!(
        display.contains(" - loading config"),
        "trace display should contain context pattern, got: {display}"
    );
}

// ---------------------------------------------------------------------------
// chain_err — convert between discrete types, preserving trace
// ---------------------------------------------------------------------------

/// `chain_err` takes a `Traced<E>` extracted from an error set variant and
/// chains a new discrete error onto its trace. The result is a `Traced<NewError>`
/// whose trace contains every location from the original plus a new entry at
/// the chain call site.
#[test]
fn chain_err_carries_originating_trace() {
    // Create a NotFound in SetA — From<NotFound> captures the creation site.
    let source: SetA = NotFound {
        resource: "db.json".into(),
    }
    .into();
    let source_len = source.trace().len(); // 1: the From<> call above

    // Destructure to get the Traced<NotFound> directly.
    let SetA::NotFound(traced_nf) = source else {
        unreachable!()
    };

    // Chain a Timeout onto the existing trace.
    let chained: Traced<Timeout> = Timeout {
        duration_ms: 30_000,
    }
    .chain_err(traced_nf, "reading config file");

    let trace = chained.trace();
    // The chain adds exactly one entry.
    assert_eq!(trace.len(), source_len + 1);

    // The new entry carries the context and points to this file.
    let last = trace.locations().last().unwrap();
    assert_eq!(last.context(), Some("reading config file"));
    assert!(
        last.file.contains("tracing.rs"),
        "chain site should be in this file, got: {}",
        last.file
    );
}

#[test]
fn chain_err_display_shows_originating_and_chain_sites() {
    let source: SetA = NotFound {
        resource: "config.toml".into(),
    }
    .into();
    let SetA::NotFound(traced_nf) = source else {
        unreachable!()
    };

    let chained: Traced<Timeout> =
        Timeout { duration_ms: 1_000 }.chain_err(traced_nf, "loading settings");

    let display = format!("{}", chained.trace());
    // Emit the full trace so it is visible under `cargo test -- --nocapture`.
    println!("chain_err trace output:\n{display}");

    // Two entries: original creation site + chain site.
    assert_eq!(
        display.matches("  at ").count(),
        2,
        "expected 2 trace entries, got:\n{display}"
    );
    // Both entries reference this file.
    let at_lines: Vec<&str> = display.lines().filter(|l| l.contains("  at ")).collect();
    for line in &at_lines {
        assert!(
            line.contains("tracing.rs"),
            "entry should reference tracing.rs: {line}"
        );
    }
    // The chain entry carries its context.
    assert!(
        display.contains("loading settings"),
        "chain context missing from display:\n{display}"
    );
    // The creation entry has no context — it is formatted as file:line:col.
    assert!(
        !at_lines[0].contains(" - "),
        "creation entry should have no context, got: {}",
        at_lines[0]
    );
    // The chain entry has a context after " - ".
    assert!(
        at_lines[1].contains(" - loading settings"),
        "chain entry should show context, got: {}",
        at_lines[1]
    );
}

// ---------------------------------------------------------------------------
// chain_err_from — same as chain_err but accepts an error-set enum directly
// ---------------------------------------------------------------------------

/// `chain_err_from` accepts the error-set enum itself (anything implementing
/// `HasTrace`) so the caller does not need to pattern-match first.
#[test]
fn chain_err_from_carries_trace_from_error_set() {
    let source: SetA = NotFound {
        resource: "users.db".into(),
    }
    .into();
    let source_len = source.trace().len(); // 1

    // chain_err_from takes the enum directly — no destructuring.
    let chained: Traced<Timeout> =
        Timeout { duration_ms: 5_000 }.chain_err_from(source, "querying database");

    let trace = chained.trace();
    assert_eq!(trace.len(), source_len + 1);

    let last = trace.locations().last().unwrap();
    assert_eq!(last.context(), Some("querying database"));
    assert!(last.file.contains("tracing.rs"));
}

#[test]
fn chain_err_from_display_shows_originating_and_chain_sites() {
    let source: SetA = NotFound {
        resource: "cache".into(),
    }
    .into();

    let chained: Traced<Timeout> =
        Timeout { duration_ms: 200 }.chain_err_from(source, "warming cache");

    let display = format!("{}", chained.trace());
    println!("chain_err_from trace output:\n{display}");

    assert_eq!(
        display.matches("  at ").count(),
        2,
        "expected 2 trace entries, got:\n{display}"
    );
    assert!(display.contains("tracing.rs"));
    assert!(display.contains("warming cache"));
}

/// When the source error set already carries a multi-hop trace (e.g. from a
/// prior `forward`), `chain_err_from` accumulates onto it rather than
/// starting fresh.
#[test]
fn chain_err_from_accumulates_onto_multi_hop_trace() {
    // Build a 2-entry trace: creation + forward.
    let result_a: Result<(), SetA> = Err(NotFound {
        resource: "x".into(),
    }
    .into());
    let result_b: Result<(), SetB> = result_a.forward("hop 1");
    let source = result_b.unwrap_err(); // trace len == 2

    assert_eq!(source.trace().len(), 2);

    let chained: Traced<Timeout> = Timeout { duration_ms: 100 }.chain_err_from(source, "hop 2");

    let trace = chained.trace();
    assert_eq!(trace.len(), 3, "creation + hop1 + hop2");

    let locs = trace.locations();
    assert_eq!(locs[0].context(), None, "creation has no context");
    assert_eq!(locs[1].context(), Some("hop 1"));
    assert_eq!(locs[2].context(), Some("hop 2"));

    let display = format!("{trace}");
    println!("multi-hop chain_err_from trace:\n{display}");

    assert_eq!(display.matches("  at ").count(), 3);
    assert!(display.contains("hop 1"));
    assert!(display.contains("hop 2"));
}

#[test]
fn internal_helper_traces_caller() {
    let before_error = ::std::panic::Location::caller();
    let before_error = Location::new(
        before_error.file(),
        before_error.line(),
        before_error.column(),
    );
    let a = SetA::internal("Test message");
    let after_error = ::std::panic::Location::caller();
    let after_error = Location::new(after_error.file(), after_error.line(), after_error.column());

    assert_eq!(a.trace().locations().len(), 1);
    let trace_location = a.trace().locations().first().unwrap();

    assert_eq!(trace_location.file, before_error.file);

    assert!(
        trace_location.line > before_error.line,
        "{} > {}",
        trace_location.line,
        before_error.line
    );
    assert!(
        after_error.line > trace_location.line,
        "{} > {}",
        after_error.line,
        trace_location.line
    );
}

#[test]
fn internal_with_context_helper_traces_caller() {
    let before_error = ::std::panic::Location::caller();
    let before_error = Location::new(
        before_error.file(),
        before_error.line(),
        before_error.column(),
    );
    let io_err = std::io::Error::other("disk failure");
    let a = SetA::internal_with_context(io_err, "test context");
    let after_error = ::std::panic::Location::caller();
    let after_error = Location::new(after_error.file(), after_error.line(), after_error.column());

    assert_eq!(a.trace().locations().len(), 1);
    let trace_location = a.trace().locations().first().unwrap();

    assert_eq!(trace_location.file, before_error.file);

    assert!(
        trace_location.line > before_error.line,
        "{} > {}",
        trace_location.line,
        before_error.line
    );
    assert!(
        after_error.line > trace_location.line,
        "{} > {}",
        after_error.line,
        trace_location.line
    );
}
