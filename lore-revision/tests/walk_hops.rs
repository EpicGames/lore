// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The hand-offs of each walk, from each of its callers: the jobs the syscall pools run and the
//! blocking tasks spawned while it runs, and each walk's allocations where the process counts them.
//! A process of its own, as each count is the process's.

// Test fixtures write to the filesystem outside the repo write-token discipline.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_io::clone::Kind;
use lore_revision::fs::walk::Progress;
use lore_revision::fs::walk::Visit;

/// The jobs run and the blocking tasks spawned while something ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hops {
    jobs: usize,
    blocking_spawns: usize,
}

/// No hand-off at all.
const NONE: Hops = Hops {
    jobs: 0,
    blocking_spawns: 0,
};

/// One move to a blocking thread, and no job.
const ONE_SPAWN: Hops = Hops {
    jobs: 0,
    blocking_spawns: 1,
};

fn now() -> Hops {
    Hops {
        jobs: lore_io::pool::jobs_run(),
        blocking_spawns: lore_base::test_util::blocking_spawns(),
    }
}

/// What `run` returns, and the hand-offs made while it ran.
fn during<T>(run: impl FnOnce() -> T) -> (T, Hops) {
    let before = now();
    let done = run();
    let after = now();
    (
        done,
        Hops {
            jobs: after.jobs - before.jobs,
            blocking_spawns: after.blocking_spawns - before.blocking_spawns,
        },
    )
}

/// [`during`] for a future, run to its end on `runtime`.
fn during_async<T>(runtime: &tokio::runtime::Runtime, run: impl Future<Output = T>) -> (T, Hops) {
    during(|| runtime.block_on(run))
}

/// A tree of 3 levels of 3 directories, each with 4 files and a link, under `root`.
fn source_tree(root: &Path) -> PathBuf {
    let source = root.join("source");
    write_level(&source, 3);
    source
}

fn write_level(at: &Path, depth: usize) {
    std::fs::create_dir_all(at).expect("a directory");
    for file in 0..4 {
        std::fs::write(at.join(format!("f{file}")), b"walk hops").expect("a file");
    }
    #[cfg(target_family = "unix")]
    std::os::unix::fs::symlink("f0", at.join("link")).expect("a link");
    if depth > 0 {
        for directory in 0..3 {
            write_level(&at.join(format!("d{directory}")), depth - 1);
        }
    }
}

fn each_entry(_path: &Path, _relative: &Path, kind: Kind) -> Visit {
    match kind {
        Kind::Directory => Visit::EachEntry,
        _ => Visit::Clone,
    }
}

/// Prints and checks the hand-offs of `walk` from `caller`.
fn check(walk: &str, caller: &str, hops: Hops, expected: Hops) {
    println!(
        "{walk:<18} from {caller:<58} jobs {} blocking spawns {}",
        hops.jobs, hops.blocking_spawns
    );
    assert_eq!(hops, expected, "{walk} from {caller}");
}

