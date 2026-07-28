# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from error_types import ImproperArgumentsError
from lore import Lore
from lore_parsers import parse_status_json

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_diff(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text-File.txt"
    another_file = "another-file.txt"
    delete_file = os.path.join("extra-path", "to-delete-file.txt")

    repo.write_commit_push(
        None,
        {
            text_file: ["One line\n", "Another line\n", "Third line\n"],
            another_file: ["One line\n", "Another line\n", "Third line\n"],
            delete_file: [
                "One line\n",
                "Another line\n",
                "Third line\nAnother line\n",
            ],
        },
        offline=True,
    )

    # Modify some files, stage and commit
    repo.write_commit_push(
        "Test commit 2",
        {another_file: ["Reduced to one line\n"]},
        offline=True,
    )

    # Make a branch, do some commits
    repo.branch_create("test-branch", offline=True)

    repo.write_commit_push(
        "Test commit 3 on branch",
        {another_file: ["Modified on branch\n"]},
        offline=True,
    )

    repo.write_commit_push(
        "Test commit 4 on branch",
        {text_file: ["Modified on branch to generate a conflict\n"]},
        offline=True,
    )

    # Switch back to main branch, do some commits
    repo.branch_switch("main", offline=True)

    repo.write_commit_push(
        "Test commit 5 on main",
        {text_file: ["Modified on main to generate conflict\n"]},
        offline=True,
    )

    # Verify diff3 identifies the conflict
    output = repo.branch_diff("main", source="test-branch", offline=True)
    assert "C text-File.txt" in output, (
        "Branch diff did not correctly identify conflict"
    )

    # Now create another branch and commit a change
    repo.branch_create("another-branch", offline=True)

    third_file = "third-file.txt"
    with repo.open_file(third_file, "w+") as output_file:
        output_file.writelines(["Another file added on branch\n"])

    repo.remove_file(delete_file)
    repo.remove_dir(os.path.dirname(delete_file))

    repo.stage([third_file, os.path.dirname(delete_file)], offline=True, scan=True)
    repo.commit("Test commit 6 on another branch", offline=True)

    # Switch back to main and commit a change to a file
    repo.branch_switch("main", offline=True)

    repo.write_commit_push(
        "Test commit 7 on main",
        {text_file: ["Modified first file on main yet again\n"]},
        offline=True,
    )

    # Verify no conflicts in diff
    output = repo.branch_diff(
        "main", source="another-branch", auto_resolve=True, offline=True, level="trace"
    )
    assert "C " not in output, "Branch diff identified conflicts which should not exist"
    lines = output.splitlines()
    assert "A third-file.txt" in lines, "Branch diff did not identify added file"
    assert "D extra-path/to-delete-file.txt" in lines, (
        "Branch diff did not identify deleted file"
    )
    assert "D extra-path/" in lines, "Branch diff did not identify deleted directory"

    # Verify no conflicts in reverse diff
    output = repo.branch_diff("another-branch", source="main", offline=True)
    assert "C " not in output, "Branch diff identified conflicts which should not exist"
    assert "M text-File.txt" in output, "Branch diff did not identify modified file"

    # Merge in main into another branch
    repo.branch_switch("another-branch", offline=True)
    repo.branch_merge_start("main", offline=True, no_commit=True)
    repo.status(offline=True)
    repo.commit("Merge main to another-branch", offline=True)

    # Verify no conflicts in diff
    output = repo.branch_diff("main", source="another-branch", offline=True)
    assert "C " not in output, "Branch diff identified conflicts which should not exist"
    assert "A third-file.txt" in output, "Branch diff did not identify added file"

    # Modify merged in file in branch

    repo.write_commit_push(
        "Test commit 8 on another branch",
        {text_file: ["Modified first file on another branch yet again\n"]},
        offline=True,
    )

    # Verify no conflicts in diff since we merged in the change from main
    output = repo.branch_diff("main", source="another-branch", offline=True)
    assert "C " not in output, "Branch diff identified conflicts which should not exist"
    assert "A third-file.txt" in output, "Branch diff did not identify added file"
    assert "M text-File.txt" in output, (
        "Branch diff did not identify merged and modified file"
    )

    # Modify file on main again to get back into conflict
    repo.branch_switch("main", offline=True)

    repo.write_commit_push(
        "Test commit 9 on main",
        {text_file: ["Creating a conflict on main\n"]},
        offline=True,
    )

    output = repo.branch_diff("main", source="another-branch", offline=True)
    assert "A third-file.txt" in output, "Branch diff did not identify added file"
    assert "C text-File.txt" in output, (
        "Branch diff did not identify re-conflicted file"
    )

    # Verify directories added and merged are diffed correctly
    repo.branch_create("dir-add-1", offline=True)

    dirpath = "add-dir"
    repo.write_commit_push(
        "Test add on dir-add-1",
        {
            os.path.join(dirpath, "branch-1.txt"): [
                "Creating a file in subdir on dir-add-1\n"
            ]
        },
        offline=True,
    )

    repo.branch_switch("main", offline=True)
    repo.branch_create("dir-add-2", offline=True)

    dirpath = "add-dir"
    repo.write_commit_push(
        "Test add on dir-add-2",
        {
            os.path.join(dirpath, "branch-2.txt"): [
                "Creating a file in subdir on dir-add-2\n"
            ]
        },
        offline=True,
    )

    repo.branch_switch("main", offline=True)

    diff_output = repo.branch_diff("main", source="dir-add-1", offline=True)
    assert "A add-dir/" in diff_output.splitlines(), (
        "Diff did not identify added directory"
    )
    assert "A add-dir/branch-1.txt" in diff_output, (
        "Diff did not identify added file in directory"
    )

    diff_output = repo.branch_diff("main", source="dir-add-2", offline=True)
    assert "A add-dir/" in diff_output.splitlines(), (
        "Diff did not identify added directory"
    )
    assert "A add-dir/branch-2.txt" in diff_output, (
        "Diff did not identify added file in directory"
    )

    repo.branch_merge_start("dir-add-1", offline=True, no_commit=True)
    repo.commit("Merge dir-add-1 to main", offline=True)

    diff_output = repo.branch_diff("main", source="dir-add-1", offline=True)
    assert "A add-dir/" not in diff_output.splitlines(), (
        "Diff wrongly found added directory after merge"
    )
    assert "A add-dir/branch-1.txt" not in diff_output, (
        "Diff wrongly identified added file in directory after merge"
    )
    assert "A add-dir/branch-2.txt" not in diff_output, (
        "Diff wrongly identified added file in directory after merge"
    )

    diff_output = repo.branch_diff("main", source="dir-add-2", offline=True)
    assert "A add-dir/" not in diff_output.splitlines(), (
        "Diff wrongly found added directory after merge"
    )
    assert "A add-dir/branch-2.txt" in diff_output, (
        "Diff did not identify added file in directory after merge"
    )

    status = repo.status(offline=True)

    base_revision = ""
    for line in status.split("\n"):
        if "On branch main revision" in line:
            tokens = line.split(" -> ")
            base_revision = tokens[1]
    assert base_revision != "", "Failed to get revision"

    # Generate a commit with some changes
    first_path_file = "first/path/file.txt"
    first_other_file = "first/other/file.foo"
    second_path_file = "second/path/file.txt"

    repo.write_commit_push(
        "Some changes",
        {
            first_path_file: os.urandom(100),
            first_other_file: os.urandom(100),
            second_path_file: os.urandom(100),
        },
        offline=True,
    )

    status = repo.status(offline=True)

    new_revision = ""
    for line in status.split("\n"):
        if "On branch main revision" in line:
            tokens = line.split(" -> ")
            new_revision = tokens[1]
    assert new_revision != "", "Failed to get revision"

    output = repo.revision_diff(base_revision, offline=True)

    assert "C " not in output, (
        "Revision diff identified conflicts which should not exist"
    )
    assert "A " + first_path_file in output, "Revision diff did not identify added file"
    assert "A " + first_other_file in output, (
        "Revision diff did not identify added file"
    )
    assert "A " + second_path_file in output, (
        "Revision diff did not identify added file"
    )

    output = repo.revision_diff(base_revision, paths="first", offline=True)

    assert "C " not in output, (
        "Revision diff identified conflicts which should not exist"
    )
    assert "A " + first_path_file in output, "Revision diff did not identify added file"
    assert "A " + first_other_file in output, (
        "Revision diff did not identify added file"
    )
    assert "A second" not in output, (
        "Revision diff identified path which should not be included"
    )

    output = repo.revision_diff(base_revision, paths="second", offline=True)

    assert "C " not in output, (
        "Revision diff identified conflicts which should not exist"
    )
    assert "A first" not in output, (
        "Revision diff identified path which should not be included"
    )
    assert "A " + second_path_file in output, (
        "Revision diff did not identify added file"
    )

    output = repo.revision_diff(base_revision, paths=["first", "second"], offline=True)

    assert "C " not in output, (
        "Revision diff identified conflicts which should not exist"
    )
    assert "A " + first_path_file in output, "Revision diff did not identify added file"
    assert "A " + first_other_file in output, (
        "Revision diff did not identify added file"
    )
    assert "A " + second_path_file in output, (
        "Revision diff did not identify added file"
    )

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(
            ["Creating a local modification\nLine two of the local modification\n"]
        )

    output = repo.file_diff(text_file, offline=True)
    expected_diff = (
        "text-File.txt\n"
        + "--- text-File.txt@8\n"
        + "+++ text-File.txt\n"
        + "@@ -1 +1,2 @@\n"
        + "-Creating a conflict on main\n"
        + "+Creating a local modification\n"
        + "+Line two of the local modification\n"
        + "\n"
    )
    assert expected_diff in output, (
        "File diff did not generate the expected output\n"
        + expected_diff
        + "\nOutput:\n"
        + output
    )

    output = repo.file_diff(text_file, source="@1", offline=True)
    expected_diff = (
        "text-File.txt\n"
        + "--- text-File.txt@1\n"
        + "+++ text-File.txt\n"
        + "@@ -1,3 +1,2 @@\n"
        + "-One line\n"
        + "-Another line\n"
        + "-Third line\n"
        + "+Creating a local modification\n"
        + "+Line two of the local modification\n"
        + "\n"
    )
    assert expected_diff in output, (
        "File diff did not generate the expected output\n"
        + expected_diff
        + "\nOutput:\n"
        + output
    )

    output = repo.file_diff(text_file, source="@2", target="@3", offline=True)
    expected_diff = (
        "text-File.txt\n"
        + "--- text-File.txt@2\n"
        + "+++ text-File.txt@3\n"
        + "@@ -1,3 +1 @@\n"
        + "-One line\n"
        + "-Another line\n"
        + "-Third line\n"
        + "+Modified on main to generate conflict\n"
        + "\n"
    )
    assert expected_diff in output, (
        "File diff did not generate the expected output\n"
        + expected_diff
        + "\nOutput:\n"
        + output
    )


@pytest.mark.smoke
def test_file_diff_deleted_files(new_lore_repo):
    """Test that file diff correctly shows deleted files with patch content."""
    repo: Lore = new_lore_repo()

    # Create a file and commit it
    test_file = "deleted_file.txt"
    repo.write_commit_push(
        "Add file that will be deleted",
        {test_file: ["Content of file to be deleted\n", "Second line\n"]},
        offline=True,
    )

    # Delete the file from filesystem
    repo.remove_file(test_file)

    # Run file diff to get unified diff format
    output = repo.file_diff(offline=True)

    # Expected unified diff format for deleted file
    expected_diff = (
        "deleted_file.txt\n"
        + "--- deleted_file.txt@1\n"
        + "+++ /dev/null\n"
        + "@@ -1,2 +0,0 @@\n"
        + "-Content of file to be deleted\n"
        + "-Second line\n"
    )

    # Check that the patch contains the unified diff with deleted content
    assert expected_diff in output, (
        "File diff did not generate the expected unified diff for deleted file\n"
        + "Expected:\n"
        + expected_diff
        + "\n"
        + "Output:\n"
        + output
    )


@pytest.mark.smoke
def test_file_diff_added_files(new_lore_repo):
    """Test that file diff correctly shows added files with patch content."""
    repo: Lore = new_lore_repo()

    # Create initial commit so we have a baseline
    initial_file = "existing.txt"
    repo.write_commit_push(
        "Initial commit",
        {initial_file: ["Initial file\n"]},
        offline=True,
    )

    # Create a new file (this will be "added")
    test_file = "added_file.txt"
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["Content of new file\n", "Second line of new file\n"])

    # Run file diff to get unified diff format
    output = repo.file_diff(offline=True)

    # Expected unified diff format for added file
    expected_diff = (
        "added_file.txt\n"
        + "--- /dev/null\n"
        + "+++ added_file.txt\n"
        + "@@ -0,0 +1,2 @@\n"
        + "+Content of new file\n"
        + "+Second line of new file\n"
    )

    # Check that the patch contains the unified diff with added content
    assert expected_diff in output, (
        "File diff did not generate the expected unified diff for added file\n"
        + "Expected:\n"
        + expected_diff
        + "\n"
        + "Output:\n"
        + output
    )


