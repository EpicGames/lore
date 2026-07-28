// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use lore_error_set::prelude::*;

use crate::error::LoreResultExt;
use crate::errors::InvalidPath;

#[error_set]
pub enum PathError {
    InvalidPath,
}

/// Resolves `path` against the working directory of the call in progress, or
/// against this process's own when the call did not name one.
///
/// A call arriving over IPC carries the directory of the process that made it,
/// because the service's own is unrelated to the caller's.
pub fn make_absolute(path: impl AsRef<str>) -> Result<PathBuf, PathError> {
    let context = crate::runtime::try_execution_context();
    let base = context
        .as_ref()
        .and_then(|context| context.globals().working_directory())
        .map(Path::new);
    make_absolute_from(path, base)
}

/// [`make_absolute`] with the base directory supplied by the caller, for the
/// call wrappers that resolve paths before the execution context exists and so
/// cannot look it up.
pub fn make_absolute_from(
    path: impl AsRef<str>,
    base: Option<&Path>,
) -> Result<PathBuf, PathError> {
    let path = path.as_ref();
    let cleanpath = clean(path.to_owned());
    let pathbuf = PathBuf::from_str(cleanpath.as_str()).emit_map_err(InvalidPath {
        path: path.to_string(),
    })?;
    if pathbuf.is_absolute() {
        return Ok(pathbuf);
    }
    match base {
        Some(base) => Ok(base.join(pathbuf)),
        None => Ok(std::env::current_dir()
            .emit_map_err(PathError::internal(
                "failed to get current working directory",
            ))?
            .join(pathbuf)),
    }
}

/// Returns `true` when `candidate` resolves to a location inside
/// `repository_path`, `false` only when it is confidently outside.
///
/// Wraps the canonical [`RelativePath::new_from_user_path`] check used at ~30
/// call sites for input-path validation. Internal errors (e.g. failed
/// `current_dir` resolution) are treated as "inside" — callers use this to
/// pick between read- and write-dispatching, so over-classifying as inside
/// keeps the safe (write) default.
pub fn is_path_inside_repository(repository_path: &Path, candidate: &str) -> bool {
    !matches!(
        RelativePath::new_from_user_path(repository_path, candidate),
        Err(PathError::InvalidPath(_)),
    )
}

pub fn clean(path: String) -> String {
    // Remove verbatim path and device path prefixes
    let mut path = path.replace("\\\\?\\", "").replace("\\\\.\\", "");

    // Convert to forward slashes and remove multiple consecutive slashes
    path = path.replace('\\', "/").replace("//", "/");

    // Remove any /./
    path = path.replace("/./", "/");

    // Remove any leading ./
    if path.starts_with("./") {
        path = path.trim_start_matches("./").to_owned();
    }

    // Remove any trailing /.
    if path.ends_with("/.") {
        path = path.trim_end_matches("/.").to_owned();
    }

    // Reduce any ..
    if path.contains("..") {
        let elements: Vec<&str> = path.split('/').collect();
        let mut remain: Vec<&str> = Vec::with_capacity(elements.len());
        for element in elements {
            if element.is_empty() {
                continue;
            }
            if element == ".." && !remain.is_empty() {
                #[cfg(target_family = "windows")]
                if remain.len() == 1
                    && let Some(first) = remain.last()
                    && first.len() == 2
                    && first.chars().nth(1).unwrap_or_default() == ':'
                {
                    // Keep the drive letter
                    continue;
                }
                if let Some(last) = remain.last()
                    && *last == ".."
                {
                    // Stepping up a directory shouldn't cancel out another ".."
                    remain.push(element);
                    continue;
                }

                remain.pop();
                continue;
            }
            remain.push(element);
        }

        let mut reduced_path = remain.join("/");
        if path.starts_with('/') {
            // Remove any leading ../
            reduced_path = reduced_path.trim_start_matches("../").to_owned();
            reduced_path.insert(0, '/');
        }

        path = reduced_path;
    }

    path
}

