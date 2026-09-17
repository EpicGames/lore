// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod call;
pub mod command;
pub mod connection;
pub mod message;

pub mod network;
pub mod service_process;

use std::ffi::OsString;
use std::path::Path;

use lore_revision::lore_warn;

/// Name of the socket a service listens on, when nothing else names one.
pub const LORE_SERVICE_SOCKET_NAME: &str = "lore_service";

/// Names the socket, so that processes sharing a value get a service of their
/// own rather than the one everything else on the machine is using.
///
/// One socket per user is right for a user: every command they run reaches the
/// service they have. It is wrong for anything that wants a service to itself —
/// a test suite, which would otherwise stop a service the developer is using and
/// could not run twice at once, or two checkouts being worked on side by side.
pub const LORE_SERVICE_SOCKET_VAR: &str = "LORE_SERVICE_SOCKET";

/// Whether `name` is a single file name, and so cannot move the socket out of
/// the directory chosen for it.
///
/// The socket's directory is picked for being private to the user; a value
/// carrying a path could put the socket somewhere with weaker permissions, or
/// somewhere another user can point a process at.
fn is_single_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        // Catches what a platform treats as a path but the checks above do not,
        // such as a drive-relative name on Windows.
        && Path::new(name).file_name().is_some_and(|only| only == name)
}

/// The name of the socket a service listens on, given the one named in the
/// environment.
///
/// A named value that is not a single file name is refused in favour of the
/// default rather than honoured, so a value that would move the socket elsewhere
/// cannot do so quietly.
fn socket_name_from(named: Option<OsString>) -> String {
    let Some(named) = named else {
        return LORE_SERVICE_SOCKET_NAME.to_string();
    };

    let named = named.to_string_lossy();
    let named = named.trim();
    if named.is_empty() {
        return LORE_SERVICE_SOCKET_NAME.to_string();
    }

    if !is_single_file_name(named) {
        lore_warn!(
            "Ignoring {LORE_SERVICE_SOCKET_VAR}={named}: it must name a single file, not a path"
        );
        return LORE_SERVICE_SOCKET_NAME.to_string();
    }

    named.to_string()
}

static SOCKET_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The name of the socket a service listens on.
///
/// Read once. Every connect asks for it and a start or stop wait asks hundreds
/// of times, while reading the environment takes a process-global lock in `std`.
/// Fixing it also keeps a process from splitting its calls across two sockets if
/// the variable were to change underneath it.
pub fn service_socket_name() -> &'static str {
    SOCKET_NAME
        .get_or_init(|| socket_name_from(std::env::var_os(LORE_SERVICE_SOCKET_VAR)))
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
