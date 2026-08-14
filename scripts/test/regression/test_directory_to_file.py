import os

import pytest

from lore import Lore


@pytest.mark.regression
def test_directory_to_file_sync(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Create a file that will later become a directory
    # file_that_becomes_a_directory = "ExtremelyLongFileAndDirectoryNamesUnderThisVeryLongDirectoryToTestEllipsisAndClippingWithFirefoxChromeAndSafari"
    file_that_becomes_a_directory = "file_to_directory"
    file_contents = os.urandom(1024)
    repo.write_commit_push(None, {file_that_becomes_a_directory: file_contents})
    assert repo.open_file(file_that_becomes_a_directory, "rb").read() == file_contents

    # Remove the file
    os.remove(repo._fix_path(file_that_becomes_a_directory))
    repo.stage(".", scan=True)
    repo.commit(".")

    # Create a directory with the same name as that file and create a new file nested in it
    os.mkdir(repo._fix_path(file_that_becomes_a_directory))
    nested_file_name = "nested"
    repo.write_commit_push(
        None,
        {
            os.path.join(file_that_becomes_a_directory, nested_file_name): os.urandom(
                1024
            )
        },
    )

    # Sync back to main@1, which needs to turn the directory back into a file
    repo.sync("main@1")
    assert repo.open_file(file_that_becomes_a_directory, "rb").read() == file_contents


@pytest.mark.regression
def test_directory_to_file_reset(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Create a file that will later become a directory
    # file_that_becomes_a_directory = "ExtremelyLongFileAndDirectoryNamesUnderThisVeryLongDirectoryToTestEllipsisAndClippingWithFirefoxChromeAndSafari"
    file_that_becomes_a_directory = "file_to_directory"
    file_contents = os.urandom(1024)
    repo.write_commit_push(None, {file_that_becomes_a_directory: file_contents})
    assert repo.open_file(file_that_becomes_a_directory, "rb").read() == file_contents

    # Remove the file
    os.remove(repo._fix_path(file_that_becomes_a_directory))
    repo.stage(".", scan=True)
    repo.commit(".")

    # Create a directory with the same name as that file and create a new file nested in it
    os.mkdir(repo._fix_path(file_that_becomes_a_directory))
    nested_file_name = "nested"
    repo.write_commit_push(
        None,
        {
            os.path.join(file_that_becomes_a_directory, nested_file_name): os.urandom(
                1024
            )
        },
    )

    # Reset back to main@1, which needs to turn the directory back into a file
    repo.reset(revision="main@1")

    assert repo.open_file(file_that_becomes_a_directory, "rb").read() == file_contents