// ============================================================================
// Shared helper functions for RelativePath and RelativePathBuf
// These operate on &str to avoid code duplication between the two types.
// ============================================================================

/// Returns the last path component (after the last `/`).
fn name_impl(path: &str) -> &str {
    if !path.is_empty() {
        if let Some(sep) = path.rfind('/') {
            &path[(sep + 1)..]
        } else {
            path
        }
    } else {
        ""
    }
}

/// Returns the first path component (before the first `/`).
fn root_impl(path: &str) -> &str {
    if let Some(sep) = path.find('/') {
        &path[..sep]
    } else {
        path
    }
}

/// Returns everything except the last component, or None if the path has no parent.
fn parent_impl(path: &str) -> Option<&str> {
    if !path.is_empty()
        && let Some(sep) = path.rfind('/')
    {
        return Some(&path[..sep]);
    }
    None
}

/// Checks if two paths overlap (one is a prefix of or equal to the other).
fn overlaps_impl(lhs: &str, rhs: &str) -> bool {
    if lhs.is_empty() || rhs.is_empty() {
        return true;
    }

    let shortest = std::cmp::min(lhs.len(), rhs.len());

    lhs.is_char_boundary(shortest)
        && rhs.is_char_boundary(shortest)
        && lhs[..shortest] == rhs[..shortest]
        && ((lhs.len() > shortest && lhs.as_bytes()[shortest] == b'/')
            || (rhs.len() > shortest && rhs.as_bytes()[shortest] == b'/')
            || (lhs.len() == rhs.len()))
}

/// Shared data wrapped in Arc for cheap cloning
pub struct RelativePathData {
    path: String,
    path_lower: String,
}

/// The COW path type with offset-based views
#[derive(Clone)]
pub struct RelativePath {
    data: Arc<RelativePathData>,
    start: usize,       // View start offset into path
    end: usize,         // View end offset into path
    start_lower: usize, // View start offset into path_lower
    end_lower: usize,   // View end offset into path_lower
}

impl RelativePath {
    pub fn new() -> Self {
        RelativePath {
            data: Arc::new(RelativePathData {
                path: String::new(),
                path_lower: String::new(),
            }),
            start: 0,
            end: 0,
            start_lower: 0,
            end_lower: 0,
        }
    }

    pub fn pop(&mut self) -> &mut Self {
        if !self.is_empty() {
            let view = &self.data.path[self.start..self.end];
            if let Some(sep) = view.rfind('/') {
                // Adjust end to remove the last component
                self.end = self.start + sep;

                // Adjust end_lower similarly
                let view_lower = &self.data.path_lower[self.start_lower..self.end_lower];
                if let Some(sep_lower) = view_lower.rfind('/') {
                    self.end_lower = self.start_lower + sep_lower;
                }
            } else {
                // No separator found, clear the view
                self.end = self.start;
                self.end_lower = self.start_lower;
            }
        }
        self
    }

    pub fn pop_root(&mut self) -> &str {
        let view = &self.data.path[self.start..self.end];
        let root = if let Some(sep) = view.find('/') {
            &self.data.path[self.start..(self.start + sep)]
        } else {
            &self.data.path[self.start..self.end]
        };

        let root_len = root.len();
        if root_len + 1 < self.len() {
            self.start = self.start + root_len + 1;

            let view_lower = &self.data.path_lower[self.start_lower..self.end_lower];
            if let Some(sep) = view_lower.find('/') {
                self.start_lower = self.start_lower + sep + 1;
            }
        } else {
            self.start = self.end;
            self.start_lower = self.end_lower;
        }
        root
    }

    pub fn name(&self) -> &str {
        name_impl(self.as_str())
    }

    pub fn name_lowercase(&self) -> &str {
        name_impl(self.as_lowercase_str())
    }

    pub fn root(&self) -> &str {
        root_impl(self.as_str())
    }

