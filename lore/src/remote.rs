// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod call;
pub mod command;
pub mod connection;
pub mod message;

pub mod network;
pub mod process;

pub const LORE_SERVICE_SOCKET_NAME: &str = "lore_service";

/// Names a socket of this process group's own, rather than the one service per
/// user that Lore otherwise shares.
///
/// The socket lives in a per-user directory and is named the same for everyone,
/// so every Lore process belonging to a user reaches the same service —
/// including one started with a different `LORE_GLOBAL_PATH`, which would then
/// serve calls against a global config the caller never asked for. Setting this
/// to a distinct value gives a set of processes a service they alone reach; the
/// test suite sets it per session, so that it can launch a real service and
/// exercise automatic use without meeting, or stopping, whatever the developer
/// is running.
///
/// The value is one path component of a file name. Anything that could reach
/// out of the socket directory, or an empty value, is refused in favour of the
/// default rather than being sanitised into something the caller did not ask
/// for.
const SOCKET_NAME_VAR: &str = "LORE_SERVICE_SOCKET";

static SOCKET_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[cfg(test)]
static SOCKET_NAME_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Points the transport at a socket of its own for the duration of the test
/// binary, so that it neither collides with nor disturbs a service already
/// running on the machine.
#[cfg(test)]
pub(crate) fn set_service_socket_name_for_test(name: &str) {
    let _ = SOCKET_NAME_OVERRIDE.set(name.to_owned());
}

/// Whether `name` is usable as the socket's file name: one non-empty component,
/// with nothing that would place the socket outside its directory.
fn valid_socket_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(std::path::is_separator)
        && !name.contains('\0')
}

/// The socket name asked for by the environment, if it asked for a usable one.
fn socket_name_from_env() -> Option<String> {
    let requested = std::env::var(SOCKET_NAME_VAR).ok()?;
    let requested = requested.trim();
    if valid_socket_name(requested) {
        return Some(requested.to_owned());
    }
    lore_revision::lore_warn!(
        "{SOCKET_NAME_VAR} is not a usable socket name ({requested:?}), \
         using {LORE_SERVICE_SOCKET_NAME}"
    );
    None
}

/// The socket file name the service listens on.
pub(crate) fn service_socket_name() -> &'static str {
    #[cfg(test)]
    if let Some(name) = SOCKET_NAME_OVERRIDE.get() {
        return name.as_str();
    }
    SOCKET_NAME
        .get_or_init(|| {
            socket_name_from_env().unwrap_or_else(|| LORE_SERVICE_SOCKET_NAME.to_owned())
        })
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_is_usable() {
        for name in ["lore_service", "lore_service_test_ab12", "lore-service.1"] {
            assert!(valid_socket_name(name), "{name:?} should be usable");
        }
    }

    /// The name becomes a file name in the socket directory, so a value that
    /// walks out of it is refused rather than trusted.
    #[test]
    fn a_name_that_escapes_its_directory_is_refused() {
        let mut escaping = vec!["", ".", "..", "../lore_service", "sub/lore_service"];
        if cfg!(windows) {
            escaping.push("sub\\lore_service");
        }
        for name in escaping {
            assert!(!valid_socket_name(name), "{name:?} should be refused");
        }
    }
}
