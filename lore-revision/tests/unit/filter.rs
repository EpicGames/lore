// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The arithmetic behind a [`RuleIndex`] answer, which nothing outside the
//! module can reach, and the rules [`FilterInstance::add_path_inclusion`]
//! produces. The behaviour of authored rules is asserted in `tests/filter.rs`,
//! except the subtree companion an escaped trailing `*` does not get.

use lore_revision::filter::*;
use lore_revision::util::path::RelativePath;

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

/// A filter re-includes below an excluded directory only through an inclusion that can land
/// below one: a name rule matches at any depth but re-opens no directory a rule excluded.
#[test]
fn a_filter_reincludes_only_through_an_inclusion_below_a_directory() {
    for (rules, reincludes) in [
        (&[][..], false),
        (&["/engine/", "*.tmp"][..], false),
        (&["/*", "!*.ini"][..], false),
        (&["/*", "!/game/saved/config/"][..], true),
        (&["/*", "!*/saved/config/"][..], true),
    ] {
        let mut filter = FilterInstance::default();
        for rule in rules {
            match rule.strip_prefix('!') {
                Some(inclusion) => filter.add_inclusion(inclusion).expect("include"),
                None => filter.add_exclusion(rule).expect("exclude"),
            }
        }
        assert_eq!(filter.reincludes(), reincludes, "{rules:?}");
    }
}

fn path(text: &str) -> RelativePath {
    RelativePath::new_from_initial_path(text).expect("a path")
}

/// A filter that excludes every path and re-includes `added` through
/// [`FilterInstance::add_path_inclusion`].
fn path_inclusion(added: &RelativePath) -> FilterInstance {
    let mut filter = FilterInstance::default();
    filter.add_exclusion("**").expect("exclude");
    filter.add_path_inclusion(added);
    filter
}

/// A path inclusion re-includes the path and everything below it, and no other
/// path its text matches as a glob or as a name, whatever its case and
/// characters, and at the top level too.
#[test]
fn a_path_inclusion_reincludes_its_path_alone() {
    for (added, others) in [
        ("dir/[x].txt", &["dir/x.txt"][..]),
        ("d/[x]/f", &["d/x/f"][..]),
        ("{guid}/[x].txt", &["guid/x.txt"][..]),
        ("**/f.txt", &["f.txt", "a/f.txt"][..]),
        ("dir/notes*", &["dir/notes", "dir/notesx"][..]),
        ("[x].txt", &["x.txt", "d/[x].txt"][..]),
        ("a{b,c}.txt", &["ab.txt", "d/a{b,c}.txt"][..]),
        ("Dir/\u{c4}[X].txt", &["dir/\u{e4}x.txt"][..]),
        ("!x/f.txt", &["x/f.txt"][..]),
        ("#x.txt", &["x.txt"][..]),
        (" x/f.txt", &["x/f.txt"][..]),
    ] {
        let filter = path_inclusion(&path(added));
        for kept in [added.to_lowercase(), format!("{added}/x")] {
            assert!(
                !filter.excludes(&path(&kept), false),
                "{added} does not re-include {kept}"
            );
        }
        for other in others {
            assert!(
                filter.excludes(&path(other), false),
                "{added} re-includes {other}"
            );
        }
    }
}

/// A path inclusion sends a walk towards its path and nowhere else, whatever
/// its first component holds: the index records every component as text.
#[test]
fn a_path_inclusion_bounds_the_walk_whatever_its_first_component_holds() {
    for first in ["{guid}", "[x]", "x]", "*", "?", "**", "a\\b"] {
        let filter = path_inclusion(&RelativePath::new_from_clean_parts(first, "f.txt"));
        let descends = |directory: &RelativePath| {
            let state = filter.child_exclusion_state(FilterState::ROOT, directory, true);
            filter.should_descend(state, directory)
        };
        assert!(
            descends(&RelativePath::new_from_clean_parts(first, "")),
            "the walk never reaches {first}/f.txt"
        );
        assert!(
            !descends(&path("engine")),
            "{first}/f.txt sends the walk into engine"
        );
        assert!(
            filter.excludes_subtree(&path("engine")),
            "{first}/f.txt holds engine open"
        );
    }
}