    pub fn parent(&self) -> Option<&str> {
        parent_impl(self.as_str())
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    pub fn as_str(&self) -> &str {
        &self.data.path[self.start..self.end]
    }

    pub fn as_lowercase_str(&self) -> &str {
        &self.data.path_lower[self.start_lower..self.end_lower]
    }

    pub fn new_from_initial_path(name: impl AsRef<str>) -> Result<RelativePath, PathError> {
        RelativePathBuf::new_from_initial_path(name).map(|p| p.freeze())
    }

    /// Construct a path from two parts. Parts are required to be clean.
    pub fn new_from_clean_parts(root: &str, tail: &str) -> RelativePath {
        RelativePathBuf::new_from_clean_parts(root, tail).freeze()
    }

    pub fn new_from_user_path(
        repository_path: &Path,
        user_path: &str,
    ) -> Result<RelativePath, PathError> {
        RelativePathBuf::new_from_user_path(repository_path, user_path).map(|p| p.freeze())
    }

    pub fn to_absolute_path(&self, repository_path: impl AsRef<Path>) -> PathBuf {
        repository_path.as_ref().join(self.as_str())
    }

    pub fn join(&self, suffix: impl AsRef<str>) -> RelativePath {
        self.push_into_buf(suffix).freeze()
    }

    /// Convert to mutable `RelativePathBuf`.
    /// Extracts the visible portion from the Arc-wrapped data.
    /// Optimized: if this is the only reference to the data AND viewing the full string,
    /// we can take ownership instead of cloning.
    pub fn into_buf(self) -> RelativePathBuf {
        match Arc::try_unwrap(self.data) {
            Ok(data) => {
                // We have exclusive ownership
                // If we're viewing the full string, we can avoid allocation
                if self.start == 0
                    && self.end == data.path.len()
                    && self.start_lower == 0
                    && self.end_lower == data.path_lower.len()
                {
                    // Full view - take ownership directly
                    RelativePathBuf {
                        path: data.path,
                        path_lower: data.path_lower,
                    }
                } else {
                    // Partial view - must extract substring
                    let path = data.path[self.start..self.end].to_owned();
                    let path_lower = data.path_lower[self.start_lower..self.end_lower].to_owned();
                    RelativePathBuf { path, path_lower }
                }
            }
            Err(arc) => {
                // Shared - must clone the view
                let path = arc.path[self.start..self.end].to_owned();
                let path_lower = arc.path_lower[self.start_lower..self.end_lower].to_owned();
                RelativePathBuf { path, path_lower }
            }
        }
    }

    /// Efficiently append a suffix to this path, returning a new `RelativePathBuf`.
    /// This concatenates the suffix directly without adding a path separator.
    ///
    /// More efficient than `into_buf().append()` because it pre-allocates the exact
    /// capacity needed and copies directly without intermediate allocations.
    pub fn append_into_buf(&self, suffix: &str) -> RelativePathBuf {
        let view = &self.data.path[self.start..self.end];
        let view_lower = &self.data.path_lower[self.start_lower..self.end_lower];

        if suffix.is_empty() {
            let mut path = String::with_capacity(view.len());
            path.push_str(view);
            let mut path_lower = String::with_capacity(view_lower.len());
            path_lower.push_str(view_lower);
            return RelativePathBuf { path, path_lower };
        }

        let suffix_lower = suffix.to_lowercase();

        let mut path = String::with_capacity(view.len() + suffix.len());
        path.push_str(view);
        path.push_str(suffix);

        let mut path_lower = String::with_capacity(view_lower.len() + suffix_lower.len());
        path_lower.push_str(view_lower);
        path_lower.push_str(&suffix_lower);

        RelativePathBuf { path, path_lower }
    }

    /// Efficiently push a path component to this path, returning a new `RelativePathBuf`.
    /// This adds a path separator before the suffix (if the path is non-empty).
    ///
    /// More efficient than `into_buf().push()` because it pre-allocates the exact
    /// capacity needed and copies directly without intermediate allocations.
    pub fn push_into_buf(&self, suffix: impl AsRef<str>) -> RelativePathBuf {
        let view = &self.data.path[self.start..self.end];
        let view_lower = &self.data.path_lower[self.start_lower..self.end_lower];

        let suffix = suffix.as_ref();
        if suffix.is_empty() {
            let mut path = String::with_capacity(view.len());
            path.push_str(view);
            let mut path_lower = String::with_capacity(view_lower.len());
            path_lower.push_str(view_lower);
            return RelativePathBuf { path, path_lower };
        }

        let suffix_lower = suffix.to_lowercase();
        let needs_sep = !view.is_empty();
        let sep_len = if needs_sep { 1 } else { 0 };

        let mut path = String::with_capacity(view.len() + sep_len + suffix.len());
        path.push_str(view);
        if needs_sep {
            path.push('/');
        }
        path.push_str(suffix);

        let mut path_lower = String::with_capacity(view_lower.len() + sep_len + suffix_lower.len());
        path_lower.push_str(view_lower);
        if needs_sep {
            path_lower.push('/');
        }
        path_lower.push_str(&suffix_lower);

        RelativePathBuf { path, path_lower }
    }

    pub fn overlaps(&self, other: &impl AsRef<str>) -> bool {
        overlaps_impl(self.as_str(), other.as_ref())
    }

    /// Returns `true` if `self` equals `child` or is a path-ancestor of it.
    pub fn covers(&self, child: &impl AsRef<str>) -> bool {
        covers_impl(self.as_str(), child.as_ref())
    }

    /// Reduces a set of paths to the minimal covering set by removing exact
    /// duplicates and replacing any descendant path with its ancestor — so each
    /// returned path is a superset of the input paths it covers.
    ///
    /// If any input path is the repository root (empty path), returns an empty
    /// `Vec` — the root covers everything, and callers should treat the empty
    /// result as "no path filter / scan the entire repository".
    ///
    /// Comparison is structural on the canonical `/`-separated form
    /// (case-sensitive). The returned paths are in lexicographic order.
    pub fn dedup_to_supersets(paths: Vec<RelativePath>) -> Vec<RelativePath> {
        if paths.iter().any(|p| p.is_empty()) {
            return Vec::new();
        }

        let mut sorted = paths;
        sorted.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut result: Vec<RelativePath> = Vec::with_capacity(sorted.len());
        for path in sorted {
            if !result
                .iter()
                .any(|kept| covers_impl(kept.as_str(), path.as_str()))
            {
                result.push(path);
            }
        }
        result
    }
}

/// Returns `true` if `parent` equals `child`, or is a strict path-ancestor of
/// `child` (i.e. `child` starts with `parent` followed by a `/`).
fn covers_impl(parent: &str, child: &str) -> bool {
    if parent.len() == child.len() {
        return parent == child;
    }
    if parent.len() < child.len() {
        return child.as_bytes()[parent.len()] == b'/' && child.starts_with(parent);
    }
    false
}

/// Iterator that yields deduplicated ancestor paths for staging.
/// See [`expand_path_ancestors`] for details.
pub struct ExpandPathAncestors {
    /// Paths sorted alphabetically, processed from the end (reverse order)
    sorted_paths: Vec<RelativePath>,
    /// Index from the end of `sorted_paths` (0 = last element)
    path_index_from_end: usize,
    /// Remaining components of current path (uses `pop_root` to walk through)
    remaining: RelativePath,
    /// Partial path being built up component by component
    partial: RelativePathBuf,
    /// Last path yielded, used to skip already-covered prefixes
    last_yielded: RelativePathBuf,
}

impl ExpandPathAncestors {
    fn new(mut paths: Vec<RelativePath>) -> Self {
        paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Self {
            sorted_paths: paths,
            path_index_from_end: 0,
            remaining: RelativePath::new(),
            partial: RelativePathBuf::new(),
            last_yielded: RelativePathBuf::new(),
        }
    }
}

impl Iterator for ExpandPathAncestors {
    type Item = RelativePath;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // If remaining is empty, move to the next path
            if self.remaining.is_empty() {
                if self.path_index_from_end >= self.sorted_paths.len() {
                    return None;
                }
                let path_idx = self.sorted_paths.len() - 1 - self.path_index_from_end;
                self.remaining = self.sorted_paths[path_idx].clone();
                self.partial.clear();
                self.path_index_from_end += 1;
            }

