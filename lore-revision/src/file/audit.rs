// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::borrow::Cow;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Serialize;

use crate::change::FileAction;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::file::diff::DiffContent;
use crate::file::diff::PatchEntry;
use crate::file::diff::normalise_text;
use crate::file::diff::read_node_content;
use crate::file::diff::read_working_content;
use crate::file::diff::walk_patch;
use crate::file::history;
use crate::interface::LoreError;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeID;
use crate::repository::RemoteStatus;
use crate::repository::RepositoryContext;
use crate::state;
use crate::state::State;
use crate::util::path::RelativePath;

/// Revisions of a file's predecessor chain an audit walks unless the caller
/// raises the bound. Matches the default `history()` applies to its own walk.
pub const DEFAULT_MAX_REVISIONS: u32 = 100;

#[error_set]
pub enum AuditError {
    InvalidArguments,
    InvalidPath,
    FileNotFound,
    RevisionNotFound,
    BranchNotFound,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    LinkNotFound,
    NodeNotFound,
    NotFound,
    Oversized,
    PayloadNotFound,
    WriteRequired,
    Maintenance,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotSupported,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
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

impl EventError for AuditError {
    fn translated(&self) -> LoreError {
        match self {
            AuditError::InvalidArguments(_) | AuditError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            AuditError::FileNotFound(_) => LoreError::FileNotFound,
            AuditError::RevisionNotFound(_)
            | AuditError::BranchNotFound(_)
            | AuditError::NotFound(_) => LoreError::NotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// What a range of lines resolved to, or why it did not resolve.
///
/// An outcome is named the same way wherever it is reported, as
/// `binary-in-history` rather than `binaryInHistory`.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "kebab-case")]
pub enum LoreFileAuditOutcome {
    /// Resolved to the revision that last changed the lines.
    Attributed = 0,
    /// Differs on disk from the revision the audit read, so no revision holds it.
    Uncommitted = 1,
    /// The revision budget ran out before the lines resolved.
    BudgetExhausted = 2,
    /// A version the walk had to compare no longer holds its content, the
    /// address it is stored at surviving the payload.
    Obliterated = 3,
    /// A version the walk had to compare holds binary content, which cannot be
    /// compared line by line.
    BinaryInHistory = 4,
    /// The chain continues in a linked repository that is unauthorized or out
    /// of reach.
    LinkUnavailable = 5,
    /// A version the walk had to compare is not held locally, and the instance
    /// reads its local store alone.
    ContentUnavailable = 6,
    /// The walk ended with an error before it reached the lines. The error
    /// arrives on the completion event.
    Unresolved = 7,
}

impl LoreFileAuditOutcome {
    pub fn as_string(self) -> &'static str {
        match self {
            LoreFileAuditOutcome::Attributed => "attributed",
            LoreFileAuditOutcome::Uncommitted => "uncommitted",
            LoreFileAuditOutcome::BudgetExhausted => "budget-exhausted",
            LoreFileAuditOutcome::Obliterated => "obliterated",
            LoreFileAuditOutcome::BinaryInHistory => "binary-in-history",
            LoreFileAuditOutcome::LinkUnavailable => "link-unavailable",
            LoreFileAuditOutcome::ContentUnavailable => "content-unavailable",
            LoreFileAuditOutcome::Unresolved => "unresolved",
        }
    }
}

/// Data for the event carrying one range of lines in a file.
///
/// A range names the revision that last changed it and nothing more of that
/// revision: a caller reads the revision's metadata itself, once per distinct
/// revision. It carries no revision number, because numbers are per-branch and
/// reporting one would make the answer depend on the caller's branch.
///
/// Each range is sent as soon as the step of the walk that resolves it ends, so
/// ranges arrive most recently changed first rather than in line order. Each
/// range is complete and never needs joining with another. A walk that ends
/// with an error still reports every line: the lines it resolved keep their outcomes, and the lines
/// it did not reach are reported as `LORE_FILE_AUDIT_OUTCOME_UNRESOLVED`. An
/// audit that fails before the walk starts reports no range. Read the
/// completion status before treating the ranges received as the whole answer.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, bitcode::Encode, bitcode::Decode)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileAuditEventData {
    /// Identifier of the repository the range came from. A walk that follows
    /// the file into a linked repository reports that repository.
    pub repository: RepositoryId,
    /// Revision that last changed the range. Zero unless `outcome` is
    /// `Attributed`.
    pub revision: Hash,
    /// First line of the range, 1-based.
    pub line_first: u32,
    /// Last line of the range, 1-based and inclusive.
    pub line_last: u32,
    /// What the range resolved to, or why it did not resolve.
    pub outcome: LoreFileAuditOutcome,
}

/// What an audit reads, and how far it walks.
#[derive(Clone, Debug)]
pub struct AuditOptions {
    /// Revision to read the file at, in any form a revision specifier takes:
    /// a whole hash signature, `[branch]@<number>`, `[branch]@LATEST` or
    /// `<branch>@<hash>`. `None` reads the revision the instance is synced to.
    pub revision: Option<String>,

