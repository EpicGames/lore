# SPDX-FileCopyrightText: 2026 LoreLab.io
# SPDX-License-Identifier: MIT

"""End-to-end coverage for `TreeNode.last_revision_index` and the interleaved
`TreeRevision` payloads on `ThinClientService.RevisionTree`.

The individual layers are proved by unit tests:

- Attribution correctness (`revision[0]` back-pointer, directory propagation,
  linked-subtree attribution and merges) lives in
  `lore-revision/tests/file_revision_history.rs`.
- Emission mechanics (dedup, ordering, indices, cross-repo non-collision) live
  in the `emission` submodule of
  `lore-server/tests/unit/grpc/thinclient/v1/revision_tree.rs`.
- Resolver read path lives in the same file's
  `load_revision_attribution_reads_the_revision_record` test.

This file exercises the whole path end-to-end against a running server: real
CLI commits populate the file-metadata back-pointers, the server walks and
attributes them, the interleaved emission ships `TreeRevision` payloads
alongside `TreeNode`s, and the wire assertions read what a real client would
see. It is the only regression net for the joined path.

Uses the wire-level `thin_client.py` harness rather than a generated client.
The test environment has no protobuf runtime and no generated stubs.
"""

import logging
import os

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


def _attributed_to(nodes, paths):
    """The signature each path attributes to, as lowercase hex, or None."""
    return {
        path: nodes[path].last_revision.signature.hex()
        if nodes[path].last_revision is not None
        else None
        for path in paths
    }


def _signatures(expected):
    """`expected` with its signatures lowercased, to compare with `_attributed_to`."""
    return {path: signature.lower() for path, signature in expected.items()}


@pytest.mark.smoke
def test_tree_attribution_is_gated_by_the_request_flag(new_lore_repo, lore_grpc_target):
    """Same revision, walked twice: `include_last_revision=False` returns no
    `last_revision` on any node and no `TreeRevision` payloads on the wire;
    `True` returns one on every attributed node and announces the underlying
    revisions. Asserts the wire respects the flag, distinct from asserting
    attribution correctness."""
    repo: Lore = new_lore_repo()

    with repo.open_file("a.txt", "w+") as f:
        f.write("first\n")
    repo.stage(scan=True)
    repo.commit(message="add a.txt")
    repo.push()

    repository_id, signature = _wire_identity(repo)

    unattributed = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_revision=False
    )
    assert unattributed.nodes, "The walk must emit at least one node"
    assert all(node.last_revision is None for node in unattributed.nodes), (
        "last_revision must be absent when the request does not ask for it, "
        f"got {[node for node in unattributed.nodes if node.last_revision is not None]}"
    )
    assert not unattributed.revisions, (
        "no TreeRevision payloads without the flag, "
        f"got {[r.index for r in unattributed.revisions]}"
    )

    attributed = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_revision=True
    )
    assert len(attributed) == len(unattributed), (
        "The flag must not change which nodes are emitted"
    )
    file_entries = [
        node for node in attributed.nodes if node.node_type == NODE_TYPE_FILE
    ]
    assert file_entries, "Sanity: fixture must produce at least one file entry"
    assert all(node.last_revision is not None for node in file_entries), (
        "Every file entry in a committed revision must be attributed, "
        f"got {[node.path for node in file_entries if node.last_revision is None]}"
    )
    assert attributed.revisions, (
        "attributed walk must announce at least one TreeRevision"
    )


