# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from error_types import NotSupportedError, RevisionNotFound
from lore import Lore, verify_signatures

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_syncfind(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Generate a file
    text_file = "binary-file.txt"

    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(100))

    repo.stage(scan=True)
    repo.commit("Test commit 1")

    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(100))

    repo.stage(scan=True)
    repo.commit("Test commit 2")

    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(100))

    repo.stage(scan=True)
    repo.commit("Test commit 3")
    repo.branch_push()

    # List all revisions
    list_output = repo.revision_history()
    verify_signatures(list_output, 3)

    # A partial hash signature is a form that is not supported, reported as such
    # rather than as a revision that could not be found
    with pytest.raises(NotSupportedError):
        repo.sync("abcdef1")

    # Digits alone are a revision number at every length but that of a whole
    # signature, so a truncated signature carrying no hex letter is read as the
    # number it spells rather than as a signature
    with pytest.raises(RevisionNotFound):
        repo.sync("2689572")

    # Which is what leaves a number free to be read as a revision number with no
    # `@` to mark it as one
    output = repo.sync("2")
    assert list_output[1].signature in output, "Failed to sync to bare revision 2"

    output = repo.sync("3")
    assert list_output[2].signature in output, "Failed to sync to bare revision 3"

    output = repo.sync("2~1")
    assert list_output[0].signature in output, "Failed to sync to bare revision 2~1"

    with pytest.raises(RevisionNotFound):
        repo.sync("4")

    # Revision numbering starts at one, so zero names no revision
    with pytest.raises(RevisionNotFound):
        repo.sync("0")

    repo.sync()

    output = repo.sync("@LATEST~1")
    assert list_output[1].signature in output, "Failed to sync to LATEST~1"

    output = repo.sync("@LATEST")
    assert list_output[2].signature in output, "Failed to sync to LATEST"

    # The `@` names a branch, so a target read on the current branch leaves it out
    output = repo.sync("LATEST~1")
    assert list_output[1].signature in output, "Failed to sync to bare LATEST~1"

    output = repo.sync("LATEST")
    assert list_output[2].signature in output, "Failed to sync to bare LATEST"

    output = repo.sync("latest")
    assert list_output[2].signature in output, "Failed to sync to bare lowercase latest"

    # A branch is still named with the `@` it needs, never read as a target
    with pytest.raises(RevisionNotFound):
        repo.sync("main")

    with pytest.raises(RevisionNotFound):
        repo.sync("@LATEST~3")

    with pytest.raises(RevisionNotFound):
        repo.sync("@TYPO")

    output = repo.sync("@3~1")
    assert list_output[1].signature in output, "Failed to sync to @3~1"

    output = repo.sync("@3")
    assert list_output[2].signature in output, "Failed to sync to @3"

    with pytest.raises(RevisionNotFound):
        repo.sync("@3~3")

    with pytest.raises(RevisionNotFound):
        repo.sync("@3~nonint")

    output = repo.sync(f"{list_output[2].signature}~1")
    assert list_output[1].signature in output, "Failed to sync to <LATEST revision>~1"

    output = repo.sync(f"{list_output[2].signature}")
    assert list_output[2].signature in output, "Failed to sync to <LATEST revision>"

    with pytest.raises(RevisionNotFound):
        repo.sync(f"{list_output[2].signature}~3")


