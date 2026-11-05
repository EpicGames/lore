// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The served chain rule (C11a) against the changes that race it: a served mark, removal,
//! rename or copy-up below a directory a swap removes clean or a rename moves, and each served
//! change below a directory a settle clears. Beside them, RMDIRs against a create or a swap's copy
//! into their directory (H11) and against its settle, a commit's flip against a rename of its file,
//! and H20's renames of a created file over a held name, beside a removal of the holder or a
//! settle.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_revision::state::State;

use crate::harness::catalogue::modelled::Modelling;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Report;
use crate::harness::invariants::Rules;
use crate::harness::model::Committed;
use crate::harness::model::Modelled;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;
use crate::harness::scheduler::Setup;

/// A served tree and whether its served change answered done.
struct Context {
    tree: Tree,
    done: AtomicBool,
    /// Whether a creating op's node landed; any one landing sets it.
    created: AtomicBool,
    /// How the first settle op to return ended; unset where none returned, none having run or
    /// the scheduler having dropped it midway.
    settle: OnceLock<ops::Settle>,
    /// The nodes a setup made past the tree's paths, in the order it made them.
    made: OnceLock<Vec<NodeID>>,
}

impl Modelled for Context {
    fn states(&self) -> Vec<(Arc<State>, Arc<RepositoryContext>)> {
        self.tree.states()
    }

    fn committed(&self) -> Vec<Committed> {
        self.tree.committed()
    }
}

/// The served tree `paths` with the directories `marked` marked dirty, as local changes leave
/// them.
async fn served_tree(paths: &[&str], marked: &[&str]) -> Context {
    served(Tree::new(paths).await, marked).await
}

/// `tree` served, with the directories `marked` marked dirty.
async fn served(tree: Tree, marked: &[&str]) -> Context {
    tree.state.mark_served();
    for path in marked {
        assert!(ops::served_mark_dir(&tree, tree.id(path)).await);
    }
    Context {
        tree,
        done: AtomicBool::new(false),
        created: AtomicBool::new(false),
        settle: OnceLock::new(),
        made: OnceLock::new(),
    }
}

async fn invariants_hold(tree: &Tree) -> Report {
    crate::holding(
        &tree.state,
        &tree.repository,
        Rules {
            dirty_closure: true,
            unique_names: true,
            no_ext_flags: false,
            no_builds: false,
        },
    )
    .await
}

/// How many runs a scenario took, in how many its first op answered done, and in how many the
/// served chain rule refused a change under its locks after its pre-test passed.
#[derive(Clone, Copy, Debug)]
struct Outcomes {
    runs: usize,
    done: usize,
    refused_under_locks: usize,
}

impl Outcomes {
    /// Asserts the chain rule refused a change under its locks in some run, so the race between
    /// a pre-test and its critical section was met.
    fn some_refused_under_locks(self, name: &str) -> Self {
        assert!(
            self.refused_under_locks > 0,
            "'{name}': no run refused a change under its locks: {self:?}"
        );
        self
    }

    /// Asserts some run's first op answered other than done, so the race it is for was met.
    fn some_refused(self, name: &str) {
        assert!(
            self.done < self.runs,
            "'{name}': every run answered done: {self:?}"
        );
    }
}

/// Explores `ops` on the served tree `paths` with `marked` dirty, then checks the invariants
/// and, where the first op answered done, `done_check` on the report.
fn explore(
    name: &'static str,
    (paths, marked): (&'static [&'static str], &'static [&'static str]),
    ops: Vec<Op<Context>>,
    done_check: fn(&Context, &Report),
) -> Outcomes {
    explore_from(
        name,
        Box::new(move || Box::pin(served_tree(paths, marked))),
        (ops, &[]),
        (false, done_check),
    )
}

/// [`explore`], every run's first op answering done.
fn explore_all_done(
    name: &'static str,
    (paths, marked): (&'static [&'static str], &'static [&'static str]),
    ops: Vec<Op<Context>>,
    done_check: fn(&Context, &Report),
) -> Outcomes {
    explore_from(
        name,
        Box::new(move || Box::pin(served_tree(paths, marked))),
        (ops, &[]),
        (true, done_check),
    )
}

/// [`explore`] from `setup`, also dropping the ops `cancellable` names at each point they park
/// at. Asserts some run's first op answered done, so no scenario passes on refusals alone, and
/// with `all_done`, that every run's did: a run whose op did not fails with its trace.
fn explore_from(
    name: &'static str,
    setup: Setup<Context>,
    (ops, cancellable): (Vec<Op<Context>>, &[usize]),
    (all_done, done_check): (bool, fn(&Context, &Report)),
) -> Outcomes {
    let tally = Arc::new((
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ));
    let counted = tally.clone();
    let scenario = Scenario::<Context> {
        name,
        setup,
        ops,
        cancellable: cancellable.to_vec(),
        check: Box::new(move |c: Arc<Context>| {
            let counted = counted.clone();
            Box::pin(async move {
                let report = invariants_hold(&c.tree).await;
                counted.0.fetch_add(1, Ordering::Relaxed);
                if c.tree.state.served_refusals() > 0 {
                    counted.2.fetch_add(1, Ordering::Relaxed);
                }
                assert!(
                    !all_done || c.done.load(Ordering::Relaxed),
                    "the first op answered other than done"
                );
                if c.done.load(Ordering::Relaxed) {
                    counted.1.fetch_add(1, Ordering::Relaxed);
                    done_check(&c, &report);
                }
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            cancellation: !cancellable.is_empty(),
            ..Exploration::default()
        },
    );
    let outcomes = Outcomes {
        runs: tally.0.load(Ordering::Relaxed),
        done: tally.1.load(Ordering::Relaxed),
        refused_under_locks: tally.2.load(Ordering::Relaxed),
    };
    eprintln!("'{name}': {outcomes:?}");
    assert!(
        outcomes.done > 0,
        "'{name}': no run answered done: {outcomes:?}"
    );
    outcomes
}

/// A served mark of directory `path`, recording whether it marked.
fn marking(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let marked = ops::served_mark_dir(&c.tree, c.tree.id(path)).await;
            c.done.store(marked, Ordering::Relaxed);
        })
    })
}

/// A served mark of directory `path`, recording nothing.
fn mark(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            ops::served_mark_dir(&c.tree, c.tree.id(path)).await;
        })
    })
}

/// A served removal of `path`, recording whether it removed it.
fn removing(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let detached = ops::served_detach(&c.tree, c.tree.id(path)).await;
            c.done.store(detached, Ordering::Relaxed);
        })
    })
}

/// A served create of file `name` in directory `dir`, recording whether it landed.
fn creating_done(dir: &'static str, name: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let created = ops::served_create(&c.tree, c.tree.id(dir), name).await;
            c.done.store(created.is_some(), Ordering::Relaxed);
        })
    })
}

/// A served removal of `path`, recording nothing.
fn remove(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            ops::served_detach(&c.tree, c.tree.id(path)).await;
        })
    })
}

/// A served copy-up of file `path`, found below `dir`, recording whether it rewrote the file.
fn copying_up(path: &'static str, dir: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let (f, d) = (c.tree.id(path), c.tree.id(dir));
            let copied = ops::served_copy_up(&c.tree, f, d).await;
            c.done.store(copied, Ordering::Relaxed);
        })
    })
}

