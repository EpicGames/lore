// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ffi::OsString;

mod network;
mod occupy;
mod service_process;

use lore::remote::command::LoreCommand;
use lore::remote::*;

#[test]
fn a_named_socket_is_used() {
    assert_eq!(
        socket_name_from(Some(OsString::from("lore_service-test-abc123"))),
        "lore_service-test-abc123"
    );
    // Surrounding space is not part of a file name.
    assert_eq!(socket_name_from(Some(OsString::from("  named  "))), "named");
}

#[test]
fn no_named_socket_uses_the_default() {
    assert_eq!(socket_name_from(None), LORE_SERVICE_SOCKET_NAME);
    assert_eq!(
        socket_name_from(Some(OsString::new())),
        LORE_SERVICE_SOCKET_NAME
    );
    assert_eq!(
        socket_name_from(Some(OsString::from("   "))),
        LORE_SERVICE_SOCKET_NAME
    );
}

/// Refused in favour of the default rather than honoured: a value carrying a
/// path could put the socket somewhere with weaker permissions.
#[test]
fn a_named_socket_that_is_a_path_is_refused() {
    for named in ["../escape", "sub/lore_service", "/tmp/lore_service", ".."] {
        assert_eq!(
            socket_name_from(Some(OsString::from(named))),
            LORE_SERVICE_SOCKET_NAME,
            "{named} must not be honoured"
        );
    }
}

#[test]
fn a_single_file_name_is_allowed() {
    for name in ["lore_service", "lore_service-1", "lore.service", "a"] {
        assert!(is_single_file_name(name), "{name} names one file");
    }
}

/// A value carrying a path would move the socket out of the directory
/// chosen for being private to the user.
#[test]
fn anything_carrying_a_path_is_refused() {
    for name in [
        "",
        ".",
        "..",
        "../lore_service",
        "sub/lore_service",
        "/tmp/lore_service",
        "..\\lore_service",
        "C:\\lore_service",
        "lore\0service",
    ] {
        assert!(
            !is_single_file_name(name),
            "{name:?} does not name a single file"
        );
    }
}

/// A relayed call's future holds its globals and command once, in its captures: the call the move
/// reply hands back holds them a second time only behind a box. Inline, the second copy takes the
/// future past three times their size.
#[test]
fn a_relayed_call_holds_its_arguments_once() {
    let call = lore::remote::call::run_service_call(
        None,
        lore_revision::interface::LoreGlobalArgs::default(),
        LoreCommand::RepositoryStatus(lore::repository::LoreRepositoryStatusArgs {
            staged: 0,
            scan: 0,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: lore_revision::interface::LoreArray::default(),
        }),
        None,
    );
    let arguments =
        size_of::<lore_revision::interface::LoreGlobalArgs>() + size_of::<LoreCommand>();
    assert!(
        size_of_val(&call) < 3 * arguments,
        "the call holds {} bytes for arguments of {arguments}",
        size_of_val(&call)
    );
}

/// A move reply after the first message fails the call as internal: the service socket answers
/// a move with it alone.
#[test]
fn a_move_reply_after_the_first_message_fails_the_call() {
    let failed = lore::remote::call::handle_message(
        &lore_revision::relay::EventDispatcher::no_dispatch(),
        lore::remote::message::MessageToClient::ExternalRootMove,
        bytes::Bytes::new(),
    )
    .expect_err("the call fails");
    assert!(failed.is_internal());
}
