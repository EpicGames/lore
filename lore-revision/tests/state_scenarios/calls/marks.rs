// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A task's marked listing of a directory and its reads by the listing's `ChainMark`, as the stage
//! makes them, explored beside the changes that decide what a search since the mark answers: a
//! create of the name searched for, before the search or within the listing; a move out of the
//! directory, of a child behind the mark's head or of the head itself, or within a search; a
//! create and then a reposition of the listing's head (H9); a rename in place changing a name's
//! hash; a trusted move out at a listing's block crossing, and into another directory, read there;
//! and the task's own adds, counted in the mark. Each run replays through the model, which holds
//! each answer to the directory as the mark and the read's interval found it. The trees of one
//! block build `d/g` after `d/f`, so `g` heads `d`'s chain, as the scenarios moving a child behind
//! the head assert; the trees whose chain crosses blocks are headed by `d/y`.

use lore_revision::node::NodeID;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_storage::hash::hash_string;

use super::OpFn;
use super::Run;
use super::across;
use super::changes_chain_of;
use super::dirty_moved;
use super::explored_within;
use super::path_of;
use super::renamed_within;
use super::served;
use super::served_create_n;
use super::tree;
use crate::harness::catalogue;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Tree;
use crate::harness::scheduler::Setup;

/// What a task's reads by its marked listing of its directory made in a run.
struct MarkRun {
    /// The paths its listing listed, in the order it listed them.
    listed: Vec<String>,
    /// What each of its searches since the mark answered, in order.
    since: Vec<Answer>,
    /// The paths of the nodes its searches found, in order, `None` for those that found none.
    found: Vec<Option<String>>,
    /// What its read of whether the mark found its directory reattached answered.
    reattached: bool,
    /// Whether another task changed its directory's chain within the listing's call, and within its
    /// first search's.
    changed_in_listing: bool,
    changed_in_search: bool,
}

impl MarkRun {
    /// Whether its listing listed the node at `path`.
    fn listed(&self, path: &str) -> bool {
        self.listed.iter().any(|listed| listed == path)
    }

    /// Whether its listing listed the node at `path` first, as its chain's head.
    fn headed_by(&self, path: &str) -> bool {
        self.listed.first().is_some_and(|first| first == path)
    }

    /// Whether its search by `index` answered stale.
    fn stale(&self, index: usize) -> bool {
        self.since[index] == Answer::Stale
    }

    /// Whether its search by `index` found the node at `path`.
    fn found(&self, index: usize, path: &str) -> bool {
        self.found[index].as_deref() == Some(path)
    }

    /// Whether its search by `index` answered none.
    fn none(&self, index: usize) -> bool {
        self.since[index] == Answer::Since(None)
    }
}

/// A task's marked listing of `d/`, a served create in `d` of each of `adds`, each that attached
/// counted in the mark as the stage counts its own adds, its searches since the mark for each of
/// `names`, then its read of whether the mark found `d/` reattached. A scheduling point follows
/// the listing, as the stage reads the file system's directory there, and precedes the reattach
/// read, where another thread may land after the search, as a stale search's find through the
/// chain awaits there in the stage's claim. A listing or a search has a point of its own only where
/// its walk crosses blocks.
async fn marked_reads(tree: &Tree, adds: &[&str], names: &[&str]) {
    marked_reads_in(tree, "d", adds, names).await;
}

/// [`marked_reads`] of the directory at `dir`.
async fn marked_reads_in(tree: &Tree, dir: &str, adds: &[&str], names: &[&str]) {
    let d = tree.id(dir);
    let (_, mut marked) = ops::listed_marked(tree, d)
        .await
        .unwrap_or_else(|| panic!("'{dir}' lists with a mark"));
    hooks::reached(Point::Named("listed")).await;
    for add in adds {
        if ops::served_create(tree, d, add).await.is_some() {
            marked.count_own_prepend();
        }
    }
    for name in names {
        let _ = ops::added_since(tree, &marked, name).await;
    }
    hooks::reached(Point::Named("searched")).await;
    let _ = ops::reattached_since(tree, &marked).await;
}

/// Explores `reads`, a task's reads by its marked listing of a directory ([`marked_reads_in`]),
/// beside `changes` on the tree `setup` builds, within `preemptions` per run where given. What the
/// reads made in each run.
fn marks_beside(
    preemptions: Option<usize>,
    setup: Setup<Tree>,
    reads: OpFn,
    changes: Vec<OpFn>,
) -> Vec<MarkRun> {
    explored_within(
        preemptions,
        setup,
        (reads, &catalogue::LISTED_MARKED),
        changes,
        mark_run,
    )
}

