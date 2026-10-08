// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::node::NodeID;
use lore_revision::util::fan_out::*;
use lore_revision::util::path::DepthPath;
use lore_revision::util::path::RelativePath;
use lore_revision::util::path::path_depth;
use tokio::task::JoinSet;

use crate::fs::filesystem_provider::setup_test_execution;

fn antichain(targets: &[&str]) -> Vec<RelativePath> {
    RelativePath::dedup_to_supersets(
        targets
            .iter()
            .map(|path| RelativePath::new_from_initial_path(path).expect("Path init failed"))
            .collect(),
    )
}

fn derived(targets: &[&str]) -> Vec<String> {
    shared_ancestors(&antichain(targets))
        .iter()
        .map(|ancestor| ancestor.path().to_string())
        .collect()
}

/// Every ancestor of every target, counted. What the scan over neighbours
/// arrives at without the counting.
fn counted(targets: &[RelativePath]) -> Vec<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for target in targets {
        let mut ancestor = target.clone();
        ancestor.pop();
        while !ancestor.is_empty() {
            *counts.entry(ancestor.as_str().to_string()).or_insert(0) += 1;
            ancestor.pop();
        }
    }
    let mut shared: Vec<String> = counts
        .into_iter()
        .filter_map(|(path, count)| (count >= 2).then_some(path))
        .collect();
    shared.sort_unstable_by(|a, b| path_depth(a).cmp(&path_depth(b)).then_with(|| a.cmp(b)));
    shared
}

#[test]
fn only_a_directory_two_targets_share_is_returned() {
    assert!(derived(&[]).is_empty());
    assert!(derived(&["a/b/c"]).is_empty());
    assert!(derived(&["a/x", "b/y"]).is_empty());
    assert_eq!(derived(&["a/x", "a/y"]), vec!["a"]);
}

#[test]
fn the_result_is_prefix_closed() {
    assert_eq!(derived(&["a/b/c/x", "a/b/c/y"]), vec!["a", "a/b", "a/b/c"]);
}

#[test]
fn one_depth_is_a_contiguous_range_shallowest_first() {
    let shared = shared_ancestors(&antichain(&[
        "a/p/x", "a/p/y", "a/q/x", "a/q/y", "b/p/x", "b/p/y",
    ]));
    let paths: Vec<&str> = shared.iter().map(DepthPath::path).collect();
    assert_eq!(paths, vec!["a", "b", "a/p", "a/q", "b/p"]);
    let depths: Vec<usize> = shared.iter().map(DepthPath::depth).collect();
    assert!(depths.windows(2).all(|pair| pair[0] <= pair[1]));
}

/// Two case variations of one directory would be two ancestors naming one
/// node, and the depth holding both would add it twice. The target set they
/// are taken from settles on one, and that is what carries into here.
#[test]
fn targets_on_one_case_variation_give_ancestors_on_one() {
    assert_eq!(
        derived(&["Assets/Meshes/a", "assets/meshes/b", "ASSETS/Meshes/c"]),
        vec!["Assets", "Assets/Meshes"]
    );
}

/// The scan reads neighbours, so the shapes that matter are the ones where
/// lexicographic order puts something unrelated between two targets, or
/// where a shared string prefix stops inside a component.
#[test]
fn it_agrees_with_counting_every_ancestor() {
    for targets in [
        &[][..],
        &["a/b/c"],
        &["a/x", "a/y"],
        &["a/x", "b/y"],
        &["a/b/c/d/x", "a/b/c/d/y"],
        &["a/p/x", "a/p/y", "a/q/x", "a/q/y", "b/p/x", "b/p/y"],
        // '-' sorts below '/', so this lands between "a" and its subtree.
        &["a/x", "a-foo/y", "a/z"],
        // '0' sorts above '/', so this lands after the subtree.
        &["a/x", "a0/y", "a/z"],
        // A shared string prefix that stops inside a component.
        &["a/b/x", "a/bc/y", "a/b/z"],
        // A directory target covering the files beneath it.
        &["a/b", "a/b/x", "a/b/y", "a/c/x", "a/c/y"],
        // Three levels, and a lone target beside them.
        &["t/m/l/f", "t/m/l/g", "t/m/n/f", "t/m/n/g", "u/v/w/x"],
    ] {
        let antichain = antichain(targets);
        let derived: Vec<String> = shared_ancestors(&antichain)
            .iter()
            .map(|ancestor| ancestor.path().to_string())
            .collect();
        assert_eq!(derived, counted(&antichain), "{targets:?}");
    }
}