    /// Revisions of the file's predecessor chain to walk, and `None` for all of
    /// them. Stated as an option rather than as a zero so that no value of the
    /// type means an unbounded walk by accident.
    pub max_revisions: Option<u32>,

    /// How every comparison the audit makes treats whitespace.
    pub whitespace: Whitespace,
}

/// How a comparison treats whitespace, named as the two switches `lore file
/// diff` exposes for the same rules.
///
/// An audit compares under different defaults from a diff: a diff describes one
/// change literally, while attribution asks who wrote a line, which trailing
/// space and line-ending churn do not answer. Reindentation is a change to the
/// line, so it counts unless a caller says otherwise. The defaults live with the
/// caller-facing arguments; this type carries a decided pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Whitespace {
    /// Treat lines that differ only in trailing whitespace as unchanged.
    pub ignore_space_at_eol: bool,
    /// Collapse runs of internal whitespace to a single space before comparing.
    pub ignore_space_change: bool,
}

/// A line of the audited file that no revision has claimed yet.
#[lore_macro::test_pub]
#[derive(Clone, Copy, PartialEq, Debug)]
struct UnattributedLine {
    /// 1-based line in the file at the audited revision.
    audited: u32,
    /// 1-based line in the version the walk is comparing.
    compared: u32,
}

/// A revision, and the repository whose chain the walk was following when it
/// reached that revision.
///
/// The repository rides on the answer rather than being taken from where the
/// audit started, because a walk that crosses a link claims lines in more than
/// one repository. Two revisions of one hash in two repositories are two
/// different answers, so the pair is what a range is keyed and reported on.
#[lore_macro::test_pub]
#[derive(Clone, Copy, PartialEq, Eq, std::hash::Hash, Debug)]
struct RevisionSource {
    repository: RepositoryId,
    revision: Hash,
}

/// What the audit resolved one line of the audited file to.
#[lore_macro::test_pub]
#[derive(Clone, Copy, PartialEq, Debug)]
enum LineOutcome {
    /// The revision that last changed the line, and where it lives.
    Revision(RevisionSource),
    /// No revision holds the line, for the reason the outcome gives.
    Other(LoreFileAuditOutcome),
}

/// What one walk holds for its whole length: the file it follows, how far it may
/// follow it, how it compares, and where that file's chain lies.
struct WalkRequest<'a> {
    /// What the query decided: the path the walk follows, how it compares, and
    /// the repository the audited version was read in.
    query: &'a AuditQuery<'a>,
    /// Revisions of the predecessor chain to compare, and `None` for all of them.
    max_revisions: Option<u32>,
    /// How far the walk can read where it starts, which decides how a version it
    /// could not read is named. The walk carries its own copy forward, because
    /// crossing a link changes it.
    reach: Reachability,
}

/// What the query itself decided.
///
/// The repository is the one the audited version was read in, which a path
/// resolving into a linked repository makes differ from the repository the audit
/// was run in.
struct AuditQuery<'a> {
    repository: RepositoryId,
    path: &'a RelativePath,
    whitespace: Whitespace,
}

/// Consecutive lines of the audited file claimed with one outcome in one step.
///
/// Each outcome is claimed in one step only, so no later step can extend a
/// range, and a range is final as soon as its step ends.
#[lore_macro::test_pub]
#[derive(Clone, Copy, PartialEq, Debug)]
struct AuditRange {
    /// First line of the range, 1-based.
    first: u32,
    /// Last line of the range, 1-based and inclusive.
    last: u32,
    outcome: LineOutcome,
}

