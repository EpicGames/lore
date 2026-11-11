// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_storage::options::ReadOptions;
use lore_transport::ProtocolError;
use lore_transport::RevisionItem;
use lore_transport::RevisionListResponse;
use tokio_util::task::AbortOnDropHandle;

use crate::errors::*;
use crate::find;
use crate::lore::Hash;
use crate::repository::RepositoryContext;

#[error_set]
pub enum HistoryError {
    AddressNotFound,
    Disconnected,
    InvalidArguments,
    InvalidNodeHierarchy,
    InvalidPath,
    LinkNotFound,
    Maintenance,
    NodeNotFound,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    RevisionNotFound,
    SlowDown,
    WriteRequired,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
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
    FileNotFound,
    MissingIdentity,
}

impl crate::event::EventError for HistoryError {}

/// How a round takes a line further.
enum Step {
    /// The round walks the line in the local store.
    Store,
    /// The round asks the remote for the page holding the line's cursor, a revision the store
    /// lacks.
    Remote,
    /// The remote took the line no further, so it is not asked for the line again. Holds the error
    /// of the request that stopped the line; a failed request is a cold path, so the error is
    /// boxed rather than held in every state of the walk.
    Stopped(Option<Box<ProtocolError>>),
}

/// One line of the walk: its revisions, newest first, and its cursor. The cursor is the next
/// revision to walk in the local store, zero once the line has ended, and the line's last revision
/// from where the store lacks it until a page moves the line.
struct Line {
    history: Vec<Hash>,
    /// The number of each revision in `history`. A revision the store lacks is numbered zero until
    /// a page lists it, and stays zero where no page does, which ends the line there.
    numbers: Vec<u64>,
    cursor: Hash,
    step: Step,
    /// How many revisions the round added to the line.
    added: usize,
}

impl Line {
    fn new(latest: Hash) -> Self {
        Self {
            history: Vec::new(),
            numbers: Vec::new(),
            cursor: latest,
            step: Step::Store,
            added: 0,
        }
    }

    /// The revision at `index` on the line, with its number.
    fn at(&self, index: usize) -> Option<(u64, Hash)> {
        Some((*self.numbers.get(index)?, *self.history.get(index)?))
    }

    /// The revision at `index` on the line, with its number, short of the revision the round asks
    /// the remote to number. That revision is numbered zero until the page lists it, so [`meet`]
    /// would pass the other line's revisions against it that a revision below it can still match.
    fn numbered_at(&self, index: usize) -> Option<(u64, Hash)> {
        if self.asks() && index + 1 >= self.history.len() {
            return None;
        }
        self.at(index)
    }

    /// Add `revision`, numbered `number`, to the line in the round.
    fn push(&mut self, number: u64, revision: Hash) {
        self.numbers.push(number);
        self.history.push(revision);
        self.added += 1;
    }

    /// Whether the round asks the remote for the page holding the line's cursor. A line leaves the
    /// local store only at a revision the store lacks, so its cursor is not zero then.
    fn asks(&self) -> bool {
        matches!(self.step, Step::Remote)
    }

    /// Whether a round can walk the line further: it has neither ended nor stopped.
    fn advances(&self) -> bool {
        matches!(self.step, Step::Store) && !self.cursor.is_zero()
    }

    /// Walk up to ten revisions of the line in the local store, as far as the store holds them,
    /// reading each state with a [`find::local_line_step`]. The first revision the store lacks is
    /// added to the line, numbered zero, and the line asks the remote for the page holding it. A
    /// blob that is not a revision state fails the walk.
    async fn walk_store(
        &mut self,
        repository: &Arc<RepositoryContext>,
    ) -> Result<(), HistoryError> {
        for _ in 0..10 {
            if self.cursor.is_zero() {
                break;
            }
            let (number, parent) = match find::local_line_step(
                repository,
                self.cursor,
                ReadOptions::default(),
            )
            .await
            {
                Ok(Some(step)) => step,
                Ok(None) => {
                    self.push(0, self.cursor);
                    self.step = Step::Remote;
                    break;
                }
                Err(err) => {
                    let hash = self.cursor;
                    return Err(err).forward_with::<HistoryError, _>(|| {
                        format!("reading the state of revision {hash}")
                    });
                }
            };
            self.push(number, self.cursor);
            // TODO(mjansson): Support finding through merged and other parent
            self.cursor = parent;
        }
        Ok(())
    }

