# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import os

import pytest
from error_types import ImproperArgumentsError, LocalChanges, NotSupportedError
from link_helpers import (
    assert_crr_clean,
    build_nested_link_repos_at,
    make_parent_with_link,
    make_repo,
    make_repo_renumbering_folder,
)
from lore_parsers import parse_status_json
from test_utils import has_staged_anchor, status_by_path

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


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
@pytest.mark.parametrize("layout", ["link", "nested", "repinned"])
def test_link_reset_refuses_a_staged_file_inside_a_link(new_lore_repo, layout):
    """`reset` refuses a staged file inside a link, and a directory holding one, as it
    does outside a link, and keeps the staged edit. An unstaged edit beside it still
    resets. That holds in a link inside a link, and in a link re-pinned and not yet
    committed to a revision that numbers the file's nodes differently.
    """
    if layout == "nested":
        repo, _, _, outer_mount, inner_mount = build_nested_link_repos_at(
            new_lore_repo, "linked", "inner"
        )
        staged_directory = f"{inner_mount}/c-data"
        staged_file = f"{staged_directory}/inner.txt"
        edited_file = f"{outer_mount}/b-root.txt"
    elif layout == "repinned":
        link_repo, first_pin, second_pin = make_repo_renumbering_folder(new_lore_repo)
        repo = make_repo(new_lore_repo, {"parent.txt": "parent\n"})
        repo.link_add("linked", link_repo.get_id(), "/", pin=first_pin)
        repo.commit("Add link")
        repo.push()
        repo.link_update("linked", pin=second_pin)
        staged_directory = "linked/sub"
        staged_file = f"{staged_directory}/a.txt"
        edited_file = "parent.txt"
    else:
        repo, _link_repo = make_parent_with_link(
            new_lore_repo,
            "linked",
            {"dir/staged.txt": "original\n", "edited.txt": "original\n"},
        )
        staged_directory = "linked/dir"
        staged_file = f"{staged_directory}/staged.txt"
        edited_file = "linked/edited.txt"
    files = repo.read_files()

    repo.write_files({staged_file: "staged edit\n", edited_file: "unstaged edit\n"})
    repo.stage(staged_file)
    repo.reset(edited_file)
    status = status_by_path(repo)

    for path in (staged_file, staged_directory):
        with pytest.raises(ImproperArgumentsError):
            repo.reset(path)

    assert repo.read_files() == {**files, staged_file: "staged edit\n"}
    assert status_by_path(repo) == status
    assert any(entry["flagStaged"] for entry in status.values())


@pytest.mark.smoke
def test_link_reset_refuses_a_file_added_inside_a_link(new_lore_repo):
    """`reset` refuses a file added inside a link, staged or marked dirty, and leaves the
    working tree and the status as they were. An untracked file there still resets.

    The parent holds more nodes than the linked repository, so the added file's node id
    also numbers a committed file in the parent.
    """
    repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        "linked",
        parent_files={f"parent{i}.txt": f"parent {i}\n" for i in range(6)},
    )
    staged_file = "linked/staged.txt"
    dirty_file = "linked/dirty.txt"
    untracked_file = "linked/untracked.txt"
    repo.write_files({staged_file: "staged\n", dirty_file: "dirty\n"})
    repo.stage(staged_file)
    repo.dirty(dirty_file)
    files = repo.read_files()
    status = status_by_path(repo)
    repo.write_files({untracked_file: "untracked\n"})

    for path in (staged_file, dirty_file):
        with pytest.raises(NotSupportedError):
            repo.reset(path)
    repo.reset(untracked_file, purge=True)

    assert repo.read_files() == files
    assert files[staged_file] == "staged\n"
    assert files[dirty_file] == "dirty\n"
    assert status_by_path(repo) == status
    assert status[staged_file]["flagStaged"]
    assert status[dirty_file]["flagDirty"]


