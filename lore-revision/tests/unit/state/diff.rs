// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The decisions a walk takes and does not report: what it makes of its two filters,
//! what it seeds each side with, what it asks the to side about a paired node, and what it
//! does with a subtree it has no budget left to walk in a task.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::MAX_CONCURRENT_TREE_TASKS;
use lore_revision::change;
use lore_revision::change::NodeChangeState;
use lore_revision::filter;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterInstance;
use lore_revision::filter::FilterMode;
use lore_revision::filter::FilterStates;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::ChangeStream;
use lore_revision::state::GraftOracle;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::diff::*;
use lore_revision::state::may_warm_differing_blocks;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_slice;
use lore_storage::hash::hash_string;
use tokio::sync::Semaphore;

use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::in_memory;
use crate::helper::test_store_create;

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
        (true, true),
        DiffFlags::empty(),
        FilterMode::Full,
    );

    assert_eq!(
        (from_states, excluded),
        seed_states(&filter, &path, true, true, FilterMode::Full),
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

    let (from_states, to_states, excluded) = seed_sides(
        &from,
        &to,
        &path,
        (true, true),
        DiffFlags::TwoViews,
        FilterMode::Full,
    );

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

/// [`to_subtree_verdict`] from the root in `mode`, of a node whose tracking is `tracked`.
#[allow(clippy::too_many_arguments)]
async fn subtree_verdict(
    filter: &Filter,
    path: &RelativePath,
    was_file: bool,
    is_file: bool,
    flags: DiffFlags,
    mode: FilterMode,
    tracked: bool,
    stats: &DiffWalkStats,
) -> (FilterStates, bool) {
    let Ok(verdict) = to_subtree_verdict(
        filter,
        FilterStates::ROOT,
        path,
        was_file,
        is_file,
        flags,
        mode,
        stats,
        async || Ok::<_, Infallible>(tracked),
    )
    .await;
    verdict
}

/// The to side is not asked about a paired file under one filter.
#[tokio::test]
async fn a_paired_file_under_one_filter_costs_no_to_side_query() {
    let filter = excluding("x");
    let path = RelativePath::new_from_initial_path("x").expect("valid path");
    let stats = DiffWalkStats::default();
    let verdict = async |flags| {
        subtree_verdict(
            &filter,
            &path,
            true,
            true,
            flags,
            FilterMode::Full,
            false,
            &stats,
        )
        .await
    };

    assert_eq!(
        verdict(DiffFlags::empty()).await,
        (FilterStates::ROOT, false)
    );
    assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 0);
    assert!(
        verdict(DiffFlags::TwoViews).await.1,
        "the filter must be one that answers true, or the case above proves nothing"
    );
    assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 1);
}

/// A directory the to side's ignore rules name is excluded under two views only where its tree
/// does not track it, and its children inherit the ignore slot's step either way.
#[tokio::test]
async fn a_tracked_directory_passes_the_to_side_ignore_slot() {
    let mut filter = Filter::default();
    filter.ignore.add_exclusion("/x").expect("ignore exclusion");
    let path = stepped_path();
    let stats = DiffWalkStats::default();
    let verdict = async |tracked| {
        subtree_verdict(
            &filter,
            &path,
            false,
            false,
            DiffFlags::TwoViews,
            FilterMode::TRACKED,
            tracked,
            &stats,
        )
        .await
    };

    let (untracked_states, untracked) = verdict(false).await;
    let (tracked_states, tracked) = verdict(true).await;
    assert!(
        untracked,
        "an untracked directory the rules name is excluded"
    );
    assert!(!tracked, "a tracked directory is past the ignore slot");
    assert_eq!(
        tracked_states, untracked_states,
        "the keep bits decide which slots exclude, not the states"
    );
    assert_ne!(
        tracked_states,
        FilterStates::ROOT,
        "the states must be stepped"
    );
}

/// A paired directory needs the states below it whatever the flags say, so it is
/// asked either way -- and reports no exclusion under one filter, where the from
/// side's verdict already stands for both.
#[tokio::test]
async fn a_paired_directory_is_asked_under_one_filter_and_reports_nothing() {
    let filter = excluding("x");
    let path = RelativePath::new_from_initial_path("x").expect("valid path");
    let stats = DiffWalkStats::default();
    let (states, excluded) = subtree_verdict(
        &filter,
        &path,
        false,
        false,
        DiffFlags::empty(),
        FilterMode::Full,
        false,
        &stats,
    )
    .await;

    assert_ne!(states, FilterStates::ROOT, "the states must be stepped");
    assert!(!excluded, "one filter cannot route the two sides apart");
    assert_eq!(stats.filter_queries.load(Ordering::Relaxed), 1);
}

