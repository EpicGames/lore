# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import pytest
import re
from lore import Lore
from lore_parsers import (
    parse_branch_info,
    parse_complete_json,
    parse_jsonl,
    parse_layer_list_json,
    parse_layer_remove_json,
    parse_status_json,
)


logger = logging.getLogger(__name__)


def _setup_repo_with_layer(new_lore_repo):
    """Create a main repo with a layer repo and initial content in both.

    Uses matching target_path and source_path ("lay" -> "lay/") so that
    layer::sync works (target_path != source_path is not yet implemented).
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    repo.write_commit_push(None, {"main_file.txt": b"main content"})

    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/layer_file.txt": b"layer content v1"})

    repo.layer_add("lay", layer_repo, "lay/")
    return repo, layer_repo


@pytest.mark.smoke
def test_layer_add_list_remove(new_lore_repo):
    """
    An repo repository can have layers added, listed and removed
    """

    repo: Lore = new_lore_repo()
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    third_repo: Lore = new_lore_repo(repo.name + "_third")

    with repo.open_file("root_repo.txt", mode="w+b") as out:
        out.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    second_file = os.path.join("second", "second_repo.txt")
    second_repo.make_dirs(os.path.dirname(second_file))
    with second_repo.open_file(second_file, mode="w+b") as out:
        out.write(os.urandom(1000))

    second_repo.stage(scan=True)
    second_repo.commit()
    second_repo.push()

    third_file = os.path.join("third", "third_repo.txt")
    third_repo.make_dirs(os.path.dirname(third_file))
    with third_repo.open_file(third_file, mode="w+b") as out:
        out.write(os.urandom(1000))

    third_repo.stage(scan=True)
    third_repo.commit()
    third_repo.push()

    repo.layer_add("sec", second_repo, "/")
    repo.layer_add("thr", third_repo, "third/")

    # Verify the files were cloned as expected
    assert os.path.isdir(os.path.join(repo.path, "sec", "second")), (
        "Layer was not added in expected path"
    )
    assert os.path.isfile(
        os.path.join(repo.path, "sec", "second", "second_repo.txt")
    ), "Layer did not clone expected file"

    assert os.path.isdir(os.path.join(repo.path, "thr")), (
        "Layer was not added in expected path"
    )
    assert os.path.isfile(os.path.join(repo.path, "thr", "third_repo.txt")), (
        "Layer did not clone expected file"
    )

    output = repo.layer_list()

    count = sum(
        bool(re.match(r"^[0-9A-Fa-f]{32}", line)) for line in output.splitlines()
    )
    assert count == 2, "Unexpected number of layers in list output"

    assert "sec" in output and "thr" in output, (
        "Expected layer paths not in list output"
    )

    # Remove the second layer and verify only the third remains.
    remove_output = repo.layer_remove("sec", second_repo, json=True)
    remove_event = parse_layer_remove_json(remove_output)
    assert remove_event is not None, (
        f"Expected layerRemove event, got: {remove_output}"
    )
    assert remove_event.get("targetPath") == "sec"
    assert remove_event.get("forced") == 0
    assert remove_event.get("purged") == 0
    assert remove_event.get("modifiedCount") == 0
    assert remove_event.get("fileCount") == 1

    list_output = repo.layer_list(json=True)
    remaining = parse_layer_list_json(list_output)
    assert len(remaining) == 1, f"Expected single remaining layer, got {remaining}"
    assert remaining[0].get("targetPath") == "thr"

    assert not os.path.exists(os.path.join(repo.path, "sec")), (
        "Layer mount directory should be gone after remove"
    )
    assert os.path.isfile(os.path.join(repo.path, "thr", "third_repo.txt")), (
        "Other layer's files must remain after removing 'sec'"
    )


@pytest.mark.smoke
def test_layer_stage_status_commit(new_lore_repo):
    """
    An repo repository with layers can have files staged, status checked and committed
    """

    repo: Lore = new_lore_repo()
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    third_repo: Lore = new_lore_repo(repo.name + "_third")

    with repo.open_file("root_repo.txt", mode="w+b") as out:
        out.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    second_file = os.path.join("second", "second_repo.txt")
    second_repo.make_dirs(os.path.dirname(second_file))
    with second_repo.open_file(second_file, mode="w+b") as out:
        out.write(os.urandom(1000))

    second_repo.stage(scan=True)
    second_repo.commit()
    second_repo.push()

    third_file = os.path.join("third", "third_repo.txt")
    third_repo.make_dirs(os.path.dirname(third_file))
    with third_repo.open_file(third_file, mode="w+b") as out:
        out.write(os.urandom(1000))

    third_repo.stage(scan=True)
    third_repo.commit()
    third_repo.push()

    repo.layer_add("sec", second_repo, "/")
    repo.layer_add("thr", third_repo, "third/")

    output = repo.layer_list()
    previous_revision = ""
    for line in output.splitlines():
        if "third -> thr" in line:
            parts = line.split()
            previous_revision = parts[1]
            break

    third_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(third_file, mode="wb") as out:
        out.write(os.urandom(2000))

    repo.stage(os.path.join("thr", "third_repo.txt"), debug=True)

    status_output = repo.status(json=True)
    status_entries = parse_status_json(status_output)
    assert len(status_entries) == 1, (
        f"Expected 1 status entry, got {len(status_entries)}: {status_entries}"
    )
    entry = status_entries[0]
    assert entry.get("path") == "thr/third_repo.txt", (
        f"Expected path 'thr/third_repo.txt', got: {entry.get('path')}"
    )
    assert entry.get("flagStaged") is True, (
        f"Expected flagStaged=true, got: {entry.get('flagStaged')}"
    )
    assert entry.get("action") == "keep", (
        f"Expected action='keep' (modified), got: {entry.get('action')}"
    )

    output = repo.commit(debug=True)

    output = repo.layer_list()
    new_revision = None
    for line in output.splitlines():
        if "third -> thr" in line:
            parts = line.split()
            new_revision = parts[1]
            break
    assert new_revision is not None
    assert previous_revision != new_revision

    status_output = repo.status(json=True)
    status_entries = parse_status_json(status_output)
    assert len(status_entries) == 0, (
        f"Expected 0 status entry, got {len(status_entries)}: {status_entries}"
    )

    repo.push()


@pytest.mark.smoke
def test_layer_branch_create(new_lore_repo):
    """
    An repo repository with layers can have branches created
    """

    repo: Lore = new_lore_repo()
    second_repo: Lore = new_lore_repo(repo.name + "_second")

    with repo.open_file("root_repo.txt", mode="w+b") as out:
        out.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    second_file = os.path.join("second", "second_repo.txt")
    second_repo.make_dirs(os.path.dirname(second_file))
    with second_repo.open_file(second_file, mode="w+b") as out:
        out.write(os.urandom(1000))

    second_repo.stage(scan=True)
    second_repo.commit()
    second_repo.push()

    repo.layer_add("sec", second_repo, "/")

    repo.branch_create("test-branch")
    repo.push()

    repo_branch_list = repo.branch_list()
    second_branch_list = second_repo.branch_list()

    print(str(repo_branch_list))
    print(str(second_branch_list))
    assert repo_branch_list.has_remote_branch("test-branch")
    assert second_branch_list.has_remote_branch("test-branch")


@pytest.mark.smoke
def test_layer_branch_switch_basic(new_lore_repo):
    """
    Switching to a branch that exists in the layer repo syncs layer files
    to the latest revision on that branch
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    # Create branch in main repo (also creates in layer)
    repo.branch_create("feature")
    repo.push()

    # Switch to feature branch
    repo.branch_switch("feature")

    # Make a change in the layer on the feature branch
    with repo.open_file(os.path.join("lay", "layer_file.txt"), "wb") as f:
        f.write(b"layer content feature")
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Switch back to main branch
    output = repo.branch_switch("main", json=True)
    events = parse_jsonl(output, "branchSwitchEnd")
    assert len(events) > 0, "Expected branchSwitchEnd event"
    assert events[0]["branch"]["name"] == "main"

    # Layer file should be back to the original content
    with repo.open_file(os.path.join("lay", "layer_file.txt"), "rb") as f:
        content = f.read()
    assert content == b"layer content v1", (
        f"Expected original layer content after switch to main, got: {content}"
    )

    # Switch back to feature branch
    output = repo.branch_switch("feature", json=True)
    events = parse_jsonl(output, "branchSwitchEnd")
    assert len(events) > 0, "Expected branchSwitchEnd event"
    assert events[0]["branch"]["name"] == "feature"

    # Layer file should have the feature content
    with repo.open_file(os.path.join("lay", "layer_file.txt"), "rb") as f:
        content = f.read()
    assert content == b"layer content feature", (
        f"Expected feature layer content after switch, got: {content}"
    )