@pytest.mark.smoke
def test_sync_branch_at_branch_point(new_lore_repo):
    """Naming a branch is what syncs onto it at the revision it was created at.

    That revision belongs to the branch it was created from and is the last one
    the two share, so its own metadata names the other branch — a bare signature
    for it lands there instead.
    """
    repo: Lore = new_lore_repo()

    text_file = "text.txt"

    for i in range(2):
        with repo.open_file(text_file, "a+") as output_file:
            output_file.writelines([f"main line {i + 1}\n"])
        repo.stage(scan=True)
        repo.commit(f"Main commit {i + 1}")
    repo.push()

    main_history = repo.revision_history()
    verify_signatures(main_history, 2)
    first_revision = main_history[0].signature
    branch_point = main_history[-1].signature

    # branch create switches to the new branch, so this commit lands on feature
    repo.branch_create("feature")
    with repo.open_file(text_file, "a+") as output_file:
        output_file.writelines(["feature line\n"])
    repo.stage(scan=True)
    repo.commit("Feature commit")
    repo.push()

    # Take main past the branch point, so syncing back to it is a move
    repo.branch_switch("main")
    with repo.open_file(text_file, "a+") as output_file:
        output_file.writelines(["main line 3\n"])
    repo.stage(scan=True)
    repo.commit("Main commit 3")
    repo.push()

    # The branch point belongs to main, so a bare signature stays on main
    output = repo.sync(branch_point)
    assert branch_point in output, "Failed to sync to the branch point"
    assert "On branch main" in repo.status(), "A bare signature left main"

    # Naming the branch moves onto it at that same revision
    repo.sync(f"feature@{branch_point}")
    status = repo.status()
    assert "On branch feature" in status, (
        f"Naming feature did not move onto it: {status}"
    )
    assert branch_point in status, "Not at the branch point after moving onto feature"

    # Naming a branch answers for the revision it was created at alone. Any other
    # revision is taken on the branch it records, so the first revision of main is
    # taken on main however it is named
    repo.sync(f"feature@{first_revision}")
    status = repo.status()
    assert first_revision in status, f"Failed to sync to the first revision: {status}"
    assert "On branch main" in status, (
        f"A revision main records was taken on another branch: {status}"
    )

    # The same move from a revision the working tree has to be realized away
    # from, rather than from the branch point it is already at
    repo.branch_switch("main")
    assert "On branch main revision 3" in repo.status(), "Not at main latest"

    repo.sync(f"feature@{branch_point}")
    status = repo.status()
    assert "On branch feature" in status, (
        f"Naming feature did not move onto it: {status}"
    )
    assert branch_point in status, "Not at the branch point after moving onto feature"

    # The branch answers for the revision reached, not the one named, so a walk
    # off the branch point lands on the branch that revision records
    repo.sync(f"feature@{branch_point}~1")
    status = repo.status()
    assert first_revision in status, f"Failed to walk from the branch point: {status}"
    assert "On branch main" in status, (
        f"A walked revision kept the branch that was named: {status}"
    )

    # A revision number and a signature name the branch point alike, so both are
    # taken on the branch that was named
    repo.branch_switch("main")
    repo.sync("feature@2")
    status = repo.status()
    assert branch_point in status, f"feature@2 is not the branch point: {status}"
    assert "On branch feature" in status, (
        f"A revision number naming the branch point did not move onto it: {status}"
    )

    # Switching to a branch takes the revision it was created at
    repo.branch_switch("feature", revision=f"feature@{branch_point}")
    assert "On branch feature" in repo.status(), "Failed to switch at the branch point"


@pytest.mark.smoke
def test_clone_and_pin_branch_at_branch_point(new_lore_repo):
    """A clone and a link pin take the branch a revision specifier names.

    Both otherwise read the branch from the revision's own metadata, which names
    the branch it was created from at a branch point.
    """
    source: Lore = new_lore_repo()

    with source.open_file("text.txt", "a+") as output_file:
        output_file.writelines(["main line\n"])
    source.stage(scan=True)
    source.commit("Main commit")
    source.push()

    branch_point = source.revision_info().signature

    source.branch_create("feature")
    with source.open_file("text.txt", "a+") as output_file:
        output_file.writelines(["feature line\n"])
    source.stage(scan=True)
    source.commit("Feature commit")
    source.push()

    clone = source.clone(revision=f"feature@{branch_point}")
    status = clone.status()
    assert "On branch feature" in status, f"Clone did not land on feature: {status}"
    assert branch_point in status, f"Clone is not at the branch point: {status}"

    # A tracking link mirrors the branch of the repository holding it, so the
    # pin names the branch only where the link keeps its own
    consumer: Lore = new_lore_repo()
    link_path = "linked"
    consumer.link_add(
        link_path,
        source.get_id(),
        "/",
        pin=f"feature@{branch_point}",
        disable_branching=True,
    )

    output = consumer.link_info(link_path)
    assert "Branch: feature" in output, f"The pin did not record feature: {output}"
    assert f"Revision: {branch_point}" in output, (
        f"The pin is not at the branch point: {output}"
    )