            // Pop the next component and build partial path
            let component = self.remaining.pop_root();
            self.partial.push(component);

            // Skip if partial is a prefix of last_yielded (already covered)
            if self.last_yielded.overlaps(&self.partial)
                && self.partial.len() <= self.last_yielded.len()
            {
                continue;
            }

            self.last_yielded = self.partial.clone();
            return Some(self.partial.clone().freeze());
        }
    }
}

/// Given a list of paths, compute the deduplicated set of ancestor paths that need to be
/// created/staged. This processes paths to avoid returning the same path component twice.
///
/// Returns an iterator that yields paths lazily.
///
/// For example, given `["a/b/c", "a/b/d"]`, this yields `["a", "a/b", "a/b/d", "a/b/c"]`
/// because after processing "a/b/d", the shared prefix "a/b" is already covered,
/// so only the full path "a/b/c" remains to be yielded.
pub fn expand_path_ancestors(paths: Vec<RelativePath>) -> ExpandPathAncestors {
    ExpandPathAncestors::new(paths)
}

impl std::fmt::Debug for RelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Display for RelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RelativePath {
    type Err = std::convert::Infallible;

    fn from_str(path: &str) -> Result<Self, std::convert::Infallible> {
        let path_lower = path.to_lowercase();
        let end = path.len();
        let end_lower = path_lower.len();
        Ok(RelativePath {
            data: Arc::new(RelativePathData {
                path: path.to_owned(),
                path_lower,
            }),
            start: 0,
            end,
            start_lower: 0,
            end_lower,
        })
    }
}

