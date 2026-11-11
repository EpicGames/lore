# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from lore_parsers import parse_status_json
from test_utils import to_posix, working_tree_files

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_view(new_lore_repo, scratch_dir):
    view_dir = scratch_dir("view", create=True)
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    unicode_dir = "奇怪的路徑"
    unicode_file = os.path.join(unicode_dir, "کاراکترهای یونیکد")
    first_dir = "aaaa"
    second_dir = "bbbb"

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    repo.make_dirs(os.path.dirname(unicode_file))
    with repo.open_file(unicode_file, "w+", encoding="utf-8") as output_file:
        output_file.writelines(["只需將一些文本寫入文件即可\n"])

    for i in range(4):
        subpath = os.path.join(first_dir, str(i))
        repo.make_dirs(subpath)
        for j in range(5):
            with repo.open_file(
                os.path.join(subpath, str(j) + ".uasset"), "w+b"
            ) as output_file:
                output_file.write(os.urandom(1024))

        subpath = os.path.join(second_dir, str(i))
        repo.make_dirs(subpath)
        for j in range(5):
            with repo.open_file(
                os.path.join(subpath, str(j) + ".uasset"), "w+b"
            ) as output_file:
                output_file.write(os.urandom(1024))

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create a view filter
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("**\n")
        view_file.write("!" + second_dir + "/1/**\n")

    # Clone the repository with a view filter
    clone = repo.clone(view=view_path)

    os.unlink(view_path)

    # Verify files contents, mode and last modified timestamp
    for index in range(5):
        assert repo.compare_file(
            clone, os.path.join(second_dir, "1", str(index) + ".uasset")
        )

    assert not os.path.exists(os.path.join(clone.path, first_dir)), (
        "Directory not filtered out as expected: " + os.path.join(clone.path, first_dir)
    )
    assert not os.path.exists(os.path.join(clone.path, text_file)), (
        "Top level file not filtered out as expected: "
        + os.path.join(clone.path, text_file)
    )
    assert not os.path.exists(os.path.join(clone.path, unicode_dir)), (
        "Directory not filtered out as expected: "
        + os.path.join(clone.path, unicode_dir)
    )

    for index in range(4):
        if index == 1:
            continue
        test_path2 = os.path.join(clone.path, second_dir, str(index))
        assert not os.path.exists(test_path2), (
            "Directory not filtered out as expected: " + test_path2
        )

    # Modify and stage some files in a branch in the filtered repository clone
    clone.branch_create("test-filter")

    with clone.open_file(
        os.path.join(second_dir, "1", "1.uasset"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1024))

    os.unlink(os.path.join(clone.path, second_dir, "1", "2.uasset"))

    with clone.open_file(
        os.path.join(second_dir, "1", "3.uasset"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1024))

    clone.stage(os.path.join(second_dir, "1", "1.uasset"))
    clone.commit("Modification commit")

    clone.stage(scan=True)
    clone.commit("Second modification commit")
    clone.push()

    repo.branch_switch("test-filter")
    repo.sync()

    # Verify files contents, mode and last modified timestamp

    for i in range(5):
        if i == 2:
            test_path = os.path.join(repo.path, second_dir, "1", str(i) + ".uasset")
            assert not os.path.exists(test_path), (
                "File not deleted as expected: " + test_path
            )
        else:
            assert repo.compare_file(
                clone, os.path.join(second_dir, "1", str(i) + ".uasset")
            )

    assert os.path.exists(os.path.join(repo.path, first_dir)), (
        "Directory not retained as expected: " + os.path.join(repo.path, first_dir)
    )

    assert os.path.exists(os.path.join(repo.path, text_file)), (
        "Top level file not retained as expected: " + os.path.join(repo.path, text_file)
    )

    assert os.path.exists(os.path.join(repo.path, unicode_dir)), (
        "Directory not retained as expected: " + os.path.join(repo.path, unicode_dir)
    )

    for i in range(4):
        if i == 1:
            continue
        test_path2 = os.path.join(repo.path, second_dir, str(i))
        assert os.path.exists(test_path2), (
            "Directory not retained as expected: " + test_path2
        )


