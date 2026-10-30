// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The threaded walk across both slots, held against each slot stepped on its own.

use lore_revision::filter::Filter;
use lore_revision::filter::FilterInstance;
use lore_revision::filter::FilterMode;
use lore_revision::filter::FilterReason;
use lore_revision::filter::FilterState;
use lore_revision::filter::FilterStates;
use lore_revision::util::path::RelativePath;

/// Paths that one slot excludes, the other, or both; that one slot settles and
/// the other does not; and that a slot re-includes below an exclusion.
const PATHS: &[(&str, bool)] = &[
    ("build", false),
    ("build", true),
    ("build/keep", true),
    ("build/keep/a.txt", false),
    ("build/out.bin", false),
    ("cache.tmp", true),
    ("cache.tmp/a.txt", false),
    ("docs", true),
    ("docs/readme.md", false),
    ("engine", true),
    ("engine/a.cpp", false),
    ("engine/intermediate", true),
    ("engine/intermediate/x.obj", false),
    ("engine/scratch.tmp", false),
    ("game", true),
    ("game/maps", true),
    ("game/maps/level.umap", false),
    ("game/scratch.tmp", false),
    ("root.tmp", false),
    ("top.txt", false),
];

fn slot(rules: &[&str]) -> FilterInstance {
    let mut slot = FilterInstance::default();
    for rule in rules {
        match rule.strip_prefix('!') {
            Some(rest) => slot.add_inclusion(rest).expect("inclusion"),
            None => slot.add_exclusion(rule).expect("exclusion"),
        }
    }
    slot
}

/// The state `slot` steps `path` to where the mode consults it, and `parent`
/// where it does not.
fn slot_state(
    slot: &FilterInstance,
    consulted: bool,
    parent: FilterState,
    path: &RelativePath,
    is_directory: bool,
) -> FilterState {
    if consulted {
        slot.child_exclusion_state(parent, path, is_directory)
    } else {
        parent
    }
}

/// The reason for the first slot that holds, ignore before view.
fn first(ignore: bool, view: bool) -> Option<u8> {
    if ignore {
        Some(FilterReason::Ignore as u8)
    } else if view {
        Some(FilterReason::View as u8)
    } else {
        None
    }
}

/// A threaded step answers for every slot in the mode as that slot answers on
/// its own: the states the children inherit, the reason with ignore before view,
/// and whether the walk descends or drops the path.
#[test]
fn a_threaded_step_answers_as_each_slot_does_on_its_own() {
    let mut filter = Filter::default();
    filter.ignore = slot(&["*.tmp", "/build", "!/build/keep", "/engine/intermediate"]);
    filter.view = slot(&["/*", "!/engine", "!/game", "/game/maps", "!/build"]);

    for (name, is_directory) in PATHS {
        let path = RelativePath::new_from_initial_path(name).expect("path");
        let parent_path = path.parent_path();
        let parent = FilterStates {
            ignore: filter.ignore.exclusion_state(&parent_path, true),
            view: filter.view.exclusion_state(&parent_path, true),
        };
        for mode in [FilterMode::Ignore, FilterMode::View, FilterMode::Full] {
            let ignore = mode.contains(FilterMode::Ignore);
            let view = mode.contains(FilterMode::View);
            let expected = FilterStates {
                ignore: slot_state(&filter.ignore, ignore, parent.ignore, &path, *is_directory),
                view: slot_state(&filter.view, view, parent.view, &path, *is_directory),
            };
            let excluded = first(
                ignore && expected.ignore.excluded(),
                view && expected.view.excluded(),
            );

            let (states, reason) =
                filter.child_exclusion_states(parent, &path, *is_directory, mode);
            assert_eq!(states, expected, "{name} steps to other states, {mode:?}");
            assert_eq!(
                reason.map(|reason| reason as u8),
                excluded,
                "{name} reports another reason, {mode:?}"
            );

            let (tree_states, dropped) =
                filter.child_excludes_tree(parent, &path, *is_directory, mode);
            assert_eq!(
                tree_states, expected,
                "{name} hands down other states, {mode:?}"
            );
            if *is_directory {
                let stops = (ignore && !filter.ignore.should_descend(expected.ignore, &path))
                    || (view && !filter.view.should_descend(expected.view, &path));
                assert_eq!(dropped, stops, "{name} is dropped otherwise, {mode:?}");
                assert_eq!(
                    filter.should_descend(expected, &path, mode),
                    !stops,
                    "{name} is descended otherwise, {mode:?}"
                );
            } else {
                assert_eq!(
                    dropped,
                    excluded.is_some(),
                    "{name} is dropped otherwise, {mode:?}"
                );
            }
        }
    }
}
