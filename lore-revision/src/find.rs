// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_storage::options::ReadOptions;
use lore_transport::RevisionListIdentifier;
use lore_transport::RevisionListStart;
use serde::Serialize;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::task::AbortOnDropHandle;
use zerocopy::FromBytes;

use crate::branch;
use crate::errors::Cancelled;
use crate::errors::SlowDown;
use crate::errors::absent_unless;
use crate::event;
use crate::immutable;
use crate::immutable::ImmutableError;
use crate::immutable::ReadFromImmutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::lore_trace;
use crate::metadata::Metadata;
use crate::repository::RepositoryContext;
use crate::runtime::CancellationCheck;
use crate::state::State;
use crate::state::StateData;

pub const DEFAULT_SEARCH_LIMIT: usize = 1000;

#[error_set]
pub enum FindError {
    Cancelled,
    SlowDown,
}

impl event::EventError for FindError {
    fn translated(&self) -> LoreError {
        match self {
            FindError::SlowDown(_) => LoreError::SlowDown,
            FindError::Cancelled(_) | FindError::Internal(_) => LoreError::Internal,
        }
    }
}

pub enum FindMatchResult {
    Match,
    Continue,
    Abort,
}

/// Walk a branch's history from `revision` until `matcher` accepts a revision.
///
/// A zero `revision` starts from the branch latest, preferring the remote one so
/// a search covers revisions the local store has not seen. An offline or
/// local-only operation takes the local latest alone, since waiting on the
/// connect is the whole cost such an operation asked to avoid.
pub async fn find_revision<F>(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    revision: Hash,
    with_metadata: bool,
    search_limit: Option<usize>,
    mut matcher: F,
) -> Result<Hash, FindError>
where
    F: FnMut(Arc<State>, Option<Metadata>) -> FindMatchResult,
{
    let mut revision = revision;
    if revision.is_zero() {
        if !execution_context().globals().offline_or_local()
            && let Ok(remote) = repository.remote().await
        {
            // no propagation here: a throttled or unreachable remote is what
            // the local fallback below exists for, so it stays a zero latest
            // rather than a failure.
            revision = branch::load_remote_latest(remote, repository.id, branch)
                .await
                .unwrap_or_default();
        }
        if revision.is_zero() {
            revision = absent_unless::<_, _, FindError>(
                branch::load_latest(repository.clone(), branch).await,
                branch::BranchError::is_slow_down,
                "loading branch latest",
            )?
            .unwrap_or_default();
        }
    }
    lore_debug!("Find start revision {}", revision);
    let mut search_count = 0;
    let cancellation = CancellationCheck::current();
    while !revision.is_zero() {
        cancellation.check()?;
        search_count += 1;
        if let Some(search_limit) = search_limit
            && search_count > search_limit
        {
            return Err(FindError::internal("search limit reached"));
        }

        // Check if we have the revision cached locally
        if !stored_local(&repository, revision).await {
            // Batch load a chunk of history
            // TODO(mjansson): Hide batch cache latency by issuing requests while iterating
            //                 previous batch of history, trying to stay ahead of the load curve
            let _ = batch_load_history(repository.clone(), revision).await;
        }

        let state = State::deserialize(repository.clone(), revision)
            .await
            .forward_any::<FindError>("deserializing state")?;

        let metadata = if with_metadata {
            Some(
                Metadata::deserialize(repository.clone(), state.metadata_hash())
                    .await
                    .forward_any::<FindError>("deserializing metadata")?,
            )
        } else {
            None
        };

        match matcher(state.clone(), metadata) {
            FindMatchResult::Continue => {
                lore_trace!(
                    "Revision {} does not match, continue to parent revision {}",
                    revision,
                    state.parent_self()
                );
            }
            FindMatchResult::Match => {
                lore_debug!(
                    "Found matching revision {} after {} iterations",
                    revision,
                    search_count
                );
                return Ok(revision);
            }
            FindMatchResult::Abort => {
                lore_debug!("Revision {} does not match, abort search", revision,);
                break;
            }
        }

        revision = state.parent_self();
    }

    lore_debug!(
        "Found NO matching revision after {} iterations",
        search_count
    );

    // No emit, let caller decide if it is an error or not
    Err(FindError::internal("no revision found"))
}

/// Whether `ancestor`, numbered `ancestor_number`, is on the history of `revision`: `revision`
/// itself, a revision its first parents lead back to, or the other parent of a merge on that line.
/// A zero `ancestor` precedes every revision.
///
/// The walk stops at the first revision numbered at or below `ancestor_number`, so it reads at most
/// one revision for each revision number between the two, and after `search_limit` revisions where
/// one is given. It fails where it cannot decide: where a revision cannot be read, `search_limit`
/// is spent, or the walk ends before passing `ancestor_number`.
///
/// The branch module's history-line search walks a line too, but over batches of revision numbers
/// and hashes, which carry no other parent. This walk reads each revision's state for it.
pub async fn try_is_on_history(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    ancestor: Hash,
    ancestor_number: u64,
    search_limit: Option<usize>,
) -> Result<bool, FindError> {
    if ancestor.is_zero() {
        return Ok(true);
    }
    if revision.is_zero() {
        return Ok(false);
    }
    let mut passed = false;
    // The branch names where a zero revision starts, so a walk from `revision` never reads it.
    let found = find_revision(
        repository,
        BranchId::default(),
        revision,
        false,
        search_limit,
        |state, _metadata| {
            if state.revision() == ancestor || state.parent_other() == ancestor {
                FindMatchResult::Match
            } else if state.revision_number() <= ancestor_number {
                passed = true;
                FindMatchResult::Abort
            } else {
                FindMatchResult::Continue
            }
        },
    )
    .await;
    match found {
        Ok(_) => Ok(true),
        Err(_) if passed => Ok(false),
        Err(err) => Err(err),
    }
}

