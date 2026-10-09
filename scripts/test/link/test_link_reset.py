# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import os

import pytest
from error_types import LoreException
from link_helpers import assert_crr_clean, make_parent_with_link, make_repo
from lore_parsers import parse_status_json

from lore import Lore


@pytest.mark.smoke
def test_link_reset(new_lore_repo):
    """Test resetting files within linked repositories."""
    repo: Lore = new_lore_repo()

    # Create source repository
    source_file = "source-file.txt"
    with repo.open_file(source_file, "w+") as output_file:
        output_file.writelines(["source repository content\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create link repository with multiple files
    link_repo = new_lore_repo()

    link_file1 = "file1.txt"
    link_file2 = "file2.txt"
    link_subdir_file = "subdir/file3.txt"
    link_deep_file = "deep/path/file4.txt"

    with link_repo.open_file(link_file1, "w+") as output_file:
        output_file.writelines(["link file 1 original\n"])

    with link_repo.open_file(link_file2, "w+") as output_file:
        output_file.writelines(["link file 2 original\n"])

    link_repo.make_dirs("subdir")
    with link_repo.open_file(link_subdir_file, "w+") as output_file:
        output_file.writelines(["link subdir file original\n"])

    link_repo.make_dirs(os.path.dirname(link_deep_file))
    with link_repo.open_file(link_deep_file, "w+") as output_file:
        output_file.writelines(["link deep file original\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    # Add link to main repository
    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")

    expected_file1 = f"{link_path}/{link_file1}"
    expected_file2 = f"{link_path}/{link_file2}"
    expected_subdir_file = f"{link_path}/{link_subdir_file}"
    expected_deep_file = f"{link_path}/{link_deep_file}"

    # Verify link files exist
    assert repo.compare_file(repo, expected_file1)
    assert repo.compare_file(repo, expected_file2)
    assert repo.compare_file(repo, expected_subdir_file)
    assert repo.compare_file(repo, expected_deep_file)

    repo.commit()
    repo.push()

    # Test 1: Reset a single modified file inside a link
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1\n"])

    repo.reset(expected_file1)

    with repo.open_file(expected_file1, "r") as f:
        content = f.read()
        assert "MODIFIED" not in content, "File1 should be restored after reset"
        assert "link file 1 original" in content, "File1 should have original content"

    # Verify other files are unaffected
    with repo.open_file(expected_file2, "r") as f:
        assert "link file 2 original" in f.read(), "File2 should be unaffected"

    # Test 2: Reset a modified file in a link subdirectory
    with repo.open_file(expected_subdir_file, "w+") as output_file:
        output_file.writelines(["MODIFIED subdir file\n"])

    repo.reset(expected_subdir_file)

    with repo.open_file(expected_subdir_file, "r") as f:
        content = f.read()
        assert "MODIFIED" not in content, "Subdir file should be restored after reset"
        assert "link subdir file original" in content, (
            "Subdir file should have original content"
        )

    # Test 3: Reset an entire linked subdirectory
    with repo.open_file(expected_subdir_file, "w+") as output_file:
        output_file.writelines(["MODIFIED subdir file again\n"])

    untracked_file = f"{link_path}/subdir/untracked.txt"
    with repo.open_file(untracked_file, "w+") as output_file:
        output_file.writelines(["untracked file content\n"])

    repo.reset(f"{link_path}/subdir")

    with repo.open_file(expected_subdir_file, "r") as f:
        content = f.read()
        assert "MODIFIED" not in content, (
            "Subdir file should be restored after directory reset"
        )
        assert "link subdir file original" in content, (
            "Subdir file should have original content"
        )

    # Untracked file should still exist (purge is off)
    assert repo.file_exists(untracked_file), (
        "Untracked file should still exist without purge"
    )

    # Clean up untracked file
    repo.remove_file(untracked_file)

    # Test 4: Reset the entire link directory
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1 for test 4\n"])

    with repo.open_file(expected_file2, "w+") as output_file:
        output_file.writelines(["MODIFIED file 2 for test 4\n"])

    with repo.open_file(expected_deep_file, "w+") as output_file:
        output_file.writelines(["MODIFIED deep file for test 4\n"])

    repo.remove_file(expected_subdir_file)

    repo.reset(link_path)

    with repo.open_file(expected_file1, "r") as f:
        assert "link file 1 original" in f.read(), (
            "File1 should be restored after link reset"
        )

    with repo.open_file(expected_file2, "r") as f:
        assert "link file 2 original" in f.read(), (
            "File2 should be restored after link reset"
        )

    with repo.open_file(expected_deep_file, "r") as f:
        assert "link deep file original" in f.read(), (
            "Deep file should be restored after link reset"
        )

    assert repo.file_exists(expected_subdir_file), (
        "Deleted subdir file should be restored after link reset"
    )
    with repo.open_file(expected_subdir_file, "r") as f:
        assert "link subdir file original" in f.read(), (
            "Restored subdir file should have original content"
        )

    # Test 5: Reset entire repository traverses into links
    with repo.open_file(source_file, "w+") as output_file:
        output_file.writelines(["MODIFIED source file\n"])

    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED file 1 for test 5\n"])

    repo.reset(".")

    with repo.open_file(source_file, "r") as f:
        assert "source repository content" in f.read(), (
            "Source file should be restored after root reset"
        )

    with repo.open_file(expected_file1, "r") as f:
        assert "link file 1 original" in f.read(), (
            "Linked file should be restored after root reset"
        )

    # Test 6: Reset with purge removes untracked files in links
    untracked_link_file = f"{link_path}/untracked-purge.txt"
    with repo.open_file(untracked_link_file, "w+") as output_file:
        output_file.writelines(["untracked file for purge test\n"])

    assert repo.file_exists(untracked_link_file), (
        "Untracked file should exist before purge reset"
    )

    repo.reset(link_path, purge=True)

    assert not repo.file_exists(untracked_link_file), (
        "Untracked file should be deleted after purge reset"
    )

    # Test 7: Reset with multiple links
    second_link_repo = new_lore_repo()

    second_link_file = "second-file.txt"
    with second_link_repo.open_file(second_link_file, "w+") as output_file:
        output_file.writelines(["second link file original\n"])

    second_link_repo.stage(scan=True)
    second_link_repo.commit()
    second_link_repo.push()

    second_link_path = "other-link"
    repo.link_add(second_link_path, second_link_repo.get_id(), "/")

    expected_second_file = f"{second_link_path}/{second_link_file}"
    assert repo.compare_file(repo, expected_second_file)

    repo.commit()
    repo.push()

    # Modify files in both links
    with repo.open_file(expected_file1, "w+") as output_file:
        output_file.writelines(["MODIFIED first link file\n"])

    with repo.open_file(expected_second_file, "w+") as output_file:
        output_file.writelines(["MODIFIED second link file\n"])

    repo.reset(".")

    with repo.open_file(expected_file1, "r") as f:
        assert "link file 1 original" in f.read(), (
            "First link file should be restored after multi-link root reset"
        )

    with repo.open_file(expected_second_file, "r") as f:
        assert "second link file original" in f.read(), (
            "Second link file should be restored after multi-link root reset"
        )


@pytest.mark.smoke
@pytest.mark.xfail(
    strict=True,
    reason="reset of a staged file inside a link discards the staged edit instead "
    "of refusing, as the same reset does outside a link",
)
def test_link_reset_refuses_a_staged_file_inside_a_link(new_lore_repo):
    """`reset` refuses a file whose change is staged, inside a link as outside.

    Outside a link the reset fails with `Failed to reset staged node` and leaves
    the edit alone. Inside a link it currently restores the committed content and
    reports success, which loses the staged edit.
    """
    link_path = "linked"
    staged_file = f"{link_path}/file1.txt"
    repo, _link_repo = make_parent_with_link(
        new_lore_repo, link_path, {"file1.txt": "link file 1 original\n"}
    )

    repo.write_files({staged_file: "MODIFIED file 1 for staged test\n"})
    repo.stage(staged_file)

    with pytest.raises(LoreException):
        repo.reset(staged_file)

    with repo.open_file(staged_file) as f:
        assert f.read() == "MODIFIED file 1 for staged test\n", (
            "A refused reset must leave the staged edit on disk"
        )


@pytest.mark.smoke
def test_link_reset_honours_a_directory_rule_naming_the_mount(new_lore_repo):
    """A directory rule naming a link mount excludes the content mounted there.

    The mount node is a link, not a directory, so the rule does not match the
    node itself. It matches the mount path, which is what the content below it
    sits in, and every walk folds the mount path that way. Reset has to reach
    the same verdict or it restores files the filter excludes.
    """
    repo: Lore = new_lore_repo()

    outside_file = "outside.txt"
    with repo.open_file(outside_file, "w+") as output_file:
        output_file.writelines(["outside original\n"])

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    link_repo = new_lore_repo()
    link_file = "inside.txt"
    with link_repo.open_file(link_file, "w+") as output_file:
        output_file.writelines(["inside original\n"])

    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    mounted_file = f"{link_path}/{link_file}"
    assert repo.compare_file(repo, mounted_file)

    # A rooted directory rule naming the mount, the shape a sparse view uses.
    with repo.open_file(repo.ignore_file(), "w+") as ignore_file:
        ignore_file.write(f"/{link_path}/\n")

    with repo.open_file(mounted_file, "w+") as output_file:
        output_file.writelines(["inside modified\n"])
    with repo.open_file(outside_file, "w+") as output_file:
        output_file.writelines(["outside modified\n"])

    repo.reset(".")

    with repo.open_file(outside_file, "r") as f:
        assert "outside original" in f.read(), (
            "Reset should restore a file the filter does not exclude"
        )
    with repo.open_file(mounted_file, "r") as f:
        assert "inside modified" in f.read(), (
            "Reset should not descend into a link mount the filter excludes"
        )


# ---------------------------------------------------------------------------
# Reset for added/removed/updated links.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_link_reset_staged_add(new_lore_repo):
    """`lore file reset` undoes a staged-add link: registry, on-disk content,
    and parent state are restored to pre-add."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_files = ["a.txt", "nested/b.txt"]
    source_repo = make_repo(
        new_lore_repo,
        {path: f"link source content for {path}\n" for path in source_files},
    )

    link_path = "linked"
    expected = [f"{link_path}/{p}" for p in source_files]

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    for p in expected:
        assert main_repo.file_exists(p), f"Pre-reset: expected {p} on disk"
    assert source_repo.get_id() in main_repo.link_list()

    main_repo.reset(link_path)

    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=expected,
        expected_link_registry={source_repo.get_id(): False, link_path: False},
    )


@pytest.mark.smoke
def test_link_reset_staged_add_reports_reset(new_lore_repo):
    """Resetting a staged-add link counts the node it reset in the summary."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"a.txt": "link source content\n"})

    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    output = main_repo.reset(link_path, json=True)

    events = [json.loads(line) for line in output.splitlines() if line.strip()]
    reset_end = next(e for e in events if e.get("tagName") == "fileResetEnd")
    count = reset_end["data"]["count"]
    total = (
        count["directoryResetCount"]
        + count["directoryDeleteCount"]
        + count["fileResetCount"]
        + count["fileDeleteCount"]
    )
    assert total >= 1, f"Reset of a staged link must count the reset, got: {count}"


@pytest.mark.smoke
def test_link_reset_staged_add_into_existing_directory(new_lore_repo):
    """If the link path was a committed directory before `link add`, reset
    restores the empty directory rather than removing it."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"a.txt": "link source content\n"})

    # Commit an empty directory at the link path so it predates the link add.
    link_path = "linked"
    main_repo.make_dirs(link_path)
    main_repo.stage(scan=True)
    main_repo.commit("add empty linked directory")
    main_repo.push()

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    assert main_repo.file_exists(f"{link_path}/a.txt")

    main_repo.reset(link_path)

    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )
    assert main_repo.path_exists(link_path), (
        "Pre-existing committed directory must survive reset"
    )


@pytest.mark.smoke
def test_link_reset_staged_add_creates_parent_directories(new_lore_repo):
    """If `link add` had to auto-stage parent directories, reset removes
    both the link and the auto-staged parents."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"a.txt": "link source content\n"})

    link_path = "deep/parent/chain/linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    staged_paths = {e["path"] for e in parse_status_json(main_repo.status(json=True))}
    assert any(p.startswith("deep") for p in staged_paths), (
        f"Auto-staged parents should be visible in pre-reset status, got: {staged_paths}"
    )

    main_repo.reset(link_path)

    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )
    assert not main_repo.path_exists("deep"), (
        "Parent chain that didn't exist before the link must be gone"
    )


@pytest.mark.smoke
def test_link_reset_staged_remove(new_lore_repo):
    """`lore file reset` of a staged-remove link restores the registry, the
    link node, and re-materializes content on disk."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_files = ["a.txt", "nested/b.txt"]
    source_repo = make_repo(
        new_lore_repo,
        {path: f"link source content for {path}\n" for path in source_files},
    )

    link_path = "linked"
    expected = [f"{link_path}/{p}" for p in source_files]

    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.commit("add link")
    main_repo.push()

    main_repo.link_remove(link_path)
    for p in expected:
        assert not main_repo.file_exists(p), (
            f"Pre-reset: expected {p} absent after link_remove"
        )

    main_repo.reset(link_path)

    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", *expected],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )
    for p in expected:
        source_relative = p.removeprefix(f"{link_path}/")
        assert main_repo.compare_file(
            source_repo, p, other_file_path=source_relative
        ), f"Restored content of {p} must match source repo content"


@pytest.mark.smoke
def test_link_reset_staged_update(new_lore_repo):
    """`lore file reset` of a staged pin change restores the previous pin in
    the registry and re-realizes content from the previous pin."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})

    source_repo = make_repo(
        new_lore_repo,
        {
            "v1.txt": "v1\n",
        },
    )

    source_repo.branch_create("feature")
    with source_repo.open_file("v2.txt", "w+") as f:
        f.writelines(["v2\n"])
    source_repo.stage(scan=True)
    source_repo.commit("v2")
    source_repo.push()

    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/", pin="main@LATEST")
    main_repo.commit("add link")
    main_repo.push()

    pre_link_list = main_repo.link_list()
    assert main_repo.file_exists(f"{link_path}/v1.txt")
    assert not main_repo.file_exists(f"{link_path}/v2.txt")

    main_repo.link_update(link_path, pin="feature@LATEST")
    assert main_repo.file_exists(f"{link_path}/v2.txt"), (
        "Pre-reset: link_update should have realized v2.txt"
    )

    main_repo.reset(link_path)

    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", f"{link_path}/v1.txt"],
        expected_files_absent=[f"{link_path}/v2.txt"],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )
    assert main_repo.link_list() == pre_link_list, (
        "Registry must match pre-update exactly (branch + signature)"
    )


@pytest.mark.smoke
def test_link_reset_root_handles_staged_link_changes(new_lore_repo):
    """Root reset (`reset(".")`) traverses into the link node and reverts
    a staged add/remove/update there."""
    # --- staged-add ---
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"a.txt": "link source content\n"})
    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.reset(".")
    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )

    # --- staged-remove ---
    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.commit("add link")
    main_repo.push()
    main_repo.link_remove(link_path)
    main_repo.reset(".")
    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", f"{link_path}/a.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )

    # --- staged-update ---
    source_repo.branch_create("feature")
    with source_repo.open_file("a.txt", "w+") as f:
        f.writelines(["feature a\n"])
    source_repo.stage(scan=True)
    source_repo.commit("feature a")
    source_repo.push()

    pre_link_list = main_repo.link_list()
    main_repo.link_update(link_path, pin="feature@LATEST")
    main_repo.reset(".")
    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt", f"{link_path}/a.txt"],
        expected_files_absent=[],
        expected_link_registry={source_repo.get_id(): True, link_path: True},
    )
    assert main_repo.link_list() == pre_link_list, (
        "Registry must match pre-update after root reset"
    )


