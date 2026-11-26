// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::FutureExt;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::RepositoryId;
use lore_revision::filter::FilterMode;
use lore_revision::node::INVALID_NODE;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::clone::order::Order;
use lore_revision::repository::clone::order::OrderCounts;
use lore_revision::repository::clone::order::OrderKey;
use lore_revision::repository::clone::order::OrderLimits;
use lore_revision::repository::clone::order::Release;
use lore_revision::repository::clone::*;
use lore_revision::stage::stage_single_node;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenWithNameIterator;
use lore_revision::test_util::DiscoveryStep;
use lore_revision::test_util::step_discovery;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use zerocopy::FromZeros;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::repository::test_helpers::working_tree_repository_filtered;

/// Files a segment walk reserves: the nodes of a block.
const SEGMENT: usize = lore_revision::node::BLOCK_NODE_COUNT;

/// Files of `fill/`, which pushes what is staged after it into the next node block.
const FILLER_FILES: usize = 3 * SEGMENT + 16;

/// How long a walk that should finish may take before the test fails rather than hangs.
const FINISHES_WITHIN: Duration = Duration::from_secs(30);

fn relative_path(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("Relative path")
}

/// An empty file whose content needs no store.
fn empty_file() -> Node {
    let mut node = Node::new_zeroed();
    node.flags = NodeFlags::File.bits();
    node
}

/// A link to a repository no remote serves, so a clone gives it up.
fn unreachable_link() -> Node {
    let mut node = Node::new_zeroed();
    node.flags = NodeFlags::Link.bits();
    node.address.context = RepositoryId::from([7; 16]).into();
    node
}

fn limits(window: usize, cap: usize, low_water: usize) -> OrderLimits {
    OrderLimits {
        window,
        cap,
        low_water,
    }
}

/// A key at each position of a chain below the root, past the root's own segment.
fn keys(count: u32) -> Vec<OrderKey> {
    (0..count)
        .map(|position| OrderKey::root().child_segment(position))
        .collect()
}

/// Whether `wake` was notified since it was last consumed.
fn woken(wake: &Notify) -> bool {
    wake.notified().now_or_never().is_some()
}

/// A walked file at `path`, as discovery hands one to the order.
fn walked_file(repository: &Arc<RepositoryContext>, path: &str) -> CloneWorkItem {
    CloneWorkItem {
        repository: repository.clone(),
        node: empty_file(),
        repository_path: relative_path(path),
    }
}

async fn stage(repository: &Arc<RepositoryContext>, state: &Arc<State>, path: &str, node: Node) {
    stage_single_node(
        repository.clone(),
        state.clone(),
        relative_path(path),
        node,
        Arc::default(),
        None,
        FilterMode::empty(),
    )
    .await
    .expect("stage the node");
}

/// The node at `path` below the root.
async fn node_at(repository: &Arc<RepositoryContext>, state: &Arc<State>, path: &str) -> NodeID {
    let mut node = ROOT_NODE;
    for name in path.split('/') {
        node = state
            .find_subnode(repository.clone(), node, hash_string(name))
            .await
            .expect("the node resolves");
    }
    node
}

/// Sets the first child of the directory `node` where `first_child_of`, and otherwise the sibling
/// of `node`, to `to`, or to none.
async fn rewire(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    node: NodeID,
    first_child_of: bool,
    to: Option<NodeID>,
) {
    let block_index = NodeBlock::index(node);
    let block = state
        .block_with_nametable(repository.clone(), block_index)
        .await
        .expect("the block reads back");
    {
        let mut writer = block.write_for_tests();
        let written = writer.node(Node::index(node));
        let link = to.unwrap_or(INVALID_NODE);
        if first_child_of {
            written.child = link;
        } else {
            written.sibling = link;
        }
        writer.mark_dirty();
    }
    state.block_modified_for_tests(block, block_index);
}