/// Whether `ancestor` is on the history of `revision`, as [`try_is_on_history`] answers it, walked
/// at most [`branch::MAX_DIVERGENT_HISTORY_LENGTH`] revisions. Reads the revision number of
/// `ancestor` itself.
pub async fn try_is_on_recent_history(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    ancestor: Hash,
) -> Result<bool, FindError> {
    let ancestor_number = if ancestor.is_zero() {
        0
    } else {
        State::deserialize(repository.clone(), ancestor)
            .await
            .forward_any::<FindError>("reading the ancestor's revision number")?
            .revision_number()
    };
    try_is_on_history(
        repository.clone(),
        revision,
        ancestor,
        ancestor_number,
        Some(branch::MAX_DIVERGENT_HISTORY_LENGTH),
    )
    .await
}

/// Where a revision sits against a later revision, `latest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryPosition {
    /// On the history of `latest`.
    OnHistory,
    /// Holds `latest` on its own history.
    Ahead,
    /// Neither holds the other.
    Diverged,
}

/// Where `revision` sits against `latest`, each way walked as [`try_is_on_recent_history`] walks
/// it. It fails where either walk cannot decide.
pub async fn try_history_position(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    latest: Hash,
) -> Result<HistoryPosition, FindError> {
    if try_is_on_recent_history(repository, latest, revision).await? {
        return Ok(HistoryPosition::OnHistory);
    }
    if try_is_on_recent_history(repository, revision, latest).await? {
        return Ok(HistoryPosition::Ahead);
    }
    Ok(HistoryPosition::Diverged)
}

/// Find revision in the current branch history which has a matching key-value pair in metadata
pub async fn revision_by_metadata(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    revision: Hash,
    key: &str,
    value: Option<&str>,
) -> Result<Hash, FindError> {
    lore_debug!(
        "Find revision where {} = {}",
        key,
        value.unwrap_or("<None>"),
    );
    find_revision(
        repository.clone(),
        branch,
        revision,
        true, /* With metadata */
        None,
        |state, metadata| {
            if let Some(metadata) = metadata
                && let Ok(found_value) = metadata.get_string(key)
            {
                if let Some(value) = value {
                    if found_value == value {
                        lore_debug!(
                            "Metadata match {} = {} for revision {}",
                            key,
                            value,
                            state.revision()
                        );
                        return FindMatchResult::Match;
                    }
                    return FindMatchResult::Continue;
                }
                lore_debug!(
                    "Metadata match key = {} for revision {}",
                    key,
                    state.revision()
                );
                return FindMatchResult::Match;
            }
            FindMatchResult::Continue
        },
    )
    .await
}

/// Find revision by number in the current branch
pub async fn revision_by_number(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    revision_start: Hash,
    revision_number: u64,
) -> Result<Hash, FindError> {
    lore_debug!("Find revision {}", revision_number);
    find_revision(
        repository.clone(),
        branch,
        revision_start,
        false, /* No metadata */
        None,
        |state, _metadata| {
            let state_revision_number = state.revision_number();
            match state_revision_number.cmp(&revision_number) {
                Ordering::Equal => {
                    lore_debug!(
                        "Revision number {} match search number {} for revision {}",
                        state_revision_number,
                        revision_number,
                        state.revision()
                    );
                    FindMatchResult::Match
                }
                Ordering::Less => {
                    lore_debug!(
                        "Revision number {} less than search number {} for revision {}",
                        state_revision_number,
                        revision_number,
                        state.revision()
                    );
                    FindMatchResult::Abort
                }
                Ordering::Greater => {
                    lore_trace!(
                        "Revision number {} greater than search number {} for revision {}",
                        state_revision_number,
                        revision_number,
                        state.revision()
                    );
                    FindMatchResult::Continue
                }
            }
        },
    )
    .await
}

/// What a walk of a revision's line in the local store finds for a revision number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberWalk {
    /// The revision with the number.
    Found {
        /// The revision.
        revision: Hash,
        /// The hash of the revision's metadata, as its state records it.
        metadata: Hash,
    },
    /// No revision on the line has the number: the walk reached a revision numbered below it, or
    /// the end of the line.
    Absent,
    /// The read of this revision's state answered `AddressNotFound` before the walk could tell
    /// whether the line holds the number: the local store lacks the state or, on a context without
    /// strict-local reads, the read failed or found its payload corrupt. With the remote off, a
    /// context that reads from the remote alone ([`RepositoryContext::reading_remote_only`]) reads
    /// nothing, so the walk answers this for any non-zero start.
    Missing(Hash),
}

