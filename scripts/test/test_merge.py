# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import random
import shutil
import stat
import string

import pytest
from error_types import CommitFailed

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_merge(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Create example file
    example_file = "example.txt"
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(
            ["This file exists to create an initial commit on the 'main' branch.\n"]
        )

    # Stage the example file
    repo.stage(scan=True, offline=True)

    # Commit the example file
    repo.commit("Required commit on the main branch", offline=True)

    # Merge test base class
    class MergeTester:
        def __init__(self, name):
            self.name = name
            self.branch_current = "current_" + "".join(
                random.choices(string.digits, k=8)
            )
            self.branch_incoming = "incoming_" + "".join(
                random.choices(string.digits, k=8)
            )
            self.result_merge = ""

        def try_stage(self):
            repo.file_stage(scan=True, offline=True)
            return True

        def try_commit(self, message):
            output = repo.commit(message, offline=True)
            return message in output

        def try_fail_commit(self, message):
            with pytest.raises(CommitFailed):
                repo.commit(message, offline=True)
            return True

        def try_branch_create(self, branch):
            repo.branch_create(branch, offline=True)
            return True

        def try_branch_switch(self, branch):
            repo.branch_switch(branch, offline=True)
            return True

        def try_resolve_path(self, path):
            output = repo.branch_merge_resolve(path, offline=True)
            return not "No conflicts resolved" in output

        def try_unresolve_path(self, path):
            output = repo.branch_merge_unresolve(path, offline=True)
            return not "No files marked as unresolved" in output

        def try_resolve_mine(self, path):
            output = repo.branch_merge_resolve_mine(path, offline=True)
            return not "No changes staged" in output

        def try_resolve_theirs(self, path):
            output = repo.branch_merge_resolve_theirs(path, offline=True)
            return not "No changes staged" in output

        def run(self):
            # Create a current branch
            assert self.try_branch_create(self.branch_current), (
                "Create current branch failed"
            )
            # Prepare test
            self.prepare()

            # Stage the files
            assert self.try_stage(), "Staging files failed"
            # Commit the files
            message = "Initial commit on current branch"
            assert self.try_commit(message), "Commit files failed"
            # Create a incoming branch
            assert self.try_branch_create(self.branch_incoming), (
                "Create incoming branch failed"
            )
            # Apply changes on incoming branch to commit
            if self.apply_incoming_branch_changes_to_commit():
                # Stage the files
                assert self.try_stage(), "Staging files failed"
                # Commit the files
                message = "Commit on incoming branch"
                assert self.try_commit(message), "Commit files failed"
            # Switch to current branch
            assert self.try_branch_switch(self.branch_current), (
                "Switch to current branch failed"
            )
            # Apply changes on current branch to commit
            if self.apply_current_branch_changes_to_commit():
                # Stage the files
                assert self.try_stage(), "Staging files failed"
                # Commit the files
                message = "Commit on current branch"
                assert self.try_commit(message), "Commit files failed"
            # Apply pending changes on current branch
            self.apply_current_branch_pending_changes()

            # Merge the incoming branch to current branch
            self.result_merge = repo.branch_merge_start(
                self.branch_incoming, check=False, offline=True, no_commit=True
            )
            self.result_merge_succeeded = "Error" not in self.result_merge
            logger.info(self.result_merge)

            # Display repository status
            self.result_status = repo.status(offline=True)
            logger.info(self.result_status)

            # Verify that the merge went as expected
            self.verify()

            # Reset the branch so we can switch
            self.resolve()

            # Switch to main branch
            assert self.try_branch_switch("main"), "Switch to main branch failed"
            # Verify that only example file exists
            dirs = [
                f
                for f in os.listdir(repo.path)
                if os.path.isdir(os.path.join(repo.path, f))
            ]
            assert len(dirs) == 1, (
                "Unexpected number of directories after reset: " + str(dirs)
            )

            files = [
                f
                for f in os.listdir(repo.path)
                if os.path.isfile(os.path.join(repo.path, f))
            ]
            assert len(files) == 1, "Unexpected number of files after reset: " + str(
                files
            )

        # Called at start of test to setup the base state.
        def prepare(self):
            pass

        # Called prior to 'merge' to create pending changes on the current branch.
        def apply_current_branch_pending_changes(self):
            pass

        # Called to make changes to the base state on the current branch.
        def apply_current_branch_changes_to_commit(self) -> bool:
            return False  # Nothing to stage/commit.

        # Called to make changes to the base state on the incoming branch.
        def apply_incoming_branch_changes_to_commit(self) -> bool:
            return False  # Nothing to stage/commit.

        # Called after 'merge' to verify the state on the current branch.
        def verify(self):
            pass

        # Called prior to switch back to the main branch to reset pending changes on the current branch.
        def resolve(self):
            if self.result_merge_succeeded:
                # By default, this does a 'merge abort'
                repo.branch_merge_abort(offline=True)

        def is_conflicted_count(self, count):
            if count > 0:
                if f"{count} conflicted" in self.result_merge:
                    return True
                return False
            else:
                if f" 0 conflicted" in self.result_merge:
                    return True
                if f" conflicted" in self.result_merge:
                    return False
                return True

        def is_staged(self, path):
            check = False
            output = self.result_status
            for line in output.split("\n"):
                if line.startswith("Changes"):
                    check = False
                if len(line) == 0:
                    check = False
                if line.startswith("Changes staged for commit:"):
                    check = True
                if check and path in line:
                    return True
            return False

        def is_merged(self, path):
            output = self.result_status
            for line in output.split("\n"):
                if path in line:
                    return True
            return False

        def is_conflicted(self, path):
            output = self.result_status
            for line in output.split("\n"):
                if path in line and "!" in line:
                    return True
            return False

        def is_containing_text(self, path, contents):
            with repo.open_file(path, "r") as f:
                data = f.read()
                if data == contents:
                    return True
            return False

        def is_containing_data(self, path, contents):
            with repo.open_file(path, "r+b") as f:
                data = f.read()
                if data == contents:
                    return True
            return False

        def expect_merge_failed(self):
            assert not self.result_merge_succeeded, (
                "Expected `merge` to fail but it succeeded."
            )

        def expect_merge_succeeded(self):
            assert self.result_merge_succeeded, (
                "Expected `merge` to succeed but it failed."
            )

        def expect_conflict_count(self, count):
            assert self.is_conflicted_count(count), (
                f"Expected {count} file(s) to be in conflict.\n {self.result_merge}"
            )

        def expect_merged(self, path):
            assert self.is_merged(path), f"Expected `{path}` to be merged."

        def expect_not_merged(self, path):
            assert not self.is_merged(path), f"Expected `{path}` to NOT be merged."

        def expect_staged(self, path):
            assert self.is_staged(path), f"Expected `{path}` to be staged."

        def expect_not_staged(self, path):
            assert not self.is_staged(path), f"Expected `{path}` to NOT be staged."

        def expect_conflicted(self, path):
            assert self.is_conflicted(path), f"Expected `{path}` to be in conflict."

        def expect_not_conflicted(self, path):
            assert not self.is_conflicted(path), (
                f"Expected `{path}` to NOT be in conflict."
            )

        def expect_text_contents(self, path, contents):
            assert self.is_containing_text(path, contents), (
                f"Expected different text contents for `{path}`."
            )

        def expect_data_contents(self, path, contents):
            assert self.is_containing_data(path, contents), (
                f"Expected different data contents for `{path}`."
            )

        def expect_exists(self, path):
            assert os.path.exists(os.path.join(repo.path, path)), (
                f"Expected `{path}` to exist."
            )

        def expect_not_exists(self, path):
            assert not os.path.exists(os.path.join(repo.path, path)), (
                f"Expected `{path}` to NOT exist."
            )

    # Merge test for a failed merge because of pending changes on the current branch to a file that has been modified on the incoming branch
    class MergeTesterNoMerge(MergeTester):
        def __init__(self):
            super().__init__("no_merge")

        def prepare(self):
            self.text_file1 = "some_text_file.txt"

            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )

        def apply_current_branch_pending_changes(self):
            # Backup the base file and write a new one.
            os.replace(
                os.path.join(repo.path, self.text_file1),
                os.path.join(repo.path, self.text_file1 + ".bak"),
            )
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Line inserted in middle on current branch\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )

        def apply_current_branch_changes_to_commit(self):
            return False  # Nothing to stage/commit

        def apply_incoming_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Line inserted in middle on incoming branch\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )
            return True

        def verify(self):
            self.expect_merge_failed()
            self.expect_conflict_count(0)
            self.expect_not_merged(self.text_file1)
            self.expect_not_staged(self.text_file1)
            self.expect_not_conflicted(self.text_file1)

            contents = (
                "Beware the Jabberwock, my son!\n"
                "The jaws that bite, the claws that catch!\n"
                "Line inserted in middle on current branch\n"
                "Beware the Jubjub bird, and shun\n"
                "The frumious Bandersnatch!\n"
            )
            self.expect_text_contents(self.text_file1, contents)

        def resolve(self):
            super().resolve()

            # Restore the base file.
            os.replace(
                os.path.join(repo.path, self.text_file1 + ".bak"),
                os.path.join(repo.path, self.text_file1),
            )

    # Merge test for a trivial merge of text file(s) that are modified on the incoming branch but are not modified on the current branch
    class MergeTesterNoConflict(MergeTester):
        def __init__(self):
            super().__init__("no_conflict")

        def prepare(self):
            self.text_file1 = "some_text_file.txt"
            self.text_file2 = "subdir/another_text_file.txt"

            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )

            repo.make_dirs(os.path.dirname(self.text_file2))
            with repo.open_file(self.text_file2, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )

        def apply_current_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Line inserted in middle on current branch\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )
            return True

        def apply_incoming_branch_changes_to_commit(self):
            with repo.open_file(self.text_file2, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "Line inserted in middle on incoming branch\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(0)
            self.expect_not_merged(self.text_file1)
            self.expect_merged(self.text_file2)
            self.expect_not_staged(self.text_file1)
            self.expect_staged(self.text_file2)
            self.expect_not_conflicted(self.text_file1)
            self.expect_not_conflicted(self.text_file2)

            contents = (
                "Beware the Jabberwock, my son!\n"
                "The jaws that bite, the claws that catch!\n"
                "Line inserted in middle on current branch\n"
                "Beware the Jubjub bird, and shun\n"
                "The frumious Bandersnatch!\n"
            )
            self.expect_text_contents(self.text_file1, contents)

            contents = (
                "About, about, in reel and rout\n"
                "The death-fires danced at night\n"
                "Line inserted in middle on incoming branch\n"
                "The water, like a witchs oils\n"
                "Burnt green, and blue and white\n"
            )
            self.expect_text_contents(self.text_file2, contents)

    # Merge test for a successful merge of text file(s) that are modified on current and incoming branch but the changes need to be merged manually (using conflict markers)
    class MergeTesterTextModifyModifyConflict(MergeTester):
        def __init__(self):
            super().__init__("text_modify_modify_conflict")

        def prepare(self):
            self.text_file1 = "some_text_file.txt"
            self.text_file2 = "subdir/another_text_file.txt"

            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )

            repo.make_dirs(os.path.dirname(self.text_file2))
            with repo.open_file(self.text_file2, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )

        def apply_current_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Line inserted in middle on current branch\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )
            with repo.open_file(self.text_file2, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "Line inserted in middle on current branch\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )
            return True

        def apply_incoming_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Line inserted in middle on incoming branch\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )
            with repo.open_file(self.text_file2, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "Line inserted in middle on incoming branch\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(2)
            self.expect_merged(self.text_file1)
            self.expect_merged(self.text_file2)
            self.expect_not_staged(self.text_file1)
            self.expect_not_staged(self.text_file2)
            self.expect_conflicted(self.text_file1)
            self.expect_conflicted(self.text_file2)

            contents = (
                "Beware the Jabberwock, my son!\n"
                "The jaws that bite, the claws that catch!\n"
                "<<<<<<< ours\n"
                "Line inserted in middle on current branch\n"
                "||||||| original\n"
                "=======\n"
                "Line inserted in middle on incoming branch\n"
                ">>>>>>> theirs\n"
                "Beware the Jubjub bird, and shun\n"
                "The frumious Bandersnatch!\n"
            )
            self.expect_text_contents(self.text_file1, contents)

            contents = (
                "About, about, in reel and rout\n"
                "The death-fires danced at night\n"
                "<<<<<<< ours\n"
                "Line inserted in middle on current branch\n"
                "||||||| original\n"
                "=======\n"
                "Line inserted in middle on incoming branch\n"
                ">>>>>>> theirs\n"
                "The water, like a witchs oils\n"
                "Burnt green, and blue and white\n"
            )
            self.expect_text_contents(self.text_file2, contents)

        def resolve(self):
            message = "Commit using files still containing conflict markers"
            assert self.try_fail_commit(message), (
                "Could commit file with conflict markers."
            )
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "Beware the Jabberwock, my son!\n",
                        "The jaws that bite, the claws that catch!\n",
                        "Line inserted in middle on current branch\n",
                        "Line inserted in middle on incoming branch\n",
                        "Beware the Jubjub bird, and shun\n",
                        "The frumious Bandersnatch!\n",
                    ]
                )
            with repo.open_file(self.text_file2, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "Line inserted in middle on current branch\n",
                        "Line inserted in middle on incoming branch\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )

            contents = (
                "Beware the Jabberwock, my son!\n"
                "The jaws that bite, the claws that catch!\n"
                "Line inserted in middle on current branch\n"
                "Line inserted in middle on incoming branch\n"
                "Beware the Jubjub bird, and shun\n"
                "The frumious Bandersnatch!\n"
            )
            self.expect_text_contents(self.text_file1, contents)

            contents = (
                "About, about, in reel and rout\n"
                "The death-fires danced at night\n"
                "Line inserted in middle on current branch\n"
                "Line inserted in middle on incoming branch\n"
                "The water, like a witchs oils\n"
                "Burnt green, and blue and white\n"
            )
            self.expect_text_contents(self.text_file2, contents)

            assert self.try_resolve_path(self.text_file1), "Could not resolve."
            assert self.try_resolve_path(self.text_file2), "Could not resolve."
            # Test if un-resolve and re-resolve work.
            assert self.try_unresolve_path(self.text_file2), "Could not unresolve."
            assert self.try_resolve_path(self.text_file2), "Could not resolve."
            message = "Resolve using manual edits"
            assert self.try_commit(message), "Could not commit."

    # Merge test for a successful merge of text file(s) that are modified on current and incoming branch but the changes can be auto merged because they are on different lines
    class MergeTesterTextModifyModifyAutoMerge(MergeTester):
        def __init__(self):
            super().__init__("text_modify_modify_automerge")

        def prepare(self):
            self.text_file1 = "some_text_file.txt"

            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )

        def apply_current_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                        "Line inserted at end on current branch\n",
                    ]
                )
            return True

        def apply_incoming_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "Line inserted in middle on incoming branch\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(0)
            self.expect_merged(self.text_file1)
            self.expect_staged(self.text_file1)
            self.expect_not_conflicted(self.text_file1)

            contents = (
                "About, about, in reel and rout\n"
                "The death-fires danced at night\n"
                "Line inserted in middle on incoming branch\n"
                "The water, like a witchs oils\n"
                "Burnt green, and blue and white\n"
                "Line inserted at end on current branch\n"
            )
            self.expect_text_contents(self.text_file1, contents)

    # Merge test for a successful merge of text file(s) that are modified identically on current and incoming branch
    class MergeTesterTextModifyModifyAutoResolve(MergeTester):
        def __init__(self):
            super().__init__("text_modify_modify_autoresolve")

        def prepare(self):
            self.text_file1 = "some_text_file.txt"

            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                    ]
                )

        def apply_current_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                        "Line inserted at end on both branches\n",
                    ]
                )
            return True

        def apply_incoming_branch_changes_to_commit(self):
            with repo.open_file(self.text_file1, "w+") as output_file:
                output_file.writelines(
                    [
                        "About, about, in reel and rout\n",
                        "The death-fires danced at night\n",
                        "The water, like a witchs oils\n",
                        "Burnt green, and blue and white\n",
                        "Line inserted at end on both branches\n",
                    ]
                )
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(0)
            self.expect_merged(self.text_file1)
            self.expect_staged(self.text_file1)
            self.expect_not_conflicted(self.text_file1)

            contents = (
                "About, about, in reel and rout\n"
                "The death-fires danced at night\n"
                "The water, like a witchs oils\n"
                "Burnt green, and blue and white\n"
                "Line inserted at end on both branches\n"
            )
            self.expect_text_contents(self.text_file1, contents)

    # Merge test for a successful merge of a binary file that conflicts
    class MergeTesterBinaryModifyModifyResolveNone(MergeTester):
        def __init__(self, name="binary_modify_modify_resolve_none"):
            super().__init__(name)

        def prepare(self):
            self.binary_file = "subdir/some_file.uasset"

            self.binary_data_base = os.urandom(97901)
            repo.make_dirs(os.path.dirname(self.binary_file))
            with repo.open_file(self.binary_file, "w+b") as output_file:
                output_file.write(self.binary_data_base)

        def apply_current_branch_changes_to_commit(self):
            self.binary_data_current = os.urandom(78543)
            with repo.open_file(self.binary_file, "w+b") as output_file:
                output_file.write(self.binary_data_current)
            return True

        def apply_incoming_branch_changes_to_commit(self):
            self.binary_data_incoming = os.urandom(211923)
            with repo.open_file(self.binary_file, "w+b") as output_file:
                output_file.write(self.binary_data_incoming)
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(1)
            self.expect_merged(self.binary_file)
            self.expect_not_staged(self.binary_file)
            self.expect_conflicted(self.binary_file)
            self.expect_data_contents(self.binary_file, self.binary_data_current)

    # Merge test for a successful merge of a binary file that conflicts, resolved using the 'mine' version.
    class MergeTesterBinaryModifyModifyResolveMine(
        MergeTesterBinaryModifyModifyResolveNone
    ):
        def __init__(self):
            super().__init__("binary_modify_modify_resolve_mine")

        def resolve(self):
            assert self.try_resolve_mine(self.binary_file), "Could not resolve to mine."
            self.expect_data_contents(self.binary_file, self.binary_data_current)

            message = "Resolve using mine version"
            assert self.try_commit(message), "Could not commit mine."

    # Merge test for a successful merge of a binary file that conflicts, resolved using the 'theirs' version.
    class MergeTesterBinaryModifyModifyResolveTheirs(
        MergeTesterBinaryModifyModifyResolveNone
    ):
        def __init__(self):
            super().__init__("binary_modify_modify_resolve_theirs")

        def resolve(self):
            assert self.try_resolve_theirs(self.binary_file), (
                "Could not resolve to theirs."
            )
            self.expect_data_contents(self.binary_file, self.binary_data_incoming)

            message = "Resolve using theirs version"
            assert self.try_commit(message), "Could not commit theirs."

    # Merge test for a successful merge of a binary file that conflicts on modify/delete
    class MergeTesterBinaryModifyDeleteResolveNone(MergeTester):
        def __init__(self, name="binary_modify_delete_resolve_none"):
            super().__init__(name)

        def prepare(self):
            self.binary_file = "some_file.uasset"

            self.binary_data_base = os.urandom(97901)
            with repo.open_file(self.binary_file, "w+b") as output_file:
                output_file.write(self.binary_data_base)

        def apply_current_branch_changes_to_commit(self):
            self.binary_data_current = os.urandom(78543)
            with repo.open_file(self.binary_file, "w+b") as output_file:
                output_file.write(self.binary_data_current)
            return True

        def apply_incoming_branch_changes_to_commit(self):
            repo.remove_file(self.binary_file)
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(1)
            self.expect_merged(self.binary_file)
            self.expect_not_staged(self.binary_file)
            self.expect_conflicted(self.binary_file)
            self.expect_exists(self.binary_file)
            self.expect_data_contents(self.binary_file, self.binary_data_current)

    # Merge test for a successful merge of a binary file that conflicts on delete/modify
    class MergeTesterBinaryDeleteModifyResolveNone(MergeTester):
        def __init__(self, name="binary_delete_modify_resolve_none"):
            super().__init__(name)

        def prepare(self):
            self.binary_file = "subdir/some_file.uasset"
            self.binary_data_base = os.urandom(97901)

            repo.make_dirs(os.path.dirname(self.binary_file))
            with repo.open_file(self.binary_file, "w+b") as output_file:
                output_file.write(self.binary_data_base)

        def apply_current_branch_changes_to_commit(self):
            repo.remove_file(self.binary_file)
            return True

        def apply_incoming_branch_changes_to_commit(self):
            self.binary_data_incoming = os.urandom(78543)
            with repo.open_file(self.binary_file, "w+b") as output_file:
                output_file.write(self.binary_data_incoming)
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(1)
            self.expect_merged(self.binary_file)
            self.expect_not_staged(self.binary_file)
            self.expect_conflicted(self.binary_file)
            self.expect_not_exists(self.binary_file)

    # Merge test for a successful merge of a binary file that conflicts on delete/modify, resolved using the 'mine' version.
    class MergeTesterBinaryDeleteModifyResolveMine(
        MergeTesterBinaryDeleteModifyResolveNone
    ):
        def __init__(self):
            super().__init__("binary_delete_modify_resolve_mine")

        def resolve(self):
            assert self.try_resolve_mine(self.binary_file), "Could not resolve to mine."
            self.expect_not_exists(self.binary_file)

            message = "Resolve using mine version"
            assert self.try_commit(message), "Could not commit mine."

    # Merge test for a successful merge of a binary file that conflicts on delete/modify, resolved using the 'theirs' version.
    class MergeTesterBinaryDeleteModifyResolveTheirs(
        MergeTesterBinaryDeleteModifyResolveNone
    ):
        def __init__(self):
            super().__init__("binary_delete_modify_resolve_theirs")

        def resolve(self):
            assert self.try_resolve_theirs(self.binary_file), (
                "Could not resolve to theirs."
            )
            self.expect_data_contents(self.binary_file, self.binary_data_incoming)

            message = "Resolve using theirs version"
            assert self.try_commit(message), "Could not commit theirs."

    # Merge test for a successful merge of a path that conflicts because it's type (file vs directory) is different
    class MergeTesterBinaryModifyTypeResolveNone(MergeTester):
        def __init__(self, name="binary_modify_type_resolve_none"):
            super().__init__(name)

        def prepare(self):
            self.binary_file1 = "some_path"  # 'some_path' is a file on current branch.
            self.binary_file2 = "some_path/some_file.uasset"  # 'some_path' is a directory on incoming branch.
            self.binary_file2_transposed = "some_path~theirs/some_file.uasset"

            self.binary_data_base = os.urandom(97901)
            with repo.open_file(self.binary_file1, "w+b") as output_file:
                output_file.write(self.binary_data_base)

        def apply_current_branch_changes_to_commit(self):
            self.binary_data_current = os.urandom(78543)
            with repo.open_file(self.binary_file1, "w+b") as output_file:
                output_file.write(self.binary_data_current)
            return True

        def apply_incoming_branch_changes_to_commit(self):
            self.binary_data_incoming = os.urandom(211923)
            repo.remove_file(self.binary_file1)
            repo.make_dirs(os.path.dirname(self.binary_file2))
            with repo.open_file(self.binary_file2, "w+b") as output_file:
                output_file.write(self.binary_data_incoming)
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(3)
            self.expect_merged(self.binary_file1)
            # self.expect_merged(self.binary_file2_transposed)
            self.expect_not_staged(self.binary_file1)
            self.expect_not_staged(self.binary_file2)
            self.expect_not_staged(self.binary_file2_transposed)
            self.expect_conflicted(self.binary_file1)
            # self.expect_conflicted(self.binary_file2_transposed)
            self.expect_data_contents(self.binary_file1, self.binary_data_current)

    # Merge test for a successful merge of a path that conflicts because it's type (file vs directory) is different, by keeping both versions.
    class MergeTesterBinaryModifyTypeResolveKeepBoth(
        MergeTesterBinaryModifyTypeResolveNone
    ):
        def __init__(self):
            super().__init__("binary_modify_type_resolve_keep_both")

        def resolve(self):
            assert self.try_resolve_mine(self.binary_file1), (
                "Could not resolve original version."
            )
            assert self.try_resolve_path(self.binary_file2_transposed), (
                "Could not resolve transposed version."
            )
            message = "Resolve using both versions"
            assert self.try_commit(message), "Could not commit both versions."

    # Merge test for a successful merge of a path that conflicts because it's type (directory vs file) is different
    class MergeTesterBinaryModifyTypeInvertedResolveNone(MergeTester):
        def __init__(self, name="binary_modify_type_inverted_resolve_none"):
            super().__init__(name)

        def prepare(self):
            self.binary_file1 = "some_path/some_file.uasset"  # 'some_path' is a directory on current branch.
            self.binary_file2 = "some_path"  # 'some_path' is a file on incoming branch.
            self.binary_file2_transposed = "some_path~theirs"

            self.binary_data_base = os.urandom(97901)
            repo.make_dirs(os.path.dirname(self.binary_file1))
            with repo.open_file(self.binary_file1, "w+b") as output_file:
                output_file.write(self.binary_data_base)

        def apply_current_branch_changes_to_commit(self):
            self.binary_data_current = os.urandom(78543)
            with repo.open_file(self.binary_file1, "w+b") as output_file:
                output_file.write(self.binary_data_current)
            return True

        def apply_incoming_branch_changes_to_commit(self):
            self.binary_data_incoming = os.urandom(211923)
            repo.remove_file(self.binary_file1)
            repo.remove_dir(os.path.dirname(self.binary_file1))
            with repo.open_file(self.binary_file2, "w+b") as output_file:
                output_file.write(self.binary_data_incoming)
            return True

        def verify(self):
            self.expect_merge_succeeded()
            self.expect_conflict_count(3)
            self.expect_merged(self.binary_file1)
            # self.expect_merged(self.binary_file2_transposed)
            self.expect_not_staged(self.binary_file1)
            self.expect_not_staged(self.binary_file2)
            # self.expect_not_staged(self.binary_file2_transposed)
            self.expect_conflicted(self.binary_file1)
            # self.expect_conflicted(self.binary_file2_transposed)
            self.expect_data_contents(self.binary_file1, self.binary_data_current)

    # Merge test for a successful merge of a path that conflicts because it's type (directory vs file) is different, by keeping both versions.
    class MergeTesterBinaryModifyTypeInvertedResolveKeepBoth(
        MergeTesterBinaryModifyTypeInvertedResolveNone
    ):
        def __init__(self):
            super().__init__("binary_modify_type_inverted_resolve_keep_both")

        def resolve(self):
            assert self.try_resolve_mine(self.binary_file1), (
                "Could not resolve original version."
            )
            assert self.try_resolve_path(self.binary_file2_transposed), (
                "Could not resolve transposed version."
            )
            message = "Resolve using both versions"
            assert self.try_commit(message), "Could not commit both versions."

    # Create list of merge tests to perform
    testers = []
    testers.append(MergeTesterNoMerge())
    testers.append(MergeTesterNoConflict())
    testers.append(MergeTesterTextModifyModifyConflict())
    testers.append(MergeTesterTextModifyModifyAutoMerge())
    testers.append(MergeTesterTextModifyModifyAutoResolve())
    testers.append(MergeTesterBinaryModifyModifyResolveNone())
    testers.append(MergeTesterBinaryModifyModifyResolveMine())
    testers.append(MergeTesterBinaryModifyModifyResolveTheirs())
    testers.append(MergeTesterBinaryModifyDeleteResolveNone())
    testers.append(MergeTesterBinaryDeleteModifyResolveNone())
    testers.append(MergeTesterBinaryDeleteModifyResolveMine())
    testers.append(MergeTesterBinaryDeleteModifyResolveTheirs())
    # testers.append(MergeTesterBinaryModifyTypeResolveNone())
    # testers.append(MergeTesterBinaryModifyTypeResolveKeepBoth())
    # testers.append(MergeTesterBinaryModifyTypeInvertedResolveNone())
    # testers.append(MergeTesterBinaryModifyTypeInvertedResolveKeepBoth())

    # Execute them all in the same repository
    for tester in testers:
        tester.run()

    # Specific merge flow tests


