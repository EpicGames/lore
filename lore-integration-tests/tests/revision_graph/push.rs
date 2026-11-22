// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `branch_push` against an in-process Lore Server: publishing a local latest, a first push, a
//! remote latest that moved, fast-forward merges, force, the flags, nothing to publish, a killed
//! push's revision the remote moved past, a merge revision whose second parent the remote lacks,
//! and the
//! events a push sends.

use lore::revision_graph::branch_push::LoreRevisionGraphBranchPushOptions;
use lore::revision_graph::branch_sync::LoreRevisionGraphBranchSyncOptions;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchNotFound;
use lore_base::error::Divergent;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::interface::LoreGlobalArgs;
use lore_storage::test_util::HookedStore;

use super::support::*;
use crate::common::net_common::close_handle;
use crate::common::net_common::open_handle;

/// The options of a push with neither `force` nor a fast-forward merge.
fn plain() -> LoreRevisionGraphBranchPushOptions {
    LoreRevisionGraphBranchPushOptions::default()
}

/// The options of a push that lets the remote merge onto a remote latest that moved.
fn merging() -> LoreRevisionGraphBranchPushOptions {
    LoreRevisionGraphBranchPushOptions {
        fast_forward_merge: 1,
        ..Default::default()
    }
}

/// The status and events of a no-flag push of `branch` on `f`'s handle with `options`.
async fn push(
    f: &Fixture,
    branch: BranchId,
    options: LoreRevisionGraphBranchPushOptions,
) -> (i32, Vec<LoreEvent>) {
    push_branch_call(f, f.store, graph_globals(), branch, options).await
}

/// The number of the revision `revision` at the server.
async fn number_at_server(f: &Fixture, revision: Hash) -> u64 {
    on_server(f, async |context| {
        lore_revision::state::State::deserialize(context, revision)
            .await
            .expect("the server holds the revision")
            .revision_number()
    })
    .await
}

/// A push of a revision on top of the remote latest publishes it, and moves the local latest to it,
/// convergent. Its pushed event precedes `Complete`.
#[tokio::test]
async fn a_push_publishes_the_local_latest_and_the_event_precedes_complete() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let (status, events) = push(&f, f.default_branch, plain()).await;

    assert_eq!(status, 0);
    assert_eq!(server_latest(&f, f.default_branch).await, committed);
    assert_eq!(pushed(&events), [published(committed, 2)]);
    let position = |pick: fn(&LoreEvent) -> bool| events.iter().position(pick);
    assert!(
        position(|event| matches!(event, LoreEvent::RevisionGraphBranchPushed(_)))
            < position(|event| matches!(event, LoreEvent::Complete(_)))
    );
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    f.close().await;
}

/// A push of a merge revision the revision tree API committed, whose second parent is on a branch
/// the remote doesn't hold, publishes it and the line of its second parent, and leaves that branch
/// off the remote.
#[tokio::test]
async fn a_push_of_a_merge_publishes_its_second_parents_line() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_local_branch(&f, "feature", f.default_branch, CREATOR).await;
    let local_only = handle_over_local_stores(&f, None);
    let merged = commit_on(local_only, f.repository, feature, seeded).await;
    close_handle(local_only).await;
    assert!(!server_holds_revision(&f, f.repository, merged).await);
    let merge = commit_merge_on(f.store, f.repository, f.default_branch, seeded, merged).await;

    let (status, events) = push(&f, f.default_branch, plain()).await;

    assert_eq!(status, 0);
    assert_eq!(server_latest(&f, f.default_branch).await, merge);
    assert_eq!(pushed(&events), [published(merge, 3)]);
    assert!(server_holds_revision(&f, f.repository, merged).await);
    let parents = on_server(&f, async |context| {
        lore_revision::state::State::deserialize(context, merge)
            .await
            .expect("the server holds the merge")
            .parents()
    })
    .await;
    assert_eq!(parents, [seeded, merged]);
    let feature_at_server = on_server(&f, async |context| {
        branch::load_latest(context, feature)
            .await
            .unwrap_or_default()
    })
    .await;
    assert!(feature_at_server.is_zero());
    f.close().await;
}