/// The names the scenarios create nodes under.
const CREATED: [&str; 2] = ["n", "o"];

/// The path of `node` in `run`: where the tree built it, or, for a node a call of the run attached
/// under one of [`CREATED`], its directory's path and that name.
fn path_in(run: &Run<'_>, node: NodeID) -> String {
    let attached =
        (run.calls.iter()).find(|call| call.outcome == Outcome::Done(Answer::Node(node)));
    let named = attached.and_then(|call| {
        let name = CREATED
            .into_iter()
            .find(|name| call.name == Some(hash_string(name)))?;
        Some(format!("{}/{name}", path_of(run.tree, call.target?)))
    });
    named.unwrap_or_else(|| path_of(run.tree, node))
}

/// Whether a task other than `call`'s changed directory `dir`'s chain within `call`'s interval
/// ([`changes_chain_of`]).
fn changed_within(run: &Run<'_>, call: &Call, dir: NodeID) -> bool {
    run.records.iter().any(|record| {
        record.task != call.task
            && (call.begin..call.end).contains(&record.seq)
            && changes_chain_of(&record.effect, dir)
    })
}

/// What the task of `listing`, a marked listing, made by its mark in `run`.
fn mark_run(run: &Run<'_>, listing: &Call) -> MarkRun {
    let dir = listing.target.expect("a listing of a directory");
    let listed = (listing.walks.iter())
        .flat_map(|walk| walk.yielded.iter())
        .map(|yielded| path_in(run, yielded.node))
        .collect();
    let own = |kind| {
        (run.calls.iter())
            .filter(move |call: &&Call| call.task == listing.task && std::ptr::eq(call.kind, kind))
    };
    let answer = |call: &Call| match &call.outcome {
        Outcome::Answer(answer) => *answer,
        outcome => panic!("a call of '{}' answered {outcome:?}", call.kind.name),
    };
    let since: Vec<Answer> = own(&catalogue::ADDED_SINCE).map(answer).collect();
    let found = (since.iter())
        .map(|answer| match answer {
            Answer::Since(Some(node)) => Some(path_in(run, *node)),
            _ => None,
        })
        .collect();
    let reattached = match own(&catalogue::REATTACHED_SINCE).map(answer).next() {
        Some(Answer::Reattached(reattached)) => reattached,
        answered => panic!("the mark's reattach read answered {answered:?}"),
    };
    let first_search = own(&catalogue::ADDED_SINCE).next();
    MarkRun {
        listed,
        since,
        found,
        reattached,
        changed_in_listing: changed_within(run, listing, dir),
        changed_in_search: first_search.is_some_and(|search| changed_within(run, search, dir)),
    }
}

/// A served move of `d/<name>` into `e`, out of `d`.
async fn moved_out(tree: &Tree, name: &str) {
    let (node, e) = (tree.id(&format!("d/{name}")), tree.id("e"));
    assert!(ops::served_rename(tree, node, e, name).await);
}

/// A served move of `d/f` into `e`, out of `d`.
fn move_f_out() -> OpFn {
    |tree| Box::pin(async move { moved_out(&tree, "f").await })
}

/// Searches since a mark of `d/` for `n` and for `f`, the listed child behind the mark's head,
/// beside a served create of `d/n`: `n` found where it was created after the listing's head read
/// and before the search; none in every run whose listing listed it, as it heads the chain the
/// search stops at, and where it came after the search; `f` none in every run, as the search walks
/// no further than the mark's head; and the mark never found reattached, as a create is no
/// reattach.
#[test]
fn a_search_since_a_mark_beside_a_create_of_its_name_finds_what_joined_since() {
    let runs = marks_beside(
        None,
        served(&["d/f", "d/g"]),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["n", "f"]).await }),
        vec![served_create_n()],
    );
    assert!(
        runs.iter().all(|run| run.none(1) && !run.reattached),
        "a search found the listed 'f', or a create showed as a reattach"
    );
    assert!(
        runs.iter().all(|run| !run.listed("d/n") || run.none(0)),
        "a search found the 'n' its listing listed"
    );
    assert!(
        runs.iter()
            .any(|run| run.found(0, "d/n") && !run.listed("d/n")),
        "no search found 'n' created since the mark"
    );
    assert!(
        runs.iter().any(|run| run.none(0) && run.listed("d/n")),
        "no search missed the 'n' its listing listed"
    );
    assert!(
        runs.iter().any(|run| run.none(0) && !run.listed("d/n")),
        "no search came before the create"
    );
}