/// Attribute every line of a file to the revision that last changed it.
///
/// Reads the file at `options.revision`, or at the revision the instance is
/// synced to when the caller names none, and emits one
/// [`event::LoreEvent::FileAudit`] per range of consecutive lines sharing an
/// attribution.
///
/// Each range is sent as soon as the step that resolves it ends, rather than
/// once the walk ends. The walk resolves the most recently changed lines first,
/// so ranges arrive in that order and not in line order. Each range is complete
/// and never needs joining with another.
pub async fn audit(
    repository: Arc<RepositoryContext>,
    path: String,
    options: AuditOptions,
) -> Result<(), AuditError> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, &path)
        .forward::<AuditError>("resolving user path")?;

    let revision = find_audit_revision(repository.clone(), &options).await?;
    let state = state::State::deserialize(repository.clone(), revision)
        .await
        .forward::<AuditError>("Failed to deserialize state")?;

    // A path the revision does not hold is a not-found path, whether it was
    // deleted or never added, and no walk runs for it.
    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_matched_err("resolving the audited path", |m| match m {
            MatchedStateErrors::NodeNotFound(traced) => AuditError::FileNotFound(
                FileNotFound {
                    resource: path.clone(),
                }
                .chain_err(traced, "the revision does not hold the audited path"),
            ),
            other => other.forward::<AuditError>("resolving the audited path"),
        })?;
    if !node_link.is_valid() {
        return Err(FileNotFound { resource: path }.into());
    }

    // The working tree is the instance's own, whatever repository the path
    // resolves into, so it is read before the walk moves into a link.
    let working = read_working_text(&repository, &relative_path, &options).await?;

    // A path resolving into a linked repository carries its own node ids and its
    // own chain, so the walk continues against that repository's revision. The
    // link is entered through the resolve the read path already uses, which
    // leaves a path in the audit's own repository where it stands.
    let linked = node_link.crosses_link(&repository, &state);
    let (repository, state) = node_link
        .resolve(repository, state)
        .await
        .forward::<AuditError>("Failed to enter the repository the path resolves into")?;

    let committed =
        read_audited_content(repository.clone(), &state, node_link.node, &relative_path)
            .await?
            .into_text();

    let query = AuditQuery {
        repository: repository.id,
        path: &relative_path,
        whitespace: options.whitespace,
    };

    let (unattributed, uncommitted) =
        initial_lines(&committed, working.as_deref(), options.whitespace);
    // The working tree's lines need no walk, so they are sent before the first
    // step reads anything.
    send_ranges(&query, &uncommitted);

    walk_predecessors(
        repository.clone(),
        state,
        node_link.node,
        unattributed,
        committed,
        &WalkRequest {
            query: &query,
            max_revisions: options.max_revisions,
            reach: Reachability {
                linked,
                offline: reads_local_store_only(&repository).await,
            },
        },
    )
    .await
}

