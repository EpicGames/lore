// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Windows path helpers for direct Win32 file API calls, defined in lore-io.

#![cfg(target_family = "windows")]

pub use lore_io::win_path::to_extended_wide;