@pytest.mark.smoke
def test_file_diff_context(new_lore_repo):
    """Test that --context / -U controls the number of unified-diff context lines."""
    repo: Lore = new_lore_repo()

    test_file = "multi_line.txt"
    original_lines = [f"Line {i:02d}\n" for i in range(1, 11)]
    repo.write_commit_push(
        "Add multi-line file",
        {test_file: original_lines},
        offline=True,
    )

    modified_lines = list(original_lines)
    modified_lines[4] = "Line 05 (modified)\n"
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(modified_lines)

    # Default: diffy default of 3 context lines on each side. Hunk spans lines 2..8.
    default_output = repo.file_diff(test_file, offline=True)
    expected_default = (
        "--- multi_line.txt@1\n"
        + "+++ multi_line.txt\n"
        + "@@ -2,7 +2,7 @@\n"
        + " Line 02\n"
        + " Line 03\n"
        + " Line 04\n"
        + "-Line 05\n"
        + "+Line 05 (modified)\n"
        + " Line 06\n"
        + " Line 07\n"
        + " Line 08\n"
    )
    assert expected_default in default_output, (
        "Default file diff context did not match the expected 3-line window\n"
        + "Expected:\n"
        + expected_default
        + "\nOutput:\n"
        + default_output
    )

    # --context 0: only the changed line, no surrounding context.
    zero_output = repo.file_diff(test_file, context=0, offline=True)
    expected_zero = (
        "@@ -5 +5 @@\n" + "-Line 05\n" + "+Line 05 (modified)\n"
    )
    assert expected_zero in zero_output, (
        "context=0 should show only the changed line with no surrounding context\n"
        + "Expected:\n"
        + expected_zero
        + "\nOutput:\n"
        + zero_output
    )
    assert "Line 04" not in zero_output, (
        f"context=0 should not show surrounding context lines\nOutput:\n{zero_output}"
    )
    assert "Line 06" not in zero_output, (
        f"context=0 should not show surrounding context lines\nOutput:\n{zero_output}"
    )

    # --context 7: saturates to full file (10 lines).
    wide_output = repo.file_diff(test_file, context=7, offline=True)
    expected_wide_header = "@@ -1,10 +1,10 @@\n"
    assert expected_wide_header in wide_output, (
        f"context=7 should produce a hunk covering all 10 lines\nOutput:\n{wide_output}"
    )
    assert " Line 01\n" in wide_output, (
        f"context=7 should include line 1 as surrounding context\nOutput:\n{wide_output}"
    )
    assert " Line 10\n" in wide_output, (
        f"context=7 should include line 10 as surrounding context\nOutput:\n{wide_output}"
    )

    # Explicit --context 3 must be identical to the omitted default.
    explicit_three_output = repo.file_diff(test_file, context=3, offline=True)
    assert explicit_three_output == default_output, (
        "Explicit --context 3 should produce identical output to the default\n"
        + "Default:\n"
        + default_output
        + "\nExplicit:\n"
        + explicit_three_output
    )


