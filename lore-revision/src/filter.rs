// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;
use std::hash::BuildHasherDefault;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bitflags::bitflags;
use dashmap::DashMap;
use lore_base::types::PassThroughHasher;
use lore_error_set::prelude::*;
use serde::Serialize;
use smallvec::SmallVec;

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

/// One filter's rules, with the indexes that answer subtree questions about them
/// and narrow the lines a step visits.
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
    /// A path added by [`add_path_inclusion`](Self::add_path_inclusion)
    /// contributes a line and its companion. Every line enters through
    /// [`push_line`](Self::push_line), which keeps `prefixes` in step with it.
    lines: Vec<FilterLine>,
    /// Answers whether any inclusion can land below a directory, which is what
    /// decides descent into an excluded one. Built as the lines are added.
    reinclude: RuleIndex,
    /// The same question asked of the exclusions, which is what decides whether
    /// an included directory holds anything this filter could exclude. See
    /// [`covers_subtree`](Self::covers_subtree).
    exclude: RuleIndex,
    /// The lines by their leading literal components once there are
    /// [`INDEXED_LINES`] of them, which spares [`step`](Self::step) the lines
    /// listed under components other than the path's.
    prefixes: Option<Box<LinePrefixes>>,
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
    /// Fewest path components this line can match, so a prefix shorter than that
    /// is skipped without running the glob. A whole-path query folds over every
    /// ancestor, and most lines cannot possibly match the shallow ones.
    min_depth: u32,
    /// The glob is compared to the path as text rather than evaluated as a
    /// glob. Most lines in a real filter are literal paths or names, and this
    /// runs for every line on every path a walk visits.
    literal: bool,
}

impl FilterLine {
    /// Whether the glob matches `path`, or `name` for a name rule, by
    /// comparison where the line is literal.
    fn hits(&self, path: &str, name: &str) -> bool {
        let text = if self.filename { name } else { path };
        if self.literal {
            self.glob == text
        } else {
            glob_match::glob_match(self.glob.as_str(), text)
        }
    }
}

impl FilterInstance {
    /// Whether both hold the same authored rules in the same order, and so answer every path
    /// the same way. The indexes are derived from the lines, so comparing lines is enough.
    pub fn same_rules(&self, other: &FilterInstance) -> bool {
        self.lines.len() == other.lines.len()
            && self.lines.iter().zip(&other.lines).all(|(a, b)| {
                a.glob == b.glob
                    && a.negated == b.negated
                    && a.directory == b.directory
                    && a.filename == b.filename
                    && a.literal == b.literal
            })
    }
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
/// already lost at an excluded ancestor cannot win below it, and the
/// re-inclusion index's `RuleIndex::below` ignores re-inclusions before it, so
/// a rule that lost cannot force a descent either. For an excluded directory a
/// re-inclusion can reach below, it is the last line that excludes the
/// directory, since every re-inclusion authored before that line lost there.
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

    /// A state carrying only whether the path is excluded, decided at no
    /// particular line (as `ROOT` is).
    ///
    /// For a caller that persisted just the excluded bit and needs a
    /// `FilterState` back to hand a walk. Resuming a walk from an excluded
    /// result takes every line from the top, where the original decision took
    /// only those from the line that decided it, so a re-inclusion authored
    /// before that line, which lost there, re-includes below it. A caller that
    /// can keep the line resumes with [`decided`](Self::decided).
    pub fn excluded_from(excluded: bool) -> Self {
        Self {
            excluded,
            decided_at: 0,
        }
    }

    /// A state carrying whether the path is excluded and the line that decided
    /// it, as [`decided_at`](Self::decided_at) answered, for a caller that
    /// persisted both: a walk resumed from it reaches the verdicts the original
    /// walk would have.
    pub fn decided(excluded: bool, decided_at: u32) -> Self {
        Self {
            excluded,
            decided_at,
        }
    }

    /// The line that decided the verdict, which floors the lines a walk below
    /// an excluded path takes.
    pub fn decided_at(&self) -> u32 {
        self.decided_at
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
    with_ignore(load_filter(ignore_path)?, view_path)
}

/// The filter of `ignore`, the rules of a repository's ignore file, with the exclusions every
/// repository holds and the view at `view_path`.
fn with_ignore(
    mut ignore: FilterInstance,
    view_path: impl AsRef<Path>,
) -> Result<Filter, FilterError> {
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

/// [`load`], with the exclusions in the instance's own ignore file at
/// `instance_ignore_path` after the rules of the repository's.
///
/// The instance's file is read as [`load_filter`] reads one: a file that cannot
/// be read adds no rules, and one that cannot be decoded is an error. Its rules
/// can only exclude: a `!` re-inclusion is skipped with a warning naming the line,
/// so the file never brings back what the repository ignores.
pub fn load_with_instance_ignore(
    ignore_path: impl AsRef<Path>,
    instance_ignore_path: impl AsRef<Path>,
    view_path: impl AsRef<Path>,
) -> Result<Filter, FilterError> {
    with_instance_ignore(load(ignore_path, view_path)?, instance_ignore_path)
}

/// [`load_with_instance_ignore`] with the repository's ignore file read already: `ignore` holds
/// its bytes and the path they came from, for the error message, and `None` where it has none.
pub fn load_with_instance_ignore_from(
    ignore: Option<(&[u8], &Path)>,
    instance_ignore_path: impl AsRef<Path>,
    view_path: impl AsRef<Path>,
) -> Result<Filter, FilterError> {
    let rules = match ignore {
        Some((bytes, path)) => parse_filter(bytes, path)?,
        None => FilterInstance::default(),
    };
    with_instance_ignore(with_ignore(rules, view_path)?, instance_ignore_path)
}

/// `filter` with the exclusions in the instance's own ignore file at `instance_ignore_path` after
/// its own, as [`load_with_instance_ignore`] reads them.
fn with_instance_ignore(
    mut filter: Filter,
    instance_ignore_path: impl AsRef<Path>,
) -> Result<Filter, FilterError> {
    let path = instance_ignore_path.as_ref();
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(filter);
    };
    let text = decode_filter(&bytes, path)?;
    for (line, negated, glob) in authored_rules(&text) {
        if negated {
            lore_warn!(
                "Skipping line {line} of {}, !{glob}: an instance's ignore rules cannot re-include",
                path.display()
            );
            continue;
        }
        filter
            .ignore
            .add_exclusion(glob)
            .map_err(|error| InvalidArguments {
                reason: format!("{}: line {line}: {error}", path.display()),
            })?;
    }
    Ok(filter)
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
/// One leading `!` makes a rule a re-inclusion, as in git, and the rest of the
/// rule is its glob: `!!a` re-includes the name `!a`.
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
    let text = decode_filter(bytes, path)?;
    let mut filter = FilterInstance::default();
    let mut has_include = false;
    let mut has_exclude = false;
    for (_, negated, glob) in authored_rules(&text) {
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

/// Decodes the bytes of a filter file, naming `path` in the error.
fn decode_filter<'a>(bytes: &'a [u8], path: &Path) -> Result<Cow<'a, str>, FilterError> {
    Ok(
        decode_text_for_parsing(bytes).map_err(|error| InvalidArguments {
            reason: format!("{}: {}", path.display(), error.reason),
        })?,
    )
}

/// The rules authored in a filter file's text: the 1-based line number of each,
/// whether a leading `!` makes it a re-inclusion, and its glob. Blank lines and
/// `#` comments are skipped.
fn authored_rules(text: &str) -> impl Iterator<Item = (usize, bool, &str)> {
    text.lines().enumerate().filter_map(|(index, line)| {
        let mut glob = line.trim();
        if glob.is_empty() || glob.starts_with('#') {
            return None;
        }

        let negated = glob.starts_with('!');
        glob = &glob[usize::from(negated)..];

        // Allow exclamation marks in path/file names through escape backslash
        if glob.starts_with("\\!") {
            glob = &glob[1..];
        }
        Some((index + 1, negated, glob))
    })
}

/// How many components a non-empty relative path has.
fn component_count(path: &str) -> u32 {
    path.bytes().filter(|byte| *byte == b'/').count() as u32 + 1
}

/// How many components `path` has, which is none for the empty path that names
/// the repository root.
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
#[lore_macro::test_pub]
fn is_literal(glob: &str) -> bool {
    !glob
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\'))
}

/// The glob `<text>/**`, with a backslash before every character of `text`
/// the matcher reads as syntax, so it matches everything below `text` alone.
fn escaped_subtree(text: &str) -> String {
    let mut subtree = String::with_capacity(text.len() + 3);
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}' | b',' | b'\\') {
            subtree.push_str(&text[start..index]);
            subtree.push('\\');
            start = index;
        }
    }
    subtree.push_str(&text[start..]);
    subtree.push_str("/**");
    subtree
}

