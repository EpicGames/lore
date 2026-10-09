# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import os

import pytest
from link_helpers import make_repo, parse_revision_diff
from lore_parsers import parse_status_json
from thin_client import (
    ACTION_ADD,
    ACTION_DELETE,
    NODE_TYPE_FILE,
    NODE_TYPE_LINK,
    revision_diff,
    revision_tree,
)

from lore import Lore


@pytest.mark.smoke
def test_link_add_diff_reports_link_only(new_lore_repo):
    """Adding a link is a parent-tree change, not a per-file addition of
    the link's mounted contents.

    This test documents the user-visible contract for `lore link add`. It
    pairs two independent assertions that reach two different code paths:

      - Positive proof (parent-tree path): `lore status` reports the link
        path as a staged addition. This signal comes from
        `state::diff_collect` (state-vs-staged), the change stream used by
        status.

      - Negative proof (filesystem-walker path): `lore file diff` against
        the pre-link revision emits no unified-diff hunk for any file
        inside the link. This is the surface where the regression appeared.

    The negative-proof half is a contract assertion; on its own it would
    also pass if `file diff` were broken in some unrelated way. The
    walker's liveness is pinned by the symmetric content tests
    (`test_link_diff_file_added_in_linked_repo` and siblings), which prove
    `file diff` does emit hunks when the linked repository's content
    actually changes.
    """
    link_repo = make_repo(new_lore_repo, {"shared.txt": "main content\n"})
    pinned_revision = link_repo.branch_info().local_latest
    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    baseline_revision = parent_repo.branch_info().local_latest

    link_path = "libs/shared"
    linked_file = f"{link_path}/shared.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pinned_revision)

    # Positive proof: before committing, `lore status` must report the link
    # path itself as a staged addition, and must not list files inside the
    # link as separate staged adds.
    staged_paths = [
        entry.get("path", "")
        for entry in parse_status_json(parent_repo.status(json=True))
    ]
    assert link_path in staged_paths, (
        f"`lore status` must report the link path {link_path!r} as staged "
        f"after `lore link add`. Staged paths: {staged_paths}"
    )
    assert linked_file not in staged_paths, (
        f"`lore status` must not list files inside the link as separate "
        f"staged adds. Staged paths: {staged_paths}"
    )

    parent_repo.commit("Add link libs/shared")
    parent_repo.push()

    output = parent_repo.file_diff(source=baseline_revision, no_pager=True)

    # Negative proof: the file under the link must not appear in any unified-
    # diff header. We assert on the diff headers specifically so the test
    # fails for the exact format produced by the bug
    # (`--- /dev/null\n+++ libs/shared/...`).
    assert f"+++ {linked_file}" not in output, (
        "lore file diff must not emit a '+++ <link>/<file>' header for a "
        f"file inside a newly-added link.\nOutput:\n{output}"
    )
    assert f"--- {linked_file}" not in output, (
        "lore file diff must not emit a '--- <link>/<file>' header for a "
        f"file inside a newly-added link.\nOutput:\n{output}"
    )
    # The parent's baseline file was not touched -> no diff for it either.
    assert "+++ README.txt" not in output and "--- README.txt" not in output, (
        f"Parent's baseline file must not appear in the diff.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_update_diff_reports_link_only(new_lore_repo):
    """Updating a link's pin is a parent-tree change. When the new pin
    leaves the mounted subtree byte-identical, no file under the link path
    is touched in the parent.

    Setup: the link target has a subdirectory `mounted/` containing the
    file the parent's link exposes, plus an unrelated `outside/` file.
    Between pin P1 and pin P2 only `outside/` is modified; the subtree
    `mounted/` is byte-identical. The parent pins the link to `mounted/`,
    so the new pin is a metadata change (different signature) but the
    mounted tree is unchanged.

    This test pairs two independent assertions reaching different code paths:

      - Positive proof (parent-tree path): `lore status` reports the link
        path as a staged change after `lore link update`. This signal
        comes from `state::diff_collect` (state-vs-staged).

      - Negative proof (filesystem-walker path): `lore file diff` across
        the pin bump emits no unified-diff hunk for any file under the
        link path.

    The negative-proof half is a contract assertion. The walker's
    liveness is pinned by the symmetric content tests, which prove
    `file diff` does emit hunks when the linked repository's content
    actually changes between pins.
    """
    # Link target with a stable mounted/ subtree and an evolving outside/
    # file. The parent will mount only mounted/.
    link_repo = make_repo(
        new_lore_repo,
        {
            "mounted/stable.txt": "stable mounted content\n",
            "outside/v1.txt": "outside v1\n",
        },
    )
    pin_v1 = link_repo.branch_info().local_latest

    # Modify only outside/, leaving the mounted/ subtree untouched, so
    # v1 and v2 resolve to the same mounted/ tree.
    with link_repo.open_file("outside/v2.txt", "w+") as f:
        f.writelines(["outside v2\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2 changes only outside/")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    # Parent: baseline -> add link pinned to v1 mounting only mounted/ ->
    # re-pin to v2 (mounted/ subtree unchanged).
    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    link_path = "libs/shared"
    linked_file = f"{link_path}/stable.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/mounted", pin=pin_v1)
    parent_repo.commit("Add link at v1, mounting mounted/")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)

    # Positive proof: before committing, `lore status` must report the link
    # path as a staged change (the link node's pin moved), and must not list
    # files under the link path as separate staged entries.
    staged_paths = [
        entry.get("path", "")
        for entry in parse_status_json(parent_repo.status(json=True))
    ]
    assert link_path in staged_paths, (
        f"`lore status` must report the link path {link_path!r} as staged "
        f"after a `lore link update` pin move. Staged paths: {staged_paths}"
    )
    assert linked_file not in staged_paths, (
        f"`lore status` must not list files inside the link as separate "
        f"staged entries after a pin move. Staged paths: {staged_paths}"
    )

    parent_repo.commit("Bump link pin to v2 (no mounted/ change)")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    # No file content changed under the link path, so the diff must not
    # emit any unified-diff hunks for files under the link.
    assert f"+++ {linked_file}" not in output, (
        "lore file diff must not emit file edits when the link pin moved "
        f"but no mounted file changed.\nOutput:\n{output}"
    )
    assert f"--- {linked_file}" not in output, (
        "lore file diff must not emit file edits when the link pin moved "
        f"but no mounted file changed.\nOutput:\n{output}"
    )
    assert "stable mounted content" not in output, (
        "lore file diff must not surface stable mounted content as a "
        f"change when only the link pin signature moved.\nOutput:\n{output}"
    )
    # The unrelated outside/ file lives outside the link's source_path and
    # must never appear in the parent's diff.
    assert "outside" not in output, (
        "Files outside the link's mounted source_path must not appear in "
        f"the parent's file diff.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_remove_diff_reports_link_only(new_lore_repo):
    """Removing a link is a parent-tree change, not per-file deletions of
    the files that were visible through it.

    This test pairs two independent assertions reaching different code paths:

      - Positive proof (parent-tree path): `lore status` reports the link
        path as a staged removal after `lore link remove`. This signal
        comes from `state::diff_collect` (state-vs-staged).

      - Negative proof (filesystem-walker path): `lore file diff` against
        the pre-remove revision emits no unified-diff hunk for any file
        that was previously visible through the link.

    The negative-proof half is a contract assertion. The walker's
    liveness is pinned by the symmetric content tests, which prove
    `file diff` does emit hunks when the linked repository's content
    actually changes.
    """
    link_repo = make_repo(new_lore_repo, {"shared.txt": "main content\n"})
    pinned_revision = link_repo.branch_info().local_latest
    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})

    link_path = "libs/shared"
    linked_file = f"{link_path}/shared.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pinned_revision)
    parent_repo.commit("Add link")
    parent_repo.push()
    pre_remove_revision = parent_repo.branch_info().local_latest

    parent_repo.link_remove(link_path)

    # Positive proof: before committing, `lore status` must report the link
    # path as a staged removal, and must not list files that were visible
    # through the link as separate staged deletions.
    staged_paths = [
        entry.get("path", "")
        for entry in parse_status_json(parent_repo.status(json=True))
    ]
    assert link_path in staged_paths, (
        f"`lore status` must report the link path {link_path!r} as staged "
        f"after `lore link remove`. Staged paths: {staged_paths}"
    )
    assert linked_file not in staged_paths, (
        f"`lore status` must not list files inside the link as separate "
        f"staged deletions. Staged paths: {staged_paths}"
    )

    parent_repo.commit("Remove link")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_remove_revision, no_pager=True)

    # The diff must not show files inside the (now-removed) link as
    # individual file deletions in the parent.
    assert f"--- {linked_file}" not in output, (
        "lore file diff must not emit a '--- <link>/<file>' header for a "
        f"file inside a removed link.\nOutput:\n{output}"
    )
    assert "-main content" not in output, (
        "lore file diff must not surface deleted-link content as a parent "
        f"file deletion.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_diff_file_added_in_linked_repo(new_lore_repo):
    """When a link is updated and the new pin adds a new file inside the
    linked repository, `lore file diff` against the pin-bump must show the
    added file as an addition under the link path.
    """
    # Link target with two revisions: P1 has only "existing.txt"; P2 also
    # contains "new.txt".
    link_repo = make_repo(
        new_lore_repo,
        {
            "existing.txt": "already here\n",
        },
    )
    pin_v1 = link_repo.branch_info().local_latest

    with link_repo.open_file("new.txt", "w+") as f:
        f.writelines(["brand new content\n"])
    link_repo.stage(scan=True)
    link_repo.commit("add new file")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    link_path = "libs/shared"
    added_file = f"{link_path}/new.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link to v2")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    expected_header = f"--- /dev/null\n+++ {added_file}"
    assert expected_header in output, (
        "lore file diff must report the newly-added file inside the link "
        f"as added.\nExpected to contain:\n{expected_header}\n"
        f"Output:\n{output}"
    )
    assert "+brand new content" in output, (
        "lore file diff must include the added file's content as additions."
        f"\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_diff_file_modified_in_linked_repo(new_lore_repo):
    """When a link is updated and the new pin modifies an existing file
    inside the linked repository, `lore file diff` must show that file as
    modified under the link path with the correct hunk content.
    """
    link_repo = make_repo(
        new_lore_repo,
        {
            "shared.txt": "before\n",
        },
    )
    pin_v1 = link_repo.branch_info().local_latest

    with link_repo.open_file("shared.txt", "w+") as f:
        f.writelines(["after\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    link_path = "libs/shared"
    modified_file = f"{link_path}/shared.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link to v2")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    assert f"+++ {modified_file}" in output, (
        "lore file diff must report the modified linked file under its "
        f"link path.\nOutput:\n{output}"
    )
    assert f"--- {modified_file}" in output, (
        "lore file diff must report the modified linked file's source "
        f"side under its link path.\nOutput:\n{output}"
    )
    assert "-before" in output and "+after" in output, (
        f"lore file diff must include the modified content hunk.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_diff_file_removed_in_linked_repo(new_lore_repo):
    """When a link is updated and the new pin removes a file inside the
    linked repository, `lore file diff` must show that file as removed
    under the link path.
    """
    link_repo = make_repo(
        new_lore_repo,
        {
            "keep.txt": "keep me\n",
            "doomed.txt": "delete me\n",
        },
    )
    pin_v1 = link_repo.branch_info().local_latest

    link_repo.remove_file("doomed.txt")
    link_repo.stage(scan=True)
    link_repo.commit("v2 removes doomed.txt")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    link_path = "libs/shared"
    removed_file = f"{link_path}/doomed.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link to v2")
    parent_repo.push()

    output = parent_repo.file_diff(source=pre_update_revision, no_pager=True)

    assert f"--- {removed_file}" in output, (
        "lore file diff must report the removed linked file under its "
        f"link path.\nOutput:\n{output}"
    )
    assert "+++ /dev/null" in output, (
        "lore file diff must report a deletion target of /dev/null for the "
        f"removed linked file.\nOutput:\n{output}"
    )
    assert "-delete me" in output, (
        f"lore file diff must include the removed file's content as a deletion."
        f"\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_pin_update_revision_diff_reports_link_and_content(new_lore_repo):
    """A pin update must surface both the link itself and the linked
    repository's file changes.

    The contract for the "pin updated" case:

      - the link path is reported as a modification (the pin moved),
      - the linked repository's changed files are reported under the mount
        path,
      - an unrelated parent-side change in the same revision is still
        reported.
    """
    link_repo = make_repo(
        new_lore_repo,
        {
            "a.txt": "v1\n",
            "sub/b.txt": "b1\n",
        },
    )
    pin_v1 = link_repo.branch_info().local_latest

    # v2 modifies one mounted file and adds another.
    with link_repo.open_file("a.txt", "w+") as f:
        f.writelines(["v2\n"])
    with link_repo.open_file("sub/c.txt", "w+") as f:
        f.writelines(["c1\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2 modifies a.txt and adds sub/c.txt")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    link_path = "libs/shared"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pin_v1)
    parent_repo.commit("Add link@v1")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    # The pin update has to come first: `lore link update` refuses to run once
    # a scan has marked the mount point dirty.
    parent_repo.link_update(link_path, pin=pin_v2)
    with parent_repo.open_file("README.txt", "w+") as f:
        f.writelines(["baseline touched on this revision\n"])
    parent_repo.stage("README.txt")
    parent_repo.commit("Bump link to v2 and touch README")
    parent_repo.push()

    output = parent_repo.revision_diff(pre_update_revision, no_pager=True)
    entries = parse_revision_diff(output)
    paths = {path: action for action, path in entries}

    assert paths.get(link_path) == "M", (
        f"`lore revision diff` must report the link path {link_path!r} as a "
        "modification when its pin moved, so a consumer can tell the pin "
        f"changed and which paths belong to the link.\nOutput:\n{output}"
    )
    assert paths.get(f"{link_path}/a.txt") == "M", (
        "`lore revision diff` must report the modified file inside the "
        f"linked repository under the mount path.\nOutput:\n{output}"
    )
    assert paths.get(f"{link_path}/sub/c.txt") == "A", (
        "`lore revision diff` must report the file added inside the linked "
        f"repository under the mount path.\nOutput:\n{output}"
    )
    assert paths.get("README.txt") == "M", (
        "`lore revision diff` must still report the parent's own change in a "
        f"revision that also moved a link pin.\nOutput:\n{output}"
    )
    assert f"{link_path}/sub/b.txt" not in paths, (
        "`lore revision diff` must not report unchanged files inside the "
        f"linked repository.\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_pin_update_revision_diff_reports_link_when_content_identical(
    new_lore_repo,
):
    """A pin bump that leaves the mounted subtree byte-identical must still
    be visible in `lore revision diff`.

    Setup mirrors `test_link_update_diff_reports_link_only`: the parent mounts
    only `mounted/`, and between the two pins nothing inside it changes, so the
    content walk finds nothing. Without an entry for the link node the diff
    comes back empty, reading as "this revision changed nothing".
    """
    link_repo = make_repo(
        new_lore_repo,
        {
            "mounted/stable.txt": "stable mounted content\n",
            "outside/v1.txt": "outside v1\n",
        },
    )
    pin_v1 = link_repo.branch_info().local_latest

    with link_repo.open_file("outside/v2.txt", "w+") as f:
        f.writelines(["outside v2\n"])
    link_repo.stage(scan=True)
    link_repo.commit("v2 changes only outside/")
    link_repo.push()
    pin_v2 = link_repo.branch_info().local_latest

    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    link_path = "libs/shared"

    parent_repo.link_add(link_path, link_repo.get_id(), "/mounted", pin=pin_v1)
    parent_repo.commit("Add link at v1, mounting mounted/")
    parent_repo.push()
    pre_update_revision = parent_repo.branch_info().local_latest

    parent_repo.link_update(link_path, pin=pin_v2)
    parent_repo.commit("Bump link pin to v2 (no mounted/ change)")
    parent_repo.push()

    output = parent_repo.revision_diff(pre_update_revision, no_pager=True)
    entries = parse_revision_diff(output)
    paths = {path: action for action, path in entries}

    assert paths.get(link_path) == "M", (
        "`lore revision diff` must report the link path as a modification "
        "when the pin moved, even though no mounted file changed. Otherwise "
        f"the revision looks empty.\nOutput:\n{output}"
    )
    under_link = [path for path in paths if path.startswith(f"{link_path}/")]
    assert not under_link, (
        "`lore revision diff` must not report any file under the link path "
        f"when the mounted subtree is unchanged. Got: {under_link}"
        f"\nOutput:\n{output}"
    )


@pytest.mark.smoke
def test_link_add_remove_revision_diff_reports_link_only(new_lore_repo):
    """Adding or removing a link is reported as a single entry for the link
    path; the mounted contents are never expanded into per-file changes.

    Guards the asymmetry with the pin-update tests above: only an *updated*
    link expands into the linked repository's changes.
    """
    link_repo = make_repo(new_lore_repo, {"shared.txt": "main content\n"})
    pinned_revision = link_repo.branch_info().local_latest
    parent_repo = make_repo(new_lore_repo, {"README.txt": "baseline\n"})
    baseline_revision = parent_repo.branch_info().local_latest

    link_path = "libs/shared"
    linked_file = f"{link_path}/shared.txt"

    parent_repo.link_add(link_path, link_repo.get_id(), "/", pin=pinned_revision)
    parent_repo.commit("Add link libs/shared")
    parent_repo.push()
    post_add_revision = parent_repo.branch_info().local_latest

    add_output = parent_repo.revision_diff(baseline_revision, no_pager=True)
    add_paths = {path: action for action, path in parse_revision_diff(add_output)}

    assert add_paths.get(link_path) == "A", (
        f"Adding a link must report {link_path!r} as an addition."
        f"\nOutput:\n{add_output}"
    )
    assert linked_file not in add_paths, (
        "Adding a link must not expand the mounted contents into per-file "
        f"additions.\nOutput:\n{add_output}"
    )

    parent_repo.link_remove(link_path)
    parent_repo.commit("Remove link libs/shared")
    parent_repo.push()

    remove_output = parent_repo.revision_diff(post_add_revision, no_pager=True)
    remove_paths = {path: action for action, path in parse_revision_diff(remove_output)}

    assert remove_paths.get(link_path) == "D", (
        f"Removing a link must report {link_path!r} as a deletion."
        f"\nOutput:\n{remove_output}"
    )
    assert linked_file not in remove_paths, (
        "Removing a link must not expand the previously mounted contents "
        f"into per-file deletions.\nOutput:\n{remove_output}"
    )


# ---------------------------------------------------------------------------
# Link tracking on the thin-client wire.
# ---------------------------------------------------------------------------


def _wire_identity(repo: Lore) -> tuple[bytes, bytes]:
    """The repository id and latest revision signature as the raw bytes the
    thin-client wire expects."""
    latest = repo.branch_info().local_latest
    assert len(latest) == 64, f"Expected a full revision signature, got {latest!r}"
    return bytes.fromhex(repo.get_id()), bytes.fromhex(latest)


def _mount_changes(changes: list, link_path: str) -> list:
    """Every diff entry describing the link node at `link_path`."""
    return [
        change
        for change in changes
        if change.path == link_path and change.node_type == NODE_TYPE_LINK
    ]


@pytest.mark.smoke
def test_thin_client_tree_discriminates_tracking_from_pinned(
    new_lore_repo, lore_grpc_target
):
    """One tree holding both link kinds and a plain file: only the link left on
    its parent's branch reports tracking. Asserting all three off a single
    response is what proves the field is populated rather than left at its
    default."""
    tracking_source = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    pinned_source = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    repo.link_add("tracked", tracking_source.get_id(), "/")
    repo.link_add("pinned", pinned_source.get_id(), "/", disable_branching=True)
    repo.commit()
    repo.push()

    repository_id, signature = _wire_identity(repo)
    nodes = {
        node.path: node
        for node in revision_tree(lore_grpc_target, repository_id, signature)
    }
    for path in ("tracked", "pinned", "own.txt"):
        assert path in nodes, f"{path} missing from tree: {sorted(nodes)}"

    assert nodes["tracked"].node_type == NODE_TYPE_LINK, (
        f"Mount path must be reported as a link, got {nodes['tracked']}"
    )
    assert nodes["tracked"].tracking, (
        f"A link following its parent's branch must report tracking, "
        f"got {nodes['tracked']}"
    )

    assert nodes["pinned"].node_type == NODE_TYPE_LINK, (
        f"Mount path must be reported as a link, got {nodes['pinned']}"
    )
    assert nodes["pinned"].tracking is False, (
        f"A link pinned to its own branch must report tracking False, "
        f"got {nodes['pinned']}"
    )

    assert nodes["own.txt"].node_type == NODE_TYPE_FILE, (
        f"Parent's own file must be reported as a file, got {nodes['own.txt']}"
    )
    assert nodes["own.txt"].tracking is False, (
        f"Tracking is a link property, so a file must report False, "
        f"got {nodes['own.txt']}"
    )


@pytest.mark.smoke
def test_thin_client_diff_reports_tracking_on_added_link(
    new_lore_repo, lore_grpc_target
):
    """Adding a tracking link is reported as a tracking link entry on the
    thin-client revision diff."""
    link_repo = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    repository_id, before = _wire_identity(repo)

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_ADD for change in mount_changes), (
        f"A newly mounted link must be reported as an add, got {mount_changes}"
    )
    assert all(change.tracking for change in mount_changes), (
        f"A link following its parent's branch must report tracking, "
        f"got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_reports_tracking_on_removed_link(
    new_lore_repo, lore_grpc_target
):
    """Removing a tracking link reports tracking on the delete entry. A delete
    resolves against the "from" side, the only action that does."""
    link_repo = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    repo.link_remove(link_path)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_DELETE for change in mount_changes), (
        f"A removed link must be reported as a delete, got {mount_changes}"
    )
    assert all(change.tracking for change in mount_changes), (
        f"A link following its parent's branch must report tracking, "
        f"got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_reports_tracking_on_moved_pin(
    new_lore_repo, lore_grpc_target
):
    """Committing content through a tracking link moves its pin, and the pin
    move is reported as a tracking link entry on the thin-client revision
    diff, while the parent's own file change is not."""
    link_repo = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    with repo.open_file(os.path.join(link_path, "inner.txt"), "w+") as output_file:
        output_file.writelines(["linked content, revised\n"])
    with repo.open_file("own.txt", "w+") as output_file:
        output_file.writelines(["parent content, revised\n"])
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, (
        f"Moved pin must be reported as a link entry for the mount path, got {changes}"
    )
    assert all(change.tracking for change in mount_changes), (
        f"A link following its parent's branch must report tracking, "
        f"got {mount_changes}"
    )

    own_changes = [change for change in changes if change.path == "own.txt"]
    assert own_changes, f"Parent's own file change missing from diff: {changes}"
    assert all(change.tracking is False for change in own_changes), (
        f"Tracking is a link property, so a file change must report False, "
        f"got {own_changes}"
    )


# ---------------------------------------------------------------------------
# Link partitions on the thin-client wire.
# ---------------------------------------------------------------------------


@pytest.mark.smoke
def test_thin_client_diff_partitions_added_link_under_linked_repository(
    new_lore_repo, lore_grpc_target
):
    """A newly mounted link is the only diff entry for its mount path, and the
    content it names is a revision of the linked repository, so the entry must
    be partitioned there for a consumer to find that revision."""
    link_repo = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    repository_id, before = _wire_identity(repo)

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_ADD for change in mount_changes), (
        f"A newly mounted link must be reported as an add, got {mount_changes}"
    )
    assert all(change.partition == link_repo.get_id() for change in mount_changes), (
        f"A link entry's content is a revision of the linked repository "
        f"{link_repo.get_id()}, so it must be partitioned there, got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_partitions_removed_link_under_linked_repository(
    new_lore_repo, lore_grpc_target
):
    """A removed link names the revision it was pinned to, which still lives in
    the linked repository, so the delete entry must be partitioned there. A
    delete resolves against the "from" side, the only action that does."""
    link_repo = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    repo.link_remove(link_path)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, f"Mount path must be reported as a link entry, got {changes}"
    assert all(change.action == ACTION_DELETE for change in mount_changes), (
        f"A removed link must be reported as a delete, got {mount_changes}"
    )
    assert all(change.partition == link_repo.get_id() for change in mount_changes), (
        f"A removed link's content is the revision of the linked repository "
        f"{link_repo.get_id()} it was pinned to, so it must be partitioned there, "
        f"got {mount_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_partitions_moved_pin_under_linked_repository(
    new_lore_repo, lore_grpc_target
):
    """Committing through a link moves its pin, and every entry the diff reports
    for the mount path must agree on the linked repository as its partition,
    while the parent's own file stays in the parent's."""
    link_repo = make_repo(new_lore_repo, {"inner.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    link_path = "linked"
    repo.link_add(link_path, link_repo.get_id(), "/")
    repo.commit()
    repo.push()

    repository_id, before = _wire_identity(repo)

    with repo.open_file(os.path.join(link_path, "inner.txt"), "w+") as output_file:
        output_file.writelines(["linked content, revised\n"])
    with repo.open_file("own.txt", "w+") as output_file:
        output_file.writelines(["parent content, revised\n"])
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    mount_changes = _mount_changes(changes, link_path)
    assert mount_changes, (
        f"Moved pin must be reported as a link entry for the mount path, got {changes}"
    )
    assert all(change.partition == link_repo.get_id() for change in mount_changes), (
        f"A moved pin names revisions of the linked repository "
        f"{link_repo.get_id()}, so every entry for the mount path must be "
        f"partitioned there, got {mount_changes}"
    )

    own_changes = [change for change in changes if change.path == "own.txt"]
    assert own_changes, f"Parent's own file change missing from diff: {changes}"
    assert all(change.partition == repo.get_id() for change in own_changes), (
        f"The parent's own file lives in {repo.get_id()}, so its change must be "
        f"partitioned there, got {own_changes}"
    )


@pytest.mark.smoke
def test_thin_client_diff_partitions_each_link_under_its_own_repository(
    new_lore_repo, lore_grpc_target
):
    """Two links added in one commit take separate partition indices, and each
    mount entry resolves to the repository it mounts."""
    first_repo = make_repo(new_lore_repo, {"first.txt": "initial content\n"})
    second_repo = make_repo(new_lore_repo, {"second.txt": "initial content\n"})
    repo = make_repo(new_lore_repo, {"own.txt": "initial content\n"})

    repository_id, before = _wire_identity(repo)

    repo.link_add("first", first_repo.get_id(), "/")
    repo.link_add("second", second_repo.get_id(), "/")
    repo.commit()
    repo.push()

    _, after = _wire_identity(repo)
    changes = revision_diff(lore_grpc_target, repository_id, before, after)

    for link_path, link_repo in (("first", first_repo), ("second", second_repo)):
        mount_changes = _mount_changes(changes, link_path)
        assert mount_changes, (
            f"Mount path {link_path} must be reported as a link entry, got {changes}"
        )
        assert all(
            change.partition == link_repo.get_id() for change in mount_changes
        ), (
            f"Mount path {link_path} mounts {link_repo.get_id()}, so its entries "
            f"must be partitioned there, got {mount_changes}"
        )