@pytest.mark.smoke
def test_layer_branch_switch_creates_missing_branch(new_lore_repo):
    """
    Switching to a branch creates the branch in the layer repo if it
    does not already exist there
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    repo.write_commit_push(None, {"main_file.txt": b"main content"})

    # Create branch before adding the layer so the layer doesn't know about it
    repo.branch_create("new-feature")
    repo.push()
    repo.branch_switch("main")

    # Now create and add the layer — only the main branch is propagated
    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/layer_file.txt": b"layer content v1"})
    repo.layer_add("lay", layer_repo, "lay/")

    # Verify the branch does NOT exist in the layer yet
    layer_branch_list = layer_repo.branch_list()
    assert not layer_branch_list.has_remote_branch("new-feature"), (
        "Branch should not exist in layer before switch"
    )

    # Switch to the branch — layer_branch_switch should create it in the layer
    repo.branch_switch("new-feature")
    # Push to ensure branch is created on remote
    repo.push()

    # The branch should now exist in the layer repo
    layer_branch_list = layer_repo.branch_list()
    assert layer_branch_list.has_remote_branch("new-feature"), (
        f"Expected 'new-feature' branch in layer repo, got: {layer_branch_list}"
    )

    # Verify the main repo branch info confirms we're on the new branch
    branch_info = repo.branch_info()
    assert branch_info.name == "new-feature"


@pytest.mark.smoke
def test_layer_branch_switch_multiple_layers(new_lore_repo):
    """
    Branch switch correctly handles multiple layers, switching all of them
    """
    repo: Lore = new_lore_repo()
    layer_a: Lore = new_lore_repo(repo.name + "_layer_a")
    layer_b: Lore = new_lore_repo(repo.name + "_layer_b")

    repo.write_commit_push(None, {"root.txt": b"root"})

    repo.branch_create("multi-branch")
    repo.push()
    repo.branch_switch("main")

    layer_a.make_dirs("la")
    layer_a.write_commit_push(None, {"la/a_file.txt": b"layer a v1"})

    layer_b.make_dirs("lb")
    layer_b.write_commit_push(None, {"lb/b_file.txt": b"layer b v1"})

    repo.layer_add("la", layer_a, "la/")
    repo.layer_add("lb", layer_b, "lb/")

    # Switch branch should create in layer repos
    repo.branch_switch("multi-branch")

    # Modify both layers on the feature branch
    repo.write_commit_push(
        None,
        {
            os.path.join("la", "a_file.txt"): b"layer a feature",
            os.path.join("lb", "b_file.txt"): b"layer b feature",
        },
    )

    # Switch back to main
    repo.branch_switch("main")

    with repo.open_file(os.path.join("la", "a_file.txt"), "rb") as f:
        assert f.read() == b"layer a v1"
    with repo.open_file(os.path.join("lb", "b_file.txt"), "rb") as f:
        assert f.read() == b"layer b v1"

    # Switch to feature again
    repo.branch_switch("multi-branch")

    with repo.open_file(os.path.join("la", "a_file.txt"), "rb") as f:
        assert f.read() == b"layer a feature"
    with repo.open_file(os.path.join("lb", "b_file.txt"), "rb") as f:
        assert f.read() == b"layer b feature"


@pytest.mark.smoke
def test_layer_branch_switch_sync_latest(new_lore_repo):
    """
    After switching branches, the layer is synced to the latest revision
    on the target branch, not the branch point revision
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    # Create feature branch, also switches
    repo.branch_create("evolving")
    repo.push()

    # Commit on the feature branch (revision 2 in the layer)
    repo.write_commit_push(
        None, {os.path.join("lay", "layer_file.txt"): b"evolving v1"}
    )

    # Make another commit on the main branch to diverge layer states
    repo.branch_switch("main")
    repo.write_commit_push(
        None, {os.path.join("lay", "layer_file.txt"): b"main v2"}
    )

    # Switch back to evolving - should be at the feature revision
    repo.branch_switch("evolving")

    with repo.open_file(os.path.join("lay", "layer_file.txt"), "rb") as f:
        content = f.read()
    assert content == b"evolving v1", (
        f"Expected feature layer content 'evolving v1', got: {content}"
    )

    # Switch to main - should be at the main revision
    repo.branch_switch("main")

    with repo.open_file(os.path.join("lay", "layer_file.txt"), "rb") as f:
        content = f.read()
    assert content == b"main v2", (
        f"Expected main layer content 'main v2', got: {content}"
    )