impl PartialEq for RelativePath {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl AsRef<str> for RelativePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Default for RelativePath {
    fn default() -> Self {
        Self::new()
    }
}

/// Owned, mutable version of `RelativePath`.
/// This type stores owned `String` fields and supports mutation operations.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RelativePathBuf {
    path: String,
    path_lower: String,
}

impl Default for RelativePathBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl RelativePathBuf {
    /// Create a new empty `RelativePathBuf`.
    pub fn new() -> Self {
        RelativePathBuf {
            path: String::with_capacity(256),
            path_lower: String::with_capacity(256),
        }
    }

    /// Construct from an initial path string.
    /// Validates that the path is not absolute and cleans it.
    pub fn new_from_initial_path(name: impl AsRef<str>) -> Result<RelativePathBuf, PathError> {
        let name = name.as_ref();
        if name.starts_with("..") || (name.len() >= 2 && name.as_bytes()[1] == b':') {
            return Err(InvalidPath {
                path: name.to_string(),
            }
            .into());
        }
        let mut initial_path = RelativePathBuf::new();
        let name = name.trim_matches('/');
        let name = name.replace('\\', "/").replace("//", "/");
        let name = name.trim_start_matches("./");
        if name == "." || name.is_empty() {
            // Leave as empty
        } else {
            initial_path.push(name);
        }
        Ok(initial_path)
    }

    /// Construct a path from two clean parts (root and tail).
    /// Parts are required to be clean.
    pub fn new_from_clean_parts(mut root: &str, mut tail: &str) -> RelativePathBuf {
        let mut path = RelativePathBuf::new();
        if root.ends_with('/') {
            root = &root[..(root.len() - 1)];
        }
        if tail.starts_with('/') {
            tail = &tail[1..tail.len()];
        }
        if !root.is_empty() {
            path.push(root);
        }
        if !tail.is_empty() {
            path.push(tail);
        }
        path
    }

