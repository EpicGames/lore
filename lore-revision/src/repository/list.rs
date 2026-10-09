// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_error_set::prelude::*;
use serde::Serialize;

use super::RepositoryError;
use crate::event;
use crate::interface::LoreString;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::protocol;

/// One entry in a repository listing.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreRepositoryListEntryEventData {
    /// Repository identifier.
    pub id: RepositoryId,
    /// Repository name.
    pub name: LoreString,
}

pub async fn list(url: &str, identity: &str) -> Result<(), RepositoryError> {
    lore_debug!("Repository list at {url} using identity \"{identity}\"");
    let connection = protocol::connect(
        url,
        identity,
        RepositoryId::default(), /* No repository */
    )
    .await
    .forward_with::<RepositoryError, _>(|| {
        format!("Failed to connect to remote repository {url}")
    })?;

    let repository_service = connection
        .repository()
        .await
        .forward_with::<RepositoryError, _>(|| {
            format!("Failed to connect to remote repository {url}")
        })?;

    let list = repository_service
        .list()
        .await
        .forward::<RepositoryError>("Failed to list repositories")?;

    lore_debug!("Got repository list with {} entries", list.len());

    for entry in list {
        lore_debug!("{:?}", entry);
        event::LoreEvent::RepositoryListEntry(LoreRepositoryListEntryEventData {
            id: entry.id,
            name: entry.name.into(),
        })
        .send();
    }

    Ok(())
}
