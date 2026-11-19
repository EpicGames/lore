// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Scratch space for tests that need a file system Lore has verified as local and durable.

use std::path::Path;
use std::path::PathBuf;
#[cfg(target_family = "unix")]
use std::sync::OnceLock;

/// The name of the scratch directory made beside the outermost Lore working tree that holds the
/// build's temp directory.
const BESIDE_TREE: &str = "lore-test-scratch";

/// The scratch directory beside a working tree that this process chose, removed as it exits.
#[cfg(target_family = "unix")]
static MADE_BESIDE_TREE: OnceLock<PathBuf> = OnceLock::new();

/// The directory a test makes its scratch directories in where it needs a file system that
/// [`crate::clone::locality_of`] reads as local, and no Lore working tree above it, as a move of
/// the external root does: the first of the system temp directory, then `fallback`, the build's
/// own temp directory, then a directory beside the outermost working tree that holds `fallback`,
/// in that tree's parent. The last is made where needed and, on Unix, removed once empty as the
/// process exits. Panics, naming each candidate and why it was refused, where none qualifies.
pub fn verified_scratch_base(fallback: &Path) -> PathBuf {
    let mut refused = Vec::new();
    let beside = outermost_tree(fallback)
        .and_then(|tree| tree.parent().map(|parent| parent.join(BESIDE_TREE)));
    let candidates = [
        Some(std::env::temp_dir()),
        Some(fallback.to_path_buf()),
        beside,
    ];
    for (index, candidate) in candidates.into_iter().flatten().enumerate() {
        match refusal(&candidate) {
            None => {
                if index == 2 {
                    remove_at_exit(&candidate);
                }
                return candidate;
            }
            Some(why) => refused.push(format!("{} ({why})", candidate.display())),
        }
    }
    panic!(
        "no scratch directory is on a file system Lore has verified as local and durable, outside \
         any Lore working tree: {}",
        refused.join(", ")
    );
}

/// Why `candidate` can't hold scratch directories for a move, made where it is missing: its file
/// system, or the working tree it lies in. `None` where it can.
fn refusal(candidate: &Path) -> Option<String> {
    if let Some(tree) = outermost_tree(candidate) {
        return Some(format!(
            "inside the Lore working tree at {}",
            tree.display()
        ));
    }
    if let Err(err) = std::fs::create_dir_all(candidate) {
        return Some(err.to_string());
    }
    match (
        crate::clone::volume_of(candidate),
        crate::clone::locality_of(candidate),
    ) {
        (Ok(_), Ok(locality)) if locality.local => None,
        (Ok(volume), _) => Some(volume.filesystem),
        (Err(err), _) => Some(err.to_string()),
    }
}

/// The outermost directory at or above `path` that holds a `.lore` directory, if any.
fn outermost_tree(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .filter(|ancestor| ancestor.join(".lore").is_dir())
        .last()
        .map(Path::to_path_buf)
}

/// Removes `made`, once empty, as the process exits.
#[cfg(target_family = "unix")]
fn remove_at_exit(made: &Path) {
    extern "C" fn remove() {
        if let Some(made) = MADE_BESIDE_TREE.get() {
            let _ = std::fs::remove_dir(made);
        }
    }
    if MADE_BESIDE_TREE.set(made.to_path_buf()).is_ok() {
        // SAFETY: `remove` is an `extern "C"` function that takes nothing, as `atexit` asks.
        unsafe { libc::atexit(remove) };
    }
}

/// Leaves `made` in place: only Unix removes it as the process exits.
#[cfg(not(target_family = "unix"))]
fn remove_at_exit(_made: &Path) {}
