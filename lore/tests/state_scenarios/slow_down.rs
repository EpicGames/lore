// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A `SlowDown` of the State reported through the C API's revision-tree verbs as `SLOW_DOWN`, which
//! a caller can retry, on the entry and on the call. Each scenario parks one call after its State
//! operation read the chain its change or its walk depends on, runs a call that changes that chain,
//! then lets it go on: under the State's retry bound lowered to one attempt or one restart it gives
//! up, and under the product's bound it tries again and succeeds. The delete of an added node and
//! the move each meet an add into the directory they detach from; the listing and the add's check
//! of its name meet two children moved out of the directory they walk.

use lore::revision_tree::handle::LoreRevisionTree;
use lore_revision::event::LoreErrorCode;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreNodeType;
use lore_revision::node::NodeID;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;

use crate::capi;
use crate::capi::Events;
use crate::capi::Handle;
use crate::capi::Reported;
use crate::harness::invariants::Rules;
use crate::park::Call;
use crate::park::Parks;
use crate::park::until_parked;

/// The status a call that gave up under concurrent changes completes with.
const SLOW_DOWN: i32 = LoreError::SlowDown as i32;

/// The codes a call reported on its entries and on its batch, and the status it completed with.
fn outcome(events: &Events) -> (Vec<LoreErrorCode>, Vec<LoreErrorCode>, i32) {
    let reported = events.all();
    let entries = reported
        .iter()
        .filter_map(|event| match event {
            Reported::Add(_, _, code)
            | Reported::Delete(_, _, code)
            | Reported::Move(_, _, code)
            | Reported::Modify(_, _, code) => Some(*code),
            _ => None,
        })
        .collect();
    let batches = reported
        .iter()
        .filter_map(|event| match event {
            Reported::Batch(_, code) => Some(*code),
            _ => None,
        })
        .collect();
    let status = reported
        .iter()
        .find_map(|event| match event {
            Reported::Complete(status, _) => Some(*status),
            _ => None,
        })
        .expect("the call completes");
    (entries, batches, status)
}

/// That a batch call of one entry, which returned `status` and reported `events`, gave up under
/// concurrent changes where `slowed`, reporting `SLOW_DOWN` on its entry, its batch and its
/// `Complete`, and succeeded otherwise.
fn assert_batch(status: i32, events: &Events, slowed: bool) {
    let (entries, batches, completed) = outcome(events);
    let (code, expected) = if slowed {
        (LoreErrorCode::SlowDown, SLOW_DOWN)
    } else {
        (LoreErrorCode::None, 0)
    };
    assert_eq!(entries, [code], "the entry: {:?}", events.all());
    assert_eq!(batches, [code], "the batch: {:?}", events.all());
    assert_eq!(
        (status, completed),
        (expected, expected),
        "the call: {:?}",
        events.all()
    );
}