/// A state whose tree holds, at the root: `a/` with three files and a subdirectory `a/s/` holding
/// one, `skip/` with files the view `/skip/` excludes, the empty `empty/`, an unreachable link
/// `link`, [`FILLER_FILES`] files in `fill/`, `z/` holding `x0`, `x1` and `x2`, and files `r0` to
/// `r3`. `z`'s chain runs `x1`, `x0`, `x2`, with `x0` in the root's first node block and `x1` and
/// `x2` past `fill/` in the next, so it leaves a block and comes back.
async fn ordered_tree(repository: &Arc<RepositoryContext>) -> Arc<State> {
    let state = State::new();
    for directory in ["a", "a/s", "skip", "empty", "fill", "z"] {
        stage(repository, &state, directory, Node::new_zeroed()).await;
    }
    for file in [
        "a/f1", "a/f2", "a/f3", "a/s/g", "skip/k1", "skip/k2", "z/x0",
    ] {
        stage(repository, &state, file, empty_file()).await;
    }
    stage(repository, &state, "link", unreachable_link()).await;
    for index in 0..FILLER_FILES {
        stage(
            repository,
            &state,
            &format!("fill/f{index:04}"),
            empty_file(),
        )
        .await;
    }
    for file in ["z/x1", "z/x2", "r0", "r1", "r2", "r3"] {
        stage(repository, &state, file, empty_file()).await;
    }
    let [x0, x1, x2, z] = [
        node_at(repository, &state, "z/x0").await,
        node_at(repository, &state, "z/x1").await,
        node_at(repository, &state, "z/x2").await,
        node_at(repository, &state, "z").await,
    ];
    assert_ne!(NodeBlock::index(x0), NodeBlock::index(x1));
    assert_eq!(NodeBlock::index(x1), NodeBlock::index(x2));
    rewire(repository, &state, z, true, Some(x1)).await;
    rewire(repository, &state, x1, false, Some(x0)).await;
    rewire(repository, &state, x0, false, Some(x2)).await;
    rewire(repository, &state, x2, false, None).await;
    state
}

/// The files below the root in depth-first tree order, each directory's files in chain order
/// before its subdirectories, which follow in chain order. Directories named `skip` are left out.
async fn tree_order(repository: &Arc<RepositoryContext>, state: &Arc<State>) -> Vec<String> {
    let mut order = Vec::new();
    let mut directories = vec![(ROOT_NODE, RelativePath::new())];
    while let Some((directory, path)) = directories.pop() {
        let mut children =
            StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), directory)
                .await
                .expect("list the directory");
        let mut subdirectories = Vec::new();
        while let Some((child, node, name)) = children.next().await.expect("list an entry") {
            let child_path = path.join(name);
            if node.is_file() {
                order.push(child_path.to_string());
            } else if node.is_directory() && !child_path.as_str().ends_with("skip") {
                subdirectories.push((child, child_path));
            }
        }
        directories.extend(subdirectories.into_iter().rev());
    }
    order
}

/// A clone context over `repository` and `state`, with its own statistics.
async fn clone_context(repository: Arc<RepositoryContext>, state: Arc<State>) -> CloneContext {
    CloneContext {
        operation: repository
            .file_system()
            .begin_operation()
            .await
            .expect("Starting test operation"),
        repository,
        state,
        options: Arc::default(),
        stats: Arc::default(),
        modified_times: Arc::default(),
    }
}

/// Discovers what `ctx` clones within `limits` through a work channel of `capacity`, answering
/// the paths in the order the writers received them with the walk's result. The receiver waits
/// until `holds_back` answers false before it takes the first file.
async fn discovered(
    ctx: CloneContext,
    limits: OrderLimits,
    capacity: usize,
    holds_back: impl Fn(&CloneStats) -> bool,
) -> (Vec<String>, Result<OrderCounts, CloneError>) {
    let stats = ctx.stats.clone();
    let (tx, mut rx) = mpsc::channel::<CloneWorkItem>(capacity);
    let receive = async {
        while holds_back(&stats) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut received = Vec::new();
        while let Some(item) = rx.recv().await {
            received.push(item.repository_path.to_string());
        }
        received
    };
    let (received, walked) = tokio::time::timeout(FINISHES_WITHIN, async {
        tokio::join!(receive, discover(ctx, limits, tx))
    })
    .await
    .expect("the walk finishes");
    (received, walked)
}

