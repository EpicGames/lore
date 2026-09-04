// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Timing for descending a large tree, the two ways a walk can ask the filter.
//!
//! Both strategies are timed in one binary over one tree, so the difference is
//! the filter call and nothing else -- no build, allocator or layout variance
//! between them:
//!
//! - **whole path**: `excludes_tree` per node, which is what every walk did
//!   before the descent split. It folds the node's ancestors, taking the shared
//!   part from the [`AncestorMemo`], then steps the last component.
//! - **threaded**: `child_excludes_tree` per node, carrying the parent's
//!   `FilterStates` down. One step, no fold, no memo.
//!
//! Both walks prune a directory on the same verdict and count the same files;
//! the run asserts that before reporting either number.
//!
//! Run with:
//!     `cargo test -p lore-revision --release --test filter_descend_bench -- --nocapture`

use std::time::Instant;

use lore_revision::filter::Filter;
use lore_revision::filter::FilterMode;
use lore_revision::filter::FilterStates;
use lore_revision::util::path::RelativePathBuf;

#[path = "support/filter_workload.rs"]
mod workload;

use workload::TreeNode;
use workload::rules;
use workload::tree;

/// The filter as a repository holds one: the hundred-line rule set in the ignore
/// slot, view empty.
fn build() -> Filter {
    let mut filter = Filter::default();
    for rule in rules() {
        match rule.strip_prefix('!') {
            Some(rest) => filter.ignore.add_inclusion(rest).expect("inclusion"),
            None => filter.ignore.add_exclusion(&rule).expect("exclusion"),
        }
    }
    filter
}