@pytest.mark.smoke
def test_file_diff_invalid_context(new_lore_repo):
    """Invalid --context / -U values produce one clear, actionable error.

    Both non-numeric and negative input must report 'expected a non-negative
    integer' and echo the offending value, instead of Rust's internal
    'invalid digit found in string' or clap's misleading 'unexpected argument'
    tip for the negative case.
    """
    repo: Lore = new_lore_repo()

    test_file = "ctx.txt"
    repo.write_commit_push(
        "Add file for context validation",
        {test_file: ["Line 1\n", "Line 2\n"]},
        offline=True,
    )

    for bad in ["abc", "-1", "1.5"]:
        with pytest.raises(
            ImproperArgumentsError, match="expected a non-negative integer"
        ) as exc_info:
            repo.file_diff(test_file, context=bad, offline=True)

        message = str(exc_info.value)
        assert f"got '{bad}'" in message, (
            f"--context {bad!r} error should echo the offending value\n"
            f"Output:\n{message}"
        )
        # The negative case must reach the value parser, not be rejected as an
        # unrelated argument with the misleading '-- -1' tip.
        assert "unexpected argument" not in message, (
            f"--context {bad!r} should not be treated as an unexpected argument\n"
            f"Output:\n{message}"
        )


@pytest.mark.smoke
def test_file_diff_blank_context_line_has_sentinel(new_lore_repo):
    """Blank context lines must be emitted as " \\n" (leading-space sentinel),
    not a bare "\\n". Standard unified-diff parsers require every hunk-body line
    to begin with ' ', '+', '-', or '\\'; a bare blank line makes them under-count
    rows so downstream line numbers drift. Regression for CROWD CR-1860."""
    repo: Lore = new_lore_repo()

    test_file = "blank_ctx.txt"
    repo.write_commit_push(
        "Initial commit",
        {test_file: ["alpha\n", "\n", "bravo\n"]},
        offline=True,
    )

    # Non-whitespace change on line 1; blank line 2 stays as context.
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["ALPHA\n", "\n", "bravo\n"])

    expected = (
        "@@ -1,3 +1,3 @@\n"
        + "-alpha\n"
        + "+ALPHA\n"
        + " \n"  # blank context line keeps the leading-space sentinel
        + " bravo\n"
    )

    # Default path (diffy native formatter).
    output = repo.file_diff(test_file, offline=True)
    assert expected in output, (
        "Default path: blank context line must be ' \\n', not bare '\\n'\n"
        + "Expected:\n"
        + expected
        + "\nOutput:\n"
        + repr(output)
    )

    # Ignore-whitespace path (format_patch_preserving_originals / write_patch_line).
    output_ws = repo.file_diff(test_file, ignore_space_at_eol=True, offline=True)
    assert expected in output_ws, (
        "Ignore-whitespace path: blank context line must be ' \\n', not bare '\\n'\n"
        + "Expected:\n"
        + expected
        + "\nOutput:\n"
        + repr(output_ws)
    )


@pytest.mark.smoke
def test_file_diff_ignore_space_at_eol(new_lore_repo):
    """--ignore-space-at-eol suppresses lines that differ only in trailing whitespace."""
    repo: Lore = new_lore_repo()

    test_file = "eol.txt"
    # Commit with trailing whitespace already on line 1, so we can later test that
    # context lines preserve that original whitespace in the display.
    repo.write_commit_push(
        "Initial commit",
        {test_file: ["foo   \n", "bar\n", "baz\n"]},
        offline=True,
    )

    # Edit: change trailing whitespace on line 1 (different amount) — no other change.
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["foo\n", "bar\n", "baz\n"])

    # Without the flag, the diff shows the trailing-whitespace change.
    plain_output = repo.file_diff(test_file, offline=True)
    assert "-foo   \n" in plain_output and "+foo\n" in plain_output, (
        f"Without --ignore-space-at-eol the trailing-whitespace change should show\nOutput:\n{plain_output}"
    )

    # With the flag, no hunk is emitted for the file.
    ignored_output = repo.file_diff(test_file, ignore_space_at_eol=True, offline=True)
    assert "@@" not in ignored_output, (
        f"--ignore-space-at-eol should suppress trailing-whitespace-only diffs\nOutput:\n{ignored_output}"
    )

    # Re-edit: keep the trailing-whitespace-only diff on line 1 AND change line 2.
    # Line 1 should appear as context, preserving the committed (OLD) trailing whitespace.
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["foo\n", "BAR\n", "baz\n"])
    faithful_output = repo.file_diff(
        test_file, ignore_space_at_eol=True, offline=True
    )
    assert " foo   \n" in faithful_output, (
        f"Context line should preserve the OLD side's original trailing whitespace\nOutput:\n{faithful_output}"
    )
    assert "-bar\n" in faithful_output and "+BAR\n" in faithful_output, (
        f"Real change should still appear\nOutput:\n{faithful_output}"
    )


@pytest.mark.smoke
def test_file_diff_ignore_space_change(new_lore_repo):
    """--ignore-space-change suppresses lines that differ only in internal whitespace runs."""
    repo: Lore = new_lore_repo()

    test_file = "inline.txt"
    repo.write_commit_push(
        "Initial commit",
        {test_file: ["a b c\n", "second\n"]},
        offline=True,
    )

    # Edit: expand whitespace runs on line 1.
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["a  b   c\n", "second\n"])

    plain_output = repo.file_diff(test_file, offline=True)
    assert "-a b c\n" in plain_output and "+a  b   c\n" in plain_output, (
        f"Without --ignore-space-change the run-expansion should show\nOutput:\n{plain_output}"
    )

    ignored_output = repo.file_diff(test_file, ignore_space_change=True, offline=True)
    assert "@@" not in ignored_output, (
        f"--ignore-space-change should suppress run-only changes\nOutput:\n{ignored_output}"
    )

    # Introducing whitespace where there was none MUST still register as a change.
    repo.write_commit_push(
        "Collapse to abc",
        {test_file: ["abc\n", "second\n"]},
        offline=True,
    )
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["a bc\n", "second\n"])

    introduced_output = repo.file_diff(
        test_file, ignore_space_change=True, offline=True
    )
    assert "-abc\n" in introduced_output and "+a bc\n" in introduced_output, (
        f"Introducing whitespace must still appear as a change with --ignore-space-change\nOutput:\n{introduced_output}"
    )