/// A swap's rewrite of file `path` to its committed content where it is clean, recording
/// nothing.
fn swap_rewrite(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            ops::swap_rewrite_if_clean(&c.tree, c.tree.id(path)).await;
        })
    })
}

/// [`swap_rewrite`], recording whether it rewrote.
fn swap_rewriting(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let rewrote = ops::swap_rewrite_if_clean(&c.tree, c.tree.id(path)).await;
            c.done.store(rewrote, Ordering::Relaxed);
        })
    })
}

/// What a served move moves, which says how it moves it.
#[derive(Clone, Copy)]
enum Moving {
    /// A file, or a directory's respell, through the provider's rename ([`ops::served_rename`]).
    File,
    /// A directory to another place, through the reattach moving any directory
    /// ([`ops::served_reattach_any`]), never into its own subtree.
    Directory,
}

impl Moving {
    /// Whether the move of `node` to `name` in `dir` moved it.
    async fn moved(self, c: &Context, node: NodeID, dir: NodeID, name: &str) -> bool {
        match self {
            Self::File => ops::served_rename(&c.tree, node, dir, name).await,
            Self::Directory => ops::served_reattach_any(&c.tree, node, dir, name)
                .await
                .expect("no move here goes into its own subtree"),
        }
    }
}

/// A served move of `path` to `name` in directory `dir`, as `moving` says, recording nothing.
fn rename(
    moving: Moving,
    path: &'static str,
    dir: &'static str,
    name: &'static str,
) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let (node, dir) = (c.tree.id(path), c.tree.id(dir));
            moving.moved(&c, node, dir, name).await;
        })
    })
}

fn swap_detach_if_clean(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            ops::swap_detach_if_clean(&c.tree, c.tree.id(path)).await;
        })
    })
}

/// What a scenario's RMDIRs met, over every run: the detaches a changed chain refused, and the
/// walks after which the chain had moved.
struct Rmdirs {
    refused: AtomicUsize,
    rechecked: AtomicUsize,
}

impl Rmdirs {
    const fn new() -> Self {
        Self {
            refused: AtomicUsize::new(0),
            rechecked: AtomicUsize::new(0),
        }
    }
}

/// An RMDIR of directory `path`, recording whether it removed the directory, and counting in
/// `met` what it met.
fn rmdir(path: &'static str, met: &'static Rmdirs) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let ended = ops::served_rmdir(&c.tree, c.tree.id(path)).await;
            c.done.store(ended.removed, Ordering::Relaxed);
            met.refused.fetch_add(ended.refused, Ordering::Relaxed);
            met.rechecked.fetch_add(ended.rechecked, Ordering::Relaxed);
        })
    })
}

/// A served create of file `name` in directory `dir`, recording in `created` whether it landed.
fn creating(dir: &'static str, name: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let created = ops::served_create(&c.tree, c.tree.id(dir), name).await;
            c.created.fetch_or(created.is_some(), Ordering::Relaxed);
        })
    })
}

/// A swap's copy of file `name` into directory `dir`, recording whether it landed.
fn swap_copying(dir: &'static str, name: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let copied = ops::swap_attach(&c.tree, c.tree.id(dir), name).await;
            c.created.fetch_or(copied, Ordering::Relaxed);
        })
    })
}

fn live_and_dirty(c: &Context, report: &Report, path: &str) -> NodeID {
    let id = c.tree.id(path);
    let node = report
        .nodes
        .get(&id)
        .unwrap_or_else(|| panic!("'{path}' is no longer in the tree"));
    assert!(node.is_dirty(), "'{path}' is not dirty");
    id
}

/// A mark climbing three clean levels, and a swap's clean detach of the top one: a mark that
/// answered done left its target live and dirty, so the detach refused.
#[test]
fn a_climbing_mark_and_a_clean_detach_of_an_ancestor() {
    let name = "served: a climbing mark against an ancestor's clean detach";
    explore(
        name,
        (&["p/q/d/"], &[]),
        vec![marking("p/q/d"), swap_detach_if_clean("p")],
        |c, report| {
            live_and_dirty(c, report, "p/q/d");
        },
    )
    .some_refused(name);
}

/// [`a_climbing_mark_and_a_clean_detach_of_an_ancestor`], the mark dropped at each point it
/// parks at: a mark cancelled mid-descent leaves the levels it marked dirty above the ones it
/// did not reach, and the detach refuses them.
#[test]
fn a_cancelled_climbing_mark_and_a_clean_detach_of_an_ancestor() {
    explore_from(
        "served: a cancelled climbing mark against an ancestor's clean detach",
        Box::new(|| Box::pin(served_tree(&["p/q/d/"], &[]))),
        (vec![marking("p/q/d"), swap_detach_if_clean("p")], &[0]),
        (false, |c, report| {
            live_and_dirty(c, report, "p/q/d");
        }),
    );
}

/// [`a_climbing_mark_and_a_clean_detach_of_an_ancestor`] with the mark's levels across blocks,
/// `p` in block 0 above `q` and `d` in block 1, so its pair locks take two blocks; and again
/// from a State loaded from the store, so its passes load the blocks they climb through.
#[test]
fn a_climbing_mark_across_blocks_and_a_clean_detach_of_an_ancestor() {
    async fn spread() -> Tree {
        let tree = Tree::spread(&["p/"], "fill", &["p/q/d/"]).await;
        assert_ne!(
            lore_revision::node::NodeBlock::index(tree.id("p")),
            lore_revision::node::NodeBlock::index(tree.id("p/q"))
        );
        tree
    }
    let name = "served: a climbing mark across blocks against an ancestor's clean detach";
    explore_from(
        name,
        Box::new(|| Box::pin(async { served(spread().await, &[]).await })),
        (vec![marking("p/q/d"), swap_detach_if_clean("p")], &[]),
        (false, |c, report| {
            live_and_dirty(c, report, "p/q/d");
        }),
    )
    .some_refused(name);
    let name = "served: a climbing mark in a loaded State against an ancestor's clean detach";
    explore_from(
        name,
        Box::new(|| {
            Box::pin(async {
                let tree = spread().await.reloaded().await;
                let q = lore_revision::node::NodeBlock::index(tree.id("p/q"));
                assert!(
                    tree.state.resident_block(q).is_none(),
                    "block {q} is in memory"
                );
                served(tree, &[]).await
            })
        }),
        (vec![marking("p/q/d"), swap_detach_if_clean("p")], &[]),
        (false, |c, report| {
            live_and_dirty(c, report, "p/q/d");
        }),
    )
    .some_refused(name);
}

/// A mark six clean levels deep, past one pass, so it marks the directory at the top of its
/// first pass first, and a swap's clean detach of the top one: a mark that answered done left
/// its target live and dirty.
#[test]
fn a_deep_mark_and_a_clean_detach_of_its_top() {
    let name = "served: a deep mark against its top's clean detach";
    explore(
        name,
        (&["a/b/c/d/e/f/"], &[]),
        vec![marking("a/b/c/d/e/f"), swap_detach_if_clean("a")],
        |c, report| {
            live_and_dirty(c, report, "a/b/c/d/e/f");
        },
    )
    .some_refused(name);
}