/// Workload multiplier, from `LORE_BENCH_SCALE`.
///
/// The default keeps this file inside the ordinary test suite's budget, where
/// what earns its place is the assertion that the two strategies agree on every
/// tree. The published measurements used `LORE_BENCH_SCALE=8`.
fn scale() -> usize {
    std::env::var("LORE_BENCH_SCALE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
}

/// What a walk did, so the two strategies can be held against each other.
#[derive(PartialEq, Eq, Debug)]
struct WalkResult {
    /// Files the walk kept.
    files: usize,
    /// Nodes it asked the filter about, which is where the time goes.
    queries: usize,
}

/// Descends `nodes`, asking `excludes_tree` for one whole path per node.
///
/// The pre-split shape: nothing is carried down, so every node's answer starts
/// from its ancestors.
fn descend_whole_paths(
    filter: &Filter,
    nodes: &[TreeNode],
    mode: FilterMode,
    at: usize,
    path: &mut RelativePathBuf,
    result: &mut WalkResult,
) {
    for &child in &nodes[at].children {
        let child = child as usize;
        path.push(&nodes[child].name);
        result.queries += 1;
        if !filter.excludes_tree(&*path, nodes[child].is_dir, mode) {
            if nodes[child].is_dir {
                descend_whole_paths(filter, nodes, mode, child, path, result);
            } else {
                result.files += 1;
            }
        }
        path.pop();
    }
}

fn walk_whole_paths(filter: &Filter, nodes: &[TreeNode], mode: FilterMode) -> WalkResult {
    let mut result = WalkResult {
        files: 0,
        queries: 0,
    };
    let mut path = RelativePathBuf::new();
    descend_whole_paths(filter, nodes, mode, 0, &mut path, &mut result);
    result
}

/// Descends `nodes`, stepping the parent's states with `child_excludes_tree`.
fn descend_threaded(
    filter: &Filter,
    nodes: &[TreeNode],
    mode: FilterMode,
    at: usize,
    states: FilterStates,
    path: &mut RelativePathBuf,
    result: &mut WalkResult,
) {
    for &child in &nodes[at].children {
        let child = child as usize;
        path.push(&nodes[child].name);
        result.queries += 1;
        let (child_states, excluded) =
            filter.child_excludes_tree(states, &*path, nodes[child].is_dir, mode);
        if !excluded {
            if nodes[child].is_dir {
                descend_threaded(filter, nodes, mode, child, child_states, path, result);
            } else {
                result.files += 1;
            }
        }
        path.pop();
    }
}

fn walk_threaded(filter: &Filter, nodes: &[TreeNode], mode: FilterMode) -> WalkResult {
    let mut result = WalkResult {
        files: 0,
        queries: 0,
    };
    let mut path = RelativePathBuf::new();
    descend_threaded(
        filter,
        nodes,
        mode,
        0,
        FilterStates::ROOT,
        &mut path,
        &mut result,
    );
    result
}

fn report(label: &str, rounds: usize, queries: usize, elapsed: std::time::Duration) -> f64 {
    let per_query = elapsed.as_nanos() as f64 / (rounds * queries) as f64;
    let per_walk = elapsed.as_secs_f64() / rounds as f64;
    println!(
        "  {label:<12} {per_query:>7.1} ns/node   {:>8.1} ms/walk   {:>7.2} M nodes/s",
        per_walk * 1_000.0,
        queries as f64 / per_walk / 1_000_000.0
    );
    per_query
}

/// A cold filter per round, so the ancestor memo is built from nothing the way
/// it is on a real operation, which opens the repository and walks once.
fn timed_cold(
    rounds: usize,
    nodes: &[TreeNode],
    mode: FilterMode,
    walk: impl Fn(&Filter, &[TreeNode], FilterMode) -> WalkResult,
) -> (WalkResult, std::time::Duration) {
    let mut total = std::time::Duration::ZERO;
    let mut last = None;
    for _ in 0..rounds {
        let filter = build();
        let start = Instant::now();
        let result = walk(&filter, nodes, mode);
        total += start.elapsed();
        last = Some(result);
    }
    (last.expect("at least one round"), total)
}

/// A filter reused across rounds, so the memo is warm from the first round on --
/// the best case for the whole-path strategy.
fn timed_warm(
    rounds: usize,
    nodes: &[TreeNode],
    mode: FilterMode,
    walk: impl Fn(&Filter, &[TreeNode], FilterMode) -> WalkResult,
) -> (WalkResult, std::time::Duration) {
    let filter = build();
    // One unmeasured pass to fill the memo.
    let warm = walk(&filter, nodes, mode);
    let start = Instant::now();
    let mut last = None;
    for _ in 0..rounds {
        last = Some(walk(&filter, nodes, mode));
    }
    let elapsed = start.elapsed();
    let last = last.expect("at least one round");
    assert_eq!(warm, last, "a warm memo changed the walk's answer");
    (last, elapsed)
}

#[test]
fn descend_large_tree() {
    // At scale 8 this is roughly a quarter of a million nodes.
    let nodes = tree(12 * scale(), 4 * scale());
    let directories = nodes.iter().filter(|node| node.is_dir).count();
    println!(
        "tree: {} nodes ({directories} directories, {} files), {} filter lines",
        nodes.len(),
        nodes.len() - directories,
        build().ignore.lines.len()
    );

    const ROUNDS: usize = 2;

    for mode in [FilterMode::Ignore, FilterMode::Full] {
        println!("\n{mode:?}, memo cold each walk:");
        let (whole, whole_time) = timed_cold(ROUNDS, &nodes, mode, walk_whole_paths);
        let (threaded, threaded_time) = timed_cold(ROUNDS, &nodes, mode, walk_threaded);
        assert_eq!(
            whole, threaded,
            "the two strategies disagree about the tree"
        );
        println!(
            "  visited {} nodes, kept {} files",
            whole.queries, whole.files
        );
        let a = report("whole path", ROUNDS, whole.queries, whole_time);
        let b = report("threaded", ROUNDS, threaded.queries, threaded_time);
        println!("  speedup {:.2}x", a / b);

        println!("\n{mode:?}, memo warm:");
        let (whole, whole_time) = timed_warm(ROUNDS, &nodes, mode, walk_whole_paths);
        let (threaded, threaded_time) = timed_warm(ROUNDS, &nodes, mode, walk_threaded);
        assert_eq!(
            whole, threaded,
            "the two strategies disagree about the tree"
        );
        let a = report("whole path", ROUNDS, whole.queries, whole_time);
        let b = report("threaded", ROUNDS, threaded.queries, threaded_time);
        println!("  speedup {:.2}x", a / b);
    }
}

/// Work units a parallel walk divides a tree into: every directory at `depth`,
/// with the path that reaches it.
fn units(nodes: &[TreeNode], depth: usize) -> Vec<(usize, String)> {
    fn collect(
        nodes: &[TreeNode],
        at: usize,
        path: &mut RelativePathBuf,
        left: usize,
        out: &mut Vec<(usize, String)>,
    ) {
        for &child in &nodes[at].children {
            let child = child as usize;
            if !nodes[child].is_dir {
                continue;
            }
            path.push(&nodes[child].name);
            if left == 0 {
                out.push((child, path.as_str().to_owned()));
            } else {
                collect(nodes, child, path, left - 1, out);
            }
            path.pop();
        }
    }

    let mut out = Vec::new();
    let mut path = RelativePathBuf::new();
    collect(nodes, 0, &mut path, depth.saturating_sub(1), &mut out);
    out
}

/// Aggregate throughput when `threads` walkers share one filter, which is the
/// shape every walk in the client runs in: subtree tasks spawned into a
/// `JoinSet`, all holding the same `Arc<Filter>`.
///
/// The whole-path strategy reaches the shared [`AncestorMemo`] on every node --
/// an atomic load to revalidate, then a `DashMap` shard lookup. The threaded
/// strategy touches no shared state at all, so this is where the two diverge
/// most.
fn concurrent(
    threads: usize,
    rounds: usize,
    nodes: &[TreeNode],
    units: &[(usize, String)],
    mode: FilterMode,
    threaded: bool,
) -> (WalkResult, std::time::Duration) {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let filter = build();
    let next = AtomicUsize::new(0);
    let files = AtomicUsize::new(0);
    let queries = AtomicUsize::new(0);

    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut local = WalkResult {
                    files: 0,
                    queries: 0,
                };
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= units.len() * rounds {
                        break;
                    }
                    let (node, path) = &units[index % units.len()];
                    let mut buffer = RelativePathBuf::new();
                    buffer.push(path);
                    if threaded {
                        // A walk rooted below the tree's root folds its seed
                        // once, then steps.
                        let seed = filter.exclusion_states(&buffer);
                        descend_threaded(
                            &filter,
                            nodes,
                            mode,
                            *node,
                            seed,
                            &mut buffer,
                            &mut local,
                        );
                    } else {
                        descend_whole_paths(&filter, nodes, mode, *node, &mut buffer, &mut local);
                    }
                }
                files.fetch_add(local.files, Ordering::Relaxed);
                queries.fetch_add(local.queries, Ordering::Relaxed);
            });
        }
    });
    let elapsed = start.elapsed();

    (
        WalkResult {
            files: files.load(Ordering::Relaxed) / rounds,
            queries: queries.load(Ordering::Relaxed) / rounds,
        },
        elapsed,
    )
}

