// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore::interface::LoreGlobalArgs;
use lore::revision_graph::branch_push::LoreRevisionGraphBranchPushArgs;
use lore::revision_graph::branch_push::branch_push;
use lore::revision_graph::branch_push::classify;
use lore::revision_graph::branch_push::pushed_on_failure;
use lore::revision_graph::branch_push::pushed_on_success;
use lore::storage::handle;
use lore::storage::handle::LoreStore;
use lore::storage::store::BoundFlags;
use lore_base::error::AddressNotFound;
use lore_base::error::BranchAdvanced;
use lore_base::error::BranchAlreadyExists;
use lore_base::error::Disconnected;
use lore_base::error::Divergent;
use lore_base::error::InvalidArguments;
use lore_base::error::LinkDivergent;
use lore_base::error::Maintenance;
use lore_base::error::NoRemote;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::error::NotConnected;
use lore_base::error::Oversized;
use lore_base::error::RevisionNotFound;
use lore_base::error::SlowDown;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_error_set::FfiError;
use lore_revision::branch::push::PushError;
use lore_revision::branch::push::PushReport;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_graph::LoreRevisionGraphBranchPushedEventData;

use crate::call::test_support::recording_callback;
use crate::call::test_support::take_without_log_lines;
use crate::revision_graph::support::close_handle;
use crate::revision_graph::support::hash;
use crate::revision_graph::support::new_branch;
use crate::revision_graph::support::new_repository;
use crate::revision_graph::support::open_in_memory_handle;
use crate::storage::in_memory_store;

/// The `id` every push in this file passes.
const CALL_ID: u64 = 7;

/// The FFI code of the failure [`classify`] reports for `err`, a failed push of `branch` with a name
/// holder of `name_holder`.
fn classified(err: PushError, branch: BranchId, name_holder: Option<BranchId>) -> i32 {
    classify(err, branch, name_holder).ffi_code()
}

/// Each failure maps as the LEP's error table has it: refusals of the caller, the remote's refusal
/// of a push onto a remote latest that moved, a linked revision the remote lacks, a missing or
/// oversized blob, and an overloaded or unreachable remote keep their codes; a name another branch
/// holds is `BranchAlreadyExists`, but not the branch's own; a divergence is `Divergent`; anything
/// else, such as a failed upload, is `Internal`.
#[test]
fn classify_maps_each_row() {
    let branch = new_branch();
    let other = new_branch();
    let failed = || PushError::internal("a failed upload");
    let internal = -1;

    let rows = [
        (
            classified(PushError::from(NotAuthorized), branch, None),
            NotAuthorized::FFI_CODE,
        ),
        (
            classified(PushError::from(NotAuthenticated), branch, None),
            NotAuthenticated::FFI_CODE,
        ),
        (
            classified(PushError::from(BranchAdvanced), branch, None),
            BranchAdvanced::FFI_CODE,
        ),
        (
            classified(
                PushError::from(RevisionNotFound {
                    revision: "linked".into(),
                }),
                branch,
                None,
            ),
            RevisionNotFound::FFI_CODE,
        ),
        (
            classified(
                PushError::from(AddressNotFound { address: [1; 48] }),
                branch,
                None,
            ),
            AddressNotFound::FFI_CODE,
        ),
        (
            classified(
                PushError::from(Oversized {
                    context: "fragment".into(),
                }),
                branch,
                None,
            ),
            Oversized::FFI_CODE,
        ),
        (
            classified(failed(), branch, Some(other)),
            BranchAlreadyExists::FFI_CODE,
        ),
        (classified(failed(), branch, Some(branch)), internal),
        (classified(failed(), branch, None), internal),
        (
            classified(PushError::from(Divergent), branch, None),
            Divergent::FFI_CODE,
        ),
        (
            classified(
                PushError::from(LinkDivergent {
                    path: "vendor".into(),
                    branch: "main".into(),
                }),
                branch,
                None,
            ),
            Divergent::FFI_CODE,
        ),
        (
            classified(PushError::from(SlowDown), branch, None),
            SlowDown::FFI_CODE,
        ),
        (
            classified(PushError::from(Disconnected), branch, None),
            Disconnected::FFI_CODE,
        ),
        (
            classified(
                PushError::from(NotConnected {
                    reason: "unreachable".into(),
                }),
                branch,
                None,
            ),
            NotConnected::FFI_CODE,
        ),
        (
            classified(PushError::from(Maintenance), branch, None),
            Maintenance::FFI_CODE,
        ),
        (
            classified(
                PushError::from(InvalidArguments {
                    reason: "core".into(),
                }),
                branch,
                None,
            ),
            internal,
        ),
    ];

    for (row, (code, expected)) in rows.into_iter().enumerate() {
        assert_eq!(code, expected, "row {row}");
    }
}