@pytest.mark.smoke
def test_merge_resolver(new_lore_repo):
    repo: Lore = new_lore_repo()

    with repo.open_file("merge.txt", "w+b") as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    repo.branch_create("feature-branch")

    with repo.open_file("merge.txt", "w+b") as output_file:
        output_file.write(os.urandom(2000))

    repo.make_dirs("feature-dir")
    with repo.open_file(
        os.path.join("feature-dir", "additional.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(12345))

    repo.stage(scan=True)
    repo.commit("Feature branch change")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge("feature-branch", dry_run=True)
    repo.branch_merge("feature-branch")
    repo.push()

    with repo.open_file("merge.txt", "w+b") as output_file:
        output_file.write(os.urandom(3000))

    repo.stage(scan=True)
    repo.commit("Main branch change")
    repo.push()
    repo.branch_switch("feature-branch")

    with repo.open_file("another.txt", "w+b") as output_file:
        output_file.write(os.urandom(4000))

    repo.stage(scan=True)
    repo.commit("Main branch change")
    repo.push()
    repo.branch_merge("main")
    repo.branch_push()

    repo.branch_switch("main")

    repo.branch_merge("feature-branch")
    repo.branch_push()


@pytest.mark.smoke
def test_merge_conflicting_directories(new_lore_repo):
    repo: Lore = new_lore_repo()

    with repo.open_file("base.txt", "w+b") as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Import")
    repo.push()
    repo.branch_create("branch-1")

    directory_path = "directory"
    repo.make_dirs(directory_path)
    with repo.open_file(
        os.path.join(directory_path, "merge.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Add in branch-1")
    repo.push()
    repo.branch_switch("main")
    repo.branch_create("branch-2")

    directory_path = "directory"
    repo.make_dirs(directory_path)
    with repo.open_file(
        os.path.join(directory_path, "merge.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Add in branch-2")
    repo.push()
    repo.branch_switch("main")
    repo.branch_merge("branch-1")
    repo.push()

    output = repo.branch_merge("branch-2")
    assert f"1 conflicted" in output, "Merge did NOT correctly identify the conflict"

    repo.branch_merge_abort()

    # Merge stacked branch after delete

    repo.branch_create("first-stacked-branch")

    directory_path = "testdir-stacked"
    repo.make_dirs(directory_path)
    with repo.open_file(os.path.join(directory_path, "file.txt"), "w+b") as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("First stacked branch")
    repo.push()

    repo.branch_create("second-stacked-branch")

    repo.make_dirs(directory_path)
    with repo.open_file(
        os.path.join(directory_path, "otherfile.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1200))

    repo.stage(scan=True)
    repo.commit("Second stacked branch")
    repo.push()

    repo.branch_archive("first-stacked-branch")
    repo.branch_switch("main")

    assert not repo.file_exists(os.path.join(directory_path, "file.txt")), (
        "File from first branch exist after switch to main"
    )
    assert not repo.file_exists(os.path.join(directory_path, "otherfile.txt")), (
        "File from second branch exist after switch to main"
    )

    repo.branch_merge("second-stacked-branch")
    repo.push()

    assert os.path.exists(os.path.join(repo.path, directory_path, "file.txt")), (
        "File from first branch not merged after stacked branch delete and merge"
    )
    assert os.path.exists(os.path.join(repo.path, directory_path, "otherfile.txt")), (
        "File from second branch not merged after stacked branch delete and merge"
    )

    # Multiple merges from branch

    directory_path = "multi-merge"
    repo.make_dirs(directory_path)
    with repo.open_file(os.path.join(directory_path, "file.txt"), "w+b") as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Create file for multi merge")
    repo.push()

    with repo.open_file(
        os.path.join(directory_path, "second.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Create another file for multi merge")
    repo.push()

    repo.branch_create("multi-merge")

    # This will later be used to generate a conflict before a merge
    with repo.open_file(
        os.path.join(directory_path, "second.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Modify second file in branch")
    repo.push()

    repo.branch_switch("main")

    with repo.open_file(
        os.path.join(directory_path, "third.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Create a third file for multi merge")
    repo.push()

    # Grab the file ID for the added file
    output = repo.repository_dump(os.path.join(directory_path, "third.txt"))

    file_id = ""
    for line in output.splitlines():
        if "third.txt id" in line:
            file_id = line.rsplit(
                "-",
            )[1]

    with repo.open_file(os.path.join(directory_path, "file.txt"), "w+b") as output_file:
        output_file.write(os.urandom(1100))

    repo.stage(scan=True)
    repo.commit("Modify first file for multi merge")
    repo.push()

    repo.branch_switch("multi-merge")

    repo.branch_merge("main")
    repo.push()

    # Ensure the file ID for the added file was maintained
    output = repo.repository_dump(os.path.join(directory_path, "third.txt"))

    merged_file_id = ""
    for line in output.splitlines():
        if "third.txt id" in line:
            merged_file_id = line.rsplit(
                "-",
            )[1]

    assert file_id == merged_file_id, (
        "File ID was not maintained across a merge (merge revision ID "
        + merged_file_id
        + ", source revision ID "
        + file_id
        + ")"
    )

    repo.branch_switch("main")

    # Generate a change that conflicts with main
    with repo.open_file(
        os.path.join(directory_path, "second.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1100))

    repo.stage(scan=True)
    repo.commit("Modify second file in main branch")

    with repo.open_file(os.path.join(directory_path, "file.txt"), "w+b") as output_file:
        output_file.write(os.urandom(1100))

    with repo.open_file(
        os.path.join(directory_path, "fourth.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1100))

    repo.stage(scan=True)
    repo.commit("Modify first file again and add a fourth file in main branch")
    repo.push()

    repo.branch_switch("multi-merge")

    repo.branch_merge("main")

    # This should fail since second.txt is in conflict
    with pytest.raises(CommitFailed):
        repo.commit("Merge main again into multi merge branch")

    # Resolve conflict
    repo.branch_merge_resolve_mine(os.path.join(directory_path, "second.txt"))
    repo.commit("Merge main again into multi merge branch")
    repo.push()

    os.chmod(os.path.join(repo.path, directory_path, "fourth.txt"), stat.S_IWRITE)
    repo.remove_file(os.path.join(directory_path, "fourth.txt"))

    repo.stage(scan=True)
    repo.commit("Remove fourth file in multi merge branch")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge("multi-merge")
    repo.push()

    assert not os.path.exists(os.path.join(directory_path, "fourth.txt")), (
        "Removed file not merged correctly"
    )

    repo.branch_switch("multi-merge")

    with repo.open_file(
        os.path.join(directory_path, "fourth.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1200))

    with repo.open_file(
        os.path.join(directory_path, "fifth.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1200))

    repo.stage(scan=True)
    repo.commit("Restore and add files on multi merge branch")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge("multi-merge")
    repo.push()

    # This should no longer conflict
    with repo.open_file(
        os.path.join(directory_path, "second.txt"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1100))

    repo.stage(scan=True)
    repo.commit("Modify second file again on main")
    repo.push()

    repo.branch_switch("multi-merge")
    repo.branch_merge("main")
    repo.push()

    # Make sure history weaving works when merging in a large number of deleted files
    repo.branch_switch("main")
    repo.branch_create("delete-files")

    for i in range(10):
        subpath = str(i)
        repo.make_dirs(subpath)
        for j in range(10):
            subsubpath = os.path.join(subpath, str(j))
            repo.make_dirs(subsubpath)
            for k in range(10):
                with repo.open_file(
                    os.path.join(subsubpath, str(k) + ".uasset"), "w+b"
                ) as output_file:
                    output_file.write(os.urandom(10))

    repo.stage(scan=True)
    repo.commit("Generate files")
    repo.push()
    for i in range(10):
        subpath = str(i)
        shutil.rmtree(os.path.join(repo.path, subpath), ignore_errors=True)

    repo.stage(scan=True)
    repo.commit("Delete files")
    repo.push()
    repo.branch_switch("main")
    repo.branch_merge("delete-files", dry_run=True)
    repo.branch_merge("delete-files")
    repo.branch_push()


@pytest.mark.smoke
def test_merge_deleted_files(new_lore_repo):
    repo: Lore = new_lore_repo()

    directory_path = "directory"
    with repo.open_file("base.txt", "w+b") as output_file:
        output_file.write(os.urandom(1000))
    repo.make_dirs(directory_path)
    with repo.open_file(os.path.join(directory_path, "file.one"), "w+b") as output_file:
        output_file.write(os.urandom(1000))
    with repo.open_file(os.path.join(directory_path, "file.two"), "w+b") as output_file:
        output_file.write(os.urandom(1000))
    with repo.open_file(
        os.path.join(directory_path, "file.three"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Import")
    repo.push()

    repo.branch_create("branch-1")
    repo.remove_file(os.path.join(directory_path, "file.one"))
    with repo.open_file(os.path.join(directory_path, "file.two"), "w+b") as output_file:
        output_file.write(os.urandom(2000))
    repo.remove_file(os.path.join(directory_path, "file.three"))

    repo.stage(scan=True)
    repo.commit("Modify and delete")
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start("branch-1", no_commit=True)

    output = repo.status()
    assert "C " not in output, f"Unexpected conflict found after merge: {output}"
    assert "Changes not staged for commit:" not in output, (
        f"Unexpected unstaged changes found after merge: {output}"
    )
    assert "D directory/file.one (M)" in output, (
        f"Expected file not staged as delete merge: {output}"
    )
    assert "D directory/file.three (M)" in output, (
        f"Expected file not staged as delete merge: {output}"
    )
    assert "M directory/file.two (M)" in output, (
        f"Expected file not staged as modified merge: {output}"
    )