/// Whether `stars` is two or more `*` and nothing else, which git reads as one
/// globstar.
fn is_globstar(stars: &str) -> bool {
    stars.len() > 1 && stars.bytes().all(|byte| byte == b'*')
}

/// How a [`RuleIndex`] compares one component of a glob with one path
/// component. A text edge sorts before a pattern edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ComponentKind {
    /// The component holds no metacharacter, so it matches only its own text.
    Text,
    /// The matcher evaluates the component on its own. None of `*`, `?` and a
    /// bracket expression matches a separator, so it matches one path component
    /// as it does within the whole glob.
    Pattern,
}

/// How a [`RuleIndex`] compares one component of a glob with a path component,
/// or `None` where it cannot.
///
/// A globstar matches any number of components. A brace group may hold a
/// separator in an alternative, and a rule compared as text holds its braces as
/// text. A [`RuleIndex`] enumerates the alternatives rather than stop here, and
/// this is what answers for the forms it declines to enumerate: a rule compared
/// as text, an escaped brace, and one with more alternatives than the cap.
///
/// A component ending in an unpaired backslash escapes the separator after it:
/// `a\/b` names `a/b`, not a component `a\`. And where `brackets` says a bracket
/// expression may hold a separator, which it matches none of, the text's
/// components from the first `[` on need not be the path's.
fn component_kind(component: &str, brackets: bool) -> Option<ComponentKind> {
    if is_globstar(component) {
        return None;
    }
    let mut kind = ComponentKind::Text;
    let mut escaping = false;
    for byte in component.bytes() {
        match byte {
            b'{' => return None,
            b'[' if brackets => return None,
            b'*' | b'?' | b'[' | b']' => kind = ComponentKind::Pattern,
            b'\\' => {
                kind = ComponentKind::Pattern;
                escaping = !escaping;
                continue;
            }
            _ => {}
        }
        escaping = false;
    }
    (!escaping).then_some(kind)
}

/// Whether a `/` lies between the first `[` and the last `]` in `glob`, so a
/// bracket expression may hold it. One matches no separator, so `x[a/b]y`
/// matches `xay`.
fn bracket_holds_separator(glob: &str) -> bool {
    glob.find('[').is_some_and(|open| {
        glob.rfind(']')
            .is_some_and(|close| open < close && glob[open..close].contains('/'))
    })
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
#[lore_macro::test_pub]
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
    #[lore_macro::test_pub]
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
    #[lore_macro::test_pub]
    fn reaches(self, floor: u32, depth: u32) -> bool {
        self.lines > floor && self.depth > depth
    }
}

/// Most alternation-free forms of one rule a [`RuleIndex`] enumerates.
///
/// Nested groups multiply, so this bounds what a filter can cost to index. Six
/// two-way groups reach it; a rule written by hand carries a handful.
const MAX_ALTERNATIVES: usize = 64;

/// Component index over the lines of one polarity, answering whether any of
/// them can match below a directory without evaluating a whole glob.
///
/// A walk needs that answer for every directory it meets. Over the inclusions it
/// decides whether to descend into an excluded directory anyway; over the
/// exclusions it decides whether an included one can be taken whole, which is
/// what [`FilterInstance::covers_subtree`] asks. Deriving either from the lines
/// would cost a scan of every line per directory, and a filter built from diff
/// paths carries up to `SOURCE_FILTER_THRESHOLD` of them.
///
/// Two independent bounds keep a rule out of an answer, and a rule escapes the
/// index only by escaping both. The trie bounds *where* it can match: each edge
/// is a leading component of a glob, compared with one path component as the
/// matcher compares it, so `*/saved/config` reaches below `game/saved` and not
/// below `game/saved/logs`. It follows a glob up to the first component it
/// cannot compare, such as a `**` or a brace group, below which the rule
/// reaches any path. The [`RuleReach`] depth bounds *how deep*, and so bounds
/// such a rule too: `/{a,b}`, which compares its braces as text, cannot match
/// below the first level.
///
/// Over-approximating costs traversal, under-approximating drops content, so
/// every uncertain case answers "a rule reaches here".
#[derive(Clone, Debug, Default)]
struct RuleIndex {
    root: RuleNode,
    /// Rules no leading component can rule out: a name rule, matched at any
    /// depth, and a glob whose first component the index cannot compare.
    unprefixed: RuleReach,
}

#[derive(Clone, Debug, Default)]
struct RuleNode {
    /// Component to child: the text components sorted by their text, then the
    /// patterns. A walk finds the one text that matches through
    /// [`text`](Self::text), as adding a rule does, and tries every pattern. A
    /// `Vec` rather than a map: a lookup is a few compares with no hashing.
    children: Vec<(Component, RuleNode)>,
    /// Rules sitting strictly below this node.
    below: RuleReach,
    /// Rules whose next component the index cannot compare, such as a `**`, so
    /// they could match anything below this node.
    wildcard_tail: RuleReach,
}