/// The pushed event after a push that published names what it published, the remote latest and
/// the local latest it left. One that published nothing names the local latest, with the remote
/// latest the push knew, which differs where the remote moved past a local latest an earlier push
/// published. A failure names the last revision the push published, zero for none. Each echoes the
/// call's `id` and names the branch.
#[test]
fn the_pushed_event_reports_success_a_no_op_and_a_partial_failure() {
    let branch = new_branch();
    let event = |data: LoreRevisionGraphBranchPushedEventData| {
        assert_eq!((data.id, data.branch), (CALL_ID, branch));
        (
            data.revision_hash,
            data.revision_number,
            data.fast_forward_merged,
            data.remote_latest,
            data.local_latest,
        )
    };
    let published = PushReport {
        remote_read: true,
        remote_latest: hash(3),
        published: hash(3),
        revision_number: 4,
        fast_forward_merged: true,
        local_latest: hash(3),
        ..PushReport::default()
    };
    let at_remote_latest = PushReport {
        remote_read: true,
        remote_latest: hash(1),
        ..PushReport::default()
    };
    let moved_past = PushReport {
        remote_read: true,
        remote_latest: hash(5),
        ..PushReport::default()
    };
    let partial = PushReport {
        remote_read: true,
        remote_latest: hash(3),
        published: hash(3),
        revision_number: 4,
        ..PushReport::default()
    };

    assert_eq!(
        [
            event(pushed_on_success(CALL_ID, branch, &published, hash(1), 1)),
            event(pushed_on_success(
                CALL_ID,
                branch,
                &at_remote_latest,
                hash(1),
                1
            )),
            event(pushed_on_success(CALL_ID, branch, &moved_past, hash(1), 1)),
            event(pushed_on_failure(
                CALL_ID,
                branch,
                &partial,
                hash(6),
                hash(2)
            )),
            event(pushed_on_failure(
                CALL_ID,
                branch,
                &PushReport::default(),
                hash(6),
                hash(2)
            )),
        ],
        [
            (hash(3), 4, 1, hash(3), hash(3)),
            (hash(1), 1, 0, hash(1), hash(1)),
            (hash(1), 1, 0, hash(5), hash(1)),
            (hash(3), 4, 0, hash(6), hash(2)),
            (Hash::default(), 0, 0, hash(6), hash(2)),
        ]
    );
}

/// The status and the events of a push of `branch` through `store` under `globals`.
async fn push_with(
    globals: LoreGlobalArgs,
    store: LoreStore,
    branch: BranchId,
) -> (i32, Vec<LoreEvent>) {
    let (events, callback) = recording_callback();
    let status = branch_push(
        globals,
        LoreRevisionGraphBranchPushArgs {
            id: CALL_ID,
            store,
            repository: new_repository(),
            branch,
            ..Default::default()
        },
        callback,
    )
    .await;
    (status, take_without_log_lines(&events))
}

/// Asserts that `events` hold only the terminal events, as a push that fails before any read
/// sends.
fn assert_terminal_only(events: &[LoreEvent]) {
    let [LoreEvent::Complete(_), LoreEvent::End(_)] = events else {
        panic!("expected Complete, then End");
    };
}

/// A push of a zero branch fails with `InvalidArguments`.
#[tokio::test]
async fn a_zero_branch_fails_invalid_arguments() {
    let (_store, handle) = open_in_memory_handle().await;

    let (status, events) = push_with(LoreGlobalArgs::default(), handle, BranchId::default()).await;

    close_handle(handle).await;
    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_terminal_only(&events);
}

/// A push writes to the remote, so the `local` and `offline` flags on the call fail it with
/// `InvalidArguments`, before it reads anything.
#[tokio::test]
async fn local_and_offline_flags_fail_invalid_arguments() {
    let (_store, handle) = open_in_memory_handle().await;

    let mut outcomes = vec![];
    for globals in [
        LoreGlobalArgs {
            local: 1,
            ..Default::default()
        },
        LoreGlobalArgs {
            offline: 1,
            ..Default::default()
        },
    ] {
        let (status, events) = push_with(globals, handle, new_branch()).await;
        assert_terminal_only(&events);
        outcomes.push(status);
    }

    close_handle(handle).await;
    assert_eq!(
        outcomes,
        [InvalidArguments::FFI_CODE, InvalidArguments::FFI_CODE]
    );
}

/// A handle opened offline fails a push with `InvalidArguments`, as the flag on the call does.
#[tokio::test]
async fn a_handle_opened_offline_fails_invalid_arguments() {
    let store = in_memory_store(
        None,
        BoundFlags {
            offline: true,
            ..Default::default()
        },
    )
    .await;
    let handle = handle::register(Arc::new(store));

    let (status, events) = push_with(LoreGlobalArgs::default(), handle, new_branch()).await;

    close_handle(handle).await;
    assert_eq!(status, InvalidArguments::FFI_CODE);
    assert_terminal_only(&events);
}

/// A handle without a remote fails a push with `NoRemote`, with the `remote` flag and without it.
#[tokio::test]
async fn a_handle_without_a_remote_fails_no_remote() {
    let (_store, handle) = open_in_memory_handle().await;

    let mut outcomes = vec![];
    for globals in [
        LoreGlobalArgs::default(),
        LoreGlobalArgs {
            remote: 1,
            ..Default::default()
        },
    ] {
        let (status, events) = push_with(globals, handle, new_branch()).await;
        assert_terminal_only(&events);
        outcomes.push(status);
    }

    close_handle(handle).await;
    assert_eq!(outcomes, [NoRemote::FFI_CODE, NoRemote::FFI_CODE]);
}
