// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;
use std::fmt::Display;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_error_set::prelude::*;

use crate::errors::InvalidPath;
use crate::repository::RepositoryContext;

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
///
/// This is the one sanctioned `current_dir` fallback the `disallowed_methods`
/// fence points every other caller at, so it cannot itself route through
/// [`make_absolute`]. Reached only when the caller named no base and the call
/// carries no working directory — in-process use, never a service call, which
/// fills the field on the caller's side before it crosses the IPC boundary.
#[allow(clippy::disallowed_methods)]
pub fn make_absolute_from(
    path: impl AsRef<str>,
    base: Option<&Path>,
) -> Result<PathBuf, PathError> {
    let path = path.as_ref();
    let cleanpath = clean(path.to_owned());
    // `PathBuf: FromStr` has `Err = Infallible`, so the old error arm here was
    // unreachable. Construct it directly rather than mapping an error that
    // cannot occur.
    let pathbuf = PathBuf::from(cleanpath.as_str());
    if pathbuf.is_absolute() {
        return Ok(pathbuf);
    }
    match base {
        Some(base) => Ok(base.join(pathbuf)),
        None => Ok(std::env::current_dir()
            .internal("getting the current working directory")?
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

/// Number of path components in a repository-relative path.
pub(crate) fn path_depth(path: &str) -> usize {
    path.matches('/').count() + 1
}

/// Number of leading components `left` and `right` share.
pub(crate) fn shared_component_depth(left: &str, right: &str) -> usize {
    left.split('/')
        .zip(right.split('/'))
        .take_while(|(left, right)| left == right)
        .count()
}

/// A repository-relative path carrying its own depth.
///
/// Ordering is by depth and then by path, which the field order gives the
/// derive. Carrying the depth counts the components once per path rather than
/// once per comparison, and leaves a sorted set grouped into depth levels.
///
/// The fields are private so the depth is always the one the path has.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DepthPath {
    depth: usize,
    path: String,
}

impl DepthPath {
    pub(crate) fn new(path: String) -> Self {
        Self {
            depth: path_depth(&path),
            path,
        }
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

/// A Windows verbatim path prefix, which names what follows it.
const VERBATIM_PREFIX: &str = r"\\?\";

/// A Windows device path prefix, which names what follows it.
const DEVICE_PREFIX: &str = r"\\.\";

/// Replace every `from` in `path` with `to`, leaving a path that holds no `from`
/// as it is.
///
/// [`str::replace`] has nowhere to hand back the string it was given, so it
/// allocates and copies the whole of it whether or not it replaced anything.
fn replace_present(path: &mut String, from: &str, to: &str) {
    if path.contains(from) {
        *path = path.replace(from, to);
    }
}

/// Remove the leading `prefix`, however many times `path` repeats it.
fn trim_leading(path: &mut String, prefix: &str) {
    let trimmed = path.trim_start_matches(prefix).len();
    path.drain(..(path.len() - trimmed));
}

/// Fold `path` to lowercase in place where ASCII covers it, and through
/// [`str::to_lowercase`] where it does not.
fn make_lowercase(path: &mut String) {
    if path.is_ascii() {
        path.make_ascii_lowercase();
    } else {
        *path = path.to_lowercase();
    }
}

/// Collapse every run of `/` in `path` to one, in place.
///
/// [`str::replace`] takes the matches of one pass and they do not overlap, so it
/// leaves a run of three separators as a run of two.
fn collapse_separators(path: &mut String) {
    if !path.contains("//") {
        return;
    }

    let mut preceded_by_separator = false;
    path.retain(|character| {
        let repeated = preceded_by_separator && character == '/';
        preceded_by_separator = character == '/';
        !repeated
    });
}

/// Remove every `/./` from `path`.
///
/// Removing one leaves what stood either side of it adjacent, which can form
/// another, so this is taken to a fixed point where [`replace_present`] takes a
/// single pass.
fn remove_dot_segments(path: &mut String) {
    while path.contains("/./") {
        *path = path.replace("/./", "/");
    }
}

/// Resolve every `..` in `path` against the component above it.
///
/// A component is a step up only where it is `..` entire, so `...` and `..name`
/// are names like any other. One with nothing above it to resolve against is
/// kept, since a path can start below where it is rooted, except at the root of
/// an absolute path, where nothing stands above it to step up to.
fn reduce_parent_segments(path: &mut String) {
    if !path.contains("..") {
        return;
    }

    let mut remain: Vec<&str> = Vec::with_capacity(path_depth(path));
    for element in path.split('/') {
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
                // The drive names where the path is rooted, not a component of it.
                continue;
            }
            if let Some(last) = remain.last()
                && *last == ".."
            {
                // A step up does not resolve against a step up.
                remain.push(element);
                continue;
            }

            remain.pop();
            continue;
        }
        remain.push(element);
    }

    let mut reduced = remain.join("/");
    if path.starts_with('/') {
        trim_leading(&mut reduced, "../");
        reduced.insert(0, '/');
    }

    *path = reduced;
}

/// Checks if the path contains any ".." segments. For example "..", "subdir/.." matches.
/// A legitimate file name starting with dots does not match, e.g. "..hidden.txt"
fn contains_directory_step_up(path: &str) -> bool {
    path.contains("..") && path.split('/').any(|component| component == "..")
}

/// `path` in the form the repository names paths in: forward separators, none of
/// them repeated, and no `.` or `..` left to resolve.
pub fn clean(mut path: String) -> String {
    replace_present(&mut path, VERBATIM_PREFIX, "");
    replace_present(&mut path, DEVICE_PREFIX, "");
    replace_present(&mut path, "\\", "/");
    collapse_separators(&mut path);
    remove_dot_segments(&mut path);

    if path.starts_with("./") {
        trim_leading(&mut path, "./");
    }

    if path.ends_with("/.") {
        path.truncate(path.trim_end_matches("/.").len());
    }

    reduce_parent_segments(&mut path);

    path
}

/// What is left of `path` below the components of `prefix_lower`, or `None`
/// where they do not name its ancestors.
///
/// Matches one component at a time against the lowercase form. A fold can change
/// the length of a component, which leaves a byte offset taken from one form of
/// a path no longer naming the same place in the other, and a prefix that stops
/// part way through a component names a sibling rather than an ancestor.
fn strip_lowercase_prefix<'a>(path: &'a str, prefix_lower: &str) -> Option<&'a str> {
    let mut remainder = path.trim_start_matches('/');
    let mut folded = String::new();

    for expected in prefix_lower.split('/').filter(|part| !part.is_empty()) {
        let (component, rest) = remainder.split_once('/').unwrap_or((remainder, ""));
        folded.clear();
        push_lowercase(&mut folded, component);
        if folded != expected {
            return None;
        }
        remainder = rest;
    }

    Some(remainder)
}

