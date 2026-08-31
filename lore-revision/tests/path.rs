// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::path::Path;

    use lore_revision::util::path::RelativePath;
    use lore_revision::util::path::RelativePathBuf;
    use lore_revision::util::path::expand_path_ancestors;

    #[test]
    fn empty_path() {
        let path = RelativePath::new();
        assert_eq!(path.as_str().len(), 0);
        assert_eq!(path.len(), 0);
        assert!(path.is_empty());
    }

    #[test]
    fn push_path() {
        let mut buf = RelativePathBuf::new();
        buf.push("test");
        let path = buf.clone().freeze();
        assert_eq!(path.as_str(), "test");
        assert_eq!(path.len(), 4);

        buf.push("again");
        let path = buf.clone().freeze();
        assert_eq!(path.as_str(), "test/again");
        assert_eq!(path.len(), 10);

        buf.push("file");
        let path = buf.freeze();
        assert_eq!(path.as_str(), "test/again/file");
        assert_eq!(path.len(), 15);
    }

    #[test]
    fn pop_path() {
        let mut buf = RelativePathBuf::new();
        buf.push("test");
        buf.push("a");
        buf.push("path");
        let mut path = buf.freeze();
        assert_eq!(path.as_str(), "test/a/path");
        assert_eq!(path.len(), 11);

        path.pop();
        assert_eq!(path.as_str(), "test/a");
        assert_eq!(path.len(), 6);

        path.pop();
        assert_eq!(path.as_str(), "test");
        assert_eq!(path.len(), 4);

        path.pop();
        assert_eq!(path.as_str(), "");
        assert_eq!(path.len(), 0);

        path.pop();
        assert_eq!(path.as_str(), "");
        assert_eq!(path.len(), 0);
    }

    #[test]
    fn root() {
        let mut buf = RelativePathBuf::new();
        buf.push("test");
        buf.push("again");
        buf.push("file");
        let mut path = buf.freeze();
        assert_eq!(path.as_str(), "test/again/file");

        let root = path.root();
        assert_eq!(root, "test");

        path.pop();
        let root = path.root();
        assert_eq!(root, "test");

        path.pop();
        let root = path.root();
        assert_eq!(root, "test");

        path.pop();
        let root = path.root();
        assert_eq!(root, "");
    }

    /// `U+0130` folds to two scalars, so an ancestor holding one puts the
    /// separator below it at a different byte offset in the lowercase form than
    /// in the written one. A pop that took its offset from one string and applied
    /// it to the other would cut the wrong place.
    #[test]
    fn push_and_pop_below_a_component_whose_fold_is_longer() {
        let mut buf = RelativePathBuf::new();
        buf.push("MESH_\u{130}");
        buf.push("inner");
        assert_eq!(buf.as_str(), "MESH_\u{130}/inner");
        assert_eq!(buf.as_lowercase_str(), "mesh_i\u{307}/inner");
        assert_ne!(
            buf.as_str().rfind('/'),
            buf.as_lowercase_str().rfind('/'),
            "the two forms must disagree on where the separator is, or this proves nothing"
        );

        buf.pop();
        assert_eq!(buf.as_str(), "MESH_\u{130}");
        assert_eq!(buf.as_lowercase_str(), "mesh_i\u{307}");

        buf.push("after");
        assert_eq!(buf.as_str(), "MESH_\u{130}/after");
        assert_eq!(
            buf.as_lowercase_str(),
            "mesh_i\u{307}/after",
            "the sibling must not inherit bytes the fold left behind"
        );
    }

    #[test]
    fn pop_root() {
        let mut buf = RelativePathBuf::new();
        buf.push("test");
        buf.push("again");
        buf.push("file");
        let mut path = buf.freeze();
        assert_eq!(path.as_str(), "test/again/file");

        let root = path.root();
        assert_eq!(root, "test");

        let root = path.pop_root();
        assert_eq!(root, "test");

        let root = path.root();
        assert_eq!(root, "again");
        assert_eq!(path.as_str(), "again/file");

        let mut buf = path.into_buf();
        buf.push("another");
        buf.push("name");
        let mut path = buf.freeze();
        let root = path.root();
        assert_eq!(root, "again");
        assert_eq!(path.as_str(), "again/file/another/name");

        let root = path.pop_root();
        assert_eq!(root, "again");
        assert_eq!(path.as_str(), "file/another/name");

        let root = path.pop_root();
        assert_eq!(root, "file");
        assert_eq!(path.as_str(), "another/name");

        let root = path.pop_root();
        assert_eq!(root, "another");
        assert_eq!(path.as_str(), "name");

        let root = path.pop_root();
        assert_eq!(root, "name");
        assert!(path.is_empty());

        let root = path.pop_root();
        assert_eq!(root, "");
        assert!(path.is_empty());
    }

    #[test]
    fn overlaps() {
        assert!(
            RelativePath::new_from_initial_path("some")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/path").expect("Path init failed")
                )
        );
        assert!(
            !RelativePath::new_from_initial_path("something")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/path").expect("Path init failed")
                )
        );
        assert!(
            !RelativePath::new_from_initial_path("some")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("something/path")
                        .expect("Path init failed")
                )
        );
        assert!(
            RelativePath::new_from_initial_path("some/path")
                .expect("Path init failed")
                .overlaps(&RelativePath::new_from_initial_path("some").expect("Path init failed"))
        );
        assert!(
            !RelativePath::new_from_initial_path("something/path")
                .expect("Path init failed")
                .overlaps(&RelativePath::new_from_initial_path("some").expect("Path init failed"))
        );
        assert!(
            !RelativePath::new_from_initial_path("some/path")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("something").expect("Path init failed")
                )
        );
        assert!(
            RelativePath::new_from_initial_path("some/path")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/path").expect("Path init failed")
                )
        );
        assert!(
            RelativePath::new_from_initial_path("some/path")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/path/more")
                        .expect("Path init failed")
                )
        );
        assert!(
            RelativePath::new_from_initial_path("some/path/more")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/path").expect("Path init failed")
                )
        );
        assert!(
            !RelativePath::new_from_initial_path("some/path")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/paths").expect("Path init failed")
                )
        );
        assert!(
            !RelativePath::new_from_initial_path("some/paths")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/path").expect("Path init failed")
                )
        );
        assert!(
            !RelativePath::new_from_initial_path("some/path/a")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some/path/b").expect("Path init failed")
                )
        );
        assert!(
            !RelativePath::new_from_initial_path("subdir/another_text_file.txt")
                .expect("Path init failed")
                .overlaps(
                    &RelativePath::new_from_initial_path("some_text_file.txt")
                        .expect("Path init failed")
                )
        );

        // Test case that could lead to slicing into the middle of a unicode char
        assert!(
            !RelativePath::new_from_initial_path("😊")
                .expect("Path init failed")
                .overlaps(&RelativePath::new_from_initial_path("hi").expect("Path init failed"))
        );
    }

    /// Test the path ancestor expansion algorithm used in `realize_conflicts` (sync.rs)
    /// This algorithm processes sorted paths to avoid staging the same path component twice.
    #[test]
    fn test_expand_path_ancestors() {
        fn collect_staged_paths(input_paths: &[&str]) -> Vec<String> {
            let paths: Vec<RelativePath> = input_paths
                .iter()
                .map(|p| RelativePath::new_from_initial_path(p).expect("Path init failed"))
                .collect();

            expand_path_ancestors(paths)
                .map(|path| path.to_string())
                .collect()
        }

        // Test case 1: Simple shared prefix
        let staged = collect_staged_paths(&["a/b/c", "a/b/d"]);
        // After sort: ["a/b/c", "a/b/d"], process from end (reverse order)
        // Process "a/b/d": yield "a", "a/b", "a/b/d"
        // Process "a/b/c": skip "a", "a/b" (already covered), yield "a/b/c"
        assert_eq!(staged, vec!["a", "a/b", "a/b/d", "a/b/c"]);

        // Test case 2: Disjoint paths
        let staged = collect_staged_paths(&["a/b", "c/d"]);
        // After sort: ["a/b", "c/d"], process from end
        // Process "c/d": yield "c", "c/d"
        // Process "a/b": yield "a", "a/b" (no overlap with "c/d")
        assert_eq!(staged, vec!["c", "c/d", "a", "a/b"]);

        // Test case 3: Multiple shared prefixes at different levels
        let staged = collect_staged_paths(&["a/b/c", "a/b/d", "a/e"]);
        // After sort: ["a/b/c", "a/b/d", "a/e"], process from end
        // Process "a/e": yield "a", "a/e"
        // Process "a/b/d": skip "a" (covered), yield "a/b", "a/b/d"
        // Process "a/b/c": skip "a", "a/b" (covered), yield "a/b/c"
        assert_eq!(staged, vec!["a", "a/e", "a/b", "a/b/d", "a/b/c"]);

        // Test case 4: Single path
        let staged = collect_staged_paths(&["foo/bar/baz"]);
        assert_eq!(staged, vec!["foo", "foo/bar", "foo/bar/baz"]);

        // Test case 5: Empty input
        let staged = collect_staged_paths(&[]);
        assert!(staged.is_empty());
    }

    #[test]
    fn dedup_to_supersets() {
        fn dedup(inputs: &[&str]) -> Vec<String> {
            let paths: Vec<RelativePath> = inputs
                .iter()
                .map(|p| RelativePath::new_from_initial_path(p).expect("Path init failed"))
                .collect();
            RelativePath::dedup_to_supersets(paths)
                .into_iter()
                .map(|p| p.as_str().to_owned())
                .collect()
        }

        // Empty input — empty output.
        assert!(dedup(&[]).is_empty());

        // Single path — unchanged.
        assert_eq!(dedup(&["foo/bar"]), vec!["foo/bar"]);

        // Exact duplicates collapse.
        assert_eq!(dedup(&["foo/bar", "foo/bar"]), vec!["foo/bar"]);
        assert_eq!(dedup(&["a/b", "a/b", "a/b"]), vec!["a/b"],);

        // Child paths collapse into their parent.
        assert_eq!(dedup(&["a/b", "a/b/c"]), vec!["a/b"]);
        assert_eq!(dedup(&["a/b/c", "a/b"]), vec!["a/b"]);
        assert_eq!(dedup(&["a/b/c/d", "a/b/c", "a/b"]), vec!["a/b"]);

        // Sibling paths are preserved (and returned in lex order).
        assert_eq!(dedup(&["c/d", "a/b"]), vec!["a/b", "c/d"],);

        // Non-overlapping path prefixes that share a string prefix are NOT
        // collapsed — "a" only covers "a/..." not "a-foo" or "aa".
        assert_eq!(dedup(&["a", "a-foo", "aa"]), vec!["a", "a-foo", "aa"],);

        // The classic awkward sort-order case: "a", "a-foo", "a/x" — sorted
        // lexicographically "a-foo" lands between "a" and "a/x" because '-' <
        // '/'. "a/x" must still collapse into "a".
        assert_eq!(dedup(&["a", "a-foo", "a/x"]), vec!["a", "a-foo"],);

        // Root (".") collapses everything to the empty set — caller treats
        // this as "scan the entire repo".
        assert!(dedup(&[".", "foo/bar"]).is_empty());
        assert!(dedup(&["foo/bar", "."]).is_empty());
        assert!(dedup(&["."]).is_empty());
        assert!(dedup(&[""]).is_empty());
        assert!(dedup(&["foo", "bar", "."]).is_empty());

        // Mixed: duplicates + nested + siblings.
        assert_eq!(
            dedup(&["a/b", "a/b/c", "a/b", "x/y", "x/y/z", "x/y/z"]),
            vec!["a/b", "x/y"],
        );

        // Order of input does not affect the (sorted) output.
        assert_eq!(dedup(&["x/y/z", "a/b/c", "a/b", "x/y"]), vec!["a/b", "x/y"],);
    }

    /// The single-candidate covering scan is only correct because a path's
    /// subtree is contiguous in the order it scans. These are the inputs that
    /// break if that ordering regresses to a plain byte comparison: a sibling
    /// whose name sorts between a directory and its children, because the
    /// character it differs by is below `/`.
    #[test]
    fn dedup_to_supersets_collapses_across_separator_sort_order() {
        fn dedup(inputs: &[&str]) -> Vec<String> {
            let paths: Vec<RelativePath> = inputs
                .iter()
                .map(|p| RelativePath::new_from_initial_path(p).expect("Path init failed"))
                .collect();
            RelativePath::dedup_to_supersets(paths)
                .into_iter()
                .map(|p| p.as_str().to_owned())
                .collect()
        }

        // '!' 0x21, '-' 0x2D and '.' 0x2E all sort below the separator '/' 0x2F.
        assert_eq!(dedup(&["a/b", "a/b.c", "a/b/c"]), vec!["a/b", "a/b.c"]);
        assert_eq!(dedup(&["a/b/c", "a/b.c", "a/b"]), vec!["a/b", "a/b.c"]);
        assert_eq!(
            dedup(&["a", "a!x", "a-y", "a.z", "a/c"]),
            vec!["a", "a!x", "a-y", "a.z"]
        );
        assert_eq!(
            dedup(&["a/b", "a/b.1", "a/b.2", "a/b/c/d/e"]),
            vec!["a/b", "a/b.1", "a/b.2"]
        );
        assert_eq!(
            dedup(&["a/b", "a/b.x", "a/b/c", "m/n", "m/n.y", "m/n/o"]),
            vec!["a/b", "a/b.x", "m/n", "m/n.y"]
        );
        assert_eq!(
            dedup(&["a", "a.1", "a/b", "a/b.2", "a/b/c"]),
            vec!["a", "a.1"]
        );

        let leaves: Vec<String> = (0..64).map(|i| format!("p/{i:03}")).collect();
        let refs: Vec<&str> = leaves.iter().map(String::as_str).collect();
        assert_eq!(dedup(&refs), leaves);

        let mut with_parent = vec!["p"];
        with_parent.extend(refs.iter().copied());
        assert_eq!(dedup(&with_parent), vec!["p"]);
    }

    /// A file system that distinguishes case holds `Assets` and `assets` side by
    /// side, while one parent in the tree holds a single entry of a name. Paths
    /// that differ only in the case of a shared component are brought onto one
    /// variation, so what follows is never asked to create one entry twice.
    #[test]
    fn dedup_to_supersets_unifies_the_case_of_a_shared_component() {
        fn dedup(inputs: &[&str]) -> Vec<String> {
            let paths: Vec<RelativePath> = inputs
                .iter()
                .map(|p| RelativePath::new_from_initial_path(p).expect("Path init failed"))
                .collect();
            RelativePath::dedup_to_supersets(paths)
                .into_iter()
                .map(|p| p.as_str().to_owned())
                .collect()
        }

        // A shared directory in two case variations: both targets keep their own
        // leaf, under the one variation of the directory.
        assert_eq!(
            dedup(&["assets/second.file", "Assets/first.file"]),
            vec!["Assets/first.file", "Assets/second.file"]
        );

        // The same entry in two case variations is one target.
        assert_eq!(
            dedup(&["Assets/Rock.mesh", "assets/rock.mesh"]),
            vec!["Assets/Rock.mesh"]
        );

        // Each component is settled on its own, at whatever depth it disagrees.
        assert_eq!(
            dedup(&["a/B/x", "a/b/y", "A/b/z"]),
            vec!["a/B/x", "a/B/y", "a/B/z"]
        );

        // A directory that a target covers takes the covering variation with it.
        assert_eq!(dedup(&["Assets", "assets/first.file"]), vec!["Assets"]);

        // Collapse and unification interleaved: `a/B/c/x` is brought onto
        // `A/b/c` and then dropped as covered, and `A/B/d` onto the same case.
        assert_eq!(
            dedup(&["A/b/c", "a/B/c/x", "A/B/d"]),
            vec!["A/b/c", "A/b/d"]
        );

        // Names that differ by more than case are different entries and keep what
        // they were given.
        assert_eq!(dedup(&["a/B", "a/b2"]), vec!["a/B", "a/b2"]);
        assert_eq!(dedup(&["Foo/x", "bar/Y"]), vec!["Foo/x", "bar/Y"]);
    }

    /// Node lookup hashes names lowercased, so a path resolved from user input
    /// can differ in case from the stored paths it is compared against.
    /// `covers` alone would miss those; the prefix boundary still has to hold.
    #[test]
    fn covers_ignore_case() {
        fn path(value: &str) -> RelativePath {
            RelativePath::new_from_initial_path(value).expect("Path init failed")
        }

        assert!(!path("libs/Shared").covers(&"libs/shared/inner.txt"));

        assert!(path("libs/Shared").covers_ignore_case(&path("LIBS/shared/inner.txt")));
        assert!(!path("libs/Shared").covers_ignore_case(&path("libs/SHARED-extra/inner.txt")));
    }

    #[test]
    fn new_from_user_path() {
        let repository_path = "my_repo";
        let user_path = "my_repo/test";
        let relative_path = RelativePath::new_from_user_path(Path::new(repository_path), user_path)
            .expect("Failed to create user path");
        assert_eq!(relative_path.as_str(), "test");

        let repository_path = std::path::absolute("my_repo").expect("Failed to get absolute path");
        let user_path = "my_repo/test";
        let relative_path = RelativePath::new_from_user_path(repository_path.as_path(), user_path)
            .expect("Failed to create user path");
        assert_eq!(relative_path.as_str(), "test");

        let repository_path = "my_repo";
        let user_path = std::path::absolute("my_repo/test").expect("Failed to get absolute path");
        let user_path = user_path
            .to_str()
            .expect("Failed to convert path to string");

        let relative_path = RelativePath::new_from_user_path(Path::new(repository_path), user_path)
            .expect("Failed to create user path");
        assert_eq!(relative_path.as_str(), "test");

        // Test-only: any real absolute directory serves as the repository root.
        #[allow(clippy::disallowed_methods)]
        let repository_path = std::env::current_dir().expect("No current dir");
        let relative_path = RelativePath::new_from_user_path(&repository_path, "")
            .expect("Relative path from empty user path failed");
        assert_eq!(relative_path.as_str().len(), 0);
        assert_eq!(relative_path.len(), 0);
        assert!(relative_path.is_empty());

        let relative_path = RelativePath::new_from_user_path(&repository_path, ".")
            .expect("Relative path from empty user path failed");
        assert_eq!(relative_path.as_str().len(), 0);
        assert_eq!(relative_path.len(), 0);
        assert!(relative_path.is_empty());

        let relative_path = RelativePath::new_from_user_path(
            Path::new(&repository_path),
            &repository_path.join("test").display().to_string(),
        )
        .expect("Failed to create user path");
        assert_eq!(relative_path.as_str(), "test");
    }

    /// An absolute root in the form the platform holds one. A path the platform
    /// does not call absolute is resolved against the working directory instead,
    /// which is not what the tests below cover.
    #[cfg(target_os = "windows")]
    const ROOT: &str = r"C:\repo";
    #[cfg(not(target_os = "windows"))]
    const ROOT: &str = "/repo";

    fn relative_of(repository_path: &str, user_path: &str) -> String {
        RelativePathBuf::new_from_user_path(Path::new(repository_path), user_path)
            .expect("the path is under the repository")
            .as_str()
            .to_owned()
    }

    #[test]
    fn new_from_user_path_takes_the_repository_off_the_front() {
        assert_eq!(
            relative_of(ROOT, &format!("{ROOT}/Assets/Rock.mesh")),
            "Assets/Rock.mesh"
        );
        assert_eq!(
            relative_of(ROOT, &format!("{}/Assets", ROOT.to_uppercase())),
            "Assets",
            "the repository is matched whatever case it is named in"
        );
        assert_eq!(
            relative_of(&format!("{ROOT}/"), &format!("{ROOT}/Assets")),
            "Assets"
        );
        assert_eq!(
            relative_of(ROOT, &format!("{ROOT}//Assets//Rock.mesh")),
            "Assets/Rock.mesh"
        );
        assert!(relative_of(ROOT, ROOT).is_empty());
    }

    /// A repository named beyond ASCII is matched on the fold the rest of the
    /// system uses, which for these characters is the length it already has.
    #[test]
    fn new_from_user_path_matches_a_repository_named_beyond_ascii() {
        let root = format!("{ROOT}/\u{00c5}NGSTR\u{00d6}M");
        assert_eq!(relative_of(&root, &format!("{root}/Assets")), "Assets");
        assert_eq!(
            relative_of(&root, &format!("{ROOT}/\u{00e5}ngstr\u{00f6}m/Assets")),
            "Assets",
            "the repository is matched whatever case it is named in"
        );
    }

    /// A fold can change the length of a component, so the boundary between the
    /// repository and what is under it cannot be a byte offset taken from one
    /// form of the path and applied to the other.
    #[test]
    fn new_from_user_path_matches_a_repository_whose_fold_changes_its_length() {
        let root = format!("{ROOT}/\u{0130}\u{0130}");
        assert_eq!(
            relative_of(&root, &format!("{root}/Assets/Rock.mesh")),
            "Assets/Rock.mesh"
        );
    }

    /// The repository is an ancestor of the path or it is nothing to it. A
    /// directory whose name it merely begins is not one.
    #[test]
    fn new_from_user_path_rejects_a_path_under_a_longer_name() {
        let sibling = format!("{ROOT}sitory/Assets");
        assert!(RelativePathBuf::new_from_user_path(Path::new(ROOT), &sibling).is_err());
    }

    #[test]
    fn new_from_user_path_rejects_a_path_outside_the_repository() {
        let outside = ROOT.replace("repo", "elsewhere");
        assert!(RelativePathBuf::new_from_user_path(Path::new(ROOT), &outside).is_err());
    }

    // ========================================
    // COW (Copy-on-Write) Behavior Tests
    // ========================================
    // These tests verify COW behavior through observable effects only,
    // without accessing private fields

    #[test]
    fn cow_clone_preserves_content() {
        // Test that cloning preserves content
        let path1 = RelativePath::new_from_initial_path("foo/bar/baz").unwrap();
        let path2 = path1.clone();

        assert_eq!(path1.as_str(), "foo/bar/baz");
        assert_eq!(path2.as_str(), "foo/bar/baz");
        assert_eq!(path1.as_lowercase_str(), "foo/bar/baz");
        assert_eq!(path2.as_lowercase_str(), "foo/bar/baz");
    }

    #[test]
    fn cow_offset_independence_after_pop() {
        // Test that clones can have independent views after pop()
        let path1 = RelativePath::new_from_initial_path("foo/bar/baz").unwrap();
        let mut path2 = path1.clone();

        path2.pop();

        // Original unchanged, clone modified
        assert_eq!(path1.as_str(), "foo/bar/baz");
        assert_eq!(path2.as_str(), "foo/bar");
    }

    #[test]
    fn cow_offset_independence_multiple_pops() {
        // Test multiple clones with different pop levels
        let path1 = RelativePath::new_from_initial_path("a/b/c/d").unwrap();
        let mut path2 = path1.clone();
        let mut path3 = path1.clone();

        path2.pop();
        path3.pop().pop();

        assert_eq!(path1.as_str(), "a/b/c/d");
        assert_eq!(path2.as_str(), "a/b/c");
        assert_eq!(path3.as_str(), "a/b");
    }

    #[test]
    fn cow_lowercase_independence() {
        // Test that lowercase views maintain independence
        let path1 = RelativePath::new_from_initial_path("FOO/BAR/BAZ").unwrap();
        let mut path2 = path1.clone();

        path2.pop();

        assert_eq!(path1.as_str(), "FOO/BAR/BAZ");
        assert_eq!(path1.as_lowercase_str(), "foo/bar/baz");
        assert_eq!(path2.as_str(), "FOO/BAR");
        assert_eq!(path2.as_lowercase_str(), "foo/bar");
    }

    #[test]
    fn cow_offset_independence_after_pop_root() {
        // Test independence with pop_root()
        let path1 = RelativePath::new_from_initial_path("foo/bar/baz").unwrap();
        let mut path2 = path1.clone();

        let root = path2.pop_root();
        assert_eq!(root, "foo");

        assert_eq!(path1.as_str(), "foo/bar/baz");
        assert_eq!(path2.as_str(), "bar/baz");
    }

    #[test]
    fn cow_into_buf_preserves_content() {
        // Test that converting to buf preserves content
        let path = RelativePath::new_from_initial_path("foo/bar/baz").unwrap();
        let buf = path.into_buf();

        assert_eq!(buf.as_str(), "foo/bar/baz");
        assert_eq!(buf.as_lowercase_str(), "foo/bar/baz");
    }

    #[test]
    fn cow_into_buf_with_offset() {
        // Test that into_buf extracts visible portion with offset
        let mut buf = RelativePathBuf::new();
        buf.push("foo");
        buf.push("bar");
        buf.push("baz");
        let mut path = buf.freeze();

        path.pop_root();
        assert_eq!(path.as_str(), "bar/baz");

        let buf = path.into_buf();
        assert_eq!(buf.as_str(), "bar/baz");
    }

    #[test]
    fn cow_round_trip_conversion() {
        // Test that round-trip preserves path
        let mut buf = RelativePathBuf::new();
        buf.push("foo");
        buf.push("bar");
        buf.push("baz");
        let path = buf.freeze();

        let original_str = path.as_str().to_string();
        let round_trip = path.into_buf().freeze();

        assert_eq!(original_str, round_trip.as_str());
        assert_eq!("foo/bar/baz", round_trip.as_str());
    }

    #[test]
    fn cow_round_trip_with_pop() {
        // Test round-trip with pop operations
        let mut buf = RelativePathBuf::new();
        buf.push("foo");
        buf.push("bar");
        buf.push("baz");
        let mut path = buf.freeze();

        path.pop();
        assert_eq!(path.as_str(), "foo/bar");

        let round_trip = path.into_buf().freeze();
        assert_eq!(round_trip.as_str(), "foo/bar");
    }

    #[test]
    fn cow_join_creates_new_path() {
        // Test that join() creates a new independent path
        let path1 = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let path2 = path1.join("baz");

        // Verify content
        assert_eq!(path1.as_str(), "foo/bar");
        assert_eq!(path2.as_str(), "foo/bar/baz");

        // Modify path2 shouldn't affect path1
        let mut path2 = path2;
        path2.pop();
        assert_eq!(path1.as_str(), "foo/bar");
        assert_eq!(path2.as_str(), "foo/bar");
    }

    #[test]
    fn cow_empty_path_clone() {
        // Test that empty paths work correctly with clone
        let path1 = RelativePath::new();
        let path2 = path1.clone();

        assert!(path1.is_empty());
        assert!(path2.is_empty());
    }

    #[test]
    fn cow_relative_path_buf_operations() {
        // Test RelativePathBuf operations
        let mut buf = RelativePathBuf::new();
        buf.push("test");
        assert_eq!(buf.as_str(), "test");

        buf.push("again");
        assert_eq!(buf.as_str(), "test/again");

        buf.push("file");
        assert_eq!(buf.as_str(), "test/again/file");

        buf.clear();
        assert!(buf.is_empty());
    }

    // ========================================
    // name() and name_lowercase() Tests
    // ========================================

    #[test]
    fn name_multi_component_path() {
        let path = RelativePath::new_from_initial_path("foo/bar/baz.txt").unwrap();
        assert_eq!(path.name(), "baz.txt");

        let buf = RelativePathBuf::new_from_initial_path("foo/bar/baz.txt").unwrap();
        assert_eq!(buf.name(), "baz.txt");
    }

    #[test]
    fn name_single_component_path() {
        let path = RelativePath::new_from_initial_path("file.txt").unwrap();
        assert_eq!(path.name(), "file.txt");

        let buf = RelativePathBuf::new_from_initial_path("file.txt").unwrap();
        assert_eq!(buf.name(), "file.txt");
    }

    #[test]
    fn name_empty_path() {
        let path = RelativePath::new();
        assert_eq!(path.name(), "");

        let buf = RelativePathBuf::new();
        assert_eq!(buf.name(), "");
    }

    #[test]
    fn name_lowercase_mixed_case() {
        let path = RelativePath::new_from_initial_path("FOO/BAR/BaZ.TXT").unwrap();
        assert_eq!(path.name(), "BaZ.TXT");
        assert_eq!(path.name_lowercase(), "baz.txt");

        let buf = RelativePathBuf::new_from_initial_path("FOO/BAR/BaZ.TXT").unwrap();
        assert_eq!(buf.name(), "BaZ.TXT");
        assert_eq!(buf.name_lowercase(), "baz.txt");
    }

    #[test]
    fn name_lowercase_single_component() {
        let path = RelativePath::new_from_initial_path("FILE.TXT").unwrap();
        assert_eq!(path.name(), "FILE.TXT");
        assert_eq!(path.name_lowercase(), "file.txt");

        let buf = RelativePathBuf::new_from_initial_path("FILE.TXT").unwrap();
        assert_eq!(buf.name(), "FILE.TXT");
        assert_eq!(buf.name_lowercase(), "file.txt");
    }

    #[test]
    fn name_lowercase_empty_path() {
        let path = RelativePath::new();
        assert_eq!(path.name_lowercase(), "");

        let buf = RelativePathBuf::new();
        assert_eq!(buf.name_lowercase(), "");
    }

    // ========================================
    // parent() Tests
    // ========================================

    #[test]
    fn parent_multi_component_path() {
        let path = RelativePath::new_from_initial_path("foo/bar/baz").unwrap();
        assert_eq!(path.parent(), Some("foo/bar"));

        let buf = RelativePathBuf::new_from_initial_path("foo/bar/baz").unwrap();
        assert_eq!(buf.parent(), Some("foo/bar"));
    }

    #[test]
    fn parent_two_component_path() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        assert_eq!(path.parent(), Some("foo"));

        let buf = RelativePathBuf::new_from_initial_path("foo/bar").unwrap();
        assert_eq!(buf.parent(), Some("foo"));
    }

    #[test]
    fn parent_single_component_path() {
        let path = RelativePath::new_from_initial_path("foo").unwrap();
        assert_eq!(path.parent(), None);

        let buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        assert_eq!(buf.parent(), None);
    }

    #[test]
    fn parent_empty_path() {
        let path = RelativePath::new();
        assert_eq!(path.parent(), None);

        let buf = RelativePathBuf::new();
        assert_eq!(buf.parent(), None);
    }

    // ========================================
    // new_from_clean_parts() Tests
    // ========================================

    #[test]
    fn new_from_clean_parts_both_non_empty() {
        let path = RelativePath::new_from_clean_parts("foo/bar", "baz/qux");
        assert_eq!(path.as_str(), "foo/bar/baz/qux");

        let buf = RelativePathBuf::new_from_clean_parts("foo/bar", "baz/qux");
        assert_eq!(buf.as_str(), "foo/bar/baz/qux");
    }

    #[test]
    fn new_from_clean_parts_empty_root() {
        let path = RelativePath::new_from_clean_parts("", "baz/qux");
        assert_eq!(path.as_str(), "baz/qux");

        let buf = RelativePathBuf::new_from_clean_parts("", "baz/qux");
        assert_eq!(buf.as_str(), "baz/qux");
    }

    #[test]
    fn new_from_clean_parts_empty_tail() {
        let path = RelativePath::new_from_clean_parts("foo/bar", "");
        assert_eq!(path.as_str(), "foo/bar");

        let buf = RelativePathBuf::new_from_clean_parts("foo/bar", "");
        assert_eq!(buf.as_str(), "foo/bar");
    }

    #[test]
    fn new_from_clean_parts_both_empty() {
        let path = RelativePath::new_from_clean_parts("", "");
        assert_eq!(path.as_str(), "");
        assert!(path.is_empty());

        let buf = RelativePathBuf::new_from_clean_parts("", "");
        assert_eq!(buf.as_str(), "");
        assert!(buf.is_empty());
    }

    #[test]
    fn new_from_clean_parts_trailing_slash_on_root() {
        let path = RelativePath::new_from_clean_parts("foo/bar/", "baz");
        assert_eq!(path.as_str(), "foo/bar/baz");

        let buf = RelativePathBuf::new_from_clean_parts("foo/bar/", "baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");
    }

    #[test]
    fn new_from_clean_parts_leading_slash_on_tail() {
        let path = RelativePath::new_from_clean_parts("foo", "/bar/baz");
        assert_eq!(path.as_str(), "foo/bar/baz");

        let buf = RelativePathBuf::new_from_clean_parts("foo", "/bar/baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");
    }

    #[test]
    fn new_from_clean_parts_preserves_case() {
        let path = RelativePath::new_from_clean_parts("FOO", "BAR");
        assert_eq!(path.as_str(), "FOO/BAR");
        assert_eq!(path.as_lowercase_str(), "foo/bar");

        let buf = RelativePathBuf::new_from_clean_parts("FOO", "BAR");
        assert_eq!(buf.as_str(), "FOO/BAR");
        assert_eq!(buf.as_lowercase_str(), "foo/bar");
    }

    #[test]
    fn new_from_clean_parts_leading_slash_on_root_not_stripped() {
        // Leading slash on root is NOT stripped (parts are expected to be clean)
        let path = RelativePath::new_from_clean_parts("/foo", "bar");
        assert_eq!(path.as_str(), "/foo/bar");

        let buf = RelativePathBuf::new_from_clean_parts("/foo", "bar");
        assert_eq!(buf.as_str(), "/foo/bar");
    }

    #[test]
    fn new_from_clean_parts_trailing_slash_on_tail_not_stripped() {
        // Trailing slash on tail is NOT stripped (parts are expected to be clean)
        let path = RelativePath::new_from_clean_parts("foo", "bar/");
        assert_eq!(path.as_str(), "foo/bar/");

        let buf = RelativePathBuf::new_from_clean_parts("foo", "bar/");
        assert_eq!(buf.as_str(), "foo/bar/");
    }

    // ========================================
    // to_absolute_path() Tests
    // ========================================

    #[test]
    fn to_absolute_path_basic() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let abs = path.to_absolute_path("/repo");
        assert_eq!(abs, Path::new("/repo/foo/bar"));
    }

    #[test]
    fn to_absolute_path_empty() {
        let path = RelativePath::new();
        let abs = path.to_absolute_path("/repo");
        assert_eq!(abs, Path::new("/repo/"));
    }

    // ========================================
    // append_into_buf() Tests
    // ========================================

    #[test]
    fn append_into_buf_basic() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let buf = path.append_into_buf("_suffix");
        assert_eq!(buf.as_str(), "foo/bar_suffix");
    }

    #[test]
    fn append_into_buf_empty_suffix() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let buf = path.append_into_buf("");
        assert_eq!(buf.as_str(), "foo/bar");
    }

    #[test]
    fn append_into_buf_empty_path() {
        let path = RelativePath::new();
        let buf = path.append_into_buf("suffix");
        assert_eq!(buf.as_str(), "suffix");
    }

    #[test]
    fn append_into_buf_preserves_case() {
        let path = RelativePath::new_from_initial_path("FOO/BAR").unwrap();
        let buf = path.append_into_buf("_SUFFIX");
        assert_eq!(buf.as_str(), "FOO/BAR_SUFFIX");
        assert_eq!(buf.as_lowercase_str(), "foo/bar_suffix");
    }

    #[test]
    fn append_into_buf_does_not_consume_original() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let _buf = path.append_into_buf("_suffix");
        // Original path should still be usable
        assert_eq!(path.as_str(), "foo/bar");
    }

    #[test]
    fn append_into_buf_with_offset() {
        // Create a path with an offset (via pop_root)
        let mut path = RelativePath::new_from_initial_path("foo/bar/baz").unwrap();
        path.pop_root();
        assert_eq!(path.as_str(), "bar/baz");

        let buf = path.append_into_buf("_suffix");
        assert_eq!(buf.as_str(), "bar/baz_suffix");
    }

    // ========================================
    // push_into_buf() Tests
    // ========================================

    #[test]
    fn push_into_buf_basic() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let buf = path.push_into_buf("baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");
    }

    #[test]
    fn push_into_buf_empty_suffix() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let buf = path.push_into_buf("");
        assert_eq!(buf.as_str(), "foo/bar");
    }

    #[test]
    fn push_into_buf_empty_path() {
        let path = RelativePath::new();
        let buf = path.push_into_buf("suffix");
        assert_eq!(buf.as_str(), "suffix");
    }

    #[test]
    fn push_into_buf_preserves_case() {
        let path = RelativePath::new_from_initial_path("FOO/BAR").unwrap();
        let buf = path.push_into_buf("BAZ");
        assert_eq!(buf.as_str(), "FOO/BAR/BAZ");
        assert_eq!(buf.as_lowercase_str(), "foo/bar/baz");
    }

    #[test]
    fn push_into_buf_does_not_consume_original() {
        let path = RelativePath::new_from_initial_path("foo/bar").unwrap();
        let _buf = path.push_into_buf("baz");
        assert_eq!(path.as_str(), "foo/bar");
    }

    // ========================================
    // RelativePathBuf-specific Tests
    // ========================================

    #[test]
    fn relative_path_buf_pop() {
        let mut buf = RelativePathBuf::new_from_initial_path("foo/bar/baz").unwrap();
        assert_eq!(buf.as_str(), "foo/bar/baz");

        buf.pop();
        assert_eq!(buf.as_str(), "foo/bar");

        buf.pop();
        assert_eq!(buf.as_str(), "foo");

        buf.pop();
        assert_eq!(buf.as_str(), "");
        assert!(buf.is_empty());

        // Pop on empty should be safe
        buf.pop();
        assert!(buf.is_empty());
    }

    #[test]
    fn relative_path_buf_pop_preserves_lowercase() {
        let mut buf = RelativePathBuf::new_from_initial_path("FOO/BAR/BAZ").unwrap();
        assert_eq!(buf.as_lowercase_str(), "foo/bar/baz");

        buf.pop();
        assert_eq!(buf.as_str(), "FOO/BAR");
        assert_eq!(buf.as_lowercase_str(), "foo/bar");
    }

    #[test]
    fn relative_path_buf_root() {
        let buf = RelativePathBuf::new_from_initial_path("foo/bar/baz").unwrap();
        assert_eq!(buf.root(), "foo");

        let buf = RelativePathBuf::new_from_initial_path("single").unwrap();
        assert_eq!(buf.root(), "single");

        let buf = RelativePathBuf::new();
        assert_eq!(buf.root(), "");
    }

    #[test]
    fn relative_path_buf_overlaps() {
        let buf1 = RelativePathBuf::new_from_initial_path("foo/bar").unwrap();
        let buf2 = RelativePathBuf::new_from_initial_path("foo/bar/baz").unwrap();
        assert!(buf1.overlaps(&buf2));
        assert!(buf2.overlaps(&buf1));

        let buf3 = RelativePathBuf::new_from_initial_path("foo/baz").unwrap();
        assert!(!buf1.overlaps(&buf3));

        // Empty overlaps with everything
        let empty = RelativePathBuf::new();
        assert!(empty.overlaps(&buf1));
        assert!(buf1.overlaps(&empty));
    }

    #[test]
    fn relative_path_buf_append_and_freeze() {
        let buf = RelativePathBuf::new_from_initial_path("foo/bar").unwrap();
        let path = buf.append_and_freeze("_suffix");
        assert_eq!(path.as_str(), "foo/bar_suffix");
    }

    #[test]
    fn relative_path_buf_push_and_freeze() {
        let buf = RelativePathBuf::new_from_initial_path("foo/bar").unwrap();
        let path = buf.push_and_freeze("baz");
        assert_eq!(path.as_str(), "foo/bar/baz");
    }

    // ========================================
    // new_from_initial_path() Validation Tests
    // ========================================

    #[test]
    fn new_from_initial_path_rejects_dotdot() {
        assert!(RelativePath::new_from_initial_path("..").is_err());
        assert!(RelativePath::new_from_initial_path("../foo").is_err());
        assert!(RelativePathBuf::new_from_initial_path("..").is_err());
        assert!(RelativePathBuf::new_from_initial_path("../foo").is_err());
    }

    /// A step up is a step up wherever in the path it stands. A path that walks
    /// into a subdirectory first reaches just as far above the repository as one
    /// that starts with the steps, so the front of the path is not where this can
    /// be decided.
    #[test]
    fn new_from_initial_path_rejects_a_step_up_below_the_front() {
        for name in [
            "subdir/..",
            "subdir/../foo",
            "subdir/../../foo",
            "subdir/../foo/../..",
            "subdir/../../../something",
            "a/b/../../..",
            "a/b/c/../../../../outside",
            "subdir/./../../outside",
            "subdir/dir/..",
        ] {
            assert!(
                RelativePath::new_from_initial_path(name).is_err(),
                "{name:?} steps above the repository"
            );
            assert!(
                RelativePathBuf::new_from_initial_path(name).is_err(),
                "{name:?} steps above the repository"
            );
        }
    }

    /// The separator a step up is written with does not decide whether it is one,
    /// and neither does a leading separator or a repeated one. Each is brought
    /// onto the canonical form before the path is judged.
    #[test]
    fn new_from_initial_path_rejects_a_step_up_however_it_is_written() {
        for name in [
            r"..\foo",
            r"subdir\..\..\outside",
            r"\..\..\outside",
            r"subdir\\..\\..\\outside",
            "subdir//..//..//outside",
            "/../outside",
            "/subdir/../../outside",
            "./../outside",
            "././../outside",
        ] {
            assert!(
                RelativePath::new_from_initial_path(name).is_err(),
                "{name:?} steps above the repository"
            );
            assert!(
                RelativePathBuf::new_from_initial_path(name).is_err(),
                "{name:?} steps above the repository"
            );
        }
    }

    /// Only a component that is `..` entirely is a step up. A name that merely
    /// begins with two periods, or is made of them, is a name a repository can
    /// hold, and it is kept as it is at any depth.
    #[test]
    fn new_from_initial_path_keeps_a_name_beginning_with_two_periods() {
        for name in [
            "..filename",
            "..hidden.txt",
            "...",
            "....",
            "...filename",
            "..config/settings.ini",
            "subdir/..filename",
            "subdir/nested/..filename",
            "..a/..b/..c",
            "name..",
            "a../b..",
        ] {
            let path = RelativePath::new_from_initial_path(name)
                .unwrap_or_else(|_| panic!("{name:?} is a name, not a step up"));
            assert_eq!(path.as_str(), name);

            let buf = RelativePathBuf::new_from_initial_path(name)
                .unwrap_or_else(|_| panic!("{name:?} is a name, not a step up"));
            assert_eq!(buf.as_str(), name);
        }
    }

    /// A name beginning with two periods is not a step up even where the path also
    /// holds one, so the two are told apart within a single path rather than the
    /// whole path being judged by what it contains.
    #[test]
    fn new_from_initial_path_tells_a_step_up_from_a_name_in_one_path() {
        assert!(RelativePath::new_from_initial_path("..filename/../outside").is_err());
        assert!(RelativePath::new_from_initial_path("subdir/../..filename").is_err());

        let path = RelativePath::new_from_initial_path("..dir/..sub/..file")
            .expect("no component is a step up");
        assert_eq!(path.as_str(), "..dir/..sub/..file");
    }

    /// Nothing a constructed path holds is a step up, so no path handed to the
    /// filesystem reaches outside the repository by being resolved there.
    #[test]
    fn new_from_initial_path_leaves_no_step_up_in_the_path() {
        for name in [
            "..filename",
            "subdir/..filename",
            "...",
            "a/b/c",
            r"a\b\..c",
            "/a//b/",
            "./a/b",
        ] {
            let path = RelativePath::new_from_initial_path(name)
                .unwrap_or_else(|_| panic!("{name:?} is a path the repository can hold"));
            assert!(
                !path.as_str().split('/').any(|component| component == ".."),
                "{name:?} kept a step up as {:?}",
                path.as_str()
            );
        }
    }

    #[test]
    fn new_from_initial_path_rejects_drive_letter() {
        assert!(RelativePath::new_from_initial_path("C:").is_err());
        assert!(RelativePath::new_from_initial_path("C:/foo").is_err());
        assert!(RelativePath::new_from_initial_path("D:\\bar").is_err());
        assert!(RelativePathBuf::new_from_initial_path("C:").is_err());
        assert!(RelativePathBuf::new_from_initial_path("C:/foo").is_err());
    }

    #[test]
    fn new_from_initial_path_handles_empty_and_dot() {
        let path = RelativePath::new_from_initial_path("").unwrap();
        assert!(path.is_empty());

        let path = RelativePath::new_from_initial_path(".").unwrap();
        assert!(path.is_empty());

        let buf = RelativePathBuf::new_from_initial_path("").unwrap();
        assert!(buf.is_empty());

        let buf = RelativePathBuf::new_from_initial_path(".").unwrap();
        assert!(buf.is_empty());
    }

    // ========================================
    // Separator Handling Tests
    // ========================================

    #[test]
    fn new_from_initial_path_normalizes_backslashes() {
        let path = RelativePath::new_from_initial_path("foo\\bar\\baz").unwrap();
        assert_eq!(path.as_str(), "foo/bar/baz");

        let buf = RelativePathBuf::new_from_initial_path("foo\\bar\\baz").unwrap();
        assert_eq!(buf.as_str(), "foo/bar/baz");
    }

    #[test]
    fn new_from_initial_path_removes_double_slashes() {
        let path = RelativePath::new_from_initial_path("foo//bar//baz").unwrap();
        assert_eq!(path.as_str(), "foo/bar/baz");

        let buf = RelativePathBuf::new_from_initial_path("foo//bar//baz").unwrap();
        assert_eq!(buf.as_str(), "foo/bar/baz");
    }

    #[test]
    fn new_from_initial_path_trims_leading_trailing_slashes() {
        let path = RelativePath::new_from_initial_path("/foo/bar/").unwrap();
        assert_eq!(path.as_str(), "foo/bar");

        let buf = RelativePathBuf::new_from_initial_path("/foo/bar/").unwrap();
        assert_eq!(buf.as_str(), "foo/bar");
    }

    #[test]
    fn new_from_initial_path_removes_leading_dot_slash() {
        let path = RelativePath::new_from_initial_path("./foo/bar").unwrap();
        assert_eq!(path.as_str(), "foo/bar");

        let buf = RelativePathBuf::new_from_initial_path("./foo/bar").unwrap();
        assert_eq!(buf.as_str(), "foo/bar");
    }

    #[test]
    fn push_empty_string_is_noop() {
        let mut buf = RelativePathBuf::new_from_initial_path("foo/bar").unwrap();
        buf.push("");
        assert_eq!(buf.as_str(), "foo/bar");
    }

    #[test]
    fn append_empty_string_is_noop() {
        let mut buf = RelativePathBuf::new_from_initial_path("foo/bar").unwrap();
        buf.append("");
        assert_eq!(buf.as_str(), "foo/bar");
    }

    // ========================================
    // Case Sensitivity Tests
    // ========================================

    #[test]
    fn lowercase_str_unicode() {
        let path = RelativePath::new_from_initial_path("FÖLDER/FÏLE.TXT").unwrap();
        assert_eq!(path.as_str(), "FÖLDER/FÏLE.TXT");
        assert_eq!(path.as_lowercase_str(), "földer/fïle.txt");

        let buf = RelativePathBuf::new_from_initial_path("FÖLDER/FÏLE.TXT").unwrap();
        assert_eq!(buf.as_str(), "FÖLDER/FÏLE.TXT");
        assert_eq!(buf.as_lowercase_str(), "földer/fïle.txt");
    }

    #[test]
    fn push_preserves_and_lowercases() {
        let mut buf = RelativePathBuf::new();
        buf.push("FOO");
        buf.push("BAR");
        assert_eq!(buf.as_str(), "FOO/BAR");
        assert_eq!(buf.as_lowercase_str(), "foo/bar");
    }

    #[test]
    fn append_preserves_and_lowercases() {
        let mut buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        buf.append("_SUFFIX");
        assert_eq!(buf.as_str(), "foo_SUFFIX");
        assert_eq!(buf.as_lowercase_str(), "foo_suffix");
    }

    // ========================================
    // RelativePathBuf new_from_user_path Tests
    // ========================================

    #[test]
    fn relative_path_buf_new_from_user_path() {
        // Test-only: any real absolute directory serves as the repository root.
        #[allow(clippy::disallowed_methods)]
        let repository_path = std::env::current_dir().expect("No current dir");
        let buf = RelativePathBuf::new_from_user_path(&repository_path, "")
            .expect("Failed to create path");
        assert!(buf.is_empty());

        let buf = RelativePathBuf::new_from_user_path(&repository_path, ".")
            .expect("Failed to create path");
        assert!(buf.is_empty());

        let buf = RelativePathBuf::new_from_user_path(
            &repository_path,
            &repository_path.join("test").display().to_string(),
        )
        .expect("Failed to create path");
        assert_eq!(buf.as_str(), "test");
    }

    // ========================================
    // Suffix with Path Separators Tests
    // ========================================

    #[test]
    fn push_with_separator_in_suffix() {
        let mut buf = RelativePathBuf::new();
        buf.push("foo");
        buf.push("bar/baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");

        let mut buf = RelativePathBuf::new();
        buf.push("foo/bar/baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");

        // Push with leading separator
        let mut buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        buf.push("/bar");
        assert_eq!(buf.as_str(), "foo//bar");

        // Push with trailing separator
        let mut buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        buf.push("bar/");
        assert_eq!(buf.as_str(), "foo/bar/");
    }

    #[test]
    fn join_with_separator_in_suffix() {
        // RelativePathBuf::join
        let buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        let buf = buf.join("bar/baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");

        // RelativePath::join
        let path = RelativePath::new_from_initial_path("foo").unwrap();
        let path = path.join("bar/baz");
        assert_eq!(path.as_str(), "foo/bar/baz");

        // Join with multiple separators
        let path = RelativePath::new_from_initial_path("a").unwrap();
        let path = path.join("b/c/d/e");
        assert_eq!(path.as_str(), "a/b/c/d/e");

        // Preserves case
        let path = RelativePath::new_from_initial_path("FOO").unwrap();
        let path = path.join("BAR/BAZ");
        assert_eq!(path.as_str(), "FOO/BAR/BAZ");
        assert_eq!(path.as_lowercase_str(), "foo/bar/baz");
    }

    #[test]
    fn append_with_separator_in_suffix() {
        // Append does NOT add a separator, so separators in suffix become part of the string
        let mut buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        buf.append("/bar/baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");

        // Append to create path-like structure from single component
        let mut buf = RelativePathBuf::new_from_initial_path("prefix").unwrap();
        buf.append("_suffix/with/path");
        assert_eq!(buf.as_str(), "prefix_suffix/with/path");

        // Preserves case
        let mut buf = RelativePathBuf::new_from_initial_path("FOO").unwrap();
        buf.append("/BAR/BAZ");
        assert_eq!(buf.as_str(), "FOO/BAR/BAZ");
        assert_eq!(buf.as_lowercase_str(), "foo/bar/baz");
    }

    #[test]
    fn append_and_freeze_with_separator() {
        let buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        let path = buf.append_and_freeze("/bar/baz");
        assert_eq!(path.as_str(), "foo/bar/baz");
    }

    #[test]
    fn push_and_freeze_with_separator() {
        let buf = RelativePathBuf::new_from_initial_path("foo").unwrap();
        let path = buf.push_and_freeze("bar/baz");
        assert_eq!(path.as_str(), "foo/bar/baz");
    }

    #[test]
    fn append_into_buf_with_separator() {
        let path = RelativePath::new_from_initial_path("foo").unwrap();
        let buf = path.append_into_buf("/bar/baz");
        assert_eq!(buf.as_str(), "foo/bar/baz");
        assert_eq!(buf.as_lowercase_str(), "foo/bar/baz");
    }
}