/// A path inclusion and a rule holding the same component text reach apart,
/// whichever is added first: `[x]` names the directory `[x]` in the path
/// `[x]/b/f`, and the directory `x` in the rule `/[x]/a/`.
#[test]
fn a_path_inclusion_and_a_rule_with_its_text_reach_apart() {
    for path_first in [true, false] {
        let mut filter = FilterInstance::default();
        filter.add_exclusion("**").expect("exclude");
        if path_first {
            filter.add_path_inclusion(&path("[x]/b/f"));
        }
        filter.add_inclusion("/[x]/a/").expect("include");
        if !path_first {
            filter.add_path_inclusion(&path("[x]/b/f"));
        }
        for kept in ["[x]/b/f/g", "x/a/g"] {
            assert!(
                !filter.excludes(&path(kept), false),
                "{kept} is excluded, the path inclusion added first: {path_first}"
            );
        }
    }
}

/// An authored rule ending in `*` gets no subtree companion, escaped or not:
/// with `**` and `!art/foo\*`, `art/foo*` comes back and nothing below it does.
#[test]
fn an_authored_rule_ending_in_an_escaped_star_gets_no_companion() {
    let mut filter = FilterInstance::default();
    filter.add_exclusion("**").expect("exclude");
    filter.add_inclusion("art/foo\\*").expect("include");
    assert!(
        !filter.excludes(&path("art/foo*"), false),
        "art/foo\\* does not re-include art/foo*"
    );
    assert!(
        filter.excludes(&path("art/foo*/x.png"), false),
        "art/foo\\* re-includes art/foo*/x.png"
    );
}

/// Every named mode other than [`FilterMode::KeepStaged`] keeps its bits through
/// [`FilterMode::at`], so no mode can share the staged bit.
#[test]
fn a_mode_keeps_every_bit_but_the_staged_one_through_at() {
    use bitflags::Flags;

    for flag in FilterMode::FLAGS {
        if flag.name() == "KeepStaged" {
            continue;
        }
        let mode = *flag.value();
        assert_eq!(mode.at(false), mode, "{}", flag.name());
        assert_eq!(
            (mode | FilterMode::KeepStaged).at(false),
            mode,
            "{}",
            flag.name()
        );
    }
}

/// A staged node passes the ignore slot, which still folds it, so a child holding nothing staged
/// inherits the exclusion. A whole-path query reads the staged mode as [`FilterMode::Full`].
#[test]
fn a_staged_node_passes_the_ignore_slot_and_its_unstaged_children_do_not() {
    let mut filter = Filter::default();
    filter
        .ignore
        .add_exclusion("bin/")
        .expect("Filter exclusion");
    let bin = path("bin");
    let child = path("bin/a.txt");

    let (states, excluded) =
        filter.child_excludes_tree(FilterStates::ROOT, &bin, true, FilterMode::STAGED.at(true));
    assert!(!excluded, "a staged directory passes the ignore slot");
    assert!(
        filter
            .child_excludes_tree(FilterStates::ROOT, &bin, true, FilterMode::STAGED.at(false))
            .1,
        "an unstaged directory does not"
    );
    assert!(
        filter
            .child_excludes_tree(states, &child, false, FilterMode::STAGED.at(false))
            .1,
        "an unstaged child of the staged directory inherits the exclusion"
    );
    assert!(
        !filter
            .child_excludes_tree(states, &child, false, FilterMode::STAGED.at(true))
            .1,
        "a staged child passes it"
    );
    assert!(filter.excludes_tree(&bin, true, FilterMode::STAGED));
}
