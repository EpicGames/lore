// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Filesystem abstraction layer for repository operations.
//!
//! This module provides traits and implementations for filesystem access, enabling both
//! OS-backed and VFS-backed (e.g., SWFS) repository instances.

pub mod filesystem_provider;
#[cfg(target_os = "linux")]
pub mod fuse;
pub mod hold;
pub mod invalid_filesystem_provider;
pub mod mount_manager;
pub mod mount_manager_state;
pub mod os;
pub mod realize;
pub mod swfs;
