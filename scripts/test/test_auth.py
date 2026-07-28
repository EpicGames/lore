# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging

import pytest
from error_types import NotSupportedError
from lore_ffi import NOT_AUTHENTICATED, NOT_SUPPORTED

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_auth_login_not_supported_without_auth_endpoint(new_lore_repo):
    """The local test server is authless (no auth endpoint configured), so an
    interactive `auth login` against it must fail with `NotSupported` rather
    than an opaque internal error."""

    repo: Lore = new_lore_repo()

    with pytest.raises(NotSupportedError):
        repo.run(urc_args=["auth", "login", repo.remote_path, "--no-browser"])


@pytest.mark.smoke
def test_auth_info_not_supported_without_auth_endpoint(new_lore_repo):
    """`auth info` resolves its auth endpoint from the repository's remote. The
    authless test server advertises no auth endpoint, so there is no URL to key
    a token lookup on and the command must fail with `NotSupported`."""

    repo: Lore = new_lore_repo()

    with pytest.raises(NotSupportedError):
        repo.run(urc_args=["auth", "info"])


@pytest.mark.smoke
def test_auth_user_info_not_supported_without_auth_endpoint(
    new_lore_repo, lore_library
):
    """`authUserInfo` (remote user-info resolution) must fail with
    `NotSupported` against the authless test server, not `NotAuthenticated`:
    the real failure is that the server has no auth endpoint at all, and
    replacing it with `NotAuthenticated` sends consumers chasing login state
    that cannot exist.

    No CLI command surfaces this call's errors (the CLI only uses it to
    decorate output with display names and deliberately ignores failures), so
    the test calls the public C API — the surface the SDK's `authUserInfo`
    binding is built on — and asserts on the returned FFI code."""

    repo: Lore = new_lore_repo()

    result = lore_library.auth_user_info(repo.path, ["some-other-user"])

    assert result != 0, "resolving a user against an authless server must fail"
    assert result != NOT_AUTHENTICATED, (
        "the authless failure must not be masked as NotAuthenticated"
    )
    assert result == NOT_SUPPORTED, f"expected NotSupported (18), got FFI code {result}"
