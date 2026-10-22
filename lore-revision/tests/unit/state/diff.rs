// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The decisions a walk takes and does not report: what it makes of its two filters,
//! what it seeds each side with, what it asks the to side about a paired node, and what it
//! does with a subtree it has no budget left to walk in a task.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::types::Address;
use lore_revision::MAX_CONCURRENT_TREE_TASKS;
use lore_revision::change;
use lore_revision::change::NodeChangeState;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterMode;
use lore_revision::filter::FilterStates;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::ChangeStream;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::diff::*;
use lore_revision::util::path::RelativePath;
use tokio::sync::Semaphore;

/// A filter that excludes `x` and re-includes under it, so a query against it steps
/// to a verdict no root state matches and still descends.
///
/// Descending is what keeps it free of an event, and so of the execution context a
/// send reaches for and a unit test does not stand up.
fn stepping() -> Arc<Filter> {
    let mut filter = Filter::default();
    filter.view.add_exclusion("/x").expect("view exclusion");
    filter
        .view
        .add_inclusion("/x/keep")
        .expect("view inclusion");
    Arc::new(filter)
}

fn stepped_path() -> RelativePath {
    RelativePath::new_from_initial_path("x").expect("valid path")
}

#[test]
fn one_filter_answers_for_both_sides() {
    let filter = stepping();
    let path = stepped_path();

    let (from_states, to_states, excluded) = seed_sides(
        &filter,
        &filter,
        &path,
        DiffFlags::empty(),
        FilterMode::Full,
    );

    assert_eq!(
        (from_states, excluded),
        seed_states(&filter, &path, true, FilterMode::Full),
        "one filter must seed both sides with the one answer it gives"
    );
    assert_eq!(from_states, to_states);
}

/// Two filters are asked separately, and the walk goes on unless both exclude.
#[test]
fn two_filters_seed_each_side_from_its_own() {
    let from = stepping();
    let to = Arc::new(Filter::default());
    let path = stepped_path();

    let (from_states, to_states, excluded) =
        seed_sides(&from, &to, &path, DiffFlags::TwoViews, FilterMode::Full);

    assert_ne!(
        from_states, to_states,
        "each side must carry the verdict of the filter it walks under"
    );
    assert_eq!(
        to_states,
        FilterStates::ROOT,
        "a filter with no rules steps nowhere"
    );
    assert!(
        !excluded,
        "a path one side still holds must not leave the walk"
    );
}

#[test]
fn one_filter_on_both_sides_is_not_two_views() {
    let filter = Arc::new(Filter::default());
    let shared = filter.clone();
    assert_eq!(DiffFlags::between(&filter, &shared), DiffFlags::empty());
}

#[test]
fn two_filters_are_two_views_even_where_their_rules_agree() {
    let from = Arc::new(Filter::default());
    let to = Arc::new(Filter::default());
    assert_eq!(DiffFlags::between(&from, &to), DiffFlags::TwoViews);
}

/// A filter that excludes `path`, so a query against it is visible in what comes
/// back: states other than the parent's, and a verdict of `true`.
fn excluding(path: &str) -> Filter {
    let mut filter = Filter::default();
    filter
        .view
        .add_exclusion(&format!("/{path}"))
        .expect("view exclusion");
    filter
}

/// The to side is not asked about a paired file under one filter.
#[test]
fn a_paired_file_under_one_filter_costs_no_to_side_query() {
    let filter = excluding("x");
    let path = RelativePath::new_from_initial_path("x").expect("valid path");
    let stats = DiffWalkStats::default();
    let verdict = |flags| {
        to_subtree_verdict(
            &filter,
            FilterStates::ROOT,
            &path,
            true,
            true,
            flags,
            FilterMode::Full,
            &stats,
        )
    };

    assert_eq!(verdict(DiffFlags::empty()), (FilterStates::ROOT, false));
    assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 0);
    assert!(
        verdict(DiffFlags::TwoViews).1,
        "the filter must be one that answers true, or the case above proves nothing"
    );
    assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 1);
}

