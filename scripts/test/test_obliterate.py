# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import shutil

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_obliterate(new_lore_repo, tmp_path_factory):
    repo: Lore = new_lore_repo()
    temp = tmp_path_factory.mktemp("obliterate")

    # Generate some files
    multi_reference_file_one = "multi-ref-file-one.txt"
    multi_reference_file_two = os.path.join("subdir", "multi-ref-file-two.txt")
    multi_fragment_file = "multi-frag-file.txt"
    multi_fragment_file_backup = os.path.join(temp, "multi-frag-file.txt")

    multi_reference_content = ["First line\n", "Second line\n", "Third line\n"]

    with repo.open_file(multi_reference_file_one, "w+") as output_file:
        output_file.writelines(multi_reference_content)

    repo.make_dirs(os.path.dirname(multi_reference_file_two))
    with repo.open_file(multi_reference_file_two, "w+") as output_file:
        output_file.writelines(multi_reference_content)

    with repo.open_file(multi_fragment_file, "w+b") as output_file:
        output_file.write(os.urandom(3456789))

    # Store a backup of a file so we can try and re-add it later
    shutil.copy(os.path.join(repo.path, multi_fragment_file), multi_fragment_file_backup)

    # Stage the files
    repo.stage(scan=True, offline=True)

    # Commit the files
    repo.commit(offline=True)

    # Obliterate first multi-reference file
    output = repo.file_obliterate(path=multi_reference_file_one, offline=True)

    assert "Obliterated 1" in output, "Did not obliterate first multi-reference file"
    assert "0 payloads" in output, "Obliterated payload with an existing reference"

    # Check first multi-reference file. An obliterated address is absent rather than described: the
    # store reports no match instead of a tombstone carrying its flags and sizes.
    output = repo.file_dump(path=multi_reference_file_one, offline=True)

    assert "Store match : 0" in output, (
        "First multi-reference file still resolves after being obliterated"
    )
    # The whole line, not `": 100"`: a bare substring matches an address that
    # happens to begin `100`, which one dumped fragment in 4,096 does, and the
    # content is random.
    assert "Store match : 100" not in output, (
        "Obliterated fragment was described rather than reported absent"
    )

    # Obliterate second multi-reference file
    output = repo.file_obliterate(path=multi_reference_file_two, offline=True)

    assert "Obliterated 1" in output, "Did not obliterate first multi-reference file"
    assert "1 payloads" in output, "Obliterated payload with an existing reference"

    # Obliterate multi-fragment file
    output = repo.file_obliterate(path=multi_fragment_file, offline=True)

    assert not "Obliterated 0" in output, "Did not obliterate multi-fragment file"
    assert not "removed 0 payloads" in output, (
        "Did not obliterate payloads of multi-fragment file"
    )

    # Try to obliterate multi-fragment file again
    output = repo.file_obliterate(path=multi_fragment_file, offline=True)

    assert "already obliterated" in output, (
        "Fragment already obliterated, warning not shown"
    )

    # Check multi-fragment file
    output = repo.file_dump(path=multi_fragment_file, offline=True)

    assert "Store match : 0" in output, (
        "Multi-fragment file still resolves after being obliterated"
    )
    # The whole line, not `": 100"`: a bare substring matches an address that
    # happens to begin `100`, which one dumped fragment in 4,096 does, and the
    # content is random.
    assert "Store match : 100" not in output, (
        "Obliterated fragment was described rather than reported absent"
    )

    # Check whether there are files staged after obliteration
    output = repo.repository_status(offline=True)

    assert "staged for commit" in output, "No files staged after obliterate"

    # Restore the multi-fragment file and then try and re-add it
    shutil.copy(multi_fragment_file_backup, os.path.join(repo.path, multi_fragment_file))

    # Stage the files
    repo.stage(scan=True, offline=True)

    # Commit the files
    repo.commit(offline=True)