@pytest.mark.smoke
def test_view_clone_materializes_directory_emptied_by_filter(
    new_lore_repo, scratch_dir
):
    """Cloning with a view filter that excludes every child of a directory --
    but not the directory node itself -- still materializes the (now empty)
    directory, and a following status agrees that nothing is missing.

    The committed tree has a directory `data` holding two files and a nested
    subdirectory with its own file, plus an unrelated top-level file. The view
    filter `data/**` excludes everything *under* `data` (both files, the `sub`
    subdirectory, and `sub/nested.txt`) while leaving the `data` directory node
    itself in view.

    Expected after the clone:
      - the unrelated in-view file `keep.txt` is materialized,
      - none of the excluded children exist on disk, and
      - `data` is materialized as an empty directory (this is the regression:
        a clone that drops a directory once the view filter removes all its
        children leaves `data` missing).

    Because the `data` node is in view, `status --scan` (a.k.a. --unstaged) on
    the fresh clone must report no changes. If the clone skipped creating the
    emptied directory, clone and status disagree about the view and `data`
    surfaces as a phantom delete.
    """
    repo: Lore = new_lore_repo()

    keep = "keep.txt"
    emptied_dir = "data"
    filtered_children = [
        os.path.join(emptied_dir, "file1.txt"),
        os.path.join(emptied_dir, "file2.txt"),
        os.path.join(emptied_dir, "sub", "nested.txt"),
    ]
    with repo.open_file(keep, "w+b") as f:
        f.write(os.urandom(64))
    for child in filtered_children:
        repo.make_dirs(os.path.dirname(child))
        with repo.open_file(child, "w+b") as f:
            f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Pure-exclusion view: `data/**` drops every descendant of `data` while the
    # `data` directory node itself stays in view.
    view_dir = scratch_dir("view", create=True)
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("data/**\n")
    clone: Lore = repo.clone(view=view_path)

    # Sanity: the unrelated file is in view; every child of `data` is filtered.
    assert clone.file_exists(keep), "in-view file should be materialized"
    for child in filtered_children:
        assert not clone.path_exists(child), (
            f"view filter materialized an excluded child: {child}"
        )
    assert not clone.path_exists(os.path.join(emptied_dir, "sub")), (
        "excluded subdirectory should not be materialized"
    )

    # The directory node is in view, so it must be materialized even though the
    # filter left it empty.
    emptied_abs = os.path.join(clone.path, emptied_dir)
    assert os.path.isdir(emptied_abs), (
        "clone did not materialize the directory left empty by the view filter: "
        + emptied_dir
    )
    assert os.listdir(emptied_abs) == [], (
        "directory emptied by the view filter should have no materialized "
        f"children, got: {os.listdir(emptied_abs)}"
    )

    # status --scan must agree with the clone's view: no changes at all, and in
    # particular no phantom delete of the in-view (empty) `data` directory.
    entries = parse_status_json(clone.status(json=True, offline=True, scan=True))
    by_path = {to_posix(e.get("path", "")): e for e in entries}
    assert by_path == {}, (
        "status --scan on a pristine view-filtered clone reported changes "
        f"(clone/status view mismatch): {sorted(by_path)}"
    )


def _commit(repo: Lore, files: list[str], directories: tuple[str, ...] = ()) -> None:
    """Commits and pushes `files`, each holding its own path, and the empty
    `directories`."""
    repo.write_files({path: path for path in files})
    for path in directories:
        repo.make_dirs(path)
    repo.stage(scan=True)
    repo.commit()
    repo.push()