/// `path` in cleaned absolute form, resolved against the working directory if it
/// is not already absolute.
///
/// Each step takes the buffer it is handed rather than formatting a second one,
/// so a path the platform holds outside UTF-8 is an [`InvalidPath`] rather than a
/// lossy rendering of one.
fn absolute_clean(path: &Path) -> Result<String, PathError> {
    let Some(text) = path.to_str() else {
        return Err(InvalidPath {
            path: path.to_string_lossy().into_owned(),
        }
        .into());
    };

    if path.is_absolute() {
        return Ok(clean(text.to_owned()));
    }

    let absolute = make_absolute(text)?.into_os_string();
    let absolute = absolute.into_string().map_err(|absolute| InvalidPath {
        path: absolute.to_string_lossy().into_owned(),
    })?;
    Ok(clean(absolute))
}

// ============================================================================
// Shared helper functions for RelativePath and RelativePathBuf
// These operate on &str to avoid code duplication between the two types.
// ============================================================================

/// Append `name` to `out` in lowercase.
///
/// ASCII, which is the whole of nearly every path, folds into the destination's
/// spare capacity in one branchless pass and needs no string of its own, and the
/// high bits that pass accumulates say afterwards whether the fold was the usable
/// one. Testing per byte instead would stop the pass vectorizing.
///
/// Anything else takes [`str::to_lowercase`], the fold node names are hashed
/// over. Folding character by character instead would disagree with it wherever
/// the mapping depends on where in a word the character falls.
fn push_lowercase(out: &mut String, name: &str) {
    let restore = out.len();

    let was_ascii = {
        // SAFETY: the loop writes `name` back byte for byte with only `A-Z`
        // moved to `a-z`, and UTF-8 spends those encodings on nothing but those
        // scalars, so what is appended is valid UTF-8 whether or not the fold
        // turns out to be the usable one. `restore` is where a string ended, so
        // the truncate below lands on a character boundary.
        let bytes = unsafe { out.as_mut_vec() };
        bytes.reserve(name.len());

        let mut high_bits = 0u8;
        for (target, &source) in bytes.spare_capacity_mut().iter_mut().zip(name.as_bytes()) {
            high_bits |= source;
            target.write(source.to_ascii_lowercase());
        }

        // SAFETY: the reserve above left room for `name`, so the loop wrote
        // every one of its bytes.
        unsafe { bytes.set_len(restore + name.len()) };

        high_bits & 0x80 == 0
    };

    if !was_ascii {
        out.truncate(restore);
        out.push_str(&name.to_lowercase());
    }
}

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

    /// [`pop_root`](Self::pop_root) `count` times over, leaving a view of what
    /// is below the first `count` components. Stops at the end of the path.
    ///
    /// The data is shared, so a suffix of a path costs no allocation.
    pub fn pop_root_repeat(&mut self, count: usize) -> &mut Self {
        for _ in 0..count {
            if self.is_empty() {
                break;
            }
            self.pop_root();
        }
        self
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

        let mut path = String::with_capacity(view.len() + suffix.len());
        path.push_str(view);
        path.push_str(suffix);

        let mut path_lower = String::with_capacity(view_lower.len() + suffix.len());
        path_lower.push_str(view_lower);
        push_lowercase(&mut path_lower, suffix);

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

        let needs_sep = !view.is_empty();
        let sep_len = if needs_sep { 1 } else { 0 };

        let mut path = String::with_capacity(view.len() + sep_len + suffix.len());
        path.push_str(view);
        if needs_sep {
            path.push('/');
        }
        path.push_str(suffix);

        let mut path_lower = String::with_capacity(view_lower.len() + sep_len + suffix.len());
        path_lower.push_str(view_lower);
        if needs_sep {
            path_lower.push('/');
        }
        push_lowercase(&mut path_lower, suffix);

        RelativePathBuf { path, path_lower }
    }

    pub fn overlaps(&self, other: &impl AsRef<str>) -> bool {
        overlaps_impl(self.as_str(), other.as_ref())
    }

    /// Returns `true` if `self` equals `child` or is a path-ancestor of it.
    pub fn covers(&self, child: &impl AsRef<str>) -> bool {
        covers_impl(self.as_str(), child.as_ref())
    }

    /// [`covers`](Self::covers) on the lowercased form. Node lookup hashes
    /// names case-insensitively, so a path the caller resolved a node from can
    /// differ in case from the stored paths it has to be compared against.
    pub fn covers_ignore_case(&self, child: &RelativePath) -> bool {
        covers_impl(self.as_lowercase_str(), child.as_lowercase_str())
    }

    /// Reduces a set of paths to the minimal covering set by removing exact
    /// duplicates and replacing any descendant path with its ancestor — so each
    /// returned path is a superset of the input paths it covers.
    ///
    /// If any input path is the repository root (empty path), returns an empty
    /// `Vec` — the root covers everything, and callers should treat the empty
    /// result as "no path filter / scan the entire repository".
    ///
    /// One parent cannot hold two entries whose names differ only in case, so
    /// paths that differ only in the case of a shared component are brought onto
    /// one variation and collapse together.
    ///
    /// Comparison is structural on the canonical `/`-separated form. The
    /// returned paths are in lexicographic order.
    ///
    /// Runs in O(n log n): the sort dominates, and the scan that follows tests
    /// each path against one candidate rather than against everything kept so
    /// far. That matters at the sizes a `--targets` file reaches — a
    /// scan-against-all pass over 900,000 paths is ~4×10¹¹ comparisons, each one
    /// chasing a separate allocation, which does not finish in an hour.
    pub fn dedup_to_supersets(paths: Vec<RelativePath>) -> Vec<RelativePath> {
        if paths.iter().any(|p| p.is_empty()) {
            return Vec::new();
        }

        // Ordered by the lowercase form, so the case variations of one entry sort
        // together and the first of them is the one the rest are brought onto.
        let mut sorted = paths;
        sorted.sort_unstable_by(|a, b| {
            compare_subtree_order(a.as_lowercase_str(), b.as_lowercase_str())
                .then_with(|| compare_subtree_order(a.as_str(), b.as_str()))
        });

        // Testing only the last kept path is sufficient, not a bug: a covering
        // path's subtree is contiguous in this order, so anything kept earlier
        // that covered this path would also cover the last kept one, and a
        // covered path is never kept. Unification runs before that test, so the
        // kept path already carries the case every later path is brought onto.
        let mut result: Vec<RelativePath> = Vec::with_capacity(sorted.len());
        for path in sorted {
            let path = match result.last() {
                Some(kept) => unify_case_with(kept, &path).unwrap_or(path),
                None => path,
            };
            if !result
                .last()
                .is_some_and(|kept| covers_impl(kept.as_str(), path.as_str()))
            {
                result.push(path);
            }
        }

        // Subtree order is not the lexicographic order this returns.
        result.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        result
    }
}