/// A leading component of an indexed glob, compared with one path component as
/// `kind` says.
#[derive(Clone, Debug)]
struct Component {
    glob: String,
    kind: ComponentKind,
}

impl RuleNode {
    /// Records `reach` as below this node, and returns the child for the
    /// component `glob` compared as `kind`, added if it is not there.
    ///
    /// The kind is part of the edge: `[x]` as text and `[x]` as a pattern match
    /// different names, so they lead to different children.
    fn child(&mut self, glob: &str, kind: ComponentKind, reach: RuleReach) -> &mut RuleNode {
        self.below.widen(reach);
        let found = match kind {
            ComponentKind::Pattern => self
                .patterns()
                .position(|(edge, _)| edge.glob == glob)
                .map(|from_last| self.children.len() - 1 - from_last)
                .ok_or(self.children.len()),
            ComponentKind::Text => self.text(glob).ok_or_else(|| {
                self.children.partition_point(|(edge, _)| {
                    (edge.kind, edge.glob.as_str()) < (ComponentKind::Text, glob)
                })
            }),
        };
        let position = found.unwrap_or_else(|position| {
            let edge = Component {
                glob: glob.to_owned(),
                kind,
            };
            self.children.insert(position, (edge, RuleNode::default()));
            position
        });
        &mut self.children[position].1
    }

    /// The pattern edges, which follow every text edge, from the last.
    fn patterns(&self) -> impl Iterator<Item = &(Component, RuleNode)> {
        self.children
            .iter()
            .rev()
            .take_while(|(edge, _)| edge.kind == ComponentKind::Pattern)
    }

    /// The index of the text edge for `name`. A node of at most
    /// [`SCANNED_CHILDREN`] edges is scanned and a wider one searched, which is
    /// the cheaper of the two at either width.
    fn text(&self, name: &str) -> Option<usize> {
        if self.children.len() <= SCANNED_CHILDREN {
            return self
                .children
                .iter()
                .position(|(edge, _)| edge.kind == ComponentKind::Text && edge.glob == name);
        }
        self.children
            .binary_search_by(|(edge, _)| {
                (edge.kind, edge.glob.as_str()).cmp(&(ComponentKind::Text, name))
            })
            .ok()
    }
}