/// A directory the to side excludes but re-includes under is still held there, so the
/// verdict is the subtree's and not the node's: deleting it would take the re-included
/// content with it.
#[tokio::test]
async fn a_directory_re_including_below_itself_is_not_excluded() {
    let path = RelativePath::new_from_initial_path("x").expect("valid path");
    let excluded = async |filter: &Filter| {
        subtree_verdict(
            filter,
            &path,
            false,
            false,
            DiffFlags::TwoViews,
            FilterMode::Full,
            false,
            &DiffWalkStats::default(),
        )
        .await
        .1
    };
    let mut re_including = excluding("x");
    re_including
        .view
        .add_inclusion("/x/keep")
        .expect("view inclusion");

    assert!(
        excluded(&excluding("x")).await,
        "the rule alone must exclude the directory"
    );
    assert!(
        !excluded(&re_including).await,
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
#[tokio::test]
async fn a_directory_only_rule_answers_the_two_questions_differently() {
    let mut filter = Filter::default();
    filter.view.add_exclusion("/x/").expect("view exclusion");
    let path = stepped_path();
    let excluded = async |was_file, is_file| {
        subtree_verdict(
            &filter,
            &path,
            was_file,
            is_file,
            DiffFlags::TwoViews,
            FilterMode::Full,
            false,
            &DiffWalkStats::default(),
        )
        .await
        .1
    };

    assert!(
        excluded(false, false).await,
        "the subtree question must exclude"
    );
    assert!(
        !excluded(true, true).await,
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
            let changes = lore_revision::state::ChangeSender::from_sender(changes);
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

/// A link named `name` mounting the node `child` of revision `revision` of repository `target`.
fn link_node(name: &str, target: RepositoryId, revision: Hash, child: NodeID) -> Node {
    Node {
        flags: NodeFlags::Link.bits(),
        mode: 0o755,
        name_hash: hash_string(name),
        child,
        address: Address {
            hash: revision,
            context: target.into(),
        },
        ..Default::default()
    }
}

/// The changes a walk between the roots of `from` and `to` emits with `extra` in `mode`, as the
/// action letter and the path of each, sorted.
async fn walk_flagged(
    repository: &Arc<RepositoryContext>,
    from: &Arc<State>,
    to: &Arc<State>,
    extra: DiffFlags,
    mode: FilterMode,
) -> Vec<(String, String)> {
    let root = |state: &Arc<State>| NodeChangeState {
        mapping: NodeMapping {
            repository: repository.clone(),
            state: state.clone(),
            path: RelativePath::new(),
            node: ROOT_NODE,
        },
        observed: None,
        flags: NodeFlags::NoFlags,
        address: Address::default(),
        mode: 0,
    };
    let (from, to) = (root(from), root(to));
    let mut walk = ChangeStream::spawn(async move |changes| {
        diff_subtree_flagged(from, to, RelativePath::new(), None, extra, &changes, mode).await
    });
    let mut emitted = Vec::new();
    while let Some(change) = walk.next().await {
        emitted.push((
            change.action.as_string_short().to_string(),
            change.path().as_str().to_string(),
        ));
    }
    walk.finish().await.expect("a walk of the two states");
    emitted.sort();
    emitted
}

/// A staged link that replaces a directory is a type change, asked about as the node it is: the
/// ignore rules exclude it as a node, and would descend below it as a directory, and the to side's
/// tree holding it staged takes it past them, so the walk reports its add with the delete.
#[tokio::test]
async fn a_staged_link_replacing_a_directory_passes_the_ignore_rules_as_a_node() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let mut filter = Filter::default();
            filter
                .ignore
                .add_exclusion("/mnt")
                .expect("ignore exclusion");
            filter
                .ignore
                .add_inclusion("/mnt/keep")
                .expect("ignore inclusion");
            let repository = Arc::new(
                RepositoryContext::new(
                    crate::helper::default_repository_creation_args(immutable, mutable)
                        .with_id(RepositoryId::from(uuid::Uuid::now_v7()))
                        .with_filter(Arc::new(filter)),
                )
                .with_write_token(in_memory::token()),
            );
            let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
            let target_revision = in_memory::linked_revision(&repository, target_id).await;

            let state = State::new();
            let directory = in_memory::add(
                &state,
                repository.clone(),
                ROOT_NODE,
                in_memory::directory("mnt"),
                "mnt",
            )
            .await;
            in_memory::add(
                &state,
                repository.clone(),
                directory,
                in_memory::file("a.txt"),
                "a.txt",
            )
            .await;
            let revision =
                in_memory::commit(&repository, &state, Hash::default(), in_memory::branch_id())
                    .await;
            let from = in_memory::revision_state(&repository, revision).await;
            let to = State::new();
            in_memory::add(
                &to,
                repository.clone(),
                ROOT_NODE,
                in_memory::link_node("mnt", target_id, target_revision),
                "mnt",
            )
            .await;

            let reported = walk_flagged(
                &repository,
                &from,
                &to,
                DiffFlags::empty(),
                FilterMode::TRACKED,
            )
            .await;
            assert_eq!(
                reported,
                [
                    ("A".to_string(), "mnt".to_string()),
                    ("D".to_string(), "mnt".to_string())
                ],
                "the staged link passes the ignore rules"
            );
        })
        .await;
}

/// A walk leaving link pins to the link registry neither descends into nor reports a link that
/// pins another revision of the subtree it mounts, and reports a link that mounts another subtree
/// as the link. A walk that descends reports what the two pins hold differently instead.
#[tokio::test]
async fn a_walk_leaving_link_pins_reports_a_link_only_where_its_mount_changes() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = in_memory::repository(immutable, mutable);
            let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
            let target = repository.to_link_context(target_id).await;
            let target_branch = in_memory::branch_id();
            let state = State::new();
            let sub = in_memory::add(
                &state,
                target.clone(),
                ROOT_NODE,
                in_memory::directory("sub"),
                "sub",
            )
            .await;
            in_memory::add(
                &state,
                target.clone(),
                sub,
                in_memory::file("inner.bin"),
                "inner.bin",
            )
            .await;
            let outer = in_memory::add(
                &state,
                target.clone(),
                ROOT_NODE,
                in_memory::file("outer.bin"),
                "outer.bin",
            )
            .await;
            let first = in_memory::commit(&target, &state, Hash::default(), target_branch).await;
            let state = in_memory::revision_state(&target, first).await;
            in_memory::modify(
                &state,
                &target,
                outer,
                Address::zero_context_hash(hash_slice(b"second")),
                NodeFlags::StagedModify,
            )
            .await;
            let second = in_memory::commit(&target, &state, first, target_branch).await;
            let sub_in_second = in_memory::revision_state(&target, second)
                .await
                .find_node_link(target.clone(), "sub")
                .await
                .expect("the second revision holds sub")
                .node;

            let parent = async |mounts: [(Hash, NodeID); 2]| {
                let state = State::new();
                for (name, (revision, child)) in ["pinned", "remounted"].into_iter().zip(mounts) {
                    in_memory::add(
                        &state,
                        repository.clone(),
                        ROOT_NODE,
                        link_node(name, target_id, revision, child),
                        name,
                    )
                    .await;
                }
                let revision =
                    in_memory::commit(&repository, &state, Hash::default(), in_memory::branch_id())
                        .await;
                in_memory::revision_state(&repository, revision).await
            };
            let from = parent([(first, ROOT_NODE), (first, ROOT_NODE)]).await;
            let to = parent([(second, ROOT_NODE), (second, sub_in_second)]).await;

            assert_eq!(
                walk_flagged(
                    &repository,
                    &from,
                    &to,
                    DiffFlags::LeavesLinkPins,
                    FilterMode::empty(),
                )
                .await,
                vec![("M".to_string(), "remounted".to_string())],
                "a pin moved under the same mount is the link registry's to report"
            );
            let descended = walk_flagged(
                &repository,
                &from,
                &to,
                DiffFlags::empty(),
                FilterMode::empty(),
            )
            .await;
            assert!(
                descended.contains(&("M".to_string(), "pinned/outer.bin".to_string()))
                    && descended.contains(&("M".to_string(), "remounted".to_string())),
                "a walk that descends reports the file the two pins hold differently: \
                 {descended:?}"
            );
        })
        .await;
}

