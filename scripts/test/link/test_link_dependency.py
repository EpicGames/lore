# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import NotSupportedError
from link_helpers import (
    DEFAULT_LINK_MOUNT,
    DEFAULT_LINKED_FILE,
    change_and_delete,
    dependency_listing,
    dependency_parent_files,
    make_parent_with_link,
)
from test_utils import working_tree_files


pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)

LINK_FILES = {f"linked{index}.txt": f"linked {index}\n" for index in range(8)} | {
    "linked.txt": "linked\n"
}


@pytest.mark.smoke
def test_link_dependency_inside_a_link_is_refused(new_lore_repo):
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo, parent_files=dependency_parent_files(), link_files=LINK_FILES
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
        parent_repo.file_dependency_add(DEFAULT_LINKED_FILE, "d.txt", offline=True)
    with pytest.raises(NotSupportedError):
        parent_repo.file_dependency_add(
            "d.txt", ["c.txt", DEFAULT_LINKED_FILE], offline=True
        )
    with pytest.raises(NotSupportedError):
        parent_repo.file_dependency_remove(
            "a.txt", ["b.txt", DEFAULT_LINKED_FILE], offline=True
        )
    with pytest.raises(NotSupportedError):
        parent_repo.file_dependency_list(["a.txt", DEFAULT_LINKED_FILE], offline=True)

    assert dependency_listing(parent_repo) == expected


@pytest.mark.smoke
def test_link_dependency_root_file_inside_a_link_is_skipped(new_lore_repo):
    parent_repo, link_repo = make_parent_with_link(
        new_lore_repo, parent_files=dependency_parent_files(), link_files=LINK_FILES
    )
    parent_repo.file_dependency_add("a.txt", ["b.txt", "c.txt"], offline=True)
    parent_repo.write_commit_push("Add dependencies", {"marker.txt": "marker\n"})
    roots = ["a.txt", DEFAULT_LINKED_FILE]

    assert working_tree_files(parent_repo.clone(root_files=roots)) == [
        "a.txt",
        "b.txt",
        "c.txt",
    ]
    with pytest.raises(NotSupportedError):
        parent_repo.clone(root_files=[DEFAULT_LINKED_FILE])

    clone = parent_repo.clone()
    linked_deleted = [path for index, path in enumerate(LINK_FILES) if index % 2]
    change_and_delete(link_repo, LINK_FILES, linked_deleted)
    parent_repo.link_update(DEFAULT_LINK_MOUNT)
    parent_repo.commit("Update the link")
    parent_repo.push()
    change_and_delete(parent_repo, dependency_parent_files(), ["c.txt"])
    with pytest.raises(NotSupportedError):
        clone.sync(root_files=[DEFAULT_LINKED_FILE])
    clone.sync(root_files=roots)

    linked = {f"{DEFAULT_LINK_MOUNT}/{path}": text for path, text in LINK_FILES.items()}
    expected = dependency_parent_files() | {"a.txt": "changed\n", "b.txt": "changed\n"}
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
