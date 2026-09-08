# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import filecmp
import logging
import os
import shutil

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_merge_restart(new_lore_repo, scratch_dir):
    repo: Lore = new_lore_repo()
    merge_backup_path = scratch_dir("backup_path", create=True)
    example_file = "example.txt"
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(["This is the first file.\n"])

    # Stage and commit example file
    repo.stage(example_file)
    repo.commit()

    # Create second example file
    example_file = "example2.txt"
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(["This is the second file.\n"])

    # Stage and commit second example file
    repo.stage(scan=True)
    repo.commit()

    # Create binary example file
    main_binary = os.urandom(800)
    with repo.open_file("example.bin", "w+b") as output_file:
        output_file.write(main_binary)

    # Stage and commit binary example file
    repo.stage(scan=True)
    repo.commit()

    # Push all the commits from main
    repo.push()

    # Create and switch to a branch
    repo.branch_create("test-branch")

    # Create a third example file
    example_file = "example3.txt"
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(["This is the third file.\n"])

    # Modify an existing file (example2.txt)
    example_file = "example2.txt"
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(["This is a conflict from the branch.\n"])

    # Modify the binary file (example.bin)
    branch_binary = os.urandom(1000)
    with repo.open_file("example.bin", "w+b") as output_file:
        output_file.write(branch_binary)

    # Stage commit and push the changes on the branch
    repo.stage(scan=True)
    repo.commit("branch one")
    repo.push()

    # Switch back to the main branch
    repo.branch_switch("main")

    # Make a conflict in example2.txt
    example_file = "example2.txt"
    with repo.open_file(example_file, "w+") as output_file:
        output_file.writelines(["This is a conflict from main.\n"])

    # Make a conflict on binary file
    main_confict_binary = os.urandom(1024)
    with repo.open_file("example.bin", "w+b") as output_file:
        output_file.write(main_confict_binary)

    # Stage commit and push the conflicts
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    output = repo.branch_merge("test-branch", debug=True)

    # Expect the right number of changes and cnflicts reported
    if "1 changes and 2 conflicts" not in output:
        print("Expected changes and conflicts not found after merge of test branch")
        exit(-1)

    # Copy file state before making changes in merge
    shutil.copyfile(
        os.path.join(repo.path, "example.txt"),
        os.path.join(merge_backup_path, "example.txt"),
    )

    shutil.copyfile(
        os.path.join(repo.path, "example2.txt"),
        os.path.join(merge_backup_path, "example2.txt"),
    )

    shutil.copyfile(
        os.path.join(repo.path, "example3.txt"),
        os.path.join(merge_backup_path, "example3.txt"),
    )

    shutil.copyfile(
        os.path.join(repo.path, "example.bin"),
        os.path.join(merge_backup_path, "example.bin"),
    )

    # Stage 'theirs' for all files
    repo.branch_merge_resolve_theirs("example.txt")

    repo.branch_merge_resolve_theirs("example2.txt")

    repo.branch_merge_resolve_theirs("example3.txt")

    repo.branch_merge_resolve_theirs("example.bin")

    # Expect the version from the branch to be on disk
    example_file = "example2.txt"
    with repo.open_file(example_file, "rt") as output_file:
        line = output_file.readline()
        assert "This is a conflict from the branch.\n" in line, (
            f"Stage theirs had incorrect text in {example_file}\nExpected:\nThis is a conflict from the branch.\nFound: {line}"
        )

    with repo.open_file("example.bin", "rb") as output_file:
        binary = output_file.read()
        if binary != branch_binary:
            print("Stage theirs had incorrect binary")
            exit(-1)

    output = repo.branch_merge_restart("example.txt")

    assert "0 changes and 0 conflicts" in output, (
        "Expected changes and conflicts not found after merge restart of example.txt"
    )

    output = repo.branch_merge_restart("example2.txt", debug=True)

    assert "0 changes and 1 conflicts" in output, (
        "Expected changes and conflicts not found after merge restart of example2.txt"
    )

    output = repo.branch_merge_restart("example3.txt")

    assert "1 changes and 0 conflicts" in output, (
        "Expected changes and conflicts not found after merge restart of example3.txt"
    )

    output = repo.branch_merge_restart("example.bin")

    assert "0 changes and 1 conflicts" in output, (
        "Expected changes and conflicts not found after merge restart of example.bin"
    )

    if not filecmp.cmp(
        os.path.join(repo.path, "example.txt"),
        os.path.join(merge_backup_path, "example.txt"),
        False,
    ):
        logger.critical("file example.txt differs after restart")
        logger.critical(
            "Got "
            + str(os.path.getsize(os.path.join(repo.path, "example.txt")))
            + " bytes:"
        )
        with repo.open_file("example.txt", "r") as file:
            logger.critical(file.read())
        logger.critical(
            "Expected "
            + str(os.path.getsize(os.path.join(merge_backup_path, "example.txt")))
            + " bytes:"
        )
        with open(os.path.join(merge_backup_path, "example.txt"), "r") as file:
            logger.critical(file.read())
        assert False, "Files differ after restart"

    if not filecmp.cmp(
        os.path.join(repo.path, "example2.txt"),
        os.path.join(merge_backup_path, "example2.txt"),
        False,
    ):
        logger.critical("file example2.txt differs after restart")
        logger.critical(
            "Got "
            + str(os.path.getsize(os.path.join(repo.path, "example2.txt")))
            + " bytes:"
        )
        with repo.open_file("example2.txt", "r") as file:
            logger.critical(file.read())
        logger.critical(
            "Expected "
            + str(os.path.getsize(os.path.join(merge_backup_path, "example2.txt")))
            + " bytes:"
        )
        with open(os.path.join(merge_backup_path, "example2.txt"), "r") as file:
            logger.critical(file.read())
        assert False, "Files differ after restart"

    if not filecmp.cmp(
        os.path.join(repo.path, "example3.txt"),
        os.path.join(merge_backup_path, "example3.txt"),
        False,
    ):
        logger.critical("file example3.txt differs after restart")
        logger.critical(
            "Got "
            + str(os.path.getsize(os.path.join(repo.path, "example3.txt")))
            + " bytes:"
        )
        with repo.open_file("example3.txt", "r") as file:
            logger.critical(file.read())
        logger.critical(
            "Expected "
            + str(os.path.getsize(os.path.join(merge_backup_path, "example3.txt")))
            + " bytes:"
        )
        with open(os.path.join(merge_backup_path, "example3.txt"), "r") as file:
            logger.critical(file.read())
        assert False, "Files differ after restart"

    if not filecmp.cmp(
        os.path.join(repo.path, "example.bin"),
        os.path.join(merge_backup_path, "example.bin"),
        False,
    ):
        logger.critical("file example.bin differs after restart")
        assert False, "Files differ after restart"
