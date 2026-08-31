#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
from __future__ import annotations

import json
import logging
import re
import typing
from dataclasses import dataclass, field
from dataclasses import field as dc_field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from lore import GlobalOptions

logger = logging.getLogger(__name__)


@dataclass
class BranchList:
    current_branch: str | None
    local_branches: list[str]
    remote_branches: list[str]
    archived_branches: list[str] = dc_field(default_factory=list)

    def has_local_branch(self, name: str) -> bool:
        return name in self.local_branches

    def has_remote_branch(self, name: str) -> bool:
        return name in self.remote_branches

    def has_branch(self, name: str) -> bool:
        """Check if a branch exists locally or remotely."""
        return self.has_local_branch(name) or self.has_remote_branch(name)

    def has_archived_branch(self, name: str) -> bool:
        return name in self.archived_branches


@dataclass
class BranchDescription:
    id: str
    name: str
    local_latest: str
    remote_latest: str
    parent: str
    branch_point: str
    category: str
    creator: str
    created: str


@dataclass
class RevisionInfo:
    revision: str = ""
    signature: str = ""
    parent: str = ""
    branch: str = ""
    merge: str = ""
    creator: str = ""
    committer: str = ""
    reviewer: str = ""
    merger: str = ""
    date: str = ""
    message: str = ""
    changelist: str = ""
    restored: str = ""
    cherry_picked_from: str = ""
    reverted_from: str = ""
    change_request: str = ""
    fast_forward_merge: str = ""
    merged_by: str = ""

    def has_valid_signature(self):
        return len(self.signature) == 64


@dataclass
class BisectPath:
    start: int
    end: int


@dataclass
class BisectResults:
    is_done: bool
    current_revision: int
    left: BisectPath | None = None
    right: BisectPath | None = None


@dataclass
class FileDescription:
    path: str = ""
    type: str = ""
    size: str = ""
    mode: str = ""
    context: str = ""
    hash: str = ""
    local_size: str = ""
    local_hash: str = ""
    filtered_size: str = ""
    status: str = ""


@dataclass
class LockAcquire:
    acquired: list[str] = dc_field(default_factory=list)
    already_owned: list[str] = dc_field(default_factory=list)


@dataclass
class LockRelease:
    released: list[str] = dc_field(default_factory=list)


@dataclass
class LockQuery:
    file: str = ""
    owner: str = ""
    branch: str = ""


@dataclass
class LockStatus:
    file: str = ""
    owner: str = ""
    date: str = ""
    invalid_path: bool = False


@dataclass
class SpecificSharedStoreInfo:
    path: str = ""
    exists: bool = False

    def __eq__(self, other) -> bool:
        return self.path == other.path and self.exists == other.exists


@dataclass
class SharedStoreInfo:
    is_automatic: bool = False
    stores: typing.Dict[str, SpecificSharedStoreInfo] = field(default_factory=dict)


BISECT_START_END_PATTERN = re.compile(
    r"lore revision bisect --start @(?P<start>\d+) --end @(?P<end>\d+)"
)
BISECT_SYNCHRONIZED_PATTERN = re.compile(r"Synchronized to @(?P<rev>\d+)")
BISECT_DONE_PATTERN = re.compile(r"Bisect complete")


def can_parse_output(options: GlobalOptions) -> bool:
    if ("debug" in options and options["debug"]) or (
        "json" in options and options["json"]
    ):
        return False
    return True


def parse_jsonl(output: str, tag_name: str) -> list[dict]:
    """
    Parse JSONL output and return data dicts for events matching tag_name.

    The JSON output format is line-by-line JSON objects (JSONL) with format:
    {"tagName":"<eventName>","data":{...}}

    Returns a list of the 'data' dictionaries from matching events.
    """
    entries = []
    for line in output.strip().split("\n"):
        line = line.strip()
        if not line:
            continue
        try:
            parsed = json.loads(line)
            if parsed.get("tagName") == tag_name and "data" in parsed:
                entries.append(parsed["data"])
        except json.JSONDecodeError:
            # Skip non-JSON lines (headers, etc.)
            continue
    return entries


def parse_status_json(status_output: str) -> list[dict]:
    """Parse JSON status output into a list of status entry dictionaries."""
    return parse_jsonl(status_output, "repositoryStatusFile")


def parse_status_count_json(status_output: str) -> dict | None:
    """Parse `lore status --count --json` output and return the single
    repositoryStatusCount event data (with `directories` and `files` keys),
    or None if no count event was emitted."""
    entries = parse_jsonl(status_output, "repositoryStatusCount")
    return entries[-1] if entries else None