/// A mark climbing two clean levels below a dirty directory, and a served removal of that
/// directory: the removal lands before the climb reads it, between the climb and the descent,
/// between two levels of the descent, or after the mark. A mark that answered done left its target
/// dirty wherever it is still in the tree; one the removal cut off before the level below the
/// removed directory answered not found. A removal landing below a level already marked leaves the
/// mark to finish in the detached subtree.
#[test]
fn a_climbing_mark_and_a_removal_of_an_ancestor() {
    let name = "served: a climbing mark against an ancestor's removal";
    explore(
        name,
        (&["p/q/d/"], &["p"]),
        vec![marking("p/q/d"), remove("p")],
        |c, report| {
            if let Some(d) = report.nodes.get(&c.tree.id("p/q/d")) {
                assert!(d.is_dirty(), "'p/q/d' is not dirty");
            }
        },
    )
    .some_refused(name);
}

/// A mark climbing two clean levels, and a served rename moving the middle one to another clean
/// directory: the target ends dirty wherever the rename left it, with dirty closure.
#[test]
fn a_climbing_mark_and_a_rename_of_an_ancestor() {
    let name = "served: a climbing mark against an ancestor's reattach";
    explore_all_done(
        name,
        (&["a/b/c/", "x/"], &[]),
        vec![marking("a/b/c"), rename(Moving::Directory, "a/b", "x", "b")],
        |c, report| {
            live_and_dirty(c, report, "a/b/c");
        },
    );
}

/// A served removal of a file, and a swap's clean detach of its directory: a removal that
/// answered done left the directory live and dirty, so a diff sees the deletion.
#[test]
fn a_removal_and_a_clean_detach_of_its_directory() {
    let name = "served: a removal against its directory's clean detach";
    explore(
        name,
        (&["a/f"], &[]),
        vec![removing("a/f"), swap_detach_if_clean("a")],
        |c, report| {
            live_and_dirty(c, report, "a");
            assert!(
                !report.nodes.contains_key(&c.tree.id("a/f")),
                "the removed file is still in the tree"
            );
        },
    )
    .some_refused(name);
}

/// A served rename of a file between two clean directories, and a swap's clean detach of the
/// destination's parent: a rename that answered done left the file live and both directories
/// dirty.
#[test]
fn a_rename_and_a_clean_detach_of_the_destination() {
    let name = "served: a rename against its destination's clean detach";
    explore(
        name,
        (&["a/f", "p/x/"], &[]),
        vec![
            renaming(Moving::File, "a/f", "p/x", "f"),
            swap_detach_if_clean("p"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a");
            live_and_dirty(c, report, "p/x");
            assert_eq!(
                report.nodes.get(&c.tree.id("a/f")).map(|node| node.parent),
                Some(c.tree.id("p/x")),
                "the renamed file is not in its destination"
            );
        },
    )
    .some_refused(name);
}

/// A served copy-up of a file two clean levels down, and a swap's clean detach of the top
/// directory: a copy-up that answered done left the file live and dirty.
#[test]
fn a_copy_up_and_a_clean_detach_of_an_ancestor() {
    let name = "served: a copy-up against an ancestor's clean detach";
    explore(
        name,
        (&["p/d/f"], &[]),
        vec![copying_up("p/d/f", "p/d"), swap_detach_if_clean("p")],
        |c, report| {
            live_and_dirty(c, report, "p/d/f");
        },
    )
    .some_refused(name);
}

/// A swap's rewrite of a file, and a served removal of it: the rewrite lands on the live file or
/// finds it gone, and neither leaves a dirty node under a clean directory.
#[test]
fn a_swaps_rewrite_and_a_removal_of_its_file() {
    let name = "served: a swap's rewrite against a removal of its file";
    explore(
        name,
        (&["a/f"], &["a/"]),
        vec![swap_rewriting("a/f"), remove("a/f")],
        |_, _| {},
    )
    .some_refused(name);
}

/// A copy-up of a file, and a swap's rewrite of it to the target's content where it is clean: the
/// rewrite lands before the copy-up, or finds the file local and leaves it, so a copy-up that
/// answered done left the file dirty with local content and no address.
#[test]
fn a_copy_up_and_a_swaps_rewrite_of_its_file() {
    let name = "served: a copy-up against a swap's rewrite of its file";
    explore_all_done(
        name,
        (&["a/f"], &[]),
        vec![copying_up("a/f", "a"), swap_rewrite("a/f")],
        |c, report| {
            let f = live_and_dirty(c, report, "a/f");
            let node = &report.nodes[&f];
            assert!(
                node.ext_flags().contains(NodeExtFlags::LocalContent),
                "the copy-up's local content stands"
            );
            assert!(node.address.hash.is_zero(), "no address over local content");
        },
    );
}

/// A copy-up of a file, and a served rename moving the file to another clean directory: the
/// copy-up follows the file to where the rename left it, marking that directory, and a copy-up
/// that answered done left the file dirty below a dirty directory.
#[test]
fn a_copy_up_and_a_rename_of_its_file() {
    let name = "served: a copy-up against a rename of its file";
    explore_all_done(
        name,
        (&["a/f", "x/"], &[]),
        vec![
            copying_up("a/f", "a"),
            rename(Moving::File, "a/f", "x", "f"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/f");
        },
    );
}

/// H11: an RMDIR of an empty directory and a create in it. The RMDIR removes the directory only
/// where the create has not landed in it, wherever the create's attach falls between the RMDIR's
/// emptiness check and its detach, so no created file goes with a removed directory.
#[test]
fn an_rmdir_and_a_create_in_its_directory() {
    static MET: Rmdirs = Rmdirs::new();
    let name = "served: an RMDIR and a create in its directory";
    explore(
        name,
        (&["keep", "d/"], &[]),
        vec![rmdir("d", &MET), creating("d", "x")],
        |c, _| {
            assert!(
                !c.created.load(Ordering::Relaxed),
                "a file created in the removed directory went with it"
            );
        },
    )
    .some_refused(name);
    assert!(
        MET.refused.load(Ordering::Relaxed) > 0,
        "'{name}': no run's detach found the chain changed"
    );
}

/// [`an_rmdir_and_a_create_in_its_directory`] with walks parking at their own points: a create
/// landing while the RMDIR walks has it find the chain moved after the walk and check again
/// before its detach, as the VFS provider's does before it touches the host.
#[test]
fn an_rmdir_whose_walk_parks_and_a_create_in_its_directory() {
    static MET: Rmdirs = Rmdirs::new();
    let name = "served: an RMDIR whose walk parks and a create in its directory";
    let scenario = Scenario::<Context> {
        name,
        setup: Box::new(|| Box::pin(served_tree(&["keep", "d/"], &[]))),
        ops: vec![rmdir("d", &MET), creating("d", "x")],
        cancellable: vec![],
        check: Box::new(|c: Arc<Context>| {
            Box::pin(async move {
                invariants_hold(&c.tree).await;
                assert!(
                    !(c.done.load(Ordering::Relaxed) && c.created.load(Ordering::Relaxed)),
                    "a file created in the removed directory went with it"
                );
            })
        }),
    };
    crate::explored(
        scenario,
        Modelling::default(),
        Exploration {
            reader_points: true,
            ..Exploration::default()
        },
    );
    assert!(
        MET.rechecked.load(Ordering::Relaxed) > 0,
        "'{name}': no run's RMDIR found the chain moved after its walk"
    );
}

/// H11: an RMDIR of an empty directory against both a create in it and a swap's copy into it.
/// The RMDIR removes the directory only where neither has landed in it.
#[test]
fn an_rmdir_a_create_and_a_swap_copy_in_its_directory() {
    static MET: Rmdirs = Rmdirs::new();
    let name = "served: an RMDIR, a create and a swap copy in its directory";
    explore(
        name,
        (&["keep", "d/"], &[]),
        vec![rmdir("d", &MET), creating("d", "x"), swap_copying("d", "y")],
        |c, _| {
            assert!(
                !c.created.load(Ordering::Relaxed),
                "a file that landed in the removed directory went with it"
            );
        },
    )
    .some_refused(name);
    assert!(
        MET.refused.load(Ordering::Relaxed) > 0,
        "'{name}': no run's detach found the chain changed"
    );
}

/// H11: an RMDIR of an empty directory and a swap's copy into it, which marks nothing. The RMDIR
/// removes the directory only where the copy has not landed in it.
#[test]
fn an_rmdir_and_a_swap_copy_into_its_directory() {
    static MET: Rmdirs = Rmdirs::new();
    let name = "served: an RMDIR and a swap copy into its directory";
    explore(
        name,
        (&["keep", "d/"], &[]),
        vec![rmdir("d", &MET), swap_copying("d", "x")],
        |c, _| {
            assert!(
                !c.created.load(Ordering::Relaxed),
                "a file copied into the removed directory went with it"
            );
        },
    )
    .some_refused(name);
    assert!(
        MET.refused.load(Ordering::Relaxed) > 0,
        "'{name}': no run's detach found the chain changed"
    );
}

fn settle(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let outcome = ops::settle(&c.tree, c.tree.id(path)).await;
            let _ = c.settle.set(outcome);
        })
    })
}