/// The first push of a branch the remote doesn't hold creates it there and publishes its revision.
#[tokio::test]
async fn a_first_push_of_a_new_branch_creates_it_on_the_remote() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_local_branch(&f, "feature", f.default_branch, CREATOR).await;
    let committed = commit_on(f.store, f.repository, feature, seeded).await;

    let (status, events) = push(&f, feature, plain()).await;

    assert_eq!(status, 0);
    let record = server_record(&f, feature).await;
    assert_eq!(
        (record.latest, record.name.as_str()),
        (committed, "feature")
    );
    assert_eq!(pushed(&events), [published(committed, 2)]);
    f.close().await;
}

/// A branch created at both locations takes a commit through the revision tree API, and the push
/// publishes it.
#[tokio::test]
async fn a_branch_created_at_both_locations_takes_a_commit_and_publishes_it() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let both = create_branch_at_both_locations(
        &f,
        "both",
        branch::default_category(),
        f.default_branch,
        CREATOR,
    )
    .await;
    let committed = commit_on(f.store, f.repository, both, seeded).await;

    let (status, _) = push(&f, both, plain()).await;

    assert_eq!(status, 0);
    assert_eq!(server_latest(&f, both).await, committed);
    f.close().await;
}

/// A push onto a remote latest another handle moved, without a fast-forward merge, fails with
/// `BranchAdvanced`: the pushed event names the remote latest and the local latest, which stays,
/// and no published revision.
#[tokio::test]
async fn a_moved_remote_without_a_merge_fails_branch_advanced_with_remote_latest() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    let moved = publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, events) = push(&f, f.default_branch, plain()).await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    assert_eq!(
        pushed(&events),
        [Pushed {
            revision_hash: Hash::default(),
            revision_number: 0,
            fast_forward_merged: false,
            remote_latest: moved,
            local_latest: committed,
        }]
    );
    assert_eq!(server_latest(&f, f.default_branch).await, moved);
    assert_eq!(
        local_latest(f.store, f.repository, f.default_branch).await,
        committed
    );
    f.close().await;
}

/// With a fast-forward merge onto a remote latest another handle moved, the remote publishes a
/// merge revision, and the push moves the local latest to it, convergent.
#[tokio::test]
async fn fast_forward_merge_publishes_the_merge_revision() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, events) = push(&f, f.default_branch, merging()).await;

    assert_eq!(status, 0);
    let merge = server_latest(&f, f.default_branch).await;
    assert_ne!(merge, committed);
    assert_eq!(
        pushed(&events),
        [Pushed {
            revision_hash: merge,
            revision_number: number_at_server(&f, merge).await,
            fast_forward_merged: true,
            remote_latest: merge,
            local_latest: merge,
        }]
    );
    assert_eq!(
        (
            local_latest(f.store, f.repository, f.default_branch).await,
            local_value(&f, branch::LATEST_STATUS, f.default_branch).await
        ),
        (merge, None)
    );
    f.close().await;
}

/// A fast-forward merge of the first of two local revisions leaves the local latest at the last
/// revision published: the remote merges the second one too, onto the first one's merge.
#[tokio::test]
async fn a_merge_on_the_first_of_two_revisions_leaves_local_at_the_published_revision() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let first = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    commit_on(f.store, f.repository, f.default_branch, first).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, events) = push(&f, f.default_branch, merging()).await;

    assert_eq!(status, 0);
    let latest = server_latest(&f, f.default_branch).await;
    let [event] = pushed(&events)[..] else {
        panic!("one pushed event");
    };
    assert_eq!(
        (
            event.revision_hash,
            event.local_latest,
            event.fast_forward_merged
        ),
        (latest, latest, true)
    );
    assert_eq!(
        local_latest(f.store, f.repository, f.default_branch).await,
        latest
    );
    f.close().await;
}

