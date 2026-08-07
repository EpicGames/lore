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

    #[test]
    fn test_conflict_without_trailing_newlines() {
        let base_string = "This is line 1.\nThis is line 2.";
        let mine_string = "This is line 1.\nThis is line 2 changed.";
        let theirs_string = "This is line 1.\nThis is line 2 also changed.";
        let expected_string = "This is line 1.\n<<<<<<< ours\nThis is line 2 changed.\n||||||| original\nThis is line 2.\n=======\nThis is line 2 also changed.\n>>>>>>> theirs\n";

        let result_string =
            match merge3_text(base_string, mine_string, theirs_string, None, None, None) {
                Err(str) | Ok(str) => str,
            };

        assert_eq!(result_string, expected_string);
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