@pytest.mark.smoke
def test_file_diff_ignore_both_flags_combined(new_lore_repo):
    """Both ignore-whitespace flags together suppress all whitespace-only diffs."""
    repo: Lore = new_lore_repo()

    test_file = "combined.txt"
    # Commit with whitespace already present, so context display can show OLD originals.
    repo.write_commit_push(
        "Initial commit",
        {test_file: ["alpha   \n", "beta  gamma\n", "delta\n"]},
        offline=True,
    )

    # Line 1: trailing-whitespace-only diff. Line 2: internal-whitespace-only diff.
    # Line 3: real change.
    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["alpha\n", "beta gamma\n", "DELTA\n"])

    # Without flags all three show.
    plain_output = repo.file_diff(test_file, offline=True)
    assert "-alpha   \n" in plain_output
    assert "-beta  gamma\n" in plain_output
    assert "+DELTA\n" in plain_output

    # With both flags, only the real change shows; surrounding context preserves originals.
    combined_output = repo.file_diff(
        test_file,
        ignore_space_at_eol=True,
        ignore_space_change=True,
        offline=True,
    )
    assert "-delta\n" in combined_output and "+DELTA\n" in combined_output, (
        f"Real change should still appear\nOutput:\n{combined_output}"
    )
    assert "-alpha   \n" not in combined_output, (
        f"Trailing-whitespace-only line should not appear as a change\nOutput:\n{combined_output}"
    )
    assert "-beta  gamma\n" not in combined_output, (
        f"Internal-whitespace-only line should not appear as a change\nOutput:\n{combined_output}"
    )
    # Context lines preserve original (un-normalised) whitespace from the OLD side.
    assert " alpha   \n" in combined_output, (
        f"Context line should preserve OLD-side trailing whitespace\nOutput:\n{combined_output}"
    )
    assert " beta  gamma\n" in combined_output, (
        f"Context line should preserve OLD-side internal whitespace runs\nOutput:\n{combined_output}"
    )


@pytest.mark.smoke
def test_file_diff_ignore_flags_default_off_matches_existing(new_lore_repo):
    """Passing the new flags as False is byte-identical to omitting them."""
    repo: Lore = new_lore_repo()

    test_file = "regression.txt"
    repo.write_commit_push(
        "Initial commit",
        {test_file: [f"Line {i:02d}\n" for i in range(1, 6)]},
        offline=True,
    )

    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(
            ["Line 01\n", "Line 02 changed\n", "Line 03\n", "Line 04\n", "Line 05\n"]
        )

    omitted = repo.file_diff(test_file, offline=True)
    explicit = repo.file_diff(
        test_file,
        ignore_space_at_eol=False,
        ignore_space_change=False,
        offline=True,
    )
    assert omitted == explicit, (
        "Explicitly passing the flags as False must match omitting them\n"
        f"Omitted:\n{omitted}\nExplicit:\n{explicit}"
    )


