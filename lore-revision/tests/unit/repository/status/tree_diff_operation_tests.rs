// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::status::*;
use lore_revision::state;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::TestFilesystemProvider;
use crate::fs::filesystem_provider::test_store_create;
use crate::repository::test_helpers::RepositoryContextCreationArgsExt;
use crate::repository::test_helpers::default_repository_creation_args;

/// Runs `plan` over a repository whose states are empty and whose working tree holds what they
/// do, and answers how many operations it began and what each finalize reported.
///
/// Empty states leave every phase with nothing to report, which is what isolates the count from
/// the reporting.
async fn operations_begun(plan: TreeDiffPlan) -> (usize, Vec<bool>) {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem.clone()),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let state = State::new();
            report_tree_diffs(
                &repository,
                &[None],
                &state,
                &state,
                &[],
                &Arc::new(Vec::new()),
                &Arc::new(StatusSummaryStats::default()),
                plan,
            )
            .await
            .expect("The diff succeeded");
        })
        .await;

    let finalizes = filesystem.finalize_events.lock().clone();
    (filesystem.begins(), finalizes)
}

#[tokio::test]
async fn a_staged_comparison_alone_reads_no_working_tree() {
    let (begins, finalizes) = operations_begun(TreeDiffPlan {
        compare_staged: true,
        check_dirty: false,
        scan: false,
    })
    .await;

    assert_eq!(
        0, begins,
        "A comparison of two states read the working tree"
    );
    assert!(finalizes.is_empty());
}

#[tokio::test]
async fn a_dirty_check_without_a_staged_comparison_reads_no_working_tree() {
    let (begins, _) = operations_begun(TreeDiffPlan {
        compare_staged: false,
        check_dirty: true,
        scan: false,
    })
    .await;

    assert_eq!(
        0, begins,
        "A dirty check with no comparison to check for opened an operation"
    );
}

#[tokio::test]
async fn a_dirty_check_opens_one_operation() {
    let (begins, finalizes) = operations_begun(TreeDiffPlan {
        compare_staged: true,
        check_dirty: true,
        scan: false,
    })
    .await;

    assert_eq!(1, begins);
    assert_eq!(vec![false], finalizes);
}

#[tokio::test]
async fn a_scan_opens_one_operation() {
    let (begins, finalizes) = operations_begun(TreeDiffPlan {
        compare_staged: false,
        check_dirty: false,
        scan: true,
    })
    .await;

    assert_eq!(1, begins);
    assert_eq!(vec![false], finalizes);
}

/// The snapshot a dirty check reads is the one the scan reads, which holds only while both run
/// within a single operation.
#[tokio::test]
async fn a_dirty_check_and_a_scan_share_one_operation() {
    let (begins, finalizes) = operations_begun(TreeDiffPlan {
        compare_staged: true,
        check_dirty: true,
        scan: true,
    })
    .await;

    assert_eq!(
        1, begins,
        "Checking dirty flags and scanning read separate snapshots"
    );
    assert_eq!(vec![false], finalizes);
}

/// Reporting the changes a scan finds holds each change once, beside the report of it.
#[tokio::test]
async fn a_scanned_change_is_held_once_while_it_is_reported() {
    let filesystem = Arc::new(TestFilesystemProvider::new());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem.clone()),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let operation = filesystem.begin_operation().await.expect("An operation");
            let side = lore_revision::change::NodeChangeState {
                mapping: state::NodeMapping::root(repository.clone(), State::new()),
                observed: None,
                flags: lore_revision::node::NodeFlags::NoFlags,
                address: Default::default(),
                mode: 0,
            };
            let change = NodeChange {
                action: FileAction::Keep,
                flags: lore_revision::change::Flags::None,
                from: side.clone(),
                to: side,
            };
            let summary = StatusSummaryStats::default();
            let mut changes = state::ChangeStream::nothing();

            let report = report_scan_change(&operation, &repository, &summary, &change);
            let reports = report_scan_changes(&operation, &repository, &summary, &mut changes);

            assert!(
                size_of_val(&reports) < size_of_val(&report) + 2 * size_of::<NodeChange>(),
                "reporting the changes holds {} bytes, reporting one {}",
                size_of_val(&reports),
                size_of_val(&report)
            );
        })
        .await;
}

/// **A scan that fails still discards what the other scans found stale.** The discards are held
/// until every scan has drained, so a failure returned before applying them would leave a reverted
/// add, and the marks it carried up the tree, in a state the scans that did finish had reconciled.
#[tokio::test]
async fn a_failed_scan_still_discards_what_the_other_scans_found_stale() {
    let filesystem = Arc::new(TestFilesystemProvider::holding_every_path());
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Making test stores");
    let repository = Arc::new(RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_filesystem_provider(filesystem.clone()),
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            // The staged tree holds an add the scan of `kept` finds reverted, and the scan of
            // `failing` fails.
            let staged = State::new();
            let stale = staged
                .node_add(
                    repository.clone(),
                    ROOT_NODE,
                    Node {
                        name_hash: lore_storage::hash::hash_string("stale.txt"),
                        flags: NodeFlags::DirtyAdd.bits(),
                        ..Default::default()
                    },
                    "stale.txt",
                )
                .await
                .expect("Adding the add to the staged tree");
            filesystem
                .stale_on_scan
                .lock()
                .push(("kept".to_string(), stale));
            filesystem.failing_scans.lock().push("failing".to_string());

            let path = |path: &str| {
                Some(RelativePath::new_from_initial_path(path).expect("A relative path"))
            };
            let scanned = report_tree_diffs(
                &repository,
                &[path("failing"), path("kept")],
                &State::new(),
                &staged,
                &[],
                &Arc::new(Vec::new()),
                &Arc::new(StatusSummaryStats::default()),
                TreeDiffPlan {
                    compare_staged: false,
                    check_dirty: false,
                    scan: true,
                },
            )
            .await;
            assert!(scanned.is_err(), "the failing scan fails the run");
            assert!(
                staged
                    .find_node_link(repository.clone(), "stale.txt")
                    .await
                    .is_err(),
                "the reverted add the other scan found is discarded all the same"
            );
        })
        .await;
}
