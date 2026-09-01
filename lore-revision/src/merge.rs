// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;

use lore_base::lore_spawn;

use crate::repository::RepositoryWriteToken;
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

pub async fn merge3_text_by_pathbuf(
    base: PathBuf,
    mine: PathBuf,
    theirs: PathBuf,
    result: PathBuf,
    mode: MergeTextMode<'_>,
) -> std::io::Result<bool> {
    let base_buffer =
        lore_spawn!(async move { lore_io::IoDriver::global().read_file_bytes(base).await });
    let mine_buffer =
        lore_spawn!(async move { lore_io::IoDriver::global().read_file_bytes(mine).await });
    let theirs_buffer =
        lore_spawn!(async move { lore_io::IoDriver::global().read_file_bytes(theirs).await });

    let base_buffer = base_buffer.await;
    let mine_buffer = mine_buffer.await;
    let theirs_buffer = theirs_buffer.await;

    let base_buffer = base_buffer.map_err(std::io::Error::other)??;
    let mine_buffer = mine_buffer.map_err(std::io::Error::other)??;
    let theirs_buffer = theirs_buffer.map_err(std::io::Error::other)??;

    let base_string = String::from_utf8_lossy(&base_buffer).into_owned();
    let mine_string = String::from_utf8_lossy(&mine_buffer).into_owned();
    let theirs_string = String::from_utf8_lossy(&theirs_buffer).into_owned();

    let merge_result = merge3_text(&base_string, &mine_string, &theirs_string, None, None, None);
    let merge_conflicts = merge_result.is_err();

    if let MergeTextMode::Write(_token) = mode {
        let merge_output = match merge_result {
            Err(str) | Ok(str) => str,
        };
        lore_io::IoDriver::global()
            .write_file_bytes(result, bytes::Bytes::from(merge_output), false)
            .await?;
    }

    Ok(merge_conflicts)
}

pub async fn merge3_text_by_path(
    repository_path: impl AsRef<Path>,
    base: &RelativePath,
    mine: &RelativePath,
    theirs: &RelativePath,
    result: &RelativePath,
    mode: MergeTextMode<'_>,
) -> std::io::Result<bool> {
    let repository_path = repository_path.as_ref();
    let base = base.to_absolute_path(repository_path);
    let mine = mine.to_absolute_path(repository_path);
    let theirs = theirs.to_absolute_path(repository_path);

    let base_buffer =
        lore_spawn!(async move { lore_io::IoDriver::global().read_file_bytes(base).await });
    let mine_buffer =
        lore_spawn!(async move { lore_io::IoDriver::global().read_file_bytes(mine).await });
    let theirs_buffer =
        lore_spawn!(async move { lore_io::IoDriver::global().read_file_bytes(theirs).await });

    let base_buffer = base_buffer.await;
    let mine_buffer = mine_buffer.await;
    let theirs_buffer = theirs_buffer.await;

    let base_buffer = base_buffer.map_err(std::io::Error::other)??;
    let mine_buffer = mine_buffer.map_err(std::io::Error::other)??;
    let theirs_buffer = theirs_buffer.map_err(std::io::Error::other)??;

    let base_string = String::from_utf8_lossy(&base_buffer).into_owned();
    let mine_string = String::from_utf8_lossy(&mine_buffer).into_owned();
    let theirs_string = String::from_utf8_lossy(&theirs_buffer).into_owned();

    let merge_result = merge3_text(&base_string, &mine_string, &theirs_string, None, None, None);
    let merge_conflicts = merge_result.is_err();

    if let MergeTextMode::Write(_token) = mode {
        let result = result.to_absolute_path(repository_path);
        let merge_output = match merge_result {
            Err(str) | Ok(str) => str,
        };
        lore_io::IoDriver::global()
            .write_file_bytes(result, bytes::Bytes::from(merge_output), false)
            .await?;
    }

    Ok(merge_conflicts)
}
