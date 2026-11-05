// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The reads through a link explored beside a repin of it to another revision of the repository it
//! pins: each listing of the link, which walks the State the pin names where it follows links, its
//! children those of the pinned node in either revision, beside a whole repin and beside repins of
//! the revision alone, which keep the node exposed, there another directory or a file, and answers
//! none where it follows none; two listings, by id and by name hash, on either side of a repin the
//! reading task makes between them, each loading its own revision; and a listing of a file, which
//! answers none. No listing of a link or a file reads a head of its node. Resolutions of a path
//! through the link beside repins, whole and of the revision alone, and beside a rename on the
//! path, and ones missing in the tree's own State or crossing no link, each hop in the tree's State
//! judged as a find, the rest of the path by the revision a pin of the link held from its last hop.
//! Collections of the dirty paths through the link, dirty and pinning a revision holding markers,
//! beside repins of it, a clear of its dirty flag and a rename of it, each reporting the paths the
//! revision it read the pin of holds below the node the pin exposes. Each run replays through the
//! model, which holds a walk through a link to the revision of a pin the link held at some `seq` of
//! its interval.

use lore_revision::node::ROOT_NODE;
use lore_revision::state::LinkPin;
use lore_revision::state::node::hooks::log::Effect;

use super::OpFn;
use super::Run;
use super::collected::collect_d;
use super::collected::collected;
use super::collected::collected_beside;
use super::collected::collected_by;
use super::explored_within;
use super::renamed_within;
use super::tree;
use crate::harness::catalogue;
use crate::harness::catalogue::Kind;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Over;
use crate::harness::catalogue::ops;
use crate::harness::fixtures::Linked;
use crate::harness::fixtures::Pinned;
use crate::harness::fixtures::Tree;
use crate::harness::scheduler::Setup;

/// A tree holding `d/f` and the link `d/l`.
fn linking() -> Setup<Tree> {
    Box::new(|| Box::pin(Tree::linking(&["d/f"], "d/l")))
}

/// [`linking`], `d/l` repinned to `src` in the second revision.
fn linking_second() -> Setup<Tree> {
    Box::new(|| {
        Box::pin(async {
            let tree = Tree::linking(&["d/f"], "d/l").await;
            let pin = tree
                .linked
                .as_ref()
                .expect("a linking tree")
                .pin(Pinned::Second);
            (tree.state)
                .node_set_link_target(&tree.repository, tree.id("d/l"), pin)
                .await
                .expect("the link repins");
            tree
        })
    })
}

/// A repin of `d/l` to what `pin` makes of its tree's linked revisions.
async fn repinned(tree: &Tree, pin: fn(&Linked) -> LinkPin) {
    let pin = pin(tree.linked.as_ref().expect("a linking tree"));
    ops::repin(tree, tree.id("d/l"), pin).await;
}

/// A repin of `d/l` to `src` in the first revision.
fn repin_first() -> OpFn {
    |tree| Box::pin(async move { repinned(&tree, |linked| linked.pin(Pinned::First)).await })
}

/// A repin of `d/l` to `src` in the second revision.
fn repin_second() -> OpFn {
    |tree| Box::pin(async move { repinned(&tree, |linked| linked.pin(Pinned::Second)).await })
}

/// A repin of `d/l` to the second revision alone, keeping the node it exposes: there, `old`.
fn repin_revision_second() -> OpFn {
    |tree| {
        Box::pin(async move {
            let revision = |linked: &Linked| LinkPin::Revision(linked.second.signature);
            repinned(&tree, revision).await;
        })
    }
}

/// A repin of `d/l` to the first revision alone, keeping the node it exposes: in the first, the
/// file `b`, where the link pinned `src` in the second.
fn repin_revision_first() -> OpFn {
    |tree| {
        Box::pin(async move {
            let revision = |linked: &Linked| LinkPin::Revision(linked.first.signature);
            repinned(&tree, revision).await;
        })
    }
}