/// A walk reporting link nodes reports a link whose pin changed as a change of the link node and
/// reads nothing of the linked repository: the pins here name revisions the store doesn't hold,
/// which a walk that descends fails to read.
#[tokio::test]
async fn a_walk_reporting_link_nodes_reports_a_repinned_link_on_its_node_unread() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = in_memory::repository(immutable, mutable);
            let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
            let parent = async |pin: Hash| {
                let state = State::new();
                in_memory::add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    link_node("lib", target_id, pin, ROOT_NODE),
                    "lib",
                )
                .await;
                let revision =
                    in_memory::commit(&repository, &state, Hash::default(), in_memory::branch_id())
                        .await;
                in_memory::revision_state(&repository, revision).await
            };
            let from = parent(Hash::from_u64(1)).await;
            let to = parent(Hash::from_u64(2)).await;

            assert_eq!(
                walk_flagged(
                    &repository,
                    &from,
                    &to,
                    DiffFlags::ReportsLinkNodes,
                    FilterMode::empty(),
                )
                .await,
                vec![("M".to_string(), "lib".to_string())]
            );
        })
        .await;
}

/// A whole-tree diff warms the node blocks its two trees differ in where both sides are of one
/// repository, the filter of each takes in the whole tree for the walk's mode, and a graft adopts
/// nothing. A view leaving a path out, on either side, prunes the walk, unless the mode leaves the
/// view out; a mode taking in the ignore rules a repository loads, which match at any depth, never
/// covers the whole tree; a side of another repository numbers its nodes apart; a graft whose view
/// leaves a path out adopts the subtree rather than entering it.
#[tokio::test]
async fn a_diff_warms_the_blocks_its_trees_differ_in_only_where_its_walk_reads_them() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let full = in_memory::repository(immutable, mutable);
            let narrowed = full.to_filter_context(stepping());
            let other = full
                .to_link_context(Context::from(uuid::Uuid::now_v7()).into())
                .await;
            let graft = |view: Arc<Filter>| GraftOracle::new(full.clone(), State::new(), view);
            let warms = |from: &RepositoryContext, to: &RepositoryContext| {
                may_warm_differing_blocks(from, to, None, FilterMode::View)
            };

            assert!(warms(&full, &full));
            assert!(!warms(&full, &narrowed));
            assert!(!warms(&narrowed, &full));
            assert!(may_warm_differing_blocks(
                &narrowed,
                &narrowed,
                None,
                FilterMode::Ignore
            ));
            let loaded = full.to_filter_context(Arc::new(
                filter::from_instances(FilterInstance::default(), FilterInstance::default())
                    .expect("the repository filter"),
            ));
            assert!(warms(&loaded, &loaded));
            assert!(!may_warm_differing_blocks(
                &loaded,
                &loaded,
                None,
                FilterMode::CHANGES
            ));
            assert!(!warms(&full, &other));
            assert!(may_warm_differing_blocks(
                &full,
                &full,
                Some(&graft(Arc::default())),
                FilterMode::View
            ));
            assert!(!may_warm_differing_blocks(
                &full,
                &full,
                Some(&graft(stepping())),
                FilterMode::View
            ));
        })
        .await;
}

