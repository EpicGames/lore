// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Times `lore_revision::fs::walk::clone_into` copying four trees: a deep one, a wide one, a flat
//! one of many small files, and a few large files. `BENCHMARKS.md`'s *The walk's bounds* reports
//! it.
//!
//! `walk prepare` writes the trees under `LORE_BENCH_DIR`, or the system temp directory, and
//! `walk run <variant> [tree]` copies each tree, or the one named, once and prints a line for each:
//! `<variant> <tree> <files> <MiB> <secs> <files/s> <MiB/s>`, the shape `build-ab.sh` reads. Each
//! copy goes to a new directory beside the trees, or in `LORE_BENCH_COPY_DIR`, as on another
//! volume, removed once it is timed.
//!
//! | Variant | Copies with |
//! | --- | --- |
//! | `clone` | `clone_into` with the copy backend, on the threads the syscall pool lends |
//! | `faithful` | `copy_faithfully`, on the threads the syscall pool lends, at most half its cap (Linux, Windows) |
//! | `faithful-alone` | `copy_faithfully` on the calling thread alone (Linux, Windows) |
//! | `faithful-all` | `copy_faithfully` on as many threads as the syscall pool's whole cap (Linux, Windows) |
//! | `reference` | A walk on one thread with `std::fs::copy`, the same code in every build, as the run's noise floor |
//!
//! Compare two builds with `lore-io/examples/build-ab.sh`, which alternates them per round:
//!
//! ```sh
//! cargo run --release -p lore-revision --example walk --features walk-bench -- prepare
//! MODE=run ENGINES="clone reference" lore-io/examples/build-ab.sh 6 <base-walk> target/release/examples/walk
//! ```

// The benchmark writes and removes its trees outside the repo write-token discipline.
#![allow(clippy::disallowed_methods)]

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use lore_io::clone::Backend;
use lore_io::clone::Kind;
use lore_revision::fs::walk::Progress;
use lore_revision::fs::walk::Visit;

/// A tree the benchmark copies: its name, and how it is written.
struct Tree {
    name: &'static str,
    /// Directories below the root, as a width and a depth: each directory holds `width`
    /// directories, down to `depth` levels.
    width: usize,
    depth: usize,
    /// Files in each directory, and the size of each.
    files: usize,
    file_size: usize,
}

/// The four trees: 8,191 directories 12 levels deep, the root and 4 directories of 25,000 files
/// each, the root and 1,000 directories of 64 small files each, and 8 files of 256 MiB.
const TREES: [Tree; 4] = [
    Tree {
        name: "deep",
        width: 2,
        depth: 12,
        files: 1,
        file_size: 4096,
    },
    Tree {
        name: "wide",
        width: 4,
        depth: 1,
        files: 25_000,
        file_size: 1024,
    },
    Tree {
        name: "flat",
        width: 1000,
        depth: 1,
        files: 64,
        file_size: 2048,
    },
    Tree {
        name: "large",
        width: 0,
        depth: 0,
        files: 8,
        file_size: 256 << 20,
    },
];

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = std::env::var_os("LORE_BENCH_DIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("lore-walk-bench");
    match (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    ) {
        (Some("prepare"), None) => prepare(&root),
        (Some("run"), Some(variant)) => {
            let only = args.get(2);
            for tree in TREES
                .iter()
                .filter(|tree| only.is_none_or(|only| only == tree.name))
            {
                run(&root, tree, variant)?;
            }
            Ok(())
        }
        _ => Err(io::Error::other(
            "usage: walk prepare\n       walk run <variant> [tree]",
        )),
    }
}

/// Writes every tree under `root`, replacing what is there.
fn prepare(root: &Path) -> io::Result<()> {
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    for tree in &TREES {
        let at = root.join(tree.name);
        std::fs::create_dir_all(&at)?;
        write_level(&at, tree, tree.depth, &vec![0x5a; tree.file_size])?;
        println!("{}: written", tree.name);
    }
    Ok(())
}

/// Writes the files of one directory of `tree` at `at`, and the `depth` levels below it.
fn write_level(at: &Path, tree: &Tree, depth: usize, contents: &[u8]) -> io::Result<()> {
    for file in 0..tree.files {
        std::fs::write(at.join(format!("f{file}")), contents)?;
    }
    if depth == 0 {
        return Ok(());
    }
    for directory in 0..tree.width {
        let below = at.join(format!("d{directory}"));
        std::fs::create_dir(&below)?;
        write_level(&below, tree, depth - 1, contents)?;
    }
    Ok(())
}

