// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bitflags::bitflags;
use dashmap::DashMap;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::bitflagsops;
use crate::errors::InvalidArguments;
use crate::event::LoreEvent;
use crate::interface::LoreString;
use crate::lore_warn;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::MERGE_ARTIFACT_SUFFIXES;
use crate::repository::TEMP_FILE_EXTENSION;
use crate::util::encoding::decode_text_for_parsing;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;

#[derive(Clone, Default, Debug)]
pub struct Filter {
    pub ignore: FilterInstance,
    pub view: FilterInstance,
    /// Folded ancestor verdicts, shared across clones. See [`AncestorMemo`].
    memo: AncestorMemo,
}

/// One filter's rules, with the indexes that answer subtree questions about them.
///
/// The footprint is load-bearing. [`Filter`] holds two of these by value and a
/// whole-path query reads across both, so a field costs several percent of that
/// query whether or not anything reads it, and removing one costs as much as
/// adding one. A dead field of the same width separates that from the cost of
/// whatever the new field does.
#[derive(Clone, Default, Debug)]
pub struct FilterInstance {
    /// Match lines in authored order. Every authored rule contributes exactly
    /// one line, except a path-form inclusion, which also contributes the
    /// subtree companion described on [`add_inclusion`](Self::add_inclusion).
    pub lines: Vec<FilterLine>,
    /// Answers whether any inclusion can land below a directory, which is what
    /// decides descent into an excluded one. Built as the lines are added.
    reinclude: RuleIndex,
    /// The same question asked of the exclusions, which is what decides whether
    /// an included directory holds anything this filter could exclude. See
    /// [`covers_subtree`](Self::covers_subtree).
    exclude: RuleIndex,
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
pub enum FilterError {
    InvalidArguments,
}

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

/// A path a walk asks its questions about: the filter matches it, a cache keys on it, a
/// message spells it, and a change records a path taken from it.
///
/// Taking a path is free where the value already is one and one path where it is a buffer the
/// walk reuses, so asking costs nothing and only recording pays.
pub trait WalkPath: FilterPath + std::fmt::Display {
    /// A path of its own, for recording or walking below.
    fn to_path(&self) -> RelativePath;
}

impl WalkPath for RelativePath {
    fn to_path(&self) -> RelativePath {
        self.clone()
    }
}

impl WalkPath for RelativePathBuf {
    fn to_path(&self) -> RelativePath {
        self.clone().freeze()
    }
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
    for suffix in MERGE_ARTIFACT_SUFFIXES {
        ignore.add_exclusion(&format!("*{suffix}"))?;
    }
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

/// Reads the authored rules from a filter file, one per line.
///
/// A file that cannot be read is not an error: there is no filter, so nothing is
/// excluded. A caller that needs to tell an absent filter from an empty one — an
/// operation replacing the view, say, where the two mean different things —
/// reads the bytes itself and calls [`parse_filter`].
pub fn load_filter(path: impl AsRef<Path>) -> Result<FilterInstance, FilterError> {
    let path = path.as_ref();
    match std::fs::read(path) {
        Ok(bytes) => parse_filter(&bytes, path),
        Err(_) => Ok(FilterInstance::default()),
    }
}

/// Parses the authored rules in `bytes`, one per line. `path` names the file
/// they came from, for the error message.
///
/// A file that can be read but not understood is an error, and the whole file is
/// refused rather than the offending line skipped — a filter missing a rule
/// excludes less than the file asks for, and the caller has no way to tell that
/// from a filter that matched everything it named.
///
/// The bytes arrive whole because the encoding is a property of the leading ones
/// and a UTF-16 file has to be transcoded before it has lines at all;
/// [`decode_text_for_parsing`] names the encodings accepted.
pub fn parse_filter(bytes: &[u8], path: &Path) -> Result<FilterInstance, FilterError> {
    let text = decode_text_for_parsing(bytes).map_err(|error| InvalidArguments {
        reason: format!("{}: {}", path.display(), error.reason),
    })?;
    let mut filter = FilterInstance::default();
    let mut has_include = false;
    let mut has_exclude = false;
    for line in text.lines() {
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
    Ok(filter)
}

/// Writes the authored rules back out, in order, as UTF-8 with no byte-order
/// mark whatever encoding they were read from.
///
/// Reconstructed from the compiled lines: a name rule is written as it stands, a
/// rooted single-component rule regains its leading separator, and a
/// directory-only rule its trailing one. An authored `**/foo` comes back as
/// `foo`, which gitignore defines as the same rule.
///
/// A reader sees either the previous rules or the new ones, and a save that
/// fails leaves the previous ones.
///
/// The whole file is built in memory, so it costs one write rather than one per
/// rule, and the I/O driver's atomic whole-file write publishes it: a temporary
/// sibling, synced to disk with its parent directory, renamed over the target.
/// Opening the target itself would truncate it at the open, so a write that then
/// failed part way, on a full filesystem for instance, would leave a prefix of
/// the new rules or nothing at all. A filter short a rule excludes less than the
/// file asked for and nothing downstream can tell, which is the same reason
/// [`load_filter`] refuses a file it cannot decode whole.
///
/// The driver leaves the sibling behind on failure and gives its cleanup to the
/// caller, so a failure removes it. Nothing reports it while it exists: it is
/// named with [`TEMP_FILE_EXTENSION`], which the ignore filter excludes and the
/// working-tree scanners skip.
pub async fn save(filter: &FilterInstance, path: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    let mut out = String::new();
    for line in filter.lines.iter().filter(|line| !line.generated) {
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
    }
    let temp_path = temp_sibling(path);
    let saved = lore_io::IoDriver::global()
        .write_file_segments_atomic(
            &temp_path,
            path,
            &lore_io::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            vec![out.into_bytes()],
        )
        .await;

    if saved.is_err() {
        // The target is unchanged, so the sibling is all there is to clean up.
        // Its own failure is not worth reporting over the one that got here.
        let _ = lore_io::IoDriver::global().remove_file(&temp_path).await;
    }
    saved
}

/// The temporary file [`save`] builds the new contents of `path` in, beside it
/// in the same directory so the rename onto it stays within one filesystem.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_owned();
    temp.push(TEMP_FILE_EXTENSION);
    PathBuf::from(temp)
}

/// How many components a non-empty relative path has.
fn component_count(path: &str) -> u32 {
    path.bytes().filter(|byte| *byte == b'/').count() as u32 + 1
}

/// How deep a query about `path` stands, which bounds which rules can reach
/// below it. The repository root arrives empty and names no component.
///
/// This is the `depth` [`Filter::covers_subtree`] is asked with. A walk that
/// descends a component at a time counts instead, and pays nothing for it.
pub(crate) fn query_depth(path: &str) -> u32 {
    if path.is_empty() {
        0
    } else {
        component_count(path)
    }
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
///
/// A brace group counts. An alternative inside one may hold a separator, so
/// neither the group's text nor what follows it names a component. A
/// [`RuleIndex`] enumerates the alternatives rather than stop here, and this is
/// what answers for the forms it declines to enumerate: a rule compared as text,
/// an escaped brace, and one with more alternatives than the cap.
fn has_wildcard(component: &str) -> bool {
    component.contains(['*', '?', '[', '{'])
}

/// The first brace group in `glob`, as the offsets of its `{` and its matching
/// `}`.
///
/// Mirrors the matcher's own scan of a group: a bracket expression suppresses
/// brace syntax, and a backslash escapes the byte after it. Braces that never
/// balance form no group, which is what the matcher makes of such a pattern --
/// it calls it invalid and matches nothing with it.
fn first_brace_group(glob: &str) -> Option<(usize, usize)> {
    let bytes = glob.as_bytes();
    let mut open = None;
    let mut depth = 0u32;
    let mut in_brackets = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'{' if !in_brackets => {
                open = open.or(Some(index));
                depth += 1;
            }
            b'}' if !in_brackets && depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return open.map(|start| (start, index));
                }
            }
            b'[' if !in_brackets => in_brackets = true,
            b']' => in_brackets = false,
            _ => {}
        }
        index += 1;
    }
    None
}

