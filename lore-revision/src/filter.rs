// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bitflags::bitflags;
use dashmap::DashMap;
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
    /// Folded ancestor verdicts, shared across clones. See [`AncestorMemo`].
    memo: AncestorMemo,
}

#[derive(Clone, Default, Debug)]
pub struct FilterInstance {
    /// Match lines in authored order. Every authored rule contributes exactly
    /// one line, except a path-form inclusion, which also contributes the
    /// subtree companion described on [`add_inclusion`](Self::add_inclusion).
    pub lines: Vec<FilterLine>,
    /// Answers whether any inclusion can land below a directory, which is what
    /// decides descent into an excluded one. Built as the lines are added.
    reinclude: ReincludeIndex,
}

/// One glob and the two facts about the authored rule that the glob text cannot
/// carry on its own.
///
/// `filename` says the rule applies at any depth, so the glob is matched against
/// the path's last component instead of the whole path. It could be folded into
/// the glob as a `**/` prefix -- gitignore documents `**/foo` as meaning the same
/// as `foo` -- but comparing a short name against a simple glob is cheaper than
/// comparing a whole path against one that has to backtrack across separators,
/// and this runs for every path a walk visits. `compile` goes the other way
/// instead, turning an authored `**/foo` into a name rule.
///
/// `directory` is a predicate on the node, not on the path text, so no glob can
/// express it at all.
#[derive(Default, Clone, Debug)]
pub struct FilterLine {
    glob: String,
    negated: bool,
    directory: bool,
    filename: bool,
    /// Emitted by [`FilterInstance::add_inclusion`] rather than authored, so
    /// [`save`] leaves it out. Read nowhere else.
    generated: bool,
    /// Fewest path components this line can match, so a prefix shorter than that
    /// is skipped without running the glob. A whole-path query folds over every
    /// ancestor, and most lines cannot possibly match the shallow ones.
    min_depth: u32,
    /// The glob holds no metacharacter, so matching it is a string comparison
    /// rather than a glob evaluation. Most lines in a real filter are literal
    /// paths or names, and this runs for every line on every path a walk visits.
    literal: bool,
}

#[error_set]
pub enum FilterError {}

/// Where a walk has got to: the verdict for the directory it is standing in,
/// and the line that produced it.
///
/// `decided_at` names the line that produced the verdict, and floors two
/// searches: [`FilterInstance::step`] skips the lines before it, so a rule that
/// already lost at an excluded ancestor cannot win below it, and
/// `ReincludeIndex::below` ignores re-inclusions before it, so a rule that lost
/// cannot force a descent either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilterState {
    excluded: bool,
    decided_at: u32,
}

impl FilterState {
    /// The state at the repository root, where nothing is excluded yet.
    pub const ROOT: Self = Self {
        excluded: false,
        decided_at: 0,
    };

    /// Whether the path this state describes is excluded.
    pub fn excluded(&self) -> bool {
        self.excluded
    }
}

impl Default for FilterState {
    fn default() -> Self {
        Self::ROOT
    }
}

/// A path a filter matches against, in the forms a match reads.
///
/// A walk that builds a path up a component at a time asks about the buffer it
/// builds it in; every other caller asks about a finished path.
pub trait FilterPath {
    /// Whether the path names nothing.
    fn is_empty(&self) -> bool;

    /// The path as it is written.
    fn as_str(&self) -> &str;

    /// The path folded to lowercase, which the globs are matched against.
    fn as_lowercase_str(&self) -> &str;

    /// The last component of the lowercase form, which a `filename` line is
    /// matched against.
    fn name_lowercase(&self) -> &str;

    /// The lowercase form split at the last separator: everything above the last
    /// component, and the component itself. The first half is empty when the path
    /// names a single component.
    ///
    /// One scan for both halves, which a whole-path query needs together.
    fn split_lowercase(&self) -> (&str, &str);
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