/// Revision the audit reads the file at: the named revision, the named branch's
/// latest, or the revision the instance is synced to when the caller names
/// neither.
async fn find_audit_revision(
    repository: Arc<RepositoryContext>,
    options: &AuditOptions,
) -> Result<Hash, AuditError> {
    if let Some(revision) = options.revision.as_ref() {
        return history::resolve_revision(repository, revision.as_str())
            .await
            .forward::<AuditError>("Failed to resolve the audited revision");
    }

    let (revision, _branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<AuditError>("Failed to deserialize current revision anchor")?;
    Ok(revision)
}

/// Read the audited file at the revision it was resolved at, refusing binary
/// content.
///
/// Attribution compares versions line by line, which binary content has no
/// answer for, so the query reports an error rather than nonsense ranges. A
/// version further back that cannot be compared is not an error: it ends the
/// walk and leaves the lines already resolved standing.
async fn read_audited_content(
    repository: Arc<RepositoryContext>,
    state: &Arc<State>,
    node: NodeID,
    relative_path: &RelativePath,
) -> Result<DiffContent, AuditError> {
    let revision = state.revision();
    let content = read_node_content(repository, state, node, relative_path)
        .await
        .forward::<AuditError>("Failed reading the audited file")?;

    if content.is_binary() {
        return Err(NotSupported {
            operation: format!("audit of binary file {relative_path} at revision {revision}"),
        }
        .into());
    }

    Ok(content)
}

/// The audited file as the working tree holds it, and `None` where the audit
/// accounts for no working copy.
///
/// The file on disk answers for the revision the instance is synced to and for
/// no other, so an audit of a named revision or branch reads history alone. A
/// path the working tree holds no file at has nothing to compare either.
async fn read_working_text(
    repository: &Arc<RepositoryContext>,
    relative_path: &RelativePath,
    options: &AuditOptions,
) -> Result<Option<String>, AuditError> {
    if options.revision.is_some() {
        return Ok(None);
    }

    let content = read_working_content(repository.clone(), relative_path)
        .await
        .forward::<AuditError>("Failed reading the audited file from the working tree")?;

    match content {
        Some(content) if content.is_binary() => Err(NotSupported {
            operation: format!("audit of binary file {relative_path} in the working tree"),
        }
        .into()),
        Some(content) => Ok(Some(content.into_text())),
        None => Ok(None),
    }
}

/// The lines the walk must resolve, and the ranges of lines the working tree
/// changed.
///
/// With no working copy to account for, every line of the revision is the
/// walk's, numbered as the revision holds them. With one, the numbering is the
/// file on disk's, so an editor can align it: a line differing on disk belongs
/// to no revision and leaves the walk, and every other line carries the number
/// it holds in the revision for the walk to compare it at.
#[lore_macro::test_pub]
fn initial_lines(
    committed: &str,
    working: Option<&str>,
    whitespace: Whitespace,
) -> (Vec<UnattributedLine>, Vec<AuditRange>) {
    let Some(working) = working.filter(|working| *working != committed) else {
        let lines = (1..=line_count(committed))
            .map(|line| UnattributedLine {
                audited: line,
                compared: line,
            })
            .collect();
        return (lines, Vec::new());
    };

    let origins = unchanged_line_origins(committed, working, whitespace);
    let mut unattributed = Vec::with_capacity(origins.len());
    let mut uncommitted = Vec::new();

    for (index, origin) in origins.into_iter().enumerate() {
        let audited = index as u32 + 1;
        match origin {
            Some(compared) => unattributed.push(UnattributedLine { audited, compared }),
            None => push_line(
                &mut uncommitted,
                audited,
                LineOutcome::Other(LoreFileAuditOutcome::Uncommitted),
            ),
        }
    }

    (unattributed, uncommitted)
}

/// Walk the file's predecessor chain, claiming each line for the revision that
/// last changed it.
///
/// Each step compares one version against the version before it: the lines the
/// comparison reports as changed were written by the revision being compared,
/// and the rest carry to the predecessor at the line they sit on there. The walk
/// ends when no line is left unattributed, when the chain reaches the revision
/// that added the file, or when the budget runs out.
///
/// The walk starts at the revision the file was read at, which need not be a
/// revision that changed it. Where it is not, the first comparison finds nothing
/// changed and carries every line onto the revision that did.
///
/// Each step's ranges are sent as soon as the step ends, rather than when the
/// walk finishes, so the most recently changed lines arrive first. A consumer
/// that has what it needs may stop reading; nothing here depends on it reading
/// to the end.
async fn walk_predecessors(
    mut repository: Arc<RepositoryContext>,
    mut state: Arc<State>,
    mut node: NodeID,
    mut unattributed: Vec<UnattributedLine>,
    text: String,
    request: &WalkRequest<'_>,
) -> Result<(), AuditError> {
    let relative_path = request.query.path;
    let whitespace = request.query.whitespace;

    let mut side = ComparedText::new(text, whitespace);
    let mut visited = 0;
    // `linked` is the one part of the walk's reach that changes as it runs: a
    // chain crossing into a linked repository stays in it, and a version out of
    // reach from there is the link being out of reach.
    let mut reach = request.reach;

    // An error ends the walk, but the lines it resolved before the error are
    // answers it verified. Every line is reported before the error is returned.
    let walked: Result<(), AuditError> = async {
        while !unattributed.is_empty() {
            if request
                .max_revisions
                .is_some_and(|max_revisions| visited >= max_revisions)
            {
                let ranges = stop_walk(
                    &mut unattributed,
                    LoreFileAuditOutcome::BudgetExhausted,
                    relative_path,
                );
                send_ranges(request.query, &ranges);
                break;
            }
            visited += 1;

            let current = RevisionSource {
                repository: repository.id,
                revision: state.revision(),
            };

            let previous =
                match read_previous_version(&repository, &state, node, relative_path, &mut reach)
                    .await?
                {
                    Ok(Some(previous)) => previous,
                    Ok(None) => {
                        // No version stands before this one, so every line left is its own.
                        let ranges =
                            claim_remaining(&mut unattributed, LineOutcome::Revision(current));
                        send_ranges(request.query, &ranges);
                        break;
                    }
                    Err(reason) => {
                        let ranges = stop_walk(&mut unattributed, reason, relative_path);
                        send_ranges(request.query, &ranges);
                        break;
                    }
                };

            let previous_side = ComparedText::new(previous.content.into_text(), whitespace);
            let origins = compared_line_origins(&previous_side.text, &side.text, side.line_count);
            let ranges = attribute_step(&mut unattributed, &origins, current);
            send_ranges(request.query, &ranges);

            let predecessor = previous.predecessor;
            if predecessor.action == FileAction::Add as u16 {
                // The previous version added the file, so nothing precedes it and
                // every line it still holds is its own.
                let ranges = claim_remaining(
                    &mut unattributed,
                    LineOutcome::Revision(RevisionSource {
                        repository: previous.repository.id,
                        revision: predecessor.revision,
                    }),
                );
                send_ranges(request.query, &ranges);
                break;
            }

            side = previous_side;
            repository = previous.repository;
            state = previous.state;
            node = predecessor.node;
        }
        Ok(())
    }
    .await;
    if let Err(error) = &walked {
        lore_debug!("Audit of {relative_path} ended with an error: {error}");
        let ranges = stop_walk(
            &mut unattributed,
            LoreFileAuditOutcome::Unresolved,
            relative_path,
        );
        send_ranges(request.query, &ranges);
    }

    // A line left unattributed here would be reported by no range at all.
    debug_assert!(
        unattributed.is_empty(),
        "audit of {relative_path} reported no range for {} lines",
        unattributed.len()
    );

    walked
}

/// Sends one [`event::LoreEvent::FileAudit`] for each range.
fn send_ranges(query: &AuditQuery<'_>, ranges: &[AuditRange]) {
    for range in ranges {
        event::LoreEvent::FileAudit(audit_event_data(query.repository, *range)).send();
    }
}

/// The repository a predecessor's chain continues in, and `None` where that is
/// the repository the walk is already following.
///
/// A file metadata node names a repository only where the version before it came
/// from a linked one, through a recorded copy or move, and a zero names none.
#[lore_macro::test_pub]
fn crossed_repository(named: RepositoryId, current: RepositoryId) -> Option<RepositoryId> {
    (!named.is_zero() && named != current).then_some(named)
}

/// Ends the walk at a step it cannot see through: every line still unattributed
/// carries the reason. Returns the ranges of those lines.
///
/// The lines already resolved were resolved against versions the walk did read
/// and are unaffected, which is the whole point of stopping rather than failing.
#[lore_macro::test_pub]
fn stop_walk(
    unattributed: &mut Vec<UnattributedLine>,
    reason: LoreFileAuditOutcome,
    relative_path: &RelativePath,
) -> Vec<AuditRange> {
    lore_debug!(
        "Audit of {relative_path} stopped as {} with {} lines left",
        reason.as_string(),
        unattributed.len()
    );
    claim_remaining(unattributed, LineOutcome::Other(reason))
}

/// The value a read produced, or the reason its failure leaves behind, and the
/// failure itself where the design accounts for none.
///
/// A gap the design named is an answer the walk reports; anything else is a
/// defect, and reporting a defect as an unresolved range would hide it.
#[lore_macro::test_pub]
fn gap<T>(
    result: Result<T, AuditError>,
    reach: Reachability,
) -> Result<Result<T, LoreFileAuditOutcome>, AuditError> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(error) => match unresolved_reason(&error, reach) {
            Some(reason) => Ok(Err(reason)),
            None => Err(error),
        },
    }
}