@pytest.mark.smoke
def test_layer_branch_switch_name_collision(new_lore_repo):
    """
    When a layer repo already has a branch with the same name but different
    ID, switching to a branch handles the name collision in the layer by
    creating the branch with a unique suffix. Content committed on each
    branch remains independent.
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    repo.write_commit_push(None, {"main.txt": b"main"})

    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {"lay/layer.txt": b"layer v1"})

    # Create the branch in main repo BEFORE adding the layer.
    # branch_create also switches to the new branch, so switch back
    # to main before adding the layer.
    repo.branch_create("colliding-name")
    repo.push()
    repo.branch_switch("main")

    # Create a branch in the layer repo independently with the same name.
    # This will have a different branch ID than the main repo's branch.
    # Commit unique content on it so we can verify independence later.
    layer_repo.branch_create("colliding-name")
    layer_repo.push()
    layer_repo.write_commit_push(
        None, {"lay/layer.txt": b"layer original branch"}
    )
    layer_repo.branch_switch("main")

    # Add the layer while on main branch (layer_add checks by branch ID,
    # main's ID matches, so no collision here)
    repo.layer_add("lay", layer_repo, "lay/")

    # Switch to the branch — layer_branch_switch encounters the name
    # collision: "colliding-name" exists in the layer with a different ID.
    # It should create a suffixed branch and succeed.
    repo.branch_switch("colliding-name")

    # Verify main repo is on the correct branch
    branch_info = repo.branch_info()
    assert branch_info.name == "colliding-name"

    # Commit content on the auto-created branch via the main repo
    repo.write_commit_push(
        None, {os.path.join("lay", "layer.txt"): b"layer autocreated branch"}
    )

    # Materialized layer file in the main repo should have the autocreated
    # branch content
    with repo.open_file(os.path.join("lay", "layer.txt"), "rb") as f:
        content = f.read()
    assert content == b"layer autocreated branch", (
        f"Expected autocreated branch content, got: {content}"
    )

    # The layer repo's original "colliding-name" branch should be untouched —
    # switch to it and verify its content is independent
    layer_repo.branch_switch("colliding-name")
    with layer_repo.open_file(os.path.join("lay", "layer.txt"), "rb") as f:
        content = f.read()
    assert content == b"layer original branch", (
        f"Expected original branch content in layer repo, got: {content}"
    )


def _setup_repo_with_two_layers(new_lore_repo):
    """Set up a parent repo with two non-overlapping layers (sec, thr) and
    initial content in each. Returns (parent_repo, second_repo, third_repo).
    """
    repo: Lore = new_lore_repo()
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    third_repo: Lore = new_lore_repo(repo.name + "_third")

    with repo.open_file("root_repo.txt", mode="w+b") as out:
        out.write(os.urandom(1000))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    second_file = os.path.join("second", "second_repo.txt")
    second_repo.make_dirs(os.path.dirname(second_file))
    with second_repo.open_file(second_file, mode="w+b") as out:
        out.write(os.urandom(1000))
    second_repo.stage(scan=True)
    second_repo.commit()
    second_repo.push()

    third_file = os.path.join("third", "third_repo.txt")
    third_repo.make_dirs(os.path.dirname(third_file))
    with third_repo.open_file(third_file, mode="w+b") as out:
        out.write(os.urandom(1000))
    third_repo.stage(scan=True)
    third_repo.commit()
    third_repo.push()

    repo.layer_add("sec", second_repo, "/")
    repo.layer_add("thr", third_repo, "third/")

    return repo, second_repo, third_repo


@pytest.mark.smoke
def test_layer_stage_root_dot(new_lore_repo):
    """`lore stage .` (or no args) in a repo with two layers stages the parent's
    own files AND each layer's matching subtree.

    Verifies the per-path loop routes the empty/root path to the parent walker
    (with layer subtrees masked) AND to a stage task per configured layer, so
    all three repositories receive their own staged changes.
    """
    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    with repo.open_file("root_repo.txt", mode="wb") as out:
        out.write(os.urandom(1500))
    sec_file = os.path.join("sec", "second", "second_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(os.urandom(1500))
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(1500))

    repo.stage(scan=True)

    status_output = repo.status(json=True)
    status_entries = parse_status_json(status_output)
    paths = sorted(e.get("path") for e in status_entries)
    expected = sorted([
        "root_repo.txt",
        "sec/second/second_repo.txt",
        "thr/third_repo.txt",
    ])
    assert paths == expected, (
        f"Expected staged entries {expected}, got {paths}: {status_entries}"
    )
    for entry in status_entries:
        assert entry.get("flagStaged") is True, (
            f"Expected flagStaged=true for {entry.get('path')}: {entry}"
        )


@pytest.mark.smoke
def test_layer_stage_ancestor(new_lore_repo):
    """`lore stage <ancestor>` where the ancestor is a parent of one or more
    layers stages the parent (with each layer's subtree masked) AND every
    matched layer.
    """
    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    # Modify a parent file at the repo root (outside any layer)
    with repo.open_file("root_repo.txt", mode="wb") as out:
        out.write(os.urandom(1500))
    # Modify a file inside the "sec" layer
    sec_file = os.path.join("sec", "second", "second_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(os.urandom(1500))
    # Modify a file inside the "thr" layer
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(1500))

    # Stage from repo root (== ancestor of both layers AND parent's own files),
    # explicit "." path rather than the no-arg form covered above.
    repo.stage(".", scan=True)

    status_output = repo.status(json=True)
    status_entries = parse_status_json(status_output)
    paths = sorted(e.get("path") for e in status_entries)
    expected = sorted([
        "root_repo.txt",
        "sec/second/second_repo.txt",
        "thr/third_repo.txt",
    ])
    assert paths == expected, (
        f"Expected staged entries {expected}, got {paths}: {status_entries}"
    )


@pytest.mark.smoke
def test_layer_stage_outside_any_layer(new_lore_repo):
    """`lore stage <path-outside-any-layer>` stages only the parent; layers
    that have separately modified files are not staged.
    """
    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    # Modify the parent's own file
    with repo.open_file("root_repo.txt", mode="wb") as out:
        out.write(os.urandom(1500))
    # Modify files in both layers — these MUST NOT be staged because the
    # stage path doesn't cover them.
    sec_file = os.path.join("sec", "second", "second_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(os.urandom(1500))
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(1500))

    # Stage just the parent file
    repo.stage("root_repo.txt")

    status_output = repo.status(json=True)
    status_entries = parse_status_json(status_output)
    paths = [e.get("path") for e in status_entries]
    assert paths == ["root_repo.txt"], (
        f"Expected only ['root_repo.txt'] staged, got {paths}: {status_entries}"
    )


def _layer_pinned_revision(repo: Lore, target_path: str) -> str:
    """Return the pinned revision hash of the layer at `target_path` from `lore layer list`."""
    output = repo.layer_list()
    for line in output.splitlines():
        if f"-> {target_path}" in line:
            parts = line.split()
            return parts[1]
    return ""


@pytest.mark.smoke
def test_layer_scoped_commit(new_lore_repo):
    """`lore commit "msg" --layer <path>` commits only the named layer's staged
    changes. The layer's pinned revision advances and no staged entries remain
    on the parent afterwards.
    """
    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    initial_thr_revision = _layer_pinned_revision(repo, "thr")
    assert initial_thr_revision != "", "Expected thr layer to have a pinned revision"

    # Modify a file inside the "thr" layer
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(2000))

    repo.stage(thr_file)
    repo.commit("Layer-only fix", layer="thr")

    new_thr_revision = _layer_pinned_revision(repo, "thr")
    assert new_thr_revision != "", "Expected thr layer to still have a pinned revision"
    assert new_thr_revision != initial_thr_revision, (
        f"Layer revision did not advance: {initial_thr_revision} == {new_thr_revision}"
    )

    # After scoped commit, no staged changes remain
    status_output = repo.status(json=True)
    status_entries = parse_status_json(status_output)
    assert len(status_entries) == 0, (
        f"Expected no staged entries after scoped commit, got: {status_entries}"
    )


@pytest.mark.smoke
def test_layer_scoped_commit_no_parent_change(new_lore_repo):
    """`--layer <path>` leaves the parent's staged state and other layers'
    staged state untouched while advancing the targeted layer's revision.
    """
    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    initial_thr_revision = _layer_pinned_revision(repo, "thr")
    initial_sec_revision = _layer_pinned_revision(repo, "sec")

    # Stage changes in both parent and the "thr" layer
    with repo.open_file("root_repo.txt", mode="wb") as out:
        out.write(os.urandom(1500))
    sec_file = os.path.join("sec", "second", "second_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(os.urandom(1500))
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(1500))

    repo.stage(".", scan=True)

    # Commit only the "thr" layer
    repo.commit("Just the thr layer", layer="thr")

    # The thr layer should have advanced
    new_thr_revision = _layer_pinned_revision(repo, "thr")
    assert new_thr_revision != initial_thr_revision, (
        f"thr revision did not advance: {initial_thr_revision} == {new_thr_revision}"
    )

    # The sec layer should NOT have advanced
    new_sec_revision = _layer_pinned_revision(repo, "sec")
    assert new_sec_revision == initial_sec_revision, (
        f"sec revision should not have advanced: {initial_sec_revision} -> {new_sec_revision}"
    )

    # The parent's own staged file change must still be staged
    status_output = repo.status(json=True)
    status_entries = parse_status_json(status_output)
    paths = sorted(e.get("path") for e in status_entries)
    # root_repo.txt and the sec layer file should still be staged
    expected = sorted(["root_repo.txt", "sec/second/second_repo.txt"])
    assert paths == expected, (
        f"Expected residual staged entries {expected}, got {paths}: {status_entries}"
    )


@pytest.mark.smoke
def test_layer_scoped_commit_not_a_layer(new_lore_repo):
    """`--layer <path>` for a path that isn't a configured layer produces a
    `NotALayer` error and the commit doesn't proceed.
    """
    from error_types import NotALayerError

    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    # Modify a file in the "thr" layer so there's something staged-able
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(1500))
    repo.stage(thr_file)

    # Attempt to commit with a bogus layer path — should error
    with pytest.raises(NotALayerError):
        repo.commit("Should fail", layer="not-a-real-layer")


@pytest.mark.smoke
def test_layer_scoped_commit_nothing_staged(new_lore_repo):
    """`--layer <path>` for a layer with no staged changes errors with
    `NothingStaged`.
    """
    from error_types import NothingStagedError

    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    # Don't stage anything — attempt scoped commit
    with pytest.raises(NothingStagedError):
        repo.commit("Should fail", layer="thr")


def _layer_revision_message(layer_repo: Lore) -> str:
    """Sync the layer repository and return the commit message of its latest revision."""
    layer_repo.sync()
    info = layer_repo.revision_info(check=True, no_pager=True)
    return info.message


@pytest.mark.smoke
def test_layer_commit_per_layer_message(new_lore_repo):
    """`lore commit "msg" --layer-message <path> "<layer-msg>"` applies the
    per-layer message to that layer's revision metadata while the parent (and
    other layers) get the main message.
    """
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    # Modify a file in the "thr" layer
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(2000))
    repo.stage(thr_file)

    repo.commit(
        "Main commit message",
        layer_messages={"thr": "Layer-specific thr message"},
        non_interactive=True,
    )
    repo.push()

    # Verify the thr layer's latest revision has the per-layer message
    thr_message = _layer_revision_message(third_repo)
    assert thr_message == "Layer-specific thr message", (
        f"Expected thr layer message 'Layer-specific thr message', got '{thr_message}'"
    )


@pytest.mark.smoke
def test_layer_commit_no_message_fallback(new_lore_repo):
    """Without `--layer-message`, the layer revision falls back to the main
    commit message.
    """
    repo, _, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(2000))
    repo.stage(thr_file)

    repo.commit("Shared main message", non_interactive=True)
    repo.push()

    thr_message = _layer_revision_message(third_repo)
    assert thr_message == "Shared main message", (
        f"Expected fallback to main message, got '{thr_message}'"
    )


@pytest.mark.smoke
def test_layer_commit_multiple_messages(new_lore_repo):
    """Multiple `--layer-message` flags in one commit apply distinct messages
    to different layers.
    """
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    # Modify a file in each layer
    sec_file = os.path.join("sec", "second", "second_repo.txt")
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(os.urandom(2000))
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(2000))
    repo.stage(".", scan=True)

    repo.commit(
        "Main",
        layer_messages={"sec": "sec-only message", "thr": "thr-only message"},
        non_interactive=True,
    )
    repo.push()

    sec_message = _layer_revision_message(second_repo)
    thr_message = _layer_revision_message(third_repo)
    assert sec_message == "sec-only message", f"sec got '{sec_message}'"
    assert thr_message == "thr-only message", f"thr got '{thr_message}'"


@pytest.mark.smoke
def test_layer_commit_partial_messages(new_lore_repo):
    """When only one of multiple staged layers has an explicit
    `--layer-message`, that layer uses the supplied message and the others
    fall back to the main commit message.
    """
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    sec_file = os.path.join("sec", "second", "second_repo.txt")
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(os.urandom(2000))
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(2000))
    repo.stage(".", scan=True)

    repo.commit(
        "Main message",
        layer_messages={"thr": "thr-only message"},
        non_interactive=True,
    )
    repo.push()

    sec_message = _layer_revision_message(second_repo)
    thr_message = _layer_revision_message(third_repo)
    assert sec_message == "Main message", f"sec should fall back, got '{sec_message}'"
    assert thr_message == "thr-only message", f"thr got '{thr_message}'"


@pytest.mark.smoke
def test_layer_commit_non_interactive(new_lore_repo):
    """`--non-interactive` suppresses prompting; layers without explicit
    `--layer-message` flags receive the main commit message.
    """
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    sec_file = os.path.join("sec", "second", "second_repo.txt")
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(os.urandom(2000))
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(2000))
    repo.stage(".", scan=True)

    # No layer_messages, --non-interactive — should not prompt and both layers
    # should receive the main message.
    repo.commit("Main only", non_interactive=True)
    repo.push()

    sec_message = _layer_revision_message(second_repo)
    thr_message = _layer_revision_message(third_repo)
    assert sec_message == "Main only", f"sec got '{sec_message}'"
    assert thr_message == "Main only", f"thr got '{thr_message}'"


@pytest.mark.smoke
def test_layer_commit_invalid_message_errors(new_lore_repo):
    """`--layer-message <path> <msg>` for a path that is not a configured
    layer produces an error; the commit does not proceed.
    """
    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(os.urandom(2000))
    repo.stage(thr_file)

    # Bogus layer path in --layer-message — must error before committing.
    with pytest.raises(Exception):
        repo.commit(
            "Should fail",
            layer_messages={"not-a-real-layer": "bogus"},
            non_interactive=True,
        )


@pytest.mark.smoke
def test_commit_no_layers_unchanged(new_lore_repo):
    """`lore commit "msg"` in a repo with no layers stages and commits parent
    file changes with the supplied message; no per-layer flags or metadata
    involved.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", mode="w+b") as out:
        out.write(b"initial content")
    repo.stage(scan=True)
    repo.commit("Initial commit")
    repo.push()

    # Modify and re-commit
    with repo.open_file("file.txt", mode="w+b") as out:
        out.write(b"updated content")
    repo.stage(scan=True)
    repo.commit("Update content")
    repo.push()

    # Verify the latest commit message via revision_info
    info = repo.revision_info(check=True, no_pager=True)
    assert info.message == "Update content", (
        f"Expected message 'Update content', got '{info.message}'"
    )