@pytest.mark.own_vfs("layers a repository, which a virtual clone does not hold")
@pytest.mark.smoke
def test_view_with_a_wildcard_reinclusion_creates_no_excluded_directory(
    new_lore_repo, scratch_dir
):
    """A view whose re-inclusion opens with a wildcard, `!*/Saved/Config/`, makes a clone
    and a layer add enter every directory the view excludes to look for what it
    re-includes. Neither creates a directory it finds nothing in, the mount aside, a dry
    run of the clone reports none, status reports nothing missing and a scanning stage
    stages nothing."""
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")
    _commit(
        repo,
        ["Game/Saved/Config/a.ini", "Game/Saved/Logs/l.log", "Engine/Build/b.txt"],
        ("Engine/Empty", "Game/Saved/Config/Kept"),
    )
    _commit(layer_repo, ["Saved/Config/a.ini", "Saved/Logs/l.log"], ("Empty",))
    view = os.path.join(scratch_dir("view", create=True), "view.txt")
    with open(view, "w") as view_file:
        view_file.write("/*\n!*/Saved/Config/\n")

    target = scratch_dir("dry-run", create=True)
    reported = repo.run(
        ["repository", "clone", repo.remote_path, str(target), "--view", view],
        dry_run=True,
    ).splitlines()
    excluded = ["Engine", "Engine/Build", "Engine/Empty", "Game/Saved/Logs"]
    assert [path for path in excluded if path in reported] == [], reported

    clone: Lore = repo.clone(view=view)

    kept = [
        "Game",
        "Game/Saved",
        "Game/Saved/Config",
        "Game/Saved/Config/Kept",
        "Game/Saved/Config/a.ini",
    ]
    assert working_tree_files(clone, directories=True) == kept
    assert parse_status_json(clone.status(json=True, offline=True, scan=True)) == []
    clone.stage(scan=True, offline=True)
    assert parse_status_json(clone.status(json=True, offline=True)) == []

    clone.layer_add("Mount", layer_repo, "/")

    assert working_tree_files(clone, directories=True) == kept + [
        "Mount",
        "Mount/Saved",
        "Mount/Saved/Config",
        "Mount/Saved/Config/a.ini",
    ]


@pytest.mark.smoke
def test_forced_stage_scan_commits_files_the_view_excludes(new_lore_repo, scratch_dir):
    """A forced staging scan in a sparse clone stages new files the view excludes, status lists
    them staged, and the commit takes their content. A clone under the same view leaves them out
    of its working tree."""
    repo: Lore = new_lore_repo()
    repo.write_files({"Docs/readme.md": "readme\n", "Engine/engine.txt": "engine\n"})
    repo.stage(scan=True)
    repo.commit("Base", offline=True)
    repo.push()

    view_dir = scratch_dir("view", create=True)
    view = os.path.join(view_dir, "view.txt")
    with open(view, "w+") as view_file:
        view_file.write("/*\n!/Docs/\n")
    clone: Lore = repo.clone(view=view)
    added = {
        "top.txt": "new at the top level\n",
        "Engine/new.txt": "new in a committed directory\n",
        "New/new.txt": "new in a new directory\n",
    }
    clone.write_files(added)
    clone.stage(".", scan=True, force=True, offline=True)

    staged = {
        to_posix(entry["path"]): entry
        for entry in parse_status_json(clone.status(json=True, offline=True))
    }
    for path in added:
        assert staged.get(path, {}).get("flagStaged") is True, (path, staged)

    clone.commit("Past the view", offline=True)
    clone.push()

    other: Lore = repo.clone(view=view)
    for path in added:
        assert not other.path_exists(path), path
    repo.sync()
    for path, content in added.items():
        with repo.open_file(path) as committed:
            assert committed.read() == content, path


