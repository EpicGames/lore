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
        // counting the text stays an upper bound over both alternatives. In a
        // glob compared as text it is text; in one the matcher evaluates, a
        // group holding a `/` may expand to fewer, so the lower bound is 1.
        ("/a{b,c/d}", "a{b,c/d}", false, 2, 2),
        ("/a{b,c/d}*", "a{b,c/d}*", false, 1, 2),
        ("/{a,b/c}*", "{a,b/c}*", false, 1, 2),
        ("/{a,b}/c*", "{a,b}/c*", false, 2, 2),
        // Two or more `*` as a whole component compile to `**`, and to `*/**`
        // before an escaped `/`, where they match at least one component.
        ("/a/***/b", "a/**/b", false, 2, u32::MAX),
        ("/a/***", "a/**", false, 2, u32::MAX),
        ("***/a", "a", true, 1, u32::MAX),
        ("/**/a", "a", true, 1, u32::MAX),
        ("/***/a", "a", true, 1, u32::MAX),
        ("/**/a/b", "**/a/b", false, 2, u32::MAX),
        ("/a/**\\/b", "a/*/**/b", false, 3, u32::MAX),
        ("/a/***\\/b", "a/*/**/b", false, 3, u32::MAX),
        ("**\\/b", "*/**/b", false, 2, u32::MAX),
        ("/a\\/**\\/b\\/c", "a\\/*/**/b\\/c", false, 4, u32::MAX),
        ("/a\\/**/b", "a\\/**/b", false, 2, u32::MAX),
        ("/a**\\/b", "a**\\/b", false, 2, 2),
        // A bracket expression matches one character other than a separator,
        // whatever it holds, and an escaped or unclosed `[` opens none. A `/`
        // between the first `[` and the last `]` may match no separator, so the
        // lower bound is 1.
        ("/x[a/b]y", "x[a/b]y", false, 1, 2),
        ("/a[b/c]/d", "a[b/c]/d", false, 1, 3),
        ("/[a]/[b]", "[a]/[b]", false, 1, 2),
        ("/a[!x]b", "a[!x]b", false, 1, 1),
        ("/a[^x]b", "a[^x]b", false, 1, 1),
        ("/a[--0]b", "a[--0]b", false, 1, 1),
        ("/src/*.[ch]", "src/*.[ch]", false, 2, 2),
        ("/a/\\[b\\]", "a/\\[b\\]", false, 2, 2),
        ("/a/[b", "a/[b", false, 2, 2),
        // Two or more `*` sharing their component with other text are `*`.
        ("/a**", "a**", false, 1, 1),
        ("/a***", "a***", false, 1, 1),
        ("/**b", "**b", false, 1, 1),
        ("/a**b", "a**b", false, 1, 1),
        ("/a/b**", "a/b**", false, 2, 2),
        ("/a/**b/c", "a/**b/c", false, 3, 3),
        // A leading `!` that reaches the glob is a literal character, not a
        // negation that would match at any depth.
        ("/!Temp*", "!temp*", false, 1, 1),
        ("/!a/b", "!a/b", false, 2, 2),
    ];

    for (rule, glob, filename, min, max) in cases {
        let (compiled, compiled_filename, _) = FilterInstance::compile(rule);
        assert_eq!(
            (compiled.as_str(), compiled_filename),
            (*glob, *filename),
            "{rule} compiled to something else"
        );
        let braced = glob.contains('{') && !is_literal(glob);
        assert_eq!(
            FilterInstance::depth_range(glob, *filename, braced),
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
