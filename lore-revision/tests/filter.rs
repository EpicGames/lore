// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use lore_revision::filter::FilterInstance;
    use lore_revision::filter::FilterMode;
    use lore_revision::filter::FilterState;
    use lore_revision::filter::FilterStates;
    use lore_revision::repository::TEMP_FILE_EXTENSION;
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

    /// The rules the encoding tests load, and the paths their verdicts are
    /// compared over.
    const ENCODED_RULES: &str =
        "# comment\n*.log\n/Intermediate\nengine/content\n!engine/content/keep\n";
    const ENCODED_PROBES: [&str; 5] = [
        "build.log",
        "Intermediate",
        "engine/content",
        "engine/content/keep",
        "engine/source",
    ];

    /// `text` in each encoding a filter file can arrive in, paired with a name
    /// for assertion messages.
    fn every_encoding(text: &str) -> Vec<(&'static str, Vec<u8>)> {
        fn utf8(mark: &[u8], text: &str) -> Vec<u8> {
            [mark, text.as_bytes()].concat()
        }
        fn utf16(mark: &[u8], text: &str, unit_bytes: fn(u16) -> [u8; 2]) -> Vec<u8> {
            let mut bytes = mark.to_vec();
            bytes.extend(text.encode_utf16().flat_map(unit_bytes));
            bytes
        }

        vec![
            ("utf8", utf8(&[], text)),
            ("utf8-bom", utf8(&[0xEF, 0xBB, 0xBF], text)),
            ("utf16le", utf16(&[], text, u16::to_le_bytes)),
            ("utf16le-bom", utf16(&[0xFF, 0xFE], text, u16::to_le_bytes)),
            ("utf16be", utf16(&[], text, u16::to_be_bytes)),
            ("utf16be-bom", utf16(&[0xFE, 0xFF], text, u16::to_be_bytes)),
        ]
    }

    fn probe_path(path: &str) -> RelativePath {
        RelativePath::new_from_initial_path(path).expect("Path create")
    }

    /// A filter file loads to the same rules in every encoding it can arrive in:
    /// UTF-8, UTF-16 LE and UTF-16 BE, each with or without a byte-order mark.
    ///
    /// A mark that reached the parser would arrive glued to the front of the
    /// first rule, and UTF-16 that was never transcoded would carry a NUL
    /// between every pair of characters. Either way the rules match nothing and
    /// nothing reports it, so verdicts are compared against the plain UTF-8
    /// baseline, and the baseline is asserted outright so agreeing on an empty
    /// filter cannot satisfy the comparison.
    #[test]
    fn rules_load_from_every_encoding() {
        let dir = TempDir::new("lore-filter-encoding-load-");
        let load = |name: &str, bytes: &[u8]| {
            let file = dir.child(name);
            test_file_write(&file, bytes);
            lore_revision::filter::load_filter(&file).expect("load")
        };

        let baseline = load("baseline", ENCODED_RULES.as_bytes());
        assert!(
            baseline.excludes(&probe_path("build.log"), false),
            "baseline filter does not apply `*.log`"
        );
        assert!(
            !baseline.excludes(&probe_path("engine/source"), false),
            "baseline filter excludes a path no rule names"
        );

        for (name, bytes) in every_encoding(ENCODED_RULES) {
            let filter = load(name, &bytes);
            assert_eq!(
                filter.lines.len(),
                baseline.lines.len(),
                "{name} loaded a different number of rules"
            );
            for probe in ENCODED_PROBES {
                for is_directory in [false, true] {
                    assert_eq!(
                        filter.excludes(&probe_path(probe), is_directory),
                        baseline.excludes(&probe_path(probe), is_directory),
                        "{name} disagrees with UTF-8 on {probe} (directory: {is_directory})"
                    );
                }
            }
        }
    }

    /// Rules naming a path outside ASCII load from every encoding, to the same
    /// verdicts. Mostly-ASCII rules are still detected without a mark, so this
    /// is the Unicode case that needs none. The baseline is asserted outright so
    /// agreeing on an empty filter cannot satisfy the comparison.
    #[test]
    fn unicode_rules_load_from_every_encoding() {
        const UNICODE_RULES: &str = "engine/内容/*.log\n";
        const UNICODE_PROBES: [&str; 3] = [
            "engine/内容/build.log",
            "engine/内容/keep.txt",
            "engine/source/build.log",
        ];

        let dir = TempDir::new("lore-filter-encoding-unicode-");
        let load = |name: &str, bytes: &[u8]| {
            let file = dir.child(name);
            test_file_write(&file, bytes);
            lore_revision::filter::load_filter(&file).expect("load")
        };

        let baseline = load("baseline", UNICODE_RULES.as_bytes());
        assert!(
            baseline.excludes(&probe_path("engine/内容/build.log"), false),
            "baseline filter does not apply the rule naming a path outside ASCII"
        );
        assert!(
            !baseline.excludes(&probe_path("engine/source/build.log"), false),
            "baseline filter excludes a path no rule names"
        );

        for (name, bytes) in every_encoding(UNICODE_RULES) {
            let filter = load(name, &bytes);
            for probe in UNICODE_PROBES {
                assert_eq!(
                    filter.excludes(&probe_path(probe), false),
                    baseline.excludes(&probe_path(probe), false),
                    "{name} disagrees with UTF-8 on {probe}"
                );
            }
        }
    }

    /// UTF-16 whose rules lie almost wholly outside ASCII has to carry a mark.
    ///
    /// Without one there is too little of the NUL pattern to name a byte order
    /// by, and the bytes spell valid UTF-8 of unrelated text: UTF-16 LE `你好\n`
    /// is the rule `` `O}Y `` and a NUL. The file is refused on that NUL rather
    /// than loaded as a rule it never held. Behind a mark the same rules load.
    #[test]
    fn non_ascii_utf16_rules_need_a_mark() {
        let dir = TempDir::new("lore-filter-encoding-non-ascii-");

        for (name, bytes) in every_encoding("你好\n") {
            let file = dir.child(name);
            test_file_write(&file, &bytes);
            let loaded = lore_revision::filter::load_filter(&file);

            if matches!(name, "utf16le" | "utf16be") {
                assert!(
                    loaded.is_err(),
                    "{name} loaded as {:?} instead of being refused",
                    String::from_utf8_lossy(&bytes)
                );
            } else {
                let filter = loaded.unwrap_or_else(|error| panic!("{name} was refused: {error}"));
                assert!(
                    filter.excludes(&probe_path("你好"), false),
                    "{name} loaded without the rule it names"
                );
            }
        }
    }

    /// A file that cannot be decoded is refused outright, rather than loading as
    /// the rules that happened to survive.
    ///
    /// A filter short a rule excludes less than the file asks for, and nothing
    /// downstream can tell that from a filter that matched everything it named,
    /// so the caller has to hear about it here.
    #[test]
    fn a_file_that_cannot_be_decoded_is_refused() {
        let dir = TempDir::new("lore-filter-encoding-refuse-");
        let mut truncated_utf16 = every_encoding(ENCODED_RULES)
            .into_iter()
            .find(|(name, _)| *name == "utf16le-bom")
            .expect("utf16le-bom case")
            .1;
        truncated_utf16.pop();

        let mut invalid_utf8 = b"*.log\n".to_vec();
        invalid_utf8.extend([0xC3, 0x28, b'\n']);

        let mut utf32 = vec![0xFF, 0xFE, 0x00, 0x00];
        utf32.extend(ENCODED_RULES.chars().flat_map(|c| (c as u32).to_le_bytes()));

        for (name, bytes) in [
            ("invalid utf8", invalid_utf8),
            ("truncated utf16", truncated_utf16),
            ("utf32", utf32),
        ] {
            let file = dir.child(name);
            test_file_write(&file, &bytes);
            assert!(
                lore_revision::filter::load_filter(&file).is_err(),
                "{name} loaded instead of being refused"
            );
        }
    }

    /// `save` writes UTF-8 with no byte-order mark, whatever encoding the rules
    /// were loaded from, so the file Lore writes back does not depend on the one
    /// it read.
    #[tokio::test]
    async fn save_writes_utf8_without_a_byte_order_mark() {
        let dir = TempDir::new("lore-filter-encoding-save-");
        let mut expected: Option<String> = None;

        for (name, bytes) in every_encoding(ENCODED_RULES) {
            let source = dir.child(name);
            test_file_write(&source, &bytes);
            let filter = lore_revision::filter::load_filter(&source).expect("load");

            let target = dir.child(&format!("{name}.saved"));
            lore_revision::filter::save(&filter, &target)
                .await
                .expect("save");
            let written = std::fs::read(&target).expect("read back");

            assert!(
                !written.starts_with(&[0xEF, 0xBB, 0xBF])
                    && !lore_revision::util::encoding::is_utf16_bom(&written),
                "{name} saved behind a byte-order mark"
            );
            let text = String::from_utf8(written)
                .unwrap_or_else(|_| panic!("{name} did not save as UTF-8"));
            assert!(!text.is_empty(), "{name} saved no rules");
            match &expected {
                Some(expected) => assert_eq!(&text, expected, "{name} saved different text"),
                None => expected = Some(text),
            }
        }
    }

    /// `parse_filter` reads the rules a load of the same bytes reads, so a
    /// caller that has the bytes already does not get a different filter for
    /// having read them itself.
    ///
    /// The split exists so absent and empty can be told apart: `load_filter`
    /// answers both with an empty filter, which is right where there is nothing
    /// to exclude and wrong where the file is the record of what is in view.
    #[test]
    fn parse_filter_reads_what_a_load_reads() {
        let dir = TempDir::new("lore-filter-parse-");
        let file = dir.child("view");
        test_file_write(&file, ENCODED_RULES.as_bytes());

        let loaded = lore_revision::filter::load_filter(&file).expect("load");
        let parsed = lore_revision::filter::parse_filter(
            ENCODED_RULES.as_bytes(),
            std::path::Path::new("view"),
        )
        .expect("parse");

        assert_eq!(parsed.lines.len(), loaded.lines.len());
        for probe in ENCODED_PROBES {
            for is_directory in [false, true] {
                assert_eq!(
                    parsed.excludes(&probe_path(probe), is_directory),
                    loaded.excludes(&probe_path(probe), is_directory),
                    "parsing disagrees with loading on {probe} (directory: {is_directory})"
                );
            }
        }

        assert!(
            lore_revision::filter::load_filter(dir.child("absent"))
                .expect("an absent filter loads")
                .lines
                .is_empty(),
            "an absent filter is still an empty one"
        );
    }

    /// Paths the covering tests fold over: directories at several depths, files
    /// under them, and a witness below a directory for every rule set in
    /// [`COVER_RULES`] -- without one, a rule wrongly left out of the index
    /// reports a subtree as covered and this list holds nothing to contradict
    /// it.
    const COVER_PATHS: &[&str] = &[
        "engine",
        "engine/content",
        "engine/content/asset.uasset",
        "engine/intermediate",
        "engine/intermediate/build.obj",
        "game",
        "game/maps",
        "game/maps/level.umap",
        "game/node_modules",
        "game/node_modules/package.json",
        "game/thumbs.db",
        "game/scratch.tmp",
        "some",
        "some/branch",
        "some/branch/path",
        "some/branch/path/leaf",
        "thumbs.db",
        // A brace alternative reaching through a separator, and a witness under
        // each of its two expansions.
        "alt",
        "alt/b",
        "alt/b/keep",
        "alt/b/keep/one.txt",
        "alt/c",
        "alt/c/d",
        "alt/c/d/keep",
        "alt/c/d/keep/two.txt",
    ];

    /// Rule sets spanning the shapes that decide a covering answer: an open
    /// re-inclusion, one with an exclusion nested under it, one with the
    /// exclusion somewhere else entirely, a literal name rule, a suffix rule, a
    /// `**` in the middle, and one of each at once.
    const COVER_RULES: &[&[&str]] = &[
        &["/*", "!/engine"],
        &["/*", "!/engine", "/engine/intermediate"],
        &["/*", "!/game", "/engine/intermediate"],
        &["Thumbs.db"],
        &["*.tmp"],
        &["/some/**/path"],
        &["/engine/content"],
        &["**/node_modules"],
        &["/*", "!/alt", "/alt/{b,c/d}/keep/*"],
        &[
            "/*",
            "!/engine",
            "!/game",
            "/engine/intermediate",
            "*.tmp",
            "Thumbs.db",
        ],
    ];

    /// A walk reaches what a braced inclusion re-includes, and no further.
    ///
    /// Each alternative is a separate path the rule names, and the index holds
    /// every one of them. Left out, the index reports no inclusion below the
    /// directory an alternative names, the walk prunes there, and the re-included
    /// content is never reached. Recorded at the group instead, every excluded
    /// directory in the repository looks as though it might hold something, which
    /// is what the last assertion here pins.
    #[test]
    fn a_braced_inclusion_is_reachable_and_bounded() {
        let mut filter = FilterInstance::default();
        filter.add_exclusion("**").expect("exclude");
        filter
            .add_inclusion("{game,engine}/content/*")
            .expect("include");

        let descends = |directory: &str| {
            let path = probe_path(directory);
            let state = filter.child_exclusion_state(FilterState::ROOT, &path, true);
            filter.should_descend(state, &path)
        };

        for named in ["game", "engine"] {
            assert!(
                descends(named),
                "the walk prunes at {named}, so nothing the rule names is reached"
            );
        }
        for kept in ["game/content/level.umap", "engine/content/asset.uasset"] {
            assert!(
                !filter.excludes(&probe_path(kept), false),
                "{kept} is not re-included"
            );
        }
        assert!(
            !descends("elsewhere"),
            "the walk descends where no alternative reaches"
        );
    }

    /// A rule compared as text keeps its braces, and its subtree companion does
    /// not.
    ///
    /// A glob carrying no `*?[]\\` is compared to the path rather than
    /// evaluated, so its braces are part of the name it matches and enumerating
    /// them would record paths it never names. The companion
    /// [`FilterInstance::add_inclusion`] generates is evaluated as a glob
    /// whatever the rule it carries was, so the same braces do alternate there
    /// and the index has to enumerate them or the walk loses the subtree.
    ///
    /// The two together are why the filter re-includes `alt/b/x` without
    /// re-including `alt/b`.
    #[test]
    fn a_literal_braced_rule_matches_its_own_text() {
        let mut exclusion = FilterInstance::default();
        exclusion.add_exclusion("/alt/{b,c}").expect("exclude");
        assert!(exclusion.excludes(&probe_path("alt/{b,c}"), true));
        for expansion in ["alt/b", "alt/c"] {
            assert!(
                !exclusion.excludes(&probe_path(expansion), true),
                "{expansion} is excluded by a rule that matches text alone"
            );
        }

        let mut inclusion = FilterInstance::default();
        inclusion.add_exclusion("**").expect("exclude");
        inclusion.add_inclusion("alt/{b,c}").expect("include");
        let alt = probe_path("alt");
        let state = inclusion.child_exclusion_state(FilterState::ROOT, &alt, true);
        assert!(
            inclusion.should_descend(state, &alt),
            "the walk never reaches what the companion re-includes"
        );
        assert!(!inclusion.excludes(&probe_path("alt/{b,c}"), true));
        assert!(!inclusion.excludes(&probe_path("alt/b/x"), false));
        assert!(inclusion.excludes(&probe_path("alt/b"), true));
    }

    /// A filter holding `rules` in its view slot, built through the file they
    /// would be authored in so the parser is exercised too.
    fn view_filter(dir: &TempDir, name: &str, rules: &[&str]) -> lore_revision::filter::Filter {
        let file = dir.child(name);
        test_file_write(&file, rules.join("\n").as_bytes());
        lore_revision::filter::load_view(&file).expect("load view")
    }

    /// Whether `filter` reports `directory` and everything under it as included.
    fn covers(filter: &lore_revision::filter::Filter, directory: &str) -> bool {
        let path = probe_path(directory);
        let states = filter.exclusion_states(&path);
        let depth = directory.split('/').count() as u32;
        filter.covers_subtree(states, &path, depth, FilterMode::View)
    }

    /// `covers_subtree` may only call a directory covered when nothing below it
    /// is excluded.
    ///
    /// That is the direction which has to hold: a walk comparing two filters
    /// takes a subtree both of them cover without descending into it, so a
    /// wrong "covered" drops every change inside it silently. The other
    /// direction only costs traversal.
    ///
    /// Held against `FilterInstance::excludes`, which folds every ancestor on
    /// every call and consults no index, the way the rest of these tests hold
    /// the memoized answers against it.
    #[test]
    fn a_covered_subtree_holds_nothing_excluded() {
        let dir = TempDir::new("lore-filter-covers-");
        let mut covered = 0;
        for (index, rules) in COVER_RULES.iter().enumerate() {
            let filter = view_filter(&dir, &format!("view{index}"), rules);
            for directory in COVER_PATHS {
                if !covers(&filter, directory) {
                    continue;
                }
                covered += 1;
                for candidate in COVER_PATHS {
                    let Some(rest) = candidate.strip_prefix(directory) else {
                        continue;
                    };
                    if !rest.starts_with('/') {
                        continue;
                    }
                    for is_directory in [false, true] {
                        assert!(
                            !filter.view.excludes(&probe_path(candidate), is_directory),
                            "{rules:?} covers {directory} but excludes {candidate} (directory: {is_directory})"
                        );
                    }
                }
            }
        }
        assert!(
            covered > 0,
            "no rule set covered any directory, so the comparison asserted nothing"
        );
    }

    /// The covering answer for each bound that produces it, named so a
    /// regression says which one broke.
    #[test]
    fn covering_answers_each_bound_that_decides_it() {
        let dir = TempDir::new("lore-filter-covers-bounds-");

        // `*` is the only exclusion and it cannot reach past the first level,
        // so the re-included subtree is covered whole.
        assert!(covers(
            &view_filter(&dir, "open", &["/*", "!/engine"]),
            "engine"
        ));

        // The trie finds `engine/intermediate` below `engine`.
        assert!(!covers(
            &view_filter(&dir, "nested", &["/*", "!/engine", "/engine/intermediate"]),
            "engine"
        ));

        // `*` keeps its own one-component bound whatever `engine/intermediate`
        // needs: a single depth for the whole index would lend `*` the longer
        // rule's reach and lose this prune everywhere.
        assert!(covers(
            &view_filter(
                &dir,
                "per-marker",
                &["/*", "!/game", "/engine/intermediate"]
            ),
            "game"
        ));

        // A literal name rule is bounded by no prefix and no depth, so it can
        // catch a file anywhere and nothing is covered. Recording it is the one
        // place the two polarities are fed differently -- as an inclusion it is
        // deliberately left out -- and leaving it out here would report a
        // subtree holding `game/thumbs.db` as untouched.
        assert!(!covers(&view_filter(&dir, "name", &["Thumbs.db"]), "game"));

        // A `**` anywhere in a rule leaves its depth unbounded.
        assert!(!covers(
            &view_filter(&dir, "globstar", &["/some/**/path"]),
            "some"
        ));

        // A brace alternative can hold a separator, so the group's text names no
        // component and the trie cannot follow it. Each alternative is indexed
        // in its own right: neither of the two the rule names is covered, and a
        // sibling neither reaches is, which is the precision enumerating them
        // buys over marking the group.
        let braced = view_filter(&dir, "brace", &["/*", "!/alt", "/alt/{b,c/d}/keep/*"]);
        assert!(
            braced
                .view
                .excludes(&probe_path("alt/c/d/keep/two.txt"), false)
        );
        assert!(!covers(&braced, "alt/c"));
        assert!(!covers(&braced, "alt/b"));
        assert!(covers(&braced, "alt/untouched"));

        // Nothing to exclude at all covers everything, the root included.
        let open = view_filter(&dir, "empty", &[]);
        assert!(covers(&open, "engine"));
        assert!(open.covers_subtree(
            FilterStates::ROOT,
            &RelativePath::new(),
            0,
            FilterMode::View
        ));
    }

    /// The root is not covered while anything at all is excluded, since
    /// everything is below it.
    ///
    /// The one path with no components, and so the one whose depth a caller
    /// counting components would get wrong.
    #[test]
    fn the_root_is_not_covered_while_anything_is_excluded() {
        let dir = TempDir::new("lore-filter-covers-root-");
        for rules in COVER_RULES {
            let filter = view_filter(&dir, "view", rules);
            assert!(
                !filter.covers_subtree(
                    FilterStates::ROOT,
                    &RelativePath::new(),
                    0,
                    FilterMode::View
                ),
                "{rules:?} reports the whole repository as covered"
            );
        }
    }

    /// A slot outside the mode cannot object, so an empty mode covers
    /// everything.
    ///
    /// What makes a walk that asks for no filtering -- the merge graft walk --
    /// answer the question deliberately rather than by accident of which slots
    /// happen to hold rules.
    #[test]
    fn an_unconsulted_slot_covers_everything() {
        let dir = TempDir::new("lore-filter-covers-mode-");
        let filter = view_filter(&dir, "view", &["/*", "!/engine", "/engine/intermediate"]);
        let path = probe_path("engine");
        let states = filter.exclusion_states(&path);
        assert!(!filter.covers_subtree(states, &path, 1, FilterMode::View));
        assert!(filter.covers_subtree(states, &path, 1, FilterMode::empty()));
    }

    /// An ignore filter covers nothing, so a caller comparing two views has to
    /// ask with `View` alone.
    ///
    /// `load` adds `.lore`, `.urc`, the merge artifacts and the temporary
    /// extension as name rules, and a name rule matches at any depth, so one
    /// reaches below every path there is. Including `Ignore` in the mode would
    /// answer `false` everywhere and silently cost a two-view walk every prune
    /// it has.
    #[test]
    fn an_ignore_filter_covers_nothing() {
        let dir = TempDir::new("lore-filter-covers-ignore-");
        let ignore = dir.child("ignore");
        test_file_write(&ignore, b"");
        let view = dir.child("view");
        test_file_write(&view, b"/*\n!/game\n");
        let filter = lore_revision::filter::load(&ignore, &view).expect("load");

        for directory in ["game", "game/maps"] {
            let path = probe_path(directory);
            let states = filter.exclusion_states(&path);
            let depth = directory.split('/').count() as u32;
            assert!(
                filter.covers_subtree(states, &path, depth, FilterMode::View),
                "the view covers {directory}"
            );
            for mode in [FilterMode::Ignore, FilterMode::Full] {
                assert!(
                    !filter.covers_subtree(states, &path, depth, mode),
                    "{mode:?} reports {directory} as covered"
                );
            }
        }
    }

    /// `with_view` must not let the new view read the old one's folded ancestor
    /// verdicts.
    ///
    /// Clones share the memo and it revalidates on the slots' line counts
    /// alone, so two views of equal length are the case that would go
    /// unnoticed: the stale entry carries a `decided_at` naming a line in the
    /// filter that is gone, and the replacement answers for a fold it never
    /// made.
    #[test]
    fn with_view_does_not_inherit_folded_verdicts() {
        let dir = TempDir::new("lore-filter-with-view-memo-");
        let first = view_filter(&dir, "first", &["/*", "!/game"]);

        let mut second = FilterInstance::default();
        second.add_exclusion("/*").expect("exclude");
        second.add_inclusion("/engine").expect("include");
        assert_eq!(
            second.lines.len(),
            first.view.lines.len(),
            "the two views have to be the same length to be the case this tests"
        );

        let probe = probe_path("game/level.umap");
        assert!(
            !first.excludes(&probe, false, FilterMode::View),
            "the first view keeps game, so there is a folded verdict to inherit"
        );

        assert!(
            first
                .with_view(second)
                .excludes(&probe, false, FilterMode::View),
            "the replacement view excludes game, so the fold cannot have come from the first"
        );
    }

    /// `with_view` carries the ignore slot over.
    ///
    /// Dropping it would stop excluding `.lore`, the merge artifacts and the
    /// temporary extension that `load` adds to every ignore filter, and nothing
    /// would report it until something staged them.
    #[test]
    fn with_view_keeps_the_ignore_rules() {
        let dir = TempDir::new("lore-filter-with-view-ignore-");
        let ignore = dir.child("ignore");
        test_file_write(&ignore, b"/build\n");
        let view = dir.child("view");
        test_file_write(&view, b"/*\n!/game\n");
        let filter = lore_revision::filter::load(&ignore, &view).expect("load");

        let mut replacement = FilterInstance::default();
        replacement.add_exclusion("/*").expect("exclude");
        let swapped = filter.with_view(replacement);

        for path in [".lore", ".urc", "build"] {
            assert!(
                swapped.excludes(&probe_path(path), true, FilterMode::Ignore),
                "the ignore slot no longer excludes {path}"
            );
        }
        assert!(
            swapped.excludes(
                &probe_path(&format!("game/asset{TEMP_FILE_EXTENSION}")),
                false,
                FilterMode::Ignore
            ),
            "the ignore slot no longer excludes the temporary extension"
        );
    }

    /// The file `save` builds the new rules in, named as the implementation
    /// names it so these tests fail if it stops writing through a sibling at all.
    fn temp_sibling_of(path: &std::path::Path) -> std::path::PathBuf {
        let mut temp = path.as_os_str().to_owned();
        temp.push(lore_revision::repository::TEMP_FILE_EXTENSION);
        std::path::PathBuf::from(temp)
    }

    /// A save that succeeds leaves the target and nothing else, the sibling
    /// having been renamed onto it rather than copied.
    #[tokio::test]
    async fn save_leaves_no_temporary_file_behind() {
        let dir = TempDir::new("lore-filter-save-clean-");
        let target = dir.child("rules");
        test_file_write(&target, ENCODED_RULES.as_bytes());
        let filter = lore_revision::filter::load_filter(&target).expect("load");

        lore_revision::filter::save(&filter, &target)
            .await
            .expect("save");

        let temporary = temp_sibling_of(&target);
        assert!(
            !temporary.exists(),
            "the save left {} behind",
            temporary.display()
        );
    }

    /// A save that fails leaves the previous rules.
    ///
    /// The target is never the file being written, so it never holds part of the
    /// new rules. Here a directory occupies the sibling's path so the write
    /// cannot open; a write breaking off part way, on a full filesystem for
    /// instance, reaches the same place having touched nothing but the sibling.
    ///
    /// A filter file lost to a failed save is not visible afterwards: the rules
    /// stop excluding what they named, and that reads as a filter matching
    /// nothing.
    #[tokio::test]
    async fn a_failed_save_leaves_the_previous_rules() {
        let dir = TempDir::new("lore-filter-save-failure-");
        let target = dir.child("rules");
        test_file_write(&target, ENCODED_RULES.as_bytes());
        let filter = lore_revision::filter::load_filter(&target).expect("load");

        // Occupy the sibling the save writes through, so opening it fails.
        let temporary = temp_sibling_of(&target);
        std::fs::create_dir(&temporary).expect("occupy the temporary path");

        assert!(
            lore_revision::filter::save(&filter, &target).await.is_err(),
            "a save that could not be written reported success"
        );
        assert_eq!(
            std::fs::read(&target).expect("read the target back"),
            ENCODED_RULES.as_bytes(),
            "a failed save changed the target"
        );

        let reloaded = lore_revision::filter::load_filter(&target).expect("reload");
        assert!(
            reloaded.excludes(&probe_path("build.log"), false),
            "the rules that were there before no longer apply"
        );
    }
}
