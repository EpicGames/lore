// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build filesystem state directly; what these test is how the merge reads and writes it.
#![allow(clippy::disallowed_methods)]

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::os::OsFilesystem;
use lore_revision::merge::*;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::setup_test_execution;

/// The three sides a merge reads, written under `root`.
fn sides(root: &std::path::Path, base: &str, mine: &str, theirs: &str) {
    std::fs::write(root.join("base"), base).expect("write base");
    std::fs::write(root.join("mine"), mine).expect("write mine");
    std::fs::write(root.join("theirs"), theirs).expect("write theirs");
}

fn relative(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("relative path")
}

async fn os_operation(root: &std::path::Path) -> std::sync::Arc<InstanceOperationImpl> {
    FilesystemProvider::begin_operation(&OsFilesystem::new(root))
        .await
        .expect("beginning an operation over the OS filesystem")
}

#[tokio::test]
async fn a_merge_writes_its_result_at_the_path_the_operation_names() {
    let dir = lore_base::test_util::TempDir::new("lore-merge-test-");
    sides(dir.path(), "one\n", "one\ntwo\n", "one\n");
    let operation = os_operation(dir.path()).await;
    let token = RepositoryWriteToken::acquire(dir.path()).await;

    let conflicted = merge3_text_in_operation(
        &operation,
        &relative("base"),
        &relative("mine"),
        &relative("theirs"),
        &relative("result"),
        MergeTextMode::Write(&token),
    )
    .await
    .expect("the merge must run");

    assert!(!conflicted);
    assert_eq!(
        "one\ntwo\n",
        std::fs::read_to_string(dir.path().join("result")).expect("read the result")
    );
}

/// A conflict is an outcome rather than a failure, and the result carries the markers.
#[tokio::test]
async fn a_conflicting_merge_writes_the_marked_result() {
    let dir = lore_base::test_util::TempDir::new("lore-merge-test-");
    sides(dir.path(), "one\n", "mine\n", "theirs\n");
    let operation = os_operation(dir.path()).await;
    let token = RepositoryWriteToken::acquire(dir.path()).await;

    let conflicted = merge3_text_in_operation(
        &operation,
        &relative("base"),
        &relative("mine"),
        &relative("theirs"),
        &relative("result"),
        MergeTextMode::Write(&token),
    )
    .await
    .expect("the merge must run");

    assert!(conflicted);
    let result = std::fs::read_to_string(dir.path().join("result")).expect("read the result");
    assert!(result.contains("mine") && result.contains("theirs"));
}

/// A merge whose sides the provider cannot name reports the provider's error.
#[tokio::test]
async fn a_merge_of_sides_that_cannot_be_named_reports_the_error() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let filesystem = TestFilesystemProvider::failing_content_sources();
            let operation = filesystem.begin_operation().await.expect("an operation");

            let merged = merge3_text_in_operation(
                &operation,
                &relative("base"),
                &relative("mine"),
                &relative("theirs"),
                &relative("result"),
                MergeTextMode::DryRun,
            )
            .await;

            assert!(matches!(merged, Err(FsError::Internal(_))), "{merged:?}");
        })
        .await;
}

/// A dry run reports the conflict and leaves the result path alone.
#[tokio::test]
async fn a_dry_run_writes_nothing() {
    let dir = lore_base::test_util::TempDir::new("lore-merge-test-");
    sides(dir.path(), "one\n", "mine\n", "theirs\n");
    let operation = os_operation(dir.path()).await;

    let conflicted = merge3_text_in_operation(
        &operation,
        &relative("base"),
        &relative("mine"),
        &relative("theirs"),
        &relative("result"),
        MergeTextMode::DryRun,
    )
    .await
    .expect("the merge must run");

    assert!(conflicted);
    assert!(
        !dir.path().join("result").exists(),
        "nothing must be written"
    );
}

/// By the exact rules, text both sides edited on different lines merges as it is, a byte order
/// mark included.
#[test]
fn merge_sides_merges_text_as_it_is() {
    assert_eq!(
        merge_sides(
            "\u{feff}one\ntwo\nthree\n".as_bytes(),
            "\u{feff}mine\ntwo\nthree\n".as_bytes(),
            "\u{feff}one\ntwo\ntheirs\n".as_bytes(),
            TextRules::Exact
        ),
        Some("\u{feff}mine\ntwo\ntheirs\n".to_string())
    );
}

/// By the exact rules, a side that is not UTF-8 text is left a conflict, as is text both sides
/// edited on one line, and by the lossy rules auto-resolve has always used it merges.
#[test]
fn merge_sides_merges_text_that_is_not_utf8_by_the_lossy_rules_alone() {
    let sides: [&[u8]; 3] = [b"one\n\xc3", b"mine\n\xc3", b"one\n\xc3"];
    assert_eq!(
        merge_sides(sides[0], sides[1], sides[2], TextRules::Exact),
        None
    );
    assert_eq!(
        merge_sides(sides[0], sides[1], sides[2], TextRules::Lossy),
        Some("mine\n\u{fffd}".to_string())
    );
    assert_eq!(
        merge_sides(b"one\n", b"mine\n", b"theirs\n", TextRules::Exact),
        None
    );
}