/// What `call`'s walk walked and how many nodes it yielded, and whether the run's first op read a
/// head of any node.
fn listed(run: &Run<'_>, call: &Call) -> (Option<(Over, usize)>, bool) {
    let walked = (call.walks.first()).map(|walk| (walk.over, walk.yielded.len()));
    let heads = (run.of_op(0).iter()).any(|record| matches!(record.effect, Effect::Head { .. }));
    (walked, heads)
}

/// Each listing of `d/l`, with its kind and whether it follows links.
fn listings_of_the_link() -> [(OpFn, &'static Kind, bool); 6] {
    [
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::listed_children(&tree, tree.id("d/l")).await;
                })
            },
            &catalogue::LISTED_CHILDREN,
            true,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::listed_name_hashes(&tree, tree.id("d/l")).await;
                })
            },
            &catalogue::LISTED_NAME_HASHES,
            true,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::listed_marked(&tree, tree.id("d/l")).await;
                })
            },
            &catalogue::LISTED_MARKED,
            true,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::collected_children(&tree, tree.id("d/l")).await;
                })
            },
            &catalogue::COLLECTED_CHILDREN,
            true,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::collected_versioned_children(&tree, tree.id("d/l")).await;
                })
            },
            &catalogue::COLLECTED_VERSIONED_CHILDREN,
            true,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::collected_named_children(&tree, tree.id("d/l")).await;
                })
            },
            &catalogue::COLLECTED_NAMED_CHILDREN,
            false,
        ),
    ]
}

/// Each listing of `d/l` on the tree `setup` builds beside `repin`: one following links, `before`
/// children before the repin and `after` after; one following none, nothing. None reads a head.
fn listings_beside(setup: fn() -> Setup<Tree>, repin: OpFn, (before, after): (usize, usize)) {
    for (listing, kind, follows) in listings_of_the_link() {
        let runs = explored_within(None, setup(), (listing, kind), vec![repin], listed);
        let name = kind.name;
        assert!(
            runs.iter().all(|(_, heads)| !heads),
            "a listing of '{name}' read a head: {runs:?}"
        );
        if follows {
            assert!(
                runs.contains(&(Some((Over::Link, before)), false))
                    && runs.contains(&(Some((Over::Link, after)), false)),
                "no listing of '{name}' of the pin before the repin, or none after: {runs:?}"
            );
        } else {
            assert!(
                runs.iter()
                    .all(|run| *run == (Some((Over::Leaf, 0)), false)),
                "a listing of '{name}' followed the link: {runs:?}"
            );
        }
    }
}

/// Each listing of `d/l` beside a whole repin to the second revision: the first revision's `src`
/// children, `a` and `b`, before it, the second's, `c`, `d` and `e`, after.
#[test]
fn each_listing_of_a_link_lists_the_pinned_directory_of_either_pin_or_nothing() {
    listings_beside(linking, repin_second(), (2, 3));
}

/// Each listing of `d/l` beside a repin to the second revision alone: the first revision's `src`
/// children before it, and after it those of the node the link kept exposing, in the second
/// revision `old`, holding `a`.
#[test]
fn each_listing_of_a_link_beside_a_repin_of_its_revision_alone_lists_the_node_kept() {
    listings_beside(linking, repin_revision_second(), (2, 1));
}

/// Each listing of `d/l`, pinned whole to the second revision, beside a repin to the first
/// revision alone: the second revision's `src` children before it, and after it none, as the
/// node the link kept exposing is in the first revision the file `b`.
#[test]
fn each_listing_of_a_link_beside_a_repin_exposing_a_file_lists_none_after() {
    listings_beside(linking_second, repin_revision_first(), (3, 0));
}