#[test]
fn descend_large_tree_in_parallel() {
    let nodes = tree(12 * scale(), 4 * scale());
    let units = units(&nodes, 3);
    println!(
        "tree: {} nodes, split into {} parallel work units",
        nodes.len(),
        units.len()
    );

    const ROUNDS: usize = 1;

    for threads in [1usize, 4, 16, 32] {
        let (whole, whole_time) =
            concurrent(threads, ROUNDS, &nodes, &units, FilterMode::Full, false);
        let (threaded, threaded_time) =
            concurrent(threads, ROUNDS, &nodes, &units, FilterMode::Full, true);
        assert_eq!(
            whole, threaded,
            "the two strategies disagree about the tree"
        );
        let whole_rate = (whole.queries * ROUNDS) as f64 / whole_time.as_secs_f64() / 1e6;
        let threaded_rate = (threaded.queries * ROUNDS) as f64 / threaded_time.as_secs_f64() / 1e6;
        println!(
            "  {threads:>2} threads: whole path {whole_rate:>6.2} M nodes/s, \
             threaded {threaded_rate:>6.2} M nodes/s, speedup {:.2}x",
            threaded_rate / whole_rate
        );
    }
}

/// A tree of `chains` chains each `depth` directories deep, with `files` files
/// at the bottom. Every level matches no rule, so a whole-path query at a leaf
/// folds the whole chain.
fn chain_tree(chains: usize, depth: usize, files: usize) -> Vec<TreeNode> {
    let mut nodes = vec![TreeNode {
        name: String::new(),
        is_dir: true,
        children: Vec::new(),
    }];
    for chain in 0..chains {
        let mut at = 0usize;
        for level in 0..depth {
            let index = nodes.len();
            nodes.push(TreeNode {
                name: format!("Chain{chain:03}Level{level:02}"),
                is_dir: true,
                children: Vec::new(),
            });
            nodes[at].children.push(index as u32);
            at = index;
        }
        for file in 0..files {
            let index = nodes.len();
            nodes.push(TreeNode {
                name: format!("Leaf{file:03}.cpp"),
                is_dir: false,
                children: Vec::new(),
            });
            nodes[at].children.push(index as u32);
        }
    }
    nodes
}

/// How the advantage scales with depth, which is what the fold pays for.
///
/// Node count is held roughly constant so only depth varies: a whole-path query
/// walks one step per ancestor component on a memo miss, a threaded walk one
/// step per node whatever the depth.
#[test]
fn descend_by_depth() {
    const ROUNDS: usize = 2;
    let target = 3_000 * scale();
    println!("nodes held near constant; memo cold each walk (one command's worth)");
    for depth in [2usize, 4, 8, 12, 16, 24, 32, 40] {
        // Total nodes held near constant across the sweep, so only depth varies.
        let chains = (target / (depth + 32)).max(1);
        let nodes = chain_tree(chains, depth, 32);
        let (whole, whole_time) = timed_cold(ROUNDS, &nodes, FilterMode::Full, walk_whole_paths);
        let (threaded, threaded_time) = timed_cold(ROUNDS, &nodes, FilterMode::Full, walk_threaded);
        assert_eq!(
            whole, threaded,
            "the two strategies disagree at depth {depth}"
        );
        let whole_ns = whole_time.as_nanos() as f64 / (ROUNDS * whole.queries) as f64;
        let threaded_ns = threaded_time.as_nanos() as f64 / (ROUNDS * threaded.queries) as f64;
        println!(
            "  depth {depth:>2}: {:>6} nodes   whole path {whole_ns:>9.0} ns/node   \
             threaded {threaded_ns:>7.0} ns/node   speedup {:>5.2}x",
            nodes.len(),
            whole_ns / threaded_ns
        );
    }
}