/// A handle over added `paths`, directories ending in `/`, its State's retry bound at `bound`.
async fn seeded(
    paths: &[&'static str],
    bound: Option<u32>,
) -> (Handle, Vec<(&'static str, NodeID)>) {
    let handle = Handle::load().await;
    let nodes = handle.seed_paths(paths).await;
    handle.state().set_retry_bound_for_tests(bound);
    (handle, nodes)
}

/// The tree's invariants once every call is done.
async fn holds(handle: &Handle) {
    crate::holding(&handle.state(), &handle.repository(), Rules::CLOSED).await;
}

/// Runs `call` of `dir`'s children over `dir/a` to `dir/d`, its chain `d`, `c`, `b`, `a` as each add
/// prepends, beside `other`, its State's retry bound at `bound`: parked at its walk's second step,
/// about to read `c`, while `c` moves out to `other`, and at the second step after, once the walk
/// restarted past the move and skipped `d` again, about to read `b`, while `b` does. Reading each
/// moved node restarts the walk, the second time past a bound of one restart. Answers the call's
/// status and events.
async fn walked_beside_two_moves(
    bound: Option<u32>,
    call: impl FnOnce(LoreRevisionTree, NodeID, Events) -> Call<'static>,
) -> (i32, Events) {
    let paths = ["dir/", "dir/a", "dir/b", "dir/c", "dir/d", "other/"];
    let (handle, nodes) = seeded(&paths, bound).await;
    let id = |path| capi::id_of(&nodes, path);
    let parks = Parks::at(Point::IterStep);
    let _installed = hooks::install(parks.clone());
    let events = Events::default();
    parks.arm(2);
    let mut walking = call(handle.tree, id("dir"), events.clone());
    for moved in ["dir/c", "dir/b"] {
        until_parked(&mut walking, &parks).await;
        let moving = Events::default();
        let name = moved.rsplit_once('/').map_or(moved, |(_, name)| name);
        let status = capi::move_node(handle.tree, id(moved), id("other"), name, &moving).await;
        assert_eq!(status, 0, "moving '{moved}' succeeds: {:?}", moving.all());
        parks.arm(if moved == "dir/c" { 2 } else { 0 });
        parks.release();
    }
    let status = walking.await;
    holds(&handle).await;
    (status, events)
}

/// A listing whose walk meets two children moved out of its directory restarts past each; under a
/// bound of one restart it gives up at the second, after the header and the first child, and
/// completes with `SLOW_DOWN`, where under the product's bound it lists the children left.
#[tokio::test]
async fn a_listing_given_up_under_moves_completes_with_slow_down() {
    crate::in_context(async {
        for (bound, slowed) in [(None, false), (Some(1), true)] {
            let (status, events) = walked_beside_two_moves(bound, |tree, dir, events| {
                Box::pin(async move { capi::list_children(tree, dir, &events).await })
            })
            .await;
            let reported = events.all();
            let listed = reported
                .iter()
                .filter(|event| matches!(event, Reported::Child(..)))
                .count();
            let completed = reported.iter().find_map(|event| match event {
                Reported::Complete(status, _) => Some(*status),
                _ => None,
            });
            if slowed {
                assert_eq!(listed, 1, "the walk gave up after 'd': {reported:?}");
                assert_eq!(
                    (status, completed),
                    (SLOW_DOWN, Some(SLOW_DOWN)),
                    "{reported:?}"
                );
            } else {
                assert_eq!(listed, 2, "'d' and 'a' are listed: {reported:?}");
                assert_eq!((status, completed), (0, Some(0)), "{reported:?}");
            }
        }
    })
    .await;
}

/// An add whose check of its name walks a directory that two children move out of meanwhile
/// restarts past each; under a bound of one restart its check gives up, and the call reports
/// `SLOW_DOWN` on the entry and the batch with nothing added, where under the product's bound the
/// add lands.
#[tokio::test]
async fn an_add_whose_name_check_gives_up_under_moves_reports_slow_down() {
    crate::in_context(async {
        for (bound, slowed) in [(None, false), (Some(1), true)] {
            let (status, events) = walked_beside_two_moves(bound, |tree, dir, events| {
                Box::pin(
                    async move { capi::add(tree, dir, "x", LoreNodeType::File, &events).await },
                )
            })
            .await;
            assert_batch(status, &events, slowed);
            assert_eq!(capi::added(&events).is_none(), slowed);
        }
    })
    .await;
}

/// A delete of the added `d/a`, parked between finding the link that reaches `a` in `d`'s chain,
/// the chain's head, and locking it, while an add publishes `x` at the head: the detach finds the
/// link changed and, under a bound of one attempt, gives up, reporting `SLOW_DOWN` on the entry
/// and the batch with `a` in place, where under the product's bound it tries again and detaches
/// `a`.
#[tokio::test]
async fn a_delete_whose_detach_gives_up_under_an_add_reports_slow_down() {
    crate::in_context(async {
        for (bound, slowed) in [(None, false), (Some(1), true)] {
            let (handle, nodes) = seeded(&["d/", "d/a"], bound).await;
            let id = |path| capi::id_of(&nodes, path);
            let parks = Parks::at(Point::PredecessorStep);
            let _installed = hooks::install(parks.clone());
            let events = Events::default();
            let deleted = [id("d/a")];
            parks.arm(1);
            let mut deleting: Call<'_> = Box::pin(capi::delete(handle.tree, &deleted, &events));
            until_parked(&mut deleting, &parks).await;
            let adding = Events::default();
            let status = capi::add(handle.tree, id("d"), "x", LoreNodeType::File, &adding).await;
            assert_eq!(status, 0, "the add succeeds: {:?}", adding.all());
            parks.release();
            let status = deleting.await;
            assert_batch(status, &events, slowed);
            let a = handle
                .state()
                .node(handle.repository(), id("d/a"))
                .await
                .expect("'a' reads");
            assert_eq!(
                a.is_discarded(),
                !slowed,
                "'a' is detached only where the delete went on"
            );
            holds(&handle).await;
        }
    })
    .await;
}

/// A move of `p/x` to `q`, parked between finding the link that reaches `x` in `p`'s chain, the
/// chain's head, and locking it, while an add publishes `z` at the head: the move finds the link
/// changed and, under a bound of one attempt, gives up, reporting `SLOW_DOWN` on the entry and the
/// batch with `x` in `p`, where under the product's bound it tries again and moves `x`.
#[tokio::test]
async fn a_move_given_up_under_an_add_reports_slow_down() {
    crate::in_context(async {
        for (bound, slowed) in [(None, false), (Some(1), true)] {
            let (handle, nodes) = seeded(&["p/", "p/x", "q/"], bound).await;
            let id = |path| capi::id_of(&nodes, path);
            let parks = Parks::at(Point::BeforeMoveLock);
            let _installed = hooks::install(parks.clone());
            let events = Events::default();
            parks.arm(1);
            let mut moving: Call<'_> = Box::pin(capi::move_node(
                handle.tree,
                id("p/x"),
                id("q"),
                "x",
                &events,
            ));
            until_parked(&mut moving, &parks).await;
            let adding = Events::default();
            let status = capi::add(handle.tree, id("p"), "z", LoreNodeType::File, &adding).await;
            assert_eq!(status, 0, "the add succeeds: {:?}", adding.all());
            parks.release();
            let status = moving.await;
            assert_batch(status, &events, slowed);
            let x = handle
                .state()
                .node(handle.repository(), id("p/x"))
                .await
                .expect("'x' reads");
            let expected = if slowed { id("p") } else { id("q") };
            assert_eq!(x.parent, expected, "'x' moved only where the move went on");
            holds(&handle).await;
        }
    })
    .await;
}