/// A listing of `d/l` by id, a repin of it to the second revision, then a listing by name hash, by
/// one task, which meets no scheduler point between its listings: two loads in one run, one of
/// each revision, each listing judged against its own.
#[test]
fn listings_of_a_link_on_either_side_of_a_repin_load_each_revision() {
    let runs = explored_within(
        None,
        linking(),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::listed_children(&tree, tree.id("d/l")).await;
                    let pin = tree
                        .linked
                        .as_ref()
                        .expect("a linking tree")
                        .pin(Pinned::Second);
                    ops::repin(&tree, tree.id("d/l"), pin).await;
                    let _ = ops::listed_name_hashes(&tree, tree.id("d/l")).await;
                })
            },
            &catalogue::LISTED_NAME_HASHES,
        ),
        vec![],
        |run, by_name| {
            let by_id = (run.calls.iter())
                .find(|call| std::ptr::eq(call.kind, &catalogue::LISTED_CHILDREN))
                .expect("the listing by id");
            (listed(run, by_id).0, listed(run, by_name).0)
        },
    );
    let (first, second) = (Some((Over::Link, 2)), Some((Over::Link, 3)));
    assert!(
        runs.iter().all(|run| *run == (first, second)),
        "a run listed other than the first pin, then the second: {runs:?}"
    );
}

/// A listing of the file `d/f`: no children, and no head read, as a node that takes none.
#[test]
fn a_listing_of_a_file_lists_none() {
    let runs = explored_within(
        None,
        tree(&["d/f"]),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::listed_children(&tree, tree.id("d/f")).await;
                })
            },
            &catalogue::LISTED_CHILDREN,
        ),
        vec![],
        listed,
    );
    assert!(
        runs.iter()
            .all(|run| *run == (Some((Over::Leaf, 0)), false)),
        "{runs:?}"
    );
}

/// An op resolving the path given below the root of its tree.
macro_rules! resolving {
    ($path:literal) => {
        |tree| {
            Box::pin(async move {
                let _ = ops::find_linked(&tree, ROOT_NODE, $path).await;
            })
        }
    };
}

/// What a resolution through the link answered in `run`: the revision of a pin it answered a node
/// from, or none where it answered nothing. Any other answer fails the test.
fn answered_through(run: &Run<'_>, call: &Call) -> Option<Pinned> {
    let linked = run.tree.linked.as_ref().expect("a linking tree");
    match call.outcome {
        Outcome::Answer(Answer::Linked { revision, .. }) => {
            let mut pinned = [Pinned::First, Pinned::Second].into_iter();
            let from = pinned.find(|&p| linked.revision(p).signature == revision);
            Some(from.expect("a revision of the tree's links"))
        }
        Outcome::Answer(Answer::Found(None)) => None,
        ref other => panic!("a resolution through the link answered {other:?}"),
    }
}

/// Explores each resolution of `resolutions` on the tree `setup` builds beside `change`: across
/// its runs, it answers each of the two answers given and no other, the first what the tree as
/// built gives, the second what it gives once changed.
fn resolutions_beside(
    setup: fn() -> Setup<Tree>,
    change: OpFn,
    resolutions: &[(OpFn, [Option<Pinned>; 2])],
) {
    for &(resolution, [before, after]) in resolutions {
        let runs = explored_within(
            None,
            setup(),
            (resolution, &catalogue::FIND_LINKED),
            vec![change],
            answered_through,
        );
        assert!(
            runs.iter().all(|run| [before, after].contains(run))
                && runs.contains(&before)
                && runs.contains(&after),
            "not the answers {before:?} before the change and {after:?} after: {runs:?}"
        );
    }
}

/// A resolution of `d/l/a` beside a whole repin of `d/l` to the second revision: `a` in the first
/// revision's `src` before it, nothing after, as the second's `src` holds no `a`; of `d/l/c`, the
/// reverse; and of `d/l/a/x`, nothing either side, `a` a file in the first. Each hop in the tree's
/// State is a find, judged as one.
#[test]
fn a_resolution_through_a_link_answers_from_either_pin() {
    resolutions_beside(
        linking,
        repin_second(),
        &[
            (resolving!("d/l/a"), [Some(Pinned::First), None]),
            (resolving!("d/l/c"), [None, Some(Pinned::Second)]),
            (resolving!("d/l/a/x"), [None, None]),
        ],
    );
}