/// Byte length of the first `count` components of `path`.
fn component_prefix_len(path: &str, count: usize) -> usize {
    path.split('/')
        .take(count)
        .map(|name| name.len() + 1)
        .sum::<usize>()
        .saturating_sub(1)
}

/// `path` with the components it shares with `established` taking that path's
/// case, or `None` when the two already agree.
///
/// Components are shared as far as their lowercase forms run together: below the
/// first component the two genuinely disagree on they are in different
/// directories and neither says anything about the other.
fn unify_case_with(established: &RelativePath, path: &RelativePath) -> Option<RelativePath> {
    let mut established_names = established.as_str().split('/');
    let mut established_lower = established.as_lowercase_str().split('/');
    let mut path_lower = path.as_lowercase_str().split('/');

    let mut shared = 0;
    let mut differs = false;
    for name in path.as_str().split('/') {
        let (Some(established_name), Some(established_name_lower), Some(name_lower)) = (
            established_names.next(),
            established_lower.next(),
            path_lower.next(),
        ) else {
            break;
        };
        if established_name_lower != name_lower {
            break;
        }
        differs |= established_name != name;
        shared += 1;
    }
    if !differs {
        return None;
    }

    let established = established.as_str();
    let head = &established[..component_prefix_len(established, shared)];
    let tail = &path.as_str()[component_prefix_len(path.as_str(), shared)..];
    Some(RelativePath::new_from_clean_parts(head, tail))
}

