// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The arithmetic behind a [`RuleIndex`] answer, which nothing outside the
//! module can reach, the rules [`FilterInstance::add_path_inclusion`]
//! produces, and the verdicts of authored rules where they depend on which
//! lines a step visits. The other behaviour of authored rules is asserted in
//! `tests/filter.rs`, except the subtree companion an escaped trailing `*` does
//! not get.

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
        add_rules(&mut filter, rules);
        assert_eq!(filter.reincludes(), reincludes, "{rules:?}");
    }
}

/// Adds `rules` to `filter` in authored order, a leading `!` marking an
/// inclusion.
fn add_rules(filter: &mut FilterInstance, rules: &[&str]) {
    for rule in rules {
        match rule.strip_prefix('!') {
            Some(inclusion) => filter.add_inclusion(inclusion).expect("include"),
            None => filter.add_exclusion(rule).expect("exclude"),
        }
    }
}

fn path(text: &str) -> RelativePath {
    RelativePath::new_from_initial_path(text).expect("a path")
}

/// The filter `add` builds, alone, which a step scans, and padded with copies of
/// the exclusion `/padding`, which a step reads through the index once there are
/// [`INDEXED_LINES`] lines. With all of them before the lines `add` adds, those
/// are listed one at a time; with one fewer, the first of them lists every line;
/// and with all of them after, the padding does.
fn layouts(add: impl Fn(&mut FilterInstance)) -> [(&'static str, FilterInstance); 4] {
    [
        ("alone", 0, 0),
        ("after padding", INDEXED_LINES, 0),
        ("after padding one line short", INDEXED_LINES - 1, 0),
        ("before padding", 0, INDEXED_LINES),
    ]
    .map(|(layout, before, after)| {
        let mut filter = FilterInstance::default();
        for _ in 0..before {
            filter.add_exclusion("/padding").expect("exclude");
        }
        add(&mut filter);
        for _ in 0..after {
            filter.add_exclusion("/padding").expect("exclude");
        }
        (layout, filter)
    })
}

/// Asserts that `rules` exclude each of `excluded` and none of `included`, all
/// asked about as files, in every one of their [`layouts`].
fn assert_verdicts(rules: &[&str], excluded: &[&str], included: &[&str]) {
    for (layout, filter) in layouts(|filter| add_rules(filter, rules)) {
        for file in excluded {
            assert!(
                filter.excludes(&path(file), false),
                "{rules:?} {layout} keeps {file}"
            );
        }
        for file in included {
            assert!(
                !filter.excludes(&path(file), false),
                "{rules:?} {layout} drops {file}"
            );
        }
    }
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
        let added_path = path(added);
        for (layout, filter) in layouts(|filter| {
            filter.add_exclusion("**").expect("exclude");
            filter.add_path_inclusion(&added_path);
        }) {
            for kept in [added.to_lowercase(), format!("{added}/x")] {
                assert!(
                    !filter.excludes(&path(&kept), false),
                    "{added} {layout} does not re-include {kept}"
                );
            }
            for other in others {
                assert!(
                    filter.excludes(&path(other), false),
                    "{added} {layout} re-includes {other}"
                );
            }
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
/// whichever is added first, at a node narrow enough to scan and at one wide
/// enough to search, whose other edges sort before both: `[x]` names the
/// directory `[x]` in the path `[x]/b/f`, and the directory `x` in the rule
/// `/[x]/a/`.
#[test]
fn a_path_inclusion_and_a_rule_with_its_text_reach_apart() {
    for (path_first, siblings) in [
        (true, 0),
        (false, 0),
        (true, SCANNED_CHILDREN),
        (false, SCANNED_CHILDREN),
    ] {
        for (layout, filter) in layouts(|filter| {
            filter.add_exclusion("**").expect("exclude");
            for sibling in 0..siblings {
                filter
                    .add_inclusion(&format!("/{sibling:02}/f"))
                    .expect("include");
            }
            if path_first {
                filter.add_path_inclusion(&path("[x]/b/f"));
            }
            filter.add_inclusion("/[x]/a/").expect("include");
            if !path_first {
                filter.add_path_inclusion(&path("[x]/b/f"));
            }
        }) {
            for kept in ["[x]/b/f/g", "x/a/g"] {
                assert!(
                    !filter.excludes(&path(kept), false),
                    "{kept} is excluded {layout} beside {siblings} siblings, the path inclusion added first: {path_first}"
                );
            }
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

/// Lines listed under different leading components apply in authored order: a
/// name rule listed at the root and a rule listed under `a/b` each override
/// the other when authored after it.
#[test]
fn lines_under_different_prefixes_apply_in_authored_order() {
    let shared = ["/**", "!/a/**", "/a/b/c/**"];
    for name_rule_last in [false, true] {
        let mut rules = shared.to_vec();
        if name_rule_last {
            rules.extend(["!/a/b/*.tmp", "*.tmp"]);
        } else {
            rules.extend(["*.tmp", "!/a/b/*.tmp"]);
        }
        let (excluded, included) = if name_rule_last {
            (["z/x", "a/x.tmp", "a/b/c/x", "a/b/x.tmp"], ["a/x", "a/b/x"])
        } else {
            (
                ["z/x", "a/x.tmp", "a/b/c/x", "a/b/c/x.tmp"],
                ["a/x", "a/b/x.tmp"],
            )
        };
        assert_verdicts(&rules, &excluded, &included);
    }
}

/// Below an excluded directory, a line authored before the line that excluded it
/// re-includes nothing, whether it is listed at the root, as `!*/drop/y` is, or
/// under the directory's own components, as the `src/**` companion of `!/src`
/// is; and one authored after it does. `!/src/drop/z` keeps a fold going past
/// `src/drop`, so the step below it runs.
#[test]
fn the_floor_holds_for_every_list() {
    assert_verdicts(
        &["/**", "!/src", "!*/drop/y", "/src/*", "!/src/drop/z"],
        &["src/drop/y"],
        &["src", "src/drop/z"],
    );
    assert_verdicts(
        &["/**", "!/src", "/src/*", "!*/drop/y"],
        &[],
        &["src", "src/drop/y"],
    );
}

/// The floor below an excluded directory is the last line that excludes it,
/// though an earlier line excludes it too. With `/**`, `!/a*`, `!*/d/y` and
/// `/a/*`, `/a/*` is the last line to exclude `a/d`, so `!*/d/y`, authored
/// before it, re-includes nothing below `a/d` and a walk does not enter it, as
/// Git prunes there. Only `/**` excludes `ab/d`, so `!*/d/y` re-includes
/// `ab/d/y`, which Git leaves out by pruning at `ab/d`. A view and an ignore
/// file answer alike, folding a whole path and stepping down a walk, in every
/// layout.
///
/// Where several exclusions follow the floor, the last of them floors the step
/// below: `/a/d/` floors `a/d` past `!*/d/y*`, which follows `/a/*`. And a
/// later rule excluding a directory on the way stops a deep re-inclusion that
/// an earlier one would not.
#[test]
fn the_floor_is_the_last_line_that_excludes_a_directory() {
    let rules = ["/**", "!/a*", "!*/d/y", "/a/*"];
    assert_verdicts(&rules, &["a/d/y", "a/x"], &["ab/d/y"]);
    assert_verdicts(
        &["/**", "!/a*", "/a/*", "!*/d/y*", "/a/d/"],
        &["a/d/y"],
        &["ab/d/y"],
    );
    assert_verdicts(&["/*", "!/e/s/r/p/i", "/e/*/"], &["e/s/r/p/i"], &[]);
    assert_verdicts(&["/*", "/e/*/", "!/e/s/r/p/i"], &[], &["e/s/r/p/i"]);
    for mode in [FilterMode::View, FilterMode::Ignore] {
        for (layout, instance) in layouts(|filter| add_rules(filter, &rules)) {
            let mut filter = Filter::default();
            if mode == FilterMode::View {
                filter.view = instance;
            } else {
                filter.ignore = instance;
            }
            for (top, kept) in [("a", false), ("ab", true)] {
                let file = format!("{top}/d/y");
                assert_eq!(
                    !filter.excludes(&path(&file), false, mode),
                    kept,
                    "{mode:?} {layout} folds {file}"
                );
                let (top_states, _) =
                    filter.child_exclusion_states(FilterStates::ROOT, &path(top), true, mode);
                assert!(!top_states.excluded(mode), "{mode:?} {layout} steps {top}");
                let directory = path(&format!("{top}/d"));
                let (states, _) = filter.child_exclusion_states(top_states, &directory, true, mode);
                assert!(states.excluded(mode), "{mode:?} {layout} steps {top}/d");
                assert_eq!(
                    filter.should_descend(states, &directory, mode),
                    kept,
                    "{mode:?} {layout} descends into {top}/d"
                );
                let (states, _) = filter.child_exclusion_states(states, &path(&file), false, mode);
                assert_eq!(
                    !states.excluded(mode),
                    kept,
                    "{mode:?} {layout} steps {file}"
                );
            }
        }
    }
}

/// Re-inclusions added out of the order their text sorts in each reach their
/// own directory, so a fold descends into it, and no other, at a node narrow
/// enough to scan and at one wide enough to search.
#[test]
fn reinclusions_added_out_of_order_each_reach_their_directory() {
    for width in [4, SCANNED_CHILDREN + 4] {
        let names: Vec<String> = (0..width)
            .map(|index| format!("d{:02}", index * 7 % width))
            .collect();
        let rules: Vec<String> = std::iter::once("**".to_owned())
            .chain(names.iter().map(|name| format!("!/{name}/f")))
            .collect();
        let rules: Vec<&str> = rules.iter().map(String::as_str).collect();
        let included: Vec<String> = names.iter().map(|name| format!("{name}/f")).collect();
        let included: Vec<&str> = included.iter().map(String::as_str).collect();
        assert_verdicts(&rules, &["zz/f", "d00/g"], &included);
    }
}

/// A line applies to every path it matches whatever its first component holds,
/// and a line whose leading components are text applies to no path that starts
/// otherwise.
#[test]
fn a_line_applies_whatever_its_leading_components_hold() {
    for (rule, excluded, included) in [
        ("/a/x", &["a/x", "A/X"][..], &["ab/x", "b/a/x", "a"][..]),
        (
            "/Top/Sub/*.TMP",
            &["top/sub/x.tmp"][..],
            &["top/x.tmp", "top/subx/x.tmp"][..],
        ),
        ("/a*/x", &["a/x", "ab/x"][..], &["b/x"][..]),
        ("/a?/x", &["ab/x"][..], &["a/x"][..]),
        ("/a,b/*.c", &["a,b/x.c"][..], &["a/x.c"][..]),
        ("/a}/x*", &["a}/xy"][..], &["a/xy"][..]),
        ("/[ab]/x", &["b/x"][..], &["c/x"][..]),
        ("/{a,b}/x*", &["b/x", "a/xy"][..], &["c/x"][..]),
        ("/{a,b}/x", &["{a,b}/x"][..], &["b/x"][..]),
        ("/a\\*/x", &["a*/x"][..], &["ab/x"][..]),
        ("/x[a/b]y", &["xay"][..], &["x/y"][..]),
        ("**/a/b", &["a/b", "x/a/b"][..], &["a/x/b"][..]),
        ("*/saved", &["game/saved"][..], &["saved"][..]),
        ("/a/*/c", &["a/b/c"][..], &["b/b/c"][..]),
    ] {
        assert_verdicts(&[rule], excluded, included);
    }
}

/// A line whose literal prefix is deeper than the index lists it under still
/// applies to its own paths alone.
#[test]
fn a_line_with_a_deep_literal_prefix_applies_to_its_own_paths() {
    assert_verdicts(
        &["/a/b/c/d/e/f/g/h/i", "/a/b/c/d/e/f/g/h/*.tmp"],
        &["a/b/c/d/e/f/g/h/i", "a/b/c/d/e/f/g/h/x.tmp"],
        &[
            "a/b/c/d/e/f/g/h/j",
            "a/b/c/d/e/f/g/x/i",
            "a/b/c/d/e/f/g/x.tmp",
            "a/b/c/d/e/f/g/h/x/y.tmp",
        ],
    );
}

/// A path through every level the index lists lines at, the deepest one
/// included, gets the verdict of the line authored last, whether it is listed
/// at the root or at the deepest level.
#[test]
fn a_path_through_every_listed_level_visits_them_in_authored_order() {
    let levels = [
        "a",
        "a/b",
        "a/b/c",
        "a/b/c/d",
        "a/b/c/d/e",
        "a/b/c/d/e/f",
        "a/b/c/d/e/f/g",
        "a/b/c/d/e/f/g/h",
    ];
    let deep = "a/b/c/d/e/f/g/h/k.x";
    for root_last in [false, true] {
        let mut rules: Vec<String> = levels.iter().map(|level| format!("/{level}/*.y")).collect();
        let pair = ["*.x".to_owned(), "!/a/b/c/d/e/f/g/h/*.x".to_owned()];
        if root_last {
            rules.extend(pair.into_iter().rev());
        } else {
            rules.extend(pair);
        }
        let rules: Vec<&str> = rules.iter().map(String::as_str).collect();
        let (excluded, included): (&[&str], &[&str]) = if root_last {
            (&[deep, "a/b/k.y", "a/b/c/d/e/f/g/h/k.y"], &[])
        } else {
            (&["a/b/k.y", "a/b/c/d/e/f/g/h/k.y", "a/k.x"], &[deep])
        };
        assert_verdicts(&rules, excluded, included);
    }
}

/// A view of the shape a Perforce stream client view produces: everything
/// excluded, prefixes re-included, and under each an extension excluded except
/// in directories named like it.
#[test]
fn a_view_of_reincluded_prefixes_with_excluded_extensions() {
    let mut rules = vec!["/**".to_owned()];
    for prefix in ["/top0", "/top1/group0"] {
        rules.push(format!("!{prefix}/**"));
        for extension in ["ext0", "ext1"] {
            let glob = format!("{prefix}/**/*.{extension}");
            rules.extend([glob.clone(), format!("!{glob}/*"), format!("!{glob}/*/**")]);
        }
    }
    let rules: Vec<&str> = rules.iter().map(String::as_str).collect();
    assert_verdicts(
        &rules,
        &[
            "top0/m/a.ext0",
            "top0/a.ext1",
            "top1/x",
            "top1/group0/m/n/a.ext1",
            "top1/group1/a.src",
            "top2/a.src",
        ],
        &[
            "top0/m/a.src",
            "top0/m/a.ext0/b",
            "top0/m/a.ext0/b/c.ext2",
            "top1/group0/m/a.src",
            "top1/group0/a.ext1/b",
        ],
    );
}

/// Every named mode other than the keep bits, [`FilterMode::KeepTracked`] and
/// [`FilterMode::KeepStagedInView`], keeps its bits through [`FilterMode::at`], so no mode can
/// share a keep bit.
#[test]
fn a_mode_keeps_every_bit_but_the_keep_bits_through_at() {
    use bitflags::Flags;

    let keeps = FilterMode::KeepTracked | FilterMode::KeepStagedInView;
    for flag in FilterMode::FLAGS {
        let mode = *flag.value();
        if keeps.contains(mode) {
            continue;
        }
        assert_eq!(mode.at(false), mode, "{}", flag.name());
        assert_eq!((mode | keeps).at(false), mode, "{}", flag.name());
    }
}

/// A tracked node passes the ignore slot, which still folds it, so an untracked child inherits the
/// exclusion. A whole-path query, a descent and a question about any kind keep a tracked path's
/// ancestors too, since every one of them is tracked.
#[test]
fn a_tracked_node_passes_the_ignore_slot_and_its_untracked_children_do_not() {
    let mut filter = Filter::default();
    filter
        .ignore
        .add_exclusion("bin/")
        .expect("Filter exclusion");
    let bin = path("bin");
    let child = path("bin/a.txt");

    let (states, excluded) =
        filter.child_excludes_tree(FilterStates::ROOT, &bin, true, FilterMode::TRACKED.at(true));
    assert!(!excluded, "a tracked directory passes the ignore slot");
    assert!(
        filter
            .child_excludes_tree(
                FilterStates::ROOT,
                &bin,
                true,
                FilterMode::TRACKED.at(false)
            )
            .1,
        "an untracked directory does not"
    );
    assert!(
        filter
            .child_excludes_tree(states, &child, false, FilterMode::TRACKED.at(false))
            .1,
        "an untracked child of the tracked directory inherits the exclusion"
    );
    assert!(
        !filter
            .child_excludes_tree(states, &child, false, FilterMode::TRACKED.at(true))
            .1,
        "a tracked child passes it"
    );
    assert!(!filter.excludes_tree(&child, false, FilterMode::TRACKED));
    assert!(filter.excludes_tree(&child, false, FilterMode::TRACKED.at(false)));
    assert!(filter.should_descend(states, &bin, FilterMode::TRACKED));
    assert!(!filter.should_descend(states, &bin, FilterMode::TRACKED.at(false)));
    assert!(!filter.excludes_any_kind(states, &child, FilterMode::TRACKED));
    assert!(filter.excludes_any_kind(states, &child, FilterMode::TRACKED.at(false)));
}

/// The view slot keeps a node that holds a staged change in a comparison of trees and nowhere
/// else, and steps its children from the node's verdict, so a child holding nothing staged is
/// excluded below it, a tracked one marked dirty included.
#[test]
fn a_staged_node_passes_the_view_slot_only_in_a_comparison() {
    let mut filter = Filter::default();
    filter.view.add_exclusion("/*").expect("Filter exclusion");
    let engine = path("Engine");
    let child = path("Engine/a.txt");
    let staged = FilterMode::CHANGES.at(true).at_staged(true);

    let (states, excluded) = filter.child_excludes_tree(FilterStates::ROOT, &engine, true, staged);
    assert!(
        !excluded,
        "a staged directory passes the view in a comparison"
    );
    for mode in [
        FilterMode::CHANGES.at(false),
        FilterMode::CHANGES.at(true).at_staged(false),
        FilterMode::TRACKED.at(true),
    ] {
        assert!(
            filter
                .child_excludes_tree(FilterStates::ROOT, &engine, true, mode)
                .1,
            "{mode:?} keeps the directory"
        );
        assert!(
            filter.child_excludes_tree(states, &child, false, mode).1,
            "{mode:?} keeps the child of the staged directory"
        );
    }
    assert!(
        !filter.child_excludes_tree(states, &child, false, staged).1,
        "a staged child passes it"
    );
    assert!(!filter.excludes_tree(&child, false, FilterMode::CHANGES));
    assert!(filter.excludes_tree(&child, false, FilterMode::TRACKED));
}