@pytest.mark.smoke
def test_tree_attribution_matches_the_touching_revision(
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
    result = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_revision=True
    )
    nodes = _by_path(result.nodes)

    for path in ("a.txt", "b.txt"):
        assert path in nodes, f"{path} missing from tree: {sorted(nodes)}"
        node = nodes[path]
        assert node.last_revision is not None, f"{path} must be attributed"
        assert node.last_revision.commit_message == "r2 modify a.txt and add b.txt", (
            f"{path} must attribute to r2's commit, got "
            f"{node.last_revision.commit_message!r}"
        )
        # A r2 walk carries r2's identifier, which is number 2 on the branch.
        assert node.last_revision.number == 2, (
            f"{path} must carry the r2 branch-relative number, got "
            f"{node.last_revision.number}"
        )
        # `branch_id` comes back as a big-endian UUID (lore ids are network
        # byte order). The wire test only checks length; comparing to
        # `branch_info` involves the byte-order rule.
        assert len(node.last_revision.branch_id) == 16, (
            f"branch_id must be a 16-byte UUID, got {node.last_revision.branch_id!r}"
        )
        assert node.last_revision.signature == signature, (
            f"{path} must be attributed against the walked revision's signature"
        )

    # Both files attribute to the same commit, so the server must dedup:
    # exactly one TreeRevision announced, and both nodes reference it.
    attributed = [n for n in result.nodes if n.path in ("a.txt", "b.txt")]
    referenced_indices = {n.last_revision.index for n in attributed}
    assert len(referenced_indices) == 1, (
        "a.txt and b.txt share one revision; they must reference the same "
        f"TreeRevision index, got {referenced_indices}"
    )
    matching = [r for r in result.revisions if r.index in referenced_indices]
    assert len(matching) == 1, (
        "one shared revision must be announced exactly once, "
        f"got {[r.index for r in result.revisions]}"
    )


@pytest.mark.smoke
def test_tree_attribution_crosses_a_link(new_lore_repo, lore_grpc_target):
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
            lore_grpc_target,
            parent_id,
            parent_signature,
            include_last_revision=True,
        )
    )

    # Files in the parent tree attribute against the parent's most recent
    # commit that touched them.
    assert "top.txt" in nodes, f"top.txt missing from tree: {sorted(nodes)}"
    assert nodes["top.txt"].last_revision is not None
    assert (
        nodes["top.txt"].last_revision.commit_message == "parent commit before linking"
    ), (
        "top.txt must attribute to its own add revision, not the one that "
        f"added the link; got {nodes['top.txt'].last_revision.commit_message!r}"
    )

    # The link entry itself lives in the parent state and attributes to the
    # revision that added it.
    assert "vendor" in nodes, f"vendor link missing from tree: {sorted(nodes)}"
    assert nodes["vendor"].node_type == NODE_TYPE_LINK
    assert nodes["vendor"].last_revision is not None
    assert (
        nodes["vendor"].last_revision.commit_message == "parent commit adding link"
    ), (
        "The link node must attribute to the parent revision that added it, "
        f"got {nodes['vendor'].last_revision.commit_message!r}"
    )

    # Content inside the link lives in the linked repo's state and attributes
    # against that repo's revision - not the parent's. The commit message
    # carried on the wire proves the resolution reached the right repo.
    assert "vendor/inner.txt" in nodes, (
        f"linked-subtree content missing: {sorted(nodes)}"
    )
    inner = nodes["vendor/inner.txt"]
    assert inner.last_revision is not None, (
        "Content behind a link must be attributed now the walker crosses links"
    )
    assert inner.last_revision.commit_message == "inner commit in linked repo", (
        "Content inside a link must attribute to the linked repository's "
        f"revision, got {inner.last_revision.commit_message!r}"
    )
    assert inner.last_revision.signature == linked_signature, (
        "The linked-subtree entry must carry the linked repository's revision "
        "signature, not the walked repository's"
    )


@pytest.mark.smoke
def test_tree_attribution_of_a_directory_follows_its_descendants(
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
            lore_grpc_target,
            repository_id,
            signature,
            include_last_revision=True,
        )
    )

    assert "sub" in nodes, f"sub directory missing: {sorted(nodes)}"
    sub = nodes["sub"]
    assert sub.node_type == NODE_TYPE_DIRECTORY, (
        f"sub must be a directory, got node_type={sub.node_type}"
    )
    assert sub.last_revision is not None, "Directories propagate and must attribute"
    assert sub.last_revision.commit_message == "r2 modify sub/child.txt", (
        f"sub must move with its descendant to r2, got "
        f"{sub.last_revision.commit_message!r}"
    )