/// Find the revision numbered `number` on the line from `revision` back through self parents, in
/// the local store alone. The walk stops at the first revision numbered at or below `number`, as
/// [`revision_by_number`] does, and where the line ends or leaves the store. Each step reads one
/// [`pointed_state`], with the options `repository` gives its reads and the remote off. A state
/// the store lacks is [`NumberWalk::Missing`]; any other read error fails the walk, a failed or
/// corrupt read among them on a strict-local context, and so does a blob that is not a revision
/// state.
///
/// [`revision_by_number`] costs more per step: it probes the store, builds a [`State`], and fetches
/// and caches from the remote what the store lacks.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub fn revision_by_number_local(
    repository: &Arc<RepositoryContext>,
    mut revision: Hash,
    number: u64,
) -> impl Future<Output = Result<NumberWalk, FindError>> + '_ {
    async move {
        let options = immutable::read_options_from_repository(repository).no_remote();
        while !revision.is_zero() {
            let (revision_number, parent, metadata) =
                match pointed_state(repository, revision, options).await {
                    Ok(Some(state)) => (
                        state.revision_number,
                        state.parent[0],
                        state.metadata_hash(),
                    ),
                    Ok(None) => return Ok(NumberWalk::Missing(revision)),
                    Err(err) => {
                        return Err(err).forward_any_with::<FindError, _>(|| {
                            format!("reading the state of revision {revision}")
                        });
                    }
                };
            match revision_number.cmp(&number) {
                Ordering::Equal => return Ok(NumberWalk::Found { revision, metadata }),
                Ordering::Less => break,
                Ordering::Greater => revision = parent,
            }
        }
        Ok(NumberWalk::Absent)
    }
}

/// One step of a walk along a line in the local store: the number and first parent of `revision`,
/// from its [`pointed_state`] read with `options` and the remote off. Its error does not name
/// `revision`.
#[inline]
pub(crate) fn local_line_step(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    options: ReadOptions,
) -> impl Future<Output = Result<Option<(u64, Hash)>, ImmutableError>> + Send {
    futures::FutureExt::map(
        pointed_state(repository, revision, options.no_remote()),
        |read| read.map(|state| state.map(|state| (state.revision_number, state.parent[0]))),
    )
}

/// Why a read of a revision's state found none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateMiss {
    /// The store doesn't hold a blob under the hash, or the hash is zero.
    Missing,
    /// The blob under the hash is not a revision state: its header lacks the state magic.
    NotAState,
}

/// The state of `revision`, read with `options`, or why the store holds none:
/// [`StateMiss::Missing`] where the store answered `AddressNotFound`, and for a zero `revision`,
/// which reads no store, and [`StateMiss::NotAState`] for a blob whose header lacks the state
/// magic. The format version is not checked, so a state a newer client wrote is still read.
#[inline]
pub fn revision_state(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    options: ReadOptions,
) -> impl Future<Output = Result<Result<StateData, StateMiss>, ImmutableError>> + Send {
    let zero = revision.is_zero();
    let read = StateData::read_from_immutable(
        repository.clone(),
        Address::zero_context_hash(revision),
        options,
    );
    futures::FutureExt::map(read, move |read| match read {
        Ok(state) if state.has_state_magic() => Ok(Ok(state)),
        Ok(_) if zero => Ok(Err(StateMiss::Missing)),
        Ok(_) => Ok(Err(StateMiss::NotAState)),
        Err(ImmutableError::AddressNotFound(_)) => Ok(Err(StateMiss::Missing)),
        Err(err) => Err(err),
    })
}

/// The [`revision_state`] of a revision that a parent link or a branch's latest names, either of
/// which names a revision state: a blob that is not one fails with an internal error, rather than
/// giving its bytes as a state. `None` where the store doesn't hold the state.
#[inline]
pub(crate) fn pointed_state(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    options: ReadOptions,
) -> impl Future<Output = Result<Option<StateData>, ImmutableError>> + Send {
    futures::FutureExt::map(
        revision_state(repository, revision, options),
        |read| match read? {
            Ok(state) => Ok(Some(state)),
            Err(StateMiss::Missing) => Ok(None),
            Err(StateMiss::NotAState) => {
                Err(ImmutableError::internal("the blob is not a revision state"))
            }
        },
    )
}

pub const BATCH_COUNT: usize = 100;

/// The revisions of `batch`, a batch [`batch_load_history`] loaded from `start`, below `start`:
/// without `start`, which the batch lists first. Empty where nothing below `start` was loaded, as
/// at the end of the line.
pub(crate) fn below(batch: &[(u64, Hash)], start: Hash) -> &[(u64, Hash)] {
    match batch.split_first() {
        Some((first, rest)) if first.1 == start => rest,
        _ => batch,
    }
}

/// The line from `revision` back through self parents, newest first, each revision with its
/// number: the revisions the local store holds, up to [`BATCH_COUNT`], each read by a
/// [`local_line_step`], then the remote's page from the revision the local walk stops at, whole.
/// The local walk stops at a revision whose step gives no number and parent: one the store lacks,
/// a failed read, or a blob that is not a revision state. Where the remote cannot list that page,
/// only the revisions the local walk read. The number of a listed revision is read from its state,
/// as [`line_state`] reads it, and the page's states are cached as [`cache_page_states`] describes.
/// Fails where the page does not continue the line, as [`remote_page`] describes.
pub async fn batch_load_history(
    repository: Arc<RepositoryContext>,
    mut revision: Hash,
) -> Result<Vec<(u64, Hash)>, FindError> {
    let mut history = Vec::with_capacity(BATCH_COUNT);

    while !revision.is_zero()
        && history.len() < BATCH_COUNT
        && let Ok(Some((number, parent))) =
            local_line_step(&repository, revision, ReadOptions::default()).await
    {
        history.push((number, revision));
        revision = parent;
    }

    if revision.is_zero() || history.len() >= BATCH_COUNT {
        return Ok(history);
    }
    // Boxed: a remote request, a cold path once a page, whose future would otherwise make every
    // caller's as large as the page's caching.
    Box::pin(remote_page(repository, revision, None, Some(&mut history))).await?;
    Ok(history)
}