/// A paired directory needs the states below it whatever the flags say, so it is
/// asked either way -- and reports no exclusion under one filter, where the from
/// side's verdict already stands for both.
#[test]
fn a_paired_directory_is_asked_under_one_filter_and_reports_nothing() {
    let filter = excluding("x");
    let path = RelativePath::new_from_initial_path("x").expect("valid path");
    let stats = DiffWalkStats::default();
    let (states, excluded) = to_subtree_verdict(
        &filter,
        FilterStates::ROOT,
        &path,
        false,
        false,
        DiffFlags::empty(),
        FilterMode::Full,
        &stats,
    );

    assert_ne!(states, FilterStates::ROOT, "the states must be stepped");
    assert!(!excluded, "one filter cannot route the two sides apart");
    assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 1);
}

/// A directory the to side excludes but re-includes under is still held there, so the
/// verdict is the subtree's and not the node's: deleting it would take the re-included
/// content with it.
#[test]
fn a_directory_re_including_below_itself_is_not_excluded() {
    let path = RelativePath::new_from_initial_path("x").expect("valid path");
    let excluded = |filter: &Filter| {
        to_subtree_verdict(
            filter,
            FilterStates::ROOT,
            &path,
            false,
            false,
            DiffFlags::TwoViews,
            FilterMode::Full,
            &DiffWalkStats::default(),
        )
        .1
    };
    let mut re_including = excluding("x");
    re_including
        .view
        .add_inclusion("/x/keep")
        .expect("view inclusion");

    assert!(
        excluded(&excluding("x")),
        "the rule alone must exclude the directory"
    );
    assert!(
        !excluded(&re_including),
        "a re-inclusion below must keep the directory"
    );
}

/// `is_file` alone decides which of the two questions is put, and a directory-only rule
/// makes the two answers diverge.
///
/// This is the shape a link arrives in: neither side of the pairing is a file, so the
/// subtree question is asked of it, and the rule reaches what a mount holds without
/// matching the mount. The node question, which the type-change branch puts, answers the
/// other way -- so neither call stands in for the other.
#[test]
fn a_directory_only_rule_answers_the_two_questions_differently() {
    let mut filter = Filter::default();
    filter.view.add_exclusion("/x/").expect("view exclusion");
    let path = stepped_path();
    let excluded = |was_file, is_file| {
        to_subtree_verdict(
            &filter,
            FilterStates::ROOT,
            &path,
            was_file,
            is_file,
            DiffFlags::TwoViews,
            FilterMode::Full,
            &DiffWalkStats::default(),
        )
        .1
    };

    assert!(excluded(false, false), "the subtree question must exclude");
    assert!(
        !excluded(true, true),
        "the node question must not match what is no directory"
    );
}

/// How deep the fixture tree runs. Every level is a paired directory the walk dispatches, so
/// a walk with no budget left takes all of them from its own queue.
///
/// Deep enough that walking them from the frames that found them would not fit a thread's
/// stack: a walk that recursed instead of queueing aborts the run here, at the 103,504 bytes
/// a level of that cost when it was measured, rather than passing quietly.
const DEPTH: u32 = 30;
/// Files in each directory of the fixture tree.
const FILES: u32 = 2;

/// The name of the directory at `level` of the chain.
fn directory_name(level: u32) -> String {
    format!("d{level:02}")
}

/// The name of file `index` in a directory of the chain.
fn file_name(index: u32) -> String {
    format!("f{index}.bin")
}

/// A file node addressing `content`, which is all the walk reads of a file: the content
/// itself is never fetched, so nothing stands behind the address but this.
fn file_node(content: &[u8]) -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        address: Address::zero_context_hash(lore_storage::hash::hash_slice(content)),
        size: content.len() as u64,
        ..Default::default()
    }
}

