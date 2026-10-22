// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)]

//! Coverage for the path-less `RepositoryContext` construction path used
//! by the in-memory revision-tree surface. The context's `path` field is
//! optional; when constructed with `None`, the context is fully usable
//! for store-backed operations but `require_path` rejects callers that
//! need a working-tree path.
use lore_revision::repository::RepositoryContext;

use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;

#[tokio::test]
async fn require_path_returns_invalid_arguments_when_path_is_none() {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    let ctx = RepositoryContext::new(default_repository_creation_args(immutable, mutable));
    ctx.require_path()
        .expect_err("path-less context should reject require_path");
}

#[tokio::test]
async fn require_path_returns_path_when_set() {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    let path = std::path::PathBuf::from("/tmp/lore-test-require-path");
    let ctx = RepositoryContext::new(
        default_repository_creation_args(immutable, mutable).with_path(&path),
    );
    let got = ctx
        .require_path()
        .expect("path-bearing context should return path");
    assert_eq!(got, path.as_path());
}
