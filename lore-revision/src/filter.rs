// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;

use bitflags::bitflags;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::bitflagsops;
use crate::event::LoreEvent;
use crate::interface::LoreString;
use crate::lore_warn;
use crate::repository::BASE_SUFFIX;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::MINE_SUFFIX;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::repository::THEIRS_SUFFIX;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

#[derive(Clone, Default, Debug)]
pub struct Filter {
    pub ignore: FilterInstance,
    pub view: FilterInstance,
}

#[derive(Clone, Default, Debug)]
pub struct FilterInstance {
    pub lines: Vec<FilterLine>,
}

#[derive(Default, Clone, Debug)]
pub struct FilterLine {
    glob: String,
    negated: bool,
    directory: bool,
    generated: bool,
    filename: bool,
}

#[error_set]
pub enum FilterError {}

/// A path [`Filter::excludes`] matches against, in the forms a match reads.
///
/// A walk that builds a path up a component at a time asks about the buffer it
/// builds it in; every other caller asks about a finished path. Only `excludes`
/// is asked from both, so only `excludes` takes this.
pub trait FilterPath {
    /// Whether the path names nothing.
    fn is_empty(&self) -> bool;

    /// The path as it is written.
    fn as_str(&self) -> &str;

    /// The path folded to lowercase, which the globs are matched against.
    fn as_lowercase_str(&self) -> &str;

    /// The last component of the lowercase form.
    fn name_lowercase(&self) -> &str;
}

impl FilterPath for RelativePath {
    fn is_empty(&self) -> bool {
        RelativePath::is_empty(self)
    }

    fn as_str(&self) -> &str {
        RelativePath::as_str(self)
    }

    fn as_lowercase_str(&self) -> &str {
        RelativePath::as_lowercase_str(self)
    }

    fn name_lowercase(&self) -> &str {
        RelativePath::name_lowercase(self)
    }
}

impl FilterPath for RelativePathBuf {
    fn is_empty(&self) -> bool {
        RelativePathBuf::is_empty(self)
    }

    fn as_str(&self) -> &str {
        RelativePathBuf::as_str(self)
    }

    fn as_lowercase_str(&self) -> &str {
        RelativePathBuf::as_lowercase_str(self)
    }

    fn name_lowercase(&self) -> &str {
        RelativePathBuf::name_lowercase(self)
    }
}

pub fn load(
    ignore_path: impl AsRef<Path>,
    view_path: impl AsRef<Path>,
) -> Result<Filter, FilterError> {
    let mut ignore = load_filter(ignore_path)?;
    ignore.add_exclusion(DOT_URC)?;
    ignore.add_exclusion(DOT_LORE)?;
    ignore.add_exclusion(&format!("*{MINE_SUFFIX}"))?;
    ignore.add_exclusion(&format!("*{THEIRS_SUFFIX}"))?;
    ignore.add_exclusion(&format!("*{BASE_SUFFIX}"))?;
    ignore.add_exclusion(&format!("*{TEMP_FILE_EXTENSION}"))?;

    let view = load_filter(view_path)?;

    Ok(Filter { ignore, view })
}

pub fn load_view(view_path: impl AsRef<Path>) -> Result<Filter, FilterError> {
    Ok(Filter {
        ignore: FilterInstance::default(),
        view: load_filter(view_path)?,
    })
}

pub fn load_filter(path: impl AsRef<Path>) -> Result<FilterInstance, FilterError> {
    let mut filter = FilterInstance::default();
    if let Ok(file) = File::open(path) {
        let mut has_include = false;
        let mut has_exclude = false;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let mut glob = line.trim();
            if glob.is_empty() || glob.starts_with('#') {
                continue;
            }

            let mut negated = false;
            while glob.starts_with('!') {
                negated = !negated;
                glob = &glob[1..];
            }

            // Allow exclamation marks in path/file names through escape backslash
            if glob.starts_with("\\!") {
                glob = &glob[1..];
            }

            if negated {
                filter.add_inclusion(glob)?;
                has_include = true;
            } else {
                filter.add_exclusion(glob)?;
                has_exclude = true;
            }
        }

        if has_include && !has_exclude {
            lore_warn!(
                "Filter only has inclusions but no exclusions, this will not have any effect - did you forget to exclude all?"
            );
        }
    }
    Ok(filter)
}

