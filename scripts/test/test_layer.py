# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import pytest
import re
import tomllib
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

MAIN_FILE = "main_file.txt"
LAYER_FILE = os.path.join("lay", "layer_file.txt")
LAYER_STAGED_FILE = os.path.join("lay", "staged_new.txt")


def _setup_repo_with_layer(new_lore_repo):
    """Create a main repo with a layer repo and initial content in both.

    Uses matching target_path and source_path ("lay" -> "lay/") so that
    layer::sync works (target_path != source_path is not yet implemented).
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    repo.write_commit_push(None, {MAIN_FILE: b"main content"})

    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {LAYER_FILE: b"layer content v1"})

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
def test_layer_branch_archive_leaves_layers_by_default(new_lore_repo):
    """`lore branch archive` touches only the repository it ran in.

    A layer is a separate repository owning its own branch lifecycle, so an
    archive must not reach into it without being asked.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    assert layer_repo.branch_list().has_remote_branch("feature"), (
        "Expected branch create to cascade into the layer repository"
    )

    repo.branch_switch("main")
    repo.branch_archive("feature")

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert layer_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the layer branch to be left alone, got: {layer_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_include_layers(new_lore_repo):
    """`--include-layers` archives the branch in the layer repository too, so
    the layer is left with exactly the branches it had before the create.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    branches_before = sorted(layer_repo.branch_list().remote_branches)

    repo.branch_create("feature")
    repo.push()
    assert layer_repo.branch_list().has_remote_branch("feature"), (
        "Expected branch create to cascade into the layer repository"
    )

    repo.branch_switch("main")
    repo.branch_archive("feature", include_layers=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )
    assert sorted(layer_repo.branch_list().remote_branches) == branches_before, (
        f"Expected layer branches {branches_before}, got: {layer_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_multiple_layers(new_lore_repo):
    """`--include-layers` archives the branch in every configured layer."""
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    for layer in (second_repo, third_repo):
        assert layer.branch_list().has_remote_branch("feature"), (
            f"Expected branch create to cascade into {layer.name}"
        )

    repo.branch_switch("main")
    repo.branch_archive("feature", include_layers=True)

    for layer in (second_repo, third_repo):
        assert sorted(layer.branch_list().remote_branches) == ["main"], (
            f"Expected only 'main' remaining in {layer.name}, got: {layer.branch_list()}"
        )


@pytest.mark.smoke
def test_layer_branch_archive_single_layer(new_lore_repo):
    """`--layer <path>` archives the branch in that layer and no other."""
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    repo.branch_archive("feature", layer="sec")

    assert not second_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the scoped layer to be archived, got: {second_repo.branch_list()}"
    )
    assert third_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the other layer to be left alone, got: {third_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_unknown_layer_errors(new_lore_repo):
    """`--layer` naming a path that is not a layer is an error, not a silent no-op."""
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", layer="not-a-layer", check=False)

    assert "not a layer" in output.lower(), (
        f"Expected an unknown layer path to be reported, got: {output}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_layer_flags_conflict(new_lore_repo):
    """`--include-layers` and `--layer` are mutually exclusive."""
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    output = repo.branch_archive(
        "feature", include_layers=True, layer="lay", check=False
    )

    assert "cannot be used with" in output.lower(), (
        f"Expected clap to reject the flag combination, got: {output}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_local_keeps_layer_remote(new_lore_repo):
    """`--local --include-layers` archives the layer's local cache only,
    leaving the layer's remote branch in place.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    repo.branch_create("feature")
    repo.push()

    repo.branch_switch("main")
    repo.branch_archive("feature", local=True, include_layers=True)

    assert not repo.branch_list().has_local_branch("feature"), (
        f"Expected the local branch to be archived, got: {repo.branch_list()}"
    )
    assert layer_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the layer remote branch to remain, got: {layer_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_skips_layer_without_branch(new_lore_repo):
    """A layer that never had the branch is skipped quietly.

    Creating the branch before the layer is configured is the one flow that
    leaves a layer with no metadata for it, so the layer answers NOT_FOUND
    rather than the idempotent already-archived success.
    """
    repo: Lore = new_lore_repo()
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")

    repo.write_commit_push(None, {MAIN_FILE: b"main content"})
    layer_repo.make_dirs("lay")
    layer_repo.write_commit_push(None, {LAYER_FILE: b"layer content v1"})

    # No layer configured yet, so the branch is never cascaded anywhere.
    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    repo.layer_add("lay", layer_repo, "lay/")
    assert not layer_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the layer to never have seen the branch, got: {layer_repo.branch_list()}"
    )

    output = repo.branch_archive("feature", include_layers=True)

    assert "not found" not in output.lower(), (
        f"Expected the missing layer branch to be skipped quietly, got: {output}"
    )
    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_tolerates_already_archived_layer(new_lore_repo):
    """Archiving a branch a layer already archived is not an error."""
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    layer_repo.branch_switch("main")
    layer_repo.branch_archive("feature")

    repo.branch_archive("feature", include_layers=True)

    assert sorted(repo.branch_list().remote_branches) == ["main"], (
        f"Expected only 'main' remaining in the parent, got: {repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_reports_once(new_lore_repo):
    """Archiving reports a single branch, not one line per layer."""
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    output = repo.branch_archive("feature", include_layers=True)

    assert output.count("Archived branch") == 1, (
        f"Expected one archive line for the outer repository, got: {output}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_continues_past_refusing_layer(new_lore_repo):
    """One layer refusing the archive does not stop the remaining layers."""
    repo, second_repo, third_repo = _setup_repo_with_two_layers(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    second_repo.branch_protect("feature")

    repo.branch_archive("feature", include_layers=True, check=False)

    assert second_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the protected layer branch to survive, got: {second_repo.branch_list()}"
    )
    assert not third_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the remaining layer to still be archived, got: {third_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_current_leaves_layers(new_lore_repo):
    """Refusing to archive the current branch leaves the layers alone."""
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    repo.branch_create("feature")
    repo.push()

    repo.branch_archive("feature", include_layers=True, check=False)

    assert layer_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the layer branch to be untouched, got: {layer_repo.branch_list()}"
    )


@pytest.mark.smoke
def test_layer_branch_archive_converges_after_partial_archive(new_lore_repo):
    """A repeat archive still reaches the layers after a partial one.

    `--local` leaves every remote behind, so the second run finds the outer
    local branch already gone. That must not abort the cascade, or the layer
    remotes stay orphaned with no way to clean them up.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    repo.branch_archive("feature", local=True, include_layers=True)
    assert layer_repo.branch_list().has_remote_branch("feature")

    repo.branch_archive("feature", include_layers=True, check=False)

    assert not layer_repo.branch_list().has_remote_branch("feature"), (
        f"Expected the repeat archive to reach the layer, got: {layer_repo.branch_list()}"
    )


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


def _setup_layer_behind(new_lore_repo, advance_layer: bool):
    """Set up a repo whose working copy is behind the parent's branch latest, so
    a plain `lore sync` moves it forward.

    A layer-only commit creates no parent revision, so a second working copy
    pushes one. The layer has no metadata link, meaning it always targets the
    layer repository's branch latest, so `advance_layer` decides whether the
    sync moves the layer's pinned revision.

    Returns (repo, layer_repo).
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    other = repo.clone(name=repo.name + "_other")
    other.write_commit_push(None, {MAIN_FILE: b"main content v2"})

    if advance_layer:
        layer_repo.write_commit_push(None, {LAYER_FILE: b"layer content v2"})

    with repo.open_file(LAYER_FILE, "rb") as f:
        content = f.read()
    assert content == b"layer content v1", (
        f"setup: expected layer still at v1, got: {content}"
    )

    return repo, layer_repo


def _stage_layer_change(repo: Lore) -> None:
    """Stage a new file inside the layer.

    Deliberately a different file from the one the incoming sync carries: a
    staged edit to that file is stopped by the local-modifications check during
    realize, which masks whether the staged-layer gate fired at all.
    """
    repo.write_files({LAYER_STAGED_FILE: b"layer staged addition"})
    repo.stage(LAYER_STAGED_FILE)

    status_entries = parse_status_json(repo.status(json=True))
    paths = [e.get("path") for e in status_entries if e.get("flagStaged")]
    assert paths == ["lay/staged_new.txt"], (
        f"setup: expected the new layer file staged, got {paths}: {status_entries}"
    )


@pytest.mark.smoke
def test_layer_sync_refused_with_staged_layer_content(new_lore_repo):
    """A sync that would advance a layer holding staged content is refused,
    naming the offending layer.
    """
    from error_types import LoreException

    repo, _ = _setup_layer_behind(new_lore_repo, advance_layer=True)
    pinned_before = _layer_pinned_revision(repo, "lay")

    _stage_layer_change(repo)

    with pytest.raises(LoreException) as excinfo:
        repo.sync()
    assert "Unable to sync when layer lay has a staged state" in str(excinfo.value), (
        f"sync should refuse and name the layer, got:\n{excinfo.value}"
    )

    assert _layer_pinned_revision(repo, "lay") == pinned_before, (
        "refused sync must not advance the layer's pinned revision"
    )
    status_entries = parse_status_json(repo.status(json=True))
    paths = [e.get("path") for e in status_entries if e.get("flagStaged")]
    assert paths == ["lay/staged_new.txt"], (
        f"staged layer content should survive the refused sync, got {paths}"
    )
    with repo.open_file(LAYER_FILE, "rb") as f:
        content = f.read()
    assert content == b"layer content v1", (
        f"refused sync must not realize the layer change, got: {content}"
    )


@pytest.mark.smoke
def test_layer_sync_leaves_unmoved_layer_staged_state(new_lore_repo):
    """A sync that does not move a layer's pinned revision keeps that layer's
    staged content, rather than refusing the sync or clearing the pin.
    """
    repo, _ = _setup_layer_behind(new_lore_repo, advance_layer=False)
    pinned_before = _layer_pinned_revision(repo, "lay")

    _stage_layer_change(repo)

    repo.sync()

    with repo.open_file(MAIN_FILE, "rb") as f:
        content = f.read()
    assert content == b"main content v2", (
        f"expected the parent to have synced forward, got: {content}"
    )
    assert _layer_pinned_revision(repo, "lay") == pinned_before, (
        "layer with no matching change should keep its pinned revision"
    )

    status_entries = parse_status_json(repo.status(json=True))
    paths = [e.get("path") for e in status_entries if e.get("flagStaged")]
    assert paths == ["lay/staged_new.txt"], (
        f"staged layer content should survive an unrelated sync, got {paths}"
    )

    repo.commit("Commit the layer edit after an unrelated sync")
    assert _layer_pinned_revision(repo, "lay") != pinned_before, (
        "committing the staged layer content should advance the layer pin"
    )


@pytest.mark.smoke
def test_layer_sync_force_clears_stale_staged_pin(new_lore_repo):
    """`--force` sync discards the layer's staged state instead of leaving a pin
    parented on the pre-sync revision.
    """
    repo, _ = _setup_layer_behind(new_lore_repo, advance_layer=True)
    pinned_before = _layer_pinned_revision(repo, "lay")

    _stage_layer_change(repo)

    repo.sync(force=True)

    pinned_after = _layer_pinned_revision(repo, "lay")
    assert pinned_after != pinned_before, (
        "forced sync should advance the layer's pinned revision"
    )
    with repo.open_file(LAYER_FILE, "rb") as f:
        content = f.read()
    assert content == b"layer content v2", (
        f"forced sync should realize the synced layer content, got: {content}"
    )

    status_entries = parse_status_json(repo.status(json=True))
    paths = [e.get("path") for e in status_entries if e.get("flagStaged")]
    assert paths == [], (
        f"forced sync should leave no staged layer content, got {paths}: {status_entries}"
    )


ZERO_HASH = "0" * 64


def _layer_config_path(repo: Lore) -> str:
    """Return the path of the repository's `layer.toml`."""
    return os.path.join(repo.dot_path(), "layer.toml")


def _layer_config_staged(repo: Lore, target_path: str) -> str:
    """Return the `staged` pin of the layer at `target_path` from `layer.toml`.

    `lore layer list` only reports the `current` pin. Returns "" when the
    config has no entry for `target_path`.
    """
    with open(_layer_config_path(repo), "rb") as config_file:
        config = tomllib.load(config_file)
    for layer in config.get("layers", []):
        if layer.get("target_path") == target_path:
            return layer.get("staged", "")
    return ""


@pytest.mark.smoke
def test_layer_stage_scan_unchanged_layer(new_lore_repo):
    """`stage . --scan` must not stage a layer whose files are all unchanged.

    A bogus staged pin on the layer makes `commit` abort with `NothingStaged`
    after the parent has committed, and the leftover pin then makes
    `branch switch` refuse and skip syncing the layer to the target branch.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)
    layer_file = os.path.join("lay", "layer_file.txt")

    # `branch create` switches to the new branch, so go back to main to commit
    # the layer change on a revision `feature` does not have.
    repo.branch_create("feature")
    repo.push()
    repo.branch_switch("main")

    with repo.open_file(layer_file, mode="wb") as out:
        out.write(b"layer content v2")
    repo.stage(".", scan=True)
    repo.commit("linked_tag")
    repo.push()

    repo.branch_switch("feature")
    with repo.open_file(layer_file, mode="rb") as out:
        assert out.read() == b"layer content v1", (
            "Expected the layer to roll back to L1 on the feature branch"
        )

    # Change only a parent file, nothing inside the layer mount.
    with repo.open_file("main_file.txt", mode="wb") as out:
        out.write(b"main content v2")

    repo.stage(".", scan=True)

    status_entries = parse_status_json(repo.status(json=True))
    paths = sorted(entry.get("path") for entry in status_entries)
    assert paths == ["main_file.txt"], (
        f"Expected only ['main_file.txt'] staged, got {paths}: {status_entries}"
    )

    assert _layer_config_staged(repo, "lay") in ("", ZERO_HASH), (
        "`stage --scan` wrote a staged pin for a layer with no modified files"
    )

    repo.commit("Test commit 2")

    assert _layer_config_staged(repo, "lay") in ("", ZERO_HASH), (
        "Layer staged pin left non-zero in layer.toml after commit"
    )

    repo.branch_switch("main")

    with repo.open_file(layer_file, mode="rb") as out:
        content = out.read()
    assert content == b"layer content v2", (
        f"Expected the layer to be restored to L2 on main, got: {content}"
    )


@pytest.mark.smoke
def test_layer_remove_refused_with_staged_layer_content(new_lore_repo):
    """`layer remove` is refused without `--force` when the layer holds staged
    changes, and the staged work survives the refusal.

    Staged changes have already been reconciled with disk, so the local
    modification gate does not see them.
    """
    from error_types import LocalModificationsError

    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)
    _stage_layer_change(repo)

    with pytest.raises(LocalModificationsError):
        repo.layer_remove("lay", layer_repo)

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert [layer.get("targetPath") for layer in layers] == ["lay"], (
        f"The refused remove dropped the layer from the config, got {layers}"
    )

    status_entries = parse_status_json(repo.status(json=True))
    paths = [entry.get("path") for entry in status_entries if entry.get("flagStaged")]
    assert paths == ["lay/staged_new.txt"], (
        f"The staged layer content should survive the refused remove, got {paths}"
    )
    assert os.path.isfile(os.path.join(repo.path, LAYER_STAGED_FILE)), (
        "The staged file should still be on disk after the refused remove"
    )


@pytest.mark.smoke
def test_layer_remove_force_cleans_staged_add(new_lore_repo):
    """`layer remove --force` counts and deletes a staged add.

    A staged add is absent from the layer's `current` revision, so it has to be
    taken from the staged state to be cleaned up.
    """
    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)
    _stage_layer_change(repo)

    remove_output = repo.layer_remove("lay", layer_repo, force=True, json=True)
    remove_event = parse_layer_remove_json(remove_output)
    assert remove_event is not None, (
        f"Expected a layerRemove event, got: {remove_output}"
    )
    assert remove_event.get("fileCount") == 2, (
        f"Expected the staged add to be counted alongside the tracked file, got {remove_event}"
    )
    assert remove_event.get("forced") == 1, (
        f"Expected the remove to report that force was required, got {remove_event}"
    )
    assert remove_event.get("modifiedCount") == 0, (
        f"A cleanly staged add is not a local modification, got {remove_event}"
    )

    assert not os.path.exists(os.path.join(repo.path, LAYER_STAGED_FILE)), (
        "The staged add was left on disk as untracked debris"
    )
    assert not os.path.exists(os.path.join(repo.path, "lay")), (
        "The layer mount directory should be gone once its files are removed"
    )

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert layers == [], f"Expected no layers configured after remove, got {layers}"


@pytest.mark.smoke
def test_layer_remove_staged_delete_is_not_a_modification(new_lore_repo):
    """A staged delete gates the remove by count, but the file it removed from
    disk is not reported as a locally modified file.

    Staging the delete is what unlinked the file, so its absence is the expected
    state and must not be mistaken for the user having deleted it behind Lore's
    back.
    """
    from error_types import LocalModificationsError

    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)
    os.remove(os.path.join(repo.path, LAYER_FILE))
    repo.stage(scan=True)

    with pytest.raises(LocalModificationsError):
        repo.layer_remove("lay", layer_repo)

    remove_event = parse_layer_remove_json(
        repo.layer_remove("lay", layer_repo, force=True, json=True)
    )
    assert remove_event.get("modifiedCount") == 0, (
        f"A staged delete is not a local modification, got {remove_event}"
    )
    assert remove_event.get("fileCount") == 0, (
        f"A staged-deleted file is already off disk, got {remove_event}"
    )
    assert not os.path.exists(os.path.join(repo.path, "lay")), (
        "The layer mount directory should be gone once its files are removed"
    )


@pytest.mark.smoke
def test_layer_remove_staged_modify_edited_again_is_gated(new_lore_repo):
    """A file staged and then edited again on disk is still gated, by the staged
    count rather than by the modified list.

    A staged node carries no content address the edit can be measured against -
    a staged modify keeps the pre-stage hash - so the modification check is
    skipped and `modifiedCount` stays 0. The staged gate is what refuses here,
    which is why it cannot be folded into the local-modification check.
    """
    from error_types import LocalModificationsError

    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)

    repo.write_files({LAYER_FILE: b"layer content staged"})
    repo.stage(LAYER_FILE)
    repo.write_files({LAYER_FILE: b"layer content edited after staging"})

    with pytest.raises(LocalModificationsError):
        repo.layer_remove("lay", layer_repo)

    remove_event = parse_layer_remove_json(
        repo.layer_remove("lay", layer_repo, force=True, json=True)
    )
    assert remove_event.get("forced") == 1, (
        f"Expected the remove to report that force was required, got {remove_event}"
    )
    assert remove_event.get("modifiedCount") == 0, (
        f"A staged node has no comparable content address, got {remove_event}"
    )
    assert not os.path.exists(os.path.join(repo.path, "lay")), (
        "The layer mount directory should be gone once its files are removed"
    )


@pytest.mark.smoke
def test_layer_remove_refused_reports_staged_and_modified(new_lore_repo):
    """A layer that is both staged and modified names both reasons in one
    refusal, so a single `--force` covers everything that would be lost.
    """
    from error_types import LocalModificationsError

    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)
    _stage_layer_change(repo)
    repo.write_files({LAYER_FILE: b"layer content modified on disk"})

    with pytest.raises(LocalModificationsError) as excinfo:
        repo.layer_remove("lay", layer_repo)

    output = str(excinfo.value)
    assert "1 staged file(s)" in output, (
        f"The refusal should name the staged count, got:\n{output}"
    )
    assert "locally modified files" in output, (
        f"The refusal should also name the modified files, got:\n{output}"
    )


@pytest.mark.smoke
def test_layer_remove_purge_refused_with_staged_layer_content(new_lore_repo):
    """`--purge` deletes the whole mount including untracked content, so it is
    gated on staged work exactly like a plain remove rather than bypassing it.
    """
    from error_types import LocalModificationsError

    repo, layer_repo = _setup_repo_with_layer(new_lore_repo)
    _stage_layer_change(repo)

    with pytest.raises(LocalModificationsError):
        repo.layer_remove("lay", layer_repo, purge=True)

    assert os.path.isfile(os.path.join(repo.path, LAYER_STAGED_FILE)), (
        "The refused purge should leave the staged file on disk"
    )

    repo.layer_remove("lay", layer_repo, purge=True, force=True)
    assert not os.path.exists(os.path.join(repo.path, "lay")), (
        "A forced purge should remove the whole layer mount"
    )


@pytest.mark.smoke
def test_layer_config_corrupt_config_surfaces_error(new_lore_repo):
    """A `layer.toml` that cannot be parsed reports an error and is preserved.

    Reading a corrupt config as an empty layer set would hand back a repository
    that appears to have no layers, and the next save would write that empty
    set back over the only record of the layer set.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)
    config_path = _layer_config_path(repo)
    with open(config_path, "rb") as config_file:
        original = config_file.read()

    with open(config_path, "wb") as config_file:
        config_file.write(b"layers = = =")

    output = repo.layer_list(json=True, check=False)
    complete = parse_complete_json(output)
    assert complete is not None and complete.get("status") != 0, (
        f"Expected a non-zero status for a corrupt layer config, got: {output}"
    )

    with open(config_path, "rb") as config_file:
        assert config_file.read() == b"layers = = =", (
            "A failed load overwrote the corrupt config instead of preserving it"
        )

    with open(config_path, "wb") as config_file:
        config_file.write(original)

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert len(layers) == 1 and layers[0].get("targetPath") == "lay", (
        f"Expected the layer to be listed again once the config is restored, got {layers}"
    )


@pytest.mark.smoke
def test_layer_config_unreadable_config_surfaces_error(new_lore_repo):
    """A `layer.toml` that cannot be opened reports an error.

    Only an absent config means "no layers configured". A config that is
    present but unreadable must not read as an empty layer set, since
    `layer.toml` is the sole record of the layer set and the next save would
    write the empty set back.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)
    config_path = _layer_config_path(repo)
    with open(config_path, "rb") as config_file:
        original = config_file.read()

    os.remove(config_path)
    os.mkdir(config_path)

    output = repo.layer_list(json=True, check=False)
    complete = parse_complete_json(output)
    assert complete is not None and complete.get("status") != 0, (
        f"Expected a non-zero status for an unreadable layer config, got: {output}"
    )

    os.rmdir(config_path)
    with open(config_path, "wb") as config_file:
        config_file.write(original)

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert len(layers) == 1 and layers[0].get("targetPath") == "lay", (
        f"Expected the layer to be listed again once the config is readable, got {layers}"
    )


@pytest.mark.smoke
def test_layer_config_add_refuses_on_corrupt_config(new_lore_repo):
    """`layer add` on a corrupt `layer.toml` reports an error and keeps the file.

    Treating the corrupt config as empty would make this add write a config
    holding only the new layer, permanently dropping the configured one.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    second_repo.make_dirs("sec")
    second_repo.write_commit_push(None, {os.path.join("sec", "second.txt"): b"second"})

    config_path = _layer_config_path(repo)
    with open(config_path, "rb") as config_file:
        original = config_file.read()

    corrupt = b"\xff\xfe\x00\x80"
    with open(config_path, "wb") as config_file:
        config_file.write(corrupt)

    output = repo.layer_add("sec", second_repo, "sec/", json=True, check=False)
    complete = parse_complete_json(output)
    assert complete is not None and complete.get("status") != 0, (
        f"Expected a non-zero status for add on a corrupt layer config, got: {output}"
    )

    with open(config_path, "rb") as config_file:
        assert config_file.read() == corrupt, (
            "A refused add rewrote the layer config, discarding the configured layer"
        )

    with open(config_path, "wb") as config_file:
        config_file.write(original)

    layers = parse_layer_list_json(repo.layer_list(json=True))
    assert [layer.get("targetPath") for layer in layers] == ["lay"], (
        f"Expected the original layer to survive the refused add, got {layers}"
    )
    assert os.path.isfile(os.path.join(repo.path, LAYER_FILE)), (
        "Expected the original layer's content to be intact after the refused add"
    )


@pytest.mark.smoke
def test_layer_config_save_leaves_no_temporary_file(new_lore_repo):
    """A saved `layer.toml` is complete and leaves no temporary file behind."""
    repo, _ = _setup_repo_with_layer(new_lore_repo)
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    second_repo.make_dirs("sec")
    second_repo.write_commit_push(None, {os.path.join("sec", "second.txt"): b"second"})

    repo.layer_add("sec", second_repo, "sec/")

    config_path = _layer_config_path(repo)
    with open(config_path, "rb") as config_file:
        config = tomllib.load(config_file)
    assert sorted(layer["target_path"] for layer in config["layers"]) == ["lay", "sec"], (
        f"Expected both layers in the saved config, got {config}"
    )

    assert not os.path.exists(config_path + ".tmp"), (
        "A successful save left its temporary file behind"
    )


@pytest.mark.smoke
def test_layer_config_save_replaces_stale_temporary_file(new_lore_repo):
    """A leftover temporary file from an interrupted save does not disturb the
    next one: the saved config holds the new layer set and the temporary file is
    consumed by the rename that installs it.
    """
    repo, _ = _setup_repo_with_layer(new_lore_repo)
    second_repo: Lore = new_lore_repo(repo.name + "_second")
    second_repo.make_dirs("sec")
    second_repo.write_commit_push(None, {os.path.join("sec", "second.txt"): b"second"})

    config_path = _layer_config_path(repo)
    temp_path = config_path + ".tmp"
    with open(temp_path, "wb") as temp_file:
        temp_file.write(b"layers = = =")

    repo.layer_add("sec", second_repo, "sec/")

    with open(config_path, "rb") as config_file:
        config = tomllib.load(config_file)
    assert sorted(layer["target_path"] for layer in config["layers"]) == ["lay", "sec"], (
        f"Expected both layers in the saved config, got {config}"
    )

    assert not os.path.exists(temp_path), (
        "The stale temporary file survived the save that should have consumed it"
    )


@pytest.mark.smoke
def test_layer_source_path_inside_link_is_rejected(new_lore_repo):
    """A layer source path that belongs to a linked repository is refused.

    Three repositories: `target` wants a layer from `middle`, but the path given
    belongs to `inner`, which `middle` links in. The guard compares the
    repository owning the resolved source node against the layer repository.
    """
    inner: Lore = new_lore_repo()
    inner.write_commit_push(
        "Initial inner", {"inner_data/inner.txt": "inner content\n"}
    )

    middle: Lore = new_lore_repo()
    middle.write_commit_push("Initial middle", {"middle.txt": "middle content\n"})
    middle.link_add("linked", inner.get_id(), "inner_data")
    middle.commit("Middle links inner")
    middle.push()

    target: Lore = new_lore_repo()
    target.write_commit_push("Initial target", {"target.txt": "target content\n"})

    # The path has to reach inside the mount. Resolving "linked" alone returns
    # middle's own link node, so the repository check passes and the later
    # "must be a directory" guard fires instead. "linked/inner.txt" resolves
    # through the link into inner, which is the case under test.
    output = target.layer_add(
        "linked/inner.txt", middle, "linked/inner.txt", check=False
    )

    assert "linked repository" in output, (
        f"Layer source inside a link should be rejected, got: {output}"
    )
    assert middle.get_id() not in target.layer_list(), (
        "No layer should be added when the source path belongs to a linked repository"
    )
