// SPDX-License-Identifier: MIT
//
// LORE: tests of the literal prefix and suffix check, which upstream lacks.

//! Each construct the literal prefix and suffix stop at still matches as the
//! matcher reads it.

/// Globs over every construct the prefix and suffix stop at, and the paths each
/// matches.
const MATCHES: &[(&str, &[&str])] = &[
  ("top0/**/*.ext00", &["top0/a/b/x.ext00", "top0/x.ext00"]),
  ("*.tmp", &["x.tmp", ".tmp"]),
  ("abc", &["abc"]),
  ("x/y*", &["x/yz"]),
  ("a/b/**", &["a/b/c"]),
  ("!a*", &["!ab"]),
  ("**/foo", &["foo", "a/foo"]),
  ("{a,b}/x", &["b/x"]),
  ("[ab]c", &["bc"]),
  ("a?", &["ab"]),
  ("*.[ch]", &["x.c"]),
  ("[a-c]x", &["bx"]),
  ("a{b,c}", &["ac"]),
  ("a{b,c}d", &["acd"]),
  ("a/**", &["a/b"]),
  ("a/**/b", &["a/b", "a/x/b"]),
  // A `\` can stand for another byte: `\b` is a backspace.
  ("a\\b", &["a\u{8}"]),
  ("a\\bc", &["a\u{8}c"]),
  ("a\\*", &["a*"]),
  ("a\\*/x", &["a*/x"]),
  ("a\\\\b", &["a\\b"]),
  // An unclosed group ends a match with its last alternative.
  ("{x,ab", &["ab"]),
  ("a{bc", &["abc"]),
];

/// Each glob in [`MATCHES`] matches its paths, the prefix and suffix check
/// included.
#[test]
fn a_path_holding_the_literal_ends_still_matches() {
  for (glob, paths) in MATCHES {
    for path in *paths {
      assert!(glob_match::glob_match(glob, path), "{glob} does not match {path}");
    }
  }
}
