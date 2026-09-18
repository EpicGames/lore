// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;

use super::LoreRepositoryDumpBeginEventData;
use super::LoreRepositoryDumpEndEventData;
use super::RepositoryContext;
use super::RepositoryError;
use crate::event;
use crate::lore::Hash;
use crate::state;
use crate::util::path::RelativePath;

pub(crate) async fn dump(
    repository: Arc<RepositoryContext>,
    revision: Option<Hash>,
    path: Option<RelativePath>,
    max_depth: usize,
) -> Result<(), RepositoryError> {
    let revision = revision.unwrap_or({
        if let Ok(staged_revision) = crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
            .ok_or("no staged revision")
        {
            staged_revision
        } else if let Ok((current_revision, _branch)) =
            crate::instance::load_current_anchor(&repository).await
        {
            current_revision
        } else {
            Hash::default()
        }
    });

    let state = state::State::deserialize(repository.clone(), revision)
        .await
        .forward::<RepositoryError>("Failed to deserialize repository state")?;

    event::LoreEvent::RepositoryDumpBegin(LoreRepositoryDumpBeginEventData {
        repository: repository.id,
        revision: state.revision(),
    })
    .send();

    let _ = state.cache_fragments(repository.clone()).await;

    let dump_result = state::dump::dump(state, repository.clone(), path, max_depth)
        .await
        .forward::<RepositoryError>("Failed to dump repository revision state");

    event::LoreEvent::RepositoryDumpEnd(LoreRepositoryDumpEndEventData::default()).send();

    dump_result
}

/// Boxed version of [`dump`] for cross-crate use.
pub fn dump_boxed(
    repository: Arc<RepositoryContext>,
    revision: Option<Hash>,
    path: Option<RelativePath>,
    max_depth: usize,
) -> crate::BoxFuture<'static, Result<(), RepositoryError>> {
    Box::pin(dump(repository, revision, path, max_depth))
}
