// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Timing for the filter's query entry points, and what one costs to build and
//! to hold.
//!
//! Written against `add_exclusion`, `add_inclusion` and the `excludes` family
//! alone, whose signatures do not move, so the same file compiles on a revision
//! either side of a change to the filter's internals and the numbers are
//! comparable across it.
//!
//! Run with:
//!     `cargo test -p lore-revision --release --test filter_bench -- --ignored --nocapture --test-threads=1`
//!
//! Serialized, because these contend with each other. Compare two revisions by
//! interleaving their binaries rep by rep rather than running each in a block,
//! and read both the minimum and the median of a dozen or more reps: a block
//! absorbs machine drift, and where the two statistics disagree in sign there is
//! no difference to report.
//!
//! [`FilterInstance`]'s footprint moves these numbers by several percent on its
//! own, in either direction. A change to its size therefore needs a control that
//! changes only the size -- a dead field of the same width -- before a delta is
//! read as the cost of work.

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
    build_from(&rules())
}

/// A filter holding `rules` in one slot.
///
/// Which slot is irrelevant to what a filter costs to build or to hold; the
/// ignore slot is the one a repository fills from a `.loreignore`.
fn build_from(rules: &[String]) -> Filter {
    let mut filter = Filter::default();
    for rule in rules {
        match rule.strip_prefix('!') {
            Some(rest) => filter.ignore.add_inclusion(rest).expect("inclusion"),
            None => filter.ignore.add_exclusion(rule).expect("exclusion"),
        }
    }
    filter
}

/// The filter `filter_from_source_changes` synthesizes from a diff: one blanket
/// exclusion, then one re-inclusion per changed path, up to
/// `SOURCE_FILTER_THRESHOLD` = 10,000 of them.
///
/// The shape that decides whether filter construction can be afforded per diff,
/// and the one where a per-line cost is multiplied by the most. Each path rule
/// also emits the subtree companion `add_inclusion` generates, so the line count
/// comes out at roughly twice the rule count.
fn synthesized_rules(paths: usize) -> Vec<String> {
    let mut out = vec!["**".to_owned()];
    out.extend(
        target_list(paths / 50, 50)
            .into_iter()
            .map(|path| format!("!{path}")),
    );
    out
}

/// Bytes the process holds resident, for a footprint measured as a delta.
///
/// Page-granular and process-wide, so it resolves a batch rather than one
/// filter, which is far smaller than a page. Nothing exposes this portably, so
/// elsewhere the footprint reads as absent and the build timing still runs --
/// the same bargain `tests/path_allocations.rs` strikes with its interposer.
#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page_size).ok().map(|size| pages * size)
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes() -> Option<usize> {
    None
}

/// Builds `rules` `batch` times, reporting the time one build costs and how far
/// the process grows while holding all of them at once.
///
/// The first build is discarded, so the allocator's arenas are already grown
/// when the batch is measured and the delta is the filters rather than the
/// allocator reaching for pages.
fn report_build(label: &str, batch: usize, rules: &[String]) {
    let warm = build_from(rules);
    let lines = warm.ignore.lines.len();
    drop(warm);

    let before = resident_bytes();
    let start = Instant::now();
    let mut held = Vec::with_capacity(batch);
    for _ in 0..batch {
        held.push(build_from(rules));
    }
    let elapsed = start.elapsed();
    let after = resident_bytes();

    println!(
        "{label:<12} {:>6} rules -> {lines:>6} lines   {:>9.1} us/build",
        rules.len(),
        elapsed.as_nanos() as f64 / batch as f64 / 1_000.0
    );
    match (before, after) {
        (Some(before), Some(after)) if after > before => println!(
            "{:<12} resident +{} KiB over {batch} filters = {} bytes/filter",
            "",
            (after - before) / 1024,
            (after - before) / batch
        ),
        (Some(_), Some(_)) => println!(
            "{:<12} resident grew by less than a page over {batch} filters",
            ""
        ),
        _ => println!("{:<12} resident size unavailable on this platform", ""),
    }

    drop(held);
}

/// What a filter costs to build and to hold, for the two shapes a repository
/// meets: the file a user authors, and the one a diff synthesizes.
///
/// Reported rather than compared. There is no threshold worth asserting here --
/// a loose one catches nothing and a tight one breaks on an unrelated change --
/// and what earns this its place is a number to hold the next change to the
/// filter against.
#[test]
#[ignore = "benchmark: run on demand with --ignored"]
fn build_cost_and_resident_size() {
    report_build("authored", 400, &rules());
    report_build("synthesized", 8, &synthesized_rules(10_000));
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
#[ignore = "benchmark: run on demand with --ignored"]
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
#[ignore = "benchmark: run on demand with --ignored"]
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
#[ignore = "benchmark: run on demand with --ignored"]
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
