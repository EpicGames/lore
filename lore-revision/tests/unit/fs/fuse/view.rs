// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::filter::FilterStates;
use lore_revision::fs::fuse::view::filter_states_from_u8;
use lore_revision::fs::fuse::view::filter_states_to_u8;
use lore_revision::fs::fuse::view::same_view_rules;

fn view_filter(exclusions: &[&str]) -> lore_revision::filter::Filter {
    let mut filter = lore_revision::filter::Filter::default();
    for glob in exclusions {
        filter.view.add_exclusion(glob).expect("exclusion");
    }
    filter
}

#[test]
fn separately_built_filters_with_the_same_view_rules_are_the_same_view() {
    assert!(same_view_rules(
        &view_filter(&["build/", "*.tmp"]),
        &view_filter(&["build/", "*.tmp"])
    ));
    assert!(!same_view_rules(
        &view_filter(&["build/"]),
        &view_filter(&["build/", "*.tmp"])
    ));
    assert!(!same_view_rules(
        &view_filter(&["build/"]),
        &view_filter(&["out/"])
    ));
}

#[test]
fn round_trips_the_excluded_bits() {
    for (ignore, view) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut states = FilterStates::ROOT;
        // `FilterState`'s only public constructor besides `ROOT` is
        // `excluded_from`, added for exactly this: build the input this
        // test packs without a `decided_at` value to compare against,
        // since the round trip cannot preserve one either.
        states.ignore = lore_revision::filter::FilterState::excluded_from(ignore);
        states.view = lore_revision::filter::FilterState::excluded_from(view);

        let packed = filter_states_to_u8(states);
        let unpacked = filter_states_from_u8(packed);

        assert_eq!(unpacked.ignore.excluded(), ignore);
        assert_eq!(unpacked.view.excluded(), view);
    }
}