/// A fast-forward merge the remote refuses because it conflicts fails with `BranchAdvanced`, the
/// remote's refusal of the push itself, and the remote latest stays.
#[tokio::test]
async fn a_conflicting_fast_forward_merge_fails_branch_advanced() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    commit_named_on(f.store, f.repository, f.default_branch, seeded, "both.txt").await;
    let other = open_handle(Some(&f.url)).await;
    let moved = commit_named_on(other, f.repository, f.default_branch, seeded, "both.txt").await;
    close_handle(other).await;
    publish(&f, f.default_branch, moved).await;

    let (status, events) = push(&f, f.default_branch, merging()).await;

    assert_eq!(status, BranchAdvanced::FFI_CODE);
    let [refused] = pushed(&events)[..] else {
        panic!("one pushed event");
    };
    assert_eq!(
        (
            refused.remote_latest,
            server_latest(&f, f.default_branch).await
        ),
        (moved, moved)
    );
    f.close().await;
}

/// After a refused push, a forced sync to the event's remote latest, expecting the event's local
/// latest, recovers as the LEP's sketch does: the local latest is the remote latest.
#[tokio::test]
async fn a_forced_sync_after_a_rejected_push_recovers_as_the_lep_sketch_does() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    commit_on(f.store, f.repository, f.default_branch, seeded).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    let (status, events) = push(&f, f.default_branch, plain()).await;
    assert_eq!(status, BranchAdvanced::FFI_CODE);
    let [refused] = pushed(&events)[..] else {
        panic!("one pushed event");
    };

    let (status, _) = sync_branch(
        &f,
        f.store,
        graph_globals(),
        f.default_branch,
        by_hash(refused.remote_latest),
        LoreRevisionGraphBranchSyncOptions {
            force: 1,
            expected_local_latest: refused.local_latest,
        },
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(
        local_latest(f.store, f.repository, f.default_branch).await,
        server_latest(&f, f.default_branch).await
    );
    f.close().await;
}

/// `force` overwrites a remote latest another handle moved.
#[tokio::test]
async fn force_overwrites_a_moved_latest() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;

    let (status, events) = push(
        &f,
        f.default_branch,
        LoreRevisionGraphBranchPushOptions {
            force: 1,
            ..Default::default()
        },
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(server_latest(&f, f.default_branch).await, committed);
    assert_eq!(pushed(&events), [published(committed, 2)]);
    f.close().await;
}

/// A branch without a local latest fails with `BranchNotFound`, with no pushed event.
#[tokio::test]
async fn no_local_latest_fails_branch_not_found() {
    let f = fixture().await;
    seed_main(&f).await;
    let remote_only = create_remote_branch(&f, "remote-only", f.default_branch, CREATOR).await;

    let (status, events) = push(&f, remote_only, plain()).await;

    assert_eq!(status, BranchNotFound::FFI_CODE);
    assert!(pushed(&events).is_empty());
    f.close().await;
}

/// The push ignores `globals.dry_run`, and publishes; it ignores `globals.force`, and fails with
/// `BranchAdvanced` without `force` in its options.
#[tokio::test]
async fn globals_force_and_dry_run_are_ignored() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    let (dry_run, _) = push_branch_call(
        &f,
        f.store,
        LoreGlobalArgs {
            dry_run: 1,
            ..graph_globals()
        },
        f.default_branch,
        plain(),
    )
    .await;
    let published_latest = server_latest(&f, f.default_branch).await;
    commit_on(f.store, f.repository, f.default_branch, committed).await;
    publish_from_another_handle(&f, f.default_branch, committed).await;

    let (forced, _) = push_branch_call(
        &f,
        f.store,
        LoreGlobalArgs {
            force: 1,
            ..graph_globals()
        },
        f.default_branch,
        plain(),
    )
    .await;

    assert_eq!(
        (dry_run, published_latest, forced),
        (0, committed, BranchAdvanced::FFI_CODE)
    );
    f.close().await;
}

