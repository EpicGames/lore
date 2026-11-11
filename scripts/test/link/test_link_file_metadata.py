# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import NotSupportedError
from link_helpers import DEFAULT_LINK_MOUNT, DEFAULT_PARENT_FILE
from lore_ffi import NOT_SUPPORTED
from lore_parsers import parse_jsonl
from test_utils import has_staged_anchor

from lore import Lore

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)

LINKED_FILE = f"{DEFAULT_LINK_MOUNT}/linked.txt"


def file_metadata(repo: Lore, path: str) -> dict[str, str]:
    events = parse_jsonl(repo.file_metadata_get(path, json=True), "metadata")
    return {event["key"]: event["value"]["data"] for event in events}


def dumped_address(repo: Lore, path: str) -> str:
    output = repo.file_dump(path=path)
    return next(line for line in output.splitlines() if line.startswith("Address"))


def staged_file_with_owner(new_lore_repo, path: str, owner: str) -> Lore:
    repo: Lore = new_lore_repo()
    # Content of its own, so a dump of one file cannot pass for the other.
    with repo.open_file(path, "w") as output_file:
        output_file.write(f"{owner}\n")
    repo.stage(path)
    repo.file_metadata_set(path, ["owner", owner])
    return repo


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

    assert file_metadata(repo, LINKED_FILE) == {"owner": "link"}
    assert dumped_address(repo, LINKED_FILE) == dumped_address(link_repo, "linked.txt")

    with pytest.raises(NotSupportedError):
        repo.file_metadata_set(LINKED_FILE, ["owner", "changed"])
    with pytest.raises(NotSupportedError):
        repo.file_metadata_clear(LINKED_FILE)
    code = repo.file_metadata_set_capi(
        lore_library_path, [DEFAULT_PARENT_FILE, LINKED_FILE], "owner", "changed"
    )

    assert code == NOT_SUPPORTED
    assert file_metadata(repo, DEFAULT_PARENT_FILE) == {"owner": "parent"}
    assert file_metadata(repo, LINKED_FILE) == {"owner": "link"}
    assert not has_staged_anchor(repo)

    repo.file_metadata_set(DEFAULT_LINK_MOUNT, ["owner", "mount"])

    assert file_metadata(repo, DEFAULT_LINK_MOUNT) == {"owner": "mount"}
