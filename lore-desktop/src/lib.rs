// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A desktop client for Lore, on the `lore` library API.
//!
//! The UI thread never calls the library. [`worker`] runs each call on a thread of its own and
//! hands its results to the UI as messages, which the view models in [`status`] fold into the
//! state [`app`] draws.

pub mod app;
pub mod status;
pub mod worker;