/// How many pages a [`LinePager`] keeps requested ahead of its walk, the page the walk is in among
/// them.
#[lore_macro::test_pub]
pub(crate) const PAGES_AHEAD: usize = 4;

/// How many pages of its own, read by hash signature, a walk reads before its [`LinePager`]
/// requests pages ahead of it, so that a walk that ends within a page or two of its start requests
/// none.
const OWN_PAGES_BEFORE_AHEAD: usize = 2;

/// How many walks sharing the permits of [`lines_ahead`] request the pages of their lines ahead at
/// once. The first walks to read [`OWN_PAGES_BEFORE_AHEAD`] pages of their own take the permits.
/// A walk refused one asks again at each next page of its own, and a permit comes free once a walk
/// ends or stops requesting pages ahead.
#[lore_macro::test_pub]
pub(crate) const LINES_AHEAD: usize = 2;

/// The permits that the pagers of one search's walks share, [`LINES_AHEAD`] of them.
#[lore_macro::test_pub]
pub(crate) fn lines_ahead() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(LINES_AHEAD))
}

/// How many revision numbers the remote's page of a branch's history line lists by default,
/// aligned on multiples of it, as [`page_floor`] answers. The remote picks the size of its pages,
/// so this is a guess: where it holds, each page requested ahead takes one request. Where the
/// remote's pages are larger, as Lore Server's are with a larger history step, each request lists
/// the remote's page whole, so a page of `n` revision numbers is listed `n / PAGE_NUMBERS` times.
const PAGE_NUMBERS: u64 = branch::DEFAULT_HISTORY_STEP_SIZE;

/// The lowest revision number of the remote's default page that holds revision number `number`:
/// those pages list the revision numbers above a multiple of [`PAGE_NUMBERS`] up to the next.
fn page_floor(number: u64) -> u64 {
    number.saturating_sub(1) / PAGE_NUMBERS * PAGE_NUMBERS + 1
}

/// Reads the history line a walk follows back through first parents from the remote into the
/// local store a page at a time, so that the walk reads a page per request rather than a revision,
/// and on a long line, requests [`PAGES_AHEAD`] pages at once.
///
/// The walk asks for each revision before it reads it, as [`Self::page_in`] describes. A revision
/// the store lacks is read with its page, by hash signature. Once the walk has read
/// [`OWN_PAGES_BEFORE_AHEAD`] such pages, and the pager holds one of the permits its search shares
/// between its pagers, it also requests the pages below, as [`PagesAhead`] describes. A pager
/// without a permit asks for one again at its walk's next page of its own.
///
/// Dropped, a pager aborts the pages it requested. Every page task it stops holding has ended,
/// and [`Self::finish`] ends the rest.
#[lore_macro::test_pub]
pub(crate) struct LinePager {
    repository: Arc<RepositoryContext>,
    lines_ahead: Arc<Semaphore>,
    /// Whether the remote lists pages: once it lists none, the walk reads each revision on its own.
    paging: bool,
    own_pages: usize,
    ahead: Ahead,
}

/// Whether a [`LinePager`] requests pages ahead of its walk.
enum Ahead {
    /// Not yet: the walk has read too few pages of its own, or the pager has no permit.
    NotYet,
    /// Requesting: the pages requested and not retired.
    On(PagesAhead),
    /// Never again for this walk: the metadata naming the branch of its line could not be read,
    /// the pages requested were ended as [`PagesAhead`] describes, the remote lists no pages, or
    /// the walk has finished.
    Off,
}

/// The pages of a walk's line requested ahead of it, newest first, by revision number on the
/// branch its revisions were created on, as their metadata records it. A revision number at or
/// below that of the branch's branch point names the revision of the branch it was created from,
/// so the branch's revision numbers name every revision of the line. Not every number names one:
/// a merged revision is numbered one above the higher of its parents, so the line skips the
/// numbers between it and a first parent numbered below its other parent. A remote may answer a
/// skipped number with the page holding the line's nearest revision below it, or list no page
/// from it. The first page is requested from the revision number below the
/// walk's last page of its own, and each after it from the highest revision number of the
/// remote's default page below, as [`page_floor`] answers. Each page reads the line down to the
/// next page, however the remote sizes and aligns its pages, as [`page_by_number`] describes.
///
/// A page the walk has read below is retired, and the next requested, so the pages requested run
/// [`PAGES_AHEAD`] ahead of the walk ([`Self::pass`]). A revision the store lacks is waited for in
/// the pages in the order requested, up to a page that lists nothing, which is removed
/// ([`Self::wait_for`]). Where none of them holds it, the walk reads its page by hash signature,
/// which retires the pages it covers. The pages go on where that page's revisions are numbered
/// below the number a page that listed nothing was requested from, so the line skips that number,
/// or where [`Self::held`] holds. Otherwise the pages are ended, as where the remote's line of the
/// branch isn't the walk's, or the remote lists no pages by number.
struct PagesAhead {
    branch: BranchId,
    /// The revision number to request the next page from.
    next: u64,
    /// The pages requested and neither retired nor removed, newest first, each with the revision
    /// number it was requested from.
    pages: VecDeque<(u64, PageAhead)>,
    /// Whether, since the walk last read a page of its own, a page the walk read below listed any
    /// of the line, or a page waited for held the revision the walk lacked.
    held: bool,
    /// The search's permit for the line, released with it.
    _permit: OwnedSemaphorePermit,
}