/// The `remote` flag means the same as no flag for a push.
#[tokio::test]
async fn remote_flag_behaves_as_no_flag() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let (status, events) = push_branch_call(
        &f,
        f.store,
        graph_globals_at(LoreBranchLocation::Remote),
        f.default_branch,
        plain(),
    )
    .await;

    assert_eq!(status, 0);
    assert_eq!(pushed(&events), [published(committed, 2)]);
    f.close().await;
}

/// A push where the remote latest is the local latest succeeds without offering a revision, and
/// its event names that revision.
#[tokio::test]
async fn a_push_with_nothing_to_publish_reports_the_local_latest() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;

    let (status, events) = push(&f, f.default_branch, plain()).await;

    assert_eq!(status, 0);
    assert_eq!(pushed(&events), [published(seeded, 1)]);
    f.close().await;
}

/// The first push of a branch with no revision of its own creates it on the remote at its branch
/// point, and its event names the branch point.
#[tokio::test]
async fn a_first_push_of_a_branch_without_revisions_reports_its_branch_point() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let feature = create_local_branch(&f, "feature", f.default_branch, CREATOR).await;

    let (status, events) = push(&f, feature, plain()).await;

    assert_eq!(status, 0);
    assert_eq!(server_latest(&f, feature).await, seeded);
    assert_eq!(pushed(&events), [published(seeded, 1)]);
    f.close().await;
}

/// A local latest an earlier push published without recording it, which the remote has moved
/// past since, is a push with nothing to publish: it succeeds, names the local latest and the
/// remote latest, and records the local latest convergent.
#[tokio::test]
async fn a_revision_a_killed_push_published_that_the_remote_moved_past_succeeds() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    publish(&f, f.default_branch, committed).await;
    in_client(branch::store_last_sync(
        local_writer(&f),
        f.default_branch,
        committed,
    ))
    .await;
    let moved = publish_from_another_handle(&f, f.default_branch, committed).await;

    let (status, events) = push(&f, f.default_branch, plain()).await;

    assert_eq!(status, 0);
    assert_eq!(
        pushed(&events),
        [Pushed {
            revision_hash: committed,
            revision_number: 2,
            fast_forward_merged: false,
            remote_latest: moved,
            local_latest: committed,
        }]
    );
    assert_eq!(
        local_value(&f, branch::LATEST_STATUS, f.default_branch).await,
        None
    );
    f.close().await;
}

/// At `globals.stats` 1 the push forwards one statistics event and its fragment progress, and no
/// other push event, in order: statistics, then the pushed event, then `Complete`.
#[tokio::test]
async fn progress_and_stats_reach_the_caller_and_no_other_push_event_does() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    commit_on(f.store, f.repository, f.default_branch, seeded).await;

    let (status, events) = push_branch_call(
        &f,
        f.store,
        LoreGlobalArgs {
            stats: 1,
            ..graph_globals()
        },
        f.default_branch,
        plain(),
    )
    .await;

    assert_eq!(status, 0);
    let count = |pick: fn(&LoreEvent) -> bool| events.iter().filter(|event| pick(event)).count();
    assert_eq!(
        (
            count(|event| matches!(event, LoreEvent::BranchPushStats(_))),
            count(|event| matches!(event, LoreEvent::BranchPushFragmentProgress(_))) > 0,
            count(|event| matches!(
                event,
                LoreEvent::BranchPush(_)
                    | LoreEvent::BranchPushRevisionPushBegin(_)
                    | LoreEvent::BranchPushRevisionPushEnd(_)
                    | LoreEvent::BranchPushRevisionPushUpdate(_)
                    | LoreEvent::BranchPushFragmentBegin(_)
                    | LoreEvent::BranchPushFragmentEnd(_)
            )),
        ),
        (1, true, 0)
    );
    let position = |pick: fn(&LoreEvent) -> bool| events.iter().position(pick);
    let stats = position(|event| matches!(event, LoreEvent::BranchPushStats(_)));
    let pushed_event = position(|event| matches!(event, LoreEvent::RevisionGraphBranchPushed(_)));
    let complete = position(|event| matches!(event, LoreEvent::Complete(_)));
    assert!(stats < pushed_event && pushed_event < complete);
    f.close().await;
}