/// The version of the file standing before the one the walk is comparing, and
/// where it was read.
struct PreviousVersion {
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    predecessor: FilePredecessor,
    content: DiffContent,
}

/// Read the version standing one step back in the file's chain, and `None` where
/// the chain ends at the version given. A version the walk cannot compare yields
/// the reason it cannot rather than a failure.
///
/// A predecessor naming another repository is a link crossed in the middle of the
/// chain: the version before this one lives in the linked repository the file was
/// brought in from, so the rest of the walk reads that repository's states and
/// nodes.
async fn read_previous_version(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    node: NodeID,
    relative_path: &RelativePath,
    reach: &mut Reachability,
) -> Result<Result<Option<PreviousVersion>, LoreFileAuditOutcome>, AuditError> {
    let read: Result<Option<PreviousVersion>, AuditError> = async {
        let Some(predecessor) = file_predecessor(repository.clone(), state, node).await? else {
            return Ok(None);
        };

        let crossed = crossed_repository(predecessor.repository, repository.id);
        let repository = match crossed {
            Some(crossed) => repository.to_link_context(crossed).await,
            None => repository.clone(),
        };
        reach.linked |= crossed.is_some();

        let state = state::State::deserialize(repository.clone(), predecessor.revision)
            .await
            .forward::<AuditError>("Failed to deserialize state")?;
        let content =
            read_node_content(repository.clone(), &state, predecessor.node, relative_path)
                .await
                .forward::<AuditError>("Failed reading a previous version of the file")?;

        Ok(Some(PreviousVersion {
            repository,
            state,
            predecessor,
            content,
        }))
    }
    .await;

    Ok(gap(read, *reach)?.and_then(|previous| match previous {
        Some(previous) if previous.content.is_binary() => {
            Err(LoreFileAuditOutcome::BinaryInHistory)
        }
        previous => Ok(previous),
    }))
}