@pytest.mark.own_vfs("layers a repository, which a virtual clone does not hold")
@pytest.mark.smoke
def test_forced_stage_scan_commits_files_the_view_excludes_below_a_link_and_a_layer(
    new_lore_repo, scratch_dir
):
    """A forced staging scan stages new files the view excludes below a link and a layer whose
    mounts the view excludes in part, status lists them staged, and the commit takes their
    content into the repositories the mounts draw from."""
    repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo(repo.name + "_link")
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")
    _commit(repo, ["Docs/readme.md"])
    _commit(link_repo, ["Hidden/hidden.txt", "linked.txt"])
    _commit(layer_repo, ["Hidden/hidden.txt", "layered.txt"])
    repo.link_add("Lib", link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    view = os.path.join(scratch_dir("view", create=True), "view.txt")
    with open(view, "w") as view_file:
        view_file.write("/Lib/Hidden/\n/Mount/Hidden/\n")
    clone: Lore = repo.clone(view=view)
    clone.layer_add("Mount", layer_repo, "/")
    for mount in ["Lib", "Mount"]:
        assert not clone.path_exists(f"{mount}/Hidden/hidden.txt"), mount

    added = {
        "Lib/Hidden/new.txt": (link_repo, "Hidden/new.txt", "new below the link\n"),
        "Lib/New/new.txt": (link_repo, "New/new.txt", "in view below the link\n"),
        "Mount/Hidden/new.txt": (layer_repo, "Hidden/new.txt", "new below the layer\n"),
    }
    clone.write_files({path: content for path, (_, _, content) in added.items()})
    clone.stage(".", scan=True, force=True, offline=True)

    staged = {
        to_posix(entry["path"]): entry
        for entry in parse_status_json(clone.status(json=True, offline=True))
    }
    for path in added:
        assert staged.get(path, {}).get("flagStaged") is True, (path, staged)

    clone.commit("Past the view below a link and a layer")
    clone.push()

    for path, (source, source_path, content) in added.items():
        source.sync()
        with source.open_file(source_path) as committed:
            assert committed.read() == content, path


@pytest.mark.own_vfs("links a repository, which a virtual clone does not hold")
@pytest.mark.smoke
def test_unstage_of_a_directory_added_below_a_link_leaves_what_the_view_excludes(
    new_lore_repo, scratch_dir
):
    """An unstage of a directory added below a link takes back what the view keeps
    in it, and leaves staged what a forced staging scan took past the view."""
    repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo(repo.name + "_link")
    _commit(repo, ["Docs/readme.md"])
    _commit(link_repo, ["linked.txt"])
    repo.link_add("Lib", link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    view = os.path.join(scratch_dir("view", create=True), "view.txt")
    with open(view, "w") as view_file:
        view_file.write("/Lib/New/Hidden/\n")
    clone: Lore = repo.clone(view=view)
    kept, hidden = "Lib/New/a.txt", "Lib/New/Hidden/b.txt"
    clone.write_files({kept: "kept\n", hidden: "hidden\n"})
    clone.stage(".", scan=True, force=True, offline=True)

    def staged() -> set[str]:
        return {
            to_posix(entry["path"])
            for entry in parse_status_json(clone.status(json=True, offline=True))
            if entry.get("flagStaged")
        }

    assert {kept, hidden} <= staged()
    clone.unstage("Lib/New", offline=True)
    after = staged()
    assert kept not in after, after
    assert hidden in after, after


@pytest.mark.own_vfs("links a repository, which a virtual clone does not hold")
@pytest.mark.smoke
@pytest.mark.parametrize("unstaged", [".", "Lib"])
def test_unstage_below_a_link_leaves_what_a_forced_stage_staged_past_the_view(
    new_lore_repo, scratch_dir, unstaged
):
    """An unstage of the tree, or of a link, leaves staged what a forced staging scan took past
    the view in the linked repository, beside nothing else staged or with a staged file in view:
    status lists it staged, and the commit takes it into the linked repository."""
    repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo(repo.name + "_link")
    _commit(repo, ["Docs/readme.md"])
    _commit(link_repo, ["Hidden/hidden.txt", "linked.txt"])
    repo.link_add("Lib", link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    view = os.path.join(scratch_dir("view", create=True), "view.txt")
    with open(view, "w") as view_file:
        view_file.write("/Lib/Hidden/\n")
    clone: Lore = repo.clone(view=view)
    hidden, content = "Lib/Hidden/new.txt", "new below the link\n"
    files = {hidden: content}
    if unstaged == "Lib":
        files["Docs/new.md"] = "new in view\n"
    clone.write_files(files)
    clone.stage(".", scan=True, force=True)
    clone.unstage(unstaged, offline=True)

    staged = {
        to_posix(entry["path"])
        for entry in parse_status_json(clone.status(json=True, offline=True))
        if entry.get("flagStaged")
    }
    assert hidden in staged, staged

    clone.commit("Past the view below a link")
    clone.push()
    link_repo.sync()
    with link_repo.open_file("Hidden/new.txt") as committed:
        assert committed.read() == content