/// A page requested ahead of a walk. Its task can be awaited only once, so the page keeps the
/// task's answer once waited for.
enum PageAhead {
    /// The task, which answers whether the page listed any of the line.
    Requested(AbortOnDropHandle<bool>),
    /// Whether the page listed any of the line.
    Answered(bool),
}

impl PageAhead {
    /// Waits for the page, and answers whether it listed any of the line.
    async fn listed(&mut self) -> bool {
        if let Self::Requested(task) = self {
            *self = Self::Answered(matches!(task.await, Ok(true)));
        }
        matches!(self, Self::Answered(true))
    }

    /// Aborts the page's task where it is still in flight.
    fn abort(&self) {
        if let Self::Requested(task) = self
            && !task.is_finished()
        {
            task.abort();
        }
    }
}

/// What waiting for the pages requested ahead found for a revision the local store lacks.
enum Waited {
    /// A page held the revision.
    Held,
    /// The page requested from this revision number listed none of the line. The other pages are
    /// still requested.
    ListedNothing(u64),
    /// No page requested holds the revision.
    NotHeld,
}

impl LinePager {
    /// A pager for one walk of a search, whose pagers share the permits of `lines_ahead`.
    #[lore_macro::test_pub]
    pub(crate) fn new(repository: Arc<RepositoryContext>, lines_ahead: Arc<Semaphore>) -> Self {
        Self {
            repository,
            lines_ahead,
            paging: true,
            own_pages: 0,
            ahead: Ahead::NotYet,
        }
    }

    /// Whether the remote lists pages, so that the walk asks for each revision before it reads it.
    #[lore_macro::test_pub]
    pub(crate) fn paging(&self) -> bool {
        self.paging
    }

    /// Makes the local store hold the state of `revision`, the next revision the walk reads, where
    /// the remote lists it. `below` is the revision number of the revision the walk read before
    /// it, or `u64::MAX` for the first. Where the store lacks the revision, the pager waits for the
    /// pages requested ahead, in order, and where none of them holds it, reads its page by hash
    /// signature, a remote request on a cold path, once a page, so its future is boxed.
    ///
    /// The pages are read only down to the line's first revision numbered `floor` or lower, and
    /// only the states of the revisions they list are cached, without their metadata: the remote
    /// answers a page whole, so the bound saves no request, but keeps the walk from storing what
    /// it won't read. Once the remote lists no page by hash signature, as where the repository has
    /// no remote, such as on a server, or the remote lists nothing, the pager stops paging, and the
    /// walk reads each revision on its own.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[lore_macro::test_pub]
    #[allow(clippy::manual_async_fn)]
    pub(crate) fn page_in(
        &mut self,
        revision: Hash,
        below: u64,
        floor: u64,
    ) -> impl Future<Output = ()> + Send + '_ {
        async move {
            if !self.paging {
                return;
            }
            if let Ahead::On(ahead) = &mut self.ahead {
                ahead.pass(below).await;
                ahead.request(&self.repository, floor);
            }
            if stored_local(&self.repository, revision).await {
                return;
            }
            let mut listed_nothing = None;
            if let Ahead::On(ahead) = &mut self.ahead {
                match ahead.wait_for(&self.repository, revision).await {
                    Waited::Held => {
                        ahead.request(&self.repository, floor);
                        return;
                    }
                    Waited::ListedNothing(number) => listed_nothing = Some(number),
                    Waited::NotHeld => {}
                }
            }
            let Ok(Some(page)) = Box::pin(remote_page(
                self.repository.clone(),
                revision,
                Some(floor),
                None,
            ))
            .await
            else {
                self.paging = false;
                self.finish().await;
                return;
            };
            self.own_pages += 1;
            match &mut self.ahead {
                Ahead::On(ahead)
                    if ahead.held || listed_nothing.is_some_and(|number| page.newest < number) =>
                {
                    ahead.pass(page.oldest).await;
                    if ahead.pages.is_empty() {
                        ahead.next = page.oldest.saturating_sub(1);
                    }
                    ahead.held = false;
                    ahead.request(&self.repository, floor);
                }
                Ahead::On(_) => self.finish().await,
                Ahead::NotYet
                    if self.own_pages >= OWN_PAGES_BEFORE_AHEAD
                        && page.oldest.saturating_sub(1) > floor =>
                {
                    self.start_ahead(page, floor).await;
                }
                Ahead::NotYet | Ahead::Off => {}
            }
        }
    }

    /// Requests the pages below `page`, the walk's last page of its own, where the pager takes a
    /// permit and reads the branch of the page's oldest revision from its metadata. The metadata
    /// is read once a walk, a cold path, so its future is boxed.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn start_ahead(&mut self, page: Listed, floor: u64) -> impl Future<Output = ()> + Send + '_ {
        async move {
            let Ok(permit) = self.lines_ahead.clone().try_acquire_owned() else {
                return;
            };
            let read = Box::pin(Metadata::deserialize(
                self.repository.clone(),
                page.metadata,
            ))
            .await;
            let Some(branch) = read.ok().and_then(|metadata| metadata.get_branch().ok()) else {
                self.ahead = Ahead::Off;
                return;
            };
            let next = page.oldest.saturating_sub(1);
            lore_debug!(
                "Requesting pages of branch {branch} ahead of the walk from revision number {next}"
            );
            let mut ahead = PagesAhead {
                branch,
                next,
                pages: VecDeque::with_capacity(PAGES_AHEAD),
                held: false,
                _permit: permit,
            };
            ahead.request(&self.repository, floor);
            self.ahead = Ahead::On(ahead);
        }
    }

    /// Ends the pages requested ahead, waiting for their tasks to end, and returns the pager's
    /// permit: the pager requests no pages ahead again.
    #[lore_macro::test_pub]
    pub(crate) async fn finish(&mut self) {
        if let Ahead::On(ahead) = &mut self.ahead {
            ahead.end().await;
        }
        self.ahead = Ahead::Off;
    }
}