@pytest.mark.smoke
def test_status_unstaged_after_layer_add(new_lore_repo):
    """`lore status --unstaged` immediately after `lore layer add` reports no
    entries — the layer's files were just checked out from the layer repo at
    the configured pin and are unmodified, so they must not appear as "added"
    against the parent repository.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)

    status_output = repo.status(json=True, unstaged=True)
    status_entries = parse_status_json(status_output)

    assert status_entries == [], (
        "Expected `status --unstaged` to be empty immediately after "
        f"`layer add`, got: {status_entries}"
    )


@pytest.mark.smoke
def test_status_unstaged_layer_file_modified(new_lore_repo):
    """`lore status --unstaged` after modifying a file inside a layer mount
    reports the file as modified — diffed against the layer's pinned revision
    rather than treated as a parent-tree add or hidden by the layer mask.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)

    # Modify a file inside the layer mount
    layer_file = os.path.join("lay", "layer_file.txt")
    with repo.open_file(layer_file, mode="wb") as out:
        out.write(b"layer content modified")

    status_output = repo.status(json=True, unstaged=True)
    status_entries = parse_status_json(status_output)

    paths = [e.get("path") for e in status_entries]
    assert "lay/layer_file.txt" in paths, (
        f"Expected modified layer file in status --unstaged, got: {status_entries}"
    )

    # The entry should reflect a modification (not "add"), because the file
    # exists in the layer's state.
    layer_file_entry = next(
        e for e in status_entries if e.get("path") == "lay/layer_file.txt"
    )
    assert layer_file_entry.get("action") != "add", (
        f"Expected layer file to be reported as modified (not 'add'), got: "
        f"{layer_file_entry}"
    )


