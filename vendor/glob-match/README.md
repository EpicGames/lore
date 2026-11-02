# glob-match

An extremely fast glob matching library with support for wildcards, character classes, and brace expansion.

* Linear time matching. No exponential backtracking.
* Zero allocations.
* No regex compilation. Matching occurs on the glob pattern in place.
* Support for capturing matched ranges of wildcards.
* Thousands of tests based on Bash and [micromatch](https://github.com/micromatch/micromatch).

## Example

```rust
use glob_match::glob_match;

assert!(glob_match("some/**/{a,b,c}/**/needle.txt", "some/path/a/to/the/needle.txt"));
```

Wildcard values can also be captured using the `glob_match_with_captures` function. This returns a `Vec` containing ranges within the path string that matched dynamic parts of the glob pattern. You can use these ranges to get slices from the original path string.

```rust
use glob_match::glob_match_with_captures;

let glob = "some/**/{a,b,c}/**/needle.txt";
let path = "some/path/a/to/the/needle.txt";
let result = glob_match_with_captures(glob, path)
  .map(|v| v.into_iter().map(|capture| &path[capture]).collect());

assert_eq!(result, vec!["path", "a", "to/the"]);
```

## Syntax

| Syntax  | Meaning                                                                                                                                                                                                                           |
| ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `?`     | LORE: Matches any single byte except `/`, the only path separator on every platform.                                                                                                                                              |
| `*`     | LORE: Matches zero or more characters, except `/`.                                                                                                                                                                                |
| `**`    | LORE: Matches zero or more characters, including path separators, only as a complete path segment, preceded by a `/` or the start of the pattern and followed by a `/` or its end. Anywhere else it is two `*`.                   |
| `[ab]`  | LORE: Matches one of the characters contained in the brackets, never a path separator. Character ranges, e.g. `[a-z]` are also supported. Use `[!ab]` or `[^ab]` to match any character _except_ those contained in the brackets. |
| `{a,b}` | Matches one of the patterns contained in the braces. Any of the wildcard characters can be used in the sub-patterns. Braces may be nested up to 10 levels deep.                                                                   |
| `!`     | LORE: A literal character, also at the start of the glob. Upstream negates the glob with a leading `!`.                                                                                                                           |
| `\`     | A backslash character may be used to escape any of the above special characters.                                                                                                                                                  |

## Benchmarks

```
globset                 time:   [35.176 µs 35.200 µs 35.235 µs]
glob                    time:   [339.77 ns 339.94 ns 340.13 ns]
glob_match              time:   [179.76 ns 179.96 ns 180.27 ns]
```

## Fuzzing

You can fuzz `glob-match` itself using `cargo fuzz`. See the
[Rust Fuzz Book](https://rust-fuzz.github.io/book/cargo-fuzz/setup.html) for
guidance on setup and installation. Follow the Rust Fuzz Book for information on
how to configure and run Fuzz steps.

After discovering artifacts, use `cargo fuzz fmt [target] [artifact-path]` to
get the original input back.

```sh
$ cargo fuzz fmt both_fuzz fuzz/artifacts/both_fuzz/slow-unit-LONG_HASH
Output of `std::fmt::Debug`:

Data {
    pat: "some pattern",
    input: "some input",
}
```