/// A search since a mark of a `d/` whose chain crosses blocks, for `n`, beside a served create of
/// `d/n` that lands while the listing walks, at the crossing, past the chain's head: the listing
/// leaves `n` out, as it was prepended behind the walk, and the search since the mark finds it,
/// the mark's head read as the listing began.
#[test]
fn a_search_since_a_mark_finds_what_its_listing_walked_past() {
    let runs = marks_beside(
        None,
        across(true),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["n"]).await }),
        vec![served_create_n()],
    );
    assert!(
        runs.iter()
            .any(|run| run.changed_in_listing && !run.listed("d/n") && run.found(0, "d/n")),
        "no search found the 'n' created while its listing walked"
    );
}

/// A search since a mark of `d/` for `n`, which no child carries, then the mark's reattach read,
/// beside a served move of `d/f`, behind the mark's head, out of `d`: stale where the move landed
/// after the listing's head read and by the search's end, as the count moved by a reattach since
/// the mark; none where it came after; the mark found reattached in every run whose search was
/// stale, and in some whose search was not, the move landing between the two reads.
#[test]
fn a_search_since_a_mark_beside_a_move_out_of_its_directory_is_stale() {
    let runs = marks_beside(
        None,
        served(&["d/f", "d/g", "e/"]),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["n"]).await }),
        vec![move_f_out()],
    );
    assert!(
        runs.iter().all(|run| run.headed_by("d/g")),
        "a listing found 'g' heading no chain"
    );
    assert!(
        runs.iter().all(|run| !run.stale(0) || run.reattached),
        "a stale search's mark found not reattached after it"
    );
    assert!(
        runs.iter().any(|run| run.stale(0) && run.listed("d/f")),
        "no search found the move since the mark"
    );
    assert!(
        runs.iter()
            .any(|run| run.none(0) && run.reattached && run.listed("d/f")),
        "no move landed between the search and the reattach read"
    );
    assert!(
        runs.iter()
            .any(|run| run.none(0) && !run.reattached && run.listed("d/f")),
        "no run read the mark before the move"
    );
}

/// Searches since a mark of `d/` for `f` and then for `n`, beside a served move of `d/g`, the
/// mark's head, out of `d`: where the move landed between the listing and the searches, each
/// search misses the head and walks the whole chain, finding the listed `f`, and misses `n` with
/// the count moved by no reattach since its walk began, though moved since the mark, which the
/// mark's reattach read finds.
#[test]
fn a_search_since_a_mark_whose_head_left_walks_the_whole_chain() {
    let runs = marks_beside(
        None,
        served(&["d/f", "d/g", "e/"]),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["f", "n"]).await }),
        vec![|tree| Box::pin(async move { moved_out(&tree, "g").await })],
    );
    assert!(
        runs.iter().any(|run| {
            run.headed_by("d/g") && run.found(0, "d/f") && run.none(1) && run.reattached
        }),
        "no search walked the whole chain once the mark's head left"
    );
    assert!(
        runs.iter()
            .all(|run| !run.found(0, "d/f") || run.reattached),
        "a search found the listed 'f' with the mark's head in place"
    );
}

/// A task's marked listing of a `d/` whose chain crosses blocks, its own served move of `d/y`, the
/// mark's head, out of `d`, then its search since the mark for `n` and its reattach read, beside
/// another task's served move of `d/x` out of `d`: with the head gone the search walks the whole
/// chain, across the blocks, and where the move of `x` lands while it walks, the count it reads at
/// its walk's end has moved by a reattach since the walk began, so it answers stale, and only
/// there. Within thirteen preemptions a run, the move landing at the search's crossing in some.
#[test]
fn a_search_since_a_mark_beside_a_reattach_within_its_walk_is_stale() {
    let runs = marks_beside(
        Some(13),
        across(true),
        |tree| {
            Box::pin(async move {
                let (_, marked) = ops::listed_marked(&tree, tree.id("d"))
                    .await
                    .expect("'d' lists with a mark");
                moved_out(&tree, "y").await;
                let _ = ops::added_since(&tree, &marked, "n").await;
                let _ = ops::reattached_since(&tree, &marked).await;
            })
        },
        vec![|tree| Box::pin(async move { moved_out(&tree, "x").await })],
    );
    assert!(
        runs.iter()
            .all(|run| !run.stale(0) || run.changed_in_search),
        "a search walking the whole chain was stale with no reattach while it walked"
    );
    assert!(
        runs.iter()
            .any(|run| run.changed_in_search && run.stale(0) && run.listed("d/y")),
        "no search found a reattach landing while it walked"
    );
}