/// Orders paths so that every path is immediately followed by its own subtree.
///
/// A byte comparison does not do that. `/` is 0x2F, so any name character below
/// it interleaves: `a/b` < `a/b.c` < `a/b/c` puts an unrelated sibling between a
/// directory and its child. Ranking the separator below every other byte makes a
/// directory's subtree the contiguous run directly after it, because `/` is then
/// the smallest thing that can follow the directory's name.
///
/// The order is total — lexicographic over the ranked bytes, then by length — so
/// it is a valid sort comparator.
fn compare_subtree_order(left: &str, right: &str) -> std::cmp::Ordering {
    fn rank(byte: u8) -> u16 {
        if byte == b'/' { 0 } else { byte as u16 + 1 }
    }

    let (left, right) = (left.as_bytes(), right.as_bytes());
    for (a, b) in left.iter().zip(right.iter()) {
        if a != b {
            return rank(*a).cmp(&rank(*b));
        }
    }
    left.len().cmp(&right.len())
}

#[derive(Clone)]
pub struct RepositoryPath {
    relative: RelativePath,
    absolute: PathBuf,
}

impl RepositoryPath {
    pub fn from_relative(
        repository: &Arc<RepositoryContext>,
        relative_path: RelativePath,
    ) -> Result<Self, InvalidArguments> {
        Ok(Self::from_relative_and_root(
            repository.require_path()?,
            relative_path,
        ))
    }

    pub fn from_relative_and_root(root: &Path, relative: RelativePath) -> Self {
        Self {
            absolute: relative.to_absolute_path(root),
            relative,
        }
    }

    pub fn relative(&self) -> &RelativePath {
        &self.relative
    }

    pub fn absolute(&self) -> &Path {
        self.absolute.as_path()
    }

    pub fn get_child(&self, child: &str) -> Self {
        RepositoryPath {
            relative: self.relative.clone().push_into_buf(child).freeze(),
            absolute: self.absolute.join(child),
        }
    }