/// How the advantage scales with fanout, which is what decides whether the
/// whole-path strategy's fold amortizes.
///
/// The memo holds one entry per directory, so a directory's fold is paid once
/// and shared by all its children. A directory with one child pays it per node;
/// a directory with sixty-four spreads it. This is why a broad tree narrows the
/// gap and a sparse one widens it.
#[test]
fn descend_by_fanout() {
    const ROUNDS: usize = 2;
    const DEPTH: usize = 12;
    let target = 3_000 * scale();
    println!("depth {DEPTH}, nodes held near constant; memo cold each walk");
    for files in [1usize, 2, 4, 8, 16, 32, 64] {
        let chains = (target / (DEPTH + files)).max(1);
        let nodes = chain_tree(chains, DEPTH, files);
        let (whole, whole_time) = timed_cold(ROUNDS, &nodes, FilterMode::Full, walk_whole_paths);
        let (threaded, threaded_time) = timed_cold(ROUNDS, &nodes, FilterMode::Full, walk_threaded);
        assert_eq!(
            whole, threaded,
            "the two strategies disagree at fanout {files}"
        );
        let whole_ns = whole_time.as_nanos() as f64 / (ROUNDS * whole.queries) as f64;
        let threaded_ns = threaded_time.as_nanos() as f64 / (ROUNDS * threaded.queries) as f64;
        println!(
            "  {files:>2} files/dir: {:>6} nodes   whole path {whole_ns:>8.0} ns/node   \
             threaded {threaded_ns:>7.0} ns/node   speedup {:>5.2}x",
            nodes.len(),
            whole_ns / threaded_ns
        );
    }
}

/// The same comparison over a deep, narrow tree, where a whole-path query's
/// ancestor fold is longest and a walk's single step is not.
#[test]
fn descend_deep_tree() {
    // One chain 40 deep, fanning out to files only at the bottom, repeated wide
    // enough to time. Each level is a directory matching nothing, so a
    // whole-path query at the leaf folds 40 components.
    let mut b_nodes = vec![TreeNode {
        name: String::new(),
        is_dir: true,
        children: Vec::new(),
    }];
    for chain in 0..8 * scale() {
        let mut at = 0usize;
        for depth in 0..40 {
            let index = b_nodes.len();
            b_nodes.push(TreeNode {
                name: format!("Chain{chain:03}Level{depth:02}"),
                is_dir: true,
                children: Vec::new(),
            });
            b_nodes[at].children.push(index as u32);
            at = index;
        }
        for file in 0..64 {
            let index = b_nodes.len();
            b_nodes.push(TreeNode {
                name: format!("Leaf{file:03}.cpp"),
                is_dir: false,
                children: Vec::new(),
            });
            b_nodes[at].children.push(index as u32);
        }
    }

    println!("deep tree: {} nodes, 40 levels", b_nodes.len());

    const ROUNDS: usize = 4;
    let mode = FilterMode::Ignore;

    println!("\n{mode:?}, memo cold each walk:");
    let (whole, whole_time) = timed_cold(ROUNDS, &b_nodes, mode, walk_whole_paths);
    let (threaded, threaded_time) = timed_cold(ROUNDS, &b_nodes, mode, walk_threaded);
    assert_eq!(
        whole, threaded,
        "the two strategies disagree about the tree"
    );
    println!(
        "  visited {} nodes, kept {} files",
        whole.queries, whole.files
    );
    let a = report("whole path", ROUNDS, whole.queries, whole_time);
    let b = report("threaded", ROUNDS, threaded.queries, threaded_time);
    println!("  speedup {:.2}x", a / b);

    println!("\n{mode:?}, memo warm:");
    let (whole, whole_time) = timed_warm(ROUNDS, &b_nodes, mode, walk_whole_paths);
    let (threaded, threaded_time) = timed_warm(ROUNDS, &b_nodes, mode, walk_threaded);
    assert_eq!(
        whole, threaded,
        "the two strategies disagree about the tree"
    );
    let a = report("whole path", ROUNDS, whole.queries, whole_time);
    let b = report("threaded", ROUNDS, threaded.queries, threaded_time);
    println!("  speedup {:.2}x", a / b);
}
