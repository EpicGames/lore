// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_revision_tree_info` — fetch the loaded revision's record-level
//! metadata: parent revision signatures plus the creation timestamp, author
//! identity, and metadata key count from the revision's Metadata fragment.
//! Revision-scoped; it takes no node id.

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::event::revision_tree::LoreRevisionTreeInfoEventData;
use lore_revision::interface::LoreError;
use lore_revision::interface::LoreString;
use lore_revision::metadata::CREATED_BY;
use lore_revision::metadata::Metadata;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::revision_tree::call::revision_tree_call;
use crate::revision_tree::handle::LoreRevisionTree;

/// Arguments for `lore_revision_tree_info`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, LoreArgs, bitcode::Encode, bitcode::Decode)]
#[handler(info_impl)]
pub struct LoreRevisionTreeInfoArgs {
    /// Per-call correlation id echoed back in events
    pub id: u64,
    /// Loaded revision-tree handle whose revision metadata is fetched
    pub handle: LoreRevisionTree,
}

#[error_set]
enum InfoError {
    InvalidArguments,
}

impl EventError for InfoError {
    fn translated(&self) -> LoreError {
        match self {
            InfoError::InvalidArguments(_) => LoreError::InvalidArguments,
            InfoError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Emit the id-carrying terminal for a failed `info`: zeroed fields plus the
/// populated `error_code`.
fn emit_info_error(id: u64, error_code: LoreErrorCode) {
    LoreEvent::RevisionTreeInfo(LoreRevisionTreeInfoEventData {
        id,
        error_code,
        ..Default::default()
    })
    .send();
}

/// Fetch the loaded revision's record-level metadata.
///
/// On success the caller receives `LORE_EVENT_REVISION_TREE_INFO` carrying the
/// `(repository, revision)` the handle represents, the parent revision
/// signatures, and — from the revision's Metadata fragment — the creation
/// timestamp, author identity, and metadata key count, with
/// `error_code = NONE`, before `Complete {status: 0}`. A revision with no
/// Metadata fragment reports zeroed metadata fields (not an error); a
/// present-but-unreadable fragment completes with `error_code = INTERNAL`. The
/// verb materializes no bytes to disk.
pub async fn info(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, info_impl).await
}

async fn info_impl(
    globals: LoreGlobalArgs,
    args: LoreRevisionTreeInfoArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    revision_tree_call(
        globals,
        callback,
        handle,
        args,
        info,
        |args: &LoreRevisionTreeInfoArgs| {
            emit_info_error(args.id, LoreErrorCode::InvalidArguments);
        },
        async move |internal, args: LoreRevisionTreeInfoArgs| {
            let id = args.id;

            let access = internal.access_shared().await;
            let state = access.state();

            let metadata_hash = state.metadata_hash();
            let metadata = if metadata_hash.is_zero() {
                Metadata::default()
            } else {
                match Metadata::deserialize(internal.repository_context.clone(), metadata_hash)
                    .await
                {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        emit_info_error(id, LoreErrorCode::Internal);
                        return Err(InfoError::internal_with_context(
                            error,
                            "Metadata::deserialize",
                        ));
                    }
                }
            };

            let creation_timestamp = metadata.get_timestamp().unwrap_or_default() as i64;
            let author_identity = metadata
                .get_string(CREATED_BY)
                .map(LoreString::from)
                .unwrap_or_default();
            let mut metadata_key_count = 0u32;
            metadata.walk(|_, _, _| metadata_key_count += 1);

            LoreEvent::RevisionTreeInfo(LoreRevisionTreeInfoEventData {
                id,
                repository: internal.repository,
                revision: state.revision(),
                parent: state.parents(),
                creation_timestamp,
                author_identity,
                metadata_key_count,
                error_code: LoreErrorCode::None,
            })
            .send();
            Ok(())
        },
    )
    .await
}