pub fn save(filter: &FilterInstance, path: impl AsRef<Path>) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    for line in &filter.lines {
        if line.generated {
            continue;
        }

        let mut out = String::new();
        if line.negated {
            out.push('!');
        }
        if !line.filename && !line.glob.contains('/') {
            out.push('/');
        }
        out.push_str(&line.glob);
        if line.directory {
            out.push('/');
        }
        out.push('\n');

        file.write_all(out.as_bytes())?;
    }
    Ok(())
}

impl FilterInstance {
    pub fn add_exclusion(&mut self, glob: &str) -> Result<(), FilterError> {
        let leading_separator = glob.starts_with('/');
        let ending_separator = glob.ends_with('/');

        let glob = glob.trim_matches('/').to_lowercase();

        let filename = !leading_separator && !glob.contains('/') && glob != "**";
        {
            self.lines.push(FilterLine {
                glob: glob.clone(),
                negated: false,
                directory: ending_separator,
                generated: false,
                filename,
            });
        }
        if !filename || ending_separator {
            // If this item turns out to be a directory, and a subpath of this item
            // gets re-included by a later rule, we must ensure that everything else
            // in this subtree is properly excluded
            if !glob.ends_with('*') && !glob.ends_with("*/") {
                let mut glob = glob;
                if !glob.ends_with('/') {
                    glob.push('/');
                }
                glob.push_str("**");
                self.lines.push(FilterLine {
                    glob,
                    negated: false,
                    directory: false,
                    generated: true,
                    filename: false,
                });
            }
        }
        Ok(())
    }

    pub fn add_inclusion(&mut self, glob: &str) -> Result<(), FilterError> {
        if glob.starts_with("**") {
            return Err(FilterError::internal(
                "filter inclusions cannot start with ** as that will force traversal of the entire revision tree",
            ));
        }

        let leading_separator = glob.starts_with('/');
        let ending_separator = glob.ends_with('/');

        let glob = glob.trim_matches('/').to_lowercase();

        let filename = !leading_separator && !glob.contains('/');
        if !filename {
            // In order to properly force traversal of excluded directories to reach reincluded subpaths, like
            // Engine
            // !Engine/Sub/Path
            // we must add a directory match reinclusion of Engine and Engine/Sub in order to reach the reincluded
            // subpath Engine/Sub/Path - but if Engine/Sub is a file it should NOT be reincluded. Use a directory
            // match flag to achieve this
            let mut subpath = RelativePathBuf::new();
            let mut path_parts: Vec<&str> = glob.split('/').collect();
            path_parts.pop();
            for part in path_parts.iter() {
                subpath.push(part);
                self.lines.push(FilterLine {
                    glob: subpath.as_lowercase_str().to_owned(),
                    negated: true,
                    directory: true,
                    generated: true,
                    filename: false,
                });
            }
        }

        self.lines.push(FilterLine {
            glob: glob.clone(),
            negated: true,
            directory: ending_separator,
            generated: false,
            filename,
        });

        if !filename {
            // Now, in order to make sure we also include anything below this path
            // add a glob pattern to re-include all the subtree items
            if !glob.ends_with('*') && !glob.ends_with("*/") {
                let mut glob = glob;
                if !glob.ends_with('/') {
                    glob.push('/');
                }
                glob.push_str("**");
                self.lines.push(FilterLine {
                    glob,
                    negated: true,
                    directory: false,
                    generated: true,
                    filename: false,
                });
            }
        }

        Ok(())
    }

    /// Returns whether `path` is excluded by applying every filter line in
    /// order, where a later matching line overrides earlier ones.
    ///
    /// An inclusion (negated) line can only clear `excluded` and an exclusion
    /// line can only set it, so a line whose effect equals the current state
    /// could at most match to no effect. Such lines are skipped before the glob
    /// match, which avoids evaluating inclusion lines while not yet excluded and
    /// exclusion lines while already excluded.
    pub fn excludes(&self, path: &impl FilterPath, is_directory: bool) -> bool {
        if path.is_empty() || path.as_str() == "." {
            return false;
        }
        let mut excluded = false;
        let match_path = path.as_lowercase_str();
        let match_filename = path.name_lowercase();
        for line in &self.lines {
            if line.negated != excluded {
                continue;
            }
            if line.directory && !is_directory {
                continue;
            }
            let to_match = if line.filename {
                match_filename
            } else {
                match_path
            };
            if glob_match::glob_match(line.glob.as_str(), to_match) {
                excluded = !line.negated;
            }
        }
        excluded
    }