@pytest.mark.smoke
def test_status_unstaged_layer_file_added(new_lore_repo):
    """A new file created on disk inside a layer mount is reported by
    `status --unstaged` as "add" against the layer's tree, with the
    filesystem (parent-relative) path.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)

    new_file = os.path.join("lay", "added_inside_layer.txt")
    with repo.open_file(new_file, mode="wb") as out:
        out.write(b"new content inside layer mount")

    status_output = repo.status(json=True, unstaged=True)
    status_entries = parse_status_json(status_output)

    paths = [e.get("path") for e in status_entries]
    assert "lay/added_inside_layer.txt" in paths, (
        f"Expected new layer file in status --unstaged with the filesystem "
        f"path 'lay/added_inside_layer.txt', got: {status_entries}"
    )
    new_entry = next(
        e for e in status_entries if e.get("path") == "lay/added_inside_layer.txt"
    )
    assert new_entry.get("action") == "add", (
        f"Expected new file to be reported as 'add', got: {new_entry}"
    )


@pytest.mark.smoke
def test_status_unstaged_layer_file_deleted(new_lore_repo):
    """A file deleted from disk inside a layer mount is reported by
    `status --unstaged` as a deletion against the layer's tree, with the
    filesystem (parent-relative) path.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)

    layer_file = os.path.join("lay", "layer_file.txt")
    os.remove(os.path.join(repo.path, layer_file))

    status_output = repo.status(json=True, unstaged=True)
    status_entries = parse_status_json(status_output)

    paths = [e.get("path") for e in status_entries]
    assert "lay/layer_file.txt" in paths, (
        f"Expected deleted layer file in status --unstaged with filesystem "
        f"path 'lay/layer_file.txt', got: {status_entries}"
    )
    deleted_entry = next(
        e for e in status_entries if e.get("path") == "lay/layer_file.txt"
    )
    assert deleted_entry.get("action") == "delete", (
        f"Expected deleted layer file to be reported as 'delete', got: "
        f"{deleted_entry}"
    )


