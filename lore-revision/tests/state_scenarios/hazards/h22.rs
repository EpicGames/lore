// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! H22 and H14: a swap's copy of a directory publishing the directory, clean, then adding the
//! nodes below it one by one. A swap dropped during that fill leaves a published, clean, partial
//! directory (H22), and a served create of a name the fill is about to add duplicates it (H14). A
//! swap builds the copy out of every chain and publishes it whole (`SwapNodes::node_attach_tree`).
//! The first two scenarios check the hazards; the others race the copy against an RMDIR of the
//! directory it copies into, a served create of its name or of another, and a served rename onto
//! its name, each below a listed directory or not, the copy dropped or not, and check that the name
//! is made once and that a copy losing it frees what it built.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Scenario;

/// The files the copy of `p/d` holds.
const COPIED: [&str; 2] = ["f", "g"];

/// Runs whose copy of `p/d` stood live at the check.
static LANDED: AtomicUsize = AtomicUsize::new(0);

/// A swap's copy of `p/d` holding two files, dropped at each point it parks at: wherever `p/d`
/// stands, it holds both files.
#[test]
fn a_swap_copy_dropped_at_every_point_publishes_its_directory_whole() {
    let name = "h22: a swap's copy dropped at every point";
    let scenario = Scenario::<Tree> {
        name,
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["p/"]).await;
                tree.state.mark_served();
                tree
            })
        }),
        ops: vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::swap_copy_tree(&tree, tree.id("p"), "d", &COPIED).await;
            })
        })],
        cancellable: vec![0],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let report = crate::holding(
                    &tree.state,
                    &tree.repository,
                    Rules {
                        dirty_closure: true,
                        unique_names: true,
                        no_ext_flags: true,
                        no_builds: false,
                    },
                )
                .await;
                if report.at("p/d").is_none() {
                    return;
                }
                LANDED.fetch_add(1, Ordering::Relaxed);
                for file in COPIED {
                    assert!(
                        report.at(&format!("p/d/{file}")).is_some(),
                        "p/d stands without {file}"
                    );
                }
            })
        }),
    };
    let covered = crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    assert!(
        LANDED.load(Ordering::Relaxed) > 0,
        "no run's copy landed in {} runs",
        covered.runs
    );
}

/// A swap's copy of `p/d` holding `f`, and a served create of `p/d/f`, which finds `p/d` by name
/// as a request does: `f` is created once, by one of them.
#[test]
fn a_swap_copy_and_a_create_of_a_name_it_holds_make_it_once() {
    let name = "h14: a swap's copy against a create of a name it holds";
    let scenario = Scenario::<Tree> {
        name,
        setup: Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["p/"]).await;
                tree.state.mark_served();
                tree
            })
        }),
        ops: vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::swap_copy_tree(&tree, tree.id("p"), "d", &["f"]).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    if let Some(d) = ops::find_dir(&tree, "p/d").await {
                        ops::served_create(&tree, d, "f").await;
                    }
                })
            }),
        ],
        cancellable: vec![],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let report = crate::holding(
                    &tree.state,
                    &tree.repository,
                    Rules {
                        dirty_closure: true,
                        unique_names: true,
                        no_ext_flags: false,
                        no_builds: false,
                    },
                )
                .await;
                assert!(report.at("p/d/f").is_some(), "p/d/f stands");
            })
        }),
    };
    crate::explored(scenario, Modelling::default(), Exploration::default());
}

/// What the scenarios of a copy racing other requests record.
struct Racing {
    tree: Tree,
    /// Whether the copy ran to its end, and whether it landed.
    copy: std::sync::Mutex<Option<bool>>,
    /// Whether the RMDIR removed its directory.
    removed: std::sync::atomic::AtomicBool,
}

impl Racing {
    async fn new(paths: &[&str]) -> Self {
        let tree = Tree::new(paths).await;
        tree.state.mark_served();
        Self {
            tree,
            copy: std::sync::Mutex::new(None),
            removed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn copied(&self) -> Option<bool> {
        *self.copy.lock().expect("the record")
    }
}

impl Modelled for Racing {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }
}

/// An op copying `name` holding `files` into the directory at `dir`, found by name when it runs,
/// recording how it ended.
fn copying(
    dir: &'static str,
    name: &'static str,
    files: &'static [&'static str],
) -> crate::harness::scheduler::Op<Racing> {
    Box::new(move |c: Arc<Racing>| {
        Box::pin(async move {
            let landed = match ops::find_dir(&c.tree, dir).await {
                Some(dir) => ops::swap_copy_tree(&c.tree, dir, name, files)
                    .await
                    .is_some(),
                None => false,
            };
            *c.copy.lock().expect("the record") = Some(landed);
        })
    })
}

