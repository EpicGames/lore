# SPDX-FileCopyrightText: 2026 LoreLab.io
# SPDX-License-Identifier: MIT

"""End-to-end coverage for `TreeNode.last_revision_index` and the interleaved
`TreeRevision` payloads on `ThinClientService.RevisionTree`, against a running
server: CLI commits record each entry's history, and the server walks,
attributes and streams the entries as a client reads them.

Each attribution case is covered in
`lore-revision/tests/unit/revision/tree_walk.rs`, and emission in
`lore-server/tests/unit/grpc/thinclient/v1/revision_tree.rs`.

Uses the wire-level `thin_client.py` harness. The test environment has no
protobuf runtime and no generated stubs.
"""

import pytest
from thin_client import (
    NODE_TYPE_LINK,
    revision_tree,
    wire_identity,
)

from lore import Lore


def _by_path(nodes):
    """Index a tree walk by path; asserts uniqueness so a missing entry
    surfaces at lookup time rather than as an arbitrary duplicate."""
    indexed = {}
    for node in nodes:
        assert node.path not in indexed, f"{node.path} appears twice in the walk"
        indexed[node.path] = node
    return indexed


@pytest.mark.smoke
def test_tree_attribution_names_the_revision_that_changed_each_entry(
    new_lore_repo, lore_grpc_target
):
    """`a.txt`, changed at r2, and `b.txt`, added at r2, are both attributed
    to r2, announced once, and r2's message, number, signature, timestamp and
    committer reach the wire, with a 16-byte branch ID."""
    repo: Lore = new_lore_repo()

    with repo.open_file("a.txt", "w+") as f:
        f.write("first\n")
    repo.stage(scan=True)
    repo.commit(message="r1 add a.txt")
    repo.push()

    with repo.open_file("a.txt", "w+") as f:
        f.write("second\n")
    with repo.open_file("b.txt", "w+") as f:
        f.write("brand new\n")
    repo.stage(scan=True)
    repo.commit(message="r2 modify a.txt and add b.txt")
    repo.push()

    repository_id, signature = wire_identity(repo)
    result = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_revision=True
    )
    nodes = _by_path(result.nodes)

    for path in ("a.txt", "b.txt"):
        assert path in nodes, f"{path} missing from tree: {sorted(nodes)}"
        named = nodes[path].last_revision
        assert named is not None, f"{path} must be attributed"
        assert named.commit_message == "r2 modify a.txt and add b.txt", (
            f"{path} must be attributed to r2, got {named.commit_message!r}"
        )
        assert named.number == 2, f"{path} must carry r2's number, got {named.number}"
        assert len(named.branch_id) == 16, (
            f"branch_id must be a 16-byte UUID, got {named.branch_id!r}"
        )
        assert named.signature == signature, f"{path} must carry r2's signature"
        assert named.timestamp > 0, f"{path} must carry r2's timestamp"
        assert named.committed_by, f"{path} must carry r2's committer"

    indices = {nodes[path].last_revision.index for path in ("a.txt", "b.txt")}
    assert len(indices) == 1, f"one revision must have one index, got {indices}"
    announced = [r.index for r in result.revisions if r.index in indices]
    assert len(announced) == 1, (
        f"the revision must be announced once, got {[r.index for r in result.revisions]}"
    )


@pytest.mark.smoke
@pytest.mark.own_vfs("links a repository, which a virtual clone does not hold")
def test_tree_attribution_crosses_a_link(new_lore_repo, lore_grpc_target):
    """An entry under a link is in the linked repository and attributed to its
    revision there. The link names the linked repository and is attributed to
    the walked repository's revision that added it, and the entry beside it is
    in the walked repository."""
    linked: Lore = new_lore_repo()
    with linked.open_file("inner.txt", "w+") as f:
        f.write("inner content\n")
    linked.stage(scan=True)
    linked.commit(message="inner revision in linked repository")
    linked.push()
    linked_id, linked_signature = wire_identity(linked)

    parent: Lore = new_lore_repo()
    with parent.open_file("top.txt", "w+") as f:
        f.write("parent content\n")
    parent.stage(scan=True)
    parent.commit(message="parent revision before linking")
    parent.push()

    parent.link_add("vendor", linked.get_id(), "/")
    parent.commit(message="parent revision adding link")
    parent.push()

    parent_id, parent_signature = wire_identity(parent)
    nodes = _by_path(
        revision_tree(
            lore_grpc_target,
            parent_id,
            parent_signature,
            include_last_revision=True,
        ).nodes
    )

    expected = {
        "top.txt": "parent revision before linking",
        "vendor": "parent revision adding link",
        "vendor/inner.txt": "inner revision in linked repository",
    }
    for path, message in expected.items():
        assert path in nodes, f"{path} missing from tree: {sorted(nodes)}"
        named = nodes[path].last_revision
        assert named is not None, f"{path} must be attributed"
        assert named.commit_message == message, (
            f"{path} must be attributed to {message!r}, got {named.commit_message!r}"
        )
    assert nodes["vendor"].node_type == NODE_TYPE_LINK
    for path, content_in, revision_in in (
        ("top.txt", parent_id, parent_id),
        ("vendor", linked_id, parent_id),
        ("vendor/inner.txt", linked_id, linked_id),
    ):
        assert nodes[path].partition == content_in.hex(), (
            f"{path} must name repository {content_in.hex()}, "
            f"got {nodes[path].partition}"
        )
        assert nodes[path].last_revision.partition == revision_in.hex(), (
            f"{path} must be attributed to a revision of {revision_in.hex()}, "
            f"got {nodes[path].last_revision.partition}"
        )
    assert nodes["vendor/inner.txt"].last_revision.signature == linked_signature, (
        "an entry under a link must carry the linked repository's revision signature"
    )