def parse_status_summary_json(status_output: str) -> dict | None:
    """Parse `lore status --scan/--check-dirty --json` output and return the
    single repositoryStatusSummary event data (with `adds`, `deletes`,
    `modifies`, `moves`, `copies` keys), or None if no summary was emitted."""
    entries = parse_jsonl(status_output, "repositoryStatusSummary")
    return entries[-1] if entries else None


def parse_commit_stats_json(output: str) -> dict | None:
    """Parse `lore commit --json` output and return the revisionCommitStats event
    data, or None if none was emitted.

    A commit emits this event once, when it has drained every background write,
    and only at statistics level one and above.
    """
    entries = parse_jsonl(output, "revisionCommitStats")
    return entries[-1] if entries else None


def parse_push_stats_json(output: str) -> dict | None:
    """Parse `lore push --json` output and return the branchPushStats event data,
    or None if none was emitted. Emitted once, as `parse_commit_stats_json`
    describes."""
    entries = parse_jsonl(output, "branchPushStats")
    return entries[-1] if entries else None


def parse_layer_list_json(output: str) -> list[dict]:
    """Parse `lore layer list --json` output into a list of layer entry dicts."""
    return parse_jsonl(output, "layerEntry")


def parse_layer_remove_json(output: str) -> dict | None:
    """Parse `lore layer remove --json` output and return the single layerRemove
    event data, or None if no event was emitted."""
    entries = parse_jsonl(output, "layerRemove")
    return entries[0] if entries else None


def parse_complete_json(output: str) -> dict | None:
    """Parse `--json` output and return the terminal complete event."""
    entries = parse_jsonl(output, "complete")
    return entries[-1] if entries else None


def parse_error_json(output: str) -> list[dict]:
    """Parse `--json` output and return any error events."""
    return parse_jsonl(output, "error")


def parse_lock_acquire(output: str):
    lines = output.strip().splitlines()
    is_parsing_already_owned = False
    results = LockAcquire()
    for line in lines:
        if "Lock acquired on" in line:
            is_parsing_already_owned = False
            continue
        if "Lock already owned on" in line:
            is_parsing_already_owned = True
            continue
        if is_parsing_already_owned:
            results.already_owned.append(line.strip())
        else:
            results.acquired.append(line.strip())
    return results


def parse_lock_release(output: str):
    if "Lock does not exist" in output:
        return LockRelease()
    lines = output.strip().splitlines()
    results = LockRelease()
    for line in lines:
        if "Lock released on" in line:
            continue
        results.released.append(line.strip())
    return results


def parse_lock_query(output: str) -> list[LockQuery]:
    lines = output.strip().splitlines()
    results = []
    for line in lines:
        if "Locks found" in line:
            continue
        splits = line.split(" by ")
        (file, rest) = (splits[0], splits[1])
        splits = rest.split(" on branch ")
        (owner, branch) = (splits[0], splits[1])
        result = LockQuery(file, owner, branch)
        results.append(result)
    return results


def parse_lock_status(output: str) -> list[LockStatus]:
    lines = output.strip().splitlines()
    results = []
    for line in lines:
        if "Files locked for edit" in line:
            continue
        if "Ignoring invalid path" in line:
            file = line.split("Ignoring invalid path: ")[0]
            result = LockStatus(file, invalid_path=True)
            results.append(result)
            continue
        splits = line.split(" by ")
        (file, rest) = (splits[0], splits[1])
        splits = rest.split(" on ")
        (owner, date) = (splits[0], splits[1])
        result = LockStatus(file, owner, date)
        results.append(result)
    return results


def parse_branch_list(output: str):
    # Split off archived section first
    archived_split = output.strip().split("Archived local branches:")
    main_output = archived_split[0]
    archived_string = archived_split[1] if len(archived_split) > 1 else ""

    # If there's a remote section after archived, split it out
    if "Remote branches:" in archived_string:
        archived_part, remote_after_archived = archived_string.split("Remote branches:", 1)
    else:
        archived_part = archived_string
        remote_after_archived = ""

    output_split = main_output.split("Remote branches:")
    local_branches = []
    current_branch = None
    for line in output_split[0].replace("Local branches:", "").strip().splitlines():
        line = line.strip()
        if not line:
            continue
        if line.startswith("*"):
            current_branch = line.lstrip("*").strip()
            local_branches.append(current_branch)
        else:
            local_branches.append(line)

    remote_string = ""
    if len(output_split) > 1:
        remote_string = output_split[1].strip()
    if remote_after_archived:
        if remote_string:
            remote_string += "\n"
        remote_string += remote_after_archived.strip()

    remote_branches = [
        branch.lstrip("*").strip()
        for branch in remote_string.splitlines()
        if branch.strip()
    ]

    archived_branches = [
        branch.strip()
        for branch in archived_part.strip().splitlines()
        if branch.strip() and not branch.strip().startswith("No ")
    ]

    return BranchList(current_branch, local_branches, remote_branches, archived_branches)