impl PagesAhead {
    /// Requests pages until [`PAGES_AHEAD`] are requested and not retired, or the next would list
    /// only revisions numbered `floor` or lower.
    fn request(&mut self, repository: &Arc<RepositoryContext>, floor: u64) {
        while self.pages.len() < PAGES_AHEAD && self.next > floor {
            let bottom = page_floor(self.next);
            let page = page_by_number(repository.clone(), self.branch, self.next, bottom, floor);
            self.pages.push_back((
                self.next,
                PageAhead::Requested(AbortOnDropHandle::new(lore_spawn!(page))),
            ));
            self.next = bottom.saturating_sub(1);
        }
    }

    /// Retires the pages whose revision numbers down to the next page are all `below` or higher,
    /// the store holding the line down to the revision numbered `below`: a page still in flight is
    /// aborted, and each has ended once retired. A retired page that listed any of the line sets
    /// [`Self::held`].
    async fn pass(&mut self, below: u64) {
        loop {
            let Some((number, page)) = self.pages.front_mut() else {
                return;
            };
            if page_floor(*number) < below {
                return;
            }
            page.abort();
            if page.listed().await {
                self.held = true;
            }
            self.pages.pop_front();
        }
    }

    /// Waits for the pages not waited for before, in the order requested, until the local store
    /// holds the state of `revision`, or a page lists none of the line, which stops the wait and is
    /// removed. A page waited for before listed the line, and had ended when the walk found the
    /// store lacked `revision`. The pages that list the line stay requested until [`Self::pass`]
    /// retires them or [`Self::end`] ends them.
    ///
    /// Not an `async fn`, which would hold a second copy of its arguments.
    #[allow(clippy::manual_async_fn)]
    fn wait_for<'a>(
        &'a mut self,
        repository: &'a Arc<RepositoryContext>,
        revision: Hash,
    ) -> impl Future<Output = Waited> + Send + 'a {
        async move {
            loop {
                {
                    let Some((index, (number, page))) = self
                        .pages
                        .iter_mut()
                        .enumerate()
                        .find(|(_, (_, page))| matches!(page, PageAhead::Requested(_)))
                    else {
                        return Waited::NotHeld;
                    };
                    if !page.listed().await {
                        let number = *number;
                        self.pages.remove(index);
                        return Waited::ListedNothing(number);
                    }
                }
                if stored_local(repository, revision).await {
                    self.held = true;
                    return Waited::Held;
                }
            }
        }
    }

    /// Aborts the pages requested and not retired, and waits for their tasks to end.
    async fn end(&mut self) {
        for (_, page) in &self.pages {
            page.abort();
        }
        loop {
            let Some((_, page)) = self.pages.front_mut() else {
                return;
            };
            page.listed().await;
            self.pages.pop_front();
        }
    }
}

/// Whether the local store of `repository` holds the state of `revision`.
fn stored_local(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
) -> impl Future<Output = bool> + Send + use<> {
    immutable::is_stored_local(repository.clone(), Address::zero_context_hash(revision))
}