/// The top-level alternatives in a brace group's body, which
/// [`first_brace_group`] delimits.
///
/// A comma inside a nested group or a bracket expression belongs to that
/// construct, and one behind a backslash is text.
fn brace_alternatives(body: &str) -> impl Iterator<Item = &str> {
    let bytes = body.as_bytes();
    let mut start = 0;
    let mut index = 0;
    let mut finished = false;
    std::iter::from_fn(move || {
        if finished {
            return None;
        }
        let mut depth = 0u32;
        let mut in_brackets = false;
        while index < bytes.len() {
            match bytes[index] {
                b'\\' => index += 1,
                b'{' if !in_brackets => depth += 1,
                b'}' if !in_brackets => depth = depth.saturating_sub(1),
                b',' if depth == 0 && !in_brackets => {
                    let alternative = &body[start..index];
                    index += 1;
                    start = index;
                    return Some(alternative);
                }
                b'[' if !in_brackets => in_brackets = true,
                b']' => in_brackets = false,
                _ => {}
            }
            index += 1;
        }
        finished = true;
        Some(&body[start..])
    })
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

/// How far the rules recorded at one point in a [`RuleIndex`] reach: the highest
/// line any of them sits on, and the deepest path any of them can match.
///
/// The line is held as a count -- one past the highest index -- so that
/// [`Default`] means "no rule here" and a query needs neither a sentinel nor
/// signed arithmetic. A query floored at `floor` asks `lines > floor`, which is
/// "the highest line is at or after the floor".
///
/// Pairing a maximum line with a maximum depth over-approximates: it can pair
/// one rule's line with another rule's depth and so answer for a rule that is
/// neither. Only ever towards "a rule reaches here", which is the direction the
/// index answers every uncertain case in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RuleReach {
    /// One past the highest line of a rule recorded here; zero for none.
    lines: u32,
    /// Most path components the deepest of those rules can match.
    depth: u32,
}