    /// Construct from a user-provided path relative to a repository path.
    /// Makes the user path absolute if needed, then computes the relative portion.
    pub fn new_from_user_path(
        repository_path: &Path,
        user_path: &str,
    ) -> Result<RelativePathBuf, PathError> {
        if user_path == "." || user_path.is_empty() {
            return Ok(RelativePathBuf::new());
        }

        let mut absolute_path = Path::new(user_path).to_path_buf();
        if !absolute_path.is_absolute() {
            absolute_path = make_absolute(absolute_path.to_string_lossy())?;
        }
        let absolute_path = clean(absolute_path.display().to_string());

        let mut repository_path = Path::new(repository_path).to_path_buf();
        if !repository_path.is_absolute() {
            repository_path = make_absolute(repository_path.to_string_lossy())?;
        }
        let repository_path = clean(repository_path.display().to_string()).to_lowercase();

        if !absolute_path
            .to_lowercase()
            .starts_with(repository_path.as_str())
        {
            return Err(InvalidPath {
                path: absolute_path,
            }
            .into());
        }

        let relative_path = absolute_path
            .split_at(repository_path.len())
            .1
            .trim_matches('/');
        if relative_path.is_empty() || relative_path == "." {
            return Ok(RelativePathBuf::new());
        }

        let mut out_path = RelativePathBuf::new();
        out_path.push(relative_path);
        Ok(out_path)
    }

    /// Append a path component, adding separator if needed.
    /// Updates `path_lower` to maintain lowercase invariant.
    pub fn push(&mut self, name: impl AsRef<str>) -> &mut Self {
        let name = name.as_ref();
        if name.is_empty() {
            return self;
        }

        self.path.reserve(1 + name.len());
        if !self.path.is_empty() && !self.path.ends_with('/') {
            self.path.push('/');
        }
        self.path.push_str(name);

        let name_lower = name.to_lowercase();
        if !self.path_lower.is_empty() && !self.path_lower.ends_with('/') {
            self.path_lower.push('/');
        }
        self.path_lower.push_str(name_lower.as_str());

        self
    }

    /// Reset both `path` and `path_lower` to empty.
    pub fn clear(&mut self) {
        self.path.clear();
        self.path_lower.clear();
    }

    /// Concatenate raw string to `path`, updating `path_lower`.
    pub fn append(&mut self, suffix: &str) -> &mut Self {
        if suffix.is_empty() {
            return self;
        }

        self.path.push_str(suffix);
        self.path_lower.push_str(&suffix.to_lowercase());

        self
    }

    /// Append a path component and return self (consuming variant of push).
    pub fn join(mut self, suffix: &str) -> Self {
        self.push(suffix);
        self
    }

    /// Returns a reference to the path string.
    pub fn as_str(&self) -> &str {
        &self.path
    }

    /// Returns a reference to the lowercase path string.
    pub fn as_lowercase_str(&self) -> &str {
        &self.path_lower
    }

    /// Returns the last path component (after the last `/`).
    pub fn name(&self) -> &str {
        name_impl(self.as_str())
    }

    /// Returns the lowercase version of the last path component.
    pub fn name_lowercase(&self) -> &str {
        name_impl(self.as_lowercase_str())
    }

    /// Returns the first path component (before the first `/`).
    pub fn root(&self) -> &str {
        root_impl(self.as_str())
    }

    /// Returns everything except the last component, or None if the path has no parent.
    pub fn parent(&self) -> Option<&str> {
        parent_impl(self.as_str())
    }

    /// Returns the length of the path string.
    pub fn len(&self) -> usize {
        self.path.len()
    }

