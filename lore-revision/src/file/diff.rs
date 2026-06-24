// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::branch;
use crate::change;
use crate::change::NodeChange;
use crate::diff;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::immutable;
use crate::immutable::read_options_from_repository;
use crate::infer::infer_is_diffable_by_slice;
use crate::interface::LoreError;
use crate::interface::LoreFileAction;
use crate::interface::LoreString;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_warn;
use crate::merge::merge3_text;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::state;
use crate::state::State;
use crate::util::collect_stream::collect_stream_with_summary;
use crate::util::encoding::decode_text_for_display;
use crate::util::encoding::is_utf16_bom;
use crate::util::path::RelativePath;

/// Default number of unchanged context lines around each unified-diff hunk.
/// Mirrors the diffy default and the universal unified-diff convention.
pub const DEFAULT_CONTEXT_LINES: u32 = 3;

#[error_set]
pub enum DiffError {
    InvalidArguments,
    InvalidPath,
    RevisionNotFound,
    FileNotFound,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    LinkNotFound,
    Maintenance,
    NodeNotFound,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    SlowDown,
    WriteRequired,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
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

impl EventError for DiffError {
    fn translated(&self) -> LoreError {
        match self {
            DiffError::InvalidArguments(_) | DiffError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            DiffError::RevisionNotFound(_) | DiffError::NotFound(_) => LoreError::NotFound,
            DiffError::FileNotFound(_) => LoreError::FileNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for the event carrying the diff of a single file.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileDiffEventData {
    /// Path of the file.
    pub path: LoreString,
    /// Unified-diff text describing the change.
    pub patch: LoreString,
    /// Action applied to the file.
    pub action: LoreFileAction,
}

/// Display options threaded through the file-diff pipeline.
#[derive(Clone, Copy, Debug)]
pub struct DiffOptions {
    /// Number of unchanged context lines around each unified-diff hunk.
    pub context_lines: u32,
    /// Treat lines that differ only in trailing whitespace as unchanged.
    pub ignore_whitespace_eol: bool,
    /// Collapse runs of internal whitespace to a single space before comparing.
    pub ignore_whitespace_inline: bool,
}

pub async fn diff(
    repository: Arc<RepositoryContext>,
    source_revision: Option<String>,
    target_revision: Option<String>,
    paths: Vec<RelativePath>,
    diff3: bool,
    options: DiffOptions,
) -> Result<(), DiffError> {
    // Resolve the source revision
    let revision_source = if let Some(signature) = source_revision {
        revision::resolve(
            repository.clone(),
            signature.as_str(),
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|_err| {
            DiffError::from(RevisionNotFound {
                revision: signature.clone(),
            })
        })?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<DiffError>("Failed deserializing revision state")?;
        current_revision
    };

    // Optionally resolve the target revision
    let revision_target = if let Some(signature) = target_revision.as_ref() {
        Some(
            revision::resolve(
                repository.clone(),
                signature.as_str(),
                execution_context().globals().search_limit(),
                execution_context().globals().search_location(),
            )
            .await
            .map_err(|_err| {
                DiffError::from(RevisionNotFound {
                    revision: signature.clone(),
                })
            })?,
        )
    } else {
        None
    };

    let state_source = State::deserialize(repository.clone(), revision_source)
        .await
        .forward::<DiffError>("Failed deserializing revision state")?;

    let state_target = if let Some(revision_target) = revision_target {
        Some(
            State::deserialize(repository.clone(), revision_target)
                .await
                .forward::<DiffError>("Failed deserializing revision state")?,
        )
    } else {
        None
    };

    if diff3 {
        Box::pin(file_diff3(
            repository,
            state_source,
            state_target,
            revision_source,
            revision_target,
            paths,
            options,
        ))
        .await
    } else {
        file_diff2(repository, state_source, state_target, paths, options).await
    }
}

async fn file_diff2(
    repository: Arc<RepositoryContext>,
    state_source: Arc<State>,
    state_target: Option<Arc<State>>,
    paths: Vec<RelativePath>,
    options: DiffOptions,
) -> Result<(), DiffError> {
    let changes = if let Some(state_target) = state_target.as_ref() {
        let (_, mut changes) = collect_stream_with_summary(|tx| {
            diff::diff_revision_paths(
                repository.clone(),
                state_source.clone(),
                state_target.clone(),
                if !paths.is_empty() { Some(paths) } else { None },
                tx,
            )
        })
        .await
        .forward::<DiffError>("Failed to calculate diff")?;
        state::detect_and_coalesce_moves(&mut changes);
        change::sort_by_path(&mut changes);
        changes
    } else {
        let (state_current, state_staged, _branch) =
            State::deserialize_current_and_staged(repository.clone())
                .await
                .forward::<DiffError>("Failed deserializing revision state")?;
        let state_staged = state_staged.unwrap_or_else(|| state_current.clone());
        let mut changes = diff::diff_filesystem_paths(
            repository.clone(),
            state_source.clone(),
            state_staged.clone(),
            if !paths.is_empty() { Some(paths) } else { None },
        )
        .await
        .forward::<DiffError>("Failed to calculate diff")?;

        coalesce_staged_moves(
            repository.clone(),
            &state_current,
            &state_staged,
            &mut changes,
        )
        .await?;

        changes
    };

    emit_unified_diffs(
        repository,
        &state_source,
        &state_target,
        &changes,
        &[],
        options,
    )
    .await
}

async fn coalesce_staged_moves(
    repository: Arc<RepositoryContext>,
    state_current: &Arc<State>,
    state_staged: &Arc<State>,
    changes: &mut Vec<NodeChange>,
) -> Result<(), DiffError> {
    if Arc::ptr_eq(state_current, state_staged)
        || state_current.revision() == state_staged.revision()
    {
        return Ok(());
    }

    let staged = state::diff_collect(
        repository.clone(),
        state_current.clone(),
        repository.clone(),
        state_staged.clone(),
        None,
        crate::filter::FilterMode::Full,
    )
    .await
    .forward::<DiffError>("Failed to detect staged moves")?;

    for staged_change in staged {
        if staged_change.action != change::FileAction::Move {
            continue;
        }
        let Some(from_path) = staged_change.from_path.as_ref() else {
            continue;
        };
        let to_path = &staged_change.path;

        let delete_idx = changes
            .iter()
            .position(|c| c.action == change::FileAction::Delete && c.path == *from_path);
        let add_idx = changes
            .iter()
            .position(|c| c.action == change::FileAction::Add && c.path == *to_path);

        let (Some(delete_idx), Some(add_idx)) = (delete_idx, add_idx) else {
            continue;
        };

        changes[add_idx].action = change::FileAction::Move;
        changes[add_idx].from = changes[delete_idx].from.clone();
        changes[add_idx].from_path = Some(from_path.clone());

        changes.remove(delete_idx);
    }

    change::sort_by_path(changes);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn file_diff3(
    repository: Arc<RepositoryContext>,
    state_source: Arc<State>,
    state_target: Option<Arc<State>>,
    revision_source: crate::lore::Hash,
    revision_target: Option<crate::lore::Hash>,
    paths: Vec<RelativePath>,
    options: DiffOptions,
) -> Result<(), DiffError> {
    let source_branch = state_source
        .revision_metadata(repository.clone())
        .await
        .forward::<DiffError>("Failed deserializing revision state")?
        .branch;

    let (target_branch, target_revision_for_diff3) = if let Some(state_target) =
        state_target.as_ref()
    {
        let target_branch = state_target
            .revision_metadata(repository.clone())
            .await
            .forward::<DiffError>("Failed deserializing revision state")?
            .branch;
        let target_rev = revision_target.unwrap_or_default();
        (target_branch, target_rev)
    } else {
        let (current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<DiffError>("Failed deserializing revision state")?;
        (current_branch, current_revision)
    };

    // Swap CLI args to match branch::diff3's convention:
    //   CLI --source (baseline) → branch::diff3 target
    //   CLI --target → branch::diff3 source
    let diff_result = Box::pin(branch::diff3_collect(
        repository.clone(),
        target_branch,
        target_revision_for_diff3,
        source_branch,
        revision_source,
        None,
        true, // include_same: surface auto-resolved files in changes
        false,
    ))
    .await
    .internal("Failed to calculate diff")?;

    let state_base = State::deserialize(repository.clone(), diff_result.base)
        .await
        .forward::<DiffError>("Failed deserializing revision state")?;

    emit_diff3_changes(
        repository.clone(),
        &state_source,
        &state_target,
        &state_base,
        &diff_result.changes,
        &paths,
        options,
    )
    .await?;

    emit_diff3_conflicts(
        repository.clone(),
        &state_source,
        &state_target,
        &state_base,
        &diff_result.conflicts,
        &paths,
        options,
    )
    .await?;

    Ok(())
}

/// Emit diffs for non-conflicting changes in diff3 mode.
///
/// If only the target branch modified a file, emits a base-to-target diff.
/// If both branches modified it without conflict, merges and diffs source to merge result.
async fn emit_diff3_changes(
    repository: Arc<RepositoryContext>,
    state_source: &Arc<State>,
    state_target: &Option<Arc<State>>,
    state_base: &Arc<State>,
    changes: &[NodeChange],
    paths: &[RelativePath],
    options: DiffOptions,
) -> Result<(), DiffError> {
    for change in changes {
        if change.action == change::FileAction::Move
            && let Some(from_path) = change.from_path.as_ref()
        {
            if !move_matches_paths(paths, &change.path, from_path) {
                continue;
            }
            emit_move_diff(
                repository.clone(),
                state_base,
                state_target,
                from_path,
                &change.path,
                options,
            )
            .await?;
            continue;
        }

        let is_from_file = change.from.flags.contains(NodeFlags::File);
        let is_to_file = change.to.flags.contains(NodeFlags::File);

        if !is_from_file && !is_to_file {
            continue;
        }

        if !paths.is_empty() && !paths.contains(&change.path) {
            continue;
        }

        let base_content = if is_from_file {
            diff_read_file(repository.clone(), Some(state_base.clone()), &change.path).await?
        } else {
            DiffContent::empty()
        };
        let target_content = if is_to_file {
            diff_read_file(repository.clone(), state_target.clone(), &change.path).await?
        } else {
            DiffContent::empty()
        };
        // Baseline content: try-read since NodeChange flags don't cover the baseline state
        let source_content = match diff_read_file(
            repository.clone(),
            Some(state_source.clone()),
            &change.path,
        )
        .await
        {
            Ok(content) => content,
            Err(ref e) if e.is_file_not_found() => DiffContent::empty(),
            Err(e) => return Err(e),
        };

        let action = if is_from_file {
            if is_to_file {
                LoreFileAction::Keep
            } else {
                LoreFileAction::Delete
            }
        } else {
            LoreFileAction::Add
        };

        // Binary content: emit a marker instead of rendering bytes as text.
        if base_content.is_binary() || source_content.is_binary() || target_content.is_binary() {
            emit_binary_diff(&change.path, action);
            continue;
        }

        let target_label = if let Some(state_target) = state_target.as_ref() {
            format!(
                "{}@{}",
                change.path.as_str(),
                state_target.revision_number()
            )
        } else {
            change.path.as_str().to_string()
        };

        if base_content.text() == source_content.text() {
            // Only the target branch modified this file
            let from_label = if is_from_file {
                format!("{}@{}", change.path.as_str(), state_base.revision_number())
            } else {
                "/dev/null".to_string()
            };
            let to_label = if is_to_file {
                target_label.clone()
            } else {
                "/dev/null".to_string()
            };
            emit_diff_event(
                base_content.text(),
                target_content.text(),
                &from_label,
                &to_label,
                &change.path,
                action,
                options,
            );
        } else {
            // Both branches modified; merge to isolate the target branch's contribution
            let merge_result = match merge3_text(
                base_content.text(),
                source_content.text(),
                target_content.text(),
                None,
                None,
                None,
            ) {
                Ok(text) => text,
                Err(text) => {
                    lore_warn!(
                        "Unexpected merge conflict for auto-resolved file {}: skipping",
                        change.path.as_str()
                    );
                    lore_debug!("Conflict output: {text}");
                    continue;
                }
            };

            emit_diff_event(
                source_content.text(),
                &merge_result,
                &format!(
                    "{}@{}",
                    change.path.as_str(),
                    state_source.revision_number()
                ),
                &format!("{target_label} (merged)"),
                &change.path,
                action,
                options,
            );
        }
    }

    Ok(())
}

/// Emit three-way merge output for conflicting changes in diff3 mode.
///
/// Conflict pairs use `branch::diff3`'s internal convention (swapped from CLI):
/// `source_change` corresponds to CLI `--target`, `target_change` to CLI `--source`.
async fn emit_diff3_conflicts(
    repository: Arc<RepositoryContext>,
    state_source: &Arc<State>,
    state_target: &Option<Arc<State>>,
    state_base: &Arc<State>,
    conflicts: &[(NodeChange, NodeChange)],
    paths: &[RelativePath],
    options: DiffOptions,
) -> Result<(), DiffError> {
    for (source_change, target_change) in conflicts {
        let target_is_file = source_change.from.flags.contains(NodeFlags::File)
            || source_change.to.flags.contains(NodeFlags::File);
        let source_is_file = target_change.from.flags.contains(NodeFlags::File)
            || target_change.to.flags.contains(NodeFlags::File);

        if !target_is_file && !source_is_file {
            continue;
        }

        if !paths.is_empty() && !paths.contains(&source_change.path) {
            continue;
        }

        let base_has_file = source_change.from.flags.contains(NodeFlags::File);
        let target_has_file = source_change.to.flags.contains(NodeFlags::File);
        let source_has_file = target_change.to.flags.contains(NodeFlags::File);

        let base = if base_has_file {
            diff_read_file(
                repository.clone(),
                Some(state_base.clone()),
                &source_change.path,
            )
            .await?
        } else {
            DiffContent::empty()
        };
        let source = if source_has_file {
            diff_read_file(
                repository.clone(),
                Some(state_source.clone()),
                &source_change.path,
            )
            .await?
        } else {
            DiffContent::empty()
        };
        let target = if target_has_file {
            diff_read_file(
                repository.clone(),
                state_target.clone(),
                &source_change.path,
            )
            .await?
        } else {
            DiffContent::empty()
        };

        // Binary content: emit a marker instead of three-way merging bytes.
        if base.is_binary() || source.is_binary() || target.is_binary() {
            emit_binary_diff(&source_change.path, LoreFileAction::Keep);
            continue;
        }

        let source_label = format!("source@{}", state_source.revision_number());
        let target_label = if let Some(state_target) = state_target.as_ref() {
            format!("target@{}", state_target.revision_number())
        } else {
            "target".to_string()
        };

        // mine = CLI --source, theirs = CLI --target
        match merge3_text(
            base.text(),
            source.text(),
            target.text(),
            Some(&format!("base@{}", state_base.revision_number())),
            Some(&source_label),
            Some(&target_label),
        ) {
            Ok(merge_result) => {
                // Clean merge — both modified but no overlapping hunks.
                // Diff baseline to merge result to show the target branch's contribution.
                emit_diff_event(
                    source.text(),
                    &merge_result,
                    &source_label,
                    &format!("{target_label} (merged)"),
                    &source_change.path,
                    LoreFileAction::Keep,
                    options,
                );
            }
            Err(conflict_text) => {
                event::LoreEvent::FileDiff(LoreFileDiffEventData {
                    path: source_change.path.clone().into(),
                    patch: conflict_text.into(),
                    action: LoreFileAction::Keep,
                })
                .send();
            }
        }
    }

    Ok(())
}

async fn emit_unified_diffs(
    repository: Arc<RepositoryContext>,
    state_source: &Arc<State>,
    state_target: &Option<Arc<State>>,
    changes: &[NodeChange],
    paths: &[RelativePath],
    options: DiffOptions,
) -> Result<(), DiffError> {
    for change in changes {
        if change.action == change::FileAction::Move
            && let Some(from_path) = change.from_path.as_ref()
        {
            if !move_matches_paths(paths, &change.path, from_path) {
                continue;
            }
            emit_move_diff(
                repository.clone(),
                state_source,
                state_target,
                from_path,
                &change.path,
                options,
            )
            .await?;
            continue;
        }

        let is_from_file = change.from.flags.contains(NodeFlags::File);
        let is_to_file = if state_target.is_some() {
            change.to.flags.contains(NodeFlags::File)
        } else {
            let check_absolute_path = change.path.to_absolute_path(repository.require_path()?);
            tokio::fs::metadata(check_absolute_path)
                .await
                .is_ok_and(|m| m.is_file())
        };

        if !is_from_file && !is_to_file {
            continue;
        }

        if !paths.is_empty() && !paths.contains(&change.path) {
            continue;
        }

        let action = if is_from_file {
            if is_to_file {
                LoreFileAction::Keep
            } else {
                LoreFileAction::Delete
            }
        } else if is_to_file {
            LoreFileAction::Add
        } else {
            continue;
        };

        let source_label = diff_label(&change.path, Some(state_source));
        let target_label = diff_label(&change.path, state_target.as_ref());

        if action == LoreFileAction::Keep {
            let source =
                diff_read_file(repository.clone(), Some(state_source.clone()), &change.path)
                    .await?;
            let target =
                diff_read_file(repository.clone(), state_target.clone(), &change.path).await?;
            if source.is_binary() || target.is_binary() {
                emit_binary_diff(&change.path, action);
                continue;
            }
            emit_diff_event(
                source.text(),
                target.text(),
                &source_label,
                &target_label,
                &change.path,
                action,
                options,
            );
        } else if action == LoreFileAction::Delete {
            let source =
                diff_read_file(repository.clone(), Some(state_source.clone()), &change.path)
                    .await?;
            if source.is_binary() {
                emit_binary_diff(&change.path, action);
                continue;
            }
            emit_diff_event(
                source.text(),
                "",
                &source_label,
                "/dev/null",
                &change.path,
                action,
                options,
            );
        } else if action == LoreFileAction::Add {
            let target =
                diff_read_file(repository.clone(), state_target.clone(), &change.path).await?;
            if target.is_binary() {
                emit_binary_diff(&change.path, action);
                continue;
            }
            emit_diff_event(
                "",
                target.text(),
                "/dev/null",
                change.path.as_str(),
                &change.path,
                action,
                options,
            );
        }
    }

    Ok(())
}

fn diff_label(path: &RelativePath, state: Option<&Arc<State>>) -> String {
    match state {
        Some(state) => format!("{}@{}", path.as_str(), state.revision_number()),
        None => path.as_str().to_string(),
    }
}

fn move_matches_paths(
    paths: &[RelativePath],
    to_path: &RelativePath,
    from_path: &RelativePath,
) -> bool {
    paths.is_empty()
        || paths
            .iter()
            .any(|p| p.is_empty() || p == to_path || p == from_path)
}

/// Emit a `Binary files differ` marker as a `FileDiff` event, bypassing the
/// text diff/merge pipeline. Used when any participating side of a diff is
/// detected as binary, so raw bytes are never rendered through the lossy text
/// decoder (which would produce U+FFFD replacement characters).
fn emit_binary_diff(path: &RelativePath, action: LoreFileAction) {
    event::LoreEvent::FileDiff(LoreFileDiffEventData {
        path: path.clone().into(),
        patch: "Binary files differ\n".into(),
        action,
    })
    .send();
}

/// Format a unified diff with proper labels and emit as a `FileDiff` event.
/// Returns `true` if a non-empty patch was emitted, `false` if skipped.
///
/// When `ignore_whitespace_eol` or `ignore_whitespace_inline` is set, the diff
/// is computed against a per-line normalised view of the content, but the
/// emitted patch text shows the original (un-normalised) line content. This
/// mirrors git's `--ignore-space-at-eol` / `--ignore-space-change` behaviour.
fn emit_diff_event(
    old: &str,
    new: &str,
    from_label: &str,
    to_label: &str,
    path: &RelativePath,
    action: LoreFileAction,
    options: DiffOptions,
) -> bool {
    let Some(patch) = build_unified_patch(old, new, from_label, to_label, options) else {
        return false;
    };
    event::LoreEvent::FileDiff(LoreFileDiffEventData {
        path: path.clone().into(),
        patch: patch.into(),
        action,
    })
    .send();
    true
}

fn build_unified_patch(
    old: &str,
    new: &str,
    from_label: &str,
    to_label: &str,
    options: DiffOptions,
) -> Option<String> {
    let patch = if options.ignore_whitespace_eol || options.ignore_whitespace_inline {
        format_patch_preserving_originals(
            old,
            new,
            options.context_lines,
            options.ignore_whitespace_eol,
            options.ignore_whitespace_inline,
        )?
    } else {
        // diffy's `Display`/`to_string()` defaults to `suppress_blank_empty: true`,
        // which drops the leading space on blank context lines (bare `\n`). Standard
        // unified-diff parsers require every hunk-body line to start with a sentinel
        // (' ', '+', '-', '\'), so format explicitly with suppression disabled.
        let patch = diffy::DiffOptions::new()
            .set_context_len(options.context_lines as usize)
            .create_patch(old, new);
        let s = diffy::PatchFormatter::new()
            .suppress_blank_empty(false)
            .fmt_patch(&patch)
            .to_string();
        if s.ends_with("+++ modified\n") {
            return None;
        }
        s
    };
    let patch = patch.replace("--- original", &format!("--- {from_label}"));
    let patch = patch.replace("+++ modified", &format!("+++ {to_label}"));
    Some(patch)
}

async fn emit_move_diff(
    repository: Arc<RepositoryContext>,
    state_source: &Arc<State>,
    state_target: &Option<Arc<State>>,
    from_path: &RelativePath,
    to_path: &RelativePath,
    options: DiffOptions,
) -> Result<(), DiffError> {
    let source = diff_read_file(repository.clone(), Some(state_source.clone()), from_path).await?;
    let target = diff_read_file(repository.clone(), state_target.clone(), to_path).await?;

    let header = format!(
        "move from {}\nmove to {}\n",
        from_path.as_str(),
        to_path.as_str()
    );

    let patch = if source.is_binary() || target.is_binary() {
        if source.text() != target.text() {
            format!("{header}Binary files differ\n")
        } else {
            header
        }
    } else {
        let from_label = diff_label(from_path, Some(state_source));
        let to_label = diff_label(to_path, state_target.as_ref());
        match build_unified_patch(
            source.text(),
            target.text(),
            &from_label,
            &to_label,
            options,
        ) {
            Some(body) => format!("{header}{body}"),
            None => header,
        }
    };

    event::LoreEvent::FileDiff(LoreFileDiffEventData {
        path: to_path.clone().into(),
        patch: patch.into(),
        action: LoreFileAction::Move,
    })
    .send();
    Ok(())
}

/// Normalise `old` and `new` per-line for comparison, run diffy, then re-emit
/// the unified diff with original (un-normalised) line content. Returns
/// `None` when no hunks remain after normalisation (i.e. the files are equal
/// under the selected whitespace rules).
///
/// The line count of each normalised side equals the line count of the
/// original side, so diffy's 1-based hunk line numbers index back into the
/// original line arrays correctly.
fn format_patch_preserving_originals(
    old: &str,
    new: &str,
    context_lines: u32,
    ignore_eol: bool,
    ignore_inline: bool,
) -> Option<String> {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    let old_norm: String = old_lines
        .iter()
        .map(|l| normalise_line(l, ignore_eol, ignore_inline))
        .collect();
    let new_norm: String = new_lines
        .iter()
        .map(|l| normalise_line(l, ignore_eol, ignore_inline))
        .collect();

    let patch = diffy::DiffOptions::new()
        .set_context_len(context_lines as usize)
        .create_patch(&old_norm, &new_norm);

    if patch.hunks().is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("--- original\n");
    out.push_str("+++ modified\n");

    for hunk in patch.hunks() {
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            hunk.old_range(),
            hunk.new_range()
        ));
        let mut old_idx = hunk.old_range().start();
        let mut new_idx = hunk.new_range().start();

        for line in hunk.lines() {
            match line {
                diffy::Line::Context(_) => {
                    let orig = old_lines
                        .get(old_idx.saturating_sub(1))
                        .copied()
                        .unwrap_or("");
                    write_patch_line(&mut out, ' ', orig);
                    old_idx += 1;
                    new_idx += 1;
                }
                diffy::Line::Delete(_) => {
                    let orig = old_lines
                        .get(old_idx.saturating_sub(1))
                        .copied()
                        .unwrap_or("");
                    write_patch_line(&mut out, '-', orig);
                    old_idx += 1;
                }
                diffy::Line::Insert(_) => {
                    let orig = new_lines
                        .get(new_idx.saturating_sub(1))
                        .copied()
                        .unwrap_or("");
                    write_patch_line(&mut out, '+', orig);
                    new_idx += 1;
                }
            }
        }
    }

    Some(out)
}

/// Per-line normalisation. Keeps the trailing `\n` (if present) so the line
/// count is preserved between original and normalised content. `\r` is
/// treated as whitespace so the EOL/inline rules apply uniformly to LF and
/// CRLF inputs.
fn normalise_line(line: &str, ignore_eol: bool, ignore_inline: bool) -> String {
    let (content, terminator) = match line.strip_suffix('\n') {
        Some(rest) => (rest, "\n"),
        None => (line, ""),
    };

    let mut work = if ignore_inline {
        collapse_inline_whitespace(content)
    } else {
        content.to_string()
    };

    if ignore_eol {
        let trimmed_len = work.trim_end_matches([' ', '\t', '\r']).len();
        work.truncate(trimmed_len);
    }

    work.push_str(terminator);
    work
}

/// Collapses runs of ASCII space/tab/CR to a single space. Does not invent
/// whitespace where there was none, and does not touch newline characters
/// (callers strip the terminator before invoking). Folding `\r` in keeps
/// LF and CRLF line endings on equal footing for inline comparison.
fn collapse_inline_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' || c == '\r' {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// Writes one unified-diff line. Every hunk-body line begins with its sentinel
/// (' ', '+', '-') — including blank context lines, which are emitted as `" \n"`
/// so standard unified-diff parsers count them as rows. Lines without a trailing
/// `\n` get a `\ No newline at end of file` marker.
fn write_patch_line(out: &mut String, sign: char, line: &str) {
    out.push(sign);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push('\n');
        out.push_str("\\ No newline at end of file\n");
    }
}

/// One side of a diff: either decoded display text, or a marker that the raw
/// bytes were detected as binary (non-text) content. Binary content carries no
/// text — it is never rendered through the diff/merge pipeline.
enum DiffContent {
    Text(String),
    Binary,
}

impl DiffContent {
    /// An absent side (file missing on this revision / `/dev/null`). Treated as
    /// empty text, never binary.
    fn empty() -> Self {
        DiffContent::Text(String::new())
    }

    fn is_binary(&self) -> bool {
        matches!(self, DiffContent::Binary)
    }

    /// The decoded text for a text side; `""` for binary. Callers short-circuit
    /// on `is_binary()` before reaching this, so the binary case is never read
    /// in practice.
    fn text(&self) -> &str {
        match self {
            DiffContent::Text(s) => s,
            DiffContent::Binary => "",
        }
    }
}

/// Build display content from raw bytes.
///
/// An empty buffer (an absent side) is text, not binary. UTF-16 BOM input is
/// exempt from the binary check: `decode_text_for_display` renders it as
/// readable text, and the diff path intentionally shows UTF-16 as text — unlike
/// the merge path, where `infer_is_diffable_by_slice` treats UTF-16 as binary
/// to preserve bytes. Everything else (null bytes, non-text MIME, Unreal
/// packages, invalid UTF-8) is classified as binary, and its bytes are never
/// decoded.
fn make_diff_content(bytes: &[u8]) -> DiffContent {
    if !bytes.is_empty() && !is_utf16_bom(bytes) && !infer_is_diffable_by_slice(bytes) {
        DiffContent::Binary
    } else {
        DiffContent::Text(decode_text_for_display(bytes))
    }
}

async fn diff_read_file(
    repository: Arc<RepositoryContext>,
    state: Option<Arc<State>>,
    relative_path: &RelativePath,
) -> Result<DiffContent, DiffError> {
    let Some(state) = state else {
        let path = relative_path.to_absolute_path(repository.require_path()?);
        let content = tokio::fs::read(path.as_path())
            .await
            .internal(&format!("Failed reading file for diff: {}", path.display()))?;
        return Ok(make_diff_content(&content));
    };

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_err(|_err| {
            DiffError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;

    let (repository, state) = if node_link.repository != repository.id {
        let repository = Arc::new(repository.to_link_context(node_link.repository).await);
        let state = state::State::deserialize(repository.clone(), node_link.revision)
            .await
            .forward::<DiffError>("Failed deserializing revision state")?;
        (repository, state)
    } else {
        (repository, state)
    };

    let Ok(node) = state.node(repository.clone(), node_link.node).await else {
        return Err(DiffError::from(FileNotFound {
            resource: relative_path.to_string(),
        }));
    };

    if !node.is_file() {
        return Err(DiffError::internal(format!(
            "The given path is not a file: {relative_path}"
        )));
    }

    let content = immutable::read(
        repository.clone(),
        node.address,
        None,
        read_options_from_repository(&repository)
            .with_decompress()
            .with_verify()
            .with_remote(),
    )
    .await
    .forward::<DiffError>("Failed reading data for diff")?;

    Ok(make_diff_content(&content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_diff_content_empty_is_text() {
        let c = make_diff_content(b"");
        assert!(!c.is_binary());
        assert_eq!(c.text(), "");
    }

    #[test]
    fn make_diff_content_valid_utf8_is_text() {
        let c = make_diff_content(b"hello\nworld\n");
        assert!(!c.is_binary());
        assert_eq!(c.text(), "hello\nworld\n");
    }

    #[test]
    fn make_diff_content_utf16_le_bom_is_text() {
        // UTF-16 BOM is exempt: the diff path renders it as readable text,
        // matching the test_file_diff_utf16be smoke test.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("Hi\n".encode_utf16().flat_map(u16::to_le_bytes));
        let c = make_diff_content(&bytes);
        assert!(!c.is_binary(), "UTF-16 LE BOM must remain diffable as text");
        assert_eq!(c.text(), "Hi\n");
    }

    #[test]
    fn make_diff_content_null_bytes_is_binary() {
        let c = make_diff_content(&[0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00]);
        assert!(c.is_binary());
    }

    #[test]
    fn make_diff_content_invalid_utf8_is_binary() {
        let c = make_diff_content(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(c.is_binary());
    }

    #[test]
    fn diff_content_empty_constructor_is_text() {
        let c = DiffContent::empty();
        assert!(!c.is_binary());
        assert_eq!(c.text(), "");
    }

    #[test]
    fn normalise_line_strips_trailing_whitespace() {
        assert_eq!(normalise_line("foo   \n", true, false), "foo\n");
        assert_eq!(normalise_line("foo\t\t\n", true, false), "foo\n");
        assert_eq!(normalise_line("foo", true, false), "foo");
        assert_eq!(normalise_line("foo   ", true, false), "foo");
    }

    #[test]
    fn normalise_line_collapses_runs() {
        assert_eq!(normalise_line("a  b   c\n", false, true), "a b c\n");
        assert_eq!(normalise_line("a\t\tb\n", false, true), "a b\n");
        // Internal whitespace gone entirely is NOT invented back.
        assert_eq!(normalise_line("abc\n", false, true), "abc\n");
    }

    #[test]
    fn normalise_line_both_flags() {
        assert_eq!(normalise_line("a  b   \n", true, true), "a b\n");
    }

    #[test]
    fn normalise_line_crlf_trailing_treated_as_whitespace() {
        // CRLF inputs (e.g. Python text-mode writes on Windows) must compare equal
        // to LF inputs under ignore_eol — the trailing `\r` counts as EOL whitespace.
        assert_eq!(normalise_line("foo   \r\n", true, false), "foo\n");
        assert_eq!(normalise_line("foo\r\n", true, false), "foo\n");
        // No-flag path keeps the `\r` intact.
        assert_eq!(normalise_line("foo\r\n", false, false), "foo\r\n");
    }

    #[test]
    fn ignore_eol_trailing_space_no_diff() {
        let old = "foo\nbar\n";
        let new = "foo  \nbar\n";
        // With the flag on, no hunks should be produced.
        assert!(format_patch_preserving_originals(old, new, 3, true, false).is_none());
    }

    #[test]
    fn ignore_eol_preserves_originals_in_real_change() {
        // Two lines: line 1 has trailing-whitespace-only diff, line 2 has a real diff.
        let old = "foo  \nbar\nbaz\n";
        let new = "foo  \nBAR\nbaz\n";
        let out = format_patch_preserving_originals(old, new, 3, true, false)
            .expect("real change should produce a hunk");
        // The unchanged "foo  " line must keep its trailing whitespace in the options.
        assert!(
            out.contains(" foo  \n"),
            "context line should show original whitespace:\n{out}"
        );
        assert!(out.contains("-bar\n"));
        assert!(out.contains("+BAR\n"));
    }

    #[test]
    fn ignore_inline_collapses_runs_no_diff() {
        let old = "a b c\n";
        let new = "a  b   c\n";
        assert!(format_patch_preserving_originals(old, new, 3, false, true).is_none());
    }

    #[test]
    fn ignore_inline_does_not_invent_whitespace() {
        // "abc" → "a bc" introduces whitespace where none existed; must still diff.
        let old = "abc\n";
        let new = "a bc\n";
        let out = format_patch_preserving_originals(old, new, 3, false, true)
            .expect("introducing whitespace must still register as a change");
        assert!(out.contains("-abc\n"));
        assert!(out.contains("+a bc\n"));
    }

    #[test]
    fn both_flags_combined_suppress_all_whitespace_only_diffs() {
        let old = "foo\nbar  baz\n";
        let new = "foo   \nbar baz\n";
        assert!(format_patch_preserving_originals(old, new, 3, true, true).is_none());
    }

    #[test]
    fn context_lines_respected_with_flags() {
        let old = "a\nb\nc\nx\ne\nf\ng\n";
        let new = "a\nb\nc\nX\ne\nf\ng\n";
        let out = format_patch_preserving_originals(old, new, 0, true, false)
            .expect("real change should produce a hunk");
        // context=0 means no surrounding lines in the hunk.
        assert!(out.contains("@@ -4 +4 @@\n"), "got:\n{out}");
        assert!(out.contains("-x\n"));
        assert!(out.contains("+X\n"));
        // No surrounding context lines.
        assert!(!out.contains(" c\n"));
        assert!(!out.contains(" e\n"));
    }

    #[test]
    fn missing_newline_marker_when_input_lacks_terminator() {
        let old = "foo";
        let new = "bar";
        let out = format_patch_preserving_originals(old, new, 3, true, false)
            .expect("differing single-line files should diff");
        assert!(
            out.contains("\\ No newline at end of file\n"),
            "expected no-newline marker, got:\n{out}"
        );
    }
}