/// Most edges [`RuleNode::text`] scans for a text component; a wider node is
/// searched. An equality scan compares an edge's text only where its length
/// matches, which on a narrow node costs less than a binary search's ordered
/// comparisons.
#[lore_macro::test_pub]
const SCANNED_CHILDREN: usize = 16;

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
    entries: Arc<DashMap<u64, Ancestor, BuildHasherDefault<PassThroughHasher>>>,
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
    /// A caller passes `false` for a rule [`FilterInstance::step`] compares as
    /// text rather than evaluating as a glob: that rule's own text is the only
    /// thing it matches, so enumerating would record paths it never names and
    /// leave out the one it does.
    fn insert(&mut self, glob: &str, expand: bool, line: usize, depth: u32) {
        if !expand || first_brace_group(glob).is_none() {
            self.insert_form(glob, line, depth);
            return;
        }

        let mut pending = vec![glob.to_owned()];
        let mut enumerated = 0;
        while let Some(form) = pending.pop() {
            let Some((open, close)) = first_brace_group(&form) else {
                self.insert_form(
                    &form,
                    line,
                    FilterInstance::depth_range(&form, false, false).1,
                );
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
    /// the next component, maybe deeper. A component the index cannot compare
    /// ends the descent and marks the node it stops at, because the rule could
    /// match anything below it. A glob compared throughout names its final node
    /// and marks nothing below it.
    fn insert_form(&mut self, glob: &str, line: usize, depth: u32) {
        let reach = RuleReach::rule(line, depth);
        let brackets = bracket_holds_separator(glob);
        let mut node = &mut self.root;
        for (index, component) in glob.split('/').enumerate() {
            let Some(kind) = component_kind(component, brackets) else {
                if index == 0 {
                    self.unprefixed.widen(reach);
                }
                node.wildcard_tail.widen(reach);
                return;
            };
            node = node.child(component, kind, reach);
        }
    }

    /// Records the rule on line `line`, which matches everything strictly below
    /// `path` at any depth. Every component of `path` is text, whatever it
    /// holds.
    fn insert_subtree(&mut self, path: &str, line: usize) {
        let reach = RuleReach::rule(line, u32::MAX);
        let mut node = &mut self.root;
        for component in path.split('/') {
            node = node.child(component, ComponentKind::Text, reach);
        }
        node.wildcard_tail.widen(reach);
    }

    /// Records the rule on line `line` as one no prefix bounds, so it reaches any
    /// path. `depth` bounds it as in [`insert`](Self::insert).
    fn insert_unprefixed(&mut self, line: usize, depth: u32) {
        self.unprefixed.widen(RuleReach::rule(line, depth));
    }

    /// One past the last line of a rule the index records; zero for none.
    fn lines(&self) -> u32 {
        self.root.below.lines.max(self.unprefixed.lines)
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
    /// Each component of `path` is compared with the edges out of the node the
    /// walk stands on, and may match more than one, so the walk follows each.
    /// It reads no more of a rule than its components up to the first it cannot
    /// compare, and returns early when nothing in the index is late enough or
    /// deep enough to matter, and when it falls off the indexed branches.
    ///
    /// An empty `path` is the repository root, which names no component and
    /// holds everything: any rule the index carries at all lands below it.
    fn below(&self, path: &str, floor: u32, depth: u32) -> bool {
        self.unprefixed.reaches(floor, depth)
            || (self.root.below.reaches(floor, depth) && self.root.reaches(path, floor, depth))
    }
}

impl RuleNode {
    /// Whether a rule recorded at or below this node can match a path strictly
    /// below the queried one. `rest` is the part of that path below this node.
    /// See [`RuleIndex::below`].
    ///
    /// Follows the one text edge that matches in a loop, and each pattern edge
    /// that matches in a call of its own.
    fn reaches(&self, mut rest: &str, floor: u32, depth: u32) -> bool {
        let mut node = self;
        while !rest.is_empty() {
            if node.wildcard_tail.reaches(floor, depth) {
                return true;
            }
            let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
            rest = tail;
            for (edge, child) in node.patterns() {
                if (edge.glob == "*" || glob_match::glob_match(&edge.glob, name))
                    && child.reaches(rest, floor, depth)
                {
                    return true;
                }
            }
            match node.text(name) {
                Some(index) => node = &node.children[index].1,
                None => return false,
            }
        }
        node.below.reaches(floor, depth) || node.wildcard_tail.reaches(floor, depth)
    }
}

/// Most leading components [`LinePrefixes`] lists a line under. A line with a
/// longer literal prefix is listed under its first ones, which bounds the lists
/// one step merges.
const MAX_PREFIX_COMPONENTS: usize = 7;

/// Fewest lines a filter indexes in [`LinePrefixes`]. A step over fewer scans
/// every line.
#[lore_macro::test_pub]
const INDEXED_LINES: usize = 1000;

/// The lines of one filter by their leading literal components.
///
/// A glob whose leading components are text, such as `engine/binaries` in
/// `engine/binaries/*.pdb`, matches only paths that start with those
/// components: the matcher compares text byte for byte, and a `/` outside a
/// bracket expression matches one separator. Such a line is listed at the node
/// those components lead to, so a step visits it only for a path that starts
/// with them. A name rule, and a glob whose first component the index cannot
/// compare as text, is listed at the root and visited for every path.
///
/// Lines are added in authored order, so every list ascends, and a step merges
/// the lists along a path back into authored order. The nodes form an arena,
/// with the root at index 0. The edges' text shares one buffer and a node keeps
/// up to four lines inline, so a node holding no more than four lines and no
/// children allocates nothing of its own.
#[derive(Clone, Debug)]
struct LinePrefixes {
    nodes: Vec<PrefixNode>,
    /// The text of every edge, each a range of it.
    text: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct PrefixNode {
    /// Edges to the children, sorted by their text.
    children: Vec<PrefixEdge>,
    /// Lines whose leading literal components, at most
    /// [`MAX_PREFIX_COMPONENTS`] of them, lead to this node, ascending. The root
    /// holds the lines with none, name rules among them.
    lines: SmallVec<[u32; 4]>,
}

/// An edge from a [`PrefixNode`]: the range of [`LinePrefixes::text`] its
/// component spans, and the child it leads to.
#[derive(Clone, Copy, Debug)]
struct PrefixEdge {
    start: u32,
    end: u32,
    child: u32,
}

impl LinePrefixes {
    /// An index holding the root alone.
    fn new() -> Self {
        Self {
            nodes: vec![PrefixNode::default()],
            text: Vec::new(),
        }
    }

    /// Lists `line`, the line numbered `number`.
    ///
    /// The literal prefix ends at the first component holding `*`, `?`, `[`,
    /// `]`, `\` or `{`, which [`component_kind`] reads as other than text.
    fn insert(&mut self, line: &FilterLine, number: usize) {
        let mut node = 0;
        if !line.filename {
            for component in line.glob.split('/').take(MAX_PREFIX_COMPONENTS) {
                if component_kind(component, false) != Some(ComponentKind::Text) {
                    break;
                }
                node = self.child(node, component.as_bytes());
            }
        }
        self.nodes[node].lines.push(number as u32);
    }

    /// The edge out of `node` for `component`: `Ok` with its index, or `Err` with
    /// the index it sorts at.
    fn edge(&self, node: &PrefixNode, component: &[u8]) -> Result<usize, usize> {
        node.children.binary_search_by(|edge| {
            self.text[edge.start as usize..edge.end as usize].cmp(component)
        })
    }

    /// The child of node `parent` for `component`, added if it is not there.
    fn child(&mut self, parent: usize, component: &[u8]) -> usize {
        match self.edge(&self.nodes[parent], component) {
            Ok(index) => self.nodes[parent].children[index].child as usize,
            Err(index) => {
                let start = self.text.len() as u32;
                self.text.extend_from_slice(component);
                let child = self.nodes.len();
                self.nodes.push(PrefixNode::default());
                let edge = PrefixEdge {
                    start,
                    end: self.text.len() as u32,
                    child: child as u32,
                };
                self.nodes[parent].children.insert(index, edge);
                child
            }
        }
    }

    /// The lines numbered `floor` or later listed at the root and at each node
    /// `path`'s leading components lead to, one ascending list per node that
    /// holds any, and how many lists there are. Always inlined, since
    /// [`visit`](Self::visit) runs for every step of an indexed filter.
    #[inline(always)]
    fn lists(&self, path: &str, floor: usize) -> ([&[u32]; MAX_PREFIX_COMPONENTS + 1], usize) {
        let mut node = &self.nodes[0];
        let mut lists: [&[u32]; MAX_PREFIX_COMPONENTS + 1] = [&[]; MAX_PREFIX_COMPONENTS + 1];
        let mut count = 0;
        let mut components = path.split('/');
        loop {
            let start = node.lines.partition_point(|&line| (line as usize) < floor);
            if start < node.lines.len() {
                lists[count] = &node.lines[start..];
                count += 1;
            }
            let Some(component) = components.next() else {
                break;
            };
            let Ok(index) = self.edge(node, component.as_bytes()) else {
                break;
            };
            node = &self.nodes[node.children[index].child as usize];
        }
        (lists, count)
    }

    /// Calls `each` in ascending order with every line numbered `floor` or later
    /// that is listed at the root or at a node `path`'s leading components lead
    /// to, which includes every line that can match `path`.
    fn visit(&self, path: &str, floor: usize, mut each: impl FnMut(usize)) {
        let (mut lists, count) = self.lists(path, floor);
        let lists = &mut lists[..count];
        loop {
            let mut next: Option<(usize, u32)> = None;
            for (index, list) in lists.iter().enumerate() {
                if let Some(&line) = list.first()
                    && next.is_none_or(|(_, lowest)| line < lowest)
                {
                    next = Some((index, line));
                }
            }
            let Some((index, line)) = next else {
                return;
            };
            lists[index] = &lists[index][1..];
            each(line as usize);
        }
    }

    /// The last of the lines [`visit`](Self::visit) would visit that `accepts`
    /// takes, found by searching each list from its end.
    fn last(
        &self,
        path: &str,
        floor: usize,
        mut accepts: impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        let (lists, count) = self.lists(path, floor);
        let mut last = None;
        for list in &lists[..count] {
            for &line in list.iter().rev() {
                let line = line as usize;
                if last.is_some_and(|found| line <= found) {
                    break;
                }
                if accepts(line) {
                    last = Some(line);
                    break;
                }
            }
        }
        last
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
    /// path's last component. `**/name` and `/**/name` mean the same thing --
    /// gitignore documents `**/foo` as "the same as pattern `foo`" -- so the
    /// prefix is stripped and the rule becomes a name rule, which keeps the
    /// commonest patterns off `**` and its separator-crossing backtrack.
    ///
    /// `**/a/b` is not the same as `a/b`: it names `b` directly under any `a`, so
    /// it keeps the prefix and is matched against the whole path. A bare `**`
    /// names no component at all and is matched against the whole path too.
    ///
    /// A component of two or more `*` is a globstar, as in git, and compiles to
    /// `**`, so every reader of the compiled glob meets one spelling of it. Git
    /// reads one followed by an escaped `/` as a globstar matching at least one
    /// component, so `a/**\/b` compiles to `a/*/**/b` rather than `a/**/*/b`:
    /// the matcher can miss a match where a `**` is followed by a component
    /// holding a wildcard, and confirms none in a glob holding an escape or a
    /// brace group.
    #[lore_macro::test_pub]
    fn compile(glob: &str) -> (String, bool, bool) {
        let leading_separator = glob.starts_with('/');
        let ending_separator = glob.ends_with('/');
        let mut glob = glob.trim_matches('/').to_lowercase();
        if glob.contains("***") || glob.contains("**\\/") {
            let mut components = glob.split('/').peekable();
            let mut spelled = Vec::new();
            while let Some(component) = components.next() {
                let stars = match component.strip_suffix('\\') {
                    Some(stars) if components.peek().is_some() => stars,
                    _ => component,
                };
                spelled.push(if !is_globstar(stars) {
                    component
                } else if stars.len() < component.len() {
                    "*/**"
                } else {
                    "**"
                });
            }
            glob = spelled.join("/");
        }

        let stripped = glob
            .strip_prefix("**/")
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
        if stripped {
            glob.drain(..3);
        }

        let filename = glob != "**" && (stripped || (!leading_separator && !glob.contains('/')));
        (glob, filename, ending_separator)
    }

    /// How many path components `glob` can match: fewest, and most or
    /// [`u32::MAX`] where nothing bounds it. A name rule is matched against the
    /// last component alone, so it applies at any depth. `braced` says the
    /// matcher evaluates the glob's brace groups rather than reading them as text.
    ///
    /// Both bounds count the components of the text split at `/`. The lower
    /// bound lets [`step`](Self::step) skip a line against a prefix with fewer
    /// components: it counts every component except a `**` that is not the last,
    /// which may absorb nothing, so `a/**/b` can match `a/b` while `a/**` does
    /// not match `a`. It is 1 where a `/` in the text may match no separator:
    /// when the text between the first `[` and the last `]` holds one, since a
    /// bracket expression matches no separator, so `x[a/b]y` matches `xay`, and
    /// when a brace group the matcher evaluates holds one, since an alternative
    /// may hold fewer than the group, so `{a,b/c}*` matches `ax`. Every other `/`
    /// matches exactly one separator.
    ///
    /// The upper bound is what a [`RuleIndex`] records, so that a rule unable to
    /// reach below a directory does not force a walk into it -- without it a
    /// rule the index stops following at its first component, such as `/{a,b}`
    /// compared as text, would reach every path. It is the count, or unbounded
    /// where a component is exactly `**`, the one construct the matcher lets
    /// match a separator that no `/` in the glob stands for.
    /// `*`, `?` and a bracket expression never match one, and a `**` sharing its
    /// component with other text is two `*`. A `/` outside a bracket expression
    /// matches one separator, escaped or not, a `/` inside one matches none, and
    /// a brace group matches the separators of one alternative. So no path the
    /// glob matches has more components than the text, which is the direction
    /// that has to hold: over-estimating costs traversal, under-estimating drops
    /// content.
    ///
    /// Both bounds come from one pass because both callers want both.
    #[lore_macro::test_pub]
    fn depth_range(glob: &str, filename: bool, braced: bool) -> (u32, u32) {
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
        let mut fewest = if bracket_holds_separator(glob) {
            1
        } else {
            (total - optional).max(1)
        };
        if braced && fewest > 1 {
            let mut rest = glob;
            while let Some((open, close)) = first_brace_group(rest) {
                if rest[open..close].contains('/') {
                    fewest = 1;
                    break;
                }
                rest = &rest[close + 1..];
            }
        }
        (fewest, if unbounded { u32::MAX } else { total })
    }

    /// How many lines the filter holds: one per authored rule, and one more for
    /// each subtree companion.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the filter holds no lines, so it excludes nothing.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Appends `line`, and lists it by its leading literal components once the
    /// filter holds [`INDEXED_LINES`] lines, listing all of them on reaching it.
    fn push_line(&mut self, line: FilterLine) {
        self.lines.push(line);
        let count = self.lines.len();
        if count < INDEXED_LINES {
            return;
        }
        let first = if count == INDEXED_LINES { 0 } else { count - 1 };
        let prefixes = self
            .prefixes
            .get_or_insert_with(|| Box::new(LinePrefixes::new()));
        for (number, line) in self.lines.iter().enumerate().skip(first) {
            prefixes.insert(line, number);
        }
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
        let literal = is_literal(&glob);
        let expand = !filename && !literal && glob.contains('{');
        let (min_depth, depth_max) = Self::depth_range(&glob, filename, expand);
        if filename {
            self.exclude.insert_unprefixed(self.lines.len(), depth_max);
        } else {
            self.exclude
                .insert(&glob, expand, self.lines.len(), depth_max);
        }
        self.push_line(FilterLine {
            glob,
            negated: false,
            directory: ending_separator,
            filename,
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
    /// The companion is always evaluated as a glob, so for a rule compared as
    /// text that holds a `{` it escapes every `{`, `}` and `,`: `!a{b,c}`
    /// re-includes what lies below `a{b,c}`, not below `ab`.
    ///
    /// Only a path rule gets a companion, and only a path rule enters the
    /// re-inclusion index. A name rule is skipped by [`step`](Self::step) below an
    /// excluded directory, so `!keep.txt` neither re-opens a pruned subtree nor
    /// sends a walk looking for one -- which is how git reads it too: with `*` and
    /// `!keep.txt`, only the top-level `keep.txt` comes back.
    ///
    /// # Errors
    ///
    /// A path inclusion whose first authored component is a globstar, however
    /// it is written: `!**/a/b`, `!/**/a/b`, `!**`, `!/**`, `!***` or `!**\/a`.
    /// Such a rule matches from every directory in the repository, so the walk
    /// would have to enter each excluded one to look. `!**/name` and `!/**/name`
    /// compile to the name rule `!name` and are accepted, and so is `!**b/c`,
    /// whose `**b` is no globstar. A rule with another first component is
    /// accepted however far it reaches, such as `!*/**/c`.
    pub fn add_inclusion(&mut self, raw: &str) -> Result<(), FilterError> {
        let (glob, filename, ending_separator) = Self::compile(raw);
        let mut components = raw.trim_matches('/').split('/');
        let first = components.next().unwrap_or_default();
        let stars = first
            .strip_suffix('\\')
            .filter(|_| components.next().is_some())
            .unwrap_or(first);
        if !filename && is_globstar(stars) {
            return Err(FilterError::internal(
                "filter inclusions cannot start with a ** component as that will force traversal of the entire revision tree",
            ));
        }

        let literal = is_literal(&glob);
        // A name rule is neither indexed nor given a companion, so it pays no scan.
        let braced = !filename && glob.contains('{');
        let expand = braced && !literal;
        let subtree = (!filename && !glob.ends_with('*')).then(|| {
            if braced && literal {
                escaped_subtree(&glob)
            } else {
                format!("{glob}/**")
            }
        });

        let (min_depth, depth_max) = Self::depth_range(&glob, filename, expand);
        if !filename {
            self.reinclude
                .insert(&glob, expand, self.lines.len(), depth_max);
        }
        self.push_line(FilterLine {
            min_depth,
            literal,
            glob,
            negated: true,
            directory: ending_separator,
            filename,
        });

        if let Some(subtree) = subtree {
            let (min_depth, depth_max) = Self::depth_range(&subtree, false, expand);
            self.reinclude
                .insert(&subtree, expand, self.lines.len(), depth_max);
            self.push_line(FilterLine {
                min_depth,
                literal: false,
                glob: subtree,
                negated: true,
                directory: false,
                filename: false,
            });
        }

        Ok(())
    }

    /// Appends a re-inclusion of `path` and everything below it, whatever
    /// characters it holds.
    ///
    /// `path` is compared as text against the whole path, never evaluated as a
    /// glob nor matched as a name, so a top-level path names itself alone. It
    /// gets a subtree companion, as a path rule does on
    /// [`add_inclusion`](Self::add_inclusion), with every character of `path`
    /// the matcher reads as syntax escaped.
    ///
    /// The re-inclusion index records the companion alone, with every component
    /// of `path` as text. It reaches every node the line for `path` itself
    /// does, so a walk descends towards `path` and below it, and nowhere else.
    pub fn add_path_inclusion(&mut self, path: &RelativePath) {
        let glob = path.as_lowercase_str();
        let depth = component_count(glob);
        self.reinclude.insert_subtree(glob, self.lines.len() + 1);
        self.push_line(FilterLine {
            glob: glob.to_owned(),
            negated: true,
            min_depth: depth,
            literal: true,
            ..FilterLine::default()
        });
        self.push_line(FilterLine {
            glob: escaped_subtree(glob),
            negated: true,
            min_depth: depth + 1,
            ..FilterLine::default()
        });
    }

    /// Advances the walk one level: the verdict for `path`, given the verdict
    /// for its parent.
    ///
    /// Lines are applied in order and a later match overrides an earlier one.
    /// These narrow the scan:
    ///
    /// - In a filter of [`INDEXED_LINES`] lines or more, only the lines
    ///   [`LinePrefixes`] lists for `match_path` are visited: a line whose
    ///   leading literal components differ from the path's cannot match it.
    /// - An inclusion can only clear `excluded` and an exclusion can only set
    ///   it, so a line whose effect equals the current state could change only
    ///   the floor, and is skipped before the glob.
    /// - A name rule is skipped when the parent is excluded: it cannot reach into
    ///   a directory a walk would have pruned.
    /// - A line needing more components than `depth` cannot match and is skipped.
    /// - When the parent is excluded, lines before the parent's floor are
    ///   skipped: for a directory a re-inclusion can reach below, the last line
    ///   that excluded it. A tree walk stops at an excluded directory, so a rule
    ///   that already lost there must not win below it -- that is what keeps
    ///   `!/src` + `/src/*` from re-including `src/drop/y` through the `src/**`
    ///   companion. When the parent is *included* nothing was pruned, so every
    ///   line applies and an unanchored `*.tmp` still bites inside a re-included
    ///   subtree.
    ///
    /// For an excluded directory that a re-inclusion can reach below and an
    /// exclusion follows the floor, [`last_exclusion`](Self::last_exclusion)
    /// then finds the last of the exclusions skipped as changing only the floor,
    /// which floors the steps below it.
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
        let mut apply = |number: usize, line: &FilterLine| {
            if line.negated != state.excluded {
                return;
            }
            if line.directory && !is_directory {
                return;
            }
            if pruned && line.negated && line.filename {
                return;
            }
            if line.min_depth > depth {
                return;
            }
            if line.hits(match_path, match_name) {
                state = FilterState {
                    excluded: !line.negated,
                    decided_at: number as u32,
                };
            }
        };
        match &self.prefixes {
            None => {
                for (offset, line) in self.lines[floor..].iter().enumerate() {
                    apply(floor + offset, line);
                }
            }
            Some(prefixes) => prefixes.visit(match_path, floor, |number| {
                apply(number, &self.lines[number]);
            }),
        }
        if is_directory
            && state.excluded
            && self.exclude.lines() > state.decided_at.saturating_add(1)
        {
            state.decided_at = self.last_exclusion(match_path, match_name, depth, state.decided_at);
        }
        state
    }

    /// The last exclusion after line `from` that matches the directory
    /// `match_path`, where a re-inclusion can reach below it, or `from` where
    /// none does or none can.
    ///
    /// Meaningful only for a directory; it floors the steps below it. Kept cold
    /// and out of [`step`](Self::step), which runs for every node a walk
    /// visits and reaches this only for an excluded directory.
    #[cold]
    #[inline(never)]
    fn last_exclusion(&self, match_path: &str, match_name: &str, depth: u32, from: u32) -> u32 {
        if !self.reinclude.below(match_path, from, depth) {
            return from;
        }
        let start = from as usize + 1;
        let excludes = |line: &FilterLine| {
            !line.negated && line.min_depth <= depth && line.hits(match_path, match_name)
        };
        match &self.prefixes {
            None => self
                .lines
                .get(start..)
                .and_then(|lines| lines.iter().rposition(excludes))
                .map_or(from, |offset| (start + offset) as u32),
            Some(prefixes) => prefixes
                .last(match_path, start, |number| excludes(&self.lines[number]))
                .map_or(from, |number| number as u32),
        }
    }

    /// The exclusion verdict for `path`, given its parent's.
    ///
    /// The threaded form of [`excludes`](Self::excludes): one pass over the
    /// lines rather than one per ancestor, because the caller already holds the
    /// parent's verdict. Read the answer with
    /// [`FilterState::excluded`], and pass the returned state to
    /// [`should_descend`](Self::should_descend) and to the children below it.
    #[cfg(feature = "test-util")]
    #[lore_macro::test_pub]
    fn child_exclusion_state(
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
    /// done.
    #[cfg(feature = "test-util")]
    #[lore_macro::test_pub]
    fn exclusion_state(&self, path: &impl FilterPath, is_directory: bool) -> FilterState {
        self.exclusion_state_settled(path, is_directory).0
    }

    /// The exclusion verdict for a path that arrives whole, folded down its own
    /// ancestors, and whether the fold ended on a prefix that settles its whole
    /// subtree.
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
            if self.settles_subtree(state, prefix, depth) {
                return (state, true);
            }
        }
        (state, false)
    }

    /// Whether `state` excludes `path` and no rule can re-include anything below
    /// it, so every descendant is excluded too. `path` is the lowercase form
    /// `state` was produced for, and `depth` is how many components it has.
    fn settles_subtree(&self, state: FilterState, path: &str, depth: u32) -> bool {
        if !state.excluded {
            return false;
        }
        debug_assert_eq!(
            depth,
            query_depth(path),
            "a depth that is not {path:?}'s own reports a subtree as settled that is not"
        );
        !self.reinclude.below(path, state.decided_at, depth)
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
            self.settles_subtree(state, match_path, depth)
        } else {
            state.excluded
        }
    }

    /// Whether `path` is excluded by this slot alone, its ancestors accounted for.
    ///
    /// Folds the ancestors on every call. [`Filter::excludes`] answers the same
    /// question across both slots and folds a directory once; this is the
    /// single-slot form, and the unmemoized one the filter tests hold the
    /// memoized answers against.
    #[cfg(feature = "test-util")]
    #[lore_macro::test_pub]
    fn excludes(&self, path: &impl FilterPath, is_directory: bool) -> bool {
        self.exclusion_state(path, is_directory).excluded
    }

    /// Whether this slot lets a walk standing at `path` descend into it.
    ///
    /// An excluded directory is still descended when a rule could re-include
    /// something below it; the contents stay excluded unless such a rule
    /// actually matches them. Git prunes there instead and documents
    /// re-inclusion below an excluded directory as impossible -- one of the
    /// departures `tests/filter_gitignore.rs` enumerates and asserts.
    #[cfg(feature = "test-util")]
    #[lore_macro::test_pub]
    fn should_descend(&self, state: FilterState, path: &impl FilterPath) -> bool {
        if !state.excluded || path.is_empty() || path.as_str() == "." {
            return true;
        }
        let lowercase = path.as_lowercase_str();
        !self.settles_subtree(state, lowercase, component_count(lowercase))
    }

    /// Whether `path` is excluded and no rule can re-include anything below it,
    /// so every path below it is excluded too.
    pub fn excludes_subtree(&self, path: &RelativePath) -> bool {
        if path.is_empty() || path.as_str() == "." {
            return false;
        }
        self.exclusion_state_settled(path, true).1
    }

    /// Whether a rule can re-include something below a directory this filter excludes, so a
    /// walk can enter one.
    pub fn reincludes(&self) -> bool {
        self.reinclude.below("", 0, 0)
    }
}

/// Data for the event emitted when a path is excluded by a filter.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
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
        /// The ignore slot does not exclude a node the tree tracks: the ignore rules govern only
        /// what is new to the tree. The slot is still stepped, so an untracked child of a
        /// tracked, ignored directory is ignored. A walk clears the bit for each node the tree
        /// does not hold; see [`FilterMode::at`].
        const KeepTracked = 0b1000;
        /// The view slot does not exclude a node that holds a staged change at or below it. The
        /// slot is still stepped, and the bit is cleared for each node the tree does not hold, as
        /// [`KeepTracked`](Self::KeepTracked) is, and for each node that holds nothing staged; see
        /// [`FilterMode::at_staged`].
        const KeepStagedInView = 0b10000;
    }
}
bitflagsops!(FilterMode, u16);

impl FilterMode {
    /// The mode for local state: the view, and the ignore rules for what the tree does not hold.
    ///
    /// A comparison of trees holds nothing untracked, so it asks [`at`](Self::at) whether
    /// something is staged or marked dirty at or below a node instead: a subtree with nothing
    /// changed in it has nothing to report, and an ignored one is pruned without being entered.
    pub const TRACKED: Self = Self::Full.union(Self::KeepTracked);

    /// The mode for the changes a stage holds: [`TRACKED`](Self::TRACKED), and the view, which
    /// excludes nothing staged.
    ///
    /// A comparison of trees in this mode also asks [`at_staged`](Self::at_staged) whether
    /// something is staged at or below a node, so a subtree past the view with nothing staged in
    /// it is pruned without being entered.
    pub const CHANGES: Self = Self::TRACKED.union(Self::KeepStagedInView);

    /// The bits that keep a slot from excluding a tracked node.
    const KEEPS: Self = Self::KeepTracked.union(Self::KeepStagedInView);

    /// The mode a walk through `self` asks about a node in, `tracked` saying whether the node is
    /// one the tree tracks.
    pub fn at(self, tracked: bool) -> Self {
        if tracked {
            self
        } else {
            self.difference(Self::KEEPS)
        }
    }

    /// The mode a walk through `self` asks about a node in, `staged` saying whether the node holds
    /// a staged change at or below it.
    pub fn at_staged(self, staged: bool) -> Self {
        if staged {
            self
        } else {
            self.difference(Self::KeepStagedInView)
        }
    }

    /// The slots a walk through `self` asks about a deletion that a path missing from the working
    /// tree drives: the view alone where the walk is forced, since a forced walk passes the ignore
    /// file and the working tree lacks what the view excludes without its having been deleted, and
    /// `self` otherwise.
    pub fn absence(self, force: bool) -> Self {
        if force { Self::View } else { self }
    }

    /// The slots that exclude.
    fn excluding(self) -> Self {
        let mut slots = self;
        if self.contains(Self::KeepTracked) {
            slots.remove(Self::Ignore);
        }
        if self.contains(Self::KeepStagedInView) {
            slots.remove(Self::View);
        }
        slots
    }
}

/// How far a query's verdict reaches.
///
/// Only a directory can tell the two apart: it can be excluded by its own rule
/// and still hold content a later rule re-includes, and then the answer depends
/// on which question was asked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
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

    /// These states with each slot outside `mode` at its root state.
    fn only(self, mode: FilterMode) -> Self {
        Self {
            ignore: if mode.contains(FilterMode::Ignore) {
                self.ignore
            } else {
                FilterState::ROOT
            },
            view: if mode.contains(FilterMode::View) {
                self.view
            } else {
                FilterState::ROOT
            },
        }
    }

    /// Whether the path these states describe is itself excluded, for the slots in `mode`.
    pub fn excluded(self, mode: FilterMode) -> bool {
        self.reason(mode).is_some()
    }

    /// The slot in `mode` that excludes the path these states describe, ignore ahead of view.
    fn reason(self, mode: FilterMode) -> Option<FilterReason> {
        if mode.contains(FilterMode::Ignore) && self.ignore.excluded {
            Some(FilterReason::Ignore)
        } else if mode.contains(FilterMode::View) && self.view.excluded {
            Some(FilterReason::View)
        } else {
            None
        }
    }
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
    /// The threaded form of [`excludes`](Self::excludes); see
    /// [`FilterInstance::child_exclusion_state`].
    ///
    /// Ignore is reported before view at the same depth, which is the order a
    /// walk would have hit them.
    #[cfg(feature = "test-util")]
    #[lore_macro::test_pub]
    fn child_exclusion_states(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, Option<FilterReason>) {
        self.child_exclude_reason(parent, path, is_directory, mode, Scope::Node)
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
        self.descends(states, path.as_lowercase_str(), mode)
    }

    /// [`should_descend`](Self::should_descend) for the lowercase path `states` were produced
    /// for.
    fn descends(&self, states: FilterStates, lowercase: &str, mode: FilterMode) -> bool {
        let mode = mode.excluding();
        if !states.excluded(mode) || lowercase.is_empty() || lowercase == "." {
            return true;
        }
        self.descend_reason(states, lowercase, component_count(lowercase), mode)
            .is_none()
    }

    /// Whether the filter leaves out `path` whatever it holds, as a walk does not descend into
    /// its parent, whose verdict is `parent`, emitting a [`LoreEvent::FilterExclude`] where it
    /// does. A path its parent leaves open is answered `false`: what it holds decides.
    pub fn emit_excludes_any_kind(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        mode: FilterMode,
    ) -> bool {
        !self.descends(parent, path.split_lowercase().0, mode)
            && self.child_emit_excludes(parent, path, true, mode).1
    }

    /// The mode a walk asks about `path` in, from `parent`: `mode` where `tracked` answers that the
    /// tree tracks the node there, and `mode` without its keep bits otherwise ([`FilterMode::at`]).
    /// `tracked` is awaited only where `mode` carries a keep bit and the filter would leave the
    /// node out, so a walk reads nothing more for a node it keeps.
    pub async fn tracked_mode<E, F>(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
        tracked: impl FnOnce() -> F,
    ) -> Result<FilterMode, E>
    where
        F: std::future::Future<Output = Result<bool, E>>,
    {
        let untracked = mode.at(false);
        if !mode.intersects(FilterMode::KEEPS)
            || !self
                .child_excludes_tree(parent, path, is_directory, untracked)
                .1
        {
            return Ok(untracked);
        }
        // Boxed: a cold path, which unboxed would sit in every walk future that asks.
        Ok(mode.at(Box::pin(tracked()).await?))
    }

    /// [`emit_excludes_any_kind`](Self::emit_excludes_any_kind) without the event.
    pub fn excludes_any_kind(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        mode: FilterMode,
    ) -> bool {
        !self.descends(parent, path.split_lowercase().0, mode)
            && self.child_excludes_tree(parent, path, true, mode).1
    }

    /// The slot that stops a walk at `path`, or `None` when it descends. `path`
    /// is the lowercase form `states` were produced for, and `depth` is how many
    /// components it has.
    ///
    /// Ignore is consulted before view.
    fn descend_reason(
        &self,
        states: FilterStates,
        path: &str,
        depth: u32,
        mode: FilterMode,
    ) -> Option<FilterReason> {
        if mode.contains(FilterMode::Ignore)
            && self.ignore.settles_subtree(states.ignore, path, depth)
        {
            return Some(FilterReason::Ignore);
        }
        if mode.contains(FilterMode::View) && self.view.settles_subtree(states.view, path, depth) {
            return Some(FilterReason::View);
        }
        None
    }

    /// One step of a walk: the states `path`'s children inherit, and why `path`
    /// is excluded at `scope` -- `None` to keep it and, for a directory at
    /// [`Scope::Tree`], to descend into it.
    ///
    /// The threaded form of [`exclude_reason`](Self::exclude_reason). The caller
    /// holds the parent's states, so no ancestor is folded and the subtree half
    /// is one index lookup rather than a second pass over the lines.
    ///
    /// A file has no subtree, so it is answered the same at either scope.
    pub(crate) fn child_exclude_reason(
        &self,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
        scope: Scope,
    ) -> (FilterStates, Option<FilterReason>) {
        if self.is_empty(mode) || path.is_empty() || path.as_str() == "." {
            return (parent, None);
        }
        let lowercase = path.as_lowercase_str();
        let name = path.name_lowercase();
        let depth = component_count(lowercase);
        let mut states = parent;
        if mode.contains(FilterMode::Ignore) && !self.ignore.lines.is_empty() {
            states.ignore = self
                .ignore
                .step(parent.ignore, lowercase, name, is_directory, depth);
        }
        if mode.contains(FilterMode::View) && !self.view.lines.is_empty() {
            states.view = self
                .view
                .step(parent.view, lowercase, name, is_directory, depth);
        }
        let mode = mode.excluding();
        let reason = if scope == Scope::Tree && is_directory {
            self.descend_reason(states, lowercase, depth, mode)
        } else {
            states.reason(mode)
        };
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
        let (states, reason) =
            self.child_exclude_reason(parent, path, is_directory, mode, Scope::Tree);
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
        let (states, reason) =
            self.child_exclude_reason(parent, path, is_directory, mode, Scope::Tree);
        (states, Self::emit(path, reason))
    }

    /// [`child_excludes_tree`](Self::child_excludes_tree) unless `force` puts the walk past the
    /// filter, where it leaves out nothing and hands back [`FilterStates::ROOT`] without reading
    /// the lines.
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

    /// [`child_emit_excludes`](Self::child_emit_excludes) for the slots in `mode`, unless `force`
    /// puts the walk past the filter. A forced step leaves out nothing and emits nothing, and the
    /// states it hands back step only the slots in `stepped`, at a directory, with every other
    /// slot at its root state. A file, which has nothing below it, takes [`FilterStates::ROOT`].
    /// With `stepped` empty, a forced step reads none of the lines.
    pub fn child_emit_unless_forced(
        &self,
        force: bool,
        stepped: FilterMode,
        parent: FilterStates,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
    ) -> (FilterStates, bool) {
        if !force {
            return self.child_emit_excludes(parent, path, is_directory, mode);
        }
        if !is_directory {
            return (FilterStates::ROOT, false);
        }
        let (states, _) =
            self.child_exclude_reason(parent.only(stepped), path, true, stepped, Scope::Node);
        (states, false)
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
    ///
    /// [`FilterMode::KeepTracked`] keeps the ancestors past the ignore slot as
    /// well as the leaf, since every ancestor of a tracked path is tracked, and
    /// [`FilterMode::KeepStagedInView`] past the view slot, since every ancestor of a staged
    /// node is staged.
    fn exclude_reason(
        &self,
        path: &impl FilterPath,
        is_directory: bool,
        mode: FilterMode,
        scope: Scope,
    ) -> Option<FilterReason> {
        let mode = mode.excluding();
        if self.is_empty(mode) {
            return None;
        }
        let lowercase = path.as_lowercase_str();
        if lowercase.is_empty() || lowercase == "." {
            return None;
        }
        let (parent, name) = path.split_lowercase();
        let ancestor = self.ancestor(parent);
        if let Some(reason) = self.descend_reason(ancestor.states, parent, ancestor.depth, mode) {
            return Some(reason);
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
    /// For a file this is whether `path` itself is excluded. For a directory it is
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
    pub(crate) fn emit(path: &impl FilterPath, reason: Option<FilterReason>) -> bool {
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
