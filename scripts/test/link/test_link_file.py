# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os
import re

import pytest
from error_types import NotSupportedError
from link_helpers import (
    DEFAULT_LINK_MOUNT,
    DEFAULT_LINKED_CONTENT,
    DEFAULT_LINKED_FILE,
    DEFAULT_PARENT_FILE,
    build_nested_link_repos_at,
    dumped_address,
    file_history_entries,
    file_metadata_values,
    link_pin,
    make_parent_with_link,
    staged_file_with_owner,
    written_content,
)
from lore_ffi import NOT_SUPPORTED
from test_utils import has_staged_anchor

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)

EDITED_CONTENT = "edited\n"


@pytest.mark.smoke
def test_a_path_inside_a_link_reads_the_linked_file_and_refuses_metadata_writes(
    new_lore_repo, lore_library_path
):
    link_repo = staged_file_with_owner(new_lore_repo, "linked.txt", "link")
    link_repo.commit("Linked file")
    link_repo.push()
    repo = staged_file_with_owner(new_lore_repo, DEFAULT_PARENT_FILE, "parent")
    repo.link_add(DEFAULT_LINK_MOUNT, link_repo.get_id(), "/")
    repo.commit("Parent file and link")
    repo.push()

    assert file_metadata_values(repo, DEFAULT_LINKED_FILE) == {"owner": "link"}
    assert dumped_address(repo, DEFAULT_LINKED_FILE) == dumped_address(
        link_repo, "linked.txt"
    )

    with pytest.raises(NotSupportedError):
        repo.file_metadata_set(DEFAULT_LINKED_FILE, ["owner", "changed"])
    with pytest.raises(NotSupportedError):
        repo.file_metadata_clear(DEFAULT_LINKED_FILE)
    code = repo.file_metadata_set_capi(
        lore_library_path,
        [DEFAULT_PARENT_FILE, DEFAULT_LINKED_FILE],
        "owner",
        "changed",
    )

    assert code == NOT_SUPPORTED
    assert file_metadata_values(repo, DEFAULT_PARENT_FILE) == {"owner": "parent"}
    assert file_metadata_values(repo, DEFAULT_LINKED_FILE) == {"owner": "link"}
    assert not has_staged_anchor(repo)

    repo.file_metadata_set(DEFAULT_LINK_MOUNT, ["owner", "mount"])

    assert file_metadata_values(repo, DEFAULT_LINK_MOUNT) == {"owner": "mount"}


@pytest.mark.smoke
def test_write_dump_and_history_of_a_linked_path_read_the_linked_file(
    new_lore_repo, scratch_dir
):
    parent_repo, link_repo = make_parent_with_link(new_lore_repo)
    pinned = parent_repo.branch_info().local_latest
    initial_pin = link_pin(parent_repo, link_repo.get_id())
    parent_repo.write_commit_push(
        "Edit the linked file", {DEFAULT_LINKED_FILE: EDITED_CONTENT}
    )
    edit_pin = link_pin(parent_repo, link_repo.get_id())
    outputs = scratch_dir("linked-read", create=True)
    latest, at_pin = str(outputs / "latest.txt"), str(outputs / "pinned.txt")
    tracked = os.path.join(parent_repo.path, "copy.txt")

    assert written_content(parent_repo, DEFAULT_LINKED_FILE, latest) == EDITED_CONTENT
    assert (
        written_content(parent_repo, DEFAULT_LINKED_FILE, at_pin, revision=pinned)
        == DEFAULT_LINKED_CONTENT
    )
    assert written_content(parent_repo, DEFAULT_LINKED_FILE, tracked) == EDITED_CONTENT
    dump = parent_repo.repository_dump(path=DEFAULT_LINKED_FILE)
    assert re.search(
        rf"^linked\.txt id .*size {os.path.getsize(latest)}\b", dump, re.MULTILINE
    ), dump
    assert file_history_entries(parent_repo, DEFAULT_LINKED_FILE) == [
        (edit_pin, "keep"),
        (initial_pin, "add"),
    ]


@pytest.mark.smoke
def test_file_write_of_a_path_in_a_link_mounted_from_a_subfolder(
    new_lore_repo, scratch_dir
):
    parent_repo, _link_repo = make_parent_with_link(
        new_lore_repo,
        link_files={"sub/linked.txt": DEFAULT_LINKED_CONTENT, "linked.txt": "root\n"},
        source_path="sub",
    )
    output = str(scratch_dir("linked-write-subfolder", create=True) / "out.txt")

    assert (
        written_content(parent_repo, DEFAULT_LINKED_FILE, output)
        == DEFAULT_LINKED_CONTENT
    )


@pytest.mark.smoke
def test_file_write_of_a_path_in_a_nested_link_writes_the_innermost_file(
    new_lore_repo, scratch_dir
):
    repo_a, _repo_b, _repo_c, _b_mount, nested_mount = build_nested_link_repos_at(
        new_lore_repo, "vendor/b", "deps/c"
    )
    output = str(scratch_dir("linked-write-nested", create=True) / "out.txt")

    assert (
        written_content(repo_a, f"{nested_mount}/c-data/inner.txt", output)
        == "content from repository C\n"
    )


@pytest.mark.smoke
def test_file_history_of_a_path_a_removed_link_held_walks_the_linked_history(
    new_lore_repo,
):
    parent_repo, link_repo = make_parent_with_link(new_lore_repo)
    initial_pin = link_pin(parent_repo, link_repo.get_id())
    parent_repo.link_remove(DEFAULT_LINK_MOUNT)
    parent_repo.commit("Remove the link")
    parent_repo.push()

    assert file_history_entries(parent_repo, DEFAULT_LINKED_FILE) == [
        (initial_pin, "add")
    ]
