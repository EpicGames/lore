# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from lore import Lore
from lore_parsers import parse_jsonl

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_info(new_lore_repo):
    repo: Lore = new_lore_repo("Info")

    # Generate some files
    text_file = "path/to/file.txt"
    another_file = "another/path.txt"

    repo.make_dirs(os.path.dirname(text_file))
    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(1000))

    repo.make_dirs(os.path.dirname(another_file))
    with repo.open_file(another_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    # Stage the files
    repo.stage([text_file, another_file], offline=True)
    repo.commit(offline=True)

    # Describe
    files = repo.file_info([text_file, another_file], offline=True)

    file_paths = [file.path for file in files]

    assert text_file in file_paths, "Missing file in info output"
    assert another_file in file_paths, "Missing file in info output"

    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(1000))

    repo.remove_file(another_file)

    # Describe
    files = repo.file_info([text_file, another_file], offline=True, local=True)

    assert len(files) == 2, "Unexpected number of files in output"

    file = [file for file in files if text_file in file.path][0]
    assert file.status == "Modified", "Missing file status in info output"

    file = [file for file in files if another_file in file.path][0]
    assert file.status == "Deleted", "Missing file status in info output"


@pytest.mark.smoke
def test_revision_info_delta_move_carries_the_source_path(new_lore_repo):
    """The delta entry for a moved file must name the path it moved from. The
    entry reports the node at its new path, so without the source path a
    consumer reads the move as an add."""
    repo: Lore = new_lore_repo("InfoMove")

    original_path = "original-file.txt"
    moved_path = "renamed-file.txt"

    with repo.open_file(original_path, "w+") as output_file:
        output_file.write("Initial content\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Add the file", offline=True)

    repo.move(original_path, moved_path)
    repo.file_stage_move(original_path, moved_path, offline=True)
    repo.commit("Move the file", offline=True)

    output = repo.revision_info(delta=True, json=True, offline=True)
    deltas = parse_jsonl(output, "revisionInfoDelta")
    moved = [delta for delta in deltas if delta["path"] == moved_path]

    assert len(moved) == 1, f"Expected exactly one delta at {moved_path}, got {deltas}"
    assert moved[0]["action"] == "move", (
        f"The delta at {moved_path} should be a move, got {moved[0]}"
    )
    assert moved[0]["fromPath"] == original_path, (
        f"The move should report fromPath={original_path!r}, got {moved[0]}"
    )

    # Called through `run` because `revision_info` parses its output
    # into a RevisionInfo, which drops the delta lines.
    output = repo.run(["revision", "info", "--delta"], offline=True)
    lines = [" ".join(line.split()) for line in output.splitlines()]
    assert f"V {original_path} -> {moved_path}" in lines, (
        f"Revision info did not print the move source path, got:\n{output}"
    )