@pytest.mark.smoke
def test_link_reset_last_merged_refuses_a_file_inside_a_link(new_lore_repo):
    """`reset --last-merged-from` refuses a file inside a link, at the merge and after it, and
    leaves the working tree as it was.

    The linked file's node id also numbers parent files that a merge changed, so the file's
    history must not be read in the parent's tree.
    """
    parent_files = {f"parent{i}.txt": f"parent {i}\n" for i in range(6)}
    repo, _link_repo = make_parent_with_link(
        new_lore_repo, "linked", parent_files=parent_files
    )
    repo.branch_create("feature")
    repo.write_commit_push("Edit the linked file", {"linked/linked.txt": "feature\n"})
    repo.branch_switch("main")
    repo.write_commit_push("Edit the parent", {path: "main\n" for path in parent_files})
    repo.branch_switch("feature")
    repo.branch_merge("main")
    files = repo.read_files()

    with pytest.raises(NotSupportedError):
        repo.reset("linked/linked.txt", last_merged_from="main")
    assert repo.read_files() == files

    repo.write_commit_push(
        "Edit the parent after the merge", {"parent0.txt": "after\n"}
    )
    files = repo.read_files()

    with pytest.raises(NotSupportedError):
        repo.reset("linked/linked.txt", last_merged_from="main")
    assert repo.read_files() == files
    assert files["linked/linked.txt"] == "feature\n"


