# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import re

import pytest

from error_types import InvalidRepositoryPath
from lore import Lore
from lore_parsers import parse_jsonl

logger = logging.getLogger(__name__)


def get_url(repository_info_output: str) -> str | None:
    match = re.search(".*Remote URL: (.*)", repository_info_output)
    if match is not None:
        return match.group(1)
    return None


@pytest.mark.smoke
def test_repository_info_url(new_lore_repo, tmp_path_factory, monkeypatch):
    no_repo_urc: Lore = new_lore_repo(create_repo=False)

    repo = new_lore_repo()

    monkeypatch.chdir(repo.path)

    assert get_url(no_repo_urc.repository_info(use_os_dir=True)) + "/" == repo.remote

    assert get_url(no_repo_urc.repository_info(path=repo.path)) + "/" == repo.remote
    with pytest.raises(InvalidRepositoryPath):
        assert no_repo_urc.repository_info()
    assert (
        get_url(no_repo_urc.repository_info(url=repo.remote_path)) + "/" == repo.remote
    )


@pytest.mark.smoke
def test_repository_create_description(new_lore_repo):
    """repository create --description stores the description and
    repository info returns it in the repositoryData event."""

    description_text = "Automated test repository for description flag"
    repo: Lore = new_lore_repo(create_repo=False)
    repo.repository_create(description=description_text)

    # Plain text output should contain the description
    output = repo.repository_info()
    assert description_text in output

    # JSON output should contain the description in repositoryData event
    json_output = repo.repository_info(json=True)
    events = parse_jsonl(json_output, "repositoryData")
    assert len(events) == 1
    assert events[0]["description"] == description_text


@pytest.mark.smoke
def test_repository_create_no_description(new_lore_repo):
    """repository create without --description should result in an empty
    description field in repositoryData."""

    repo: Lore = new_lore_repo()

    json_output = repo.repository_info(json=True)
    events = parse_jsonl(json_output, "repositoryData")
    assert len(events) == 1
    assert events[0]["description"] == ""