/// How far the walk can read, which decides how a version it could not read is
/// named.
#[lore_macro::test_pub]
#[derive(Clone, Copy, PartialEq, Debug)]
struct Reachability {
    /// Whether the chain lies in a linked repository, so a version out of reach
    /// is that repository being out of reach.
    linked: bool,
    /// Whether the walk reads the local store alone.
    offline: bool,
}

/// Whether the walk reads the local store alone, which is the case under
/// `--offline` and for an instance configured with no remote.
///
/// Read once, before the walk, so that one walk names every gap the same way
/// whatever the connection does while it runs.
async fn reads_local_store_only(repository: &Arc<RepositoryContext>) -> bool {
    matches!(repository.remote_status().await, RemoteStatus::Offline)
}

/// The reason a version could not be read, and `None` where the failure is not
/// a gap the design accounts for, which ends the audit with its error.
///
/// Content the read found in neither store means one of two things, and which one
/// depends on whether there was a remote to read through. There it means the
/// content was removed, which is what obliterating a payload leaves behind: the
/// address survives the payload. Reading the local store alone it means only that
/// this instance does not hold it, which is the ordinary state of a sparse
/// instance, so reporting a removal would name one that never happened.
///
/// An instance with no remote reads its local store alone by configuration, so
/// content it lacks is unavailable. A lost connection is different: it is a
/// failure of the environment rather than an answer about the file, so it ends
/// the audit with its error, and the lines the walk did not reach are reported
/// as unresolved. Any gap inside a linked repository is reported against the
/// link, the repository the chain continues in being the one out of reach,
/// except a removal, which is a removal wherever it happened.
#[lore_macro::test_pub]
fn unresolved_reason(error: &AuditError, reach: Reachability) -> Option<LoreFileAuditOutcome> {
    let out_of_reach = if reach.linked {
        LoreFileAuditOutcome::LinkUnavailable
    } else {
        LoreFileAuditOutcome::ContentUnavailable
    };

    match error {
        AuditError::AddressNotFound(_)
        | AuditError::PayloadNotFound(_)
        | AuditError::NotFound(_) => Some(if reach.offline {
            out_of_reach
        } else {
            LoreFileAuditOutcome::Obliterated
        }),
        AuditError::NoRemote(_) => Some(out_of_reach),
        AuditError::NotAuthenticated(_) | AuditError::NotAuthorized(_) => reach
            .linked
            .then_some(LoreFileAuditOutcome::LinkUnavailable),
        _ => None,
    }
}

/// The version of the file standing one step back in its own chain.
#[derive(Clone, Copy, PartialEq, Debug)]
struct FilePredecessor {
    /// Action the previous revision applied to the file.
    action: u16,
    /// Revision that version stands at.
    revision: Hash,
    /// Node the file has in that revision.
    node: NodeID,
    /// Repository that revision belongs to, and zero where the node names none,
    /// which means the repository the walk is already in.
    repository: RepositoryId,
}

