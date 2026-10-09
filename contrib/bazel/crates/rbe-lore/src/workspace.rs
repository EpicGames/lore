// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Per-file metadata on a Lore workspace: the SHA-256 that maps a file's Lore content onto the
//! CAS key bazel asks for.
//!
//! Lore addresses content by BLAKE3 and keeps no SHA-256, which is the REAPI's digest. Recording
//! it once at commit, as metadata on each file, means nothing downstream has to read a file to
//! learn its CAS key: not the indexer that publishes it, and not bazel, which can take it from an
//! extended attribute set from the same value.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use anyhow::bail;
use lore::file::LoreFileMetadataListArgs;
use lore::file::LoreFileMetadataSetArgs;
use lore::repository::LoreRepositoryStatusArgs;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreFileAction;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreMetadata;
use lore_revision::interface::LoreMetadataType;
use lore_revision::interface::LoreNodeType;
use lore_revision::interface::LoreString;

/// The metadata key. Its value is the SHA-256 of the file's content as 64 lowercase hex
/// characters, the same spelling the REAPI uses for a digest's hash.
pub const SHA256_KEY: &str = "sha256";

/// The size, in decimal, of the content `sha256` was computed from.
///
/// Lore keeps a file's metadata when its content changes, so a commit that changed a file without
/// re-stamping it would carry the old `sha256` forward, naming the wrong content. Comparing this
/// size against the file catches that whenever the size changed too. A same-size edit still gets
/// through; closing that needs the content address the hash was taken from, which only the
/// revision tree API exposes (`SOURCES.md` §3).
pub const SIZE_KEY: &str = "sha256-size";

/// What a file's metadata says about its SHA-256.
#[derive(Debug, PartialEq, Eq)]
pub enum Recorded {
    /// A SHA-256 recorded for content of the file's current size.
    Valid(String),
    /// No SHA-256 recorded.
    Missing,
    /// A SHA-256 recorded for content of a different size: the file changed without being
    /// re-stamped.
    Stale,
}

/// Paths per `metadata_set` call. One call per file would cost a repository open each; one call
/// for a whole tree would hold every path, key and value in a single argument block.
const MAX_PATHS_PER_CALL: usize = 1024;

/// Metadata lives in the workspace's own revision state, so none of this needs the remote.
fn workspace_globals(workspace: &Path) -> LoreGlobalArgs {
    LoreGlobalArgs {
        repository_path: workspace.to_string_lossy().as_ref().into(),
        offline: 1,
        ..Default::default()
    }
}

#[lore_macro::test_pub]
fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The files staged for the next commit that still exist: added, modified, moved or copied, but
/// not deleted. These are exactly the files whose content a commit records, and so the ones that
/// must be stamped before it.
pub async fn staged_files(workspace: &Path) -> Result<Vec<PathBuf>> {
    let found: Arc<Mutex<Vec<PathBuf>>> = Default::default();
    let f = found.clone();
    let root = workspace.to_path_buf();
    let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| {
        if let LoreEvent::RepositoryStatusFile(d) = ev
            && d.flag_staged != 0
            && d.r#type == LoreNodeType::File
            && d.action != LoreFileAction::Delete
        {
            f.lock().unwrap().push(root.join(d.path.as_str()));
        }
    }));
    let args = LoreRepositoryStatusArgs {
        staged: 1,
        scan: 0,
        check_dirty: 0,
        reset: 0,
        sync_point: 0,
        revision_only: 0,
        count: 0,
        paths: LoreArray::default(),
    };
    let status = lore::repository::status(workspace_globals(workspace), args, cb).await;
    if status != 0 {
        bail!(
            "lore status failed in {} (status={status})",
            workspace.display()
        );
    }
    let mut files = std::mem::take(&mut *found.lock().unwrap());
    files.sort();
    Ok(files)
}

