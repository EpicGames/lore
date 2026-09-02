# SPDX-FileCopyrightText: 2026 LoreLab.io
# SPDX-License-Identifier: MIT

"""End-to-end coverage for `TreeNode.last_commit` on `ThinClientService.RevisionTree`.

The fully joined attribution path is proved in pieces below the wire (attribution in
`lore-revision`, resolution and projection in `lore-server`), but the lore-server
fixtures serialize states directly rather than committing, so nothing attributes there.

This runs against a real server, so it is the only regression net for the
joined path.

Uses the wire-level `thin_client.py` harness rather than a generated client - the test
environment has no protobuf runtime and no generated stubs.
"""

import logging

import pytest
from thin_client import (
    NODE_TYPE_DIRECTORY,
    NODE_TYPE_FILE,
    NODE_TYPE_LINK,
    revision_tree,
)

from lore import Lore

logger = logging.getLogger(__name__)


def _wire_identity(repo: Lore) -> tuple[bytes, bytes]:
    """The repository id and latest revision signature as the raw bytes the
    thin-client wire expects."""
    latest = repo.branch_info().local_latest
    assert len(latest) == 64, f"Expected a full revision signature, got {latest!r}"
    return bytes.fromhex(repo.get_id()), bytes.fromhex(latest)


def _by_path(nodes):
    """Index a tree walk by path; asserts uniqueness so a missing entry
    surfaces at lookup time rather than as an arbitrary duplicate."""
    indexed = {}
    for node in nodes:
        assert node.path not in indexed, f"{node.path} appears twice in the walk"
        indexed[node.path] = node
    return indexed


@pytest.mark.smoke
def test_thin_client_tree_last_commit_gated_by_request_flag(
    new_lore_repo, lore_grpc_target
):
    """Same revision, walked twice: `include_last_commit=False` returns no
    `last_commit` on any node; `True` returns one on every attributed node.
    Asserts the wire respects the flag, distinct from asserting attribution
    correctness."""
    repo: Lore = new_lore_repo()

    with repo.open_file("a.txt", "w+") as f:
        f.write("first\n")
    repo.stage(scan=True)
    repo.commit(message="add a.txt")
    repo.push()

    repository_id, signature = _wire_identity(repo)

    unattributed = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_commit=False
    )
    assert unattributed, "The walk must emit at least one node"
    assert all(node.last_commit is None for node in unattributed), (
        "last_commit must be absent when the request does not ask for it, "
        f"got {[node for node in unattributed if node.last_commit is not None]}"
    )

    attributed = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_commit=True
    )
    assert len(attributed) == len(unattributed), (
        "The flag must not change which nodes are emitted"
    )
    file_entries = [node for node in attributed if node.node_type == NODE_TYPE_FILE]
    assert file_entries, "Sanity: fixture must produce at least one file entry"
    assert all(node.last_commit is not None for node in file_entries), (
        "Every file entry in a committed revision must be attributed, "
        f"got {[node.path for node in file_entries if node.last_commit is None]}"
    )


@pytest.mark.smoke
def test_thin_client_tree_last_commit_matches_the_touching_revision(
    new_lore_repo, lore_grpc_target
):
    """A two-revision fixture separates a per-entry back-pointer from a
    parent-revision stamp. `a.txt` (touched at r2) and `b.txt` (added at r2)
    both attribute to r2; the point of the test is that both commit messages
    round-trip through the wire, so the projection is doing what the
    lore-revision unit tests can only prove locally."""
    repo: Lore = new_lore_repo()

    # r1: add a.txt with the "first" content.
    with repo.open_file("a.txt", "w+") as f:
        f.write("first\n")
    repo.stage(scan=True)
    repo.commit(message="r1 add a.txt")
    repo.push()

    # r2: modify a.txt and add b.txt.
    with repo.open_file("a.txt", "w+") as f:
        f.write("second\n")
    with repo.open_file("b.txt", "w+") as f:
        f.write("brand new\n")
    repo.stage(scan=True)
    repo.commit(message="r2 modify a.txt and add b.txt")
    repo.push()

    repository_id, signature = _wire_identity(repo)
    nodes = _by_path(
        revision_tree(
            lore_grpc_target, repository_id, signature, include_last_commit=True
        )
    )

    for path in ("a.txt", "b.txt"):
        assert path in nodes, f"{path} missing from tree: {sorted(nodes)}"
        node = nodes[path]
        assert node.last_commit is not None, f"{path} must be attributed"
        assert node.last_commit.commit_message == "r2 modify a.txt and add b.txt", (
            f"{path} must attribute to r2's commit, got "
            f"{node.last_commit.commit_message!r}"
        )
        # A r2 walk carries r2's identifier, which is number 2 on the branch.
        assert node.last_commit.number == 2, (
            f"{path} must carry the r2 branch-relative number, got "
            f"{node.last_commit.number}"
        )
        # `branch_id` comes back as a big-endian UUID (lore ids are network
        # byte order; see .agents/Discoveries.md). The wire test only checks
        # length; comparing to `branch_info` involves the byte-order rule.
        assert len(node.last_commit.branch_id) == 16, (
            f"branch_id must be a 16-byte UUID, got {node.last_commit.branch_id!r}"
        )
        assert node.last_commit.signature == signature, (
            f"{path} must be attributed against the walked revision's signature"
        )