impl RuleReach {
    /// The reach of the one rule on line `line`, which matches at most `depth`
    /// path components.
    fn rule(line: usize, depth: u32) -> Self {
        Self {
            lines: line as u32 + 1,
            depth,
        }
    }

    /// Widens this reach to cover `other` as well.
    fn widen(&mut self, other: Self) {
        self.lines = self.lines.max(other.lines);
        self.depth = self.depth.max(other.depth);
    }

    /// Whether a rule recorded here can still matter to a query standing `depth`
    /// components deep whose earlier lines are floored at `floor`.
    ///
    /// The depth comparison is strict because every query asks about what lies
    /// *below* a path: a rule that cannot match more than `depth` components
    /// cannot match anything deeper than the directory being asked about.
    fn reaches(self, floor: u32, depth: u32) -> bool {
        self.lines > floor && self.depth > depth
    }
}

/// Most alternation-free forms of one rule a [`RuleIndex`] enumerates.
///
/// Nested groups multiply, so this bounds what a filter can cost to index. Six
/// two-way groups reach it; a rule written by hand carries a handful.
const MAX_ALTERNATIVES: usize = 64;

/// Literal-prefix index over the lines of one polarity, answering whether any of
/// them can match below a directory without evaluating a glob.
///
/// A walk needs that answer for every directory it meets. Over the inclusions it
/// decides whether to descend into an excluded directory anyway; over the
/// exclusions it decides whether an included one can be taken whole, which is
/// what [`FilterInstance::covers_subtree`] asks. Deriving either from the lines
/// would cost a scan of every line per directory, and a filter built from diff
/// paths carries up to `SOURCE_FILTER_THRESHOLD` of them.
///
/// Two independent bounds keep a rule out of an answer, and a rule escapes the
/// index only by escaping both. The trie bounds *where* it can match: only the
/// wildcard-free leading components of a glob can be indexed, so one whose first
/// component holds a wildcard is recorded in `unanchored` and reaches any path.
/// The [`RuleReach`] depth bounds *how deep*: the `/*` that opens a filter
/// written as "exclude the top, then re-include what is wanted" compiles to
/// `*`, which is unanchored and so reaches every path, but which cannot match
/// below the first level.
///
/// Over-approximating costs traversal, under-approximating drops content, so
/// every uncertain case answers "a rule reaches here".
#[derive(Clone, Debug, Default)]
struct RuleIndex {
    root: RuleNode,
    /// Rules no prefix can rule out, because the first component of the glob
    /// holds a wildcard or the rule is matched against a name at any depth.
    unanchored: RuleReach,
}