@pytest.mark.smoke
def test_link_reset_does_not_affect_unrelated_links(new_lore_repo):
    """Reset of one staged link change must not touch unrelated committed
    links."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_repo_a = make_repo(new_lore_repo, {"a.txt": "link source content\n"})
    source_repo_b = make_repo(new_lore_repo, {"b.txt": "link source content\n"})

    link_a = "linkA"
    link_b = "linkB"

    main_repo.link_add(link_a, source_repo_a.get_id(), "/")
    main_repo.link_add(link_b, source_repo_b.get_id(), "/")
    main_repo.commit("add both links")
    main_repo.push()

    # Stage a remove on linkA only; reset only that. linkB untouched.
    main_repo.link_remove(link_a)
    main_repo.reset(link_a)

    assert_crr_clean(
        main_repo,
        expected_files_present=[
            "main.txt",
            f"{link_a}/a.txt",
            f"{link_b}/b.txt",
        ],
        expected_files_absent=[],
        expected_link_registry={
            source_repo_a.get_id(): True,
            link_a: True,
            source_repo_b.get_id(): True,
            link_b: True,
        },
    )


@pytest.mark.smoke
def test_link_reset_idempotent(new_lore_repo):
    """Running reset twice on the same staged-add link change is a no-op
    on the second call."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"a.txt": "link source content\n"})

    link_path = "linked"
    main_repo.link_add(link_path, source_repo.get_id(), "/")
    main_repo.reset(link_path)
    # Second reset is a no-op (path no longer exists).
    main_repo.reset(link_path)

    assert_crr_clean(
        main_repo,
        expected_files_present=["main.txt"],
        expected_files_absent=[f"{link_path}/a.txt"],
        expected_link_registry={source_repo.get_id(): False},
    )
