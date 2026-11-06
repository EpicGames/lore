// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cell::RefCell;

use lore_revision::fs::swfs::panic_guard::panic_guard;

#[test]
fn a_callback_that_returns_gives_its_value_and_reports_nothing() {
    let reported = RefCell::new(Vec::<(String, String)>::new());

    let value = panic_guard(
        "read_file",
        || 7_u64,
        |callback, panic| {
            reported
                .borrow_mut()
                .push((callback.to_owned(), panic.to_owned()));
        },
    );

    assert_eq!(value, 7);
    assert!(reported.borrow().is_empty());
}

#[test]
fn a_callback_that_panics_gives_its_failure_value_and_reports_the_panic() {
    let reported = RefCell::new(Vec::new());

    let value = panic_guard(
        "read_file",
        || -> u64 { panic!("the node is not a directory") },
        |callback, panic| {
            reported
                .borrow_mut()
                .push((callback.to_owned(), panic.to_owned()));
        },
    );

    assert_eq!(value, 0);
    assert_eq!(
        reported.into_inner(),
        vec![(
            "read_file".to_owned(),
            "the node is not a directory".to_owned()
        )]
    );
}

#[test]
fn a_panic_with_a_formatted_message_is_reported_with_it() {
    let reported = RefCell::new(Vec::new());

    panic_guard::<()>(
        "fill_dir_begin",
        || panic!("node {} has no children", 12),
        |callback, panic| {
            reported
                .borrow_mut()
                .push((callback.to_owned(), panic.to_owned()));
        },
    );

    assert_eq!(
        reported.into_inner(),
        vec![(
            "fill_dir_begin".to_owned(),
            "node 12 has no children".to_owned()
        )]
    );
}