/// A subset of an antichain is an antichain in the same order, so the targets can
/// be handed over as borrowed paths rather than copied.
#[test]
fn borrowed_targets_give_the_same_ancestors() {
    let antichain = antichain(&["a/p/x", "a/p/y", "b/q/x", "b/q/y"]);
    let borrowed: Vec<&RelativePath> = antichain.iter().collect();
    let paths = |ancestors: Vec<DepthPath>| -> Vec<String> {
        ancestors
            .iter()
            .map(|ancestor| ancestor.path().to_string())
            .collect()
    };
    assert_eq!(
        paths(shared_ancestors(&borrowed)),
        paths(shared_ancestors(&antichain))
    );
}

const NODE_A: NodeID = 11;
const NODE_AB: NodeID = 22;

fn created<'a>(entries: &[(&'a str, NodeID)]) -> AncestorNodes<'a> {
    entries.iter().copied().collect()
}

#[test]
fn longest_ancestor_takes_the_deepest_one_created() {
    let nodes = created(&[("a", NODE_A), ("a/b", NODE_AB)]);
    assert_eq!(longest_ancestor("a/b/c/d", &nodes), Some(("a/b", NODE_AB)));
    assert_eq!(
        longest_ancestor("a/b", &nodes),
        Some(("a", NODE_A)),
        "a path in the map starts above itself"
    );
    assert_eq!(longest_ancestor("a", &nodes), None);
    assert_eq!(longest_ancestor("x/y", &nodes), None);
}

#[derive(Debug, PartialEq)]
enum CreateError {
    Failed(String),
    Joined,
}

type Creation = BoxFuture<'static, Result<Option<NodeID>, CreateError>>;

/// A node id standing for `path` alone, so a test can tell which ancestor a node
/// was created for.
fn node_for(path: &str) -> NodeID {
    path.bytes()
        .fold(1, |node, byte| node * 31 + NodeID::from(byte))
}

/// Each ancestor and the deepest one above it a creation was handed a node for.
type Started = Vec<(String, Option<String>)>;

fn started(path: &str, nodes: &AncestorNodes<'_>, record: &mut Started) {
    let parent = longest_ancestor(path, nodes).map(|(parent, node)| {
        assert_eq!(
            node,
            node_for(parent),
            "the node handed over is the parent's"
        );
        parent.to_string()
    });
    record.push((path.to_string(), parent));
}

#[tokio::test]
async fn each_level_is_created_from_the_nodes_of_the_level_above() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let ancestors = shared_ancestors(&antichain(&[
                "a/p/x", "a/p/y", "a/q/x", "a/q/y", "b/p/x", "b/p/y",
            ]));
            let mut record = Started::new();
            let nodes = create_shared_ancestors(
                &ancestors,
                |path, nodes| {
                    started(path, nodes, &mut record);
                    let node = node_for(path);
                    async move { Ok(Some(node)) }.boxed()
                },
                |_| CreateError::Joined,
            )
            .await
            .expect("Every creation succeeded");

            let mut created: Vec<(&str, NodeID)> = nodes.into_iter().collect();
            created.sort_unstable();
            let mut expected: Vec<(&str, NodeID)> = ["a", "a/p", "a/q", "b", "b/p"]
                .into_iter()
                .map(|path| (path, node_for(path)))
                .collect();
            expected.sort_unstable();
            assert_eq!(created, expected);

            record.sort_unstable();
            assert_eq!(
                record,
                vec![
                    ("a".to_string(), None),
                    ("a/p".to_string(), Some("a".to_string())),
                    ("a/q".to_string(), Some("a".to_string())),
                    ("b".to_string(), None),
                    ("b/p".to_string(), Some("b".to_string())),
                ]
            );
        })
        .await;
}