/// A resolution through `d/l` beside a repin of its revision alone: of `d/l/a`, `a` in the first
/// revision's `src` before it and in the second's `old`, the node the link kept exposing, after; of
/// `d/l/b`, nothing after, as `old` holds no `b`. From the second revision's `src`, beside a repin
/// to the first revision alone, which keeps exposing a node that is there the file `b`: `c` before
/// it, nothing after.
#[test]
fn a_resolution_through_a_link_beside_a_repin_of_its_revision_alone_walks_the_node_kept() {
    resolutions_beside(
        linking,
        repin_revision_second(),
        &[
            (
                resolving!("d/l/a"),
                [Some(Pinned::First), Some(Pinned::Second)],
            ),
            (resolving!("d/l/b"), [Some(Pinned::First), None]),
        ],
    );
    resolutions_beside(
        linking_second,
        repin_revision_first(),
        &[(resolving!("d/l/c"), [Some(Pinned::Second), None])],
    );
}

/// A resolution of `d/l/a` beside a dirty move renaming `d/l` to `m` within `d`: through the link
/// before it, and nothing after, its hop for `l` missing in `d`, each hop held from the hop before
/// it. `lore dirty move` refuses a link, and no caller renames one in place: it covers a link's
/// readers beside its rename.
#[test]
fn a_resolution_beside_a_rename_on_its_path_answers_either_side() {
    resolutions_beside(
        linking,
        |tree| Box::pin(async move { renamed_within(&tree, "d/l", "m").await }),
        &[(resolving!("d/l/a"), [Some(Pinned::First), None])],
    );
}

/// A resolution meeting a missing name, or a file where a directory would stand, in the tree's
/// State answers nothing, crossing no link: explored alone, as no repin of the link meets its
/// path.
#[test]
fn a_resolution_missing_in_the_trees_state_answers_nothing() {
    let resolutions: [OpFn; 2] = [resolving!("d/x/a"), resolving!("d/f/x")];
    for resolution in resolutions {
        let runs = explored_within(
            None,
            linking(),
            (resolution, &catalogue::FIND_LINKED),
            vec![],
            answered_through,
        );
        assert!(runs.iter().all(Option::is_none), "{runs:?}");
    }
}

/// A resolution crossing no link answers its node in the tree's State: `d/f`, and the link `d/l`
/// itself at its path's end, whatever it pins, so explored alone.
#[test]
fn a_resolution_crossing_no_link_answers_its_node() {
    let resolutions: [(OpFn, &str); 2] = [(resolving!("d/f"), "d/f"), (resolving!("d/l"), "d/l")];
    for (resolution, path) in resolutions {
        let runs = explored_within(
            None,
            linking(),
            (resolution, &catalogue::FIND_LINKED),
            vec![],
            move |run, call| (call.outcome.clone(), run.tree.id(path)),
        );
        assert!(
            (runs.iter())
                .all(|(outcome, node)| *outcome == Outcome::Answer(Answer::Found(Some(*node)))),
            "{runs:?}"
        );
    }
}

/// A tree holding `d/f` and the dirty link `d/l` pinning `src` in the revision `pinned` names
/// ([`Tree::dirty_link`]); where `spread`, `d/f` in the first block, filled, and `d/l` in the next,
/// at the head of `d/`'s chain, so a listing of `d/` moves to another block after it lists `d/l`
/// and before the collection reads `d/l`'s words.
fn linking_dirty(pinned: Pinned, spread: bool) -> Setup<Tree> {
    Box::new(move || {
        Box::pin(async move {
            let mut tree = if spread {
                Tree::spread(&["d/f"], "fill", &[]).await
            } else {
                Tree::new(&["d/f"]).await
            };
            tree.dirty_link("d/l", pinned).await;
            tree
        })
    })
}

/// A repin of `d/l` to `src` in the revision holding markers.
fn repin_dirty() -> OpFn {
    |tree| Box::pin(async move { repinned(&tree, |linked| linked.pin(Pinned::Dirty)).await })
}

/// A clear of `d/l`'s dirty flag, as the settle clears a link whose linked State holds no marker,
/// by the clear a scan makes of a file it found unchanged.
fn clear_link() -> OpFn {
    |tree| Box::pin(async move { ops::scan_clear(&tree, tree.id("d/l")).await })
}

