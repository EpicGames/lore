// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The arithmetic behind a [`RuleIndex`] answer, which nothing outside the
//! module can reach. The behaviour it produces is asserted in `tests/filter.rs`.

use lore_revision::filter::*;

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
        // Both cross a separator from inside one component of the text.
        ("/a/b**", "a/b**", false, 2, u32::MAX),
        ("/a[!x]b", "a[!x]b", false, 1, u32::MAX),
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
