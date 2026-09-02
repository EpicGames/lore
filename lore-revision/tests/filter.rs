// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use lore_revision::filter::FilterInstance;
    use lore_revision::util::path::RelativePath;

    include!("helper.rs");

    #[test]
    fn empty_filter() {
        let filter = FilterInstance::default();
        assert!(!filter.excludes(&RelativePath::new(), false));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test String").expect("Path create"),
            false
        ));
    }

    #[test]
    fn simple_match() {
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/path")
            .expect("Failed filter setup");
        assert!(!filter.excludes(&RelativePath::new(), false));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test String").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("soMe/pAth").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("sOme").expect("Path create"),
            true
        ));
    }

    #[test]
    fn glob_match() {
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/path/*")
            .expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test String").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("sOme/paTh").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("sOme").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("sOme/pAth/teST").expect("Path create"),
            false
        ));
        // `some/path/*` does not match `some/path/test/sub` -- the asterisk does
        // not cross a separator -- but the path is still excluded, because
        // `some/path/test` is, and nothing below an excluded directory is in.
        //
        // Ground truth from git: with `.gitignore` holding `some/path/*`,
        // `git status` reports `some/path/test/sub` as ignored.
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test/sub").expect("Path create"),
            false
        ));

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/path/**")
            .expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test string").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("SOme/paTH/tESt").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("soME/path/t.st/Sub").expect("Path create"),
            true
        ));
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("soMe/PAth/test/sub/any/depth.file")
                    .expect("Path create"),
                false
            )
        );

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/path/**/*")
            .expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test string").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("somE/pATh/test").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("sOme/paTH/test/sub").expect("Path create"),
            true
        ));
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("somE/patH/test/sub/any/depth.file")
                    .expect("Path create"),
                false
            )
        );

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/[pa]?th/**/*")
            .expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test string").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/Asth/test").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("sOme/Path/test/sub").expect("Path create"),
            true
        ));
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("Some/PPth/test/sub/any/depth.file")
                    .expect("Path create"),
                false
            )
        );
    }

    #[test]
    fn last_match_wins() {
        let _execution = setup_test_execution();

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/[pa]?th/**/*")
            .expect("Failed filter setup");
        filter
            .add_inclusion("some/path/this/specific/**/*")
            .expect("Failed filter setup");
        filter
            .add_inclusion("some/path/this/specific/*")
            .expect("Failed filter setup");
        filter
            .add_exclusion("some/path/this/specific/is/excluded")
            .expect("Failed filter setup");
        filter
            .add_exclusion("some/sath/this/specific/is/excluded/*")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("soMe/paTH/this/Subdir").expect("Path create"),
            true
        ));
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path("some/PAth/this/SPecific/path")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path("some/PAth/this/spEcIfic/not/excluded")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path(
                    "sOe/path/this/spEciFic/not/excluded/file.txt"
                )
                .expect("Path create"),
                false
            )
        );
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("sOMe/path/tHIs/specific/is/eXCluded")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path(
                    "some/sath/THIs/specific/is/exCluDed/file.txt"
                )
                .expect("Path create"),
                false
            )
        );
    }

    /// An excluded directory on the way to a re-included file is excluded, and
    /// still descended.
    ///
    /// This is the split the filter is built around. `excludes` answers "is this
    /// path in the tree"; `should_descend` answers "must a walk look inside
    /// anyway". Before the split, the two were conflated: reaching
    /// `some/path/this/specific/file.txt` was arranged by injecting re-inclusion
    /// lines for each ancestor, which made `excludes` report the ancestors as
    /// *not excluded* -- an answer contradicting the rules the user wrote.
    ///
    /// Ground truth from git for the exclusion itself: with `.gitignore` holding
    /// `some/[pa]?th/**/*`, `git status` reports `some/path/this/x` and
    /// `some/path/x` as ignored. Git also reports
    /// `some/path/this/specific/file.txt` as ignored even with the `!` rule,
    /// because git prunes at the excluded directory and documents re-inclusion
    /// below one as impossible. Lore deliberately departs there -- see
    /// `tests/filter_gitignore.rs`.
    #[test]
    fn directory_reinclusion() {
        let _execution = setup_test_execution();

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/[pa]?th/**/*")
            .expect("Failed filter setup");
        filter
            .add_inclusion("some/path/this/specific/file.txt")
            .expect("Failed filter setup");

        // `some/[pa]?th/**/*` needs at least three components, so the first two
        // levels match nothing and stay in.
        for depth in ["some", "some/path"] {
            let at = RelativePath::new_from_initial_path(depth).expect("Path create");
            assert!(!filter.excludes(&at, true), "{depth} as a directory");
            assert!(!filter.excludes(&at, false), "{depth} as a file");
        }

        // From the third level down the exclusion bites, ancestors of the
        // re-included file included.
        for excluded in [
            "some/path/this",
            "sOme/paTH/this",
            "sOMe/PAth/that",
            "Some/Path/that",
            "some/path/this/specific",
            "some/Path/this/SPecific",
        ] {
            let at = RelativePath::new_from_initial_path(excluded).expect("Path create");
            assert!(filter.excludes(&at, true), "{excluded} as a directory");
            assert!(filter.excludes(&at, false), "{excluded} as a file");
        }

        // Excluded, but a walk still has to look inside the two that lead to the
        // re-included file -- and must not bother with the ones that do not.
        for (dir, descend) in [
            ("some/path/this", true),
            ("some/path/this/specific", true),
            ("sOMe/PAth/that", false),
        ] {
            let at = RelativePath::new_from_initial_path(dir).expect("Path create");
            let state = filter.exclusion_state(&at, true);
            assert_eq!(
                filter.should_descend(state, &at),
                descend,
                "should_descend({dir})"
            );
        }

        // The re-included file itself, reached through both excluded ancestors.
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path("some/PAth/this/SPecific/filE.txt")
                    .expect("Path create"),
                false
            )
        );
    }

    #[test]
    fn directory_but_not_files() {
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("some/path/**/test/")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("Some/pATh/test").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/foo.uasset").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("sOme/pATh/sub/tESt").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/sub/test").expect("Path create"),
            false
        ));
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("sOMe/path/Sub/aNOther/test")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path("some/path/sub/another/test")
                    .expect("Path create"),
                false
            )
        );

        filter
            .add_exclusion("second/path/**/test/*/")
            .expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("second/path/test").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("second/path/foo.uasset").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("second/path/sub/test").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("second/path/sub/test").expect("Path create"),
            false
        ));
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("Second/Path/sub/tEst/another")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path("second/path/sub/test/another")
                    .expect("Path create"),
                false
            )
        );
    }

    #[test]
    fn files_and_directory() {
        let mut filter = FilterInstance::default();
        filter.add_exclusion("test").expect("Failed filter setup");
        // Following the gitignore syntax rules when there is no end slash in the pattern,
        // it should match both files and directories
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/tESt").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/foo.uasset").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/sub/TEst").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/sub/teST").expect("Path create"),
            false
        ));
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("some/path/sub/another/tESt")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("some/path/sub/another/tesT")
                    .expect("Path create"),
                false
            )
        );

        let mut filter = FilterInstance::default();
        filter.add_exclusion("*").expect("Failed filter setup");
        // Following the gitignore syntax rules when there is no end slash in the pattern,
        // it should match both files and directories
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/foo.uasset").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/sub/test").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/sub/test").expect("Path create"),
            false
        ));
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("some/path/sub/another/test")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            filter.excludes(
                &RelativePath::new_from_initial_path("some/path/sub/another/test")
                    .expect("Path create"),
                false
            )
        );
    }

    #[test]
    fn root_file() {
        let mut filter = FilterInstance::default();
        filter.add_exclusion("/*").expect("Failed filter setup");
        // `/*` is anchored, so it matches only the top-level entries -- but that
        // excludes `some`, and nothing below an excluded directory is in.
        //
        // Ground truth from git: with `.gitignore` holding `/*`, `git status`
        // reports `some/path/test/x` as ignored.
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/foo.uasset").expect("Path create"),
            false
        ));
        // Following the gitignore syntax rules when there is no end slash in the pattern,
        // it should match both files and directories
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("sOme").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("somE").expect("Path create"),
            true
        ));

        let mut filter = FilterInstance::default();
        filter.add_exclusion("/test").expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/foo.uasset").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/sub/test").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/sub/test").expect("Path create"),
            false
        ));
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path("some/path/sub/another/test")
                    .expect("Path create"),
                true
            )
        );
        assert!(
            !filter.excludes(
                &RelativePath::new_from_initial_path("some/path/sub/another/test")
                    .expect("Path create"),
                false
            )
        );
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("sometest").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("sometest").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("tesT").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("tESt").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("sometest").expect("Path create"),
            true
        ));
    }

    #[test]
    fn directory_match() {
        let mut filter = FilterInstance::default();
        filter.add_exclusion("*test/").expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("tEst").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("pathtest").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("pathTesT").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/path/tESt").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("pathtester").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("pathtester").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("path/tester").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("path/tester").expect("Path create"),
            true
        ));

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("/*test/")
            .expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("tEst").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("pathtest").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("pathtEst").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/path/test").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("pathtester").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("pathtester").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("path/tester").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("path/tester").expect("Path create"),
            true
        ));
    }

    #[test]
    fn gitignore_example() {
        // From https://git-scm.com/docs/gitignore

        // For example, a pattern doc/frotz/ matches doc/frotz directory,
        // but not a/doc/frotz directory;
        // however frotz/ matches frotz and a/frotz that is a directory
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("doc/frotz/")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("doC/frotz").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/doc/frotz").expect("Path create"),
            true
        ));

        let mut filter = FilterInstance::default();
        filter.add_exclusion("frotz/").expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("fRotZ").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("A/frOtz").expect("Path create"),
            true
        ));

        // A leading "**" followed by a slash means match in all directories.
        // For example, "**/foo" matches file or directory "foo" anywhere,
        // the same as pattern "foo". "**/foo/bar" matches file or directory
        // "bar" anywhere that is directly under directory "foo"
        let mut filter = FilterInstance::default();
        filter.add_exclusion("**/foo").expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foO").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("Foo").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("somE/fOo").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("sOme/fOO").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/Foo/Foo").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("sOme/fOo/fOo").expect("Path create"),
            true
        ));

        let mut filter = FilterInstance::default();
        filter.add_exclusion("foo").expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("Foo").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("fOo").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/FoO").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/fOo").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/foo/foo").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/foo/foo").expect("Path create"),
            true
        ));

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("**/foo/bar")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("fOo/baR").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("Foo/bAr").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/foO/Bar").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/fOO/baR").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/foo/Foo/Bar").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("some/fOo/fOO/bAr").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/foo/too/bar").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("some/foo/too/bar").expect("Path create"),
            true
        ));

        // A trailing "/**" matches everything inside. For example,
        // "abc/**" matches all files inside directory "abc",
        // relative to the location of the .gitignore file, with infinite depth.
        let mut filter = FilterInstance::default();
        filter.add_exclusion("abc/**").expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("ABc/bar").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("aBC/bar").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("aBc/foo/bar").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("Abc/foo/bar").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("abc").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("abc").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("foo/abc").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("foo/abc").expect("Path create"),
            true
        ));

        // A slash followed by two consecutive asterisks then a slash matches
        // zero or more directories. For example, "a/**/b" matches "a/b",
        // "a/x/b", "a/x/y/b" and so on.
        let mut filter = FilterInstance::default();
        filter.add_exclusion("a/**/b").expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("a/B").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("A/b").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("A/x/B").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("a/x/B").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("a/x/y/B").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("A/x/y/b").expect("Path create"),
            true
        ));

        // Other consecutive asterisks are considered regular asterisks
        // and will match according to the previous rules.
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("a/foo**/b")
            .expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/b").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/b").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/x/b").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/x/b").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/x/y/b").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("a/foobar/b").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("a/foobar/b").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/foo/bar/b").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/foo/bar").expect("Path create"),
            false
        ));

        // The pattern hello.* matches any file or directory whose name
        // begins with hello.
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("hello.*")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("hello.com").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("hello.com").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("test/hello.").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("test/hello.").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("test/sub/hello.a").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("test/sub/hello.a").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test/sub/ahello.a").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("test/sub/ahello.a").expect("Path create"),
            true
        ));

        // If one wants to restrict this only to the directory and not
        // in its subdirectories, one can prepend the pattern with a
        // slash, i.e. /hello.*; the pattern now matches hello.txt,
        // hello.c but not a/hello.java.
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("/hello.*")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("hello.txt").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("hello.txt").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("hello.c").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("hello.c").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/hello.java").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/hello.java").expect("Path create"),
            true
        ));

        // The pattern foo/ will match a directory foo and paths underneath it,
        // but will not match a regular file or a symbolic link foo
        let mut filter = FilterInstance::default();
        filter.add_exclusion("foo/").expect("Failed filter setup");
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("foo").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/path").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/path").expect("Path create"),
            true
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/path/sub").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/path/sub").expect("Path create"),
            true
        ));

        // The pattern doc/frotz and /doc/frotz have the same effect in any
        // .gitignore file. In other words, a leading slash is not relevant
        // if there is already a middle slash in the pattern.
        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("doc/frotz")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("doc/frotz").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("doc/frotz").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/doc/frotz").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/doc/frotz").expect("Path create"),
            true
        ));

        let mut filter = FilterInstance::default();
        filter
            .add_exclusion("/doc/frotz")
            .expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("doc/frotz").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("doc/frotz").expect("Path create"),
            true
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/doc/frotz").expect("Path create"),
            false
        ));
        assert!(!filter.excludes(
            &RelativePath::new_from_initial_path("a/doc/frotz").expect("Path create"),
            true
        ));

        // The pattern foo/*, matches foo/test.json (a regular file),
        // foo/bar (a directory), but it does not match foo/bar/hello.c
        // (a regular file), as the asterisk in the pattern does not
        // match bar/hello.c which has a slash in it.
        let mut filter = FilterInstance::default();
        filter.add_exclusion("foo/*").expect("Failed filter setup");
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/test.json").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/bar").expect("Path create"),
            true
        ));
        // The gitignore documentation's point is about the *pattern*: `foo/*`
        // does not match `foo/bar/hello.c`, because the asterisk does not match
        // `bar/hello.c`. That is not the same as the path being included. Git
        // excludes `foo/bar`, never descends into it, and so ignores everything
        // underneath.
        //
        // Ground truth from git: with `.gitignore` holding `foo/*`, `git status`
        // reports `foo/bar/hello.c` as ignored.
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/bar/hello.c").expect("Path create"),
            false
        ));
        assert!(filter.excludes(
            &RelativePath::new_from_initial_path("foo/bar/hello.c").expect("Path create"),
            true
        ));
    }
}