/// The settle race's tree: `a/d` dirty with no dirty child, so a settle alone clears it, and
/// `x` dirty as a rename's destination.
const SETTLED: (&[&str], &[&str]) = (&["a/d/f", "a/d/e/", "x/"], &["a/d", "x"]);

/// A create in a directory a settle clears: a create that answered attached left the directory
/// dirty above its dirty file, whichever phase of the settle it met (`NeedsMark`, the mark, the
/// retry).
#[test]
fn a_create_and_a_settle_of_its_directory() {
    let outcomes = explore(
        "served: a create against a settle of its directory",
        SETTLED,
        vec![creating_done("a/d", "y"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d");
            // A settle refuses a publish at most twice: through its pending clear, and through
            // the clear itself where it lands after the attempt read the directory.
            assert!(
                c.tree.state.attach_retries() <= 2,
                "the create looked its name up {} times again",
                c.tree.state.attach_retries()
            );
        },
    );
    outcomes.some_refused_under_locks("served: a create against a settle of its directory");
}

/// An unserved create in a directory a settle clears, beyond the contract (no unserved writer runs
/// on a served State), so through no kind: the create neither refuses nor drops the pending clear,
/// so where it lands between the settle's read and its clear, the chain count alone keeps the
/// directory, and dirty closure holds. The unserved mark stops at the directory, which keeps its
/// base `Dirty` bit through the settle's first phase, and drops no pending clear, so only the chain
/// count catches the create; a served create's mark drops the pending clear.
#[test]
fn an_unserved_create_and_a_settle_of_its_directory() {
    static KEPT_AT_FINISH: AtomicUsize = AtomicUsize::new(0);
    KEPT_AT_FINISH.store(0, Ordering::Relaxed);
    explore_all_done(
        "served: an unserved create against a settle of its directory",
        SETTLED,
        vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let (state, repository) = (&c.tree.state, &c.tree.repository);
                    let file = ops::file("y");
                    let attached = state
                        .node_attach(repository.clone(), c.tree.id("a/d"), file, "y")
                        .await
                        .expect("an unserved attach runs");
                    let AttachOutcome::Attached(id) = attached else {
                        return;
                    };
                    let added =
                        Mark::dirty(NodeFlags::DirtyAdd, Ext::Set(NodeExtFlags::NoFlags), false);
                    state
                        .node_mark(repository, id, added)
                        .await
                        .expect("a mark succeeds");
                    c.done.store(true, Ordering::Relaxed);
                })
            }),
            settle("a/d"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/d");
            if c.settle.get() == Some(&ops::Settle::KeptAtFinish) {
                KEPT_AT_FINISH.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    assert!(
        KEPT_AT_FINISH.load(Ordering::Relaxed) > 0,
        "the chain count never kept the directory"
    );
}

/// A removal from a directory a settle clears: a removal that answered done left the directory
/// dirty, so a diff sees the deletion.
#[test]
fn a_removal_and_a_settle_of_its_directory() {
    let name = "served: a removal against a settle of its directory";
    explore_all_done(
        name,
        SETTLED,
        vec![removing("a/d/f"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d");
        },
    )
    .some_refused_under_locks(name);
}

/// A rename out of a directory a settle clears: a rename that answered done left both
/// directories dirty.
#[test]
fn a_rename_and_a_settle_of_its_source() {
    let name = "served: a rename against a settle of its source";
    explore_all_done(
        name,
        SETTLED,
        vec![renaming(Moving::File, "a/d/f", "x", "f"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d");
            live_and_dirty(c, report, "x");
        },
    )
    .some_refused_under_locks(name);
}

/// A rename within a directory a settle clears, which re-prepends the file in the chain the
/// settle walks: a rename that answered done left the directory dirty, the settle's walk parked
/// at each child.
#[test]
fn a_rename_within_a_directory_and_its_settle() {
    let name = "served: a rename within a directory against its settle";
    explore_all_done(
        name,
        SETTLED,
        vec![renaming(Moving::File, "a/d/f", "a/d", "g"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d");
        },
    );
}

/// A case-only respell of a directory a settle clears: the name hash stays, so the settle finds
/// the directory at its place, but a respell that answered done left it dirty, whichever phase
/// it met, so status reports the new spelling.
#[test]
fn a_respell_of_a_directory_and_its_settle() {
    let name = "served: a respell of a directory against its settle";
    explore_all_done(
        name,
        SETTLED,
        vec![renaming(Moving::File, "a/d", "a", "D"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d");
        },
    );
}

/// A move of a local directory as the VFS provider's RENAME makes one, against a settle clearing
/// it, which drops its local content: either the move lands first, and the settle keeps the
/// directory, local and dirty, where it went, or the settle clears it and the move refuses with
/// nothing moved, whether the clear lands before the move's check ahead of its marks or between
/// that check and its locks.
#[test]
fn a_local_directory_move_and_its_settle() {
    // Runs that moved and runs that refused, so the check reaches both.
    static MOVED: AtomicUsize = AtomicUsize::new(0);
    static REFUSED: AtomicUsize = AtomicUsize::new(0);
    explore_from(
        "served: a local directory's move against its settle",
        Box::new(|| {
            Box::pin(async {
                let c = served_tree(SETTLED.0, SETTLED.1).await;
                ops::hold_local(&c.tree, c.tree.id("a/d")).await;
                c
            })
        }),
        (
            vec![
                Box::new(|c: Arc<Context>| {
                    Box::pin(async move {
                        let (d, x) = (c.tree.id("a/d"), c.tree.id("x"));
                        ops::served_rename(&c.tree, d, x, "d").await;
                        c.done.store(true, Ordering::Relaxed);
                    })
                }),
                settle("a/d"),
            ],
            &[],
        ),
        (true, |c, report| {
            let d = &report.nodes[&c.tree.id("a/d")];
            let local = d.ext_flags().contains(NodeExtFlags::LocalContent);
            if d.parent == c.tree.id("x") {
                MOVED.fetch_add(1, Ordering::Relaxed);
                assert!(local, "a cleared directory moved");
                assert!(d.is_dirty(), "the moved directory is clean");
            } else {
                REFUSED.fetch_add(1, Ordering::Relaxed);
                assert_eq!(
                    c.settle.get(),
                    Some(&ops::Settle::Cleared),
                    "the move refused a directory its settle did not clear"
                );
                assert!(!local, "the cleared directory kept its local content");
            }
        }),
    );
    assert!(
        MOVED.load(Ordering::Relaxed) > 0 && REFUSED.load(Ordering::Relaxed) > 0,
        "the runs moved {MOVED:?} and refused {REFUSED:?}"
    );
}

/// A rename into a directory a settle clears: a rename that answered done left the destination
/// dirty.
#[test]
fn a_rename_and_a_settle_of_its_destination() {
    let name = "served: a rename against a settle of its destination";
    explore_all_done(
        name,
        SETTLED,
        vec![renaming(Moving::Directory, "a/d/e", "x", "e"), settle("x")],
        |c, report| {
            live_and_dirty(c, report, "x");
        },
    )
    .some_refused_under_locks(name);
}

/// A copy-up of a file in a directory a settle clears: a copy-up that answered done left the
/// file dirty below a dirty directory.
#[test]
fn a_copy_up_and_a_settle_of_its_directory() {
    let name = "served: a copy-up against a settle of its directory";
    explore_all_done(
        name,
        SETTLED,
        vec![copying_up("a/d/f", "a/d"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d/f");
            live_and_dirty(c, report, "a/d");
        },
    );
}

/// A mark of a subdirectory of a directory a settle clears: a mark that answered done left both
/// dirty.
#[test]
fn a_mark_and_a_settle_of_its_parent() {
    let name = "served: a mark against a settle of its parent";
    explore_all_done(
        name,
        SETTLED,
        vec![marking("a/d/e"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d/e");
            live_and_dirty(c, report, "a/d");
        },
    )
    .some_refused_under_locks(name);
}

/// A copy-up of a file, a rename moving it into a directory a settle clears, and that settle:
/// the copy-up follows the file into the directory, and one that answered done left the file
/// dirty below a dirty directory.
#[test]
fn a_copy_up_a_rename_and_a_settle_of_the_destination() {
    let name = "served: a copy-up and a rename against a settle of the destination";
    explore_all_done(
        name,
        SETTLED,
        vec![
            copying_up("a/d/f", "a/d"),
            rename(Moving::File, "a/d/f", "x", "f"),
            settle("x"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/d/f");
        },
    );
}

/// The replacing rename's tree: `x` dirty holding clean `h`, so a settle alone clears it.
const REPLACED: (&[&str], &[&str]) = (&["a/f", "x/h"], &["a", "x"]);

/// A served rename of a file over a name held in a directory a settle clears: a rename that
/// answered done left the file in the directory, in place of the holder, and the directory
/// dirty.
#[test]
fn a_replacing_rename_and_a_settle_of_its_directory() {
    let name = "served: a replacing rename against a settle of its directory";
    explore_all_done(
        name,
        REPLACED,
        vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let (f, x, h) = (c.tree.id("a/f"), c.tree.id("x"), c.tree.id("x/h"));
                    let moved = ops::served_rename_replacing(&c.tree, f, (x, "h"), h).await;
                    c.done.store(moved, Ordering::Relaxed);
                })
            }),
            settle("x"),
        ],
        |c, report| {
            let x = live_and_dirty(c, report, "x");
            assert_eq!(
                report.at("x/h").map(|(id, node)| (id, node.parent)),
                Some((c.tree.id("a/f"), x))
            );
            assert!(
                !report.nodes.contains_key(&c.tree.id("x/h")),
                "the holder is still in the tree"
            );
        },
    )
    // The destination's refusal under the locks: the chain rule's second parent.
    .some_refused_under_locks(name);
}

/// A served rename of a file over a held name, and a served removal of the holder: whichever
/// lands first, the directory holds one node under the name, and a rename that answered done
/// left the file there.
#[test]
fn a_replacing_rename_and_a_removal_of_the_holder() {
    let name = "served: a replacing rename against a removal of the holder";
    explore_all_done(
        name,
        (&["a/f", "x/h"], &[]),
        vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let (f, x, h) = (c.tree.id("a/f"), c.tree.id("x"), c.tree.id("x/h"));
                    let moved = ops::served_rename_replacing(&c.tree, f, (x, "h"), h).await;
                    c.done.store(moved, Ordering::Relaxed);
                })
            }),
            remove("x/h"),
        ],
        |c, report| {
            let x = live_and_dirty(c, report, "x");
            assert_eq!(
                report.at("x/h").map(|(id, node)| (id, node.parent)),
                Some((c.tree.id("a/f"), x))
            );
            assert!(
                !report.nodes.contains_key(&c.tree.id("x/h")),
                "the holder is still in the tree"
            );
        },
    );
}