    /// Take `listing`, the remote's answer for the page holding the line's cursor. A page extends
    /// the line and is returned so its states can be cached, with the number of items it lists
    /// above the line's cursor, whose states the line did not verify. `NotFound` ends the line, and
    /// any other failure stops it. A line the answer moves is walked in the local store again, and
    /// one it does not move is not asked for again. Fails where the page lists a revision on the
    /// line without its own state.
    fn take(
        &mut self,
        listing: Result<RevisionListResponse, ProtocolError>,
    ) -> Result<Option<(RevisionListResponse, usize)>, HistoryError> {
        match listing {
            Ok(page) => {
                let listed = page.items_from(self.cursor);
                let unverified = page.items.len() - listed.len();
                self.step = match self.extend(listed, page.next_revision) {
                    Ok(true) => Step::Store,
                    Ok(false) => Step::Stopped(None),
                    Err(revision) => {
                        return Err(HistoryError::internal(format!(
                            "the remote listed revision {revision} without its own state"
                        )));
                    }
                };
                Ok(Some((page, unverified)))
            }
            Err(err) if err.is_not_found() => {
                self.cursor = Hash::default();
                self.step = Step::Store;
                Ok(None)
            }
            Err(err) => {
                self.stop(err);
                Ok(None)
            }
        }
    }

    /// Stop asking the remote for the line, keeping `err`, the error of its request.
    fn stop(&mut self, err: ProtocolError) {
        self.step = Step::Stopped(Some(Box::new(err)));
    }

    /// Extend the line with `listed`, the items of a page from the line's cursor on, and move the
    /// cursor to `next`, the next revision the page names. A page that does not hold the cursor is
    /// not on the line, and neither is its next revision, so it ends the line. The local walk adds
    /// the revision it stops at, with the number zero, before the line asks for the page holding
    /// it, so the page numbers that revision rather than adding it again. Each number is read once
    /// from the state the page carries, as [`find::verified_number`] describes. Returns whether the
    /// cursor moved, or the first revision that `listed` holds without its own state.
    fn extend(&mut self, listed: &[RevisionItem], next: Hash) -> Result<bool, Hash> {
        let anchor = self.cursor;
        let Some((first, older)) = listed.split_first() else {
            self.cursor = Hash::default();
            return Ok(true);
        };
        let anchor_number = find::verified_number(first).ok_or(first.signature)?;
        let start = self.history.len();
        for item in older {
            let number = find::verified_number(item).ok_or(item.signature)?;
            self.numbers.push(number);
            self.history.push(item.signature);
        }
        if let Some(number) = start
            .checked_sub(1)
            .and_then(|index| self.numbers.get_mut(index))
        {
            *number = anchor_number;
        }
        self.added += older.len();
        self.cursor = next;
        Ok(self.cursor != anchor)
    }

    /// Fail where a revision the round added does not descend in number from the one before it.
    /// Such a line is out of order, and may return to a revision and repeat without end.
    fn check_round(&self) -> Result<(), HistoryError> {
        match out_of_order(|index| self.at(index), self.history.len() - self.added) {
            Some((number, revision)) => Err(HistoryError::internal(format!(
                "the history line is out of order at revision {revision}, numbered {number}"
            ))),
            None => Ok(()),
        }
    }
}

/// The first revision of a line from index `from` on whose number is not below that of the
/// revision before it, with its number, where `line` gives each revision of the line by its index,
/// newest first, with its number. Revision numbers decrease strictly along self parents, as
/// [`meet`] relies on, so a line with such a revision does not follow its self parents, and may
/// return to a revision.
#[inline]
pub(crate) fn out_of_order(
    line: impl Fn(usize) -> Option<(u64, Hash)>,
    from: usize,
) -> Option<(u64, Hash)> {
    let mut index = from.max(1);
    let (mut above, _) = line(index - 1)?;
    while let Some((number, revision)) = line(index) {
        if number >= above {
            return Some((number, revision));
        }
        above = number;
        index += 1;
    }
    None
}

