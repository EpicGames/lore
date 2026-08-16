// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! PostgreSQL fragment catalog for Lore object-backed immutable storage.

mod catalog;

pub use catalog::PostgresFragmentCatalog;
pub use catalog::PostgresFragmentCatalogConfig;