/// An order key puts a directory's segments in chain order before the subtrees of its entries, and
/// the subtrees in chain order after them, the whole of each before the next.
#[test]
fn an_order_key_puts_a_directorys_segments_before_its_subtrees() {
    let root = OrderKey::root();
    let continued = root.continuation(700);
    let [second, third] = [root.child_segment(2), root.child_segment(3)];
    let third_continued = third.continuation(600);
    let below_third = third.child_segment(0);

    assert_eq!(continued.segment_start(), 700);
    assert_eq!(root.subtree(3).first_segment(), third);
    let keys = [
        &root,
        &continued,
        &second,
        &third,
        &third_continued,
        &below_third,
        &root.child_segment(4),
    ];
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "{keys:?}");
    assert!(root.subtree(3) < third && third < root.subtree(4));
    assert_eq!(
        OrderKey::default().child_segment(3),
        OrderKey::default(),
        "a walk without an order derives no order"
    );
}

/// An order starts only the smallest pending keys its window holds: a key smaller than the window's
/// largest takes its place, and the next smallest key joins as one starts, its task woken.
#[test]
fn an_order_starts_only_the_smallest_pending_keys_in_its_window() {
    let order = Order::new(limits(2, usize::MAX, 0));
    let [k1, k2, k3] = <[OrderKey; 3]>::try_from(keys(3)).expect("three keys");
    let [w1, w2, w3] = [(); 3].map(|()| Arc::new(Notify::new()));
    order.insert(k3.clone(), &w3);
    order.insert(k1.clone(), &w1);
    assert!(woken(&w3));
    assert!(woken(&w1));
    order.insert(k2.clone(), &w2);
    assert!(woken(&w2), "a smaller key joins a full window");

    assert_eq!(
        order.start(&k3, &w3),
        None,
        "the largest key left the window"
    );
    assert_eq!(order.start(&k2, &w2), Some(SEGMENT));
    assert!(woken(&w3), "the next pending key joins the window");
    assert_eq!(order.start(&k3, &w3), Some(SEGMENT));
}

/// A key that cannot reserve a segment waits, while the first entry starts with no reservation, and
/// the waiting key's task is woken once the first entry is released, which makes it the first.
#[tokio::test]
async fn an_order_wakes_a_key_waiting_on_its_cap_once_the_first_entry_is_released() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = crate::branch::null_repository().await;
            let order = Order::new(limits(8, 1, 0));
            let [k1, k2] = <[OrderKey; 2]>::try_from(keys(2)).expect("two keys");
            let [w1, w2] = [(); 2].map(|()| Arc::new(Notify::new()));
            order.insert(k1.clone(), &w1);
            order.insert(k2.clone(), &w2);
            let _ = (woken(&w1), woken(&w2));

            assert_eq!(order.start(&k2, &w2), None, "no segment fits the cap");
            assert_eq!(
                order.start(&k1, &w1),
                Some(0),
                "the first entry needs no reservation"
            );
            order.walked(&k1, vec![walked_file(&repository, "f")], 0);
            assert!(!woken(&w2));
            assert!(matches!(order.next(usize::MAX), Release::Files(files) if files.len() == 1));
            assert!(woken(&w2), "the waiting key became the first entry");
            assert_eq!(order.start(&k2, &w2), Some(0));
        })
        .await;
}

/// A walk that frees one segment of reserved files wakes one task waiting to reserve, the one that
/// waited longest, rather than every waiting task.
#[test]
fn an_order_wakes_one_task_waiting_on_its_cap_per_segment_freed() {
    let order = Order::new(limits(8, 2 * SEGMENT, 0));
    let keys = keys(5);
    let wakes: Vec<Arc<Notify>> = (0..5).map(|_| Arc::new(Notify::new())).collect();
    for (key, wake) in keys.iter().zip(&wakes) {
        order.insert(key.clone(), wake);
        let _ = woken(wake);
    }
    assert_eq!(order.start(&keys[0], &wakes[0]), Some(SEGMENT));
    assert_eq!(order.start(&keys[1], &wakes[1]), Some(SEGMENT));
    for index in 2..5 {
        assert_eq!(order.start(&keys[index], &wakes[index]), None);
    }

    order.walked(&keys[0], Vec::new(), SEGMENT);

    let notified: Vec<bool> = wakes[2..].iter().map(|wake| woken(wake)).collect();
    assert_eq!(notified, [true, false, false]);
}