@pytest.mark.smoke
def test_tree_revision_indices_are_dense_from_one(new_lore_repo, lore_grpc_target):
    """The `TreeRevision` payloads a walk announces must use indices in
    first-referenced order; 0 is reserved and every index used by a
    `TreeNode` must have been announced earlier on the same stream. This is
    the wire-level version of the emission-loop requirement proved in
    `revision_tree.rs`'s `emission` unit tests, held here against the real
    server so a regression in the interleaving surfaces end-to-end.

    The `thin_client.py` harness enforces "announced before referenced" as
    it deserialises (an unannounced index asserts). This test locks in the
    dense-and-starts-at-one half."""
    repo: Lore = new_lore_repo()

    with repo.open_file("a.txt", "w+") as f:
        f.write("a\n")
    with repo.open_file("b.txt", "w+") as f:
        f.write("b\n")
    repo.stage(scan=True)
    repo.commit(message="two files at once")
    repo.push()

    repository_id, signature = _wire_identity(repo)
    result = revision_tree(
        lore_grpc_target,
        repository_id,
        signature,
        include_last_revision=True,
    )

    assert result.revisions, "attributed walk must announce revisions"
    announced_indices = [r.index for r in result.revisions]
    expected = list(range(1, len(announced_indices) + 1))
    assert announced_indices == expected, (
        f"TreeRevision indices must be dense and start at 1, got {announced_indices}"
    )

    # Every referenced index sits within the announced set. The harness
    # asserts wire-order already; this is the set-membership half.
    referenced = {
        node.last_revision.index
        for node in result.nodes
        if node.last_revision is not None
    }
    assert referenced.issubset(set(announced_indices)), (
        f"TreeNode referenced indices {referenced - set(announced_indices)} "
        f"that were never announced (announced: {announced_indices})"
    )


@pytest.mark.smoke
def test_tree_attribution_of_a_divergent_sync_merge(new_lore_repo, lore_grpc_target):
    """A sync of divergent history builds its merge on the local tree, with the
    remote latest as first parent and the local latest as second. Pushed and
    walked on the server, a local change keeps its own revision, and the
    remote's changes attribute to the merge that applied them."""
    repo: Lore = new_lore_repo()

    for name in ("base.txt", "early.txt"):
        with repo.open_file(name, "w+") as f:
            f.write(f"{name}\n")
    repo.stage(scan=True)
    repo.commit(message="Base")
    repo.push()
    base = repo.branch_info().local_latest

    clone = repo.clone()

    with repo.open_file("early.txt", "w+") as f:
        f.write("changed on the remote\n")
    repo.stage(scan=True)
    repo.commit(message="Change early.txt on the remote")
    repo.push()

    with repo.open_file("remote.txt", "w+") as f:
        f.write("remote\n")
    repo.stage(scan=True)
    repo.commit(message="Add remote.txt")
    repo.push()
    remote_latest = repo.branch_info().local_latest

    with clone.open_file("local.txt", "w+") as f:
        f.write("local\n")
    clone.stage("local.txt")
    clone.commit("Add local.txt", local=True)
    local_latest = clone.branch_info().local_latest

    # Neither side changed what the other did, so the sync commits the merge.
    clone.sync()
    merge = clone.branch_info().local_latest
    assert clone.revision_info(merge).merge.split() == [remote_latest, local_latest]
    clone.push()

    repository_id, signature = _wire_identity(clone)
    assert signature == bytes.fromhex(merge), "the walk must be of the merge"
    result = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_revision=True
    )
    nodes = _by_path(result.nodes)

    expected = {
        "base.txt": base,
        "early.txt": merge,
        "local.txt": local_latest,
        "remote.txt": merge,
    }
    assert _attributed_to(nodes, expected) == _signatures(expected)