/// A create in a directory a settle clears, and a swap's clean detach of the directory: the
/// detach takes the directory only once the settle cleared it with no create in it, and a
/// create that answered attached is still in the tree below a dirty directory.
#[test]
fn a_create_a_settle_and_a_clean_detach_of_the_directory() {
    let name = "served: a create against a settle and a clean detach of its directory";
    explore(
        name,
        SETTLED,
        vec![
            creating_done("a/d", "y"),
            settle("a/d"),
            swap_detach_if_clean("a/d"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/d");
            assert!(
                report.at("a/d/y").is_some(),
                "the created file is not in the tree"
            );
        },
    )
    .some_refused(name);
}

/// A mark of a subdirectory, a settle of its parent and a mark of the parent, as a settle's
/// re-mark makes one: a mark that answered done left both dirty.
#[test]
fn a_mark_a_settle_and_a_mark_of_the_parent() {
    let name = "served: a mark against a settle and a mark of its parent";
    explore_all_done(
        name,
        SETTLED,
        vec![marking("a/d/e"), settle("a/d"), mark("a/d")],
        |c, report| {
            live_and_dirty(c, report, "a/d/e");
            live_and_dirty(c, report, "a/d");
        },
    );
}

/// A mark of a dirty directory, a settle of it, and a settle of its parent: the mark meets the
/// pending clear on the directory, on its parent, or on both, climbs above them and drops each,
/// so neither settle clears below a mark under way; a settle after the mark clears as it finds.
/// Dirty closure holds, and no pending clear outlives its settle.
#[test]
fn a_mark_and_settles_of_a_directory_and_its_parent() {
    let name = "served: a mark against settles of its directory and its parent";
    explore_all_done(
        name,
        (&["a/d/f", "a/d/e/"], &["a/d/e"]),
        vec![marking("a/d/e"), settle("a/d/e"), settle("a/d")],
        |_, _| {},
    );
}

/// A served move of `path` to `name` in directory `dir`, as `moving` says, then a served removal of
/// the directory it left, `left`, as a user moving a directory's last entry away and removing the
/// directory makes them.
fn rename_then_remove(
    moving: Moving,
    path: &'static str,
    (dir, name): (&'static str, &'static str),
    left: &'static str,
) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let (node, dir) = (c.tree.id(path), c.tree.id(dir));
            assert!(moving.moved(&c, node, dir, name).await);
            assert!(ops::served_detach(&c.tree, c.tree.id(left)).await);
        })
    })
}