    pub fn get_parent(&self) -> Option<Self> {
        let mut relative_parent = self.relative.clone();
        relative_parent.pop();
        if relative_parent == self.relative {
            return None;
        }
        let mut absolute_parent = self.absolute.clone();
        if absolute_parent.pop() {
            Some(RepositoryPath {
                relative: relative_parent,
                absolute: absolute_parent,
            })
        } else {
            None
        }
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
        let mut path_lower = String::with_capacity(path.len());
        push_lowercase(&mut path_lower, path);
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
    ///
    /// Reserves room for a path of typical depth, for the buffers that are
    /// built up a component at a time. A path whose length is known at
    /// construction takes [`RelativePathBuf::with_capacity`] instead.
    pub fn new() -> Self {
        RelativePathBuf::with_capacity(256)
    }

    /// An empty `RelativePathBuf` with room for `capacity` bytes in each of the
    /// two strings it keeps.
    ///
    /// Both are reserves rather than limits: a path lowercases character for
    /// character except where a general mapping widens it, and that grows.
    pub fn with_capacity(capacity: usize) -> Self {
        RelativePathBuf {
            path: String::with_capacity(capacity),
            path_lower: String::with_capacity(capacity),
        }
    }

    /// Construct from an initial path string.
    /// Validates that the path is not absolute, holds no step up, and cleans it.
    ///
    /// Only a path carrying a separator it does not keep is rewritten. Trimming
    /// and the canonical separator are what nearly every path already holds, and
    /// establishing that costs no string of its own.
    pub fn new_from_initial_path(name: impl AsRef<str>) -> Result<RelativePathBuf, PathError> {
        let name = name.as_ref();
        if name.len() >= 2 && name.as_bytes()[1] == b':' {
            return Err(InvalidPath {
                path: name.to_string(),
            }
            .into());
        }
        let name = name.trim_matches('/');
        let name = if name.contains('\\') || name.contains("//") {
            let mut rewritten = name.replace('\\', "/");
            collapse_separators(&mut rewritten);
            Cow::Owned(rewritten)
        } else {
            Cow::Borrowed(name)
        };
        let name = name.trim_start_matches("./");
        if contains_directory_step_up(name) {
            return Err(InvalidPath {
                path: name.to_string(),
            }
            .into());
        }
        let mut initial_path = RelativePathBuf::with_capacity(name.len());
        if name != "." && !name.is_empty() {
            initial_path.push(name);
        }
        Ok(initial_path)
    }

    /// Construct a path from two clean parts (root and tail).
    /// Parts are required to be clean.
    pub fn new_from_clean_parts(mut root: &str, mut tail: &str) -> RelativePathBuf {
        if root.ends_with('/') {
            root = &root[..(root.len() - 1)];
        }
        if tail.starts_with('/') {
            tail = &tail[1..tail.len()];
        }
        let mut path = RelativePathBuf::with_capacity(root.len() + tail.len() + 1);
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
            return Ok(RelativePathBuf::with_capacity(0));
        }

        let absolute_path = absolute_clean(Path::new(user_path))?;

        let mut repository_path = absolute_clean(repository_path)?;
        make_lowercase(&mut repository_path);

        let Some(relative_path) = strip_lowercase_prefix(&absolute_path, &repository_path) else {
            return Err(InvalidPath {
                path: absolute_path,
            }
            .into());
        };

        let relative_path = relative_path.trim_matches('/');
        if relative_path.is_empty() || relative_path == "." {
            return Ok(RelativePathBuf::with_capacity(0));
        }

        let mut out_path = RelativePathBuf::with_capacity(relative_path.len());
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

        self.path_lower.reserve(1 + name.len());
        if !self.path_lower.is_empty() && !self.path_lower.ends_with('/') {
            self.path_lower.push('/');
        }
        push_lowercase(&mut self.path_lower, name);

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
        push_lowercase(&mut self.path_lower, suffix);

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
    fn a_pushed_component_carries_into_the_lowercase_form() {
        let mut path = RelativePathBuf::new();
        path.push("Assets");
        path.push("MESHES");
        assert_eq!(path.as_str(), "Assets/MESHES");
        assert_eq!(path.as_lowercase_str(), "assets/meshes");
    }

    /// The lowercase form takes an ASCII path character for character and
    /// anything else through the general mapping, which can be more characters
    /// than it replaces. The capacity both are built with is a reserve, so the
    /// wider one grows rather than being cut short.
    #[test]
    fn a_component_beyond_ascii_lowercases_through_the_general_mapping() {
        let mut path = RelativePathBuf::with_capacity("ÅNGSTRÖM".len());
        path.push("ÅNGSTRÖM");
        assert_eq!(path.as_str(), "ÅNGSTRÖM");
        assert_eq!(path.as_lowercase_str(), "ångström");

        let mut path = RelativePathBuf::with_capacity("İ".len());
        path.push("İ");
        assert_eq!(path.as_str(), "İ");
        assert_eq!(
            path.as_lowercase_str(),
            "i\u{307}",
            "one character became two"
        );
    }

    /// Every route into the lowercase form takes a suffix beyond ASCII through
    /// the general mapping, not `push` alone. Each reserves what the suffix takes
    /// in the path, which the two characters `İ` folds to outgrow.
    #[test]
    fn every_append_route_lowercases_beyond_ascii() {
        let base = RelativePath::new_from_clean_parts("Assets", "");

        let appended = base.append_into_buf("_İ");
        assert_eq!(appended.as_str(), "Assets_İ");
        assert_eq!(appended.as_lowercase_str(), "assets_i\u{307}");

        let pushed = base.push_into_buf("MESH_İ");
        assert_eq!(pushed.as_str(), "Assets/MESH_İ");
        assert_eq!(pushed.as_lowercase_str(), "assets/mesh_i\u{307}");

        let mut buf = base.into_buf();
        buf.append("_İ");
        assert_eq!(buf.as_str(), "Assets_İ");
        assert_eq!(buf.as_lowercase_str(), "assets_i\u{307}");
    }

    /// The fold has to be the one node names are hashed over, or a name matches
    /// on its digest and not on its path. A character-wise fold parts from it
    /// where the mapping depends on where in a word the character falls, which
    /// for a final capital sigma it does.
    #[test]
    fn the_lowercase_form_takes_the_fold_node_names_are_hashed_over() {
        for name in [
            "",
            "Assets",
            "ROCK.MESH",
            "Stra\u{00df}e",
            "\u{0130}stanbul",
            "\u{00c5}NGSTR\u{00d6}M",
            "\u{039f}\u{0394}\u{039f}\u{03a3}",
            "\u{03a3}\u{03bf}\u{03c6}\u{03bf}\u{03c2}",
            "\u{4f60}\u{597d}",
        ] {
            let mut folded = String::new();
            push_lowercase(&mut folded, name);
            assert_eq!(folded, name.to_lowercase(), "{name:?} folds apart from it");
        }
    }

    /// The ASCII fold is written before the name is known to be ASCII, so a
    /// component that only reaches beyond it at the end has to have that
    /// speculative write taken back off what came before it.
    #[test]
    fn a_component_turning_non_ascii_at_its_end_keeps_what_it_was_appended_to() {
        let mut path = RelativePathBuf::new();
        path.push("Assets");
        path.push("MESH_Å");
        assert_eq!(path.as_str(), "Assets/MESH_Å");
        assert_eq!(path.as_lowercase_str(), "assets/mesh_å");
    }

    /// An initial path is trimmed and brought onto `/`, and only one that is not
    /// already there is rewritten to establish it.
    #[test]
    fn an_initial_path_holds_the_canonical_separator() {
        let expect = |name: &str| {
            RelativePathBuf::new_from_initial_path(name)
                .expect("the path is relative")
                .as_str()
                .to_owned()
        };

        assert_eq!(expect("Assets/Meshes"), "Assets/Meshes");
        assert_eq!(expect("/Assets/Meshes/"), "Assets/Meshes");
        assert_eq!(expect("./Assets"), "Assets");
        assert_eq!(expect("Assets\\Meshes"), "Assets/Meshes");
        assert_eq!(expect("Assets//Meshes"), "Assets/Meshes");
        assert_eq!(
            expect("Assets///Meshes"),
            "Assets/Meshes",
            "a run of separators collapses however long it is"
        );
        assert_eq!(
            expect("Assets\\\\Meshes"),
            "Assets/Meshes",
            "a separator that doubles once rewritten collapses with it"
        );
        assert!(expect(".").is_empty());
        assert!(expect("").is_empty());
    }

    /// `from_str` folds a whole path rather than a component, and the lowercase
    /// form it ends up with is what bounds the view of it.
    #[test]
    fn from_str_carries_a_lowercase_form_of_its_own_length() {
        let path = RelativePath::from_str("FÖLDER/İ").expect("the conversion is infallible");
        assert_eq!(path.as_str(), "FÖLDER/İ");
        assert_eq!(
            path.as_lowercase_str(),
            "földer/i\u{307}",
            "a fold that widens is not cut short by the length of the path"
        );
    }

    #[test]
    fn pop_root_repeat_leaves_a_view_of_what_is_below_them() {
        let mut path = RelativePath::new_from_clean_parts("Assets/Meshes/Rock.mesh", "");
        path.pop_root_repeat(2);
        assert_eq!(path.as_str(), "Rock.mesh");
        assert_eq!(
            path.as_lowercase_str(),
            "rock.mesh",
            "the lowercase form advances with it"
        );

        path.pop_root_repeat(5);
        assert!(path.is_empty(), "advancing past the end stops at it");
    }

    #[test]
    fn shared_component_depth_counts_whole_components() {
        assert_eq!(shared_component_depth("a/b/c", "a/b/d"), 2);
        assert_eq!(shared_component_depth("a/b/c", "a/x/c"), 1);
        assert_eq!(shared_component_depth("a/b/c", "x/b/c"), 0);
        assert_eq!(shared_component_depth("a/b/c", "a/b/c"), 3);
        // A shared string prefix that stops inside a component shares neither it
        // nor anything below it.
        assert_eq!(shared_component_depth("a/b/x", "a/bc/y"), 1);
        assert_eq!(shared_component_depth("ab/x", "a/x"), 0);
        // A path that runs out is shared as far as it goes.
        assert_eq!(shared_component_depth("a/b", "a/b/c"), 2);
        assert_eq!(shared_component_depth("", "a"), 0);
        assert_eq!(shared_component_depth("a", "a"), 1);
    }

    /// The steps a path takes to reach canonical form, each of which leaves a
    /// path already there as it is.
    #[test]
    fn clean_brings_a_path_onto_the_canonical_form() {
        assert_eq!("abc/def", clean("abc/def".to_owned()));
        assert_eq!("abc/def", clean(r"\\?\abc\def".to_owned()));
        assert_eq!("abc/def", clean(r"\\.\abc\def".to_owned()));
        assert_eq!("abc/def", clean("abc//def".to_owned()));
        assert_eq!("abc/def", clean("abc////def".to_owned()));
        assert_eq!("abc/def", clean("abc/./def".to_owned()));
        assert_eq!(
            "abc/def",
            clean("abc/././def".to_owned()),
            "a `.` left adjacent to the next by removing one is removed with it"
        );
        assert_eq!("abc/def", clean("./abc/def".to_owned()));
        assert_eq!("abc/def", clean("././abc/def".to_owned()));
        assert_eq!("abc/def", clean("abc/def/.".to_owned()));
        assert_eq!("", clean(String::new()));
    }

    /// Only a component that is `..` entire is a step up. A name that merely
    /// begins with two periods, or is made of them, is a name.
    #[test]
    fn clean_steps_up_for_a_parent_and_not_for_a_name() {
        assert_eq!("abc/.../def", clean("abc/.../def".to_owned()));
        assert_eq!("abc/..name/def", clean("abc/..name/def".to_owned()));
        assert_eq!("abc/name../def", clean("abc/name../def".to_owned()));
        assert_eq!("abc/def", clean("abc/ghi/../def".to_owned()));
    }

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

    #[test]
    fn repository_path_parent() {
        let nested_children = RepositoryPath::from_relative_and_root(
            Path::new("/a/b/c"),
            RelativePath::new_from_initial_path("d/e").unwrap(),
        );
        let nested_children_parent = nested_children
            .get_parent()
            .expect("Parent should be constructable");
        assert_eq!(nested_children_parent.absolute(), PathBuf::from("/a/b/c/d"));
        assert_eq!(
            *nested_children_parent.relative(),
            RelativePath::new_from_initial_path("d").unwrap()
        );

        let single_child = RepositoryPath::from_relative_and_root(
            Path::new("/a/b/c"),
            RelativePath::new_from_initial_path("d").unwrap(),
        );
        let single_child_parent = single_child
            .get_parent()
            .expect("Parent should be constructable");
        assert_eq!(single_child_parent.absolute(), PathBuf::from("/a/b/c"));
        assert_eq!(
            *single_child_parent.relative(),
            RelativePath::new_from_initial_path("").unwrap()
        );

        let no_children = RepositoryPath::from_relative_and_root(
            Path::new("/a/b/c"),
            RelativePath::new_from_initial_path("").unwrap(),
        );
        assert!(no_children.get_parent().is_none());
    }
}
