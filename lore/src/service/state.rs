// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Service-level state re-exports from lore-revision.
//!
//! The actual implementation is in [`lore_revision::service_state`].

pub use lore_revision::service_state::ConnectionGuard;
pub use lore_revision::service_state::LogMessage;
pub use lore_revision::service_state::MAX_BUFFER_SIZE;
pub use lore_revision::service_state::ServiceStateImpl;
