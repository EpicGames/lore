# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import filecmp
import logging
import os

import pytest

from error_types import ImproperArgumentsError
from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_metadata(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    other_file = "other-File.txt"
    metadata_file1 = os.path.join(repo.path, "binary1.bin")
    metadata_file2 = os.path.abspath(os.path.join(repo.path, "binary2.bin"))

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    with repo.open_file(other_file, "w+") as output_file:
        output_file.writelines(
            ["One line\n", "Another line\n", "Third line\nFourth line\n"]
        )

    # Stage the files
    repo.stage(scan=True, offline=True)

    # Set metadata on text file
    repo.file_metadata_set(
        text_file,
        [
            "str_value",
            "I can resist everything except temptation.",
        ],
        offline=True,
    )

    metadata_file_size = 4096
    with repo.open_file(metadata_file1, "w+b") as output_file:
        output_file.write(os.urandom(metadata_file_size))

    repo.make_dirs(os.path.dirname(metadata_file2))
    with repo.open_file(metadata_file2, "w+b") as output_file:
        output_file.write(os.urandom(metadata_file_size))

    # Set binary metadata on text file
    repo.file_metadata_set(
        text_file,
        ["bin1_value", metadata_file1, "bin2_value", metadata_file2],
        binary=True,
        offline=True,
    )

    # Commit the files
    repo.commit("Test commit with file metadata", offline=True)

    # Verify the repository
    repo.repository_verify(offline=True)

    # Push the repository
    repo.push()

    # Modify text file
    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(
            ["One line\n", "Another line\n", "Third line\nYet another line\n"]
        )

    # Stage the files
    repo.stage(text_file, offline=True)

    # Set metadata on text file
    repo.file_metadata_set(
        other_file, ["str_value", "Added metadata without modifying file"], offline=True
    )

    # Commit the files
    repo.commit("Test commit adding file metadata without changing file", offline=True)

    # Verify the repository
    repo.repository_verify(offline=True)

    # Push the repository
    repo.branch_push()

    # Create a temporary directory for cloned repository
    text_file = "text-File.txt"
    other_file = "other-File.txt"

    # Clone the repository
    clone = repo.clone()

    # Get string metadata on text file
    clone.file_metadata_get(text_file, "str_value")

    # Get addresses of binary metadata on text file
    address1 = ""
    output = clone.file_metadata_get(text_file, "bin1_value")
    for line in output.splitlines():
        if line.startswith("bin1_value: "):
            address1 = line[12:]
    assert address1 != "", "Did not find expected address metadata"

    address2 = ""
    output = clone.file_metadata_get(text_file, "bin2_value")
    for line in output.splitlines():
        if line.startswith("bin2_value: "):
            address2 = line[12:]
    assert address2 != "", "Did not find expected address metadata"

    # Write binary metadata on text file to disk
    written_metadata_file1 = os.path.join(repo.path, "binary.bin1.downloaded")
    clone.file_write(address=address1, output=written_metadata_file1)

    written_metadata_file2 = os.path.join(repo.path, "binary.bin2.downloaded")
    clone.file_write(address=address2, output=written_metadata_file2)

    # Compare file contents
    filecmp.clear_cache()
    assert filecmp.cmp(
        metadata_file1,
        written_metadata_file1,
        shallow=False,
    ), "File identical check failed for " + written_metadata_file1

    filecmp.clear_cache()
    assert filecmp.cmp(
        metadata_file2,
        written_metadata_file2,
        shallow=False,
    ), "File identical check failed for " + written_metadata_file2

    # Get string metadata on other unchanged file
    output = clone.file_metadata_get(other_file, "str_value")

    assert "Added metadata without modifying file" in output, (
        "Metadata not found in clone test"
    )

    # Modify text file
    with clone.open_file(text_file, "w+") as output_file:
        output_file.writelines(
            [
                "One line\n",
                "Another line\n",
                "Third line\nYet another line\nModified in clone repo\n",
            ]
        )

    # Stage the files
    clone.stage(text_file)

    # Set metadata on other file
    clone.file_metadata_set(
        other_file,
        ["str_value", "Metadata set on an unchanged file again"],
        offline=True,
    )

    # Commit the files
    clone.commit(
        "Test commit again modifying file metadata without changing file", offline=True
    )

    # Verify the repository
    clone.repository_verify(offline=True)

    # Push the repository
    clone.branch_push()

    text_file = "text-File.txt"
    other_file = "other-File.txt"

    # Sync source repository
    repo.sync()

    # Get string metadata on other unchanged file
    output = repo.file_metadata_get(other_file, "str_value")

    assert "Metadata set on an unchanged file again" in output, (
        "Metadata not found in final test"
    )


def _staged_file(repo: Lore, name: str = "staged.txt") -> str:
    """Create and stage a file so file metadata can target it."""
    with repo.open_file(name, "w+") as output_file:
        output_file.write("content\n")
    repo.stage(scan=True, offline=True)
    return name


@pytest.mark.smoke
def test_file_metadata_set_single_arg_rejected(new_lore_repo):
    """A lone argument has no value; the set must be rejected, not panic.

    File metadata carries a per-path entry count and indexes keys/values by
    offset, so an odd pair count is the case most prone to an out-of-bounds
    panic. The trailing key without a value must be rejected up front.
    """
    repo: Lore = new_lore_repo()
    text_file = _staged_file(repo)

    with pytest.raises(ImproperArgumentsError):
        repo.file_metadata_set(text_file, ["lonely-key"], offline=True)


@pytest.mark.smoke
def test_file_metadata_set_odd_args_rejected(new_lore_repo):
    """An odd number of arguments leaves the trailing key without a value and
    must be rejected rather than dropping the key or panicking."""
    repo: Lore = new_lore_repo()
    text_file = _staged_file(repo)

    with pytest.raises(ImproperArgumentsError):
        repo.file_metadata_set(text_file, ["k1", "v1", "k2"], offline=True)


@pytest.mark.smoke
def test_file_metadata_set_binary_single_arg_rejected(new_lore_repo):
    """The binary branch reads the value as a file path; an odd pair count
    must be rejected before that indexing happens, not panic."""
    repo: Lore = new_lore_repo()
    text_file = _staged_file(repo)

    with pytest.raises(ImproperArgumentsError):
        repo.file_metadata_set(text_file, ["bin-key"], binary=True, offline=True)


@pytest.mark.smoke
def test_file_metadata_set_even_args_succeeds(new_lore_repo):
    """A well-formed key/value pair is accepted after the odd-args guard."""
    repo: Lore = new_lore_repo()
    text_file = _staged_file(repo)

    repo.file_metadata_set(text_file, ["str_value", "kept"], offline=True)

    output = repo.file_metadata_get(text_file, "str_value", offline=True)
    assert "kept" in output, f"Expected metadata value in output.\nGot: {output}"
