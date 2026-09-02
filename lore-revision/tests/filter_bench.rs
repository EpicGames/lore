// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Timing for the filter's query entry points.
//!
//! Written against only `add_exclusion`, `add_inclusion` and `excludes`, which
//! have the same signatures before and after the descent split, so the same file
//! compiles and runs on either branch and the numbers are comparable.
//!
//! Run with:
//!     `cargo test -p lore-revision --release --test filter_bench -- --nocapture`

use std::time::Instant;

use lore_revision::filter::Filter;
use lore_revision::filter::FilterMode;
use lore_revision::util::path::RelativePath;

#[path = "support/filter_workload.rs"]
mod workload;

use workload::probe_paths;
use workload::rules;
use workload::target_list;

/// The filter as a repository holds one: rules in the ignore slot, view empty.
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

fn paths(list: Vec<(String, bool)>) -> Vec<(RelativePath, bool)> {
    list.into_iter()
        .map(|(path, is_dir)| {
            (
                RelativePath::new_from_initial_path(&path).expect("valid path"),
                is_dir,
            )
        })
        .collect()
}

/// Reports nanoseconds per call.
fn time(label: &str, calls: usize, run: impl Fn() -> usize) {
    let start = Instant::now();
    let sink = run();
    let elapsed = start.elapsed();
    println!(
        "{label:<18} {:>7.0} ns/call  ({:?} total, sink {sink})",
        elapsed.as_nanos() as f64 / calls as f64,
        elapsed
    );
}

/// `Filter::excludes`, the call every production site reaches through
/// `emit_excludes`, over paths spanning every rule kind.
#[test]
fn excludes_throughput() {
    let filter = build();
    let probes = paths(probe_paths());
    let excluded = probes
        .iter()
        .filter(|(path, is_dir)| filter.excludes(path, *is_dir, FilterMode::Full))
        .count();
    println!("probe paths        {} ({excluded} excluded)", probes.len());
    println!("compiled lines     {}", filter.ignore.lines.len());

    const ROUNDS: usize = 20_000;
    time("excludes", ROUNDS * probes.len(), || {
        let mut sink = 0;
        for _ in 0..ROUNDS {
            for (path, is_dir) in &probes {
                sink += usize::from(filter.excludes(path, *is_dir, FilterMode::Full));
            }
        }
        sink
    });
}

/// `Filter::excludes` over a targets file, the shape `lore stage --targets` is
/// handed: many paths, far fewer directories.
#[test]
fn targets_file_throughput() {
    let filter = build();
    let targets = paths(
        target_list(2_000, 50)
            .into_iter()
            .map(|path| (path, false))
            .collect(),
    );
    let excluded = targets
        .iter()
        .filter(|(path, _)| filter.excludes(path, false, FilterMode::Full))
        .count();
    println!("targets            {} ({excluded} excluded)", targets.len());

    time("excludes/target", targets.len(), || {
        targets
            .iter()
            .filter(|(path, _)| filter.excludes(path, false, FilterMode::Full))
            .count()
    });
}

/// `Filter::excludes_subtree`, which `state::diff` asks of every directory it
/// meets to decide whether the subtree can be skipped whole.
#[test]
fn excludes_subtree_throughput() {
    let filter = build();
    let dirs: Vec<RelativePath> = probe_paths()
        .into_iter()
        .filter_map(|(path, _)| path.rsplit_once('/').map(|(parent, _)| parent.to_owned()))
        .map(|path| RelativePath::new_from_initial_path(&path).expect("valid path"))
        .collect();
    println!("directories        {}", dirs.len());

    const ROUNDS: usize = 20_000;
    time("excludes_subtree", ROUNDS * dirs.len(), || {
        let mut sink = 0;
        for _ in 0..ROUNDS {
            for dir in &dirs {
                sink += usize::from(filter.excludes_subtree(dir, FilterMode::Full));
            }
        }
        sink
    });
}