/// Advance `passed`, the index on each of two lines of its first revision not yet passed, to
/// where the lines meet, and return the index on each line of the newest revision both hold.
/// `left` and `right` give each revision of their line by its index, with its number.
///
/// Each line runs newest first through self parents, and its revision numbers decrease strictly,
/// as [`out_of_order`] checks. A revision has one number wherever it is listed, so a revision the
/// lines share has the same number on both, and every revision below it is shared. While neither
/// line is passed to its end, the revisions at `passed` are where the lines meet when they are
/// the same revision. Otherwise the line whose revision has the higher number passes it, since the
/// other line holds no revision that high from there on, and at equal numbers both lines pass
/// theirs, which are siblings. A number of zero is unknown and ends its line, so the other line
/// passes its revisions one by one, each tested against that one.
///
/// Returns `None` once one line is passed to its end, with `passed` left there, so a call after
/// that line grows takes both lines on from where this one stopped.
#[inline]
pub(crate) fn meet(
    left: impl Fn(usize) -> Option<(u64, Hash)>,
    right: impl Fn(usize) -> Option<(u64, Hash)>,
    passed: &mut (usize, usize),
) -> Option<(usize, usize)> {
    while let (Some((left_number, left_revision)), Some((right_number, right_revision))) =
        (left(passed.0), right(passed.1))
    {
        if left_revision == right_revision {
            return Some(*passed);
        }
        if left_number >= right_number {
            passed.0 += 1;
        }
        if right_number >= left_number {
            passed.1 += 1;
        }
    }
    None
}

/// Which of the two lines the round walks, given `passed` as [`meet`] left it: each line `meet`
/// waits on, with every revision passed, that can go further. That is the line whose oldest
/// revision has the higher number, or both at equal numbers, so the other line is walked only
/// once its revisions are needed. Where no line `meet` waits on can go further, the round walks
/// every line that can, so a line that ended or stopped still has the other walked to its end.
fn advancing(left: &Line, right: &Line, passed: (usize, usize)) -> (bool, bool) {
    let left_waits = passed.0 == left.history.len() && left.advances();
    let right_waits = passed.1 == right.history.len() && right.advances();
    if left_waits || right_waits {
        (left_waits, right_waits)
    } else {
        (left.advances(), right.advances())
    }
}

/// Ask the remote for the page holding the cursor of each line the round asks for, take the
/// answers, and cache the states the pages carry as [`find::cache_page_states`] describes. When
/// both lines are asked, the right line's request runs as a task beside the left line's, and is
/// aborted if the walk is dropped. A round that asks for one line spawns nothing. A remote that
/// cannot be reached stops every line asked for, and a page listing a revision on its line without
/// its own state fails the walk.
async fn ask_remote(
    repository: &Arc<RepositoryContext>,
    left: &mut Line,
    right: &mut Line,
) -> Result<(), HistoryError> {
    let ask_left = left.asks();
    let ask_right = right.asks();
    if !ask_left && !ask_right {
        return Ok(());
    }
    // Scoped so the future does not hold the revision client across the caching below.
    let (first, second) = {
        let revision = match repository.remote().await {
            Ok(remote) => remote.revision(repository.id).await,
            Err(err) => Err(err),
        };
        let revision = match revision {
            Ok(revision) => revision,
            Err(err) => {
                if ask_left && ask_right {
                    right.stop(err.clone());
                }
                if ask_left {
                    left.stop(err);
                } else {
                    right.stop(err);
                }
                return Ok(());
            }
        };
        // TODO(mjansson): Support finding through merged and other parent
        let second = (ask_left && ask_right).then(|| {
            let revision = revision.clone();
            let cursor = right.cursor;
            AbortOnDropHandle::new(lore_spawn!(async move {
                revision.revision_list(cursor.into()).await
            }))
        });
        let cursor = if ask_left { left.cursor } else { right.cursor };
        (revision.revision_list(cursor.into()).await, second)
    };
    // Scoped so the future does not hold the first page across the second request.
    {
        let page = if ask_left {
            left.take(first)?
        } else {
            right.take(first)?
        };
        if let Some((page, unverified)) = &page {
            find::cache_page_states(repository.clone(), &page.items, *unverified).await;
        }
    }
    if let Some(second) = second {
        let listing = second
            .await
            .internal("listing the right line's revisions")?;
        if let Some((page, unverified)) = &right.take(listing)? {
            find::cache_page_states(repository.clone(), &page.items, *unverified).await;
        }
    }
    Ok(())
}

/// The revision the lines meet at, whose index on each line `shared` holds, with the revisions
/// above it on each line, newest first.
fn branch_point(
    mut left: Line,
    mut right: Line,
    shared: (usize, usize),
) -> (Hash, Vec<Hash>, Vec<Hash>) {
    let branch_point = left.history[shared.0];
    left.history.truncate(shared.0);
    right.history.truncate(shared.1);
    (branch_point, left.history, right.history)
}

