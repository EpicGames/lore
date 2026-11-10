// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::LockData;
use lore_revision::lock::file::acquire::*;
use lore_revision::lock::util::fold_batch_results;

/// Builds a batch error shaped like the one a batch task produces: the
/// remote's denial forwarded under the generic acquire context.
fn denied_batch_error() -> AcquireError {
    AcquireError::internal("resource already locked").forward("Failed to acquire the lock")
}

#[test]
fn batch_denial_reason_survives_the_fold() {
    let denial = denied_batch_error();
    let reported = denial.to_string();

    let (locks, num_batch_success, first_batch_error) = fold_batch_results::<LockData, _>(
        vec![Err(denial), Err(AcquireError::internal("a later batch"))],
        0,
    );

    assert!(locks.is_empty());
    assert_eq!(num_batch_success, 0);
    assert_eq!(
        first_batch_error
            .expect("a failing batch keeps its error")
            .to_string(),
        reported
    );
}

#[test]
fn batch_denial_reason_reaches_the_message() {
    let reported = denied_batch_error().to_string();

    assert!(
        reported.ends_with("resource already locked"),
        "the remote's reason is not in the reported message: {reported}"
    );
}

/// Acquires the lock on `name` below `root` through `repository`, which holds no remote: a lock
/// asked for fails with [`AcquireError::NoRemote`], and one the filter leaves out returns first.
async fn acquire_named(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    root: &std::path::Path,
    name: &str,
) -> Result<(), AcquireError> {
    acquire(
        repository,
        AcquireOptions {
            paths: crate::helper::path_arguments(root, &[name]),
            branch: String::new(),
            owner: String::new(),
        },
    )
    .await
}

/// A lock on a tracked file a directory-only rule names is asked for.
#[tokio::test]
async fn a_lock_on_a_file_a_directory_only_rule_names_is_asked_for() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["src/Intermediate"], "Intermediate/\n").await;
        let acquired = acquire_named(ruled, &tempdir, "src/Intermediate").await;
        assert!(
            acquired.as_ref().is_err_and(|err| err.is_no_remote()),
            "the lock of src/Intermediate was left out: {acquired:?}"
        );
    })
    .await;
}

/// A lock on a committed file the ignore file names is asked for: the tree tracks the file, so
/// the ignore file has no say over it. One on a file the rule names that the tree does not track
/// is left out.
#[tokio::test]
async fn a_lock_on_a_tracked_file_the_ignore_file_names_is_asked_for() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["debug.log"], "*.log\n").await;
        crate::helper::test_file_write(&tempdir.join("new.log"), b"untracked");
        let acquired = acquire_named(ruled.clone(), &tempdir, "debug.log").await;
        assert!(
            acquired.as_ref().is_err_and(|err| err.is_no_remote()),
            "the lock of debug.log was left out: {acquired:?}"
        );
        let acquired = acquire_named(ruled, &tempdir, "new.log").await;
        assert!(
            acquired.is_ok(),
            "the lock of new.log was asked for: {acquired:?}"
        );
    })
    .await;
}

/// A lock on a file the ignore file names that a forced stage added, and nothing committed, is
/// asked for: the tree tracks what it stages.
#[tokio::test]
async fn a_lock_on_a_staged_file_the_ignore_file_names_is_asked_for() {
    crate::helper::in_execution(async {
        let (ruled, _, tempdir) =
            crate::helper::ignoring_repository(&["debug.log"], "*.log\n").await;
        crate::helper::test_file_write(&tempdir.join("new.log"), b"staged");
        crate::helper::stage_paths(&ruled, &tempdir, &["new.log"], false, true).await;
        let acquired = acquire_named(ruled, &tempdir, "new.log").await;
        assert!(
            acquired.as_ref().is_err_and(|err| err.is_no_remote()),
            "the lock of the staged new.log was left out: {acquired:?}"
        );
    })
    .await;
}