@pytest.mark.smoke
def test_file_diff_ignore_flags_with_context_zero(new_lore_repo):
    """context=0 combined with --ignore-space-at-eol produces no hunks at all when only EOL whitespace changed."""
    repo: Lore = new_lore_repo()

    test_file = "ctx_zero.txt"
    repo.write_commit_push(
        "Initial commit",
        {test_file: ["line one\n", "line two\n"]},
        offline=True,
    )

    with repo.open_file(test_file, "w+") as output_file:
        output_file.writelines(["line one   \n", "line two\n"])

    output = repo.file_diff(
        test_file, context=0, ignore_space_at_eol=True, offline=True
    )
    assert "@@" not in output, (
        f"context=0 + --ignore-space-at-eol should produce no hunks\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_file_diff_ignore_flags_added_deleted_files(new_lore_repo):
    """Added/deleted files (one side empty) are not suppressed by the ignore flags."""
    repo: Lore = new_lore_repo()

    initial_file = "existing.txt"
    repo.write_commit_push(
        "Initial commit",
        {initial_file: ["baseline\n"]},
        offline=True,
    )

    # Add a file whose every line is just trailing whitespace.
    added_file = "added.txt"
    with repo.open_file(added_file, "w+") as output_file:
        output_file.writelines(["hello   \n", "world   \n"])

    add_output = repo.file_diff(
        ignore_space_at_eol=True, ignore_space_change=True, offline=True
    )
    assert "+++ added.txt" in add_output, (
        f"Added file should still be reported even with both ignore flags on\nOutput:\n{add_output}"
    )
    assert "+hello   \n" in add_output, (
        f"Added file content should preserve original whitespace\nOutput:\n{add_output}"
    )

    # Commit the add, then delete the file.
    repo.stage([added_file], offline=True)
    repo.commit("Add file", offline=True)
    repo.remove_file(added_file)

    delete_output = repo.file_diff(
        ignore_space_at_eol=True, ignore_space_change=True, offline=True
    )
    assert "--- added.txt@2" in delete_output, (
        f"Deleted file should still be reported even with both ignore flags on\nOutput:\n{delete_output}"
    )


@pytest.mark.smoke
def test_file_diff3_ignore_flags(new_lore_repo):
    """diff3 mode threads --ignore-space-at-eol through to the emitted patch."""
    repo: Lore = new_lore_repo()

    test_file = "shared.txt"
    repo.write_commit_push(
        "Initial commit",
        {test_file: "Original content\n"},
        offline=True,
    )

    repo.branch_create("feature", offline=True)
    # Whitespace-only edit on the feature branch.
    repo.write_commit_push(
        "Whitespace-only edit on feature",
        {test_file: "Original content   \n"},
        offline=True,
    )
    feature_rev = repo.revision_info(offline=True).signature

    repo.branch_switch("main", offline=True)
    main_rev = repo.revision_info(offline=True).signature

    # Without the flag the trailing-space change appears.
    plain_output = repo.file_diff(
        test_file, source=main_rev, target=feature_rev, diff3=True, offline=True
    )
    assert "+Original content   " in plain_output, (
        f"diff3 should show the trailing-space change without the flag\nOutput:\n{plain_output}"
    )

    # With the flag the whitespace-only change is suppressed.
    ignored_output = repo.file_diff(
        test_file,
        source=main_rev,
        target=feature_rev,
        diff3=True,
        ignore_space_at_eol=True,
        offline=True,
    )
    assert "@@" not in ignored_output, (
        f"diff3 + --ignore-space-at-eol should suppress trailing-whitespace-only changes\nOutput:\n{ignored_output}"
    )


@pytest.mark.smoke
def test_file_diff3_non_conflicting(new_lore_repo):
    """Test that file diff --diff3 produces a base-to-source unified diff for source-only changes."""
    repo: Lore = new_lore_repo()

    text_file = "shared.txt"

    # Initial commit on main (this becomes the merge base)
    repo.write_commit_push(
        "Initial commit",
        {text_file: "Original content\n"},
        offline=True,
    )

    # Create a feature branch and modify the file
    repo.branch_create("feature", offline=True)

    repo.write_commit_push(
        "Modify shared on feature branch",
        {text_file: "Modified on feature\n"},
        offline=True,
    )

    feature_rev = repo.revision_info(offline=True).signature

    # Main is unchanged — feature's modification is source-only
    repo.branch_switch("main", offline=True)
    main_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(
        text_file, source=main_rev, target=feature_rev, diff3=True, offline=True
    )

    # Source additions appear as + lines (base→target direction)
    assert "+Modified on feature" in output, (
        "diff3 source-only output should show source additions as + lines\nOutput:\n"
        + output
    )
    assert "-Original content" in output, (
        "diff3 source-only output should show base content as - lines\nOutput:\n"
        + output
    )

    # No conflict markers should be present for a source-only change
    assert "<<<<<<<" not in output, (
        "diff3 should not contain conflict markers for non-conflicting changes"
    )
    assert ">>>>>>>" not in output, (
        "diff3 should not contain conflict end markers for non-conflicting changes"
    )
    assert "|||||||" not in output, (
        "diff3 should not contain base separator for non-conflicting changes"
    )


@pytest.mark.smoke
def test_file_diff3_auto_resolved(new_lore_repo):
    """Test that diff3 shows only source contribution for auto-resolved files."""
    repo: Lore = new_lore_repo()

    text_file = "shared.txt"

    # Initial commit on main with multi-line file
    repo.write_commit_push(
        "Initial commit",
        {text_file: "line 1\nline 2\nline 3\n"},
        offline=True,
    )

    # Feature branch adds at the end (non-overlapping with main's change)
    repo.branch_create("feature", offline=True)

    repo.write_commit_push(
        "Add line at end on feature",
        {text_file: "line 1\nline 2\nline 3\nfeature addition\n"},
        offline=True,
    )

    feature_rev = repo.revision_info(offline=True).signature

    # Main adds at the beginning (non-overlapping with feature's change)
    repo.branch_switch("main", offline=True)

    repo.write_commit_push(
        "Add line at start on main",
        {text_file: "main addition\nline 1\nline 2\nline 3\n"},
        offline=True,
    )

    main_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(
        text_file, source=main_rev, target=feature_rev, diff3=True, offline=True
    )

    # Reviewed branch contribution (feature addition) appears as + line
    assert "+feature addition" in output, (
        "diff3 auto-resolved should show source additions as + lines\nOutput:\n" + output
    )

    # Target's change (main addition) should NOT appear as +/- since it's context
    assert "+main addition" not in output, (
        "diff3 auto-resolved should not show target additions\nOutput:\n" + output
    )
    assert "-main addition" not in output, (
        "diff3 auto-resolved should not show target content as removed\nOutput:\n" + output
    )

    # No conflict markers
    assert "<<<<<<<" not in output, (
        "diff3 should not contain conflict markers for auto-resolved changes"
    )


@pytest.mark.smoke
def test_file_diff3_conflicting(new_lore_repo):
    """Test that file diff --diff3 produces three-way merge output with correct mine/theirs."""
    repo: Lore = new_lore_repo()

    text_file = "conflict.txt"

    # Initial commit on main (this becomes the merge base)
    repo.write_commit_push(
        "Initial commit",
        {text_file: "Base content\n"},
        offline=True,
    )

    # Create a feature branch and modify the file one way
    repo.branch_create("feature", offline=True)

    repo.write_commit_push(
        "Modify on feature",
        {text_file: "Feature modification\n"},
        offline=True,
    )

    feature_rev = repo.revision_info(offline=True).signature

    # Switch to main and modify the same file differently — creates a conflict
    repo.branch_switch("main", offline=True)

    repo.write_commit_push(
        "Modify on main",
        {text_file: "Main modification\n"},
        offline=True,
    )

    main_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(
        text_file, source=main_rev, target=feature_rev, diff3=True, offline=True
    )

    # Validate: mine=source (<<<<<<< source@N), theirs=target (>>>>>>> target@N)
    # This matches what `repo merge` produces for the same conflict.
    assert "<<<<<<< source@2" in output, (
        "conflict mine marker should be source\nOutput:\n" + output
    )
    assert ">>>>>>> target@2" in output, (
        "conflict theirs marker should be target\nOutput:\n" + output
    )
    assert "||||||| base@1" in output, (
        "conflict base marker should be present\nOutput:\n" + output
    )

    # Target content (main) in the mine section, source content (feature) in theirs
    assert "Main modification" in output
    assert "Feature modification" in output


@pytest.mark.smoke
def test_diff_after_reset(new_lore_repo):
    repo: Lore = new_lore_repo()

    text_file = "text-File.txt"
    another_file = "another-file.txt"

    repo.write_commit_push(
        None,
        {text_file: ["One line\n", "Another line\n", "Third line\n"]},
        offline=True,
    )

    # Add another file
    repo.write_commit_push(
        "Test commit 2",
        {another_file: ["One line\n", "Another line\n", "Third line\n"]},
        offline=True,
    )

    # Make a branch
    repo.branch_create("test-branch", offline=True)

    # Reset to revision 1
    repo.branch_switch("main", offline=True)
    repo.branch_reset(revision="@1")

    output = repo.branch_diff(source="test-branch", target="main", debug=True)

    assert f"A {another_file}" in output


@pytest.mark.smoke
def test_file_diff_utf16le(new_lore_repo):
    """Test that file diff correctly handles UTF-16 LE encoded files.

    Files created by Windows PowerShell use UTF-16 LE encoding by default.
    When diffing such files, content should be displayed line-by-line as
    written in the file, not with each character on its own line.
    """
    repo: Lore = new_lore_repo()

    text_file = "utf16_file.txt"
    utf16_bom = b"\xff\xfe"

    base_content = "Base line one\nBase line two\n"
    base_bytes = utf16_bom + base_content.encode("utf-16-le")

    # Initial commit on main
    repo.write_commit_push(
        "Initial UTF-16 file",
        {text_file: base_bytes},
        offline=True,
    )

    # Create feature branch with a different modification
    repo.branch_create("feature", offline=True)

    feature_content = "Feature line one\nFeature line two\n"
    feature_bytes = utf16_bom + feature_content.encode("utf-16-le")

    repo.write_commit_push(
        "Modify UTF-16 file on feature",
        {text_file: feature_bytes},
        offline=True,
    )

    feature_rev = repo.revision_info(offline=True).signature

    # Switch to main
    repo.branch_switch("main", offline=True)
    main_rev = repo.revision_info(offline=True).signature

    # Test 2-way diff: feature vs main
    output = repo.file_diff(
        text_file, source=feature_rev, target=main_rev, offline=True
    )

    # The diff should produce a clean unified diff with properly decoded
    # UTF-16 content. The expected output should match what we'd see for a
    # UTF-8 file with the same text content.
    expected_diff = (
        "utf16_file.txt\n"
        + "--- utf16_file.txt@2\n"
        + "+++ utf16_file.txt@1\n"
        + "@@ -1,2 +1,2 @@\n"
        + "-Feature line one\n"
        + "-Feature line two\n"
        + "+Base line one\n"
        + "+Base line two\n"
    )
    assert expected_diff in output, (
        "File diff did not generate the expected output for UTF-16 LE file.\n"
        + "Expected:\n"
        + expected_diff
        + "\nOutput:\n"
        + output
    )

    # The output must not contain Unicode replacement characters (U+FFFD),
    # which would indicate the UTF-16 BOM was not properly handled.
    assert "\ufffd" not in output, (
        "Diff output contains replacement characters from unhandled UTF-16 BOM.\n"
        + "Output:\n"
        + repr(output)
    )


@pytest.mark.smoke
def test_file_diff3_utf16le(new_lore_repo):
    """Test that diff3 correctly handles UTF-16 LE encoded files with conflicts.

    When using --diff3 on UTF-16 LE files, the three-way merge output should
    show content line-by-line, not with each character on its own line.
    """
    repo: Lore = new_lore_repo()

    text_file = "utf16_conflict.txt"
    utf16_bom = b"\xff\xfe"

    base_content = "Base content\n"
    base_bytes = utf16_bom + base_content.encode("utf-16-le")

    # Initial commit on main (merge base)
    repo.write_commit_push(
        "Initial UTF-16 file",
        {text_file: base_bytes},
        offline=True,
    )

    # Create feature branch and modify the file
    repo.branch_create("feature", offline=True)

    feature_content = "Feature modification\n"
    feature_bytes = utf16_bom + feature_content.encode("utf-16-le")

    repo.write_commit_push(
        "Modify on feature",
        {text_file: feature_bytes},
        offline=True,
    )

    feature_rev = repo.revision_info(offline=True).signature

    # Switch to main and modify differently to create a conflict
    repo.branch_switch("main", offline=True)

    main_content = "Main modification\n"
    main_bytes = utf16_bom + main_content.encode("utf-16-le")

    repo.write_commit_push(
        "Modify on main",
        {text_file: main_bytes},
        offline=True,
    )

    main_rev = repo.revision_info(offline=True).signature

    # Test diff3 with conflicting UTF-16 files
    output = repo.file_diff(
        text_file, source=main_rev, target=feature_rev, diff3=True, offline=True
    )

    # The diff3 output should have properly decoded UTF-16 content with
    # conflict markers showing readable text, not garbled bytes.
    # mine=source (<<<<<<< source@N), theirs=target (>>>>>>> target@N)
    assert "<<<<<<< source@2" in output, (
        "diff3 UTF-16 conflict mine marker should be source (baseline)\nOutput:\n" + output
    )
    assert ">>>>>>> target@2" in output, (
        "diff3 UTF-16 conflict theirs marker should be target (reviewed)\nOutput:\n" + output
    )
    assert "||||||| base@1" in output, (
        "diff3 UTF-16 conflict base marker should be present\nOutput:\n" + output
    )
    assert "Feature modification" in output, (
        "diff3 UTF-16 output should contain decoded feature content\nOutput:\n" + output
    )
    assert "Main modification" in output, (
        "diff3 UTF-16 output should contain decoded main content\nOutput:\n" + output
    )

    # The output must not contain Unicode replacement characters (U+FFFD),
    # which would indicate the UTF-16 BOM was not properly handled.
    assert "\ufffd" not in output, (
        "diff3 output contains replacement characters from unhandled UTF-16 BOM.\n"
        + "Output:\n"
        + repr(output)
    )


@pytest.mark.smoke
def test_file_diff_utf16be(new_lore_repo):
    """File diff should handle UTF-16 BE encoded files (FE FF BOM)."""
    repo: Lore = new_lore_repo()

    text_file = "utf16be_file.txt"
    utf16be_bom = b"\xfe\xff"

    base_content = "Base line one\nBase line two\n"
    base_bytes = utf16be_bom + base_content.encode("utf-16-be")

    repo.write_commit_push(
        "Initial UTF-16 BE file",
        {text_file: base_bytes},
        offline=True,
    )

    repo.branch_create("feature", offline=True)

    feature_content = "Feature line one\nFeature line two\n"
    feature_bytes = utf16be_bom + feature_content.encode("utf-16-be")

    repo.write_commit_push(
        "Modify UTF-16 BE file on feature",
        {text_file: feature_bytes},
        offline=True,
    )

    feature_rev = repo.revision_info(offline=True).signature

    repo.branch_switch("main", offline=True)
    main_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(
        text_file, source=feature_rev, target=main_rev, offline=True
    )

    expected_diff = (
        "utf16be_file.txt\n"
        + "--- utf16be_file.txt@2\n"
        + "+++ utf16be_file.txt@1\n"
        + "@@ -1,2 +1,2 @@\n"
        + "-Feature line one\n"
        + "-Feature line two\n"
        + "+Base line one\n"
        + "+Base line two\n"
    )
    assert expected_diff in output, (
        "File diff did not generate the expected output for UTF-16 BE file.\n"
        + "Expected:\n"
        + expected_diff
        + "\nOutput:\n"
        + output
    )

    assert "\ufffd" not in output, (
        "Diff output contains replacement characters from unhandled UTF-16 BE BOM.\n"
        + "Output:\n"
        + repr(output)
    )


@pytest.mark.smoke
def test_branch_merge_utf16le_conflict_preserves_bytes(new_lore_repo):
    """UTF-16 LE BOM files must NOT be auto-merged.

    Even non-overlapping edits across branches must route through Lore's
    binary-conflict fallback, leaving the working tree bytes untouched and
    requiring manual resolution. This guards against the merge path ever
    rewriting a Windows-authored UTF-16 file as UTF-8.
    """
    repo: Lore = new_lore_repo()

    text_file = "utf16_merge.txt"
    utf16_bom = b"\xff\xfe"

    base_lines = [
        "Line one\n",
        "Line two\n",
        "Line three\n",
        "Line four\n",
        "Line five\n",
    ]
    base_bytes = utf16_bom + "".join(base_lines).encode("utf-16-le")

    repo.write_commit_push(
        "Initial UTF-16 file",
        {text_file: base_bytes},
        offline=True,
    )

    # Feature branch: modify the first line only.
    repo.branch_create("feature", offline=True)
    feature_lines = base_lines.copy()
    feature_lines[0] = "Line one (feature)\n"
    feature_bytes = utf16_bom + "".join(feature_lines).encode("utf-16-le")
    repo.write_commit_push(
        "Modify first line on feature",
        {text_file: feature_bytes},
        offline=True,
    )

    # Main branch: modify the last line only \u2014 would auto-merge cleanly for
    # plain UTF-8 since the hunks don't overlap.
    repo.branch_switch("main", offline=True)
    main_lines = base_lines.copy()
    main_lines[-1] = "Line five (main)\n"
    main_bytes = utf16_bom + "".join(main_lines).encode("utf-16-le")
    repo.write_commit_push(
        "Modify last line on main",
        {text_file: main_bytes},
        offline=True,
    )

    # Attempt the merge \u2014 UTF-16 must land in the binary-conflict path, so the
    # merge cannot complete without manual resolution.
    repo.branch_merge_start("feature", no_commit=True, check=False, offline=True)

    with repo.open_file(text_file, "rb") as handle:
        merged_bytes = handle.read()

    assert merged_bytes == main_bytes, (
        "UTF-16 LE working tree bytes must be byte-for-byte identical to "
        "main's commit; the binary-conflict path must not touch the file.\n"
        + f"first 4 bytes after merge: {merged_bytes[:4].hex()}\n"
        + f"expected first 4 bytes:    {main_bytes[:4].hex()}"
    )
    for marker in (b"<<<<<<<", b"|||||||", b">>>>>>>"):
        assert marker not in merged_bytes, (
            f"Conflict marker {marker!r} must not appear in a UTF-16 file."
        )

    raw_status = repo.status(offline=True, json=True)
    entries = parse_status_json(raw_status)
    conflicted = {
        e["path"] for e in entries if e.get("flagConflictUnresolved") is True
    }
    assert text_file in conflicted, (
        f"{text_file} should appear in `lore status` as unresolved conflict; "
        f"got conflicted set: {conflicted}"
    )


@pytest.mark.smoke
def test_branch_merge_utf8_bom_preserves_bom(new_lore_repo):
    """A UTF-8 BOM file with non-overlapping edits across branches must
    auto-merge cleanly AND retain its `EF BB BF` BOM in the working tree
    after the merge.

    Guards against `decode_text_for_display` (or anything else that strips
    the BOM) creeping back into the merge round-trip and silently changing
    a Windows-authored file from UTF-8-BOM to plain UTF-8.
    """
    repo: Lore = new_lore_repo()

    text_file = "utf8_bom_merge.txt"
    utf8_bom = b"\xef\xbb\xbf"

    base_lines = [
        "Line one\n",
        "Line two\n",
        "Line three\n",
        "Line four\n",
        "Line five\n",
    ]
    base_bytes = utf8_bom + "".join(base_lines).encode("utf-8")

    repo.write_commit_push(
        "Initial UTF-8 BOM file",
        {text_file: base_bytes},
        offline=True,
    )

    repo.branch_create("feature", offline=True)
    feature_lines = base_lines.copy()
    feature_lines[0] = "Line one (feature)\n"
    feature_bytes = utf8_bom + "".join(feature_lines).encode("utf-8")
    repo.write_commit_push(
        "Modify first line on feature",
        {text_file: feature_bytes},
        offline=True,
    )

    repo.branch_switch("main", offline=True)
    main_lines = base_lines.copy()
    main_lines[-1] = "Line five (main)\n"
    main_bytes = utf8_bom + "".join(main_lines).encode("utf-8")
    repo.write_commit_push(
        "Modify last line on main",
        {text_file: main_bytes},
        offline=True,
    )

    repo.branch_merge("feature", offline=True)

    with repo.open_file(text_file, "rb") as handle:
        merged_bytes = handle.read()

    assert merged_bytes.startswith(utf8_bom), (
        "UTF-8 BOM must survive a successful auto-merge — the binary `EF BB BF` "
        "prefix is the user's file format and must not be silently dropped.\n"
        + f"first 4 bytes after merge: {merged_bytes[:4].hex()}"
    )
    merged_text = merged_bytes[3:].decode("utf-8")
    assert "Line one (feature)" in merged_text, (
        "Auto-merge should retain the feature branch's first-line edit.\n"
        + f"Merged file: {merged_text!r}"
    )
    assert "Line five (main)" in merged_text, (
        "Auto-merge should retain main's last-line edit.\n"
        + f"Merged file: {merged_text!r}"
    )
    for marker in ("<<<<<<<", "|||||||", ">>>>>>>"):
        assert marker not in merged_text, (
            f"Conflict marker {marker!r} should not appear in a successful "
            "auto-merge result."
        )


@pytest.mark.smoke
def test_branch_diff_auto_resolve_no_write_required(new_lore_repo):
    """Regression: `branch diff --auto-resolve` must not fail with
    "Write access required but repository was opened read-only" when diff3
    has auto-resolvable, non-overlapping changes to resolve.

    The client-side `diff_local` opens the repository read-only, but the
    auto-resolve path inside diff3 tries to acquire a write token to persist
    resolution state. When auto-resolve has actual work to do, this surfaces
    as a `BranchError::WriteRequired` and the command fails. This test sets
    up exactly that scenario and asserts the error does not appear.
    """
    repo: Lore = new_lore_repo()

    shared = "shared.txt"

    # Base commit on main.
    repo.write_commit_push(
        "Base commit",
        {shared: "line 1\nline 2\nline 3\n"},
        offline=True,
    )

    # Feature branch appends at the end of the file.
    repo.branch_create("feature", offline=True)
    repo.write_commit_push(
        "Feature appends at end",
        {shared: "line 1\nline 2\nline 3\nfeature addition\n"},
        offline=True,
    )

    # Back on main, insert at the start \u2014 non-overlapping with feature's
    # change, so diff3 should be able to auto-resolve.
    repo.branch_switch("main", offline=True)
    repo.write_commit_push(
        "Main inserts at start",
        {shared: "main addition\nline 1\nline 2\nline 3\n"},
        offline=True,
    )

    # check=False so we can inspect the failure output explicitly rather
    # than rely on the subprocess wrapper raising.
    output = repo.branch_diff(
        "main",
        source="feature",
        auto_resolve=True,
        offline=True,
        check=False,
    )

    assert "Write access required" not in output, (
        "branch diff --auto-resolve regressed to read-only failure\n"
        "Output:\n" + output
    )
    assert f"C {shared}" not in output, (
        "Auto-resolvable change should not be reported as a conflict\n"
        "Output:\n" + output
    )
    assert f"M {shared}" in output, (
        "Auto-resolved file should appear as modified\n"
        "Output:\n" + output
    )


@pytest.mark.smoke
def test_file_diff_binary_emits_marker(new_lore_repo):
    """`lore diff` on binary content must emit a `Binary files differ` marker,
    not garbled bytes rendered through the lossy text decoder."""
    repo: Lore = new_lore_repo()

    binary_file = "binary-test.bin"
    base_bytes = bytes(
        [0x00, 0x01, 0x02, 0xFF, 0xFE, 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A,
         0x1A, 0x0A, 0xDE, 0xAD, 0xBE, 0xEF]
    )

    repo.write_commit_push(
        "Initial binary file",
        {binary_file: base_bytes},
        offline=True,
    )

    repo.branch_create("feature", offline=True)

    feature_bytes = bytes(
        [0x00, 0x01, 0x02, 0xFF, 0xFE, 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A,
         0x1A, 0x0A, 0xCA, 0xFE, 0xBA, 0xBE]
    )
    repo.write_commit_push(
        "Modify binary on feature",
        {binary_file: feature_bytes},
        offline=True,
    )
    feature_rev = repo.revision_info(offline=True).signature

    repo.branch_switch("main", offline=True)
    main_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(
        binary_file, source=feature_rev, target=main_rev, offline=True
    )

    assert "Binary files differ" in output, (
        "Binary diff should emit a `Binary files differ` marker.\nOutput:\n"
        + output
    )
    assert "\ufffd" not in output, (
        "Binary diff must not render replacement characters.\nOutput:\n"
        + repr(output)
    )
    assert "@@" not in output, (
        "Binary diff must not emit a unified-diff hunk header.\nOutput:\n"
        + output
    )


@pytest.mark.smoke
def test_file_diff3_binary_emits_marker(new_lore_repo):
    """`lore diff --diff3` on binary content must emit a `Binary files differ`
    marker rather than conflict markers wrapping garbled bytes."""
    repo: Lore = new_lore_repo()

    binary_file = "binary-test.bin"
    base_bytes = bytes(
        [0x00, 0x01, 0x02, 0xFF, 0xFE, 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A,
         0x1A, 0x0A, 0xDE, 0xAD, 0xBE, 0xEF]
    )

    repo.write_commit_push(
        "Initial binary file",
        {binary_file: base_bytes},
        offline=True,
    )

    repo.branch_create("feature", offline=True)
    feature_bytes = base_bytes + bytes([0x11, 0x22, 0x33])
    repo.write_commit_push(
        "Modify binary on feature",
        {binary_file: feature_bytes},
        offline=True,
    )
    feature_rev = repo.revision_info(offline=True).signature

    repo.branch_switch("main", offline=True)
    main_bytes = bytes([0x44, 0x55, 0x66]) + base_bytes
    repo.write_commit_push(
        "Modify binary on main",
        {binary_file: main_bytes},
        offline=True,
    )
    main_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(
        binary_file, source=main_rev, target=feature_rev, diff3=True, offline=True
    )

    assert "Binary files differ" in output, (
        "Binary diff3 should emit a `Binary files differ` marker.\nOutput:\n"
        + output
    )
    assert "\ufffd" not in output, (
        "Binary diff3 must not render replacement characters.\nOutput:\n"
        + repr(output)
    )
    for marker in ("<<<<<<<", "=======", ">>>>>>>"):
        assert marker not in output, (
            f"Binary diff3 must not emit conflict marker {marker!r}.\nOutput:\n"
            + output
        )


@pytest.mark.smoke
def test_diff_staged_move_shown_as_move(new_lore_repo):
    """Diff of a staged move should show a single move, not a delete + add.

    A file that has been renamed via ``stage move`` carries a move action. The
    diff output must reflect that move (connecting the original path to the new
    path) rather than decomposing it into an unrelated deletion of the old path
    and addition of the new path.
    """
    repo: Lore = new_lore_repo()

    original = "movetest.txt"
    renamed = "renamedmove.txt"

    # Commit the file so it exists in the current revision.
    repo.write_commit_push(
        "Add file that will be moved",
        {original: ["test content\n"]},
        offline=True,
    )

    # Rename it on disk, then record the move in staging.
    repo.move(original, renamed)
    repo.stage_move(original, renamed, offline=True)

    output = repo.diff(offline=True)

    # The move must not be rendered as a deletion of the original path...
    assert f"--- {original}" not in output or "+++ /dev/null" not in output, (
        "Diff rendered the staged move as a deletion of the original path "
        "instead of a move.\nOutput:\n" + output
    )
    # ...nor as a brand-new addition of the renamed path.
    assert "--- /dev/null" not in output or f"+++ {renamed}" not in output, (
        "Diff rendered the staged move as an addition of the renamed path "
        "instead of a move.\nOutput:\n" + output
    )

    # The move should appear as a single entry that connects both paths, naming
    # the original path as the move source and the renamed path as the move
    # destination.
    assert f"move from {original}" in output, (
        "Diff did not name the original path as the move source.\nOutput:\n"
        + output
    )
    assert f"move to {renamed}" in output, (
        "Diff did not name the renamed path as the move destination.\nOutput:\n"
        + output
    )


@pytest.mark.smoke
def test_diff_staged_move_with_edit_shows_move_and_content(new_lore_repo):
    """A staged move whose content also changed shows the move and the hunks.

    On top of the ``move from``/``move to`` header connecting the two paths, the
    diff must also render the unified-diff hunks for the content change against
    the original file, rather than splitting the rename into a delete + add.
    """
    repo: Lore = new_lore_repo()

    original = "movetest.txt"
    renamed = "renamedmove.txt"

    # Commit the file so it exists in the current revision.
    repo.write_commit_push(
        "Add file that will be moved and edited",
        {original: ["line one\n", "line two\n"]},
        offline=True,
    )

    # Rename it on disk and record the move, then edit the renamed file.
    repo.move(original, renamed)
    repo.stage_move(original, renamed, offline=True)
    with repo.open_file(renamed, "w") as renamed_file:
        renamed_file.writelines(["line one\n", "line two changed\n"])

    output = repo.diff(offline=True)

    # The rename must not be split into a delete of the original path or an
    # addition of the renamed path.
    assert "+++ /dev/null" not in output, (
        "Diff rendered the moved-and-edited file as a deletion.\nOutput:\n" + output
    )
    assert "--- /dev/null" not in output, (
        "Diff rendered the moved-and-edited file as an addition.\nOutput:\n" + output
    )

    # The move is reported connecting both paths...
    assert f"move from {original}" in output, (
        "Diff did not name the original path as the move source.\nOutput:\n" + output
    )
    assert f"move to {renamed}" in output, (
        "Diff did not name the renamed path as the move destination.\nOutput:\n"
        + output
    )

    # ...and the content change is rendered as unified-diff hunks against the
    # original content.
    assert "-line two\n" in output, (
        "Diff did not show the original line as removed.\nOutput:\n" + output
    )
    assert "+line two changed\n" in output, (
        "Diff did not show the edited line as added.\nOutput:\n" + output
    )


@pytest.mark.smoke
def test_diff_revision_to_revision_move_shown_as_move(new_lore_repo):
    """A committed rename between two revisions shows as a move, not delete + add.

    Diffing a source revision against a target revision that renamed (and edited)
    a file must connect the two paths via the move header and render the content
    change as hunks, rather than as an unrelated deletion plus addition.
    """
    repo: Lore = new_lore_repo()

    original = "movetest.txt"
    renamed = "renamedmove.txt"

    # Base revision with the original file.
    repo.write_commit_push(
        "Add file that will be moved",
        {original: ["line one\n", "line two\n"]},
        offline=True,
    )
    source_rev = repo.revision_info(offline=True).signature

    # Rename and edit on a feature branch, producing the target revision.
    repo.branch_create("feature", offline=True)
    repo.move(original, renamed)
    repo.stage_move(original, renamed, offline=True)
    with repo.open_file(renamed, "w") as renamed_file:
        renamed_file.writelines(["line one\n", "line two changed\n"])
    repo.stage(renamed, offline=True)
    repo.commit("Rename and edit on feature", offline=True)
    target_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(source=source_rev, target=target_rev, offline=True)

    # Not split into a deletion of the original or an addition of the renamed path.
    assert "+++ /dev/null" not in output, (
        "Revision diff rendered the move as a deletion.\nOutput:\n" + output
    )
    assert "--- /dev/null" not in output, (
        "Revision diff rendered the move as an addition.\nOutput:\n" + output
    )

    # Shown as a move connecting both paths, with the content change as hunks.
    assert f"move from {original}" in output, (
        "Revision diff did not name the original path as the move source.\nOutput:\n"
        + output
    )
    assert f"move to {renamed}" in output, (
        "Revision diff did not name the renamed path as the move destination.\n"
        "Output:\n" + output
    )
    assert "-line two\n" in output, (
        "Revision diff did not show the original line as removed.\nOutput:\n" + output
    )
    assert "+line two changed\n" in output, (
        "Revision diff did not show the edited line as added.\nOutput:\n" + output
    )


@pytest.mark.smoke
def test_diff3_move_shown_as_move(new_lore_repo):
    """diff3 of a branch that renamed a file shows the move, not delete + add.

    With the rename done only on the source branch, diff3 surfaces that branch's
    contribution as a move connecting the two paths, with the accompanying
    content change rendered as hunks against the merge base.
    """
    repo: Lore = new_lore_repo()

    original = "movetest.txt"
    renamed = "renamedmove.txt"

    # Base commit on main becomes the merge base.
    repo.write_commit_push(
        "Initial commit",
        {original: ["line one\n", "line two\n"]},
        offline=True,
    )

    # Feature branch renames and edits the file.
    repo.branch_create("feature", offline=True)
    repo.move(original, renamed)
    repo.stage_move(original, renamed, offline=True)
    with repo.open_file(renamed, "w") as renamed_file:
        renamed_file.writelines(["line one\n", "line two changed\n"])
    repo.stage(renamed, offline=True)
    repo.commit("Rename and edit on feature", offline=True)
    feature_rev = repo.revision_info(offline=True).signature

    # Main is unchanged, so the rename is the source branch's sole contribution.
    repo.branch_switch("main", offline=True)
    main_rev = repo.revision_info(offline=True).signature

    output = repo.file_diff(
        source=main_rev, target=feature_rev, diff3=True, offline=True
    )

    assert "+++ /dev/null" not in output, (
        "diff3 rendered the move as a deletion.\nOutput:\n" + output
    )
    assert "--- /dev/null" not in output, (
        "diff3 rendered the move as an addition.\nOutput:\n" + output
    )
    assert f"move from {original}" in output, (
        "diff3 did not name the original path as the move source.\nOutput:\n" + output
    )
    assert f"move to {renamed}" in output, (
        "diff3 did not name the renamed path as the move destination.\nOutput:\n"
        + output
    )
    assert "+line two changed\n" in output, (
        "diff3 did not show the source branch's edit as an added line.\nOutput:\n"
        + output
    )