/// A state holding a chain of [`DEPTH`] directories with [`FILES`] files in each, the
/// deepest of them holding `content` and the rest holding nothing.
///
/// Staged rather than committed, which is what makes every directory of it worth
/// descending: the nodes carry no computed address, so the walk pairs them on the staged
/// flag instead.
async fn chain_state(repository: &Arc<RepositoryContext>, content: &[u8]) -> Arc<State> {
    let state = State::new();
    let stage = async |path: RelativePath, node| {
        lore_revision::stage::stage_single_node(
            repository.clone(),
            state.clone(),
            path,
            node,
            Arc::default(),
            None,
            FilterMode::empty(),
        )
        .await
        .expect("a staged node");
    };
    let mut directory = RelativePath::new();
    for level in 0..DEPTH {
        directory = directory.push_into_buf(directory_name(level)).freeze();
        stage(directory.clone(), Node::default()).await;
        for file in 0..FILES {
            let deepest = level + 1 == DEPTH && file + 1 == FILES;
            stage(
                directory.push_into_buf(file_name(file)).freeze(),
                file_node(if deepest { content } else { b"" }),
            )
            .await;
        }
    }
    state
}

/// The path of the one file [`chain_state`] addresses differently for a different
/// `content`, which is the deepest file of the chain.
fn deepest_file() -> String {
    let mut path = (0..DEPTH).map(directory_name).collect::<Vec<_>>();
    path.push(file_name(FILES - 1));
    path.join("/")
}

/// A repository with no working tree, which is all a state-to-state walk needs.
async fn walk_repository() -> Arc<RepositoryContext> {
    let (immutable_store, mutable_store, _execution) =
        crate::fs::filesystem_provider::test_store_create()
            .await
            .expect("test stores");
    Arc::new(RepositoryContext::new(
        crate::helper::default_repository_creation_args(immutable_store, mutable_store),
    ))
}

/// What a walk between the two states emitted, sorted, and how many directories it stood
/// in.
///
/// Each change is its action letter, whether the walk measured the content as changed, and
/// its path. The letter alone does not carry the second: a paired file is reported `Keep`,
/// which reads as `M`, whether its content moved or only its staged flag did.
async fn walk(
    repository: &Arc<RepositoryContext>,
    from: &Arc<State>,
    to: &Arc<State>,
) -> (Vec<(String, bool, String)>, u64) {
    let (repository_from, repository_to) = (repository.clone(), repository.clone());
    let (from, to) = (from.clone(), to.clone());
    let mut walk = ChangeStream::spawn(async move |changes| {
        lore_revision::state::diff(
            repository_from,
            from,
            repository_to,
            to,
            None,
            None,
            &changes,
            FilterMode::Full,
        )
        .await
    });
    let mut emitted = Vec::new();
    while let Some(change) = walk.next().await {
        emitted.push((
            change.action.as_string_short().to_string(),
            change.flags.contains(change::Flags::Modify),
            change.path().as_str().to_string(),
        ));
    }
    let stats = walk.finish().await.expect("a walk of the two states");
    emitted.sort();
    (emitted, stats.directories_entered.load(Ordering::Relaxed))
}

/// One semaphore for the process, so that what bounds one walk bounds every walk running
/// beside it. A budget minted per walk would let each of them fan out as far as one walk
/// may.
#[test]
fn the_fan_out_budget_is_one_semaphore() {
    let first: *const Semaphore = Arc::as_ptr(subtree_task_semaphore());
    let second: *const Semaphore = Arc::as_ptr(subtree_task_semaphore());
    assert_eq!(first, second, "the budget must not be minted per caller");
}

