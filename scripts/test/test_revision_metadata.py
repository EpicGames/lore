# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import random

import pytest
from error_types import ImproperArgumentsError
from lore_parsers import parse_jsonl

from lore import Lore

logger = logging.getLogger(__name__)

# Content size above which the store splits a payload into several fragments,
# mirroring lore_base::types::FRAGMENT_SIZE_THRESHOLD
FRAGMENT_SIZE_THRESHOLD = 256 * 1024

# The store query status for an address the store does not hold, mirroring
# LoreRepositoryStoreImmutableQueryEventData
STORE_QUERY_NOT_FOUND = 3


@pytest.mark.smoke
def test_revision_metadata_get_default_keys(new_lore_repo):
    """
    Verify that auto-set metadata keys are present after a commit.
    """
    repo: Lore = new_lore_repo()

    repo.write_commit_push("Initial commit", {"file.txt": "content\n"})

    metadata = repo.revision_metadata_get()
    # The "list all" output uses display labels: "Branch", "Date", and an
    # indented commit message.
    assert "Branch" in metadata, (
        f"Expected 'Branch' label in metadata output.\nGot:\n{metadata}"
    )
    assert "Date" in metadata, (
        f"Expected 'Date' label in metadata output.\nGot:\n{metadata}"
    )
    assert "Initial commit" in metadata, (
        f"Expected commit message in metadata output.\nGot:\n{metadata}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_specific_key(new_lore_repo):
    """
    Verify fetching individual metadata keys returns correct values.
    """
    repo: Lore = new_lore_repo()

    commit_message = "Specific key test commit"
    branch_name = "specific-key-branch"
    repo.write_commit_push("Initial commit", {"file.txt": "content\n"})
    repo.branch_create(branch_name)
    repo.write_commit_push(commit_message, {"file.txt": "content modification\n"})

    timestamp = repo.revision_metadata_get("timestamp")
    assert timestamp.strip(), "timestamp should be non-empty"

    message = repo.revision_metadata_get("message")
    assert commit_message in message, (
        f"Expected commit message in metadata.\nExpected: {commit_message}\nGot: {message}"
    )

    # Branch metadata stores the branch context ID, not the name
    branch_info = repo.branch_info(branch_name)
    expected_branch_id = branch_info.id
    branch = repo.revision_metadata_get("branch")
    assert expected_branch_id in branch.strip(), (
        f"Expected branch ID '{expected_branch_id}' in branch metadata.\nGot: {branch}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_by_hash(new_lore_repo):
    """
    Exercise --revision with a hash signature to fetch metadata from
    specific revisions.
    """
    repo: Lore = new_lore_repo()

    # First commit
    first_message = "First commit message"
    repo.write_commit_push(first_message, {"file.txt": "first\n"})

    # Second commit
    second_message = "Second commit message"
    repo.write_commit_push(second_message, {"file.txt": "second\n"})

    revisions = repo.history()
    assert len(revisions) >= 2, f"Expected at least 2 revisions, got {len(revisions)}"

    # history() returns oldest-first, so [0] is first commit, [-1] is newest
    first_metadata = repo.revision_metadata_get(
        "message", revision=revisions[0].signature
    )
    assert first_message in first_metadata, (
        f"Expected first commit message via hash lookup.\n"
        f"Expected: {first_message}\nGot: {first_metadata}"
    )

    second_metadata = repo.revision_metadata_get(
        "message", revision=revisions[1].signature
    )
    assert second_message in second_metadata, (
        f"Expected second commit message via hash lookup.\n"
        f"Expected: {second_message}\nGot: {second_metadata}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_by_branch_at_revision(new_lore_repo):
    """
    Exercise --revision with <branch>@<number> notation.
    """
    repo: Lore = new_lore_repo()

    # First commit
    first_message = "Branch-at first"
    repo.write_commit_push(first_message, {"file.txt": "first\n"})

    # Second commit
    second_message = "Branch-at second"
    repo.write_commit_push(second_message, {"file.txt": "second\n"})

    first_metadata = repo.revision_metadata_get("message", revision="main@1")
    assert first_message in first_metadata, (
        f"Expected first commit message via main@1.\n"
        f"Expected: {first_message}\nGot: {first_metadata}"
    )

    second_metadata = repo.revision_metadata_get("message", revision="main@2")
    assert second_message in second_metadata, (
        f"Expected second commit message via main@2.\n"
        f"Expected: {second_message}\nGot: {second_metadata}"
    )


@pytest.mark.smoke
def test_revision_metadata_set_and_get(new_lore_repo):
    """
    Verify user-set metadata roundtrips through commit and push.
    """
    repo: Lore = new_lore_repo()

    with repo.open_file("file.txt", "w") as f:
        f.write("content\n")
    repo.stage("file.txt")
    repo.revision_metadata_set(["reviewed-by", "tester@example.com"])
    repo.commit("Commit with custom metadata")
    repo.push()

    reviewed_by = repo.revision_metadata_get("reviewed-by")
    assert "tester@example.com" in reviewed_by, (
        f"Expected 'tester@example.com' in reviewed-by metadata.\nGot: {reviewed_by}"
    )


@pytest.mark.smoke
def test_revision_metadata_get_across_branches(new_lore_repo):
    """
    Verify --revision with branch notation works across different branches.
    """
    repo: Lore = new_lore_repo()

    # Initial commit on main
    repo.write_commit_push("Initial commit", {"main.txt": "main content\n"})

    # Create feature branch with its own commit
    feature_message = "Feature branch commit"
    repo.branch_create("feature")
    repo.write_commit_push(feature_message, {"feature.txt": "feature content\n"})

    # Switch back to main and make another commit
    repo.branch_switch("main")
    main_second_message = "Main second commit"
    repo.write_commit_push(main_second_message, {"main.txt": "updated main content\n"})

    # Verify feature branch metadata via branch@LATEST
    feature_metadata = repo.revision_metadata_get("message", revision="feature@LATEST")
    assert feature_message in feature_metadata, (
        f"Expected feature commit message via feature@LATEST.\n"
        f"Expected: {feature_message}\nGot: {feature_metadata}"
    )

    # Verify main@1 (no key) returns the initial (non-latest) commit
    main_first_metadata = repo.revision_metadata_get(revision="main@1")
    assert "Initial commit" in main_first_metadata, (
        f"Expected initial commit message via main@1.\n"
        f"Expected: 'Initial commit'\nGot: {main_first_metadata}"
    )
    assert main_second_message not in main_first_metadata, (
        f"main@1 should not contain the second commit message.\nGot: {main_first_metadata}"
    )

    # Verify main branch metadata via main@LATEST
    main_metadata = repo.revision_metadata_get("message", revision="main@LATEST")
    assert main_second_message in main_metadata, (
        f"Expected main second commit message via main@LATEST.\n"
        f"Expected: {main_second_message}\nGot: {main_metadata}"
    )


def _metadata_value(output: str) -> str:
    """The value from a single-key `revision metadata get`, which prints the
    key's display label ahead of it."""
    _, _, value = output.partition(":")
    return value.strip()


def _commit_feature_with_metadata(repo: Lore) -> None:
    """Branch `feature` off main carrying planted provenance on its tip.

    `merged-by` is reserved and `status-checks` is a key lore does not know, so
    between them they cover both halves of what an inherit list governs.
    """
    repo.write_commit_push("Initial commit", {"main.txt": "main\n"})
    repo.branch_create("feature")
    with repo.open_file("feature.txt", "w") as f:
        f.write("feature\n")
    repo.stage("feature.txt")
    repo.revision_metadata_set(
        [
            "reviewed-by",
            "source.reviewer@example.com",
            "merged-by",
            "source.merger@example.com",
            "status-checks",
            "source-checks-payload",
        ]
    )
    repo.commit("Feature work")
    repo.push()


@pytest.mark.smoke
def test_revision_metadata_not_inherited_by_merge_into(new_lore_repo):
    """`branch merge into` commits and pushes to the target branch in one call,
    so an unnamed key must not reach the revision it creates there."""
    repo: Lore = new_lore_repo()

    _commit_feature_with_metadata(repo)
    repo.branch_merge_into("main", "Merge feature into main")

    merged = repo.revision_metadata_get(revision="main@LATEST")
    assert "Merge feature into main" in merged, (
        f"Expected the merge message on main@LATEST.\nGot:\n{merged}"
    )
    for value in (
        "source.reviewer@example.com",
        "source.merger@example.com",
        "source-checks-payload",
    ):
        assert value not in merged, (
            f"'{value}' must not be carried onto the merge revision.\nGot:\n{merged}"
        )


@pytest.mark.smoke
def test_revision_metadata_inherited_by_merge_when_named(new_lore_repo):
    """--inherit-metadata carries the keys it names and no others."""
    repo: Lore = new_lore_repo()

    _commit_feature_with_metadata(repo)
    repo.branch_switch("main")
    repo.branch_merge(
        "feature",
        inherit_metadata=["reviewed-by"],
        message="Merge feature into main",
    )

    reviewed_by = repo.revision_metadata_get("reviewed-by")
    assert "source.reviewer@example.com" in reviewed_by, (
        f"A named key must reach the merge revision.\nGot: {reviewed_by}"
    )

    merged = repo.revision_metadata_get()
    assert "source-checks-payload" not in merged, (
        f"An unnamed key must not be carried.\nGot:\n{merged}"
    )


@pytest.mark.smoke
def test_revision_metadata_inherit_all_excludes_the_merger(new_lore_repo):
    """The `*` sentinel carries keys lore does not know, but `merged-by` is
    reserved: the merge revision names whoever ran the merge, which for a
    client-side merge is the same actor that committed it."""
    repo: Lore = new_lore_repo()

    _commit_feature_with_metadata(repo)
    repo.branch_switch("main")
    repo.branch_merge(
        "feature", inherit_metadata=["*"], message="Merge feature into main"
    )

    status_checks = repo.revision_metadata_get("status-checks")
    assert "source-checks-payload" in status_checks, (
        f"The sentinel must carry an unknown key.\nGot: {status_checks}"
    )

    merged_by = _metadata_value(repo.revision_metadata_get("merged-by"))
    committed_by = _metadata_value(repo.revision_metadata_get("committed-by"))
    assert "source.merger@example.com" not in merged_by, (
        f"The sentinel must not carry the source revision's merger.\nGot: {merged_by}"
    )
    assert merged_by and merged_by == committed_by, (
        f"A client-side merge records its operator as merger.\n"
        f"merged-by: {merged_by}\ncommitted-by: {committed_by}"
    )


@pytest.mark.smoke
def test_revision_metadata_set_single_arg_rejected(new_lore_repo):
    """A lone argument has no value; the set must be rejected, not panic."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.revision_metadata_set(["lonely-key"])


@pytest.mark.smoke
def test_revision_metadata_set_odd_args_rejected(new_lore_repo):
    """An odd number of arguments leaves the trailing key without a value and
    must be rejected rather than dropping the key or panicking."""
    repo: Lore = new_lore_repo()

    with pytest.raises(ImproperArgumentsError):
        repo.revision_metadata_set(["key1", "value1", "key2"])


@pytest.mark.smoke
def test_revision_metadata_set_binary(new_lore_repo):
    """--binary reads file from disk, stores in immutable store, saves hash-address in metadata (set.rs:115-153).
    Source file can be deleted after set — data lives in store, not on disk."""
    repo: Lore = new_lore_repo()

    with repo.open_file("content.txt", "w") as f:
        f.write("some content\n")
    repo.stage("content.txt")

    # Create a binary payload file and set it as metadata
    payload = b"\x00\x01\x02\xff binary payload data \xfe\xfd"
    with repo.open_file("metadata_payload.bin", "wb") as f:
        f.write(payload)

    repo.revision_metadata_set(["build-artifact", "metadata_payload.bin"], binary=True)

    # Delete source file — data is already in immutable store
    repo.remove_file("metadata_payload.bin")
    assert not repo.file_exists("metadata_payload.bin")

    # Commit and push must succeed without the source file
    repo.commit("Commit with binary metadata")
    repo.push()

    # Verify the key appears in metadata listing
    all_metadata = repo.revision_metadata_get()
    assert "build-artifact" in all_metadata.lower(), (
        f"Expected 'build-artifact' key in metadata output.\nGot:\n{all_metadata}"
    )

    # Verify fetching the specific key returns an address (hash)
    value = repo.revision_metadata_get("build-artifact")
    assert value.strip(), (
        "Expected non-empty value for binary metadata key 'build-artifact'"
    )


def _metadata_address(output: str) -> str:
    """The address an Address-typed value records, from one `metadata` event."""
    events = parse_jsonl(output, "metadata")
    assert len(events) == 1, f"Expected one metadata event.\nGot: {events}"
    value = events[0]["value"]
    assert value["tagName"] == "address", (
        f"Binary metadata must record the address its payload was stored at.\n"
        f"Got: {value}"
    )
    return value["data"]


def _store_query(repo: Lore, address: str, *, remote: bool) -> dict:
    """The store query result for `address`, which reports the local store and
    the peer as an event each."""
    events = parse_jsonl(
        repo.repository_store_immutable_query(address, json=True),
        "repositoryStoreImmutableQuery",
    )
    half = [event for event in events if event["remote"] == remote]
    assert len(half) == 1, (
        f"Expected one {'remote' if remote else 'local'} result for {address}.\n"
        f"Got: {events}"
    )
    return half[0]


@pytest.mark.smoke
def test_revision_metadata_binary_payload_is_pushed(new_lore_repo, scratch_dir):
    """A binary metadata value keeps its payload in a fragment of its own, so
    push has to carry that fragment along with the revision.

    The payload is larger than FRAGMENT_SIZE_THRESHOLD, so the store splits it
    and push has to carry the fragments it is split into as well as the one
    naming them. Verified from a clone, whose store starts empty: whatever it
    resolves came off the server.
    """
    repo: Lore = new_lore_repo()

    payload = random.Random(0).randbytes(2 * FRAGMENT_SIZE_THRESHOLD)
    with repo.open_file("content.txt", "w") as f:
        f.write("content\n")
    repo.stage("content.txt")

    with repo.open_file("payload.bin", "wb") as f:
        f.write(payload)
    repo.revision_metadata_set(["build-artifact", "payload.bin"], binary=True)
    # The payload lives in the store now, so the clone cannot be reading the file
    repo.remove_file("payload.bin")

    repo.commit("Commit with binary metadata")
    repo.push()

    address = _metadata_address(repo.revision_metadata_get("build-artifact", json=True))

    clone = repo.clone()

    local = _store_query(clone, address, remote=False)
    assert local["status"] == STORE_QUERY_NOT_FOUND, (
        f"The clone must not already hold {address} locally, or the read below "
        f"would not prove the push carried it.\nGot: {local}"
    )
    remote = _store_query(clone, address, remote=True)
    assert remote["status"] != STORE_QUERY_NOT_FOUND, (
        f"Push must have uploaded the binary metadata payload {address}.\nGot: {remote}"
    )

    restored = scratch_dir("restored-payload", create=True) / "restored.bin"
    clone.file_write(address=address, output=str(restored))
    assert restored.read_bytes() == payload, (
        "The payload read back from the server must match what was stored"
    )
