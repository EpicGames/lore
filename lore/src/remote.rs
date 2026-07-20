// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod call;
pub mod command;
pub mod connection;
pub mod message;

pub mod network;
pub mod process;

pub const LORE_SERVICE_SOCKET_NAME: &str = "lore_service";

#[cfg(test)]
static SOCKET_NAME_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Points the transport at a socket of its own for the duration of the test
/// binary, so that it neither collides with nor disturbs a service already
/// running on the machine.
#[cfg(test)]
pub(crate) fn set_service_socket_name_for_test(name: &str) {
    let _ = SOCKET_NAME_OVERRIDE.set(name.to_owned());
}

/// The socket file name the service listens on.
pub(crate) fn service_socket_name() -> &'static str {
    #[cfg(test)]
    if let Some(name) = SOCKET_NAME_OVERRIDE.get() {
        return name.as_str();
    }
    LORE_SERVICE_SOCKET_NAME
}