@pytest.mark.smoke
def test_thin_client_tree_last_commit_attributes_across_a_link(
    new_lore_repo, lore_grpc_target
):
    """A link's contents live in the linked repository, and their attribution
    lives with them: `linked/inner.txt` reports the *linked* repository's
    revision on the wire, not the parent's. The link entry itself is
    attributed against the walked repository because it lives in the walked
    state."""
    linked: Lore = new_lore_repo()
    with linked.open_file("inner.txt", "w+") as f:
        f.write("inner content\n")
    linked.stage(scan=True)
    linked.commit(message="inner commit in linked repo")
    linked.push()
    _, linked_signature = _wire_identity(linked)

    parent: Lore = new_lore_repo()
    with parent.open_file("top.txt", "w+") as f:
        f.write("parent content\n")
    parent.stage(scan=True)
    parent.commit(message="parent commit before linking")
    parent.push()

    parent.link_add("vendor", linked.get_id(), "/")
    parent.commit(message="parent commit adding link")
    parent.push()

    parent_id, parent_signature = _wire_identity(parent)
    nodes = _by_path(
        revision_tree(
            lore_grpc_target, parent_id, parent_signature, include_last_commit=True
        )
    )

    # Files in the parent tree attribute against the parent's most recent
    # commit that touched them.
    assert "top.txt" in nodes, f"top.txt missing from tree: {sorted(nodes)}"
    assert nodes["top.txt"].last_commit is not None
    assert (
        nodes["top.txt"].last_commit.commit_message == "parent commit before linking"
    ), (
        "top.txt must attribute to its own add revision, not the one that "
        f"added the link; got {nodes['top.txt'].last_commit.commit_message!r}"
    )

    # The link entry itself lives in the parent state and attributes to the
    # revision that added it.
    assert "vendor" in nodes, f"vendor link missing from tree: {sorted(nodes)}"
    assert nodes["vendor"].node_type == NODE_TYPE_LINK
    assert nodes["vendor"].last_commit is not None
    assert (
        nodes["vendor"].last_commit.commit_message == "parent commit adding link"
    ), (
        "The link node must attribute to the parent revision that added it, "
        f"got {nodes['vendor'].last_commit.commit_message!r}"
    )

    # Content inside the link lives in the linked repo's state and attributes
    # against that repo's revision - not the parent's. The commit message
    # carried on the wire proves the resolution reached the right repo.
    assert "vendor/inner.txt" in nodes, (
        f"linked-subtree content missing: {sorted(nodes)}"
    )
    inner = nodes["vendor/inner.txt"]
    assert inner.last_commit is not None, (
        "Content behind a link must be attributed now the walker crosses links"
    )
    assert inner.last_commit.commit_message == "inner commit in linked repo", (
        "Content inside a link must attribute to the linked repository's "
        f"revision, got {inner.last_commit.commit_message!r}"
    )
    assert inner.last_commit.signature == linked_signature, (
        "The linked-subtree entry must carry the linked repository's revision "
        "signature, not the walked repository's"
    )


@pytest.mark.smoke
def test_thin_client_tree_last_commit_directory_follows_descendant(
    new_lore_repo, lore_grpc_target
):
    """Directories propagate descendant changes: a subdirectory whose child
    just changed reports the tip's commit, not an earlier one. This is what
    lets a UI attribute directory rows without a max-over-descendants pass."""
    repo: Lore = new_lore_repo()

    repo.make_dirs("sub")
    with repo.open_file("sub/child.txt", "w+") as f:
        f.write("first\n")
    repo.stage(scan=True)
    repo.commit(message="r1 add sub/child.txt")
    repo.push()

    with repo.open_file("sub/child.txt", "w+") as f:
        f.write("second\n")
    repo.stage(scan=True)
    repo.commit(message="r2 modify sub/child.txt")
    repo.push()

    repository_id, signature = _wire_identity(repo)
    nodes = _by_path(
        revision_tree(
            lore_grpc_target, repository_id, signature, include_last_commit=True
        )
    )

    assert "sub" in nodes, f"sub directory missing: {sorted(nodes)}"
    sub = nodes["sub"]
    assert sub.node_type == NODE_TYPE_DIRECTORY, (
        f"sub must be a directory, got node_type={sub.node_type}"
    )
    assert sub.last_commit is not None, "Directories propagate and must attribute"
    assert sub.last_commit.commit_message == "r2 modify sub/child.txt", (
        f"sub must move with its descendant to r2, got "
        f"{sub.last_commit.commit_message!r}"
    )