/// Record `sha256` and the size it was taken from on files in `workspace`, given as
/// `(path, sha256 hex, size)`. Lore only accepts metadata on a staged file, so this runs between
/// staging and committing, and the commit then carries it.
pub async fn set_sha256(workspace: &Path, entries: &[(PathBuf, String, u64)]) -> Result<()> {
    for (path, hex, _) in entries {
        if !is_sha256_hex(hex) {
            bail!("{}: not a SHA-256: {hex:?}", path.display());
        }
    }
    for chunk in entries.chunks(MAX_PATHS_PER_CALL) {
        let n = chunk.len();
        let args = LoreFileMetadataSetArgs {
            paths: LoreArray::from_vec(
                chunk
                    .iter()
                    .map(|(path, _, _)| path.to_string_lossy().as_ref().into())
                    .collect(),
            ),
            keys: LoreArray::from_vec(
                (0..n)
                    .flat_map(|_| [LoreString::from(SHA256_KEY), LoreString::from(SIZE_KEY)])
                    .collect(),
            ),
            values: LoreArray::from_vec(
                chunk
                    .iter()
                    .flat_map(|(_, hex, size)| {
                        [
                            LoreString::from(hex.as_str()),
                            LoreString::from(size.to_string().as_str()),
                        ]
                    })
                    .collect(),
            ),
            formats: LoreArray::from_vec(vec![LoreMetadataType::String; 2 * n]),
            entries: LoreArray::from_vec(vec![2u32; n]),
        };
        let status = lore::file::metadata_set(workspace_globals(workspace), args, None).await;
        if status != 0 {
            bail!(
                "lore file metadata set failed for {n} files under {} (status={status}); \
                 are they staged?",
                workspace.display()
            );
        }
    }
    Ok(())
}

/// What `path`'s metadata at the workspace's current revision says about its SHA-256, checked
/// against `size`, the size of the file as it is now. A `sha256` that is present but malformed is
/// an error: publishing under it would name the wrong content.
pub async fn get_sha256(workspace: &Path, path: &Path, size: u64) -> Result<Recorded> {
    let found: Arc<Mutex<HashMap<String, String>>> = Default::default();
    let f = found.clone();
    let cb: LoreEventCallback = Some(Box::new(move |ev: &LoreEvent| {
        if let LoreEvent::Metadata(d) = ev
            && let LoreMetadata::String(value) = &d.value
        {
            f.lock()
                .unwrap()
                .insert(d.key.as_str().to_string(), value.as_str().to_string());
        }
    }));
    let args = LoreFileMetadataListArgs {
        path: path.to_string_lossy().as_ref().into(),
        revision: LoreString::default(),
    };
    // A file without metadata may be reported as a failed call rather than an empty list, so the
    // status cannot tell "absent" from "broken". Either way the caller falls back to hashing,
    // which is slower but never wrong.
    let _status = lore::file::metadata_list(workspace_globals(workspace), args, cb).await;
    let found = std::mem::take(&mut *found.lock().unwrap());
    classify(path, found.get(SHA256_KEY), found.get(SIZE_KEY), size)
}

#[lore_macro::test_pub]
fn classify(
    path: &Path,
    hex: Option<&String>,
    recorded_size: Option<&String>,
    size: u64,
) -> Result<Recorded> {
    let Some(hex) = hex else {
        return Ok(Recorded::Missing);
    };
    if !is_sha256_hex(hex) {
        bail!("{}: {SHA256_KEY} is not a SHA-256: {hex:?}", path.display());
    }
    // Without the size there is nothing to check the hash against, so it cannot be trusted.
    match recorded_size.and_then(|s| s.parse::<u64>().ok()) {
        Some(recorded) if recorded == size => Ok(Recorded::Valid(hex.clone())),
        _ => Ok(Recorded::Stale),
    }
}

/// Every file Lore would have committed below `root`: regular files, skipping symlinks (Lore does
/// not commit them) and the workspace's own `.lore` directory. Sorted, so runs are comparable.
pub fn checkout_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, root_lore: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if path != root_lore {
                    walk(&path, root_lore, out)?;
                }
            } else if kind.is_file() {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, &root.join(".lore"), &mut out)?;
    out.sort();
    Ok(out)
}