#[derive(Clone, Debug, Default)]
struct RuleNode {
    /// Component to child. Shallow and narrow in practice, so a `Vec` beats a
    /// map: lookup is a handful of string compares with no hashing.
    children: Vec<(String, RuleNode)>,
    /// Rules sitting strictly below this node.
    below: RuleReach,
    /// Rules with a wildcard tail starting here, which could match anything at
    /// or below this node.
    wildcard_tail: RuleReach,
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

impl RuleIndex {
    /// Records that the rule on line `line` matches `glob` and can match at most
    /// `depth` path components. `expand` enumerates the rule's brace
    /// alternatives.
    ///
    /// Each alternative is a separate path the rule can name, and every one has
    /// to be recorded: the group's text names no component, so a walk prunes
    /// wherever an alternative was left out. They are enumerated up to
    /// [`MAX_ALTERNATIVES`], beyond which the rule is recorded at its group
    /// instead and reaches everything below that point.
    ///
    /// A caller passes `false` for a rule holding no group, and for one
    /// [`FilterInstance::step`] compares as text rather than evaluating as a
    /// glob: that rule's own text is the only thing it matches, so enumerating
    /// would record paths it never names and leave out the one it does.
    fn insert(&mut self, glob: &str, expand: bool, line: usize, depth: u32) {
        if !expand {
            self.insert_form(glob, line, depth);
            return;
        }

        let mut pending = vec![glob.to_owned()];
        let mut enumerated = 0;
        while let Some(form) = pending.pop() {
            let Some((open, close)) = first_brace_group(&form) else {
                self.insert_form(&form, line, FilterInstance::depth_range(&form, false).1);
                enumerated += 1;
                continue;
            };
            for alternative in brace_alternatives(&form[open + 1..close]) {
                if enumerated + pending.len() >= MAX_ALTERNATIVES {
                    self.insert_form(glob, line, depth);
                    return;
                }
                pending.push(format!(
                    "{}{alternative}{}",
                    &form[..open],
                    &form[close + 1..]
                ));
            }
        }
    }

    /// Records one alternation-free form of a rule, as [`insert`](Self::insert)
    /// enumerates them.
    ///
    /// Each node on the way down gains the rule as something below it -- at least
    /// the next component, maybe deeper. A wildcard component ends the descent and
    /// marks the node it stops at, because no literal text stands in for it. A
    /// glob that is literal throughout names its final node and marks nothing
    /// below it.
    fn insert_form(&mut self, glob: &str, line: usize, depth: u32) {
        let reach = RuleReach::rule(line, depth);
        let mut node = &mut self.root;
        for (index, component) in glob.split('/').enumerate() {
            if has_wildcard(component) {
                if index == 0 {
                    self.unanchored.widen(reach);
                }
                node.wildcard_tail.widen(reach);
                return;
            }
            node.below.widen(reach);
            let position = node
                .children
                .iter()
                .position(|(name, _)| name == component)
                .unwrap_or_else(|| {
                    node.children
                        .push((component.to_owned(), RuleNode::default()));
                    node.children.len() - 1
                });
            node = &mut node.children[position].1;
        }
    }

    /// Records the rule on line `line` as one no prefix bounds, so it reaches any
    /// path. `depth` bounds it as in [`insert`](Self::insert).
    fn insert_unanchored(&mut self, line: usize, depth: u32) {
        self.unanchored.widen(RuleReach::rule(line, depth));
    }