@pytest.mark.smoke
def test_status_unstaged_mixed_parent_and_layer(new_lore_repo):
    """`status --unstaged` reports BOTH parent and layer modifications in a
    single output. Each entry's path uses the filesystem (parent-relative)
    prefix, not the layer's internal `source_path` prefix — this matters for
    layers where `target_path != source_path` (e.g. `thr` mounted at
    `parent/thr/...` from the layer repo's `third/...` subtree).
    """
    repo, _, _ = _setup_repo_with_two_layers(new_lore_repo)

    # Modify a parent file
    with repo.open_file("root_repo.txt", mode="wb") as out:
        out.write(b"parent content modified")
    # Modify a file inside the asymmetric `thr` layer
    # (target_path = "thr", source_path = "third/" — internal path is
    # `third/third_repo.txt`, filesystem path is `thr/third_repo.txt`)
    thr_file = os.path.join("thr", "third_repo.txt")
    with repo.open_file(thr_file, mode="wb") as out:
        out.write(b"thr layer content modified")
    # Modify a file inside the `sec` layer (source_path = "/")
    sec_file = os.path.join("sec", "second", "second_repo.txt")
    with repo.open_file(sec_file, mode="wb") as out:
        out.write(b"sec layer content modified")

    status_output = repo.status(json=True, unstaged=True)
    status_entries = parse_status_json(status_output)

    paths = sorted(e.get("path") for e in status_entries)
    expected_paths = sorted(
        [
            "root_repo.txt",
            "thr/third_repo.txt",
            "sec/second/second_repo.txt",
        ]
    )
    assert paths == expected_paths, (
        f"Expected exactly {expected_paths} (filesystem paths, parent-relative), "
        f"got {paths}: {status_entries}"
    )

    # Specifically guard against the layer-internal path leaking into the
    # report — the layer repo's path for the thr file is `third/third_repo.txt`
    # and we must NOT see that.
    assert "third/third_repo.txt" not in paths, (
        f"Layer-internal source_path leaked into status output: {paths}"
    )

    # Each entry should be a modification, not an add.
    for entry in status_entries:
        assert entry.get("action") != "add", (
            f"Expected entry to be reported as modified (not 'add'), got: "
            f"{entry}"
        )