/// A walk from a tree holding a link staged, to a revision holding it pinned alike, neither reports
/// the link nor enters what it mounts: a link's address is its pin, so the staged flag says nothing
/// of it.
#[tokio::test]
async fn a_walk_from_a_staged_link_pinned_alike_enters_nothing_below_it() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = in_memory::repository(immutable, mutable);
            let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
            let target = repository.to_link_context(target_id).await;
            let state = State::new();
            let sub = in_memory::add(
                &state,
                target.clone(),
                ROOT_NODE,
                in_memory::directory("sub"),
                "sub",
            )
            .await;
            in_memory::add(
                &state,
                target.clone(),
                sub,
                in_memory::file("inner.bin"),
                "inner.bin",
            )
            .await;
            let pin =
                in_memory::commit(&target, &state, Hash::default(), in_memory::branch_id()).await;
            let staged = async || {
                let state = State::new();
                in_memory::add(
                    &state,
                    repository.clone(),
                    ROOT_NODE,
                    link_node("pinned", target_id, pin, ROOT_NODE),
                    "pinned",
                )
                .await;
                state
            };
            let to = staged().await;
            let revision =
                in_memory::commit(&repository, &to, Hash::default(), in_memory::branch_id()).await;
            let to = in_memory::revision_state(&repository, revision).await;

            assert_eq!(
                walk(&repository, &staged().await, &to).await,
                (Vec::new(), 1)
            );
        })
        .await;
}

