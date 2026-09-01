// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::change;
use crate::change::NodeChange;
use crate::diff;
use crate::errors::*;
use crate::event;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::link;
use crate::lore::Address;
use crate::lore::Hash;
use crate::node::INVALID_NODE;
use crate::repository::RepositoryContext;
use crate::state;
use crate::state::State;
use crate::util::collect_stream::collect_stream_with_summary;
use crate::util::path::RelativePath;

/// Details of a single file that differs between two revisions.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionDiffFileEventData {
    /// Path of the file relative to the repository root.
    pub path: LoreString,
    /// Action applied to the file.
    pub action: LoreFileAction,
    /// Flag indicating the entry on the source side is a file rather than a directory.
    pub old_is_file: u8,
    /// Flag indicating the entry on the target side is a file rather than a directory.
    pub new_is_file: u8,
    /// Address of the file content on the source side.
    pub old_address: Address,
    /// Address of the file content on the target side.
    pub new_address: Address,
    /// Previous path of the file when it was moved or copied. Empty otherwise.
    pub from_path: LoreString,
}

impl LoreRevisionDiffFileEventData {
    pub fn from_node_change(change: &NodeChange, old_is_file: bool, new_is_file: bool) -> Self {
        LoreRevisionDiffFileEventData {
            path: LoreString::from(&change.path),
            action: LoreFileAction::from(change.action),
            old_is_file: old_is_file.into(),
            new_is_file: new_is_file.into(),
            old_address: change.from.address,
            new_address: change.to.address,
            from_path: change.from_path.as_ref().map(|path| path.as_str()).into(),
        }
    }

    pub fn action_as_string_short(&self) -> &'static str {
        self.action.as_string_short()
    }
}

#[error_set]
pub enum DiffError {
    AddressNotFound,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    RevisionNotFound,
    WriteRequired,
    Disconnected,
    InvalidArguments,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    PayloadNotFound,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    FileNotFound,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl crate::event::EventError for DiffError {}

/// A request scoped *inside* a link asks for that subtree, so the link's own
/// entry is left out.
fn link_path_in_scope(link_path: &str, paths: Option<&[RelativePath]>) -> bool {
    let Some(paths) = paths else {
        return true;
    };
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|path| {
        let prefix = path.as_str();
        prefix.is_empty()
            || link_path == prefix
            || link_path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Calculate the difference between two revisions, as the set of changes that describe
/// going from revision 'source' to revision 'target', optionally filtered by a set of paths
pub async fn diff(
    repository: Arc<RepositoryContext>,
    source: Hash,
    target: Hash,
    paths: Option<Vec<RelativePath>>,
) -> Result<(), DiffError> {
    let state_source = State::deserialize(repository.clone(), source)
        .await
        .forward::<DiffError>("deserializing source state")?;
    let state_target = State::deserialize(repository.clone(), target)
        .await
        .forward::<DiffError>("deserializing target state")?;

    let (_, mut diff) = collect_stream_with_summary(|tx| {
        diff::diff_revision_paths(
            repository.clone(),
            state_source.clone(),
            state_target.clone(),
            paths.clone(),
            tx,
        )
    })
    .await
    .forward::<DiffError>("diffing states")?;
    state::detect_and_coalesce_moves(&mut diff);
    change::sort_by_path(&mut diff);

    let pin_changes = link::diff_link_pins(repository.clone(), &state_source, &state_target)
        .await
        .forward::<DiffError>("diffing link pins")?;
    for pin_change in pin_changes {
        if !link_path_in_scope(&pin_change.link_path, paths.as_deref()) {
            continue;
        }
        event::LoreEvent::RevisionDiffFile(LoreRevisionDiffFileEventData {
            path: LoreString::from(pin_change.link_path.as_str()),
            action: LoreFileAction::Keep,
            old_is_file: 0,
            new_is_file: 0,
            old_address: Address {
                hash: pin_change.revision_from,
                context: pin_change.link_repository.into(),
            },
            new_address: Address {
                hash: pin_change.revision_to,
                context: pin_change.link_repository.into(),
            },
            from_path: LoreString::default(),
        })
        .send();
    }

    for change in diff {
        let mut old_is_file = false;
        if change.from.node != INVALID_NODE {
            old_is_file = change
                .from
                .state
                .node(change.from.repository.clone(), change.from.node)
                .await
                .forward::<DiffError>("deserializing source state")?
                .is_file();
        }

        let mut new_is_file = false;
        if change.to.node != INVALID_NODE {
            new_is_file = change
                .to
                .state
                .node(change.to.repository.clone(), change.to.node)
                .await
                .forward::<DiffError>("deserializing target state")?
                .is_file();
        }

        event::LoreEvent::RevisionDiffFile(LoreRevisionDiffFileEventData::from_node_change(
            &change,
            old_is_file,
            new_is_file,
        ))
        .send();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn paths(values: &[&str]) -> Vec<RelativePath> {
        values
            .iter()
            .map(|value| RelativePath::from_str(value).expect("valid path"))
            .collect()
    }

    #[test]
    fn unfiltered_diff_includes_every_link() {
        assert!(link_path_in_scope("libs/shared", None));
        assert!(link_path_in_scope("libs/shared", Some(&[])));
    }

    #[test]
    fn link_at_the_requested_path_is_in_scope() {
        assert!(link_path_in_scope(
            "libs/shared",
            Some(&paths(&["libs/shared"]))
        ));
    }

    #[test]
    fn link_below_the_requested_path_is_in_scope() {
        assert!(link_path_in_scope("libs/shared", Some(&paths(&["libs"]))));
    }

    /// A request scoped inside a link asks for that subtree, not for the
    /// link's own entry.
    #[test]
    fn request_inside_a_link_excludes_the_link_itself() {
        assert!(!link_path_in_scope(
            "libs/shared",
            Some(&paths(&["libs/shared/sub"]))
        ));
    }

    #[test]
    fn unrelated_requested_path_excludes_the_link() {
        assert!(!link_path_in_scope("libs/shared", Some(&paths(&["docs"]))));
    }

    /// A sibling sharing a name prefix is not a parent directory.
    #[test]
    fn sibling_prefix_does_not_put_a_link_in_scope() {
        assert!(!link_path_in_scope(
            "libs/shared",
            Some(&paths(&["libs/sha"]))
        ));
    }

    #[test]
    fn any_matching_requested_path_puts_the_link_in_scope() {
        assert!(link_path_in_scope(
            "libs/shared",
            Some(&paths(&["docs", "libs"]))
        ));
    }
}