/// A mark of a directory whose parent moves away from a directory then removed: the mark
/// follows its target to where it went, whatever became of the directory it left, and always
/// marks it.
#[test]
fn a_mark_and_its_targets_parent_moved_out_of_a_removed_directory() {
    let name = "served: a mark against its target's parent moving out of a removed directory";
    explore_all_done(
        name,
        (&["a/i/t/", "b/"], &[]),
        vec![
            marking("a/i/t"),
            rename_then_remove(Moving::Directory, "a/i", ("b", "i"), "a"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/i/t");
        },
    );
}

/// A removal of a file that moves away from a directory then removed: the removal follows the
/// file to where it went and always removes it.
#[test]
fn a_removal_and_its_file_moved_out_of_a_removed_directory() {
    let name = "served: a removal against its file moving out of a removed directory";
    explore_all_done(
        name,
        (&["a/f", "b/"], &[]),
        vec![
            removing("a/f"),
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let (f, b) = (c.tree.id("a/f"), c.tree.id("b"));
                    // The rename finds the file gone where the removal landed first.
                    if ops::served_rename(&c.tree, f, b, "f").await {
                        assert!(ops::served_detach(&c.tree, c.tree.id("a")).await);
                    }
                })
            }),
        ],
        |c, report| {
            assert!(
                !report.nodes.contains_key(&c.tree.id("a/f")),
                "the removed file is still in the tree"
            );
        },
    );
}

/// A copy-up of a file that moves away from a directory then removed: the copy-up follows the
/// file to where it went and always rewrites it.
#[test]
fn a_copy_up_and_its_file_moved_out_of_a_removed_directory() {
    let name = "served: a copy-up against its file moving out of a removed directory";
    explore_all_done(
        name,
        (&["a/f", "b/"], &[]),
        vec![
            copying_up("a/f", "a"),
            rename_then_remove(Moving::File, "a/f", ("b", "f"), "a"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/f");
        },
    );
}

/// A create in a directory, and a served rename moving that directory, or its parent, to
/// another clean directory: the create lands wherever the directory went, below dirty
/// directories.
#[test]
fn a_create_and_a_rename_of_its_directory_or_its_parent() {
    for (name, moved, (to, as_name)) in [
        (
            "served: a create against a rename of its directory",
            "a/b/d",
            ("x", "d"),
        ),
        (
            "served: a create against a rename of its directory's parent",
            "a/b",
            ("x", "b"),
        ),
    ] {
        explore_from(
            name,
            Box::new(|| Box::pin(served_tree(&["a/b/d/", "x/"], &[]))),
            (
                vec![
                    creating_done("a/b/d", "y"),
                    rename(Moving::Directory, moved, to, as_name),
                ],
                &[],
            ),
            (true, |c, report| {
                let d = live_and_dirty(c, report, "a/b/d");
                assert!(
                    report
                        .nodes
                        .values()
                        .any(|node| node.parent == d && node.is_dirty_add()),
                    "the created file is not in its directory"
                );
            }),
        );
    }
}

/// A removal of a file, and a served rename moving its directory: the removal follows the
/// directory and leaves it dirty.
#[test]
fn a_removal_and_a_rename_of_its_directory() {
    let name = "served: a removal against a rename of its directory";
    explore_all_done(
        name,
        (&["a/d/f", "x/"], &[]),
        vec![
            removing("a/d/f"),
            rename(Moving::Directory, "a/d", "x", "d"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/d");
            assert!(!report.nodes.contains_key(&c.tree.id("a/d/f")));
        },
    );
}

/// A copy-up of a file, and a served rename moving its directory: the copy-up's hint stays
/// right, and the file ends dirty below dirty directories wherever the directory went.
#[test]
fn a_copy_up_and_a_rename_of_its_directory() {
    let name = "served: a copy-up against a rename of its directory";
    explore_all_done(
        name,
        (&["a/d/f", "x/"], &[]),
        vec![
            copying_up("a/d/f", "a/d"),
            rename(Moving::Directory, "a/d", "x", "d"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/d/f");
        },
    );
}

/// A mark of a directory, and a served rename moving that directory: the mark follows it and
/// always marks it.
#[test]
fn a_mark_and_a_rename_of_its_target() {
    let name = "served: a mark against a rename of its target";
    explore_all_done(
        name,
        (&["a/d/", "x/"], &[]),
        vec![marking("a/d"), rename(Moving::Directory, "a/d", "x", "d")],
        |c, report| {
            live_and_dirty(c, report, "a/d");
        },
    );
}

/// Two served renames of directories out of one directory into another, which the moves permit
/// serializes: both land, below dirty directories.
#[test]
fn two_directory_renames_between_the_same_directories() {
    let name = "served: two directory renames between the same directories";
    explore_all_done(
        name,
        (&["a/b/", "a/c/", "x/"], &[]),
        vec![
            renaming(Moving::Directory, "a/b", "x", "b"),
            rename(Moving::Directory, "a/c", "x", "c"),
        ],
        |c, report| {
            let x = live_and_dirty(c, report, "x");
            for path in ["a/b", "a/c"] {
                assert_eq!(
                    report.nodes[&c.tree.id(path)].parent,
                    x,
                    "'{path}' is not in x"
                );
            }
        },
    );
}

/// A served reattach of `path` into `dir` as `name` that may meet a cycle, moving any directory
/// ([`ops::served_reattach_any`]), recording whether it moved where `records`; a move the subtree
/// check refuses, as a move into its own subtree, is no error.
fn rename_or_refuse(
    path: &'static str,
    (dir, name): (&'static str, &'static str),
    records: bool,
) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let (node, dir) = (c.tree.id(path), c.tree.id(dir));
            let moved = ops::served_reattach_any(&c.tree, node, dir, name).await == Some(true);
            if records {
                c.done.store(moved, Ordering::Relaxed);
            }
        })
    })
}

/// Two served renames of directories into each other: the moves permit and the subtree check
/// let exactly one land, and both directories stay reachable from the root.
#[test]
fn two_directory_renames_into_each_other() {
    let name = "served: two directory renames into each other";
    let outcomes = explore(
        name,
        (&["a/b/", "x/"], &[]),
        vec![
            rename_or_refuse("a/b", ("x", "b"), true),
            rename_or_refuse("x", ("a/b", "x"), false),
        ],
        |c, report| {
            for path in ["a/b", "x"] {
                assert!(
                    report.nodes.contains_key(&c.tree.id(path)),
                    "'{path}' is cut off from the root"
                );
            }
        },
    );
    outcomes.some_refused(name);
}