/// A committed revision of `repository` holding `files` and the empty `directories`, each added
/// with its parent directories.
async fn committed_tree(
    repository: &Arc<RepositoryContext>,
    files: &[&str],
    directories: &[&str],
) -> Arc<State> {
    let state = State::new();
    let mut added: Vec<(String, NodeID)> = Vec::new();
    for (path, is_file) in files
        .iter()
        .map(|path| (*path, true))
        .chain(directories.iter().map(|path| (*path, false)))
    {
        let mut parent = ROOT_NODE;
        let components: Vec<&str> = path.split('/').collect();
        for (depth, name) in components.iter().enumerate() {
            let prefix = components[..=depth].join("/");
            if let Some((_, node)) = added.iter().find(|(added, _)| *added == prefix) {
                parent = *node;
                continue;
            }
            let node = if is_file && depth + 1 == components.len() {
                in_memory::file(name)
            } else {
                in_memory::directory(name)
            };
            parent = in_memory::add(&state, repository.clone(), parent, node, name).await;
            added.push((prefix, parent));
        }
    }
    let revision =
        in_memory::commit(repository, &state, Hash::default(), in_memory::branch_id()).await;
    in_memory::revision_state(repository, revision).await
}

/// A repository over `immutable` and `mutable` whose view holds `rules`, a rule starting with `!`
/// re-including.
fn repository_under_view(
    immutable: &Arc<dyn lore_storage::ImmutableStore>,
    mutable: &Arc<dyn lore_storage::MutableStore>,
    rules: &[&str],
) -> Arc<RepositoryContext> {
    let mut filter = Filter::default();
    for rule in rules {
        match rule.strip_prefix('!') {
            Some(included) => filter.view.add_inclusion(included).unwrap(),
            None => filter.view.add_exclusion(rule).unwrap(),
        }
    }
    Arc::new(
        RepositoryContext::new(
            crate::helper::default_repository_creation_args(immutable.clone(), mutable.clone())
                .with_id(RepositoryId::from(uuid::Uuid::now_v7()))
                .with_filter(Arc::new(filter)),
        )
        .with_write_token(in_memory::token()),
    )
}

/// The deletes a walk between the roots of `from` and `to` emits with `extra`, by path and
/// whether each carries [`change::Flags::KeptBeside`], sorted.
async fn deletes_marked(
    repository: &Arc<RepositoryContext>,
    from: &Arc<State>,
    to: &Arc<State>,
    extra: DiffFlags,
) -> Vec<(String, bool)> {
    let root = |state: &Arc<State>| NodeChangeState {
        mapping: NodeMapping {
            repository: repository.clone(),
            state: state.clone(),
            path: RelativePath::new(),
            node: ROOT_NODE,
        },
        observed: None,
        flags: NodeFlags::NoFlags,
        address: Address::default(),
        mode: 0,
    };
    let (from, to) = (root(from), root(to));
    let mut walk = ChangeStream::spawn(async move |changes| {
        diff_subtree_flagged(
            from,
            to,
            RelativePath::new(),
            None,
            extra,
            &changes,
            FilterMode::Full,
        )
        .await
    });
    let mut deletes = Vec::new();
    while let Some(change) = walk.next().await {
        if change.action == change::FileAction::Delete {
            deletes.push((
                change.path().as_str().to_string(),
                change.flags.contains(change::Flags::KeptBeside),
            ));
        }
    }
    walk.finish().await.expect("a walk of the two states");
    deletes.sort();
    deletes
}

