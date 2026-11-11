# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import NotSupportedError
from link_helpers import DEFAULT_LINK_MOUNT, make_parent_with_link
from lore_parsers import parse_jsonl
from test_utils import working_tree_files

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)

PARENT_FILES = ["a.txt", "b.txt", "c.txt", "d.txt"]
LINKED = f"{DEFAULT_LINK_MOUNT}/linked.txt"
LINK_FILES = {f"linked{index}.txt": f"linked {index}\n" for index in range(8)} | {
    "linked.txt": "linked\n"
}


def parent_files() -> dict[str, str]:
    return {path: f"{path}\n" for path in PARENT_FILES}


def dependency_listing(repo: Lore) -> dict[str, set[str]]:
    """Each parent file's dependencies, and its dependents marked with `<`."""
    listing = {path: set() for path in PARENT_FILES}
    for reverse, mark in ((False, ""), (True, "<")):
        output = repo.file_dependency_list(
            PARENT_FILES, reverse=reverse, json=True, offline=True
        )
        entries = iter(parse_jsonl(output, "fileDependencyListEntry"))
        files = parse_jsonl(output, "fileDependencyListFile")
        for path, file in zip(PARENT_FILES, files, strict=True):
            assert file["path"] == repo._fix_path(path)
            for _ in range(file["entryCount"]):
                listing[path].add(mark + next(entries)["path"])
    return listing


@pytest.mark.smoke
def test_link_dependency_inside_a_link_is_refused(new_lore_repo):
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, parent_files=parent_files(), link_files=LINK_FILES
    )

    parent_repo.file_dependency_add("a.txt", "b.txt", offline=True)
    parent_repo.file_dependency_add("c.txt", DEFAULT_LINK_MOUNT, offline=True)
    expected = {
        "a.txt": {"b.txt"},
        "b.txt": {"<a.txt"},
        "c.txt": {DEFAULT_LINK_MOUNT},
        "d.txt": set(),
    }
    assert dependency_listing(parent_repo) == expected

    with pytest.raises(NotSupportedError):
        parent_repo.file_dependency_add(LINKED, "d.txt", offline=True)
    with pytest.raises(NotSupportedError):
        parent_repo.file_dependency_add("d.txt", ["c.txt", LINKED], offline=True)
    with pytest.raises(NotSupportedError):
        parent_repo.file_dependency_remove("a.txt", ["b.txt", LINKED], offline=True)
    with pytest.raises(NotSupportedError):
        parent_repo.file_dependency_list(["a.txt", LINKED], offline=True)

    assert dependency_listing(parent_repo) == expected


def change_and_delete(repo: Lore, files: dict[str, str], deleted: list[str]):
    """Changes every file of `files` and deletes those of `deleted`, committed and pushed."""
    repo.write_files({path: "changed\n" for path in files})
    for path in deleted:
        repo.remove_file(path)
    repo.stage(scan=True)
    repo.commit("Change and delete files")
    repo.push()


@pytest.mark.smoke
def test_link_dependency_root_file_inside_a_link_is_skipped(new_lore_repo):
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo, parent_files=parent_files(), link_files=LINK_FILES
    )
    parent_repo.file_dependency_add("a.txt", ["b.txt", "c.txt"], offline=True)
    parent_repo.write_commit_push("Add dependencies", {"marker.txt": "marker\n"})
    roots = ["a.txt", LINKED]

    assert working_tree_files(parent_repo.clone(root_files=roots)) == [
        "a.txt",
        "b.txt",
        "c.txt",
    ]
    with pytest.raises(NotSupportedError):
        parent_repo.clone(root_files=[LINKED])

    clone = parent_repo.clone()
    linked_deleted = [path for index, path in enumerate(LINK_FILES) if index % 2]
    change_and_delete(link_repo, LINK_FILES, linked_deleted)
    parent_repo.link_update(DEFAULT_LINK_MOUNT)
    parent_repo.commit("Update the link")
    parent_repo.push()
    change_and_delete(parent_repo, parent_files(), ["c.txt"])
    with pytest.raises(NotSupportedError):
        clone.sync(root_files=[LINKED])
    clone.sync(root_files=roots)

    linked = {f"{DEFAULT_LINK_MOUNT}/{path}": text for path, text in LINK_FILES.items()}
    expected = parent_files() | {"a.txt": "changed\n", "b.txt": "changed\n"}
    del expected["c.txt"]
    assert {
        path: text for path, text in clone.read_files().items() if path != "marker.txt"
    } == expected | linked

    clone.write_files({path: "local\n" for path in [*expected, *linked]})
    clone.sync(root_files=roots, reset=True)
    assert {
        path: text for path, text in clone.read_files().items() if path != "marker.txt"
    } == {path: "local\n" for path in [*expected, *linked]} | {
        "a.txt": "changed\n",
        "b.txt": "changed\n",
    }