#[tokio::test]
async fn an_ancestor_created_with_no_node_is_left_out() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let ancestors = shared_ancestors(&antichain(&["a/p/x", "a/p/y"]));
            let mut record = Started::new();
            let nodes = create_shared_ancestors(
                &ancestors,
                |path, nodes| {
                    started(path, nodes, &mut record);
                    let node = (path != "a").then(|| node_for(path));
                    async move { Ok(node) }.boxed()
                },
                |_| CreateError::Joined,
            )
            .await
            .expect("Every creation succeeded");

            assert!(!nodes.contains_key("a"));
            assert_eq!(nodes.get("a/p"), Some(&node_for("a/p")));
            assert_eq!(
                record,
                vec![("a".to_string(), None), ("a/p".to_string(), None)],
                "a level below starts from wherever the node map leaves it"
            );
        })
        .await;
}

/// A failure stops every level below it, since those would be created under a
/// parent that may not exist.
#[tokio::test]
async fn the_first_failure_is_returned_and_no_level_below_it_starts() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let ancestors = shared_ancestors(&antichain(&["a/p/x", "a/p/y", "b/p/x", "b/p/y"]));
            let mut record = Started::new();
            let result = create_shared_ancestors(
                &ancestors,
                |path, nodes| {
                    started(path, nodes, &mut record);
                    let outcome = if path == "b" {
                        Err(CreateError::Failed(path.to_string()))
                    } else {
                        Ok(Some(node_for(path)))
                    };
                    async move { outcome }.boxed()
                },
                |_| CreateError::Joined,
            )
            .await;

            assert_eq!(result, Err(CreateError::Failed("b".to_string())));
            assert!(
                record.iter().all(|(path, _)| path_depth(path) == 1),
                "no creation below the failed level started: {record:?}"
            );
        })
        .await;
}

/// A creation in flight is allocating nodes, so the failure of another in its
/// level waits for it rather than cancelling it. The failure lands only once the
/// survivor has been asked for, and the survivor finishes only after the failure
/// has landed, so it is reached only through that drain.
#[tokio::test]
async fn a_creation_in_flight_finishes_after_another_fails() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let ancestors = shared_ancestors(&antichain(&["a/x", "a/y", "b/x", "b/y"]));
            let (survivor_asked, survivor_asked_for) = tokio::sync::oneshot::channel::<()>();
            let (failed, failure_landed) = tokio::sync::oneshot::channel::<()>();
            let mut survivor_asked = Some(survivor_asked);
            let mut pending_failure = Some((survivor_asked_for, failed));
            let mut failure_landed = Some(failure_landed);
            let finished = Arc::new(AtomicBool::new(false));

            let result = create_shared_ancestors(
                &ancestors,
                |path, _| -> Creation {
                    if path == "a" {
                        let (survivor_asked_for, failed) =
                            pending_failure.take().expect("One creation of a");
                        async move {
                            let _ = survivor_asked_for.await;
                            drop(failed);
                            Err(CreateError::Failed("a".to_string()))
                        }
                        .boxed()
                    } else {
                        let _ = survivor_asked.take().expect("One creation of b").send(());
                        let failure_landed = failure_landed.take().expect("One creation of b");
                        let finished = finished.clone();
                        async move {
                            let _ = failure_landed.await;
                            finished.store(true, Ordering::Release);
                            Ok(Some(node_for("b")))
                        }
                        .boxed()
                    }
                },
                |_| CreateError::Joined,
            )
            .await;

            assert_eq!(result, Err(CreateError::Failed("a".to_string())));
            assert!(
                finished.load(Ordering::Acquire),
                "the creation in flight was cancelled rather than drained"
            );
        })
        .await;
}

#[tokio::test]
async fn a_creation_that_panics_is_reported_through_the_join_failure() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let ancestors = shared_ancestors(&antichain(&["a/x", "a/y"]));
            let result = create_shared_ancestors(
                &ancestors,
                |_, _| async { panic!("the creation panicked") }.boxed(),
                |_| CreateError::Joined,
            )
            .await;

            assert_eq!(result, Err(CreateError::Joined));
        })
        .await;
}

#[derive(Debug, PartialEq)]
enum JoinFailure {
    Refused(u32),
    Joined,
}