/// Each of the four walks makes no hand-off of its own, run from a plain thread or from a caller
/// that blocks for it, and a caller that awaits it moves to a blocking thread once.
#[test]
fn each_walk_makes_only_its_callers_hand_off() {
    let root = TempDir::new("walk-hops-");
    let source = source_tree(root.path());
    let copy = |name: &str| {
        let destination = root.path().join(name);
        std::fs::create_dir(&destination).expect("the destination");
        destination
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let pool = lore_io::pool::SyscallPool::new(1);
    let ((), hops) = during_async(&runtime, async { pool.submit(|| ()).await });
    check(
        "(a job)",
        "a submission to a pool",
        hops,
        Hops {
            jobs: 1,
            blocking_spawns: 0,
        },
    );

    let (measured, hops) = during(|| {
        lore_revision::fs::walk::measure(&source, &each_entry, 4096, &Progress::default())
    });
    measured.expect("measure");
    check("measure", "a plain thread", hops, NONE);
    let (measured, hops) = during(|| {
        lore_revision::global::external_dir::check::measure_root(
            &source,
            4096,
            &Progress::default(),
        )
    });
    measured.expect("measure_root");
    check("measure", "check::measure_root", hops, NONE);

    let (found, hops) = during(|| {
        lore_revision::fs::walk::find_directories(&source, &|_| false, &Progress::default())
    });
    found.expect("find_directories");
    check("find_directories", "a plain thread", hops, NONE);
    let (found, hops) = during(|| {
        lore_revision::fs::os::clone::directories_above_nested_repositories(
            lore_io::clone::Backend::Apfs,
            &source,
            Path::new(".lore"),
            &Progress::default(),
        )
    });
    found.expect("directories_above_nested_repositories");
    check(
        "find_directories",
        "os::clone::directories_above_nested_repositories",
        hops,
        NONE,
    );

    let destination = copy("clone-plain");
    let (cloned, hops) = during(|| {
        lore_revision::fs::walk::clone_into(
            lore_io::clone::Backend::Copy,
            &source,
            &destination,
            &each_entry,
            false,
            &Progress::default(),
        )
    });
    cloned.expect("clone_into");
    check("clone_into", "a plain thread", hops, NONE);
    let destination = copy("clone-tree");
    let (cloned, hops) = during(|| {
        lore_revision::fs::os::clone::clone_tree(
            lore_io::clone::Backend::Copy,
            &source,
            Path::new(".lore"),
            &destination,
            &std::collections::HashSet::new(),
            false,
            &Progress::default(),
        )
    });
    cloned.expect("clone_tree");
    check("clone_into", "os::clone::clone_tree", hops, NONE);

    #[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
    {
        let destination = copy("faithful-plain");
        let (copied, hops) = during(|| {
            lore_revision::fs::walk::copy_faithfully(
                &source,
                &destination,
                &each_entry,
                &Progress::default(),
            )
        });
        copied.expect("copy_faithfully");
        check("copy_faithfully", "a plain thread", hops, NONE);
        let destination = copy("faithful-recovery");
        let run = lore_revision::global::external_dir::recovery::CopyRun {
            progress: std::sync::Arc::new(Progress::default()),
            on_copying: std::sync::Arc::new(|_| {}),
        };
        let (copied, hops) = during_async(
            &runtime,
            lore_revision::global::external_dir::recovery::copy_faithfully(
                &source,
                &destination,
                &run,
            ),
        );
        copied.expect("recovery::copy_faithfully");
        check(
            "copy_faithfully",
            "recovery::copy_faithfully",
            hops,
            ONE_SPAWN,
        );
    }

    #[cfg(target_os = "linux")]
    {
        let shm = Path::new("/dev/shm");
        let unshared = if shm.is_dir() {
            TempDir::new_in(shm, "walk-hops-")
        } else {
            TempDir::new("walk-hops-fuse-")
        };
        fuse_callers(&runtime, unshared.path());
    }

    report_allocations(&source, root.path());
}

/// Prints the allocations of five runs of each walk over `source`, copying into new directories
/// under `root`, where the process counts them, as `lore_base::test_util::allocations` describes.
fn report_allocations(source: &Path, root: &Path) {
    if lore_base::test_util::allocations().is_none() {
        return;
    }
    let mut copies = 0;
    let mut destination = || {
        copies += 1;
        let destination = root.join(format!("allocations-{copies}"));
        std::fs::create_dir(&destination).expect("the destination");
        destination
    };
    let report = |walk: &str, run: &mut dyn FnMut()| {
        let counts: Vec<u64> = (0..5)
            .map(|_| {
                let before = lore_base::test_util::allocations().unwrap_or(0);
                run();
                lore_base::test_util::allocations().unwrap_or(0) - before
            })
            .collect();
        println!("{walk:<18} allocations {counts:?}");
    };
    report("measure", &mut || {
        lore_revision::fs::walk::measure(source, &each_entry, 4096, &Progress::default())
            .expect("measure");
    });
    report("find_directories", &mut || {
        lore_revision::fs::walk::find_directories(source, &|_| false, &Progress::default())
            .expect("find_directories");
    });
    report("clone_into", &mut || {
        lore_revision::fs::walk::clone_into(
            lore_io::clone::Backend::Copy,
            source,
            &destination(),
            &each_entry,
            false,
            &Progress::default(),
        )
        .expect("clone_into");
    });
    #[cfg(any(target_os = "linux", target_os = "macos", target_family = "windows"))]
    report("copy_faithfully", &mut || {
        lore_revision::fs::walk::copy_faithfully(
            source,
            &destination(),
            &each_entry,
            &Progress::default(),
        )
        .expect("copy_faithfully");
    });
}

/// The FUSE instance's callers, on a volume under `root`: `fuse::clone::answer`, which measures
/// what a copy takes where the volume shares no blocks, as `/dev/shm` doesn't, and
/// `fuse::clone::clone`, which clones the external directory.
#[cfg(target_os = "linux")]
fn fuse_callers(runtime: &tokio::runtime::Runtime, root: &Path) {
    use lore_revision::fs::filesystem_provider::CloneAnswer;
    use lore_revision::fs::filesystem_provider::CloneRun;

    let external = root.join("external");
    std::fs::create_dir_all(external.join(".lore")).expect("the Lore directory");
    std::fs::write(external.join(".lore/config"), b"config").expect("the config");
    write_level(&external.join("write"), 2);
    let (answer, hops) = during_async(runtime, lore_revision::fs::fuse::clone::answer(&external));
    let answer = answer.expect("answer");
    let measured = matches!(answer, CloneAnswer::CopiesBytes { .. });
    check(
        if measured { "measure" } else { "(no walk)" },
        "fuse::clone::answer",
        hops,
        ONE_SPAWN,
    );
    let destination = root.join("fuse-clone");
    std::fs::create_dir_all(destination.join(".lore")).expect("the new Lore directory");
    let run = CloneRun {
        progress: std::sync::Arc::new(Progress::default()),
        hold: std::sync::Arc::new(()),
    };
    let (cloned, hops) = during_async(
        runtime,
        lore_revision::fs::fuse::clone::clone(&external, &destination, &answer, true, &run),
    );
    cloned.expect("fuse clone");
    check("clone_into", "fuse::clone::clone", hops, ONE_SPAWN);
}
