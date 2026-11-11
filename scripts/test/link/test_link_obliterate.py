# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest
from error_types import NotSupportedError
from link_helpers import DEFAULT_LINK_MOUNT, DEFAULT_PARENT_FILE, make_repo
from test_utils import status_by_path

pytestmark = pytest.mark.own_vfs(
    "links a repository, which a virtual clone does not hold"
)


@pytest.mark.smoke
def test_link_obliterate_inside_a_link_is_refused(new_lore_repo):
    """`back` links the linked repository to the parent, so a path through it returns into the
    parent's own repository at another revision."""
    parent_repo = make_repo(new_lore_repo, {DEFAULT_PARENT_FILE: "baseline\n"})
    link_repo = make_repo(new_lore_repo, {"linked.txt": "linked content\n"})
    link_repo.link_add("back", parent_repo.get_id(), "/")
    link_repo.commit("Link back to the parent")
    link_repo.push()
    parent_repo.link_add(DEFAULT_LINK_MOUNT, link_repo.get_id(), "/")
    parent_repo.commit("Add link")
    parent_repo.push()
    linked = f"{DEFAULT_LINK_MOUNT}/linked.txt"
    returned = f"{DEFAULT_LINK_MOUNT}/back/{DEFAULT_PARENT_FILE}"
    files = {
        DEFAULT_PARENT_FILE: "baseline\n",
        linked: "linked content\n",
        returned: "baseline\n",
    }
    status = status_by_path(parent_repo)

    for path in [linked, returned]:
        with pytest.raises(NotSupportedError):
            parent_repo.file_obliterate(path=path)

    assert parent_repo.read_files() == files
    assert status_by_path(parent_repo) == status
    assert parent_repo.clone().read_files() == files