/// [`a_create_and_a_settle_of_its_directory`] and [`a_rename_and_a_settle_of_its_source`] with
/// the settled directory's children across blocks: `a`, `a/d` and `a/d/f` in block 0, `a/d/e`
/// and `x` in block 1, so the pre-tests and the settle's phases touch two blocks.
#[test]
fn a_create_or_a_rename_and_a_settle_across_blocks() {
    async fn spread() -> Context {
        let tree = Tree::spread(&["a/d/f"], "fill", &["a/d/e/", "x/"]).await;
        assert_ne!(
            lore_revision::node::NodeBlock::index(tree.id("a/d")),
            lore_revision::node::NodeBlock::index(tree.id("a/d/e"))
        );
        served(tree, &["a/d", "x"]).await
    }
    let name = "served: a create against a settle across blocks";
    explore_from(
        name,
        Box::new(|| Box::pin(spread())),
        (vec![creating_done("a/d", "y"), settle("a/d")], &[]),
        (true, |c, report| {
            live_and_dirty(c, report, "a/d");
        }),
    );
    let name = "served: a rename against a settle of its source across blocks";
    explore_from(
        name,
        Box::new(|| Box::pin(spread())),
        (
            vec![
                renaming(Moving::Directory, "a/d/e", "x", "e"),
                settle("a/d"),
            ],
            &[],
        ),
        (true, |c, report| {
            live_and_dirty(c, report, "a/d");
            live_and_dirty(c, report, "x");
        }),
    );
}

/// A served move of `path` to `name` in directory `dir`, as `moving` says, recording whether it
/// moved.
fn renaming(
    moving: Moving,
    path: &'static str,
    dir: &'static str,
    name: &'static str,
) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let (node, dir) = (c.tree.id(path), c.tree.id(dir));
            let moved = moving.moved(&c, node, dir, name).await;
            c.done.store(moved, Ordering::Relaxed);
        })
    })
}

/// A mark five clean levels deep, past one pass, so it marks the directory at the top of the
/// path first, and the target's path moved out of that directory, which is then removed: the
/// mark finds the deep directory detached, checks the target's own chain, finds it live, and
/// marks it where it went.
#[test]
fn a_deep_mark_and_its_path_moved_out_of_a_removed_deep_directory() {
    explore_all_done(
        "served: a deep mark against its path moving out of a removed deep directory",
        (&["a/b/c/d/e/", "x/"], &[]),
        vec![
            marking("a/b/c/d/e"),
            rename_then_remove(Moving::Directory, "a/b", ("x", "b"), "a"),
        ],
        |c, report| {
            live_and_dirty(c, report, "a/b/c/d/e");
        },
    );
}

/// A served rename of a directory a settle clears: a settle that finds it moved from the place
/// it resolved leaves it as the rename left it, and one that cleared it first leaves it clean
/// below the dirty directory the rename moved it to.
#[test]
fn a_rename_of_a_directory_and_its_settle() {
    explore_all_done(
        "served: a rename of a directory against its settle",
        SETTLED,
        vec![renaming(Moving::Directory, "a/d", "x", "d"), settle("a/d")],
        |c, report| {
            live_and_dirty(c, report, "x");
            assert_eq!(report.nodes[&c.tree.id("a/d")].parent, c.tree.id("x"));
        },
    );
}

/// A served rename of a file that another rename moves out of a directory then removed: the
/// rename follows the file from where it went and always lands.
#[test]
fn a_rename_and_its_file_moved_out_of_a_removed_directory() {
    explore_all_done(
        "served: a rename against its file moving out of a removed directory",
        (&["a/f", "b/", "x/"], &[]),
        vec![
            renaming(Moving::File, "a/f", "x", "g"),
            rename_then_remove(Moving::File, "a/f", ("b", "f"), "a"),
        ],
        |c, report| {
            let f = &report.nodes[&c.tree.id("a/f")];
            assert!(
                report.nodes[&f.parent].is_dirty(),
                "the file's directory is clean"
            );
        },
    );
}

/// [`a_copy_up_and_a_settle_of_its_directory`], the settle also dropped at every point it parks
/// at: a settle cancelled with its clear pending drops it, and the copy-up lands below a dirty
/// directory.
#[test]
fn a_copy_up_and_a_cancelled_settle_of_its_directory() {
    explore_from(
        "served: a copy-up against a cancelled settle of its directory",
        Box::new(|| Box::pin(served_tree(SETTLED.0, SETTLED.1))),
        (vec![copying_up("a/d/f", "a/d"), settle("a/d")], &[1]),
        (true, |c, report| {
            live_and_dirty(c, report, "a/d/f");
            live_and_dirty(c, report, "a/d");
        }),
    );
}

/// [`a_create_and_a_settle_of_its_directory`] in a directory recorded added, the settle also
/// dropped at every point it parks at: a settle that returned left the directory no longer added,
/// cleared, or kept dirty through either keep; one dropped midway left it dirty, with no clear
/// pending. Whether a dropped settle still leaves it added is H20's, closed by the settle dropping
/// the add action as it begins.
#[test]
fn a_create_and_a_settle_of_an_added_directory() {
    static RUNS: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
    for runs in &RUNS {
        runs.store(0, Ordering::Relaxed);
    }
    async fn added() -> Context {
        let c = served_tree(SETTLED.0, SETTLED.1).await;
        ops::mark_added(&c.tree, c.tree.id("a/d")).await;
        c
    }
    explore_from(
        "served: a create against a settle of an added directory",
        Box::new(|| Box::pin(added())),
        (vec![creating_done("a/d", "y"), settle("a/d")], &[1]),
        (true, |c, report| {
            let d = &report.nodes[&live_and_dirty(c, report, "a/d")];
            let counted = match c.settle.get() {
                None => 0,
                Some(ops::Settle::NotBegun) => panic!("the settle did not begin"),
                Some(ops::Settle::Cleared) => return,
                Some(ops::Settle::KeptUnequal) => 1,
                Some(ops::Settle::KeptAtFinish) => 2,
            };
            assert!(
                counted == 0 || !d.is_dirty_add(),
                "a settle that returned left 'a/d' added"
            );
            RUNS[counted].fetch_add(1, Ordering::Relaxed);
        }),
    );
    let runs = RUNS.each_ref().map(|runs| runs.load(Ordering::Relaxed));
    assert!(
        runs.iter().all(|&runs| runs > 0),
        "runs dropped midway, kept unequal and kept at the finish: {runs:?}"
    );
}

/// The served tree `a/f`, `x/`, with `a/f` copied up, as a commit's flip finds a file it
/// committed.
async fn copied_up() -> Context {
    let c = served_tree(&["a/f", "x/"], &[]).await;
    assert!(ops::served_copy_up(&c.tree, c.tree.id("a/f"), c.tree.id("a")).await);
    c
}

/// A commit's flip of file `path` to its committed content, recording whether it flipped.
fn flipping(path: &'static str) -> Op<Context> {
    Box::new(move |c: Arc<Context>| {
        Box::pin(async move {
            let flipped = ops::flip(&c.tree, c.tree.id(path)).await;
            c.done.store(flipped, Ordering::Relaxed);
        })
    })
}