    /// Whether a rule on line `floor` or later can match a path strictly below
    /// `path`, which stands `depth` components deep.
    ///
    /// `floor` is the line that decided the verdict at `path`. A rule before it
    /// already lost there and [`FilterInstance::step`] will not consult it again
    /// below, so it cannot change anything and must not force a descent. Without
    /// that comparison the subtree companion of any re-inclusion would keep every
    /// excluded sibling of its own directory reachable. A caller below which
    /// every line still applies passes `0` -- see
    /// [`FilterInstance::covers_subtree`].
    ///
    /// Returns early when nothing in the index is late enough or deep enough to
    /// matter, and when the walk falls off the indexed branches.
    ///
    /// An empty `path` is the repository root, which names no component and
    /// holds everything: any rule the index carries at all lands below it.
    fn below(&self, path: &str, floor: u32, depth: u32) -> bool {
        if self.unanchored.reaches(floor, depth) {
            return true;
        }
        let mut node = &self.root;
        if !node.below.reaches(floor, depth) {
            return false;
        }
        if path.is_empty() {
            return true;
        }
        for component in path.split('/') {
            if node.wildcard_tail.reaches(floor, depth) {
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
        node.below.reaches(floor, depth) || node.wildcard_tail.reaches(floor, depth)
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

    /// How many path components `glob` can match: fewest, and most or
    /// [`u32::MAX`] where nothing bounds it. A name rule is matched against the
    /// last component alone, so it applies at any depth.
    ///
    /// The lower bound lets [`step`](Self::step) skip a line against a prefix
    /// too short for it to match: each component consumes one, except a `**`
    /// that is not the last, which may absorb nothing, so `a/**/b` can match
    /// `a/b` while `a/**` does not match `a`.
    ///
    /// The upper bound is what a [`RuleIndex`] records, so that a rule unable to
    /// reach below a directory does not force a walk into it -- without it the
    /// `*` that a leading `/*` compiles to would reach every path. A `**`
    /// consumes one component or more, so a glob holding one is unbounded.
    ///
    /// Sound because of the matcher's own arithmetic: every component other than
    /// `**` consumes exactly one path component, since `*` and `?` do not cross
    /// a separator. It is an over-estimate under every construct the matcher
    /// supports, which is the direction that has to hold -- brace alternation
    /// picks a subset of the glob's separators, and classes and escapes only
    /// make a `/` stop separating, so no expansion has more components than the
    /// glob text. Over-estimating costs traversal; under-estimating drops
    /// content.
    ///
    /// Both bounds come from one pass because both callers want both, and
    /// `filter_from_source_changes` builds a filter per three-way diff with up
    /// to `SOURCE_FILTER_THRESHOLD` rules in it, so a second scan per rule is
    /// paid there.
    fn depth_range(glob: &str, filename: bool) -> (u32, u32) {
        if filename {
            return (1, u32::MAX);
        }
        let mut total = 0u32;
        let mut optional = 0u32;
        // Reaching another component proves the previous `**` was not the last.
        let mut previous_was_globstar = false;
        let mut unbounded = false;
        for component in glob.split('/') {
            optional += u32::from(previous_was_globstar);
            total += 1;
            previous_was_globstar = component == "**";
            unbounded |= previous_was_globstar;
        }
        (
            (total - optional).max(1),
            if unbounded { u32::MAX } else { total },
        )
    }

    /// Appends an exclusion.
    ///
    /// One line, and no subtree companion: a walk does not descend past an
    /// excluded directory unless something below it is re-included, and where it
    /// does descend the state carried into [`step`](Self::step) keeps the subtree
    /// excluded without a rule saying so.
    ///
    /// Every exclusion enters the exclusion index, name rules included -- the
    /// one place the two polarities are fed differently. A name rule is not
    /// indexed as an inclusion because it cannot re-open a pruned subtree, but
    /// `*.tmp` as an *exclusion* bites at any depth, so leaving it out would let
    /// [`covers_subtree`](Self::covers_subtree) report a subtree as untouched by
    /// a filter that excludes half of it.
    pub fn add_exclusion(&mut self, glob: &str) -> Result<(), FilterError> {
        let (glob, filename, ending_separator) = Self::compile(glob);
        let (min_depth, depth_max) = Self::depth_range(&glob, filename);
        let literal = is_literal(&glob);
        if filename {
            self.exclude.insert_unanchored(self.lines.len(), depth_max);
        } else {
            let expand = !literal && glob.contains('{');
            self.exclude
                .insert(&glob, expand, self.lines.len(), depth_max);
        }
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

        let (min_depth, depth_max) = Self::depth_range(&glob, filename);
        let literal = is_literal(&glob);
        // A name rule is neither indexed nor given a companion, so it pays no scan.
        let braced = !filename && glob.contains('{');
        if !filename {
            self.reinclude
                .insert(&glob, braced && !literal, self.lines.len(), depth_max);
        }
        self.lines.push(FilterLine {
            min_depth,
            literal,
            glob,
            negated: true,
            directory: ending_separator,
            filename,
            generated: false,
        });

        if let Some(subtree) = subtree {
            let (min_depth, depth_max) = Self::depth_range(&subtree, false);
            self.reinclude
                .insert(&subtree, braced, self.lines.len(), depth_max);
            self.lines.push(FilterLine {
                min_depth,
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
        debug_assert!(
            floor <= self.lines.len(),
            "a state from another filter: decided_at {floor} exceeds {} lines",
            self.lines.len()
        );
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
        if self.lines.is_empty() || path.is_empty() || path.as_str() == "." {
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
    ///
    /// The query depth is `0`, so every indexed inclusion clears the depth
    /// bound however shallow its own rule. This side is deliberately unbounded
    /// in depth: tightening it would prune walks that reach content today.
    fn settles_subtree(&self, state: FilterState, path: &str) -> bool {
        state.excluded && !self.reinclude.below(path, state.decided_at, 0)
    }

    /// Whether `state` includes `path` and no rule can exclude anything below
    /// it, so every descendant is included too. `depth` is how many components
    /// `path` has, and `0` for the repository root.
    ///
    /// The exact dual of [`settles_subtree`](Self::settles_subtree), and for the
    /// same reason: [`step`](Self::step) skips every line whose effect equals
    /// the current state, so from an excluded directory only an inclusion can
    /// change the verdict below, and from an included one only an exclusion can.
    ///
    /// Two filters that both cover a subtree agree on every path in it, whatever
    /// else they say, so a walk comparing them can take the whole subtree
    /// without descending.
    ///
    /// The floor is `0` rather than `state.decided_at` because `step` itself
    /// floors at `0` whenever the parent is included: every line applies below
    /// an included directory, exclusions authored before whatever re-inclusion
    /// decided it included.
    fn covers_subtree(&self, state: FilterState, path: &str, depth: u32) -> bool {
        debug_assert_eq!(
            depth,
            query_depth(path),
            "a depth that is not {path:?}'s own reports a subtree as covered that is not"
        );
        !state.excluded && !self.exclude.below(path, 0, depth)
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
    /// The same ignore rules with `view` in the view slot, and a fresh
    /// [`AncestorMemo`].
    ///
    /// For an operation holding the filter an instance runs under that needs the
    /// one it is moving to: both are asked the same questions about the same
    /// tree, so both exist at once.
    ///
    /// The fresh memo is what this exists for. [`Filter`] is `Clone` with public
    /// slots, so `let mut new = old.clone(); new.view = view;` compiles and
    /// leaves the two sharing a memo that revalidates on the slots' line
    /// *counts* alone -- two views of equal length would serve each other's
    /// folded ancestor verdicts.
    ///
    /// The ignore slot carries over: it holds the rules [`load`] adds and no
    /// file names, so dropping it would let staging take `.lore` and the merge
    /// artifacts.
    pub fn with_view(&self, view: FilterInstance) -> Self {
        Self {
            ignore: self.ignore.clone(),
            view,
            memo: AncestorMemo::default(),
        }
    }

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
        self.descend_reason(states, path, mode).is_none()
    }

    /// The slot that stops a walk at `path`, or `None` when it descends.
    ///
    /// Ignore is consulted before view, which is the order
    /// [`exclude_reason`](Self::exclude_reason) reports a path both slots
    /// exclude in.
    fn descend_reason(
        &self,
        states: FilterStates,
        path: &impl FilterPath,
        mode: FilterMode,
    ) -> Option<FilterReason> {
        if mode.contains(FilterMode::Ignore) && !self.ignore.should_descend(states.ignore, path) {
            return Some(FilterReason::Ignore);
        }
        if mode.contains(FilterMode::View) && !self.view.should_descend(states.view, path) {
            return Some(FilterReason::View);
        }
        None
    }

    /// One step of a walk: the states `path`'s children inherit, and why the
    /// walk drops `path` with everything under it -- `None` to keep it and, for
    /// a directory, to descend into it.
    ///
    /// The threaded form of [`exclude_reason`](Self::exclude_reason) at
    /// [`Scope::Tree`]. The caller holds the parent's states, so no ancestor is
    /// folded and the subtree half is one index lookup rather than a second pass
    /// over the lines.
    fn child_exclude_reason(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, Option<FilterReason>) {
        let (states, reason) = self.child_exclusion_states(parent, path, is_directory, mode);
        // A file has no subtree, so both scopes ask the same question of one.
        if !is_directory {
            return (states, reason);
        }
        let reason = self.descend_reason(states, path, mode);
        (states, reason)
    }

    /// [`excludes_tree`](Self::excludes_tree) for a walk that threads state:
    /// whether the walk drops `path`, and the states its children inherit.
    pub fn child_excludes_tree(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, bool) {
        let (states, reason) = self.child_exclude_reason(parent, path, is_directory, mode);
        (states, reason.is_some())
    }

    /// [`child_excludes_tree`](Self::child_excludes_tree), emitting a
    /// [`LoreEvent::FilterExclude`] when it hits, as
    /// [`emit_excludes`](Self::emit_excludes) does for a whole path.
    pub fn child_emit_excludes(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, bool) {
        let (states, reason) = self.child_exclude_reason(parent, path, is_directory, mode);
        (states, Self::emit(path, reason))
    }

    /// [`child_excludes_tree`](Self::child_excludes_tree) unless `force` puts
    /// the walk past the filter.
    ///
    /// See [`child_emit_excludes_unless_forced`](Self::child_emit_excludes_unless_forced)
    /// for why a forced walk is answered without asking the filter.
    pub fn child_excludes_tree_unless_forced(
        &self,
        force: bool,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, bool) {
        if force {
            return (FilterStates::ROOT, false);
        }
        self.child_excludes_tree(parent, path, is_directory, mode)
    }

    /// [`child_emit_excludes`](Self::child_emit_excludes) unless `force` puts
    /// the walk past the filter.
    ///
    /// A forced operation drops nothing, so it must report nothing as excluded
    /// either, and it needs no verdict at all: `force` is one flag on the
    /// operation -- `ExecutionContext::globals` or `DirtyWalkOptions` -- so
    /// every step below is forced too and none of them reads one. The walk is
    /// answered without touching the lines, and the state handed back is
    /// [`FilterStates::ROOT`] rather than a verdict nothing will consult.
    pub fn child_emit_excludes_unless_forced(
        &self,
        force: bool,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, bool) {
        if force {
            return (FilterStates::ROOT, false);
        }
        self.child_emit_excludes(parent, path, is_directory, mode)
    }

    /// The folded verdicts at the directory `path`, seeding a walk that starts
    /// there.
    ///
    /// A walk threads its root's states down; one rooted at a named
    /// subdirectory has no parent to inherit from and folds them here instead,
    /// once for the whole walk. Every component is folded as a directory, which
    /// is what a walk standing in one has behind it.
    pub fn exclusion_states(&self, path: &impl FilterPath) -> FilterStates {
        if self.is_empty(FilterMode::Full) {
            return FilterStates::ROOT;
        }
        self.ancestor(path.as_lowercase_str()).states
    }

    /// The verdict at a link or layer mount, seeding the walk over what is
    /// mounted there.
    ///
    /// [`exclusion_states`](Self::exclusion_states) at `mount_path`, so every
    /// component is folded as a directory. The mount node's own step does not
    /// stand in for it: a link node is not a directory, so a directory-only rule
    /// naming the mount is skipped there, while the same rule reaches the
    /// content below, which does sit in a directory.
    pub fn mount_states(&self, mount_path: &impl FilterPath) -> FilterStates {
        self.exclusion_states(mount_path)
    }

    /// [`exclusion_states`](Self::exclusion_states) for `path`'s parent, seeding
    /// a walk whose first step is `path` itself.
    ///
    /// A recursion that asks the filter about the node it was handed, rather
    /// than about each child before recursing, needs the states that node's own
    /// query starts from.
    pub fn parent_exclusion_states(&self, path: &impl FilterPath) -> FilterStates {
        if self.is_empty(FilterMode::Full) {
            return FilterStates::ROOT;
        }
        self.ancestor(path.split_lowercase().0).states
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
    /// This is the verdict a walk asks for. A walk that threads a
    /// [`FilterStates`] down asks it of
    /// [`child_excludes_tree`](Self::child_excludes_tree) instead, which is the
    /// same verdict for the price of one step; this is for a path that arrives
    /// whole, and folds the ancestors through the memo to answer both halves in
    /// one call.
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

    /// Whether every path at or below `path` is included, for the slots in
    /// `mode`, given the verdicts at `path` itself. `depth` is how many
    /// components `path` has; the root is answered at depth `0` whatever a
    /// caller counted for the path naming it.
    ///
    /// The threaded form, for a walk that holds the states: see
    /// [`FilterInstance::covers_subtree`] for what the answer is worth. Covered
    /// means covered by every slot in `mode`, since content has to clear both,
    /// and a slot outside `mode` is not consulted and cannot object -- so an
    /// empty mode covers everything.
    ///
    /// **An ignore slot built by [`load`] covers nothing.** It adds `.lore`,
    /// `.urc`, the merge artifacts and the temporary extension as name rules,
    /// which match at any depth and so reach below every path. A caller
    /// comparing two views therefore asks with [`FilterMode::View`]: the two
    /// share one ignore slot, which cannot make them disagree, and including
    /// `Ignore` in the mode would answer `false` everywhere.
    pub fn covers_subtree(
        &self,
        states: FilterStates,
        path: &impl FilterPath,
        depth: u32,
        mode: FilterMode,
    ) -> bool {
        let (lowercase, depth) = if path.is_empty() || path.as_str() == "." {
            ("", 0)
        } else {
            (path.as_lowercase_str(), depth)
        };
        (!mode.contains(FilterMode::Ignore)
            || self.ignore.covers_subtree(states.ignore, lowercase, depth))
            && (!mode.contains(FilterMode::View)
                || self.view.covers_subtree(states.view, lowercase, depth))
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
    fn emit(path: &impl FilterPath, reason: Option<FilterReason>) -> bool {
        match reason {
            Some(reason) => {
                LoreEvent::FilterExclude(LoreFilterExcludeEventData {
                    reason: reason as u8,
                    path: path.as_str().into(),
                })
                .send();
                true
            }
            None => false,
        }
    }
}

/// The arithmetic behind a [`RuleIndex`] answer, which nothing outside the
/// module can reach. The behaviour it produces is asserted in `tests/filter.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    /// Both bounds over every construct the compiler can hand the index.
    ///
    /// The upper bound is the one that has to be an over-estimate: it decides
    /// whether a rule is dropped from an answer, and a bound that is too small
    /// drops content with nothing downstream able to tell. It is checked beside
    /// the lower one, since a pair that crossed would describe a rule able to
    /// match at no depth at all.
    ///
    /// The compiled glob is asserted too, because the bounds read the compiled
    /// text rather than the authored rule, and the interesting rows differ in
    /// how `compile` treats them: a leading separator is what decides whether
    /// `*` is a name rule or a path one, and `**/a/b` keeps a prefix that
    /// `**/name` loses.
    #[test]
    fn the_depth_bounds_agree_on_every_glob_shape() {
        // Authored rule, compiled glob, name rule, fewest, most.
        let cases: &[(&str, &str, bool, u32, u32)] = &[
            ("/Engine/Intermediate", "engine/intermediate", false, 2, 2),
            ("/*", "*", false, 1, 1),
            ("/Some/**/Path", "some/**/path", false, 2, u32::MAX),
            ("*.tmp", "*.tmp", true, 1, u32::MAX),
            ("Thumbs.db", "thumbs.db", true, 1, u32::MAX),
            ("**/a/b", "**/a/b", false, 2, u32::MAX),
            ("**/node_modules", "node_modules", true, 1, u32::MAX),
            ("/engine/**", "engine/**", false, 2, u32::MAX),
            ("**", "**", false, 1, u32::MAX),
            // A brace group expands to no more components than its text, so
            // counting the text stays an upper bound over both alternatives.
            ("/a{b,c/d}", "a{b,c/d}", false, 2, 2),
        ];

        for (rule, glob, filename, min, max) in cases {
            let (compiled, compiled_filename, _) = FilterInstance::compile(rule);
            assert_eq!(
                (compiled.as_str(), compiled_filename),
                (*glob, *filename),
                "{rule} compiled to something else"
            );
            assert_eq!(
                FilterInstance::depth_range(glob, *filename),
                (*min, *max),
                "{rule} has different bounds"
            );
            assert!(*min <= *max, "{rule} can match at no depth at all");
        }
    }

    /// A reach holding no rule matters to no query, whatever it is asked, and
    /// line zero still matters to a floor of zero.
    ///
    /// The default is what every unvisited node carries and what a filter with
    /// no rules of that polarity carries throughout, which is the commonest
    /// filter there is. Line zero is the boundary the count encoding turns on:
    /// one off and either no rule is ever consulted or an empty index answers
    /// for a rule it does not hold.
    #[test]
    fn an_empty_reach_reaches_nothing() {
        let empty = RuleReach::default();
        for floor in [0, 1, u32::MAX] {
            for depth in [0, 1, u32::MAX] {
                assert!(!empty.reaches(floor, depth), "floor {floor}, depth {depth}");
            }
        }
        assert!(
            RuleReach::rule(0, 1).reaches(0, 0),
            "the first line of a filter has to clear a floor of zero"
        );
    }
}
