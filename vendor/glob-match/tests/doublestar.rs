// SPDX-License-Identifier: MIT
//
// LORE: added with the `**` backtracking fix.

//! A `**` verdict must not depend on what it absorbs.
//!
//! Upstream 0.2.1 commits the `**` backtrack point on the first literal match
//! and cannot restore it, so whether a match is found depends on whether an
//! earlier segment happens to match the pattern after the `**`.
//!
//! Each case below is a pattern against two paths it cannot tell apart: the
//! pattern's decomposition is identical for both and only the spelling of the
//! segment `**` absorbs differs. A matcher that answers differently for the two
//! is wrong on one of them whichever way the glob is read, so these need no
//! reference implementation to judge.

/// The verdict is the same whichever name `**` absorbs.
///
/// Upstream answered `false` for the first of each pair and `true` for the
/// second.
#[test]
fn a_doublestar_verdict_ignores_the_name_it_absorbs() {
    // `**` takes one segment, then `*b` matches `b`.
    assert!(glob_match::glob_match("**/*b", "x/b"));
    assert!(glob_match::glob_match("**/*b", "b/b"));

    // `**` takes one segment, then `*a` matches `a`.
    assert!(glob_match::glob_match("**/*a", "x/a"));
    assert!(glob_match::glob_match("**/*a", "a/a"));

    // `a` matches `a`, `**` takes one segment, `*.tmp` matches `y.tmp`.
    assert!(glob_match::glob_match("a/**/*.tmp", "a/q/y.tmp"));
    assert!(glob_match::glob_match("a/**/*.tmp", "a/x.tmp/y.tmp"));
}

/// The cases upstream already answered `false` stay `false`, so restoring the
/// missed matches did not widen the pattern.
///
/// The middle two are upstream's own assertions, in `tests::globstars`.
#[test]
fn a_doublestar_still_rejects_what_it_should() {
    // Nothing `**` can absorb leaves `b` at the end.
    assert!(!glob_match::glob_match("**/*b", "b/a"));
    // `**` is a whole segment, so it cannot match part of `bb`.
    assert!(!glob_match::glob_match("a/**/b", "a/bb"));
    assert!(!glob_match::glob_match("a/**b**/c", "a/b/c/b/c"));
    // A trailing `**` needs a segment of its own.
    assert!(!glob_match::glob_match("a/**", "a"));
    // `*` is a whole segment here, and there is none to spare.
    assert!(!glob_match::glob_match("**/*/a", "a"));
}

/// A `**` followed only by literals never reached the defect and is answered by
/// the fast path alone.
#[test]
fn a_doublestar_followed_by_literals_is_unaffected() {
    assert!(glob_match::glob_match("**/foo", "foo"));
    assert!(glob_match::glob_match("**/foo", "a/b/foo"));
    assert!(!glob_match::glob_match("**/foo", "a/foo/b"));
}