/// Reads the history line of `branch` from revision number `number` from the remote into the local
/// store, down to revision number `bottom` or to the line's first revision numbered `floor` or
/// lower, as [`LinePager::page_in`] reads a page by hash signature. The remote's page holds the
/// revision its request names somewhere among the revisions it lists, newest first, and the remote
/// picks the page's size. So the line starts at the first revision the page lists as numbered
/// `number` or lower. Where a page ends above `bottom`, the page below it is read by the page's
/// cursor, or where it has none, from the revision number below its oldest revision, and the line
/// continues at the first parent of the line's last revision. Only the states of the line are
/// cached, and the line ends at the first state that isn't its revision's own, isn't numbered
/// below the one above it, or isn't the first parent of the one above it, and at a page whose
/// cursor isn't the first parent of the line's last revision. Answers whether a page listed any
/// of the line.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn page_by_number(
    repository: Arc<RepositoryContext>,
    branch: BranchId,
    number: u64,
    bottom: u64,
    floor: u64,
) -> impl Future<Output = bool> + Send {
    async move {
        let Ok(remote) = repository.remote().await else {
            return false;
        };
        let Ok(revision_protocol) = remote.revision(repository.id).await else {
            return false;
        };
        let mut start: RevisionListStart = RevisionListIdentifier { branch, number }.into();
        let mut above = number.saturating_add(1);
        let mut next = None;
        let mut listed_any = false;
        loop {
            let response = match revision_protocol.revision_list(start).await {
                Ok(response) => response,
                Err(err) => {
                    if listed_any {
                        lore_debug!(
                            "The remote lists no page of branch {branch} below revision number {above}: {err}"
                        );
                    } else {
                        lore_debug!(
                            "The remote lists no page of branch {branch} from revision number {number}: {err}"
                        );
                    }
                    return listed_any;
                }
            };
            let line = match next {
                Some(next) => response.items_from(next),
                None => response
                    .items
                    .iter()
                    .position(|item| item.number < above)
                    .map_or(&[][..], |first| &response.items[first..]),
            };
            let mut listed = 0;
            for item in line {
                let Some((item_number, parent)) = line_state(item, next.unwrap_or(item.signature))
                    .ok()
                    .map(|state| (state.revision_number, state.parent[0]))
                    .filter(|&(item_number, _)| item_number < above)
                else {
                    break;
                };
                listed += 1;
                above = item_number;
                next = Some(parent);
                if item_number <= floor {
                    break;
                }
            }
            if listed == 0 {
                return listed_any;
            }
            cache_page_states(repository.clone(), &line[..listed], 0).await;
            listed_any = true;
            if above <= bottom.max(floor)
                || listed < line.len()
                || !response.next_revision.is_zero() && next != Some(response.next_revision)
            {
                return true;
            }
            // `above` is above `bottom`, so the request's revision number is never 0, which the
            // remote reads as naming the branch's latest revision.
            start = if response.next_revision.is_zero() {
                RevisionListIdentifier {
                    branch,
                    number: above - 1,
                }
                .into()
            } else {
                response.next_revision.into()
            };
        }
    }
}

/// The revision numbers of the newest and the oldest revision of a line a page lists, and the
/// metadata hash of the oldest, as their states record them.
#[derive(Clone, Copy)]
struct Listed {
    newest: u64,
    oldest: u64,
    metadata: Hash,
}

/// Reads the remote's page of the line from `revision`, which the local store lacks, ending at its
/// first revision numbered `floor` or lower where there is a floor. Answers what it listed of the
/// line, as [`Listed`] records it, none where it listed none or the remote cannot list it, and
/// appends each revision it listed to `history`, if any, with its number as its state records it,
/// read by [`verified_state`]. The line runs from `revision` through the first parent each state
/// names.
///
/// Without a floor the page's states and the metadata of the revisions on the line are cached, as
/// [`batch_load_history`] describes. The read fails where the page doesn't hold `revision`, lists
/// a revision of the line without its own state, or lists one that isn't the first parent of the
/// revision above it, and where the page's cursor, if set, isn't the first parent of its last
/// revision. With a floor, only the states of the listed revisions are cached, as
/// [`LinePager::page_in`] describes, and the line ends at the first revision that isn't the first
/// parent of the one above it. A revision without its own state fails the read either way.
async fn remote_page(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    floor: Option<u64>,
    mut history: Option<&mut Vec<(u64, Hash)>>,
) -> Result<Option<Listed>, FindError> {
    let Ok(remote) = repository.remote().await else {
        return Ok(None);
    };
    let Ok(revision_protocol) = remote.revision(repository.id).await else {
        return Ok(None);
    };

    if let Some(floor) = floor {
        lore_debug!("Query remote for revision state history down to revision number {floor}");
    } else {
        lore_debug!("Query remote for revision state history");
    }

    let Ok(response) = revision_protocol.revision_list(revision.into()).await else {
        return Ok(None);
    };

    lore_debug!("Got {} revisions", response.items.len(),);

    let line = response.items_from(revision);
    if line.is_empty() && floor.is_none() {
        return Err(FindError::internal(format!(
            "the remote listed a page without revision {revision}, which it was asked for"
        )));
    }
    let mut listed = 0;
    let mut page: Option<Listed> = None;
    let mut next = revision;
    for item in line {
        let state = match line_state(item, next) {
            Ok(state) => state,
            Err(LineFault::OffTheLine { .. }) if floor.is_some() => break,
            Err(fault) => return Err(FindError::internal(fault.to_string())),
        };
        next = state.parent[0];
        let number = state.revision_number;
        if let Some(history) = history.as_deref_mut() {
            history.push((number, item.signature));
        }
        listed += 1;
        page = Some(Listed {
            newest: page.map_or(number, |page| page.newest),
            oldest: number,
            metadata: state.metadata_hash(),
        });
        if floor.is_some_and(|floor| number <= floor) {
            break;
        }
    }
    if floor.is_some() {
        cache_page_states(repository, &line[..listed], 0).await;
        return Ok(page);
    }
    if !response.next_revision.is_zero()
        && let Err(fault) = LineFault::check(response.next_revision, next)
    {
        return Err(FindError::internal(fault.to_string()));
    }

    let metadata_addresses: Vec<Address> = line
        .iter()
        .map(|item| Address::zero_context_hash(item.metadata))
        .collect();
    let unverified = response.items.len() - line.len();

    let _ = immutable::cache(repository.clone(), metadata_addresses, true).await;
    cache_page_states(repository, &response.items, unverified).await;

    Ok(page)
}