/// A task waiting to reserve is queued once however often it asks, and a task that starts while
/// another segment fits wakes the next waiting task.
#[tokio::test]
async fn an_order_queues_a_waiting_task_once_and_each_start_wakes_the_next() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = crate::branch::null_repository().await;
            let order = Order::new(limits(8, 2 * SEGMENT, 0));
            let keys = keys(5);
            let wakes: Vec<Arc<Notify>> = (0..5).map(|_| Arc::new(Notify::new())).collect();
            for (key, wake) in keys.iter().zip(&wakes) {
                order.insert(key.clone(), wake);
                let _ = woken(wake);
            }
            assert_eq!(order.start(&keys[1], &wakes[1]), Some(SEGMENT));
            assert_eq!(order.start(&keys[2], &wakes[2]), Some(SEGMENT));
            for index in [3, 3, 4] {
                assert_eq!(order.start(&keys[index], &wakes[index]), None);
            }
            assert_eq!(order.start(&keys[0], &wakes[0]), Some(0));
            let files = (0..2 * SEGMENT)
                .map(|index| walked_file(&repository, &format!("f{index}")))
                .collect();
            order.walked(&keys[0], files, 0);
            order.walked(&keys[1], Vec::new(), SEGMENT);
            order.walked(&keys[2], Vec::new(), SEGMENT);
            assert!(!woken(&wakes[3]));

            assert!(
                matches!(order.next(usize::MAX), Release::Files(files) if files.len() == 2 * SEGMENT)
            );
            assert!(woken(&wakes[3]));
            assert!(!woken(&wakes[4]));
            assert_eq!(order.start(&keys[3], &wakes[3]), Some(SEGMENT));
            assert!(woken(&wakes[4]), "the start passes the wake on");
        })
        .await;
}

/// A release skips a task waiting to reserve whose item has since started as the first entry, with
/// no reservation, and wakes the next waiting task.
#[test]
fn an_order_skips_a_waiting_task_whose_item_started_as_the_first_entry() {
    let order = Order::new(limits(8, 2 * SEGMENT, 0));
    let keys = keys(5);
    let wakes: Vec<Arc<Notify>> = (0..5).map(|_| Arc::new(Notify::new())).collect();
    for (key, wake) in keys.iter().zip(&wakes) {
        order.insert(key.clone(), wake);
        let _ = woken(wake);
    }
    assert_eq!(order.start(&keys[2], &wakes[2]), Some(SEGMENT));
    assert_eq!(order.start(&keys[3], &wakes[3]), Some(SEGMENT));
    assert_eq!(order.start(&keys[0], &wakes[0]), Some(0));
    assert_eq!(order.start(&keys[1], &wakes[1]), None);
    assert_eq!(order.start(&keys[4], &wakes[4]), None);
    order.walked(&keys[0], Vec::new(), 0);
    assert!(woken(&wakes[1]), "the waiting key became the first entry");
    assert_eq!(order.start(&keys[1], &wakes[1]), Some(0));

    order.walked(&keys[2], Vec::new(), SEGMENT);

    assert!(!woken(&wakes[1]), "the started task is skipped");
    assert!(woken(&wakes[4]), "the next waiting task is woken");
}

