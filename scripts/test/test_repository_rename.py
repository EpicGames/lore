# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
from pathlib import Path

import pytest
from lore_ffi import NOT_SUPPORTED
from service_util import LORE_SERVICE_ENVIRONMENT, SERVICE_UNAVAILABLE

from lore import Lore


@pytest.mark.smoke
class TestRepositoryRenameCApi:
    """`lore_repository_rename` reaches the remote, which answers
    `UNIMPLEMENTED` while Lore Server has no rename handler: the call reports
    error code 9 (not supported)."""

    def test_rename_through_the_c_api_reaches_the_remote(
        self, new_lore_repo, lore_library_path
    ):
        repo: Lore = new_lore_repo()

        for repository_url in ["", repo.remote_path, repo.name, repo.get_id()]:
            assert (
                repo.repository_rename_capi(
                    lore_library_path, "renamed", repository_url
                )
                == NOT_SUPPORTED
            ), repository_url

    def test_rename_through_the_c_api_on_the_service(
        self, new_lore_repo, background_lore_service, lore_library_path
    ):
        """Relayed to a running service, the call reaches the remote too."""
        repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())

        assert (
            repo.repository_rename_capi(lore_library_path, "renamed") == NOT_SUPPORTED
        )

    def test_rename_through_the_c_api_is_relayed_to_the_service(
        self,
        new_lore_repo,
        stops_background_services,
        global_dir_name,
        lore_library_path,
    ):
        """With the service required and none to start, the call fails as
        unavailable rather than running in the caller's process."""
        repo: Lore = new_lore_repo()
        repo.environment_vars.update(
            LORE_SERVICE_ENVIRONMENT,
            LORE_SERVICE_EXECUTABLE=str(Path(global_dir_name) / "no-such-lore-binary"),
        )

        assert (
            repo.repository_rename_capi(lore_library_path, "renamed")
            == SERVICE_UNAVAILABLE
        )