/// The file's predecessor in its own chain, and `None` where the chain ends at
/// the version given.
///
/// Slot 1 holds the second predecessor at a merge and the walk never follows it,
/// which keeps the walk linear. A line a merge changed therefore belongs to the
/// merge revision.
///
/// The predecessor is named as a node rather than as a path, so a recorded
/// rename is followed by reading it: the path the file had before the move is
/// what `from_path` reports a move under, and the node the audit reads is the
/// one at that path. A move recorded as a deletion and an addition is two
/// unrelated files, and the added one's chain ends where it was added.
async fn file_predecessor(
    repository: Arc<RepositoryContext>,
    state: &Arc<State>,
    node: NodeID,
) -> Result<Option<FilePredecessor>, AuditError> {
    let metadata_node = node::node_to_file_metadata(node);
    let block_index = NodeFileMetadataBlock::index(metadata_node);
    let node_index = NodeFileMetadata::index(metadata_node);

    let block = state
        .block_file_metadata(repository, block_index)
        .await
        .forward::<AuditError>("Failed to deserialize metadata block")?;

    let reader = block.read();
    let metadata = reader.node(node_index);
    if metadata.revision[0].is_zero() {
        return Ok(None);
    }

    Ok(Some(FilePredecessor {
        action: metadata.action[0],
        revision: metadata.revision[0],
        node: metadata.node[0],
        repository: metadata.repository,
    }))
}

/// One range as it is reported: the lines it covers, what they resolved to, and
/// the revision and repository that answered it.
///
/// A range a revision holds names the repository that revision came from, which
/// a walk crossing a link makes differ between ranges. A range no revision holds
/// names `audited_repository`, the repository the audited version was read in,
/// the file being the one the caller asked about.
#[lore_macro::test_pub]
fn audit_event_data(audited_repository: RepositoryId, range: AuditRange) -> LoreFileAuditEventData {
    let (outcome, source) = match range.outcome {
        LineOutcome::Revision(source) => (LoreFileAuditOutcome::Attributed, Some(source)),
        LineOutcome::Other(outcome) => (outcome, None),
    };

    LoreFileAuditEventData {
        repository: source.map_or(audited_repository, |source| source.repository),
        revision: source.map_or_else(Hash::default, |source| source.revision),
        line_first: range.first,
        line_last: range.last,
        outcome,
    }
}

/// Lines in a file's text. A last line with no terminator is an ordinary line:
/// adding a terminator changes it, and that belongs to the revision that added
/// the terminator.
#[lore_macro::test_pub]
fn line_count(text: &str) -> u32 {
    text.split_inclusive('\n').count() as u32
}

/// For each 1-based line of `new`, the 1-based line of `old` it is unchanged
/// from, or `None` where the comparison reports the line as changed.
#[lore_macro::test_pub]
fn unchanged_line_origins(old: &str, new: &str, whitespace: Whitespace) -> Vec<Option<u32>> {
    compared_line_origins(
        &normalise(old, whitespace),
        &normalise(new, whitespace),
        line_count(new),
    )
}

/// [`unchanged_line_origins`] for two sides already normalised, `compared_new`
/// holding `new_line_count` lines.
///
/// A normalised side holds as many lines as the side it was made from, so the
/// numbers this reports are the numbers of the lines as they were written.
fn compared_line_origins(
    compared_old: &str,
    compared_new: &str,
    new_line_count: u32,
) -> Vec<Option<u32>> {
    // Context lines around a hunk say nothing the untouched lines between hunks
    // do not, so the comparison asks for none of them.
    let patch = diffy::DiffOptions::new()
        .set_context_len(0)
        .create_patch(compared_old, compared_new);

    let mut origins = LineOrigins::new(new_line_count);
    walk_patch(&patch, |entry| origins.accept(entry));
    origins.finish()
}

fn normalise(text: &str, whitespace: Whitespace) -> Cow<'_, str> {
    normalise_text(
        text,
        whitespace.ignore_space_at_eol,
        whitespace.ignore_space_change,
    )
}

/// One version's text as the walk compares it, normalised under the audit's
/// whitespace rules, and the number of lines it holds.
struct ComparedText {
    text: String,
    line_count: u32,
}