@pytest.mark.smoke
def test_link_reset_purge_ignores_a_directory_rule_naming_the_mount(new_lore_repo):
    """`lore reset --purge .` restores the committed content mounted at a link and
    deletes the untracked files in the mount and beside it, whatever the ignore
    rules say, even one naming the mount as a directory.
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

    with repo.open_file(repo.ignore_file(), "w+") as ignore_file:
        ignore_file.write(f"/{link_path}/\n*.tmp\n")

    with repo.open_file(mounted_file, "w+") as output_file:
        output_file.writelines(["inside modified\n"])
    with repo.open_file(outside_file, "w+") as output_file:
        output_file.writelines(["outside modified\n"])
    untracked = [f"{link_path}/untracked.txt", f"{link_path}/build.tmp", "build.tmp"]
    repo.write_files({path: "untracked\n" for path in untracked})

    repo.reset(".", purge=True)

    with repo.open_file(outside_file, "r") as f:
        assert "outside original" in f.read(), "Reset should restore a committed file"
    with repo.open_file(mounted_file, "r") as f:
        assert "inside original" in f.read(), (
            "Reset should restore the committed file mounted at the link"
        )
    for path in untracked:
        assert not repo.file_exists(path), f"The purge kept the untracked {path}"


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
def test_link_unstage_of_an_add_over_a_committed_directory_leaves_nothing(
    new_lore_repo,
):
    """Unstaging a link added over a committed empty directory takes back the directory's staged
    delete with its dirty flags: the directory stays, and no staged state is left."""
    main_repo = make_repo(new_lore_repo, {"main.txt": "initial content\n"})
    source_repo = make_repo(new_lore_repo, {"a.txt": "link source content\n"})
    link_path = "linked"
    main_repo.make_dirs(link_path)
    main_repo.stage(scan=True)
    main_repo.commit("add empty linked directory")
    main_repo.push()
    main_repo.link_add(link_path, source_repo.get_id(), "/")

    main_repo.unstage(link_path)

    entries = status_by_path(main_repo)
    assert entries == {}, f"Expected nothing staged or dirty, got: {entries}"
    assert not has_staged_anchor(main_repo), (
        "Expected the unstage to leave no staged state"
    )
    assert main_repo.path_exists(link_path), "the committed directory stays"


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
@pytest.mark.parametrize("staged", [True, False], ids=["staged", "unstaged"])
def test_link_reset_puts_back_a_link_a_file_replaced(new_lore_repo, staged):
    """`reset` of a link whose mount a file replaced puts the link back, in the registry and
    on disk, whether the file was staged or not."""
    linked = {"shared/linked.txt": "linked\n", "shared/sub/deep.txt": "deep\n"}
    parent, link_repo = make_parent_with_link(
        new_lore_repo,
        "shared",
        {path.removeprefix("shared/"): content for path, content in linked.items()},
    )

    parent.rmtree("shared")
    parent.write_files({"shared": "file\n"})
    if staged:
        parent.stage("shared", scan=True)
    parent.reset("shared")

    assert_crr_clean(
        parent,
        expected_files_present=[],
        expected_files_absent=[],
        expected_link_registry={link_repo.get_id(): True},
    )
    for path, content in linked.items():
        with parent.open_file(path) as f:
            assert f.read() == content, f"{path} must hold the linked content"


@pytest.mark.smoke
def test_link_forced_unstage_puts_back_a_link_a_staged_file_replaced(new_lore_repo):
    """A forced `unstage` of a link whose mount a staged file replaced puts the link back, in the
    registry and on disk, and discards the file's add."""
    linked = {"shared/linked.txt": "linked\n", "shared/sub/deep.txt": "deep\n"}
    parent, link_repo = make_parent_with_link(
        new_lore_repo,
        "shared",
        {path.removeprefix("shared/"): content for path, content in linked.items()},
    )

    parent.rmtree("shared")
    parent.write_files({"shared": "file\n"})
    parent.stage("shared", scan=True)
    parent.unstage("shared", force=True)

    assert_crr_clean(
        parent,
        expected_files_present=[],
        expected_files_absent=[],
        expected_link_registry={link_repo.get_id(): True},
    )
    for path, content in linked.items():
        with parent.open_file(path) as f:
            assert f.read() == content, f"{path} must hold the linked content"


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("change", "refused"),
    [
        ("add-edited", True),
        ("add-untracked", False),
        ("replaced", True),
        ("removed-then-folder", False),
        ("added-then-replaced", False),
    ],
)
def test_link_unstage_keeps_local_work_at_the_mount(new_lore_repo, change, refused):
    """`unstage` of a staged link change keeps the local work standing at the mount: it refuses
    where restoring the link would overwrite that work, and otherwise leaves it in place."""
    parent = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked\n"})
    parent.link_add("shared", link_repo.get_id(), "/")
    if change in ("replaced", "removed-then-folder"):
        parent.commit("Add link")
        parent.push()

    local = {
        "add-edited": "shared/linked.txt",
        "add-untracked": "shared/own.txt",
        "replaced": "shared",
        "removed-then-folder": "shared/own.txt",
        "added-then-replaced": "shared",
    }[change]
    if change == "removed-then-folder":
        parent.link_remove("shared")
    if local == "shared":
        parent.rmtree("shared")
    parent.write_files({local: "local\n"})
    if change in ("replaced", "removed-then-folder", "added-then-replaced"):
        parent.stage("shared", scan=True)

    if refused:
        with pytest.raises(LocalChanges):
            parent.unstage("shared")
    else:
        parent.unstage("shared")
    with parent.open_file(local) as f:
        assert f.read() == "local\n", f"unstage must keep {local}"


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


@pytest.mark.smoke
def test_dry_run_unstage_and_reset_keep_a_staged_link_add(new_lore_repo):
    """`unstage --dry-run` and `reset --dry-run` of a staged link addition keep the link's
    files and the staged link, where `unstage` without it removes both."""
    repo = make_repo(new_lore_repo, {"a.txt": "a\n"})
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked\n"})
    repo.link_add("lnk", link_repo.get_id(), "/")
    linked_file = os.path.join(repo.path, "lnk", "linked.txt")
    assert os.path.isfile(linked_file)
    links = repo.link_list()

    repo.unstage("lnk", dry_run=True)
    repo.reset("lnk", dry_run=True)
    repo.reset(dry_run=True)

    assert os.path.isfile(linked_file), "a dry run removed the link's files"
    assert repo.link_list() == links, "a dry run changed the staged links"

    repo.unstage("lnk")

    assert not os.path.exists(linked_file), "unstage kept the link's files"
    assert repo.link_list() != links, "unstage kept the staged link"