/// Fail a walk that neither line can take further. Where the request for a line failed, the walk
/// fails with the remote's error, the left line's when both failed. Otherwise, or where there was
/// no remote to ask, it fails with [`RevisionNotFound`] naming the revision the line stopped at,
/// the left line's when both stopped.
fn stall<T>(left: Line, right: Line) -> Result<T, HistoryError> {
    let (cursor, failure) = match (left.step, right.step) {
        (Step::Stopped(Some(failure)), _) => (left.cursor, Some(failure)),
        (_, Step::Stopped(Some(failure))) => (right.cursor, Some(failure)),
        _ if left.cursor.is_zero() => (right.cursor, None),
        _ => (left.cursor, None),
    };
    let stopped = RevisionNotFound {
        revision: cursor.to_string(),
    };
    let context = format!("listing the revisions from {cursor}");
    match failure {
        Some(err) if err.is_no_remote() => Err(HistoryError::RevisionNotFound(
            stopped.chain_err_from(*err, &context),
        )),
        Some(err) => Err(*err).forward::<HistoryError>(&context),
        None => Err(HistoryError::from(stopped)),
    }
}

/// The newest revision both `left_latest` and `right_latest` descend from through self parents,
/// with the revisions above it on each line, newest first. The revision is zero when both lines
/// end without sharing one.
///
/// Each round walks up to ten revisions of a line in the local store. It walks the lines
/// [`advancing`] names. Where a line reaches a revision the store lacks, the round asks the remote
/// for the page holding that revision only where the lines have not met among the numbered
/// revisions they hold. Lines that have not met there, but both reach the same revision the store
/// lacks in the same round, meet at that revision without asking: it is the newest they share,
/// since [`meet`] found none above it. A line the page moves is walked in the store again from its
/// new cursor, and a line ends where the remote does not hold the revision asked for. A request
/// that fails, or a page that does not move the line, stops the line where it is, and the remote
/// is not asked for it again. A page that lists a revision on the line without its own state fails
/// the walk, as [`ask_remote`] describes, and so does a blob in the local store that is not a
/// revision state, as [`Line::walk_store`] describes.
///
/// Each round takes the lines on to where they meet as [`meet`] describes, from where the round
/// before left them: before the remote is asked, short of the revisions the page will number, as
/// [`Line::numbered_at`] describes, and after it answers. The walk's time grows with the revisions
/// it walks. A round that adds no revision to either line would only repeat, so the walk then
/// fails as [`stall`] describes. A line whose revision numbers do not decrease is out of order,
/// and may return to a revision and repeat without end, so the walk fails then as well.
///
/// Not an `async fn`, which would hold a second copy of its arguments.
#[inline]
#[allow(clippy::manual_async_fn)]
pub fn find_branch_point(
    repository: Arc<RepositoryContext>,
    left_latest: Hash,
    right_latest: Hash,
) -> impl Future<Output = Result<(Hash, Vec<Hash>, Vec<Hash>), HistoryError>> {
    let mut left = Line::new(left_latest);
    let mut right = Line::new(right_latest);
    async move {
        // TODO(mjansson): Accelerate by storing last synced hash for each remote and branch
        let mut passed = (0, 0);
        while !left.cursor.is_zero() || !right.cursor.is_zero() {
            let (walk_left, walk_right) = advancing(&left, &right, passed);
            left.added = 0;
            right.added = 0;
            if walk_left {
                left.walk_store(&repository).await?;
            }
            if walk_right {
                right.walk_store(&repository).await?;
            }
            if left.asks() || right.asks() {
                left.check_round()?;
                right.check_round()?;
                let shared = meet(
                    |index| left.numbered_at(index),
                    |index| right.numbered_at(index),
                    &mut passed,
                );
                if let Some(shared) = shared {
                    return Ok(branch_point(left, right, shared));
                }
                if left.asks() && right.asks() && left.cursor == right.cursor {
                    let shared = (left.history.len() - 1, right.history.len() - 1);
                    return Ok(branch_point(left, right, shared));
                }
                ask_remote(&repository, &mut left, &mut right).await?;
            }

            left.check_round()?;
            right.check_round()?;
            let shared = meet(|index| left.at(index), |index| right.at(index), &mut passed);
            if let Some(shared) = shared {
                return Ok(branch_point(left, right, shared));
            }

            if left.cursor.is_zero() && right.cursor.is_zero() {
                return Ok((Hash::default(), left.history, right.history));
            }
            if left.added == 0 && right.added == 0 {
                return stall(left, right);
            }
        }

        Err(HistoryError::internal("failed to find a branch point"))
    }
}