/// A collection of the dirty paths below the State's root.
fn collect_root() -> OpFn {
    |tree| {
        Box::pin(async move {
            let _ = ops::dirty_paths(&tree, ROOT_NODE, "").await;
        })
    }
}

/// The paths a collection below `d/` reports through `d/l` pinning the revision holding markers.
const THROUGH: [&str; 3] = ["l/a", "l/n/m", "l/o"];

/// A collection through the dirty link `d/l` reports the paths its pinned revision holds below the
/// node the pin exposes: beside a whole repin to the first revision, which holds no marker, those
/// of the revision holding markers before it and none after; beside a repin to that revision, the
/// reverse; from the State's root, the same below `d/l`.
#[test]
fn a_collection_through_a_dirty_link_beside_a_repin_reports_either_pins_paths() {
    let beside =
        |pinned, change| collected_beside(None, linking_dirty(pinned, false), vec![change]);
    let runs = beside(Pinned::Dirty, repin_first());
    assert!(collected(&runs, &[&THROUGH, &[]]), "{runs:?}");
    let runs = beside(Pinned::First, repin_dirty());
    assert!(collected(&runs, &[&[], &THROUGH]), "{runs:?}");
    let from_root = THROUGH.map(|path| format!("d/{path}"));
    let from_root = from_root.each_ref().map(String::as_str);
    let runs = collected_by(
        None,
        linking_dirty(Pinned::Dirty, false),
        collect_root(),
        vec![repin_first()],
    );
    assert!(collected(&runs, &[&from_root, &[]]), "{runs:?}");
}

/// A collection through the dirty link `d/l` beside a clear of its dirty flag reports the paths of
/// the revision holding markers before it and none after, the link clean; beside a dirty move
/// renaming it to `m` within `d`, those paths below either name. `lore dirty move` refuses a link,
/// and no caller renames one in place: it covers a link's readers beside its rename.
#[test]
fn a_collection_through_a_dirty_link_beside_its_clear_or_rename_reports_either_side() {
    let runs = collected_beside(
        None,
        linking_dirty(Pinned::Dirty, false),
        vec![clear_link()],
    );
    assert!(collected(&runs, &[&THROUGH, &[]]), "{runs:?}");
    let runs = collected_beside(
        None,
        linking_dirty(Pinned::Dirty, false),
        vec![|tree| Box::pin(async move { renamed_within(&tree, "d/l", "m").await })],
    );
    let renamed = THROUGH.map(|path| path.replacen('l', "m", 1));
    let renamed = renamed.each_ref().map(String::as_str);
    assert!(collected(&runs, &[&THROUGH, &renamed]), "{runs:?}");
}

/// Whether another task wrote `d/l`'s words in `run` between `call`'s head read of `d/` and its
/// end.
fn link_written_within(run: &Run<'_>, call: &Call) -> bool {
    let (dir, link) = (run.tree.id("d"), run.tree.id("d/l"));
    let listed = (call.first_head(dir, &run.records, &run.spawns))
        .expect("the call read its directory's head");
    (run.records.iter()).any(|record| {
        record.state == call.state
            && record.task != call.task
            && matches!(record.effect, Effect::Word { node, .. } if node == link)
            && listed < record.seq
            && record.seq < call.end
    })
}

/// A collection of `d/` whose listing moves to another block after it lists `d/l`: a repin or a
/// clear of the link landing between `d/`'s head read and the collection's read of `d/l`'s words,
/// which it follows as it reads them, the link's write within the collection and no path through
/// it reported.
#[test]
fn a_collection_through_a_dirty_link_reads_its_pin_after_its_directorys_head() {
    for change in [repin_first(), clear_link()] {
        let runs = explored_within(
            None,
            linking_dirty(Pinned::Dirty, true),
            (collect_d(), &catalogue::DIRTY_PATHS),
            vec![change],
            |run, call| (call.paths.clone(), link_written_within(run, call)),
        );
        assert!(collected(&runs, &[&THROUGH, &[]]), "{runs:?}");
        assert!(
            runs.iter()
                .any(|(paths, within)| *within && paths.is_empty()),
            "no change of the link landed between the head read and the link's: {runs:?}"
        );
    }
}