@pytest.mark.smoke
def test_layer_remove_without_repository(new_lore_repo):
    """`lore layer remove <path>` without a source repository argument finds
    the unique layer at that path and removes it.
    """
    repo, _layer_repo = _setup_repo_with_layer(new_lore_repo)

    output = repo.layer_remove("lay", json=True)
    event = parse_layer_remove_json(output)
    assert event is not None, f"Expected layerRemove event, got: {output}"
    assert event.get("targetPath") == "lay"
    assert event.get("fileCount") == 1

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert layers == []
    assert not os.path.exists(os.path.join(repo.path, "lay"))


@pytest.mark.smoke
def test_layer_remove_basic(new_lore_repo):
    """`lore layer remove` on a clean layer deletes the layer's tracked files
    and the now-empty mount directory, drops the entry from `layer list`, and
    emits a `layerRemove` event with accurate counts.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    output = repo.layer_remove("lay", layer_repo, json=True)
    event = parse_layer_remove_json(output)
    assert event is not None, f"Expected layerRemove event, got: {output}"
    assert event.get("targetPath") == "lay"
    assert event.get("fileCount") == 1
    assert event.get("modifiedCount") == 0
    assert event.get("forced") == 0
    assert event.get("purged") == 0

    complete = parse_complete_json(output)
    assert complete is not None and complete.get("status") == 0, (
        f"Expected successful complete event, got: {complete}"
    )

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert layers == [], f"Expected no layers after remove, got {layers}"
    assert not os.path.exists(os.path.join(repo.path, "lay")), (
        "Layer mount directory should be deleted when empty"
    )


@pytest.mark.smoke
def test_layer_remove_keeps_untracked_files(new_lore_repo):
    """A layer remove leaves untracked files behind. The tracked file is gone,
    the untracked file and its parent directory survive, and the layer is
    detached from the configuration.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    untracked = os.path.join(repo.path, "lay", "user_notes.txt")
    with open(untracked, "wb") as out:
        out.write(b"user-added content")

    output = repo.layer_remove("lay", layer_repo, json=True)
    event = parse_layer_remove_json(output)
    assert event is not None
    assert event.get("purged") == 0
    assert event.get("fileCount") == 1

    # Tracked file removed
    assert not os.path.exists(os.path.join(repo.path, "lay", "layer_file.txt"))
    # Untracked file preserved, keeping its parent directory alive
    assert os.path.isfile(untracked), (
        "Untracked file inside layer mount must remain after remove"
    )
    assert os.path.isdir(os.path.join(repo.path, "lay")), (
        "Layer mount directory must remain when it still contains untracked files"
    )

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert layers == [], "Layer entry should be removed from configuration"