/// Under one view, a walk reports no add or delete of a directory whose emptiness the revision
/// changes, placing directories or not: the view keeps a directory or excludes it whatever it
/// holds, as under `*.bat`, which keeps `a/b`, and under `/*/` and `!/*/**/*.txt`, which
/// excludes it.
#[tokio::test]
async fn a_walk_under_one_view_reports_no_directory_whose_emptiness_changes() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let held = (&["a/b/y.bat", "top.txt"][..], &[][..]);
            let emptied = (&["top.txt"][..], &["a/b"][..]);
            let kept_below = (&["a/b/sub/x.txt", "top.txt"][..], &[][..]);
            let cases = [
                (&["*.bat"][..], held, emptied),
                (&["*.bat"][..], emptied, held),
                (&["/*/", "!/*/**/*.txt"][..], held, emptied),
                (&["/*/", "!/*/**/*.txt"][..], emptied, held),
                (&["/*/", "!/*/**/*.txt"][..], emptied, kept_below),
                (&[][..], held, emptied),
            ];
            for (index, (rules, from, to)) in cases.into_iter().enumerate() {
                let repository = repository_under_view(&immutable, &mutable, rules);
                let from = committed_tree(&repository, from.0, from.1).await;
                let to = committed_tree(&repository, to.0, to.1).await;
                for flags in [DiffFlags::PlacesDirectories, DiffFlags::empty()] {
                    let reported =
                        walk_flagged(&repository, &from, &to, flags, FilterMode::Full).await;
                    assert!(
                        reported.iter().all(|(_, path)| path != "a/b"),
                        "case {index}, {flags:?}: {reported:?}"
                    );
                }
            }
        })
        .await;
}

/// A walk placing directories marks a delete in a directory the view excludes that still holds a
/// file the view keeps, and no delete in one holding only what the view excludes. A walk not
/// placing directories marks none.
#[tokio::test]
async fn a_walk_placing_directories_marks_a_delete_beside_a_kept_file() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository = repository_under_view(&immutable, &mutable, &["/*/", "!/*/**/*.txt"]);
            let from = committed_tree(
                &repository,
                &["a/b/x.txt", "a/b/z.txt", "c/x.txt", "c/y.bat"],
                &[],
            )
            .await;
            let to = committed_tree(&repository, &["a/b/z.txt", "c/y.bat"], &[]).await;
            for (extra, marked) in [
                (DiffFlags::PlacesDirectories, &["a/b/x.txt"][..]),
                (DiffFlags::empty(), &[][..]),
            ] {
                let expected: Vec<(String, bool)> = ["a/b/x.txt", "c/x.txt"]
                    .iter()
                    .map(|path| (path.to_string(), marked.contains(path)))
                    .collect();
                assert_eq!(
                    deletes_marked(&repository, &from, &to, extra).await,
                    expected,
                    "{extra:?}"
                );
            }
        })
        .await;
}

/// A walk placing directories reads no kept file for a delete at the root, which no realize
/// removes, nor for one in a directory the view keeps, which stays on disk.
#[tokio::test]
async fn a_walk_placing_directories_reads_no_kept_file_where_the_directory_stays() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository =
                repository_under_view(&immutable, &mutable, &["/*/", "!/*/**/*.txt", "!/n/"]);
            let from = committed_tree(
                &repository,
                &["n/x.txt", "n/z.txt", "r.txt", "top.txt"],
                &[],
            )
            .await;
            let to = committed_tree(&repository, &["n/z.txt", "top.txt"], &[]).await;
            assert_eq!(
                deletes_marked(&repository, &from, &to, DiffFlags::PlacesDirectories).await,
                [("n/x.txt".to_string(), false), ("r.txt".to_string(), false)]
            );
        })
        .await;
}

/// A walk placing directories marks a delete in a directory the view excludes that still holds a
/// directory the view keeps, as one holding a file it keeps.
#[tokio::test]
async fn a_walk_placing_directories_marks_a_delete_beside_a_placed_directory() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let repository =
                repository_under_view(&immutable, &mutable, &["/*/", "!/*/**/*.txt", "!/a/b/c/"]);
            let from = committed_tree(&repository, &["a/b/x.txt", "a/b/c/k.bat"], &[]).await;
            let to = committed_tree(&repository, &["a/b/c/k.bat"], &[]).await;
            assert_eq!(
                deletes_marked(&repository, &from, &to, DiffFlags::PlacesDirectories).await,
                [("a/b/x.txt".to_string(), true)]
            );
        })
        .await;
}