/// Cache the per-item state blobs from a remote `revision_list` response into
/// the local immutable store. Each item carries the 320-byte serialized
/// `StateData` for its revision; verifying its blake3 hash against the
/// `signature` lets the local cache absorb the response without a follow-up
/// fetch and protects against a server returning bytes that do not match the
/// signature it advertises.
pub fn cache_revision_list_states(
    repository: Arc<RepositoryContext>,
    items: &[lore_transport::RevisionItem],
) -> impl Future<Output = ()> + '_ {
    cache_page_states(repository, items, items.len())
}

/// Write the states a remote page's `items` carry to the local immutable store, so the revisions
/// they describe are read without a remote fetch. Each of the first `unverified` items is written
/// where its state is its revision's own, as [`holds_its_state`] checks, so bytes that don't hash
/// to the item's signature are not stored. Each item after them is one the caller has checked the
/// same way, as [`line_state`] does, so it is written without hashing it again.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
pub(crate) fn cache_page_states(
    repository: Arc<RepositoryContext>,
    items: &[lore_transport::RevisionItem],
    mut unverified: usize,
) -> impl Future<Output = ()> + '_ {
    async move {
        for item in items {
            if unverified > 0 {
                unverified -= 1;
                if !holds_its_state(item) {
                    continue;
                }
            }
            let _ = immutable::write(
                repository.clone(),
                Context::default(),
                item.state.clone(),
                immutable::write_options_from_repository(repository.clone())
                    .with_revision_state()
                    .with_local_cache_priority()
                    .no_remote_write(),
            )
            .await;
        }
    }
}

/// Whether `item` carries its revision's state: the bytes its signature is the hash of.
fn holds_its_state(item: &lore_transport::RevisionItem) -> bool {
    item.state.len() == std::mem::size_of::<StateData>()
        && Hash::hash_buffer(&item.state) == item.signature
}

/// The state `item` carries, where that state is the revision's own, as [`holds_its_state`]
/// checks, and its header carries the state magic. The number and metadata beside it in the item
/// are the server's claim; the state's are the ones its signature fixes.
pub fn verified_state(item: &lore_transport::RevisionItem) -> Option<StateData> {
    if !holds_its_state(item) {
        return None;
    }
    StateData::read_from_bytes(&item.state)
        .ok()
        .filter(StateData::has_state_magic)
}

/// The state `item` carries, as [`verified_state`] reads it, where `item` is `held`, the revision
/// a line through first parents holds at the item's place. Otherwise answers how the item breaks
/// the line, its state checked first.
pub(crate) fn line_state(
    item: &lore_transport::RevisionItem,
    held: Hash,
) -> Result<StateData, LineFault> {
    let state = verified_state(item).ok_or(LineFault::WithoutItsState(item.signature))?;
    LineFault::check(item.signature, held)?;
    Ok(state)
}

/// How an item of a remote page, or the page's cursor, breaks a line through first parents.
pub(crate) enum LineFault {
    /// The item, named by its hash signature, doesn't carry its own state.
    WithoutItsState(Hash),
    /// The page names revision `named` where the line holds revision `held`.
    OffTheLine { named: Hash, held: Hash },
}

impl LineFault {
    /// Fails where `named`, the revision a page names at a place of the line, isn't `held`, the
    /// revision the line holds there.
    pub(crate) fn check(named: Hash, held: Hash) -> Result<(), Self> {
        if named == held {
            Ok(())
        } else {
            Err(Self::OffTheLine { named, held })
        }
    }
}

impl std::fmt::Display for LineFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WithoutItsState(revision) => write!(
                f,
                "the remote listed revision {revision} without its own state"
            ),
            Self::OffTheLine { named, held } => write!(
                f,
                "the remote named revision {named} where the line holds revision {held}"
            ),
        }
    }
}

/// Data for the event reporting a revision found by a search.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, bitcode::Encode, bitcode::Decode, lore_macro::WireSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionFindEventData {
    /// Signature of the revision that was found.
    pub signature: Hash,
}

pub enum FindOptions {
    KeyValue { key: LoreString, value: LoreString },
    Number(u64),
}

pub(crate) async fn find(
    repository: Arc<RepositoryContext>,
    options: FindOptions,
) -> Result<(), FindError> {
    let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward_any::<FindError>("deserializing current anchor")?;

    let result = match options {
        FindOptions::KeyValue { key, value } => {
            let value = if !value.is_empty() {
                Some(value.as_str())
            } else {
                None
            };
            revision_by_metadata(
                repository.clone(),
                current_branch,
                Hash::default(),
                key.as_str(),
                value,
            )
            .await
        }
        FindOptions::Number(number) => {
            if number == 0 {
                Err(FindError::internal("no revision specified"))
            } else {
                revision_by_number(repository.clone(), current_branch, Hash::default(), number)
                    .await
            }
        }
    };

    match result {
        Ok(signature) => {
            event::LoreEvent::RevisionFind(LoreRevisionFindEventData { signature }).send();
            Ok(())
        }
        Err(err) => {
            event::LoreEvent::RevisionFind(LoreRevisionFindEventData {
                signature: Hash::default(),
            })
            .send();
            Err(err)
        }
    }
}

/// Boxed version of [`find`] for cross-crate use.
pub fn find_boxed(
    repository: Arc<RepositoryContext>,
    options: FindOptions,
) -> crate::BoxFuture<'static, Result<(), FindError>> {
    Box::pin(find(repository, options))
}