/// A task that has finished is joined below the limit as well, without waiting on the rest:
/// each call returns at once, and one of them joins the task once it has finished.
#[tokio::test]
async fn join_below_joins_what_has_finished_without_waiting_below_the_limit() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mut tasks = JoinSet::new();
            lore_spawn!(tasks, std::future::pending::<u32>());
            lore_spawn!(tasks, async { 7 });
            let mut joined = Vec::new();
            let mut failure = None;

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while joined.is_empty() && std::time::Instant::now() < deadline {
                join_below(
                    &mut tasks,
                    3,
                    &mut failure,
                    |_| JoinFailure::Joined,
                    |output| {
                        joined.push(output);
                        Ok(())
                    },
                )
                .await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }

            assert_eq!(joined, vec![7]);
            assert_eq!(tasks.len(), 1);
        })
        .await;
}

/// Tasks that never finish wait out the call, so it joins the one that finishes late, and
/// returns once fewer than the limit remain.
#[tokio::test]
async fn join_below_waits_until_fewer_than_the_limit_remain() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mut tasks = JoinSet::new();
            lore_spawn!(tasks, std::future::pending::<u32>());
            lore_spawn!(tasks, std::future::pending::<u32>());
            lore_spawn!(tasks, async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                7
            });
            let mut joined = Vec::new();
            let mut failure = None;

            join_below(
                &mut tasks,
                3,
                &mut failure,
                |_| JoinFailure::Joined,
                |output| {
                    joined.push(output);
                    Ok(())
                },
            )
            .await;

            assert_eq!(joined, vec![7]);
            assert_eq!(tasks.len(), 2);
            assert_eq!(failure, None);
        })
        .await;
}

#[tokio::test]
async fn join_below_with_a_limit_of_one_joins_every_task() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mut tasks = JoinSet::new();
            for output in 0..5u32 {
                lore_spawn!(tasks, async move { output });
            }
            let mut joined = Vec::new();
            let mut failure = None;

            join_below(
                &mut tasks,
                1,
                &mut failure,
                |_| JoinFailure::Joined,
                |output| {
                    joined.push(output);
                    Ok(())
                },
            )
            .await;

            joined.sort_unstable();
            assert_eq!(joined, vec![0, 1, 2, 3, 4]);
            assert!(tasks.is_empty());
            assert_eq!(failure, None);
        })
        .await;
}

/// A task that panics and an output the collector refuses are each a failure, and the outputs
/// beside them are still collected.
#[tokio::test]
async fn join_below_reports_a_panic_or_a_refusal_and_collects_the_rest() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mut tasks = JoinSet::new();
            lore_spawn!(tasks, async { 1u32 });
            lore_spawn!(tasks, async { panic!("the task panicked") });
            let mut joined = Vec::new();
            let mut failure = None;
            join_below(
                &mut tasks,
                1,
                &mut failure,
                |_| JoinFailure::Joined,
                |output| {
                    joined.push(output);
                    Ok(())
                },
            )
            .await;
            assert_eq!(joined, vec![1]);
            assert_eq!(failure, Some(JoinFailure::Joined));

            for output in [2u32, 3] {
                lore_spawn!(tasks, async move { output });
            }
            let mut joined = Vec::new();
            let mut failure = None;
            join_below(
                &mut tasks,
                1,
                &mut failure,
                |_| JoinFailure::Joined,
                |output| {
                    if output == 3 {
                        return Err(JoinFailure::Refused(output));
                    }
                    joined.push(output);
                    Ok(())
                },
            )
            .await;
            assert_eq!(joined, vec![2]);
            assert_eq!(failure, Some(JoinFailure::Refused(3)));
        })
        .await;
}

/// A failure already kept is the one a later call leaves in place.
#[tokio::test]
async fn join_below_keeps_the_first_failure() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let mut tasks = JoinSet::new();
            let mut failure = None;
            for output in [1u32, 2] {
                lore_spawn!(tasks, async move { output });
                join_below(
                    &mut tasks,
                    1,
                    &mut failure,
                    |_| JoinFailure::Joined,
                    |output| Err(JoinFailure::Refused(output)),
                )
                .await;
            }

            assert_eq!(failure, Some(JoinFailure::Refused(1)));
        })
        .await;
}