def parse_branch_list_json(output: str) -> BranchList:
    """Parse JSON branch list output into a BranchList."""
    entries = parse_jsonl(output, "branchListEntry")
    current_branch = None
    local_branches = []
    remote_branches = []
    for entry in entries:
        name = entry.get("name", "")
        location = entry.get("location", "local")
        if location == "local":
            local_branches.append(name)
            if entry.get("isCurrent"):
                current_branch = name
        elif location == "remote":
            remote_branches.append(name)
    return BranchList(current_branch, local_branches, remote_branches)


def parse_branch_info(output: str):
    header_to_field_map = {
        "ID": "id",
        "Latest": "local_latest",
        "Remote Latest": "remote_latest",
        "Parent": "parent",
        "Branch point": "branch_point",
        "Category": "category",
        "Creator": "creator",
        "Created": "created",
    }
    result_values = {key: "" for key in header_to_field_map.values()}
    result_values["name"] = ""
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("Branch ") and ":" not in stripped:
            result_values["name"] = stripped[len("Branch ") :]
            continue
        if ":" in line:
            header, data = line.split(":", 1)
            field = header_to_field_map.get(header.strip())
            if field:
                result_values[field] = data.strip()
    return BranchDescription(**result_values)


def parse_revision_list(revision_output: str, oneline: bool) -> list[RevisionInfo]:
    revision_output = revision_output.replace("\\n", "\n")
    revisions = []
    if oneline:
        revision_strings = revision_output.split("\n")
        for rev in revision_strings:
            split_rev = rev.split(maxsplit=1)
            if len(split_rev) == 0:
                continue
            rev_info = RevisionInfo()
            rev_info.revision = split_rev[0].strip()
            if len(split_rev) > 1:
                rev_info.message = split_rev[1]
            revisions.append(rev_info)
    else:
        revision_strings = [
            b.strip() for b in re.split(r"(?=Revision)", revision_output) if b.strip()
        ]
        for rev in revision_strings:
            record = {}
            message_lines = []
            splitlines = rev.splitlines()
            for line in splitlines:
                s = line.strip()
                if not s:
                    continue
                if ":" in s:
                    split = s.split(":")
                    key = split[0].strip().lower().replace("-", "_")
                    record[key] = split[1].strip()
                else:
                    message_lines.append(s)
            if message_lines:
                record["message"] = "\n".join(message_lines)
            revisions.append(RevisionInfo(**record))
    revisions.reverse()
    return revisions


def parse_revision_bisect(output: str):
    is_done = len(BISECT_DONE_PATTERN.findall(output)) > 0
    if is_done:
        next_commands = []
    else:
        next_commands = [
            BisectPath(int(match.group("start")), int(match.group("end")))
            for match in BISECT_START_END_PATTERN.finditer(output)
        ]
    current_revision = int(BISECT_SYNCHRONIZED_PATTERN.findall(output)[0])
    result = BisectResults(is_done, current_revision)
    if len(next_commands) == 2:
        result.left = next_commands[0]
        result.right = next_commands[1]
    return result


def parse_file_info(output: str):
    results: list[FileDescription] = []
    for section in re.split(r"(?=Path)", output):
        if section == "":
            continue
        result = {}
        for line in section.splitlines():
            split_line = line.split(":", 1)
            if len(split_line) < 2:
                logger.critical(f'file_info: Issue finding header in line "{line}"')
                continue
            (header, value) = (split_line[0].strip().lower(), split_line[1].strip())
            header = "_".join(header.split())
            result[header] = value
        results.append(FileDescription(**result))
    return results


def get_prefix(line: str) -> typing.Tuple[str, str] | None:
    (prefix, postfix) = line.strip().split(":", 1)
    return prefix.strip(), postfix.strip()


def parse_shared_store_info(output: str) -> SharedStoreInfo:
    info = SharedStoreInfo()
    latest_url = None
    lines = list(output.splitlines())
    info.is_automatic = "true" in lines.pop(0)
    for line in lines:
        prefix_result = get_prefix(line)
        if prefix_result is None:
            continue
        (prefix, value) = prefix_result
        if prefix == "Remote URL":
            latest_url = value
            info.stores[latest_url] = SpecificSharedStoreInfo()
        elif prefix == "Path":
            info.stores[latest_url].path = value
        elif prefix == "Exists":
            info.stores[latest_url].exists = value == "true"
    return info
