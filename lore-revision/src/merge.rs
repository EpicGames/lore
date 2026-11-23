// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_storage::ContentSource;

use crate::change;
use crate::change::NodeChange;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::immutable;
use crate::immutable::ImmutableError;
use crate::infer::infer_is_diffable;
use crate::node::Node;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::state::StateError;
use crate::util::path::RelativePath;

/// Merge two files given a common ancestor.
///
/// # Arguments
///
/// * `base` - A &str that holds the common ancestor of mine and theirs.
/// * `mine` - A &str that holds the mine / left / current version.
/// * `theirs` - A &str that holds the theirs / right / incoming version.
/// * `base_marker` - An optional &str that holds the text to mark the common ancestor version. Defaults to 'original'.
/// * `mine_marker` - An optional &str that holds the text to mark the mine / left / current version. Defaults to 'ours'.
/// * `theirs_marker` - An optional &str that holds the text to mark the theirs / right / incoming version. Defaults to 'theirs'.
///
/// # Return value
///
/// * `Ok(String)` if there was a successful merge.
/// * `Err(String)` if there were conflicts, with the conflicting regions marked with conflict markers.
///
pub fn merge3_text(
    base: &str,
    mine: &str,
    theirs: &str,
    base_marker: Option<&str>,
    mine_marker: Option<&str>,
    theirs_marker: Option<&str>,
) -> Result<String, String> {
    // `Git`, not diffy's `Diff3` default: `Diff3` glues the next marker onto a
    // final line that lacks a newline, which is unparsable.
    let merge_result = diffy::MergeOptions::new()
        .set_incomplete_hunk_style(diffy::IncompleteHunkStyle::Git)
        .merge(base, mine, theirs);
    let merge_conflicts = merge_result.is_err();
    let mut merge_output = match merge_result {
        Ok(str) | Err(str) => str,
    };

    if merge_conflicts {
        if let Some(str) = base_marker {
            merge_output = merge_output.replace("||||||| original", &format!("||||||| {str}"));
        }
        if let Some(str) = mine_marker {
            merge_output = merge_output.replace("<<<<<<< ours", &format!("<<<<<<< {str}"));
        }
        if let Some(str) = theirs_marker {
            merge_output = merge_output.replace(">>>>>>> theirs", &format!(">>>>>>> {str}"));
        }

        Err(merge_output)
    } else {
        Ok(merge_output)
    }
}

/// Whether a text merge should persist its output to disk.
///
/// `DryRun` computes the merge and reports conflicts without writing. `Write`
/// performs the same computation and then writes the merged result to the
/// `result` path; because writing is a repository mutation it carries a
/// borrowed [`RepositoryWriteToken`] as compile-time proof of authorization.
pub enum MergeTextMode<'a> {
    DryRun,
    Write(&'a RepositoryWriteToken),
}

/// Merges the three texts: whether they conflicted, and the merged text where `mode` asks for it
/// to be written.
///
/// A conflict yields text like any other merge, the conflicting regions carrying markers, so the
/// caller writes it either way. Bytes that are not text are read lossily, there being no
/// encoding to refuse them under.
fn merge3_text_outcome(
    base: &[u8],
    mine: &[u8],
    theirs: &[u8],
    mode: &MergeTextMode<'_>,
) -> (bool, Option<String>) {
    let base = String::from_utf8_lossy(base);
    let mine = String::from_utf8_lossy(mine);
    let theirs = String::from_utf8_lossy(theirs);

    let merged = merge3_text(&base, &mine, &theirs, None, None, None);
    let conflicted = merged.is_err();
    let output = matches!(mode, MergeTextMode::Write(_)).then(|| match merged {
        Err(text) | Ok(text) => text,
    });
    (conflicted, output)
}

/// One side of a merge, as the task spawned to read it left it.
fn merge_side(
    read: Result<Result<bytes::Bytes, lore_storage::StorageError>, tokio::task::JoinError>,
) -> Result<bytes::Bytes, FsError> {
    read.map_err(std::io::Error::other)?
        .forward_any::<FsError>("Failed to read a side of the merge")
}

/// [`merge3_text`] over three files the operation names, writing the result at `result` where
/// `mode` asks for it.
///
/// The three are read at once: none of them waits on another, and a merge is as slow as the
/// slowest of them rather than the sum.
#[lore_macro::test_pub]
pub(crate) async fn merge3_text_in_operation(
    operation: &InstanceOperationImpl,
    base: &RelativePath,
    mine: &RelativePath,
    theirs: &RelativePath,
    result: &RelativePath,
    mode: MergeTextMode<'_>,
) -> Result<bool, FsError> {
    let (base_source, mine_source, theirs_source) = tokio::try_join!(
        operation.content_source(base),
        operation.content_source(mine),
        operation.content_source(theirs)
    )
    .forward::<FsError>("naming the sides of the merge")?;

    let base_read = lore_spawn!(async move { base_source.read_all().await });
    let mine_read = lore_spawn!(async move { mine_source.read_all().await });
    let theirs_read = lore_spawn!(async move { theirs_source.read_all().await });

    let base_buffer = merge_side(base_read.await)?;
    let mine_buffer = merge_side(mine_read.await)?;
    let theirs_buffer = merge_side(theirs_read.await)?;

    let (conflicted, output) =
        merge3_text_outcome(&base_buffer, &mine_buffer, &theirs_buffer, &mode);
    if let Some(output) = output {
        operation
            .write_file(result, bytes::Bytes::from(output))
            .await?;
    }

    Ok(conflicted)
}