/// A task's marked listing of a `d/` whose chain crosses blocks, its own served creates of `d/p`,
/// which takes a free slot of the block `y`, the mark's head, stands in, and of `d/o`, from a fresh
/// block, each counted in the mark, then its search since the mark for `n`, beside another task's
/// served move of `d/g`, behind the head, out of `d`: the search walks from `o` across to `p`'s
/// block and stops at `y`, and where the move lands at that crossing, the count it reads at its
/// walk's end has moved by a reattach past the mark's, so it answers stale. Within four preemptions
/// a run.
#[test]
fn a_search_since_a_mark_stopping_at_its_head_reads_the_count_at_its_walks_end() {
    let runs = marks_beside(
        Some(4),
        across(true),
        |tree| {
            Box::pin(async move {
                let d = tree.id("d");
                let (_, mut marked) = ops::listed_marked(&tree, d)
                    .await
                    .expect("'d' lists with a mark");
                assert!(ops::served_create(&tree, d, "p").await.is_some());
                marked.count_own_prepend();
                (tree.state)
                    .take_new_nodes_from_fresh_blocks(tree.repository.clone())
                    .await
                    .expect("new nodes come from fresh blocks");
                assert!(ops::served_create(&tree, d, "o").await.is_some());
                marked.count_own_prepend();
                let _ = ops::added_since(&tree, &marked, "n").await;
                let _ = ops::reattached_since(&tree, &marked).await;
            })
        },
        vec![|tree| Box::pin(async move { moved_out(&tree, "g").await })],
    );
    assert!(
        runs.iter()
            .any(|run| run.changed_in_search && run.stale(0) && run.headed_by("d/y")),
        "no search stopping at the mark's head found a reattach landing while it walked"
    );
}

/// A search since a mark of `d/` for `n` beside a served create of `d/n` and then a reposition of
/// `d/g`, the listing's head, in `d` (H9): the reposition puts `g` back in front of `n`, so a walk
/// stopping at the mark's head misses `n`, and the search answers stale where both landed between
/// the listing's head read and the search's end, never none while `n` stands there, as the model
/// holds it. Every schedule: five preemptions a run reach them all.
#[test]
fn a_search_since_a_mark_beside_a_reposition_of_its_head_is_stale() {
    let runs = marks_beside(
        Some(5),
        served(&["d/f", "d/g"]),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["n"]).await }),
        vec![|tree| {
            Box::pin(async move {
                let d = tree.id("d");
                assert!(ops::served_create(&tree, d, "n").await.is_some());
                assert!(ops::served_rename(&tree, tree.id("d/g"), d, "G").await);
            })
        }],
    );
    assert!(
        runs.iter()
            .any(|run| run.stale(0) && !run.listed("d/n") && run.reattached),
        "no search met 'g' repositioned in front of the 'n' created since the mark"
    );
    assert!(
        runs.iter()
            .any(|run| run.found(0, "d/n") && !run.reattached),
        "no search found 'n' before the reposition"
    );
}

/// A dirty move of `d/g` into `e`, out of `d`, where `d/`'s chain crosses blocks in [`across`].
fn dirty_move_g_out() -> OpFn {
    |tree| Box::pin(async move { dirty_moved(&tree, "d/g", "e").await })
}

/// That `runs`, reads of a `d/` whose chain crosses blocks beside a change of `d/g` there, `what`,
/// each found the mark reattached where its search answered stale, that some search answered stale
/// and that in some the change landed while the listing walked.
fn stale_since_the_mark(runs: &[MarkRun], what: &str) {
    assert!(
        runs.iter().all(|run| !run.stale(0) || run.reattached),
        "beside {what}, a stale search's mark found not reattached after it"
    );
    assert!(
        runs.iter().any(|run| run.stale(0)),
        "no search found {what} since the mark"
    );
    assert!(
        runs.iter().any(|run| run.changed_in_listing),
        "no {what} landed while a listing walked"
    );
}

/// A search since a mark of a `d/` whose chain crosses blocks, for `n`, then the mark's reattach
/// read, beside a dirty move of `d/g`, where the chain crosses, into `e`: stale where the move
/// landed after the listing's head read and by the search's end, as the count moved by a reattach
/// since the mark, the mark found reattached then; in some the move landed while the listing
/// walked.
#[test]
fn a_search_since_a_mark_beside_a_dirty_move_out_at_its_crossing_is_stale() {
    let runs = marks_beside(
        None,
        across(false),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["n"]).await }),
        vec![dirty_move_g_out()],
    );
    stale_since_the_mark(&runs, "the move");
}

