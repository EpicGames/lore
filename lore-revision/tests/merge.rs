// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use lore_revision::merge::merge3_text;

    #[test]
    fn test_merge() {
        let base_string = "This is line 1.\nThis is line 2.\n";
        let mine_string = "This is line 1.\nThis is line 2.\nThis line is added at the end.\n";
        let theirs_string = "This is line 1.\nThis line is added in between.\nThis is line 2.\n";
        let expected_string = "This is line 1.\nThis line is added in between.\nThis is line 2.\nThis line is added at the end.\n";

        let result_string =
            match merge3_text(base_string, mine_string, theirs_string, None, None, None) {
                Err(str) | Ok(str) => str,
            };

        assert_eq!(result_string, expected_string);
    }

    #[test]
    fn test_conflict() {
        let base_string = "This is line 1.\nThis is line 2.\n";
        let mine_string = "This is line 1.\nThis is line 2.\nThis line is added at the end.\n";
        let theirs_string =
            "This is line 1.\nThis is line 2.\nThis line is also added at the end.\n";
        let expected_string = "This is line 1.\nThis is line 2.\n<<<<<<< ours\nThis line is added at the end.\n||||||| original\n=======\nThis line is also added at the end.\n>>>>>>> theirs\n";

        let result_string =
            match merge3_text(base_string, mine_string, theirs_string, None, None, None) {
                Err(str) | Ok(str) => str,
            };

        assert_eq!(result_string, expected_string);
    }

    /// Fails without `IncompleteHunkStyle::Git`: the default glues the next
    /// marker onto the last line when it has no trailing newline.
    #[test]
    fn test_conflict_without_trailing_newlines() {
        let base_string = "This is line 1.
This is line 2.";
        let mine_string = "This is line 1.
This is line 2 as I wrote it.";
        let theirs_string = "This is line 1.
This is line 2 as they wrote it.";

        let result_string =
            match merge3_text(base_string, mine_string, theirs_string, None, None, None) {
                Err(str) | Ok(str) => str,
            };

        for marker in [
            "<<<<<<< ours",
            "||||||| original",
            "=======",
            ">>>>>>> theirs",
        ] {
            assert!(
                result_string.lines().any(|line| line == marker),
                "`{marker}` must occupy a line of its own, got:
{result_string}"
            );
        }
    }

    #[test]
    fn test_markers() {
        let base_string = "This is line 1.\nThis is line 2.\n";
        let mine_string = "This is line 1.\nThis is line 2.\nThis line is added at the end.\n";
        let theirs_string =
            "This is line 1.\nThis is line 2.\nThis line is also added at the end.\n";
        let expected_string = "This is line 1.\nThis is line 2.\n<<<<<<< mine\nThis line is added at the end.\n||||||| base\n=======\nThis line is also added at the end.\n>>>>>>> theirs\n";

        let result_string = match merge3_text(
            base_string,
            mine_string,
            theirs_string,
            Some("base"),
            Some("mine"),
            Some("theirs"),
        ) {
            Err(str) | Ok(str) => str,
        };

        assert_eq!(result_string, expected_string);
    }
}
