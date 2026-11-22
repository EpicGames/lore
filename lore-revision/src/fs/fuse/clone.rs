// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Cloning a virtual clone: its external directory, which holds the Lore directory, the local
//! files and the deletion list its mount builds the working tree from, cloned through the backend
//! the volume of the external directories allows. The tracked files come from the store, so
//! cloning costs what the local changes take, not what the tree does.

use std::collections::HashSet;
use std::path::Component;
use std::path::Path;

use lore_base::error::InvalidPath;
use lore_io::clone::Kind;

use crate::fs::filesystem_provider::CloneAnswer;
use crate::fs::filesystem_provider::CloneRun;
use crate::fs::filesystem_provider::InstanceCloneError;
use crate::fs::fuse::deletions::DELETION_LIST;
use crate::fs::walk::CloneStats;
use crate::fs::walk::Visit;
use crate::global::external_dir::WRITE_DIR;
use crate::repository::DOT_LORE;

/// What cloning a virtual clone's external directory does with the entry at `path`, which is at
/// `relative` in it:
///
/// - the Lore directory is cloned as an on-disk instance's is ([`crate::fs::os::clone::visit`]),
///   its modified-time records with it, which hold the times the mount reports;
/// - the local files under [`WRITE_DIR`] are cloned, leaving out Lore directories and nested
///   repositories, as an on-disk instance's tree does ([`crate::fs::os::clone::visit_tree`]);
/// - the deletion list is cloned. One cloned during an append can end inside a line, which the
///   mount drops as it drops one a crash cut short;
/// - nothing else is: not the staging directory of the writes under way, nor the file a rewrite of
///   the deletion list fills.
#[lore_macro::test_pub]
fn visit(path: &Path, relative: &Path, kind: Kind) -> Visit {
    let mut components = relative.components();
    let Some(Component::Normal(top)) = components.next() else {
        return Visit::Skip;
    };
    let below = components.as_path();
    if top == DOT_LORE {
        return crate::fs::os::clone::visit(Path::new(DOT_LORE), relative, kind);
    }
    if top == WRITE_DIR {
        if below.as_os_str().is_empty() {
            return if kind == Kind::Directory {
                Visit::EachEntry
            } else {
                Visit::Skip
            };
        }
        return crate::fs::os::clone::visit_tree(
            Path::new(DOT_LORE),
            &HashSet::new(),
            path,
            relative,
            kind,
        );
    }
    if top == DELETION_LIST && below.as_os_str().is_empty() && kind == Kind::File {
        return Visit::Clone;
    }
    Visit::Skip
}

/// [`crate::fs::filesystem_provider::FilesystemProvider::clone_answer`] for the virtual clone whose
/// external directory is `external`: probes the backends from its Lore directory into the directory
/// that holds the external directories, where the new instance's goes, and where none shares
/// blocks, measures what a copy takes ([`crate::fs::os::clone::answer_between`]).
#[lore_macro::test_pub]
pub(crate) async fn answer(external: &Path) -> Result<CloneAnswer, InstanceCloneError> {
    let parent = external
        .parent()
        .ok_or_else(|| InvalidPath {
            path: format!("{}: has no parent directory", external.display()),
        })?
        .to_path_buf();
    crate::fs::os::clone::answer_between(
        (external.join(DOT_LORE), parent),
        external.to_path_buf(),
        visit,
        |_, _| Ok(HashSet::new()),
    )
    .await
}

/// [`crate::fs::filesystem_provider::FilesystemProvider::clone_instance`] for the virtual clone
/// whose external directory is `external`, into `destination`, the new instance's external
/// directory, as [`crate::fs::os::clone::clone_through`] makes it.
#[lore_macro::test_pub]
pub(crate) async fn clone(
    external: &Path,
    destination: &Path,
    answer: &CloneAnswer,
    copy_unclonable: bool,
    run: &CloneRun,
) -> Result<CloneStats, InstanceCloneError> {
    let external = external.to_path_buf();
    let destination = destination.to_path_buf();
    crate::fs::os::clone::clone_through(answer, run, move |backend, _, progress| {
        crate::fs::walk::clone_into(
            backend,
            &external,
            &destination,
            &visit,
            copy_unclonable,
            progress,
        )
    })
    .await
}
