# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

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
def test_repository_metadata_get_builtin(new_lore_repo):
    """Verify that built-in metadata keys are returned when listing all metadata."""
    repo: Lore = new_lore_repo()

    output = repo.repository_metadata_get(json=True)
    metadata = get_metadata_dict(output)

    assert "name" in metadata, f"Expected 'name' key in metadata.\nGot keys: {list(metadata.keys())}"
    assert "description" in metadata
    assert "default-branch" in metadata
    assert "default-branch-name" in metadata
    assert "creator" in metadata
    assert "created" in metadata

    assert metadata["default-branch-name"]["value"]["data"] == "main"


@pytest.mark.smoke
def test_repository_metadata_set_get_string(new_lore_repo):
    """Verify setting and getting a string metadata key."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["engine-version", "5.6"])

    output = repo.repository_metadata_get("engine-version", json=True)
    events = get_metadata_events(output)

    assert len(events) == 1, f"Expected 1 metadata event, got {len(events)}"
    assert events[0]["key"] == "engine-version"
    assert events[0]["value"]["tagName"] == "string"
    assert events[0]["value"]["data"] == "5.6"


@pytest.mark.smoke
def test_repository_metadata_set_get_numeric(new_lore_repo):
    """Verify setting and getting a numeric metadata key."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["build-number", "42"], numeric=True)

    output = repo.repository_metadata_get("build-number", json=True)
    events = get_metadata_events(output)

    assert len(events) == 1, f"Expected 1 metadata event, got {len(events)}"
    assert events[0]["key"] == "build-number"
    assert events[0]["value"]["tagName"] == "numeric"
    assert events[0]["value"]["data"] == 42


@pytest.mark.smoke
def test_repository_metadata_set_multiple_keys(new_lore_repo):
    """Verify setting multiple key-value pairs in one call."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["key1", "value1", "key2", "value2"])

    output = repo.repository_metadata_get(json=True)
    metadata = get_metadata_dict(output)

    assert "key1" in metadata
    assert metadata["key1"]["value"]["data"] == "value1"
    assert "key2" in metadata
    assert metadata["key2"]["value"]["data"] == "value2"


@pytest.mark.smoke
def test_repository_metadata_set_overwrite(new_lore_repo):
    """Verify that setting an existing key overwrites its value."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["mykey", "first"])
    repo.repository_metadata_set(["mykey", "second"])

    output = repo.repository_metadata_get("mykey", json=True)
    events = get_metadata_events(output)

    assert len(events) == 1
    assert events[0]["value"]["data"] == "second"


@pytest.mark.smoke
def test_repository_metadata_set_description(new_lore_repo):
    """Verify that the description built-in key can be overwritten."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["description", "Updated description"])

    output = repo.repository_metadata_get("description", json=True)
    events = get_metadata_events(output)

    assert len(events) == 1
    assert events[0]["value"]["data"] == "Updated description"


@pytest.mark.smoke
def test_repository_metadata_set_rejects_readonly(new_lore_repo):
    """Verify that setting a read-only built-in key is rejected."""
    repo: Lore = new_lore_repo()

    with pytest.raises(Exception):
        repo.repository_metadata_set(["name", "new-name"])


@pytest.mark.smoke
def test_repository_metadata_clear_specific(new_lore_repo):
    """Verify clearing a specific user-defined key."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["keep-me", "yes", "remove-me", "bye"])
    repo.repository_metadata_clear(["remove-me"])

    output = repo.repository_metadata_get(json=True)
    metadata = get_metadata_dict(output)

    assert "keep-me" in metadata
    assert "remove-me" not in metadata


@pytest.mark.smoke
def test_repository_metadata_clear_all(new_lore_repo):
    """Verify clearing all user-defined keys preserves built-in keys."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["user-key1", "val1", "user-key2", "val2"])
    repo.repository_metadata_clear()

    output = repo.repository_metadata_get(json=True)
    metadata = get_metadata_dict(output)

    assert "user-key1" not in metadata
    assert "user-key2" not in metadata
    assert "name" in metadata, "Built-in key 'name' should be preserved"
    assert "created" in metadata, "Built-in key 'created' should be preserved"


@pytest.mark.smoke
def test_repository_metadata_clear_rejects_builtin(new_lore_repo):
    """Verify that clearing a built-in key is rejected."""
    repo: Lore = new_lore_repo()

    with pytest.raises(Exception):
        repo.repository_metadata_clear(["name"])


@pytest.mark.smoke
def test_repository_metadata_get_nonexistent_key(new_lore_repo):
    """Verify that getting a nonexistent key returns no metadata events."""
    repo: Lore = new_lore_repo()

    output = repo.repository_metadata_get("nonexistent-key", json=True)
    events = get_metadata_events(output)

    assert len(events) == 0


@pytest.mark.smoke
def test_repository_metadata_set_binary(new_lore_repo):
    """Verify setting a binary metadata value from a file."""
    repo: Lore = new_lore_repo()

    binary_content = os.urandom(256)
    binary_path = os.path.join(repo.path, "binary-data.bin")
    with open(binary_path, "wb") as f:
        f.write(binary_content)

    repo.repository_metadata_set(["binary-key", binary_path], binary=True)

    output = repo.repository_metadata_get("binary-key", json=True)
    events = get_metadata_events(output)

    assert len(events) == 1
    assert events[0]["key"] == "binary-key"
    # Binary values are stored as Address references
    assert events[0]["value"]["tagName"] == "address"


@pytest.mark.smoke
def test_repository_metadata_get_all_includes_user_and_builtin(new_lore_repo):
    """Verify that listing all metadata returns both built-in and user-defined keys."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["custom-tag", "my-value"])

    output = repo.repository_metadata_get(json=True)
    metadata = get_metadata_dict(output)

    assert "name" in metadata, "Built-in key should be present"
    assert "custom-tag" in metadata, "User-defined key should be present"
    assert metadata["custom-tag"]["value"]["data"] == "my-value"


@pytest.mark.smoke
def test_repository_metadata_set_single_arg_rejected(new_lore_repo):
    """A lone argument has no value; the set must be rejected, not panic.

    Reproduces the original bug report: `metadata set mjansson=testing` was
    parsed as a single key with no value and panicked on an out-of-bounds index.
    """
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.repository_metadata_set(["mjansson=testing"])


@pytest.mark.smoke
def test_repository_metadata_set_odd_args_rejected(new_lore_repo):
    """An odd number of arguments leaves the trailing key without a value and
    must be rejected rather than dropping the key or panicking."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.repository_metadata_set(["key1", "value1", "key2"])


@pytest.mark.smoke
def test_repository_metadata_set_odd_args_leaves_metadata_unchanged(new_lore_repo):
    """A rejected odd-args set must not partially apply any of its keys."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.repository_metadata_set(["applied", "yes", "dangling"])

    output = repo.repository_metadata_get(json=True)
    metadata = get_metadata_dict(output)
    assert "applied" not in metadata, "Rejected set must not partially apply keys"
    assert "dangling" not in metadata


@pytest.mark.smoke
def test_repository_metadata_underscore_key(new_lore_repo):
    """Verify that keys starting with underscore are allowed."""
    repo: Lore = new_lore_repo()

    repo.repository_metadata_set(["_internal", "allowed"])

    output = repo.repository_metadata_get("_internal", json=True)
    events = get_metadata_events(output)

    assert len(events) == 1
    assert events[0]["value"]["data"] == "allowed"
