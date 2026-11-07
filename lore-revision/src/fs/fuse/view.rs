// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The view mask a directory's listing carries per child: whether the
//! mounted view excludes it, packed to fit in `ListingEntry::filter_state`.

use std::any::Any;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;

use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::filter::FilterState;
use crate::filter::FilterStates;
use crate::filter::Scope;
use crate::repository::RepositoryContext;
use crate::util::path::RelativePath;

const IGNORE_EXCLUDED: u8 = 1;
const VIEW_EXCLUDED: u8 = 2;

/// The child's filter state one level below `parent`, and whether the view
/// slot excludes it -- the bit `Listing::build` records per entry.
pub fn child_filter(
    repository: &RepositoryContext,
    parent: FilterStates,
    path: &RelativePath,
    is_directory: bool,
) -> (FilterStates, bool) {
    let parent = within(&repository.filter, parent);
    repository
        .filter
        .child_emit_excludes(parent, path, is_directory, FilterMode::View)
}

/// `parent` with its view line no later than `filter`'s last. A verdict packed under one view and
/// read under another, as a request racing a view change reads one, would step from a line the
/// filter lacks, which fails the filter's assertion and so aborts the service; past the last line
/// nothing below an excluded directory re-includes, as below one the last line excluded.
fn within(filter: &Filter, mut parent: FilterStates) -> FilterStates {
    let lines = u32::try_from(filter.view.len()).unwrap_or(u32::MAX);
    if parent.view.decided_at() > lines {
        parent.view = FilterState::decided(parent.view.excluded(), lines);
    }
    parent
}

/// [`child_filter`] for a request that may run outside any execution context: the event an
/// exclusion reports goes out under `context`, entered on that branch alone and only where no
/// context is current, so a child the view keeps costs nothing more.
pub(crate) fn child_filter_in(
    context: Option<&Arc<dyn Any + Send + Sync>>,
    repository: &RepositoryContext,
    parent: FilterStates,
    path: &RelativePath,
    is_directory: bool,
) -> (FilterStates, bool) {
    let parent = within(&repository.filter, parent);
    let (states, reason) = repository.filter.child_exclude_reason(
        parent,
        path,
        is_directory,
        FilterMode::View,
        Scope::Tree,
    );
    if reason.is_none() {
        return (states, false);
    }
    let report = || Filter::emit(path, reason);
    match context {
        Some(context) if LORE_CONTEXT.try_with(|_| ()).is_err() => {
            LORE_CONTEXT.sync_scope(context.clone(), report)
        }
        _ => report(),
    };
    (states, true)
}

/// Packs the two slots' excluded bits into one byte.
///
/// Lossy: a `FilterState` also carries the line it was decided at
/// (`filter_states_from_u8` cannot restore that half; see there).
pub fn filter_states_to_u8(states: FilterStates) -> u8 {
    let mut bits = 0u8;
    if states.ignore.excluded() {
        bits |= IGNORE_EXCLUDED;
    }
    if states.view.excluded() {
        bits |= VIEW_EXCLUDED;
    }
    bits
}

/// The line the view slot's verdict in `states` was decided at, which a listing entry and a
/// directory's inode keep beside the bits [`filter_states_to_u8`] packs: a walk below an excluded
/// directory takes no line before it, so a re-inclusion authored before the line that excluded the
/// directory, which lost there, re-includes nothing below it. A line past `u16::MAX` is kept as
/// line 0, which lets such a re-inclusion apply below, as [`filter_states_from_u8`] does.
pub fn view_line(states: FilterStates) -> u16 {
    u16::try_from(states.view.decided_at()).unwrap_or(0)
}

/// Unpacks a byte from [`filter_states_to_u8`] with the view slot's [`view_line`]. The ignore
/// slot comes back decided at line 0: a mount's listings ask the view alone.
pub fn filter_states_from_packed(bits: u8, view_line: u16) -> FilterStates {
    FilterStates {
        ignore: FilterState::excluded_from(bits & IGNORE_EXCLUDED != 0),
        view: FilterState::decided(bits & VIEW_EXCLUDED != 0, u32::from(view_line)),
    }
}

/// Unpacks a byte from [`filter_states_to_u8`].
///
/// The line each slot's exclusion was decided at is not carried by that
/// byte -- `FilterState` exposes no public way to set one apart from `ROOT`
/// (always undecided) -- so both slots come back decided at line 0. A walk
/// resumed from this state re-scans from the top rather than from wherever
/// the original decision landed; the verdict it reaches is the same, just
/// not as cheaply as resuming the original walk would have been.
pub fn filter_states_from_u8(v: u8) -> FilterStates {
    FilterStates {
        ignore: FilterState::excluded_from(v & IGNORE_EXCLUDED != 0),
        view: FilterState::excluded_from(v & VIEW_EXCLUDED != 0),
    }
}

/// Whether `a` and `b` mount the same view, which is what makes their listing caches
/// interchangeable across a swap. Sync rebuilds the filter from the view file, so equal rules
/// count, not just the same `Filter`; otherwise every sync would invalidate the whole mount.
pub fn same_view(a: &RepositoryContext, b: &RepositoryContext) -> bool {
    Arc::ptr_eq(&a.filter, &b.filter) || same_view_rules(&a.filter, &b.filter)
}

pub fn same_view_rules(a: &crate::filter::Filter, b: &crate::filter::Filter) -> bool {
    a.view.same_rules(&b.view)
}