/// The rules [`merge_conflict_as_text`] merges a conflict by.
#[lore_macro::test_pub]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextRules {
    /// The ones `branch diff --auto-resolve` has always reported a conflict resolved by: sides
    /// read lossily, the source's alone checked for being text.
    Lossy,
    /// For a merge that stores what it merges: every side text by its first bytes and UTF-8, so
    /// that the merge keeps every byte it writes.
    Exact,
}

/// A conflict [`merge_conflict_as_text`] merges: one file both sides changed, and the nodes of its
/// base, which may hold none, of the target and of the source.
pub(crate) struct TextConflict<'a> {
    pub(crate) source: &'a NodeChange,
    pub(crate) target: &'a NodeChange,
    rules: TextRules,
    base: Option<Node>,
    pub(crate) mine: Node,
    theirs: Node,
}

/// The conflict between the source's change `source` and the target's change `target` as one
/// [`merge_conflict_as_text`] merges by `rules`, or `None` where it is no such conflict.
pub(crate) async fn text_conflict<'a>(
    source: &'a NodeChange,
    target: &'a NodeChange,
    rules: TextRules,
) -> Result<Option<TextConflict<'a>>, StateError> {
    if source.path() != target.path() || change::merged_mode(source, target).is_none() {
        return Ok(None);
    }
    let base = async {
        if source.from.flags.contains(NodeFlags::File) {
            source.from.get_node().await.map(Some)
        } else {
            Ok(None)
        }
    };
    let (base, mine, theirs) = tokio::try_join!(base, target.to.get_node(), source.to.get_node())?;
    Ok(Some(TextConflict {
        source,
        target,
        rules,
        base,
        mine,
        theirs,
    }))
}

/// The text merge of `conflict` by its rules, `None` where it is left a conflict: a side of it is
/// not text the rules merge, or the text conflicts.
///
/// Read from the store and merged in memory. A binary side is refused from its first bytes, before
/// any side is read whole.
pub(crate) async fn merge_conflict_as_text(
    conflict: &TextConflict<'_>,
) -> Result<Option<String>, ImmutableError> {
    let repository = &conflict.target.to.mapping.repository;
    let exact = conflict.rules == TextRules::Exact;
    let base = conflict
        .base
        .as_ref()
        .filter(|base| exact && base.size > 0)
        .map(|base| immutable::stored_content(repository, base));
    let mine = exact.then(|| immutable::stored_content(repository, &conflict.mine));
    let theirs = immutable::stored_content(repository, &conflict.theirs);
    let diffable = tokio::try_join!(
        is_text(base.as_ref()),
        is_text(mine.as_ref()),
        infer_is_diffable(&theirs)
    )
    .internal("reading the first bytes of a file's merge")?;
    if diffable != (true, true, true) {
        return Ok(None);
    }

    let (base, mine, theirs) = tokio::try_join!(
        read_side(repository, conflict.base.as_ref()),
        read_side(repository, Some(&conflict.mine)),
        read_side(repository, Some(&conflict.theirs))
    )?;
    Ok(merge_sides(&base, &mine, &theirs, conflict.rules))
}

/// Whether `side` is text by its first bytes, `true` where no side is checked.
async fn is_text(side: Option<&ContentSource<'static>>) -> std::io::Result<bool> {
    match side {
        Some(side) => infer_is_diffable(side).await,
        None => Ok(true),
    }
}

/// The clean merge of three sides by `rules`, `None` where the text conflicts or, by
/// [`TextRules::Exact`], a side is not UTF-8 text.
#[lore_macro::test_pub]
fn merge_sides(base: &[u8], mine: &[u8], theirs: &[u8], rules: TextRules) -> Option<String> {
    if rules == TextRules::Lossy {
        let [base, mine, theirs] = [base, mine, theirs].map(String::from_utf8_lossy);
        return merge3_text(&base, &mine, &theirs, None, None, None).ok();
    }
    let [base, mine, theirs] = [base, mine, theirs].map(|side| std::str::from_utf8(side).ok());
    merge3_text(base?, mine?, theirs?, None, None, None).ok()
}

/// The content of `node`, empty where there is none.
async fn read_side(
    repository: &Arc<RepositoryContext>,
    node: Option<&Node>,
) -> Result<bytes::Bytes, ImmutableError> {
    let Some(node) = node.filter(|node| node.size > 0) else {
        return Ok(bytes::Bytes::new());
    };
    let options = immutable::read_options_from_repository(repository);
    immutable::read(repository.clone(), node.address, None, options).await
}