/// Copies `tree` once with `variant`, and prints how long it took.
fn run(root: &Path, tree: &Tree, variant: &str) -> io::Result<()> {
    let source = root.join(tree.name);
    let destination = std::env::var_os("LORE_BENCH_COPY_DIR")
        .map_or_else(|| root.to_path_buf(), PathBuf::from)
        .join(format!("{}-copy", tree.name));
    if destination.exists() {
        std::fs::remove_dir_all(&destination)?;
    }
    std::fs::create_dir(&destination)?;
    let started = Instant::now();
    let (files, bytes) = match variant {
        "clone" => {
            let progress = progress()?;
            let every_entry = |_: &Path, _: &Path, kind: Kind| match kind {
                Kind::Directory => Visit::EachEntry,
                _ => Visit::Clone,
            };
            let stats = lore_revision::fs::walk::clone_into(
                Backend::Copy,
                &source,
                &destination,
                &every_entry,
                false,
                &progress,
            )?;
            (stats.copied_file_count, total_bytes(tree))
        }
        "reference" => copy_tree(&source, &destination)?,
        #[cfg(any(target_os = "linux", target_family = "windows"))]
        faithful @ ("faithful" | "faithful-alone" | "faithful-all") => {
            let progress = progress()?;
            let every_entry = |_: &Path, _: &Path, kind: Kind| match kind {
                Kind::Directory => Visit::EachEntry,
                _ => Visit::Clone,
            };
            let doubled =
                lore_io::pool::SyscallPool::new(2 * lore_io::pool::configured_max_threads());
            let copied = if faithful == "faithful" {
                lore_revision::fs::walk::copy_faithfully(
                    &source,
                    &destination,
                    &every_entry,
                    &progress,
                )?
            } else {
                lore_revision::fs::walk::copy_faithfully_on(
                    (faithful == "faithful-all").then_some(&doubled),
                    &source,
                    &destination,
                    &every_entry,
                    &progress,
                )?
            };
            (copied.stats.copied_file_count, total_bytes(tree))
        }
        _ => return Err(io::Error::other(format!("no variant {variant}"))),
    };
    let secs = started.elapsed().as_secs_f64();
    std::fs::remove_dir_all(&destination)?;
    #[allow(clippy::cast_precision_loss)]
    let (files, mib) = (files as f64, bytes as f64 / f64::from(1 << 20));
    println!(
        "{variant} {} {files:.0} {mib:.1} {secs:.3} {:.0} {:.1}",
        tree.name,
        files / secs,
        mib / secs
    );
    Ok(())
}

/// A progress with the walk's bounds, or those `LORE_BENCH_WALK_BOUNDS` names as
/// `<queue>,<continuations>,<batch>`.
fn progress() -> io::Result<Progress> {
    let Some(bounds) = std::env::var("LORE_BENCH_WALK_BOUNDS").ok() else {
        return Ok(Progress::default());
    };
    let mut values = bounds.split(',').map(str::parse::<usize>);
    match (values.next(), values.next(), values.next(), values.next()) {
        (Some(Ok(queue)), Some(Ok(continuations)), Some(Ok(batch)), None) => {
            Ok(Progress::bounded(queue, continuations, batch))
        }
        _ => Err(io::Error::other(format!(
            "LORE_BENCH_WALK_BOUNDS is <queue>,<continuations>,<batch>, not {bounds}"
        ))),
    }
}

/// The bytes the files of `tree` hold.
fn total_bytes(tree: &Tree) -> u64 {
    let directories: usize = (0..=tree.depth)
        .map(|level| tree.width.pow(level as u32))
        .sum::<usize>();
    (directories * tree.files * tree.file_size) as u64
}

/// Copies the tree at `source` into `destination` on this thread with `std::fs::copy`: the files
/// and bytes copied.
fn copy_tree(source: &Path, destination: &Path) -> io::Result<(u64, u64)> {
    let mut copied = (0, 0);
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((from, to)) = pending.pop() {
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                std::fs::create_dir(&target)?;
                pending.push((entry.path(), target));
            } else {
                copied.1 += std::fs::copy(entry.path(), &target)?;
                copied.0 += 1;
            }
        }
    }
    Ok(copied)
}