/// A search since a mark of `e/`, empty, for `g`, beside a dirty move of `d/g` into `e`: found
/// where the move landed after the listing's head read and before the search, listed where it came
/// before the listing, none where it came after the search; the mark never found reattached, as a
/// node moving in counts as an add in `e`'s chain.
#[test]
fn a_search_since_a_mark_beside_a_dirty_move_in_finds_what_moved_in_since() {
    let runs = marks_beside(
        None,
        tree(&["d/f", "d/g", "e/"]),
        |tree| Box::pin(async move { marked_reads_in(&tree, "e", &[], &["g"]).await }),
        vec![dirty_move_g_out()],
    );
    assert!(
        runs.iter().all(|run| !run.reattached),
        "a move in showed as a reattach"
    );
    assert!(
        runs.iter()
            .any(|run| run.found(0, "d/g") && !run.listed("d/g")),
        "no search found 'g' moved in since the mark"
    );
    assert!(
        runs.iter().any(|run| run.none(0) && run.listed("d/g")),
        "no listing listed 'g' moved in before it"
    );
    assert!(
        runs.iter().any(|run| run.none(0) && !run.listed("d/g")),
        "no search came before the move"
    );
}

/// A search since a mark of a `d/` whose chain crosses blocks, for `h`, beside a dirty move
/// renaming `d/g`, where the chain crosses, to `h` within `d`: the rename in place keeps `g`'s
/// place, behind the mark's head, so the walk to the head never meets it, and counts the changed
/// hash as a reattach: stale where the rename landed after the listing's head read and by the
/// search's end, the mark found reattached then; in some the rename landed while the listing
/// walked.
#[test]
fn a_search_since_a_mark_beside_a_rename_in_place_at_its_crossing_is_stale() {
    let runs = marks_beside(
        None,
        across(false),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["h"]).await }),
        vec![|tree| Box::pin(async move { renamed_within(&tree, "d/g", "h").await })],
    );
    stale_since_the_mark(&runs, "the rename");
}

/// A search since a mark of `d/` for `n` beside a dirty move renaming `d/f`, behind the mark's
/// head, to `n` within `d`: the rename in place keeps `f`'s place, so the walk to the mark's head
/// never meets it, and counts its changed hash as a reattach, so the search answers stale where
/// the rename landed after the listing's head read and by the search's end.
#[test]
fn a_search_since_a_mark_beside_a_rename_in_place_is_stale() {
    let runs = marks_beside(
        None,
        tree(&["d/f", "d/g"]),
        |tree| Box::pin(async move { marked_reads(&tree, &[], &["n"]).await }),
        vec![|tree| {
            Box::pin(async move {
                renamed_within(&tree, "d/f", "n").await;
            })
        }],
    );
    assert!(
        runs.iter().all(|run| !run.stale(0) || run.reattached),
        "a stale search's mark found not reattached after it"
    );
    assert!(
        runs.iter().any(|run| run.stale(0)),
        "no search found the rename since the mark"
    );
    assert!(
        runs.iter().all(|run| !run.found(0, "d/f")),
        "a search walked past the mark's head to the renamed 'f'"
    );
}

/// A task's own create of `d/o` after its marked listing, counted in the mark, then its searches
/// since the mark for `o` and for `n`, beside a served move of `d/f` out of `d`: `o` found in every
/// run, as it stands ahead of the mark's head; `n` none where the move came after the search, as
/// the counted add moves the count by no reattach; the mark found reattached exactly where the
/// count moved by a reattach past the mark's and the add's, as the model holds it. A count of the
/// add missed would show only past 2^16 of them; one counted twice wraps, which the model finds.
/// Within six preemptions a run.
#[test]
fn a_search_since_a_mark_after_its_own_add_finds_it() {
    let runs = marks_beside(
        Some(6),
        served(&["d/f", "d/g", "e/"]),
        |tree| Box::pin(async move { marked_reads(&tree, &["o"], &["o", "n"]).await }),
        vec![move_f_out()],
    );
    assert!(
        runs.iter().all(|run| run.headed_by("d/g")),
        "a listing found 'g' heading no chain"
    );
    assert!(
        runs.iter().all(|run| run.found(0, "d/o")),
        "a search missed the task's own add"
    );
    assert!(
        runs.iter()
            .any(|run| run.none(1) && !run.reattached && run.listed("d/f")),
        "no run read the mark before the move"
    );
    assert!(
        runs.iter().any(|run| run.stale(1) && run.reattached),
        "no search found the move since the mark"
    );
}