/// A release skips the task of a link's walk that started its item and then ended, dropping its
/// `Notify`, and wakes the next live waiting task.
#[test]
fn an_order_skips_the_waiting_task_of_a_finished_link_walk() {
    let order = Order::new(limits(8, 2 * SEGMENT, 0));
    let keys = keys(5);
    let mut wakes: Vec<Option<Arc<Notify>>> =
        (0..5).map(|_| Some(Arc::new(Notify::new()))).collect();
    let wake =
        |wakes: &[Option<Arc<Notify>>], index: usize| wakes[index].clone().expect("a live task");
    for (index, key) in keys.iter().enumerate() {
        order.insert(key.clone(), &wake(&wakes, index));
        let _ = woken(&wake(&wakes, index));
    }
    assert_eq!(order.start(&keys[0], &wake(&wakes, 0)), Some(SEGMENT));
    assert_eq!(order.start(&keys[1], &wake(&wakes, 1)), Some(SEGMENT));
    for (index, key) in keys.iter().enumerate().skip(2) {
        assert_eq!(order.start(key, &wake(&wakes, index)), None);
    }
    order.walked(&keys[0], Vec::new(), SEGMENT);
    assert!(woken(&wake(&wakes, 2)));
    assert_eq!(
        order.start(&keys[3], &wake(&wakes, 3)),
        Some(SEGMENT),
        "the link walk's task starts its item ahead of the woken task"
    );
    wakes[3] = None;

    order.walked(&keys[1], Vec::new(), SEGMENT);

    assert!(
        woken(&wake(&wakes, 4)),
        "the next live waiting task is woken"
    );
}
/// An order releases a walked segment ahead of the first entry only while the work channel holds
/// fewer files than the low-water mark, and counts it.
#[tokio::test]
async fn an_order_releases_out_of_order_only_below_the_low_water_mark() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let repository = crate::branch::null_repository().await;
            let order = Order::new(limits(8, usize::MAX, 10));
            let [k1, k2] = <[OrderKey; 2]>::try_from(keys(2)).expect("two keys");
            let wake = Arc::new(Notify::new());
            order.insert(k1.clone(), &wake);
            order.insert(k2.clone(), &wake);
            assert_eq!(order.start(&k1, &wake), Some(SEGMENT));
            assert_eq!(order.start(&k2, &wake), Some(SEGMENT));
            order.walked(&k2, vec![walked_file(&repository, "f")], SEGMENT);

            assert!(matches!(order.next(10), Release::Wait));
            assert_eq!(order.counts().out_of_order, 0);
            assert!(matches!(order.next(9), Release::Files(files) if files.len() == 1));
            assert_eq!(order.counts().out_of_order, 1);
        })
        .await;
}

/// A clone hands its writers the files in depth-first tree order, each directory's files before
/// its subdirectories, across a chain that leaves its node block and comes back, an excluded
/// subtree, an empty directory and a link it gives up, at the clone's limits and at a window and cap
/// of one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_hands_its_writers_the_files_in_tree_order() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for (window, cap) in [(order::WINDOW, order::CAP), (1, 1)] {
                let temp = TempDir::new("lore-clone-order-");
                let repository = working_tree_repository_filtered(
                    temp.path(),
                    crate::helper::test_filter(&[], &["/skip/"]),
                )
                .await;
                let state = ordered_tree(&repository).await;
                let expected = tree_order(&repository, &state).await;
                let ctx = clone_context(repository, state).await;

                let (received, walked) =
                    discovered(ctx, limits(window, cap, 0), 64, |_| false).await;

                walked.expect("the walk succeeds");
                assert_eq!(received.len(), FILLER_FILES + 11);
                assert_eq!(received, expected, "window {window}, cap {cap}");
                assert!(temp.path().join("empty").is_dir());
            }
        })
        .await;
}

/// The files a clone's order holds never exceed its cap and one segment, at a cap no reservation
/// fits, one that one fits and one that two fit, though the tree holds more and the writers take
/// none until discovery stalls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_holds_at_most_its_cap_and_one_segment_of_files() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for cap in [1, SEGMENT, 2 * SEGMENT] {
                let temp = TempDir::new("lore-clone-order-cap-");
                let repository = working_tree_repository_filtered(
                    temp.path(),
                    crate::helper::test_filter(&[], &["/skip/"]),
                )
                .await;
                let state = ordered_tree(&repository).await;
                let expected = tree_order(&repository, &state).await;
                let ctx = clone_context(repository, state).await;

                let last_seen = std::sync::atomic::AtomicU64::new(u64::MAX);
                let stalls = std::sync::atomic::AtomicUsize::new(0);

                let (received, walked) =
                    discovered(ctx, limits(order::WINDOW, cap, 0), 1, |stats| {
                        let seen = stats.discovery.total_files.load(Ordering::Relaxed);
                        let stalled = last_seen.swap(seen, Ordering::Relaxed) == seen;
                        let stalled_for = if stalled {
                            stalls.fetch_add(1, Ordering::Relaxed) + 1
                        } else {
                            stalls.store(0, Ordering::Relaxed);
                            0
                        };
                        stalled_for < 10 && (seen as usize) < expected.len()
                    })
                    .await;

                let counts = walked.expect("the walk succeeds");
                assert_eq!(received, expected);
                assert!(expected.len() > cap + SEGMENT);
                assert!(
                    counts.most_reserved <= cap + SEGMENT,
                    "{} files reserved over a cap of {cap}",
                    counts.most_reserved
                );
            }
        })
        .await;
}

