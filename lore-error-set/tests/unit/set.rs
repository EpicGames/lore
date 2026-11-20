// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use std::error::Error;
use std::fmt;

use lore_error_set::location::Location;
use lore_error_set::set::*;
use lore_error_set::traced::Trace;
use lore_error_set::traced::Traced;

/// A simple test error type.
#[derive(Debug)]
struct TestError(String);

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "test error: {}", self.0)
    }
}

impl Error for TestError {}

#[test]
fn traced_box_from_traced() {
    let mut trace = Trace::new();
    trace.push(Location::new("test.rs", 1, 1));

    let traced = Traced::new(TestError("hello".into()), trace);
    let traced_box = TracedBox::from_traced(traced);

    assert_eq!(traced_box.inner.to_string(), "test error: hello");

    #[cfg(feature = "track-locations")]
    {
        assert_eq!(traced_box.trace.len(), 1);
        assert_eq!(traced_box.trace.locations()[0].line, 1);
    }
}

#[test]
fn traced_box_display() {
    let err = TestError("display test".into());
    let tb = TracedBox::new(Box::new(err), Trace::new());
    assert_eq!(tb.to_string(), "test error: display test");
}

#[test]
fn traced_box_debug() {
    let err = TestError("debug test".into());
    let tb = TracedBox::new(Box::new(err), Trace::new());
    let debug_str = format!("{tb:?}");
    assert!(debug_str.contains("TracedBox"));
}

#[test]
fn traced_box_downcast() {
    let err = TestError("downcast test".into());
    let tb = TracedBox::new(Box::new(err), Trace::new());

    // Should be able to downcast back to TestError.
    let inner = tb.inner.downcast::<TestError>().expect("should downcast");
    assert_eq!(inner.0, "downcast test");
}

/// A refusal, the one variant of [`Refusals`].
#[derive(Debug)]
struct Refused;

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "refused")
    }
}

impl Error for Refused {}

impl lore_error_set::FfiError for Refused {
    fn ffi_code(&self) -> i32 {
        17
    }
}

#[lore_error_set::error_set]
enum Refusals {
    Refused,
}

/// A context pushed onto a set's trace names the file and line of the call that pushed it.
#[cfg(feature = "track-locations")]
#[test]
fn a_pushed_context_names_its_caller() {
    let mut err = Refusals::from(Refused);
    let line = line!() + 1;
    err.push_context("refusing the request");

    let trace = err.extract_inner().trace;
    let pushed = trace.locations().last().expect("the pushed entry");
    assert_eq!(
        (pushed.file, pushed.line, pushed.context()),
        (file!(), line, Some("refusing the request"))
    );
}