    fn split_lowercase(&self) -> (&str, &str) {
        RelativePath::split_lowercase(self)
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

    fn split_lowercase(&self) -> (&str, &str) {
        RelativePathBuf::split_lowercase(self)
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

    Ok(Filter {
        ignore,
        view,
        memo: AncestorMemo::default(),
    })
}

pub fn load_view(view_path: impl AsRef<Path>) -> Result<Filter, FilterError> {
    Ok(Filter {
        ignore: FilterInstance::default(),
        view: load_filter(view_path)?,
        memo: AncestorMemo::default(),
    })
}

/// Reads a filter file, one rule per line.
///
/// A rule this filter cannot express is reported and skipped, and the rest of
/// the file still applies. Aborting instead would discard every rule in the
/// file, including the ones before the offending line, and a filter that
/// silently excludes nothing is far more damaging than one missing rule: the
/// only signal the author gets is that everything they meant to ignore turns up
/// as changed.
pub fn load_filter(path: impl AsRef<Path>) -> Result<FilterInstance, FilterError> {
    let path = path.as_ref();
    let mut filter = FilterInstance::default();
    if let Ok(file) = File::open(path) {
        let mut has_include = false;
        let mut has_exclude = false;
        for (index, line) in BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .enumerate()
        {
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

            let added = if negated {
                filter.add_inclusion(glob)
            } else {
                filter.add_exclusion(glob)
            };

            match added {
                Ok(()) => {
                    if negated {
                        has_include = true;
                    } else {
                        has_exclude = true;
                    }
                }
                Err(error) => {
                    lore_warn!(
                        "{}:{}: ignoring unsupported rule `{}`: {error}. The remaining rules in this file still apply.",
                        path.display(),
                        index + 1,
                        line.trim(),
                    );
                }
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

/// Writes the authored rules back out, in order.
///
/// Reconstructed from the compiled lines: a name rule is written as it stands, a
/// rooted single-component rule regains its leading separator, and a
/// directory-only rule its trailing one. An authored `**/foo` comes back as
/// `foo`, which gitignore defines as the same rule.
pub fn save(filter: &FilterInstance, path: impl AsRef<Path>) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut out = String::new();
    for line in filter.lines.iter().filter(|line| !line.generated) {
        out.clear();
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

/// How many components a non-empty relative path has.
fn component_count(path: &str) -> u32 {
    path.bytes().filter(|byte| *byte == b'/').count() as u32 + 1
}

/// Whether a whole glob is plain text, so a comparison decides it.
///
/// A backslash counts, because the glob engine unescapes it and a comparison
/// would not. So does an unpaired `]`, which the engine treats as a literal:
/// pairing it with its `[` costs more than letting the glob take the slower
/// path.
fn is_literal(glob: &str) -> bool {
    !glob.contains(['*', '?', '[', ']', '\\'])
}

/// Whether a single path component holds a glob metacharacter, so no literal
/// text can stand in for it.
fn has_wildcard(component: &str) -> bool {
    component.contains(['*', '?', '['])
}

/// Yields every ancestor of `full` from the root down, and finally `full`
/// itself. An ancestor is by definition a directory, so only the final item
/// carries the caller's `is_directory`.
///
/// Everything borrows `full` and each split hands the next component's start
/// offset forward, so walking a path costs no allocation and no repeated
/// separator search.
///
/// `full` is expected to be non-empty. A leading separator would yield an empty
/// prefix, which a `**` rule matches and no rule should be tested against, so it
/// is skipped and does not count towards the depth.
fn path_prefixes(full: &str, is_directory: bool) -> impl Iterator<Item = (&str, &str, bool, u32)> {
    let mut name_start = 0;
    let mut search_from = 0;
    let mut finished = false;
    let mut depth = 0u32;
    std::iter::from_fn(move || {
        while !finished {
            let Some(separator) = full[search_from..].find('/') else {
                finished = true;
                depth += 1;
                return Some((full, &full[name_start..], is_directory, depth));
            };
            let end = search_from + separator;
            let prefix = &full[..end];
            let name = &full[name_start..end];
            name_start = end + 1;
            search_from = end + 1;
            if !prefix.is_empty() {
                depth += 1;
                return Some((prefix, name, true, depth));
            }
        }
        None
    })
}

/// Stands for "no inclusion here", so comparing against a line index needs no
/// `Option` unwrapping.
const NO_LINE: i64 = -1;

/// Literal-prefix index over the inclusion lines, answering whether any
/// re-inclusion can land below a directory without evaluating a glob.
///
/// A walk needs that answer for every excluded directory it meets, to decide
/// whether to descend anyway. Deriving it from the lines would cost a scan of
/// every inclusion per directory, and a filter built from diff paths carries up
/// to `SOURCE_FILTER_THRESHOLD` of them.
///
/// Only the wildcard-free leading components of a glob can be indexed. An
/// inclusion whose first component holds a wildcard could match at any depth, so
/// it forces descent everywhere; that is recorded once in `unanchored`.
/// Over-approximating costs traversal, under-approximating drops re-included
/// content, so every uncertain case answers "descend".
#[derive(Clone, Debug)]
struct ReincludeIndex {
    root: ReincludeNode,
    /// Highest line of an inclusion that can match at any depth, because its
    /// first component holds a wildcard and so no prefix rules it out.
    unanchored: i64,
}

impl Default for ReincludeIndex {
    fn default() -> Self {
        Self {
            root: ReincludeNode::default(),
            unanchored: NO_LINE,
        }
    }
}

#[derive(Clone, Debug)]
struct ReincludeNode {
    /// Component to child. Shallow and narrow in practice, so a `Vec` beats a
    /// map: lookup is a handful of string compares with no hashing.
    children: Vec<(String, ReincludeNode)>,
    /// Highest line of an inclusion sitting strictly below this node.
    below: i64,
    /// Highest line of an inclusion with a wildcard tail starting here, which
    /// could match anything at or below this node.
    wildcard_tail: i64,
}

impl Default for ReincludeNode {
    fn default() -> Self {
        Self {
            children: Vec::new(),
            below: NO_LINE,
            wildcard_tail: NO_LINE,
        }
    }
}

/// Passes a `u64` key straight through.
///
/// The key is already an xxh3 digest of a path, so hashing it again would be a
/// second pass over the only thing the map looks at.
#[derive(Clone, Copy, Default)]
struct DigestHasher(u64);

impl std::hash::Hasher for DigestHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _: &[u8]) {
        debug_assert!(false, "the ancestor memo is keyed by u64 alone");
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

#[derive(Clone, Copy, Default)]
struct DigestHashBuilder;

impl std::hash::BuildHasher for DigestHashBuilder {
    type Hasher = DigestHasher;

    fn build_hasher(&self) -> DigestHasher {
        DigestHasher(0)
    }
}

/// A folded ancestor chain: where the fold got to, and how deep it went.
#[derive(Clone, Copy, Debug)]
struct Ancestor {
    states: FilterStates,
    depth: u32,
}

/// Remembers the folded verdict for a directory, so a batch of paths sharing one
/// pays for its ancestors once.
///
/// A targets file can name a million paths -- see the `MAX_TASKS` note in
/// `file::stage` -- sitting in far fewer directories, and each is a whole-path
/// query that folds from the root. That batch is walked one task per target, so
/// the cache is shared rather than threaded through the caller.
///
/// Keyed by the xxh3 digest of the lowercased directory, the identity a node
/// lookup already matches on -- see [`Node::name_hash`](crate::node::Node) and
/// `State::find_subnode`. Clones share the map.
///
/// One entry per directory the operation asks about. A filter is built per
/// repository open -- see `repository::load_and_connect_with_token` -- and is not
/// cached between them, so the map is freed with the operation and needs no cap.
///
/// `lines` records the line counts both slots had when the map was last valid.
/// [`FilterInstance::add_exclusion`] and [`FilterInstance::add_inclusion`] are
/// public and take `&mut self`, so rules can be added to a filter that has
/// already answered a query; a count that no longer matches empties the map.
/// Mutation needs `&mut Filter` and a query needs `&Filter`, so the two cannot
/// overlap; a query only ever loads, and stores when it finds a mismatch.
#[derive(Clone, Debug, Default)]
struct AncestorMemo {
    entries: Arc<DashMap<u64, Ancestor, DigestHashBuilder>>,
    lines: Arc<AtomicU64>,
}

impl AncestorMemo {
    /// Empties the map when either slot has gained lines since the last call.
    fn revalidate(&self, ignore: usize, view: usize) {
        let lines = (ignore as u64) << 32 | view as u64;
        if self.lines.load(Ordering::Relaxed) != lines {
            self.lines.store(lines, Ordering::Relaxed);
            self.entries.clear();
        }
    }
}

impl ReincludeIndex {
    /// Records that the inclusion on line `line` matches `glob`.
    ///
    /// Each node on the way down gains `line` as something below it -- at least
    /// the next component, maybe deeper. A wildcard component ends the descent and
    /// marks the node it stops at, because no literal text stands in for it. A
    /// glob that is literal throughout names its final node and marks nothing
    /// below it.
    fn insert(&mut self, glob: &str, line: usize) {
        let line = line as i64;
        let mut node = &mut self.root;
        for (index, component) in glob.split('/').enumerate() {
            if has_wildcard(component) {
                if index == 0 {
                    self.unanchored = self.unanchored.max(line);
                }
                node.wildcard_tail = node.wildcard_tail.max(line);
                return;
            }
            node.below = node.below.max(line);
            let position = node
                .children
                .iter()
                .position(|(name, _)| name == component)
                .unwrap_or_else(|| {
                    node.children
                        .push((component.to_owned(), ReincludeNode::default()));
                    node.children.len() - 1
                });
            node = &mut node.children[position].1;
        }
    }

    /// Whether an inclusion on line `floor` or later can match a path strictly
    /// below `path`.
    ///
    /// `floor` is the line that excluded `path`. An inclusion before it already
    /// lost there and [`FilterInstance::step`] will not consult it again below,
    /// so it cannot re-include anything and must not force a descent. Without
    /// that comparison the subtree companion of any re-inclusion would keep every
    /// excluded sibling of its own directory reachable.
    ///
    /// Returns early when nothing in the index is late enough to matter, and when
    /// the walk falls off the indexed branches.
    fn below(&self, path: &str, floor: u32) -> bool {
        let floor = floor as i64;
        if self.unanchored >= floor {
            return true;
        }
        let mut node = &self.root;
        if node.below < floor {
            return false;
        }
        for component in path.split('/') {
            if node.wildcard_tail >= floor {
                return true;
            }
            match node
                .children
                .iter()
                .find(|(name, _)| name.as_str() == component)
            {
                Some((_, child)) => node = child,
                None => return false,
            }
        }
        node.below >= floor || node.wildcard_tail >= floor
    }
}

impl FilterInstance {
    /// Normalizes an authored rule into `(glob, filename, directory)`.
    ///
    /// Matching is case-insensitive: the glob is folded to lowercase here and
    /// every path is matched in its lowercase form. Git is case-sensitive by
    /// default; lore is not, because it serves case-insensitive filesystems and
    /// a rule that matched only one spelling of a name would let the same
    /// content in or out depending on how it happened to be written.
    ///
    /// A rule with no separator applies at any depth and is matched against the
    /// path's last component. `**/name` means the same thing -- gitignore
    /// documents `**/foo` as "the same as pattern `foo`" -- so the prefix is
    /// stripped and the rule becomes a name rule, which keeps the commonest
    /// patterns off `**` and its separator-crossing backtrack.
    ///
    /// `**/a/b` is not the same as `a/b`: it names `b` directly under any `a`, so
    /// it keeps the prefix and is matched against the whole path. A bare `**`
    /// names no component at all and is matched against the whole path too.
    fn compile(glob: &str) -> (String, bool, bool) {
        let leading_separator = glob.starts_with('/');
        let ending_separator = glob.ends_with('/');
        let mut glob = glob.trim_matches('/').to_lowercase();

        if !leading_separator
            && let Some(rest) = glob.strip_prefix("**/")
            && !rest.contains('/')
            && !rest.is_empty()
        {
            glob = rest.to_owned();
        }

        let filename = !leading_separator && !glob.contains('/') && glob != "**";
        (glob, filename, ending_separator)
    }

    /// Fewest path components `glob` can match.
    ///
    /// Each component consumes one, except a `**` that is not the last: that one
    /// may absorb nothing, so `a/**/b` can match `a/b`. A trailing `**` needs a
    /// component of its own, which is what makes `a/**` not match `a`. A name rule
    /// is matched against the last component and so applies at any depth.
    fn min_depth(glob: &str, filename: bool) -> u32 {
        if filename {
            return 1;
        }
        let mut total = 0u32;
        let mut optional = 0u32;
        // Reaching another component proves the previous `**` was not the last.
        let mut previous_was_globstar = false;
        for component in glob.split('/') {
            optional += u32::from(previous_was_globstar);
            total += 1;
            previous_was_globstar = component == "**";
        }
        (total - optional).max(1)
    }

    /// Appends an exclusion.
    ///
    /// One line, and no subtree companion: a walk does not descend past an
    /// excluded directory unless something below it is re-included, and where it
    /// does descend the state carried into [`step`](Self::step) keeps the subtree
    /// excluded without a rule saying so.
    pub fn add_exclusion(&mut self, glob: &str) -> Result<(), FilterError> {
        let (glob, filename, ending_separator) = Self::compile(glob);
        let min_depth = Self::min_depth(&glob, filename);
        let literal = is_literal(&glob);
        self.lines.push(FilterLine {
            glob,
            negated: false,
            directory: ending_separator,
            filename,
            generated: false,
            min_depth,
            literal,
        });
        Ok(())
    }

    /// Appends a re-inclusion, and for a path rule the companion that carries it
    /// over the subtree.
    ///
    /// A blanket exclusion such as `**` matches at every depth, so re-including a
    /// directory says nothing about its contents on its own; the companion
    /// `<glob>/**` says it. A glob already ending in a wildcard covers its own
    /// subtree as far as it ever will and gets none.
    ///
    /// The companion is never directory-only, whatever the authored rule was:
    /// `engine/` excludes what is under it, so `!engine/` re-includes it.
    ///
    /// Only a path rule gets a companion, and only a path rule enters the
    /// re-inclusion index. A name rule is skipped by [`step`](Self::step) below an
    /// excluded directory, so `!keep.txt` neither re-opens a pruned subtree nor
    /// sends a walk looking for one -- which is how git reads it too: with `*` and
    /// `!keep.txt`, only the top-level `keep.txt` comes back.
    ///
    /// # Errors
    ///
    /// A `**`-prefixed inclusion naming more than one component, such as
    /// `!**/a/b`. No prefix bounds where it could match, so honouring it would
    /// send a walk into every excluded directory in the repository. `!**/name`
    /// compiles to `!name` and is accepted.
    pub fn add_inclusion(&mut self, raw: &str) -> Result<(), FilterError> {
        let (glob, filename, ending_separator) = Self::compile(raw);
        if raw.starts_with("**") && !filename {
            return Err(FilterError::internal(
                "filter inclusions cannot start with ** as that will force traversal of the entire revision tree",
            ));
        }

        let subtree = (!filename && !glob.ends_with('*')).then(|| format!("{glob}/**"));

        if !filename {
            self.reinclude.insert(&glob, self.lines.len());
        }
        self.lines.push(FilterLine {
            min_depth: Self::min_depth(&glob, filename),
            literal: is_literal(&glob),
            glob,
            negated: true,
            directory: ending_separator,
            filename,
            generated: false,
        });

        if let Some(subtree) = subtree {
            self.reinclude.insert(&subtree, self.lines.len());
            self.lines.push(FilterLine {
                min_depth: Self::min_depth(&subtree, false),
                literal: false,
                glob: subtree,
                negated: true,
                directory: false,
                filename: false,
                generated: true,
            });
        }

        Ok(())
    }

    /// Advances the walk one level: the verdict for `path`, given the verdict
    /// for its parent.
    ///
    /// Lines are applied in order and a later match overrides an earlier one.
    /// Two things narrow the scan:
    ///
    /// - An inclusion can only clear `excluded` and an exclusion can only set
    ///   it, so a line whose effect equals the current state could at most match
    ///   to no effect, and is skipped before the glob.
    /// - A name rule is skipped when the parent is excluded: it cannot reach into
    ///   a directory a walk would have pruned.
    /// - A line needing more components than `depth` cannot match and is skipped.
    /// - When the parent is excluded, lines before the one that excluded it are
    ///   skipped. A tree walk stops at an excluded directory, so a rule that
    ///   already lost there must not win below it -- that is what keeps
    ///   `!/src` + `/src/*` from re-including `src/drop/y` through the `src/**`
    ///   companion. When the parent is *included* nothing was pruned, so every
    ///   line applies and an unanchored `*.tmp` still bites inside a re-included
    ///   subtree.
    fn step(
        &self,
        parent: FilterState,
        match_path: &str,
        match_name: &str,
        is_directory: bool,
        depth: u32,
    ) -> FilterState {
        let floor = if parent.excluded {
            parent.decided_at as usize
        } else {
            0
        };
        let pruned = parent.excluded;
        let mut state = parent;
        for (offset, line) in self.lines[floor..].iter().enumerate() {
            if line.negated != state.excluded {
                continue;
            }
            if line.directory && !is_directory {
                continue;
            }
            if pruned && line.negated && line.filename {
                continue;
            }
            if line.min_depth > depth {
                continue;
            }
            let to_match = if line.filename {
                match_name
            } else {
                match_path
            };
            let hit = if line.literal {
                line.glob == to_match
            } else {
                glob_match::glob_match(line.glob.as_str(), to_match)
            };
            if hit {
                state = FilterState {
                    excluded: !line.negated,
                    decided_at: (floor + offset) as u32,
                };
            }
        }
        state
    }

    /// The exclusion verdict for `path`, given its parent's.
    ///
    /// The walk form of [`excludes`](Self::excludes): one pass over the lines
    /// rather than one per ancestor, because the caller already holds the
    /// parent's verdict. Read the answer with
    /// [`FilterState::excluded`], and pass the returned state to
    /// [`should_descend`](Self::should_descend) and to the children below it.
    pub fn child_exclusion_state(
        &self,
        parent: FilterState,
        path: &impl FilterPath,
        is_directory: bool,
    ) -> FilterState {
        if path.is_empty() || path.as_str() == "." {
            return parent;
        }
        let lowercase = path.as_lowercase_str();
        self.step(
            parent,
            lowercase,
            path.name_lowercase(),
            is_directory,
            component_count(lowercase),
        )
    }

    /// The exclusion verdict for a path that arrives whole, with no walk behind
    /// it.
    ///
    /// Folds down the path's own ancestors, which is what a walk would have
    /// done. A path named on the command line, replayed from a change list or
    /// resolved from a clone dependency has to reproduce that, or it gets an
    /// answer about the pattern rather than about the path.
    pub fn exclusion_state(&self, path: &impl FilterPath, is_directory: bool) -> FilterState {
        self.exclusion_state_settled(path, is_directory).0
    }

    /// [`exclusion_state`](Self::exclusion_state), also reporting whether the
    /// fold ended on a prefix that settles its whole subtree.
    ///
    /// The fold already asks that of every prefix to know when to stop, so a
    /// caller wanting the answer for `path` itself takes it from here rather than
    /// searching the index a second time.
    fn exclusion_state_settled(
        &self,
        path: &impl FilterPath,
        is_directory: bool,
    ) -> (FilterState, bool) {
        if path.is_empty() || path.as_str() == "." {
            return (FilterState::ROOT, false);
        }
        let mut state = FilterState::ROOT;
        for (prefix, name, prefix_is_directory, depth) in
            path_prefixes(path.as_lowercase_str(), is_directory)
        {
            state = self.step(state, prefix, name, prefix_is_directory, depth);
            if self.settles_subtree(state, prefix) {
                return (state, true);
            }
        }
        (state, false)
    }

    /// Whether `state` excludes `path` and no rule can re-include anything below
    /// it, so every descendant is excluded too.
    ///
    /// A walk stops descending here, a fold stops folding here, and
    /// [`excludes_subtree`](Self::excludes_subtree) reports it. `path` is the
    /// lowercase form `state` was produced for.
    fn settles_subtree(&self, state: FilterState, path: &str) -> bool {
        state.excluded && !self.reinclude.below(path, state.decided_at)
    }

    /// One [`step`](Self::step) from `parent`, reduced to the verdict the caller
    /// asked for: whether `match_path` is excluded, or -- with `subtree` --
    /// whether it is excluded and settles everything below it.
    fn excludes_step(
        &self,
        parent: FilterState,
        match_path: &str,
        match_name: &str,
        is_directory: bool,
        depth: u32,
        subtree: bool,
    ) -> bool {
        let state = self.step(parent, match_path, match_name, is_directory, depth);
        if subtree {
            self.settles_subtree(state, match_path)
        } else {
            state.excluded
        }
    }

    /// Whether `path` is excluded by this slot alone, its ancestors accounted for.
    ///
    /// Folds the ancestors on every call. [`Filter::excludes`] answers the same
    /// question across both slots and folds a directory once, so it is what a
    /// caller wants; this is the single-slot form, and the unmemoized one the
    /// filter tests hold the memoized answers against.
    pub fn excludes(&self, path: &impl FilterPath, is_directory: bool) -> bool {
        self.exclusion_state(path, is_directory).excluded
    }

    /// Whether a walk standing at `path` should descend into it.
    ///
    /// An excluded directory is still descended when a rule could re-include
    /// something below it; the contents stay excluded unless such a rule
    /// actually matches them. Git prunes there instead and documents
    /// re-inclusion below an excluded directory as impossible -- one of the
    /// departures `tests/filter_gitignore.rs` enumerates and asserts.
    pub fn should_descend(&self, state: FilterState, path: &impl FilterPath) -> bool {
        if path.is_empty() || path.as_str() == "." {
            return true;
        }
        !self.settles_subtree(state, path.as_lowercase_str())
    }

    /// Whether every path below `path` is excluded, at any depth.
    ///
    /// The negation of [`should_descend`](Self::should_descend) for an excluded
    /// directory: nothing below can be re-included, so nothing below is in.
    pub fn excludes_subtree(&self, path: &RelativePath) -> bool {
        if path.is_empty() || path.as_str() == "." {
            return false;
        }
        self.exclusion_state_settled(path, true).1
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

#[derive(Clone, Copy)]
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

/// How far a query's verdict reaches.
///
/// Only a directory can tell the two apart: it can be excluded by its own rule
/// and still hold content a later rule re-includes, and then the answer depends
/// on which question was asked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The path itself. Says nothing about what sits below it.
    Node,
    /// The path and everything below it, which is what a walk has to know
    /// before it drops a directory. See [`Filter::excludes_tree`].
    Tree,
}

/// The two slots' states, carried together so a walk threads one value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilterStates {
    pub ignore: FilterState,
    pub view: FilterState,
}

impl FilterStates {
    pub const ROOT: Self = Self {
        ignore: FilterState::ROOT,
        view: FilterState::ROOT,
    };
}

impl Filter {
    /// The exclusion verdict for `path` in both slots, given its parent's, plus
    /// why it is excluded if it is.
    ///
    /// The walk form of [`excludes`](Self::excludes); see
    /// [`FilterInstance::child_exclusion_state`].
    ///
    /// Ignore is reported before view at the same depth, which is the order a
    /// walk would have hit them.
    pub fn child_exclusion_states(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, Option<FilterReason>) {
        let mut states = parent;
        let mut reason = None;
        if mode.contains(FilterMode::Ignore) {
            states.ignore = self
                .ignore
                .child_exclusion_state(parent.ignore, path, is_directory);
            if states.ignore.excluded {
                reason = Some(FilterReason::Ignore);
            }
        }
        if mode.contains(FilterMode::View) {
            states.view = self
                .view
                .child_exclusion_state(parent.view, path, is_directory);
            if reason.is_none() && states.view.excluded {
                reason = Some(FilterReason::View);
            }
        }
        (states, reason)
    }

    /// Whether a walk should descend into `path`, for the slots in `mode`.
    ///
    /// Per slot: a slot that excludes the directory only permits descent when it
    /// could re-include something below. Content below has to clear both slots,
    /// so a view inclusion cannot resurrect ignore-excluded content.
    pub fn should_descend(
        &self,
        states: FilterStates,
        path: &impl FilterPath,
        mode: FilterMode,
    ) -> bool {
        if mode.contains(FilterMode::Ignore) && !self.ignore.should_descend(states.ignore, path) {
            return false;
        }
        if mode.contains(FilterMode::View) && !self.view.should_descend(states.view, path) {
            return false;
        }
        true
    }

    /// Why `path` is excluded, its ancestors accounted for, or `None` if it is
    /// not. `scope` selects whether the leaf answers for itself or for its whole
    /// subtree.
    ///
    /// The parent is folded once, through [`ancestor`](Self::ancestor); only the
    /// last component is stepped here. A slot that settles the whole subtree at
    /// the parent answers without the step whichever the scope, since no line
    /// below it can change the verdict at the leaf or under it.
    ///
    /// Ignore is consulted before view at each of those two points, so a path
    /// both slots exclude reports `Ignore`. A slot that settles at the parent is
    /// reported ahead of one that only excludes at the leaf.
    fn exclude_reason(
        &self,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
        scope: Scope,
    ) -> Option<FilterReason> {
        if self.is_empty(mode) {
            return None;
        }
        let lowercase = path.as_lowercase_str();
        if lowercase.is_empty() || lowercase == "." {
            return None;
        }
        let (parent, name) = path.split_lowercase();
        let ancestor = self.ancestor(parent);

        if mode.contains(FilterMode::Ignore)
            && self.ignore.settles_subtree(ancestor.states.ignore, parent)
        {
            return Some(FilterReason::Ignore);
        }
        if mode.contains(FilterMode::View)
            && self.view.settles_subtree(ancestor.states.view, parent)
        {
            return Some(FilterReason::View);
        }

        // A file has no subtree, so both scopes ask the same question of one.
        let subtree = scope == Scope::Tree && is_directory;
        let depth = ancestor.depth + 1;
        if mode.contains(FilterMode::Ignore)
            && self.ignore.excludes_step(
                ancestor.states.ignore,
                lowercase,
                name,
                is_directory,
                depth,
                subtree,
            )
        {
            return Some(FilterReason::Ignore);
        }
        if mode.contains(FilterMode::View)
            && self.view.excludes_step(
                ancestor.states.view,
                lowercase,
                name,
                is_directory,
                depth,
                subtree,
            )
        {
            return Some(FilterReason::View);
        }
        None
    }

    /// Whether the slots in `mode` hold no lines, so nothing can be excluded.
    ///
    /// `RepositoryContext::new_server_context` builds a filter with both slots
    /// empty, and answers for it without hashing a path or touching the memo.
    fn is_empty(&self, mode: FilterMode) -> bool {
        (!mode.contains(FilterMode::Ignore) || self.ignore.lines.is_empty())
            && (!mode.contains(FilterMode::View) || self.view.lines.is_empty())
    }

    /// The folded verdict for a directory, from the memo where it is already
    /// known.
    ///
    /// Every component of a parent chain is stepped as a directory, which is what
    /// makes a directory-only rule apply to it.
    ///
    /// Both slots are folded whatever the caller's mode, so one entry serves every
    /// mode. A slot with no lines folds over nothing, which is the usual shape: an
    /// ignore filter with an empty view, or the reverse.
    fn ancestor(&self, parent: &str) -> Ancestor {
        if parent.is_empty() {
            return Ancestor {
                states: FilterStates::ROOT,
                depth: 0,
            };
        }
        self.memo
            .revalidate(self.ignore.lines.len(), self.view.lines.len());
        let key = crate::util::path::lowercase_hash(parent);
        if let Some(hit) = self.memo.entries.get(&key) {
            return *hit;
        }

        let mut ancestor = Ancestor {
            states: FilterStates::ROOT,
            depth: 0,
        };
        for (prefix, component, _, depth) in path_prefixes(parent, true) {
            ancestor.states.ignore =
                self.ignore
                    .step(ancestor.states.ignore, prefix, component, true, depth);
            ancestor.states.view =
                self.view
                    .step(ancestor.states.view, prefix, component, true, depth);
            ancestor.depth = depth;
        }

        self.memo.entries.insert(key, ancestor);
        ancestor
    }

    /// Whether `path` itself is excluded, for the slots in `mode`.
    ///
    /// Says nothing about what sits below it: a directory excluded by its own
    /// rule can still hold re-included content. A caller about to drop a path
    /// *and its subtree* wants [`excludes_tree`](Self::excludes_tree) instead.
    pub fn excludes(&self, path: &impl FilterPath, is_directory: bool, mode: FilterMode) -> bool {
        self.exclude_reason(path, is_directory, mode, Scope::Node)
            .is_some()
    }

    /// Whether a walk can drop `path` without looking inside it.
    ///
    /// For a file this is [`excludes`](Self::excludes). For a directory it is
    /// the stricter question [`excludes_subtree`](Self::excludes_subtree) asks:
    /// an excluded directory is still kept when a rule could re-include
    /// something below it, because the walk has to descend and the node has to
    /// exist to descend into -- a sparse working tree cannot hold
    /// `engine/content/a.uasset` without `engine/content`.
    ///
    /// This is the verdict every tree walk in the client asks for. They query a
    /// whole path per node rather than threading a [`FilterStates`] down, so
    /// they cannot pair [`child_exclusion_states`](Self::child_exclusion_states)
    /// with [`should_descend`](Self::should_descend); this folds the ancestors
    /// through the memo and answers both halves in one call.
    pub fn excludes_tree(
        &self,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> bool {
        self.exclude_reason(path, is_directory, mode, Scope::Tree)
            .is_some()
    }

    /// Whether every path below `path` is excluded, for the slots in `mode`.
    pub fn excludes_subtree(&self, path: &RelativePath, mode: FilterMode) -> bool {
        (mode.contains(FilterMode::Ignore) && self.ignore.excludes_subtree(path))
            || (mode.contains(FilterMode::View) && self.view.excludes_subtree(path))
    }

    /// [`excludes_tree`](Self::excludes_tree), emitting a
    /// [`LoreEvent::FilterExclude`] when it hits.
    ///
    /// The walk verdict rather than the node one, because every caller uses the
    /// answer to skip a path and everything under it. Reporting a directory a
    /// walk still has to enter would name a path whose content is in scope.
    pub fn emit_excludes(&self, path: &RelativePath, is_directory: bool, mode: FilterMode) -> bool {
        Self::emit(
            path,
            self.exclude_reason(path, is_directory, mode, Scope::Tree),
        )
    }

    /// Reports the path that was asked about, not the ancestor that matched: it
    /// is what the caller named, and the ancestor is only available lowercased.
    fn emit(path: &RelativePath, reason: Option<FilterReason>) -> bool {
        match reason {
            Some(reason) => {
                LoreEvent::FilterExclude(LoreFilterExcludeEventData {
                    reason: reason as u8,
                    path: path.into(),
                })
                .send();
                true
            }
            None => false,
        }
    }
}