    /// Returns whether every path below `path` is excluded, at any depth.
    ///
    /// Excluding a directory node says nothing about the files under it, so two
    /// conditions must hold:
    ///
    /// - No inclusion line exists. Only an inclusion can clear `excluded`, so
    ///   any of them could re-include part of the subtree.
    /// - Some exclusion line is `<prefix>/**` where `path` is `<prefix>` or
    ///   sits below it. Such a rule matches every descendant at every depth.
    ///
    /// Returns `false` when either condition fails. `false` is always safe.
    pub fn excludes_subtree(&self, path: &RelativePath) -> bool {
        if path.is_empty() || path.as_str() == "." {
            return false;
        }
        if self.lines.iter().any(|line| line.negated) {
            return false;
        }
        let path = path.as_lowercase_str();
        self.lines.iter().any(|line| {
            if line.filename || line.directory {
                return false;
            }
            let Some(prefix) = line.glob.strip_suffix("/**") else {
                return false;
            };
            path == prefix
                || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
        })
    }
}

/// Data for the event emitted when a path is excluded by a filter.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFilterExcludeEventData {
    /// Reason the path was excluded.
    pub reason: u8,
    /// Path that was excluded.
    pub path: LoreString,
}

pub enum FilterReason {
    Ignore = 0,
    View,
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FilterMode: u16 {
        const Ignore = 0b1;
        const View = 0b10;
        const Full = 0b11;
    }
}
bitflagsops!(FilterMode, u16);

impl Filter {
    pub fn excludes(&self, path: &impl FilterPath, is_directory: bool, mode: FilterMode) -> bool {
        if path.is_empty() {
            return false;
        }
        if mode.contains(FilterMode::Ignore) && self.ignore.excludes(path, is_directory) {
            return true;
        }
        if mode.contains(FilterMode::View) && self.view.excludes(path, is_directory) {
            return true;
        }
        false
    }

    /// Whether every path below `path` is excluded, for the slots in `mode`.
    ///
    /// See [`FilterInstance::excludes_subtree`]. `excludes` answers only for
    /// the single path it is given.
    pub fn excludes_subtree(&self, path: &RelativePath, mode: FilterMode) -> bool {
        (mode.contains(FilterMode::Ignore) && self.ignore.excludes_subtree(path))
            || (mode.contains(FilterMode::View) && self.view.excludes_subtree(path))
    }

