# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import random

import pytest

from error_types import ImproperArgumentsError
from lore import Lore
from lore_parsers import parse_jsonl

logger = logging.getLogger(__name__)


def get_metadata_events(output: str) -> list[dict]:
    """Parse JSONL output and return metadata event data dicts."""
    return parse_jsonl(output, "metadata")


def get_metadata_dict(output: str) -> dict[str, dict]:
    """Parse JSONL metadata events into a key -> event dict."""
    events = get_metadata_events(output)
    return {e["key"]: e for e in events}


@pytest.mark.smoke
def test_branch_metadata_set_get_string(new_lore_repo):
    """Verify setting and getting a string metadata key on a branch."""
    repo: Lore = new_lore_repo()

    repo.branch_metadata_set(["team", "rendering"], branch="main")

    output = repo.branch_metadata_get("team", branch="main", json=True)
    events = get_metadata_events(output)

    assert len(events) == 1, f"Expected 1 metadata event, got {len(events)}"
    assert events[0]["key"] == "team"
    assert events[0]["value"]["tagName"] == "string"
    assert events[0]["value"]["data"] == "rendering"


@pytest.mark.smoke
def test_branch_metadata_set_multiple_keys(new_lore_repo):
    """Verify setting multiple key-value pairs on a branch in one call."""
    repo: Lore = new_lore_repo()

    repo.branch_metadata_set(["key1", "value1", "key2", "value2"], branch="main")

    output = repo.branch_metadata_get(branch="main", json=True)
    metadata = get_metadata_dict(output)

    assert metadata["key1"]["value"]["data"] == "value1"
    assert metadata["key2"]["value"]["data"] == "value2"


@pytest.mark.smoke
def test_branch_metadata_defaults_to_current_branch(new_lore_repo):
    """Omitting --branch operates on the current branch (here, main)."""
    repo: Lore = new_lore_repo()

    # Set and get with no --branch should target the current branch.
    repo.branch_metadata_set(["owner", "graphics"])

    output = repo.branch_metadata_get("owner", json=True)
    events = get_metadata_events(output)
    assert len(events) == 1, f"Expected 1 metadata event, got {len(events)}"
    assert events[0]["value"]["data"] == "graphics"

    # The value must be visible via the explicit current-branch name too,
    # proving the no-branch path resolved to main rather than a phantom branch.
    output_main = repo.branch_metadata_get("owner", branch="main", json=True)
    events_main = get_metadata_events(output_main)
    assert len(events_main) == 1
    assert events_main[0]["value"]["data"] == "graphics"


@pytest.mark.smoke
def test_branch_metadata_clear_defaults_to_current_branch(new_lore_repo):
    """Clearing with no --branch operates on the current branch."""
    repo: Lore = new_lore_repo()

    repo.branch_metadata_set(["temp", "value"], branch="main")
    repo.branch_metadata_clear(["temp"])

    output = repo.branch_metadata_get(branch="main", json=True)
    metadata = get_metadata_dict(output)
    assert "temp" not in metadata, (
        "Clear without --branch must affect the current branch"
    )


@pytest.mark.smoke
def test_branch_metadata_set_single_arg_rejected(new_lore_repo):
    """A lone argument has no value; the set must be rejected, not panic."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.branch_metadata_set(["lonely-key"])


@pytest.mark.smoke
def test_branch_metadata_set_odd_args_rejected(new_lore_repo):
    """An odd number of arguments leaves the trailing key without a value and
    must be rejected rather than dropping the key or panicking."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.branch_metadata_set(["key1", "value1", "key2"])


@pytest.mark.smoke
def test_branch_metadata_set_binary(new_lore_repo, scratch_dir):
    """--binary takes the value as the path of a file to read, one named relative to the
    repository here, and records the address its content was stored at."""
    repo: Lore = new_lore_repo()

    payload = random.Random(0).randbytes(4096)
    with repo.open_file("branch-payload.bin", "wb") as output_file:
        output_file.write(payload)

    repo.branch_metadata_set(
        ["build-artifact", "branch-payload.bin"], branch="main", binary=True
    )

    events = get_metadata_events(
        repo.branch_metadata_get("build-artifact", branch="main", json=True)
    )
    assert len(events) == 1, f"Expected 1 metadata event, got {len(events)}"
    assert events[0]["value"]["tagName"] == "address", (
        f"Binary metadata must record the address its payload was stored at.\n"
        f"Got: {events[0]['value']}"
    )

    restored = scratch_dir("restored-branch-payload", create=True) / "restored.bin"
    repo.file_write(address=events[0]["value"]["data"], output=str(restored))
    assert restored.read_bytes() == payload, (
        "The payload read back from the store must match the file the value named"
    )


@pytest.mark.smoke
def test_branch_metadata_set_binary_outside_repository(new_lore_repo, scratch_dir):
    """A --binary value may name a file the repository does not hold, which is read from the
    host filesystem rather than through the repository."""
    repo: Lore = new_lore_repo()

    payload = random.Random(1).randbytes(4096)
    outside = scratch_dir("outside-branch-payload", create=True) / "payload.bin"
    outside.write_bytes(payload)

    repo.branch_metadata_set(
        ["build-artifact", str(outside)], branch="main", binary=True
    )

    events = get_metadata_events(
        repo.branch_metadata_get("build-artifact", branch="main", json=True)
    )
    assert len(events) == 1, f"Expected 1 metadata event, got {len(events)}"

    restored = scratch_dir("restored-outside-payload", create=True) / "restored.bin"
    repo.file_write(address=events[0]["value"]["data"], output=str(restored))
    assert restored.read_bytes() == payload, (
        "The payload read back from the store must match the file outside the repository"
    )