/// A walk that cannot spawn takes every subtree from [`SubtreeWork::pending`] instead, and
/// reports the same changes over the same directories as a walk that spawns them all.
///
/// No permit is free here, so the queue is the only way down: each of the [`DEPTH`]
/// directories below the root is queued by the directory above it and walked by the one task
/// the walk has. That every one of them was walked is what the count says, and the chain is
/// long enough that walking them from the frames that queued them would not fit a thread's
/// stack.
///
/// The timeout is the point of the test as much as the equality is: a task holds its permit
/// until the subtrees it spawned finish, so a walk that waited for one would wait on a
/// descendant that cannot start, and would never end rather than fail.
#[tokio::test]
async fn a_walk_with_no_budget_left_walks_the_same_tree() {
    let execution = crate::fs::filesystem_provider::setup_test_execution();
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async {
            let repository = walk_repository().await;
            let from = chain_state(&repository, b"one").await;
            let to = chain_state(&repository, b"two").await;

            let spawned = walk(&repository, &from, &to).await;

            let _budget = subtree_task_semaphore()
                .clone()
                .acquire_many_owned(MAX_CONCURRENT_TREE_TASKS as u32)
                .await
                .expect("the whole fan-out budget");
            assert_eq!(
                subtree_task_semaphore().available_permits(),
                0,
                "the walk below must meet a budget that is actually spent"
            );
            let queued =
                tokio::time::timeout(Duration::from_secs(120), walk(&repository, &from, &to))
                    .await
                    .expect("a walk that never waits for a permit it cannot be given");

            assert_eq!(
                spawned.1,
                u64::from(DEPTH) + 1,
                "the walk must stand in the root and every directory below it"
            );
            assert_eq!(
                queued.1, spawned.1,
                "every queued subtree must be walked, and counted by the task that walked it"
            );
            assert_eq!(
                queued.0, spawned.0,
                "a subtree taken from the queue must report what a task of its own would have"
            );
            assert!(
                spawned.0.contains(&("M".to_string(), true, deepest_file())),
                "the one file the two states address differently must be the one reported \
                     with its content changed"
            );
        })
        .await;
}

/// A subtree is held once on its way down a walk: in the walk's queue, and then taken apart
/// by the directory walking it, never again beside either.
#[tokio::test]
async fn a_walk_holds_each_subtree_once() {
    let execution = crate::fs::filesystem_provider::setup_test_execution();
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async {
            let repository = walk_repository().await;
            let side = NodeChangeState {
                mapping: NodeMapping {
                    repository,
                    state: State::new(),
                    path: RelativePath::new(),
                    node: 0,
                },
                observed: None,
                flags: NodeFlags::NoFlags,
                address: Address::default(),
                mode: 0,
            };
            let subtree = || PendingSubtree {
                from: side.clone(),
                to: side.clone(),
                cursor: DiffCursor {
                    paths: DiffPaths {
                        from: RelativePath::new(),
                        to: RelativePath::new(),
                        depth: 0,
                    },
                    states: DiffStates {
                        from: FilterStates::ROOT,
                        to: FilterStates::ROOT,
                    },
                },
                graft: None,
            };
            let (changes, _receiver) = tokio::sync::mpsc::channel(1);
            let stats = DiffWalkStats::default();
            let mut work = SubtreeWork {
                tasks: SubtreeTasks::new(),
                pending: Vec::new(),
            };
            let mut queue = SubtreeWork {
                tasks: SubtreeTasks::new(),
                pending: Vec::new(),
            };
            let flags = DiffFlags::empty();
            let mode = FilterMode::Full;

            let directory = diff_subtree_node(subtree(), flags, &changes, mode, &mut work, &stats);
            let pending = walk_pending_subtrees(&mut queue, flags, &changes, mode, &stats);
            let (directory, pending) = (size_of_val(&directory), size_of_val(&pending));
            let walk = diff_subtree_walk(subtree(), flags, changes.clone(), mode, None);

            assert!(
                pending < directory + size_of::<PendingSubtree>(),
                "the queue loop holds {pending} bytes, the directory it walks {directory}"
            );
            assert!(
                size_of_val(&walk) < pending + size_of::<PendingSubtree>(),
                "the walk holds {} bytes, the queue loop {pending}",
                size_of_val(&walk)
            );
        })
        .await;
}