/// The rules a served tree racing a copy keeps.
fn racing_invariants() -> Rules {
    Rules {
        dirty_closure: true,
        unique_names: true,
        no_ext_flags: false,
        no_builds: false,
    }
}

/// RMDIRs a copy into the directory made refuse, over every run.
static RMDIR_REFUSALS: AtomicUsize = AtomicUsize::new(0);

/// An RMDIR of `p/d`, empty, and a swap's copy of `c` holding two files into it, dropped at each
/// point it parks at: the RMDIR removes `d` only while no copy has landed in it, a copy that lands
/// stands whole, and a copy that finds `d` gone at its publish frees what it built.
#[test]
fn an_rmdir_and_a_swap_copy_of_a_directory_into_it() {
    let name = "h22: an RMDIR and a swap's copy of a directory into its directory";
    let scenario = Scenario::<Racing> {
        name,
        setup: Box::new(|| Box::pin(Racing::new(&["keep", "p/d/"]))),
        ops: vec![
            Box::new(|c: Arc<Racing>| {
                Box::pin(async move {
                    let ended = ops::served_rmdir(&c.tree, c.tree.id("p/d")).await;
                    c.removed.store(ended.removed, Ordering::Relaxed);
                    RMDIR_REFUSALS.fetch_add(ended.refused, Ordering::Relaxed);
                })
            }),
            copying("p/d", "c", &COPIED),
        ],
        cancellable: vec![1],
        check: Box::new(|c: Arc<Racing>| {
            Box::pin(async move {
                let report =
                    crate::holding(&c.tree.state, &c.tree.repository, racing_invariants()).await;
                let removed = c.removed.load(Ordering::Relaxed);
                if report.at("p/d/c").is_some() {
                    assert!(!removed, "the RMDIR removed d below a landed copy");
                    for file in COPIED {
                        assert!(
                            report.at(&format!("p/d/c/{file}")).is_some(),
                            "the copy stands without {file}"
                        );
                    }
                }
                if let Some(landed) = c.copied() {
                    assert_eq!(landed, report.at("p/d/c").is_some(), "the copy's answer");
                    assert_eq!(
                        report.detached,
                        usize::from(removed),
                        "a finished copy leaves no slot behind, and the RMDIR one"
                    );
                }
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: true,
            ..Exploration::default()
        },
    );
    assert!(
        RMDIR_REFUSALS.load(Ordering::Relaxed) > 0,
        "no run's RMDIR found a copy landed in its directory"
    );
}

/// Runs whose copy lost its name to a create, at its lookup and at its publish.
static LOST: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// A swap's copy of `p/d` against a served create of a file `d`: the name is made once, and a
/// copy that loses it, at its lookup, taking no slot, or at its publish, frees what it built.
#[test]
fn a_swap_copy_against_a_create_of_its_name() {
    let name = "h22: a swap's copy against a create of its name";
    racing_creates(name, "d", |c, report| {
        let (_, d) = report.at("p/d").expect("p/d stands");
        match c.copied() {
            Some(true) => assert!(d.is_directory() && report.at("p/d/f").is_some()),
            Some(false) => {
                assert!(d.is_file(), "the create holds d");
                // The setup frees nothing and the create, which won, kept its slot: free slots
                // are the build's, which only a copy that lost at its publish took and freed.
                let free: u32 = (0..c.tree.state.block_count())
                    .filter_map(|index| c.tree.state.resident_block(index))
                    .map(|block| block.read().node_block().node_unused_count)
                    .sum();
                LOST[usize::from(free > 0)].fetch_add(1, Ordering::Relaxed);
            }
            None => panic!("the copy did not end"),
        }
    });
    for (at, lost) in ["its lookup", "its publish"].iter().zip(&LOST) {
        assert!(
            lost.load(Ordering::Relaxed) > 0,
            "no run's copy lost its name at {at}"
        );
    }
}

/// Runs where the copy or the create looked its name up again.
static RETRIED: AtomicUsize = AtomicUsize::new(0);

/// A swap's copy of `p/d` against a served create of `x`, which moves `p`'s chain count while
/// the copy builds: the copy looks its name up again, finds it free and lands whole. (That the
/// copy looks its name up again is the unit tests'; the create's own retries count here too.)
#[test]
fn a_swap_copy_against_a_create_of_another_name() {
    let name = "h22: a swap's copy against a create of another name";
    racing_creates(name, "x", |c, report| {
        assert_eq!(c.copied(), Some(true), "the copy lands");
        assert!(report.at("p/d/f").is_some() && report.at("p/x").is_some());
        if c.tree.state.attach_retries() > 0 {
            RETRIED.fetch_add(1, Ordering::Relaxed);
        }
    });
    assert!(
        RETRIED.load(Ordering::Relaxed) > 0,
        "no run's copy or create looked its name up again"
    );
}

/// Explores a swap's copy of `p/d` holding `f` against a served create of `created` in `p`; every
/// run keeps the invariants, leaves no slot detached, and passes `check`.
fn racing_creates(
    name: &'static str,
    created: &'static str,
    check: fn(&Racing, &invariants::Report),
) {
    racing_creates_in(
        name,
        (Racer::Create(created), Listed::No, Dropped::No),
        check,
    );
}

/// What races the copy of `p/d` in `p`.
#[derive(Clone, Copy)]
enum Racer {
    /// A served create of a file of this name.
    Create(&'static str),
    /// A served rename of directory `p/q` onto `d`, the copy's name.
    RenameOnto,
}

/// Whether `p` is listed, its chain log installed, so a copy's publish checks the log since its
/// lookup rather than `p`'s chain count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listed {
    No,
    Yes,
}

/// Whether the copy is dropped at each point it parks at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dropped {
    No,
    Yes,
}

/// Runs whose copy was dropped with slots it built not freed yet.
static DROPPED_FREEING: AtomicUsize = AtomicUsize::new(0);

/// [`racing_creates`] with `p` listed or not, and the copy dropped or not; a run whose copy was
/// dropped checks the invariants alone.
fn racing_creates_in(
    name: &'static str,
    (racer, listed, dropped): (Racer, Listed, Dropped),
    check: fn(&Racing, &invariants::Report),
) {
    let scenario = Scenario::<Racing> {
        name,
        setup: Box::new(move || {
            Box::pin(async move {
                let paths: &[&str] = match racer {
                    Racer::Create(_) => &["p/"],
                    Racer::RenameOnto => &["p/", "p/q/"],
                };
                let racing = Racing::new(paths).await;
                if listed == Listed::Yes {
                    let p = racing.tree.id("p");
                    invariants::block_of(&racing.tree.state, &racing.tree.repository, p)
                        .await
                        .watch_chain(lore_revision::node::Node::index(p), None);
                }
                racing
            })
        }),
        ops: vec![
            copying("p", "d", &["f"]),
            Box::new(move |c: Arc<Racing>| {
                Box::pin(async move {
                    let p = c.tree.id("p");
                    match racer {
                        Racer::Create(created) => {
                            ops::served_create(&c.tree, p, created).await;
                        }
                        Racer::RenameOnto => {
                            ops::served_reattach_any(&c.tree, c.tree.id("p/q"), p, "d")
                                .await
                                .expect("'p/q' moves outside its own subtree");
                        }
                    }
                })
            }),
        ],
        cancellable: if dropped == Dropped::Yes {
            vec![0]
        } else {
            vec![]
        },
        check: Box::new(move |c: Arc<Racing>| {
            Box::pin(async move {
                let report =
                    crate::holding(&c.tree.state, &c.tree.repository, racing_invariants()).await;
                if c.copied().is_none() {
                    if report.detached > 0 {
                        DROPPED_FREEING.fetch_add(1, Ordering::Relaxed);
                    }
                    return;
                }
                assert_eq!(
                    report.detached, 0,
                    "a copy that lost its name freed what it built"
                );
                check(&c, &report);
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: dropped == Dropped::Yes,
            ..Exploration::default()
        },
    );
}

/// Runs whose copy below a listed `p` lost its name to a create.
static LOST_LISTED: AtomicUsize = AtomicUsize::new(0);

/// [`a_swap_copy_against_a_create_of_its_name`] below a listed `p`: the copy's publish finds the
/// name held in `p`'s log.
#[test]
fn a_swap_copy_against_a_create_of_its_name_in_a_listed_directory() {
    let name = "h22: a swap's copy against a create of its name, listed";
    racing_creates_in(
        name,
        (Racer::Create("d"), Listed::Yes, Dropped::No),
        |c, report| {
            let (_, d) = report.at("p/d").expect("p/d stands");
            if c.copied() == Some(false) {
                assert!(d.is_file(), "the create holds d");
                LOST_LISTED.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    assert!(
        LOST_LISTED.load(Ordering::Relaxed) > 0,
        "no run's copy lost its name"
    );
}

/// [`a_swap_copy_against_a_create_of_another_name`] below a listed `p`: the copy's publish
/// checks `p`'s log since its lookup and lands whole beside the create. (That it does not look its
/// name up again is the unit tests'; the create's own retries count here too.)
#[test]
fn a_swap_copy_against_a_create_of_another_name_in_a_listed_directory() {
    let name = "h22: a swap's copy against a create of another name, listed";
    racing_creates_in(
        name,
        (Racer::Create("x"), Listed::Yes, Dropped::No),
        |c, report| {
            assert_eq!(c.copied(), Some(true), "the copy lands");
            assert!(report.at("p/d/f").is_some() && report.at("p/x").is_some());
        },
    );
}

/// [`a_swap_copy_against_a_create_of_its_name`], the copy dropped at each point it parks at,
/// among them each free of an abandon after it lost its name: every slot the abandon has not
/// freed yet is detached, none typed below a slot it freed, which the invariants check.
#[test]
fn a_swap_copy_dropped_while_it_frees_what_it_built() {
    let name = "h22: a swap's copy dropped while it frees what it built";
    racing_creates_in(
        name,
        (Racer::Create("d"), Listed::No, Dropped::Yes),
        |_, _| {},
    );
    assert!(
        DROPPED_FREEING.load(Ordering::Relaxed) > 0,
        "no run dropped the copy before it freed what it built"
    );
}

/// Runs whose copy lost its name to a rename onto it, by whether `p` was listed.
static LOST_TO_RENAMES: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// A swap's copy of `p/d` against a served rename of directory `p/q` onto `d`, `p` listed or
/// not: the name is taken once, by the copy or by `q`, and a copy that loses it frees what it
/// built.
fn racing_a_rename_onto(name: &'static str, listed: Listed) {
    fn check(c: &Racing, report: &invariants::Report, listed: Listed) {
        let (_, d) = report.at("p/d").expect("p/d stands");
        assert!(d.is_directory(), "p/d is a directory");
        match c.copied() {
            Some(true) => assert!(
                report.at("p/d/f").is_some() && report.at("p/q").is_some(),
                "the copy holds d, and q stays"
            ),
            Some(false) => {
                assert!(
                    report.at("p/q").is_none() && report.at("p/d/f").is_none(),
                    "q holds d"
                );
                LOST_TO_RENAMES[listed as usize].fetch_add(1, Ordering::Relaxed);
            }
            None => panic!("the copy did not end"),
        }
    }
    match listed {
        Listed::No => racing_creates_in(name, (Racer::RenameOnto, listed, Dropped::No), |c, r| {
            check(c, r, Listed::No);
        }),
        Listed::Yes => racing_creates_in(name, (Racer::RenameOnto, listed, Dropped::No), |c, r| {
            check(c, r, Listed::Yes);
        }),
    }
    assert!(
        LOST_TO_RENAMES[listed as usize].load(Ordering::Relaxed) > 0,
        "no run's copy lost its name to the rename"
    );
}

/// [`racing_a_rename_onto`] below an unlisted `p`, the copy's publish checking `p`'s count.
#[test]
fn a_swap_copy_against_a_rename_onto_its_name() {
    racing_a_rename_onto(
        "h22: a swap's copy against a rename onto its name",
        Listed::No,
    );
}

/// [`racing_a_rename_onto`] below a listed `p`, the copy's publish checking `p`'s log, which the
/// rename's attach patches.
#[test]
fn a_swap_copy_against_a_rename_onto_its_name_in_a_listed_directory() {
    racing_a_rename_onto(
        "h22: a swap's copy against a rename onto its name, listed",
        Listed::Yes,
    );
}
