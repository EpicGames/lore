# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging

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
    assert "temp" not in metadata, "Clear without --branch must affect the current branch"


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