    pub fn emit_excludes(&self, path: &RelativePath, is_directory: bool, mode: FilterMode) -> bool {
        if path.is_empty() {
            return false;
        }
        if mode.contains(FilterMode::Ignore) && self.ignore.excludes(path, is_directory) {
            LoreEvent::FilterExclude(LoreFilterExcludeEventData {
                reason: FilterReason::Ignore as u8,
                path: path.into(),
            })
            .send();
            return true;
        }
        if mode.contains(FilterMode::View) && self.view.excludes(path, is_directory) {
            LoreEvent::FilterExclude(LoreFilterExcludeEventData {
                reason: FilterReason::View as u8,
                path: path.into(),
            })
            .send();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_str(value).expect("valid relative path")
    }

    fn view(build: impl FnOnce(&mut FilterInstance)) -> FilterInstance {
        let mut instance = FilterInstance::default();
        build(&mut instance);
        instance
    }

    #[test]
    fn excludes_subtree_accepts_a_directory_excluded_at_every_depth() {
        let view = view(|v| v.add_exclusion("engine/**").expect("exclude"));
        assert!(view.excludes_subtree(&path("engine")));
    }

    #[test]
    fn excludes_subtree_accepts_a_directory_below_an_excluded_root() {
        let view = view(|v| v.add_exclusion("engine/**").expect("exclude"));
        assert!(view.excludes_subtree(&path("engine/deep")));
        assert!(view.excludes_subtree(&path("engine/deep/nested")));
    }

    #[test]
    fn excludes_subtree_accepts_a_rooted_exclusion_that_generates_the_subtree_rule() {
        // A rooted path exclusion generates the matching `engine/**` line.
        let view = view(|v| v.add_exclusion("/engine").expect("exclude"));
        assert!(view.excludes_subtree(&path("engine")));
    }

    #[test]
    fn excludes_subtree_rejects_a_bare_name_exclusion() {
        // `engine` with no separator is a filename rule. It excludes the
        // directory node but leaves every file under it in view.
        let view = view(|v| v.add_exclusion("engine").expect("exclude"));
        assert!(view.excludes(&path("engine"), true));
        assert!(!view.excludes(&path("engine/file.txt"), false));
        assert!(!view.excludes_subtree(&path("engine")));
    }

    #[test]
    fn excludes_subtree_rejects_a_rule_that_stops_at_one_level() {
        // `engine/*` excludes `engine/file.txt` but leaves
        // `engine/deep/file.txt` in view.
        let view = view(|v| v.add_exclusion("engine/*").expect("exclude"));
        assert!(view.excludes(&path("engine/file.txt"), false));
        assert!(!view.excludes_subtree(&path("engine")));
    }

    #[test]
    fn excludes_subtree_rejects_any_view_holding_a_re_inclusion() {
        // A re-inclusion puts part of the subtree back in view.
        let view = view(|v| {
            v.add_exclusion("engine/**").expect("exclude");
            v.add_inclusion("engine/keep").expect("re-include");
        });
        assert!(!view.excludes_subtree(&path("engine")));
        assert!(!view.excludes_subtree(&path("engine/other")));
    }

    #[test]
    fn excludes_subtree_rejects_an_unrelated_or_partially_matching_path() {
        let view = view(|v| v.add_exclusion("engine/**").expect("exclude"));
        assert!(!view.excludes_subtree(&path("game")));
        // A sibling sharing the excluded prefix as a string is not below it.
        assert!(!view.excludes_subtree(&path("engineering")));
    }

    #[test]
    fn excludes_subtree_ignores_glob_case() {
        // `add_exclusion` lowercases the glob and `excludes_subtree` lowercases
        // the path, so the two meet regardless of how either was written.
        let view = view(|v| v.add_exclusion("Engine/**").expect("exclude"));
        assert!(view.excludes_subtree(&path("engine")));
        assert!(view.excludes_subtree(&path("Engine")));
    }

    #[test]
    fn excludes_subtree_accepts_a_trailing_separator_exclusion() {
        // A trailing separator marks the entry as a directory, which also
        // generates the `engine/**` line that covers the contents.
        let view = view(|v| v.add_exclusion("engine/").expect("exclude"));
        assert!(view.excludes_subtree(&path("engine")));
    }

    #[test]
    fn excludes_subtree_rejects_a_bare_double_star() {
        // `**` excludes every path, but it names no prefix, so there is nothing
        // to compare a subtree against. Refusing costs only the optimization.
        let view = view(|v| v.add_exclusion("**").expect("exclude"));
        assert!(view.excludes(&path("engine/file.txt"), false));
        assert!(!view.excludes_subtree(&path("engine")));
    }

    #[test]
    fn excludes_subtree_rejects_a_wildcard_in_the_prefix() {
        // The prefix is compared as text, so a wildcard in it never matches a
        // real path. `engine*/**` does exclude the subtree, but proving that
        // would mean evaluating the glob, so it is refused instead.
        let view = view(|v| v.add_exclusion("engine*/**").expect("exclude"));
        assert!(view.excludes(&path("engine1/file.txt"), false));
        assert!(!view.excludes_subtree(&path("engine1")));
    }

    #[test]
    fn excludes_subtree_rejects_a_directory_only_subtree_rule() {
        // A directory-only line is skipped for files, so it cannot prove the
        // files below the subtree are excluded.
        let view = view(|v| v.add_exclusion("engine/**/").expect("exclude"));
        assert!(!view.excludes_subtree(&path("engine")));
    }

    #[test]
    fn excludes_subtree_rejects_the_empty_path() {
        let view = view(|v| v.add_exclusion("engine/**").expect("exclude"));
        assert!(!view.excludes_subtree(&RelativePath::new()));
    }
}