/// A flip that answered done left its file at the committed content, clean, wherever it stands.
fn flipped_clean(c: &Context, report: &Report) {
    let f = c.tree.id("a/f");
    let node = report.nodes.get(&f).expect("'a/f' is in the tree");
    assert!(!node.is_dirty(), "the flipped file is clean");
    assert!(
        !node.ext_flags().contains(NodeExtFlags::LocalContent),
        "the flip's committed content stands"
    );
}

/// A commit's flip of a file in place, and a served rename of the file: the flip changes the file
/// only at the place its commit stored, before the rename; a rename landing first, or between its
/// read and its lock, has it refuse under its lock.
#[test]
fn a_flip_and_a_rename_of_its_file() {
    let name = "served: a flip against a rename of its file";
    explore_from(
        name,
        Box::new(|| Box::pin(copied_up())),
        (
            vec![flipping("a/f"), rename(Moving::File, "a/f", "x", "f")],
            &[],
        ),
        (false, flipped_clean),
    )
    .some_refused(name);
}

/// The served tree `a/f`, `x/h`, with `marked` dirty, and the files `made`, each a directory and a
/// name, created: files added, which a rename over a file the revision holds makes take that
/// file's path over (H20).
async fn created(
    marked: &'static [&'static str],
    made: &'static [(&'static str, &'static str)],
) -> Context {
    let c = served_tree(&["a/f", "x/h"], marked).await;
    let mut nodes = Vec::with_capacity(made.len());
    for (dir, name) in made {
        let node = ops::served_create(&c.tree, c.tree.id(dir), name).await;
        nodes.push(node.unwrap_or_else(|| panic!("'{dir}/{name}' is created")));
    }
    c.made.set(nodes).expect("the nodes made once");
    c
}

/// The file the setup made `index`th, from 0.
fn made(c: &Context, index: usize) -> NodeID {
    c.made.get().expect("the setup made nodes")[index]
}

/// The rename of the file created first over `x/h`, recording whether it moved.
fn replacing_created() -> Op<Context> {
    Box::new(|c: Arc<Context>| {
        Box::pin(async move {
            let (x, h) = (c.tree.id("x"), c.tree.id("x/h"));
            let moved = ops::served_rename_replacing(&c.tree, made(&c, 0), (x, "h"), h).await;
            c.done.store(moved, Ordering::Relaxed);
        })
    })
}

/// A created file left at `x/h`, below a dirty `x`.
fn created_at_holders_path(c: &Context, report: &Report) {
    live_and_dirty(c, report, "x");
    let (id, node) = report.at("x/h").expect("a node holds 'x/h'");
    assert_eq!(id, made(c, 0), "the created file holds 'x/h'");
    assert!(node.is_dirty(), "the created file is dirty");
}

/// H20 under concurrency: a created file renamed over a file the revision holds, and a served
/// removal of the holder: over the holder the file takes its path over, its action a modify; with
/// the holder gone first it stays added.
#[test]
fn a_created_files_replacing_rename_and_a_removal_of_the_holder() {
    static MODIFIED: AtomicUsize = AtomicUsize::new(0);
    static ADDED: AtomicUsize = AtomicUsize::new(0);
    let name = "served: a created file's replacing rename against a removal of the holder";
    explore_from(
        name,
        Box::new(|| Box::pin(created(&[], &[("a", "n")]))),
        (vec![replacing_created(), remove("x/h")], &[]),
        (true, |c, report| {
            created_at_holders_path(c, report);
            let (_, node) = report.at("x/h").expect("a node holds 'x/h'");
            let ended = if node.is_dirty_add() {
                &ADDED
            } else {
                assert!(node.is_dirty_modify(), "over the holder, a modify");
                &MODIFIED
            };
            ended.fetch_add(1, Ordering::Relaxed);
        }),
    );
    assert!(
        MODIFIED.load(Ordering::Relaxed) > 0 && ADDED.load(Ordering::Relaxed) > 0,
        "'{name}': a run over the holder and a run after its removal"
    );
}

/// H20 beside a settle of the directory the created file takes the path in: the settle keeps the
/// directory dirty wherever the rename lands in its phases.
#[test]
fn a_created_files_replacing_rename_and_a_settle_of_its_directory() {
    let name = "served: a created file's replacing rename against a settle of its directory";
    explore_from(
        name,
        Box::new(|| Box::pin(created(&["a", "x"], &[("a", "n")]))),
        (vec![replacing_created(), settle("x")], &[]),
        (true, created_at_holders_path),
    );
}

/// A created file renamed over another created file, beside a settle of the directory it joins:
/// the holder carries an add, so the file takes no path the revision holds and stays added.
#[test]
fn a_created_files_replacing_rename_over_a_created_file_and_a_settle() {
    let name = "served: a created file's replacing rename over a created file against a settle";
    explore_from(
        name,
        Box::new(|| Box::pin(created(&["a", "x"], &[("a", "n"), ("x", "m")]))),
        (
            vec![
                Box::new(|c: Arc<Context>| {
                    Box::pin(async move {
                        let (n, m) = (made(&c, 0), made(&c, 1));
                        let moved =
                            ops::served_rename_replacing(&c.tree, n, (c.tree.id("x"), "m"), m)
                                .await;
                        c.done.store(moved, Ordering::Relaxed);
                    })
                }),
                settle("x"),
            ],
            &[],
        ),
        (true, |c, report| {
            live_and_dirty(c, report, "x");
            let (id, node) = report.at("x/m").expect("a node holds 'x/m'");
            assert_eq!(id, made(c, 0), "the renamed file holds 'x/m'");
            assert!(node.is_dirty_add(), "the renamed file stays added");
        }),
    );
}

/// A settle of an empty directory, and an RMDIR of it: an RMDIR landing between the settle's
/// phases leaves the directory's pending clear and count as they were, so the settle's last phase
/// finds it gone by its place alone, and keeps it, as a directory gone, not overtaken.
#[test]
fn a_settle_and_an_rmdir_of_its_directory() {
    static MET: Rmdirs = Rmdirs::new();
    static KEPT: AtomicUsize = AtomicUsize::new(0);
    let name = "served: a settle against an RMDIR of its directory";
    explore(
        name,
        (&["a/d/"], &["a", "a/d"]),
        vec![rmdir("a/d", &MET), settle("a/d")],
        |c, _| {
            // An empty directory matches its committed one: kept unequal, it was gone.
            if c.settle.get() == Some(&ops::Settle::KeptUnequal) {
                KEPT.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    assert!(
        KEPT.load(Ordering::Relaxed) > 0,
        "'{name}': no run's RMDIR landed between the settle's phases"
    );
}

/// A swap's clean detach of a directory, and a served removal of its parent: the detach finds its
/// parent gone under its locks and detaches nothing, as a removed subtree keeps what stood in it,
/// or detaches before the removal.
#[test]
fn a_swaps_clean_detach_and_a_removal_of_its_parent() {
    let name = "served: a swap's clean detach against a removal of its parent";
    explore(
        name,
        (&["p/q/"], &[]),
        vec![
            Box::new(|c: Arc<Context>| {
                Box::pin(async move {
                    let detached = ops::swap_detach_if_clean(&c.tree, c.tree.id("p/q")).await;
                    c.done.store(detached, Ordering::Relaxed);
                })
            }),
            remove("p"),
        ],
        |_, _| {},
    )
    .some_refused(name);
}