    /// Returns true if the path is empty.
    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
    }

    /// Checks if this path overlaps with another (one is a prefix of or equal to the other).
    pub fn overlaps(&self, other: &impl AsRef<str>) -> bool {
        overlaps_impl(self.as_str(), other.as_ref())
    }

    /// Remove the last path component (everything after the last `/`).
    pub fn pop(&mut self) -> &mut Self {
        if !self.is_empty() {
            if let Some(sep) = self.path.rfind('/') {
                self.path.truncate(sep);
                if let Some(sep) = self.path_lower.rfind('/') {
                    self.path_lower.truncate(sep);
                }
            } else {
                self.path.clear();
                self.path_lower.clear();
            }
        }
        self
    }

    /// Helper method: push a component and then freeze to `RelativePath`.
    /// This is a convenience method to avoid the issue of `push()` returning &mut Self.
    pub fn push_and_freeze(mut self, name: impl AsRef<str>) -> RelativePath {
        self.push(name);
        self.freeze()
    }

    /// Helper method: append a suffix and then freeze to `RelativePath`.
    /// This is a convenience method to avoid the issue of `append()` returning &mut Self.
    pub fn append_and_freeze(mut self, suffix: &str) -> RelativePath {
        self.append(suffix);
        self.freeze()
    }

    /// Convert to immutable `RelativePath`.
    /// Creates an Arc-wrapped data structure with the full view.
    pub fn freeze(self) -> RelativePath {
        let end = self.path.len();
        let end_lower = self.path_lower.len();
        RelativePath {
            data: Arc::new(RelativePathData {
                path: self.path,
                path_lower: self.path_lower,
            }),
            start: 0,
            end,
            start_lower: 0,
            end_lower,
        }
    }
}

impl std::fmt::Debug for RelativePathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.path.as_str())
    }
}

impl Display for RelativePathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.path.as_str())
    }
}

impl AsRef<str> for RelativePathBuf {
    fn as_ref(&self) -> &str {
        self.path.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_path_with_leading_parent() {
        assert_eq!("../../def", clean("../../abc/../def".to_owned()));
    }

    #[test]
    fn test_clean_path_with_double_parent() {
        assert_eq!("abc/jkl", clean("abc/def/ghi/../../jkl".to_owned()));
    }

    #[test]
    fn test_clean_path_with_parent_after_period() {
        assert_eq!("abc/ghi", clean("abc/def/./../ghi".to_owned()));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_clean_path_with_parent_after_drive() {
        assert_eq!("C:/abc", clean("C:\\..\\abc".to_owned()));
        assert_eq!("C:/abc", clean("C:/../abc".to_owned()));
    }

    #[cfg(not(target_os = "windows"))]
    mod is_path_inside_repository {
        use std::path::Path;

        use super::super::is_path_inside_repository;

        #[test]
        fn child_at_root() {
            assert!(is_path_inside_repository(Path::new("/a/b"), "/a/b/x.txt"));
        }

        #[test]
        fn nested_child() {
            assert!(is_path_inside_repository(
                Path::new("/a/b"),
                "/a/b/c/d/x.txt",
            ));
        }

        #[test]
        fn sibling_directory_is_outside() {
            assert!(!is_path_inside_repository(Path::new("/a/b"), "/a/c/x.txt",));
        }

        #[test]
        fn repo_equals_candidate() {
            assert!(is_path_inside_repository(Path::new("/a/b"), "/a/b"));
        }

        #[test]
        fn traversal_escaping_is_outside() {
            // /a/b/../../tmp/x.txt cleans to /tmp/x.txt, which is outside /a/b.
            assert!(!is_path_inside_repository(
                Path::new("/a/b"),
                "/a/b/../../tmp/x.txt",
            ));
        }

        #[test]
        fn traversal_returning_is_inside() {
            // /a/b/sub/../x.txt cleans to /a/b/x.txt.
            assert!(is_path_inside_repository(
                Path::new("/a/b"),
                "/a/b/sub/../x.txt",
            ));
        }

        #[test]
        fn empty_candidate_is_inside() {
            // new_from_user_path treats "" / "." as the repo root itself.
            assert!(is_path_inside_repository(Path::new("/a/b"), ""));
            assert!(is_path_inside_repository(Path::new("/a/b"), "."));
        }

        #[test]
        fn case_insensitive() {
            // new_from_user_path lowercases both sides before comparing.
            assert!(is_path_inside_repository(Path::new("/A/B"), "/a/b/x.txt",));
        }
    }
}
