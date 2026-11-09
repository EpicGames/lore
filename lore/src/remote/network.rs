// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;

// Reexport the unimplemented OS generic module
#[cfg(not(any(target_os = "windows", target_family = "unix")))]
mod stub;

#[cfg(not(any(target_os = "windows", target_family = "unix")))]
mod os_specific {
    pub use super::stub::UdsListener;
    pub use super::stub::UdsListenerClaim;
    pub use super::stub::UdsStream;
    pub use super::stub::uds_supported;
}

// Where the socket lives on Unix
#[cfg(all(target_family = "unix", not(feature = "test-util")))]
mod unix;
#[cfg(all(target_family = "unix", feature = "test-util"))]
pub mod unix;

// Where the socket lives on Windows
#[cfg(all(target_os = "windows", not(feature = "test-util")))]
mod windows;
#[cfg(all(target_os = "windows", feature = "test-util"))]
pub mod windows;

// The socket itself, the same wherever there is one
#[cfg(any(target_os = "windows", target_family = "unix"))]
mod socket;
#[cfg(any(target_os = "windows", target_family = "unix"))]
mod os_specific {
    pub use super::socket::UdsListener;
    pub use super::socket::UdsListenerClaim;
    pub use super::socket::UdsStream;
    pub use super::socket::uds_supported;
}

// Reexport everything from the private OS specific networking module
pub use os_specific::*;

#[error_set]
pub enum UdsListenerError {}

#[error_set]
pub enum UdsAcceptError {}

#[error_set]
pub enum UdsConnectionError {}
