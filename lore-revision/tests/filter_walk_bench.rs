// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Timing for the walk entry points, which main has no counterpart for.
//!
//! Run with:
//!     `cargo test -p lore-revision --release --test filter_walk_bench -- --nocapture`

use lore_revision::filter::FilterInstance;
use lore_revision::util::path::RelativePath;

#[path = "support/filter_workload.rs"]
mod workload;

use workload::probe_paths;
use workload::rules;

fn build() -> FilterInstance {
    let mut filter = FilterInstance::default();
    for rule in rules() {
        match rule.strip_prefix('!') {
            Some(rest) => filter.add_inclusion(rest).expect("inclusion"),
            None => filter.add_exclusion(&rule).expect("exclusion"),
        }
    }
    filter
}

/// Per-node cost of `child_exclusion_state`, the call a tree walk makes.
///
/// `excludes` folds a path's ancestors because it is handed a whole path with no
/// walk behind it. A walk already holds the parent's verdict, so it pays for one
/// step per node, not one per component. Main has no equivalent -- its
/// `excludes` is a single pass over the line list and never looks at ancestors --
/// so main's `excludes` number is the like-for-like comparison.
#[test]
fn child_exclusion_state_throughput() {
    use lore_revision::filter::FilterState;

    let filter = build();
    let steps: Vec<(FilterState, RelativePath, bool)> = probe_paths()
        .into_iter()
        .map(|(path, is_dir)| {
            let parent = match path.rsplit_once('/') {
                Some((head, _)) => RelativePath::new_from_initial_path(head).expect("parent"),
                None => RelativePath::new(),
            };
            let parent_state = filter.exclusion_state(&parent, true);
            (
                parent_state,
                RelativePath::new_from_initial_path(&path).expect("path"),
                is_dir,
            )
        })
        .collect();

    println!("compiled lines: {}", filter.lines.len());

    const ROUNDS: usize = 20_000;
    let mut sink = 0usize;
    let start = std::time::Instant::now();
    for _ in 0..ROUNDS {
        for (parent, path, is_dir) in &steps {
            if filter
                .child_exclusion_state(*parent, path, *is_dir)
                .excluded()
            {
                sink += 1;
            }
        }
    }
    let elapsed = start.elapsed();
    let queries = ROUNDS * steps.len();
    println!(
        "child_exclusion_state: {queries} steps in {:?} -- {:.0} ns/step, {:.2} M steps/s (sink {sink})",
        elapsed,
        elapsed.as_nanos() as f64 / queries as f64,
        queries as f64 / elapsed.as_secs_f64() / 1e6,
    );

    // And the descent question, which main cannot answer at all.
    let dirs: Vec<(FilterState, RelativePath)> = steps
        .iter()
        .map(|(parent, path, _)| {
            (
                filter.child_exclusion_state(*parent, path, true),
                path.clone(),
            )
        })
        .collect();
    let mut sink = 0usize;
    let start = std::time::Instant::now();
    for _ in 0..ROUNDS {
        for (state, path) in &dirs {
            if filter.should_descend(*state, path) {
                sink += 1;
            }
        }
    }
    let elapsed = start.elapsed();
    let queries = ROUNDS * dirs.len();
    println!(
        "should_descend: {queries} in {:?} -- {:.0} ns/query, {:.2} M/s (sink {sink})",
        elapsed,
        elapsed.as_nanos() as f64 / queries as f64,
        queries as f64 / elapsed.as_secs_f64() / 1e6,
    );
}