/// The work channel stays open until discovery is marked complete: parked after its walk and
/// before it marks itself complete, discovery still holds the channel's sender.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_marks_discovery_complete_before_its_work_channel_closes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-order-complete-");
            let repository = working_tree_repository_filtered(
                temp.path(),
                crate::helper::test_filter(&[], &["/skip/"]),
            )
            .await;
            let state = ordered_tree(&repository).await;
            let expected = tree_order(&repository, &state).await;
            let ctx = clone_context(repository, state).await;
            let stats = ctx.stats.clone();
            let (tx, mut rx) = mpsc::channel::<CloneWorkItem>(2 * expected.len());
            let (reached_tx, mut reached) = mpsc::unbounded_channel();
            let resume = Arc::new(Notify::new());
            let walk = step_discovery(
                reached_tx,
                resume.clone(),
                discover(ctx, limits(order::WINDOW, order::CAP, 0), tx),
            );
            let check = async {
                let step = reached.recv().await;
                let open_while_parked = !rx.is_closed();
                let complete_while_parked = stats.discovery.complete.load(Ordering::Relaxed);
                resume.notify_one();
                let mut received = 0;
                while rx.recv().await.is_some() {
                    received += 1;
                }
                (step, open_while_parked, complete_while_parked, received)
            };

            let (walked, (step, open_while_parked, complete_while_parked, received)) =
                tokio::time::timeout(FINISHES_WITHIN, async { tokio::join!(walk, check) })
                    .await
                    .expect("the walk finishes");

            walked.expect("the walk succeeds");
            assert_eq!(step, Some(DiscoveryStep::Walked));
            assert!(!complete_while_parked);
            assert!(
                open_while_parked,
                "the channel closed before discovery was complete"
            );
            assert_eq!(received, expected.len());
            assert!(stats.discovery.complete.load(Ordering::Relaxed));
        })
        .await;
}
/// A structural error in the middle of a walk at a window and cap of one ends the walk with that
/// error, closes the work channel and ends every block task, though they wait on the order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_mid_walk_ends_every_block_task_and_the_emitter() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-order-error-");
            let repository =
                working_tree_repository_filtered(temp.path(), crate::helper::test_filter(&[], &[]))
                    .await;
            let state = ordered_tree(&repository).await;
            let x2 = node_at(&repository, &state, "z/x2").await;
            let a = node_at(&repository, &state, "a").await;
            rewire(&repository, &state, x2, false, Some(a)).await;
            let held = Arc::strong_count(&repository);
            let ctx = clone_context(repository.clone(), state).await;

            let (_, walked) = discovered(ctx, limits(1, 1, 0), 64, |_| false).await;

            let error = walked.expect_err("the walk reaches a node of another directory");
            assert!(error.is_invalid_node_hierarchy(), "{error:?}");
            tokio::time::timeout(FINISHES_WITHIN, async {
                while Arc::strong_count(&repository) > held {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("every block task ends");
        })
        .await;
}

/// A clone whose discovery fails on a structural error, and whose writers write every file they
/// received, reports the discovery error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_reports_a_structural_error_its_writers_did_not_precede() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let temp = TempDir::new("lore-clone-order-precedence-");
            let repository =
                working_tree_repository_filtered(temp.path(), crate::helper::test_filter(&[], &[]))
                    .await;
            let state = ordered_tree(&repository).await;
            let x2 = node_at(&repository, &state, "z/x2").await;
            let a = node_at(&repository, &state, "a").await;
            rewire(&repository, &state, x2, false, Some(a)).await;
            let ctx = clone_context(repository, state).await;

            let error = clone_in_path(ctx)
                .await
                .expect_err("the walk reaches a node of another directory");

            assert!(error.is_invalid_node_hierarchy(), "{error:?}");
        })
        .await;
}