@pytest.mark.smoke
def test_tree_attribution_of_a_merge(new_lore_repo, lore_grpc_target):
    """Pushed and walked on the server, an entry a merge brought in attributes
    to the merge, directories included, whether the other side modified it or
    added it. Every other entry keeps the first-parent revision that last
    changed it."""
    repo: Lore = new_lore_repo()

    repo.make_dirs(os.path.join("src", "core"))
    with repo.open_file(os.path.join("src", "core", "engine.txt"), "w+") as f:
        f.write("engine\n")
    with repo.open_file(os.path.join("src", "core", "helpers.txt"), "w+") as f:
        f.write("helpers\n")
    with repo.open_file("readme.txt", "w+") as f:
        f.write("readme\n")
    repo.stage(scan=True)
    repo.commit("Base", offline=True)
    repo.push()
    base = repo.branch_info().local_latest

    repo.branch_create("feature")
    repo.make_dirs(os.path.join("src", "extra"))
    with repo.open_file(os.path.join("src", "extra", "new.txt"), "w+") as f:
        f.write("new on feature\n")
    with repo.open_file(os.path.join("src", "core", "helpers.txt"), "w+") as f:
        f.write("helpers changed on feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Feature", offline=True)
    repo.push()
    feature = repo.branch_info().local_latest

    repo.branch_switch("main")
    repo.branch_merge_start("feature")
    repo.push()
    merge = repo.branch_info().local_latest
    assert repo.revision_info(merge).merge.split() == [base, feature], (
        "the case needs a merge, with main first and feature second"
    )

    repository_id, signature = _wire_identity(repo)
    result = revision_tree(
        lore_grpc_target, repository_id, signature, include_last_revision=True
    )
    nodes = _by_path(result.nodes)

    # What the merge brought in: one file modified, one added.
    merged_in = [
        "src/core/helpers.txt",
        "src/extra/new.txt",
        "src",
        "src/core",
        "src/extra",
    ]
    expected = {
        "readme.txt": base,
        "src/core/engine.txt": base,
        **dict.fromkeys(merged_in, merge),
    }
    assert _attributed_to(nodes, expected) == _signatures(expected)


@pytest.mark.smoke
def test_tree_attribution_of_a_cherry_pick_and_a_revert(
    new_lore_repo, lore_grpc_target
):
    """A committed cherry-pick or revert has no second parent, so it is an
    ordinary revision: an entry it changed attributes to it, and every other
    entry keeps the revision that last changed it."""
    repo: Lore = new_lore_repo()

    for name in ("picked.txt", "kept.txt"):
        with repo.open_file(name, "w+") as f:
            f.write(f"{name}\n")
    repo.stage(scan=True)
    repo.commit("Base", offline=True)
    repo.push()
    base = repo.branch_info().local_latest

    repo.branch_create("feature")
    with repo.open_file("picked.txt", "w+") as f:
        f.write("changed on feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Change picked.txt on feature", offline=True)
    repo.push()
    feature = repo.branch_info().local_latest

    repo.branch_switch("main")
    with repo.open_file("main.txt", "w+") as f:
        f.write("main\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Add main.txt", offline=True)
    repo.push()
    main = repo.branch_info().local_latest

    repo.revision_cherry_pick(revision=feature, message="Cherry-pick picked.txt")
    repo.push()
    cherry_pick = repo.branch_info().local_latest
    assert not repo.revision_info(cherry_pick).merge, "a cherry-pick has one parent"

    repo.revision_revert(revision=cherry_pick, message="Revert the cherry-pick")
    repo.push()
    revert = repo.branch_info().local_latest
    assert not repo.revision_info(revert).merge, "a revert has one parent"

    repository_id, _ = _wire_identity(repo)
    for walked, changed_by in ((cherry_pick, cherry_pick), (revert, revert)):
        nodes = _by_path(
            revision_tree(
                lore_grpc_target,
                repository_id,
                bytes.fromhex(walked),
                include_last_revision=True,
            ).nodes
        )
        expected = {"picked.txt": changed_by, "kept.txt": base, "main.txt": main}
        assert _attributed_to(nodes, expected) == _signatures(expected)