/// A sync to the remote latest after a push the remote fast-forward merged fails with `Divergent`
/// from a local latest the merge reaches only through its second parent, the revision committed,
/// which a forced sync moves the local latest back to first; with `force` it moves. Sync's rule,
/// tested here because only a push makes Lore Server create the merge.
#[tokio::test]
async fn a_sync_after_a_fast_forward_merge_push_fails_divergent_without_force() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    publish_from_another_handle(&f, f.default_branch, seeded).await;
    let (status, _) = push(&f, f.default_branch, merging()).await;
    assert_eq!(status, 0);
    let merge = server_latest(&f, f.default_branch).await;
    let back = sync_branch(
        &f,
        f.store,
        graph_globals(),
        f.default_branch,
        by_hash(committed),
        LoreRevisionGraphBranchSyncOptions {
            force: 1,
            ..Default::default()
        },
    )
    .await
    .0;

    let plain_sync = sync_branch(
        &f,
        f.store,
        graph_globals(),
        f.default_branch,
        by_hash(merge),
        LoreRevisionGraphBranchSyncOptions::default(),
    )
    .await
    .0;
    let forced_sync = sync_branch(
        &f,
        f.store,
        graph_globals(),
        f.default_branch,
        by_hash(merge),
        LoreRevisionGraphBranchSyncOptions {
            force: 1,
            ..Default::default()
        },
    )
    .await
    .0;

    assert_eq!((back, plain_sync, forced_sync), (0, Divergent::FFI_CODE, 0));
    f.close().await;
}

/// A tree commit that moves the local latest after the push read it, as a local store that another
/// writer stores to once it answers the push's read, makes the closing compare-and-swap of the
/// local latest miss: the push still succeeds, the commit stays, and the pushed event names it.
/// The local swap's `BranchAdvanced` is no refusal of the remote.
#[tokio::test]
async fn a_local_latest_moved_during_the_push_is_no_refusal() {
    let f = fixture().await;
    let seeded = seed_main(&f).await;
    let pushed_revision = commit_on(f.store, f.repository, f.default_branch, seeded).await;
    let committed = commit_on(f.store, f.repository, f.default_branch, pushed_revision).await;
    in_client(branch::store_latest(
        local_writer(&f),
        f.default_branch,
        committed,
        pushed_revision,
        branch::BranchLatestStatus::Divergent,
    ))
    .await
    .expect("moves the local latest back to the revision pushed");
    let overtake = Overtake::default();
    let state = handle_state(&f);
    let moving = register_handle(
        &f,
        state.immutable.clone(),
        HookedStore::wrap(
            state.mutable.clone(),
            |_, _, _| Ok(()),
            overtaking_load(overtake.clone()),
        ),
        state.remote.clone(),
    );
    let (latest_key, _) = branch::mutable_key(
        lore_revision::repository::SALT_LORE,
        branch::LATEST,
        f.repository,
        f.default_branch,
    );
    *overtake.lock().unwrap() = Some((latest_key, committed));

    let (status, events) =
        push_branch_call(&f, moving, graph_globals(), f.default_branch, plain()).await;
    close_handle(moving).await;

    assert_eq!(status, 0);
    assert_eq!(
        pushed(&events),
        [Pushed {
            revision_hash: pushed_revision,
            revision_number: 2,
            fast_forward_merged: false,
            remote_latest: pushed_revision,
            local_latest: committed,
        }]
    );
    assert_eq!(
        (
            server_latest(&f, f.default_branch).await,
            local_latest(f.store, f.repository, f.default_branch).await
        ),
        (pushed_revision, committed)
    );
    f.close().await;
}