@pytest.mark.smoke
def test_layer_remove_modified_file_errors(new_lore_repo):
    """A layer remove aborts with `LocalModificationsError` when a tracked
    file has been modified locally. The layer remains configured and the
    modification survives the failed call.
    """
    from error_types import LocalModificationsError

    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    layer_file = os.path.join("lay", "layer_file.txt")
    with repo.open_file(layer_file, mode="wb") as out:
        out.write(b"locally modified content")

    with pytest.raises(LocalModificationsError):
        repo.layer_remove("lay", layer_repo)

    # Modification preserved
    with repo.open_file(layer_file, mode="rb") as inp:
        assert inp.read() == b"locally modified content"

    # Layer still configured
    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert len(layers) == 1 and layers[0].get("targetPath") == "lay"

    # Same again via JSON also produces an error event and non-zero complete
    output = repo.layer_remove("lay", layer_repo, json=True, check=False)
    complete = parse_complete_json(output)
    assert complete is not None and complete.get("status") != 0, (
        f"Expected non-zero complete status, got: {output}"
    )
    message = (complete.get("error") or {}).get("message", "")
    assert "local modifications" in message.lower(), (
        f"Expected local modifications error in complete detail, got: {output}"
    )


@pytest.mark.smoke
def test_layer_remove_force_discards_modifications(new_lore_repo):
    """The global `--force` flag overrides the modification gate; tracked
    modified files are deleted and the layer is removed.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    layer_file = os.path.join("lay", "layer_file.txt")
    with repo.open_file(layer_file, mode="wb") as out:
        out.write(b"locally modified content")

    output = repo.layer_remove("lay", layer_repo, json=True, force=True)
    event = parse_layer_remove_json(output)
    assert event is not None
    assert event.get("forced") == 1
    assert event.get("modifiedCount") == 1
    assert event.get("fileCount") == 1

    assert not os.path.exists(os.path.join(repo.path, "lay", "layer_file.txt"))
    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert layers == []


@pytest.mark.smoke
def test_layer_remove_purge_clears_untracked(new_lore_repo):
    """`--purge` deletes the whole layer mount, including untracked files and
    nested directories.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    nested_dir = os.path.join(repo.path, "lay", "userdir")
    os.makedirs(nested_dir)
    nested_file = os.path.join(nested_dir, "note.txt")
    with open(nested_file, "wb") as out:
        out.write(b"untracked content")
    sibling_file = os.path.join(repo.path, "lay", "sibling.txt")
    with open(sibling_file, "wb") as out:
        out.write(b"more untracked content")

    output = repo.layer_remove("lay", layer_repo, purge=True, json=True)
    event = parse_layer_remove_json(output)
    assert event is not None
    assert event.get("purged") == 1

    assert not os.path.exists(os.path.join(repo.path, "lay")), (
        "Layer mount directory should be deleted under --purge"
    )
    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert layers == []


@pytest.mark.smoke
def test_layer_remove_purge_with_modifications_requires_force(new_lore_repo):
    """`--purge` does not by itself override the modification gate; combining
    it with `--force` allows the full nuke.
    """
    from error_types import LocalModificationsError

    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    layer_file = os.path.join("lay", "layer_file.txt")
    with repo.open_file(layer_file, mode="wb") as out:
        out.write(b"locally modified content")

    with pytest.raises(LocalModificationsError):
        repo.layer_remove("lay", layer_repo, purge=True)
    # File and layer entry should still be present
    assert os.path.isfile(os.path.join(repo.path, "lay", "layer_file.txt"))
    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert len(layers) == 1

    # Now with --force --purge it should succeed and wipe the tree.
    output = repo.layer_remove("lay", layer_repo, purge=True, json=True, force=True)
    event = parse_layer_remove_json(output)
    assert event is not None
    assert event.get("forced") == 1
    assert event.get("purged") == 1
    assert not os.path.exists(os.path.join(repo.path, "lay"))


@pytest.mark.smoke
def test_layer_remove_unknown_errors(new_lore_repo):
    """Removing a layer at a path that is not mounted as a layer returns an
    error and leaves existing layers untouched.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    output = repo.layer_remove("nope", layer_repo, json=True, check=False)
    complete = parse_complete_json(output)
    assert complete is not None and complete.get("status") != 0, (
        f"Expected non-zero status for unknown layer, got: {output}"
    )

    # Existing layer untouched
    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert len(layers) == 1 and layers[0].get("targetPath") == "lay"
    assert os.path.isfile(os.path.join(repo.path, "lay", "layer_file.txt"))


@pytest.mark.smoke
def test_layer_remove_two_layers_non_overlapping(new_lore_repo):
    """Removing one of two non-overlapping layers leaves the other layer's
    configuration, files, and directories intact.
    """
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    output = repo.layer_remove("thr", third_repo, json=True)
    event = parse_layer_remove_json(output)
    assert event is not None
    assert event.get("targetPath") == "thr"
    assert event.get("fileCount") == 1

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert len(layers) == 1, f"Expected only 'sec' to remain, got {layers}"
    assert layers[0].get("targetPath") == "sec"

    # The thr layer's mount is gone
    assert not os.path.exists(os.path.join(repo.path, "thr"))
    # The sec layer is untouched
    assert os.path.isfile(
        os.path.join(repo.path, "sec", "second", "second_repo.txt")
    )