impl ComparedText {
    fn new(text: String, whitespace: Whitespace) -> Self {
        let text = match normalise(&text, whitespace) {
            Cow::Owned(normalised) => normalised,
            Cow::Borrowed(_) => text,
        };
        let line_count = line_count(&text);
        ComparedText { text, line_count }
    }
}

/// Builds the old-side origin of each new line as a patch walk reports its
/// lines.
///
/// A hunk names the lines it covers. The lines between hunks are unchanged and
/// keep their order on both sides, so they are carried across as the walk passes
/// them rather than being reported.
struct LineOrigins {
    origins: Vec<Option<u32>>,
    /// 1-based new line the walk has not yet recorded.
    next_new: u32,
    /// 1-based old line the walk has not yet accounted for.
    next_old: u32,
}

impl LineOrigins {
    fn new(new_line_count: u32) -> Self {
        LineOrigins {
            origins: vec![None; new_line_count as usize],
            next_new: 1,
            next_old: 1,
        }
    }

    fn accept(&mut self, entry: PatchEntry) {
        match entry {
            PatchEntry::HunkStart { .. } => {}
            PatchEntry::Context { old, new } => {
                self.carry_to_new(new as u32);
                self.record(new as u32, Some(old as u32));
                self.next_old = old as u32 + 1;
            }
            PatchEntry::Delete { old } => {
                self.carry_to_old(old as u32);
                self.next_old = old as u32 + 1;
            }
            PatchEntry::Insert { new } => {
                self.carry_to_new(new as u32);
                self.record(new as u32, None);
            }
        }
    }

    fn carry_to_new(&mut self, new_line: u32) {
        while self.next_new < new_line {
            self.carry_one();
        }
    }

    /// Carries the untouched lines before `old_line`. A deleted line names the
    /// old side alone, and the lines before it stand on both.
    fn carry_to_old(&mut self, old_line: u32) {
        while self.next_old < old_line {
            self.carry_one();
        }
    }

    fn carry_one(&mut self) {
        let old = self.next_old;
        self.record(self.next_new, Some(old));
        self.next_old = old + 1;
    }

    fn record(&mut self, new_line: u32, old_line: Option<u32>) {
        if let Some(slot) = new_line
            .checked_sub(1)
            .and_then(|index| self.origins.get_mut(index as usize))
        {
            *slot = old_line;
        }
        self.next_new = new_line + 1;
    }

    /// The origins, with every line past the last hunk carried across.
    fn finish(mut self) -> Vec<Option<u32>> {
        self.carry_to_new(self.origins.len() as u32 + 1);
        self.origins
    }
}

/// Claims for `source` every unattributed line that revision changed, and
/// carries the rest onto the line they sit on in the version before it. Returns
/// the ranges of the lines claimed, and none where the revision claimed none.
#[lore_macro::test_pub]
fn attribute_step(
    unattributed: &mut Vec<UnattributedLine>,
    origins: &[Option<u32>],
    source: RevisionSource,
) -> Vec<AuditRange> {
    let mut claimed = Vec::new();

    unattributed.retain_mut(|line| {
        let origin = (line.compared as usize)
            .checked_sub(1)
            .and_then(|index| origins.get(index))
            .copied()
            .flatten();

        if let Some(previous) = origin {
            line.compared = previous;
            true
        } else {
            push_line(&mut claimed, line.audited, LineOutcome::Revision(source));
            false
        }
    });

    claimed
}

/// Claims every line still unattributed with one outcome, emptying the set.
/// Returns the ranges of the lines claimed.
#[lore_macro::test_pub]
fn claim_remaining(
    unattributed: &mut Vec<UnattributedLine>,
    outcome: LineOutcome,
) -> Vec<AuditRange> {
    let mut claimed = Vec::new();
    for line in unattributed.drain(..) {
        push_line(&mut claimed, line.audited, outcome);
    }
    claimed
}

/// Adds one line to `ranges`, extending the last range where the line follows
/// it with the same outcome. Lines must come in ascending order.
#[lore_macro::test_pub]
fn push_line(ranges: &mut Vec<AuditRange>, audited: u32, outcome: LineOutcome) {
    match ranges.last_mut() {
        Some(range) if range.last + 1 == audited && range.outcome == outcome => {
            range.last = audited;
        }
        _ => ranges.push(AuditRange {
            first: audited,
            last: audited,
            outcome,
        }),
    }
}
