#!/usr/bin/python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
from __future__ import annotations

import filecmp
import logging
import os
import pathlib
import random
import shutil
import string
import subprocess
import typing
import uuid
from collections.abc import Iterable
from pathlib import Path
from time import sleep
from typing import TypedDict, Unpack, overload

from error_types import (
    ServerConnectionError,
    UnknownLoreError,
    get_error_type,
)
from lore_parsers import (
    BranchList,
    BranchDescription,
    RevisionInfo,
    BisectResults,
    FileDescription,
    LockAcquire,
    LockRelease,
    LockQuery,
    LockStatus,
    can_parse_output,
    parse_lock_acquire,
    parse_lock_release,
    parse_lock_query,
    parse_lock_status,
    parse_branch_list,
    parse_branch_info,
    parse_revision_list,
    parse_revision_bisect,
    parse_file_info,
    parse_shared_store_info,
    SharedStoreInfo,
)

logger = logging.getLogger(__name__)


# TODO: Remove the following section when pytest migrations are done
def lore_ensure_local():
    remote_url = os.getenv("LORE_REMOTE_URL")
    if remote_url is None or remote_url == "":
        print(
            "No remote URL is set - smoke tests should not run against a live environment"
        )
        exit(-1)


def lore_run(lore_executable, repository_path, command, *args):
    try:
        command_args = [lore_executable]
        if len(repository_path) > 0:
            command_args += ["--repository", repository_path]
        command_args += [command, *args]
        print("Executing Lore: " + " ".join(command_args))
        output = subprocess.check_output(
            command_args, text=True, stderr=subprocess.STDOUT
        )
        print(output)
        return output
    except subprocess.CalledProcessError as e:
        print("Lore failed with return code " + str(e.returncode) + ":")
        output = e.stdout if e.stdout is not None else ""
        if len(output) > 0:
            print(output)
        else:
            print("*** No output ***")
        exit(e.returncode)


def lore_fail(lore_executable, repository_path, command, *args):
    try:
        command_args = [lore_executable]
        if len(repository_path) > 0:
            command_args += ["--repository", repository_path]
        command_args += [command, *args]
        print("Executing Lore expecting failure: " + " ".join(command_args))
        output = subprocess.check_output(
            command_args, text=True, stderr=subprocess.STDOUT
        )
        print("Lore did not fail as expected:")
        print(output)
        exit(1)
    except subprocess.CalledProcessError as e:
        output = e.stdout if e.stdout is not None else ""
        if len(output) > 0:
            print(e.stdout)
        else:
            print("*** No output ***")
        return output


def lore_can_fail(lore_executable, repository_path, command, *args):
    try:
        command_args = [lore_executable]
        if len(repository_path) > 0:
            command_args += ["--repository", repository_path]
        command_args += [command, *args]
        print("Executing Lore: " + " ".join(command_args))
        output = subprocess.check_output(
            command_args, text=True, stderr=subprocess.STDOUT
        )
        print(output)
        return output
    except subprocess.CalledProcessError as e:
        output = e.stdout if e.stdout is not None else ""
        if len(output) > 0:
            print(e.stdout)
        else:
            print("*** No output ***")
        return output


def verify_file(source_path, dest_path):
    filecmp.clear_cache()
    if not filecmp.cmp(
        source_path,
        dest_path,
        shallow=False,
    ):
        print("File identical check failed for " + dest_path)
        exit(-1)


# End of the section that needs to be removed eventually


def verify_signatures(revision_list: list[RevisionInfo], expected_count):
    assert len(revision_list) == expected_count, "Incorrect number of revisions"
    assert all(rev.has_valid_signature() for rev in revision_list), (
        "Invalid revision signature"
    )


class Lore:
    def __init__(
        self,
        lore_executable_path: str,
        path: str,
        name: str,
        global_dir: str,
        environment_vars: dict[str, str] | None = None,
        remote_url: str | None = None,
        remote_path: str | None = None,
        repo_id: str | None = None,
        create_repo: bool = True,
    ):
        self.lore_executable_path = lore_executable_path
        self.path = path
        self.name = name
        self.global_dir = global_dir
        self.environment_vars = environment_vars or {}
        # If the caller picked a specific remote_url, mirror it into the env
        # subprocess overrides — otherwise repository_create inherits the
        # session-level LORE_REMOTE_URL pointing at the autouse server and
        # registers this repo against the wrong instance.
        if remote_url:
            self.environment_vars.setdefault("LORE_REMOTE_URL", remote_url)
        if create_repo:
            self.repository_create(remote_path=remote_path, repo_id=repo_id)
        self.remote = remote_url if remote_url else os.getenv("LORE_REMOTE_URL", "")
        self.remote_path = remote_path if remote_path else self.remote + self.name
        self.test_commit_id = 1

    def dot_dir(self) -> str:
        """Return the repository metadata directory name (.lore or .urc)."""
        if os.path.isdir(os.path.join(self.path, ".urc")):
            return ".urc"
        return ".lore"

    def dot_path(self) -> str:
        """Return the full path to the repository metadata directory."""
        return os.path.join(self.path, self.dot_dir())

    def ignore_file(self) -> str:
        """Return the ignore file name (.loreignore or .urcignore).

        Under .lore/ format, falls back to .urcignore if .loreignore is not found.
        """
        if os.path.isdir(os.path.join(self.path, ".urc")):
            return ".urcignore"
        # .lore format: prefer .loreignore, fall back to .urcignore
        if not os.path.isfile(
            os.path.join(self.path, ".loreignore")
        ) and os.path.isfile(os.path.join(self.path, ".urcignore")):
            return ".urcignore"
        return ".loreignore"

    def run(
        self,
        urc_args: list[str] | None = None,
        use_os_dir: bool = False,
        path: str | None = None,
        cwd: str | None = None,
        check: bool = True,
        level: str | None = None,
        debug: bool = False,
        force: bool = False,
        dry_run: bool = False,
        no_pager: bool = False,
        json: bool = False,
        offline: bool = False,
        remote: bool = False,
        local: bool = False,
        identity: str | None = None,
        max_connections: int | None = None,
        file_count_limit: int | None = None,
        file_size_limit: int | None = None,
        compress_limit: int | None = None,
        search_limit: int | None = None,
        search_nearest: bool = False,
        no_gc: bool = False,
        non_interactive: bool = False,
    ):
        if urc_args is None:
            urc_args = []
        command_args = (
            [
                self.lore_executable_path,
            ]
            + (
                []
                if use_os_dir
                else ["--repository", path if path is not None else self.path]
            )
            + (["--log-level", level] if level else [])
            + (["--debug"] if debug else [])
            + (["--force"] if force else [])
            + (["--dry-run"] if dry_run else [])
            + (["--no-pager"] if no_pager else [])
            + (["--json"] if json else [])
            + (["--offline"] if offline else [])
            + (["--remote"] if remote else [])
            + (["--local"] if local else [])
            + (["--identity", identity] if identity else [])
            + (["--max-connections", str(max_connections)] if max_connections else [])
            + (
                ["--file-count-limit", str(file_count_limit)]
                if file_count_limit
                else []
            )
            + (["--file-size-limit", str(file_size_limit)] if file_size_limit else [])
            + (["--compress-limit", str(compress_limit)] if compress_limit else [])
            + (["--search-limit", str(search_limit)] if search_limit else [])
            + (["--search-nearest"] if search_nearest else [])
            + (["--no-gc"] if no_gc else [])
            + (["--non-interactive"] if non_interactive else [])
            + urc_args
        )
        command_string = " ".join(command_args)
        logger.info("Executing Lore command: %s", command_string)
        # Run from the repository root so commands behave like a real user
        # invoking lore inside the working tree. This matters for output that
        # is rendered relative to the current directory (e.g. `lore status`
        # paths). When `use_os_dir` is set the test deliberately controls the
        # cwd (e.g. via monkeypatch.chdir) to exercise repository discovery, so
        # leave the inherited cwd untouched in that case.
        run_cwd = cwd
        if run_cwd is None and not use_os_dir:
            target_dir = path if path is not None else self.path
            if target_dir and os.path.isdir(target_dir):
                run_cwd = target_dir
        attempt = 0
        max_attempts = 3
        while True:
            try:
                env = os.environ.copy()
                for k, v in self.environment_vars.items():
                    env[k] = v
                env["LORE_GLOBAL_PATH"] = self.global_dir
                # Isolate the auth token store per test so a developer's
                # locally cached credentials don't leak into smoke runs.
                env.setdefault("LORE_AUTH_PATH", self.global_dir)
                output = subprocess.run(
                    command_args,
                    capture_output=True,
                    text=True,
                    check=check,
                    env=env,
                    cwd=run_cwd,
                )
                logger.info(output.stdout + output.stderr)
                return output.stdout + output.stderr
            except subprocess.CalledProcessError as e:
                output = e.stdout + e.stderr if e.stdout or e.stderr else ""
                if len(output) == 0:
                    output = "*** Lore command failed with no output ***"
                logger.critical(
                    "Lore Error while running Lore command: %s", command_string
                )
                logger.critical(
                    "Lore failed with return code %s:\n%s", e.returncode, output
                )
                error_type = get_error_type(e)
                if error_type == UnknownLoreError:
                    logger.critical(
                        f"Unknown Lore error: {e}\n Do you need to add a new test exception?"
                    )
                elif error_type == ServerConnectionError:
                    attempt += 1
                    logger.critical(
                        f"Failed to connect to server, attempt {attempt} / {max_attempts}.\nError: {e}"
                    )
                    sleep(attempt)
                    if attempt < max_attempts:
                        continue
                raise error_type(f"Command: {command_string}\n{output}") from None
            except Exception as e:
                logger.critical(f"Unknown error: {e}")
                raise UnknownLoreError(str(e)) from None

    def repository_status(
        self,
        path: str | list[str] | Path | list[Path] | None = None,
        unstaged: bool = False,
        reset: bool = False,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        path = self._fix_paths(path)
        return self.run(
            ["repository", "status"]
            + path
            + (["--unstaged"] if unstaged else [])
            + (["--reset"] if reset else [])
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def repository_info(self, url: str | None = None, **kwargs: Unpack[GlobalOptions]):
        return self.run(["repository", "info"] + ([url] if url else []), **kwargs)

    def repository_list(self, url: str | None = None, **kwargs: Unpack[GlobalOptions]):
        return self.run(["repository", "list"] + ([url] if url else []), **kwargs)

    def repository_create(
        self,
        remote_path: str | None = None,
        description: str | None = None,
        repo_id: str | None = None,
        use_shared_store: bool = False,
        shared_store_path: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        output = self.run(
            ["repository", "create", remote_path if remote_path else self.name]
            + (["--description", description] if description else [])
            + (["--id", repo_id] if repo_id else [])
            + (["--use-shared-store"] if use_shared_store else [])
            + (["--shared-store-path", shared_store_path] if shared_store_path else []),
            **kwargs,
        )
        self._ensure_test_identity_in_config()
        return output

    def _ensure_test_identity_in_config(self) -> None:
        """Seed `identity = "test-user"` into .lore/config.toml when absent.

        Test infra avoids passing --identity globally because repository
        create/delete have an asymmetric server-side ownership check
        (`metadata.creator` is stamped from --identity, but the delete
        authorization uses the JWT-derived user_id, which is `<unknown>`
        under no-auth — so a `test-user` creator never matches). Seeding
        identity locally lets commit-producing commands resolve it via
        the existing config.toml chain without touching create/delete
        RPCs. Insert before the first `[section]` header so the key stays
        top-level rather than being parsed into a table.

        Pass identity="..." on individual commands to override per-call.
        """
        config = Path(self.dot_path()) / "config.toml"
        if not config.exists():
            return
        lines = config.read_text().splitlines()
        if any(line.strip().startswith("identity") for line in lines):
            return
        out: list[str] = []
        inserted = False
        for line in lines:
            if not inserted and line.strip().startswith("["):
                out.append('identity = "test-user"')
                inserted = True
            out.append(line)
        if not inserted:
            out.append('identity = "test-user"')
        config.write_text("\n".join(out) + "\n")

    def repository_delete(
        self, remote_path: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["repository", "delete", remote_path if remote_path else self.name],
            **kwargs,
        )

    def repository_verify(
        self,
        path: str | None = None,
        heal: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["repository", "verify"]
            + (["--path", path] if path else [])
            + (["--heal"] if heal else []),
            **kwargs,
        )

    def repository_dump(
        self,
        path: str | None = None,
        revision: str | None = None,
        max_depth: int | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["repository", "dump"]
            + (["--path", self._fix_path(path)] if path else [])
            + (["--revision", revision] if revision else [])
            + (["--max-depth", max_depth] if max_depth else []),
            **kwargs,
        )

    def repository_gc(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["repository", "gc"], **kwargs)

    def repository_store(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["repository", "store"], **kwargs)

    def repository_store_immutable(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["repository", "store", "immutable"], **kwargs)

    def repository_store_immutable_query(
        self,
        address: str | None = None,
        recurse: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["repository", "store", "immutable", "query"]
            + ([address] if address else [])
            + (["--recurse"] if recurse else []),
            **kwargs,
        )

    def repository_verify_fragment(
        self,
        fragment_hash: str,
        context: str | None = None,
        local: bool = False,
        heal: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["repository", "verify", "fragment", fragment_hash]
            + (["--context", context] if context else [])
            + (["--local"] if local else [])
            + (["--heal"] if heal else []),
            **kwargs,
        )

    @overload
    def branch_list(
        self, *, archived: bool = False, **kwargs: Unpack[GlobalOptionsParseable]
    ) -> BranchList: ...

    @overload
    def branch_list(
        self, *, archived: bool = False, **kwargs: Unpack[GlobalOptions]
    ) -> BranchList | str: ...

    def branch_list(
        self, *, archived: bool = False, **kwargs: Unpack[GlobalOptions]
    ) -> BranchList | str:
        args = ["branch", "list"] + (["--archived"] if archived else [])
        output = self.run(args, **kwargs)
        if can_parse_output(kwargs):
            return parse_branch_list(output)
        return output

    @overload
    def branch_info(
        self, name: str | None = None, **kwargs: Unpack[GlobalOptionsParseable]
    ) -> BranchDescription: ...

    @overload
    def branch_info(
        self, name: str | None = None, **kwargs: Unpack[GlobalOptions]
    ) -> BranchDescription | str: ...

    def branch_info(
        self, name: str | None = None, **kwargs: Unpack[GlobalOptions]
    ) -> BranchDescription | str:
        output = self.run(["branch", "info"] + ([name] if name else []), **kwargs)
        if can_parse_output(kwargs):
            return parse_branch_info(output)
        return output

    def branch_create(
        self,
        name: str | None = None,
        id: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "create"]
            + ([name] if name else [])
            + (["--id", id] if id else []),
            **kwargs,
        )

    def branch_switch(
        self,
        name: str | None = None,
        revision: str | None = None,
        reset: bool = False,
        bare: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "switch"]
            + ([name] if name else [])
            + ([revision] if revision else [])
            + (["--reset"] if reset else [])
            + (["--bare"] if bare else []),
            **kwargs,
        )

    def branch_push(self, name: str | None = None, **kwargs: Unpack[GlobalOptions]):
        return self.run(["branch", "push"] + ([name] if name else []), **kwargs)

    def branch_merge(
        self,
        name: str | None = None,
        repo_id: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "merge"]
            + ([name] if name else [])
            + (["--id", repo_id] if repo_id else []),
            **kwargs,
        )

    def branch_merge_unresolve(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["branch", "merge", "unresolve"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def branch_merge_into(
        self,
        name: str | None = None,
        message: str | None = None,
        repo_id: str | None = None,
        link: str | None = None,
        ignore_links: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            [
                "branch",
                "merge",
                "into",
                name if name else "",
                message if message else "",
            ]
            + (["--id", repo_id] if repo_id else [])
            + (["--link", link] if link else [])
            + (["--ignore-links"] if ignore_links else []),
            **kwargs,
        )

    def branch_merge_start(
        self,
        name: str | None = None,
        repo_id: str | None = None,
        message: str | None = None,
        no_commit: bool = False,
        link: str | None = None,
        dry_run: bool = False,
        ignore_links: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "merge", "start"]
            + ([name] if name else [])
            + (["--id", repo_id] if repo_id else [])
            + (["--message", message] if message else [])
            + (["--no-commit"] if no_commit else [])
            + (["--link", link] if link else [])
            + (["--dry-run"] if dry_run else [])
            + (["--ignore-links"] if ignore_links else []),
            **kwargs,
        )

    def branch_merge_restart(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["branch", "merge", "restart"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def branch_merge_resolve(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["branch", "merge", "resolve"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def branch_merge_resolve_mine(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["branch", "merge", "resolve", "mine"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def branch_merge_resolve_theirs(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["branch", "merge", "resolve", "theirs"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def branch_merge_abort(
        self,
        link: str | None = None,
        ignore_links: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "merge", "abort"]
            + (["--link", link] if link else [])
            + (["--ignore-links"] if ignore_links else []),
            **kwargs,
        )

    def branch_diff(
        self,
        target: str | None = None,
        source: str | None = None,
        auto_resolve: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "diff"]
            + ([target] if target else [])
            + (["--source", source] if source else [])
            + (["--auto-resolve"] if auto_resolve else []),
            **kwargs,
        )

    def branch_archive(self, branch: str | None = None, **kwargs: Unpack[GlobalOptions]):
        return self.run(["branch", "archive"] + ([branch] if branch else []), **kwargs)

    def branch_reset(
        self,
        revision: str | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "reset"]
            + ([revision] if revision else [])
            + (["--branch", branch] if branch else []),
            **kwargs,
        )

    def branch_protect(
        self, branch: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(["branch", "protect"] + ([branch] if branch else []), **kwargs)

    def branch_unprotect(
        self, branch: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["branch", "unprotect"] + ([branch] if branch else []), **kwargs
        )

    def branch_latest(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["branch", "latest"], **kwargs)

    def branch_latest_list(
        self,
        limit: int | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "latest", "list"]
            + ([str(limit)] if limit else [])
            + (["--branch", branch] if branch else []),
            **kwargs,
        )

    def branch_metadata_set(
        self,
        pairs: list[str] | None = None,
        branch: str | None = None,
        binary: bool = False,
        numeric: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "metadata", "set"]
            + (pairs if pairs else [])
            + (["--branch", branch] if branch else [])
            + (["--binary"] if binary else [])
            + (["--numeric"] if numeric else []),
            **kwargs,
        )

    def branch_metadata_get(
        self,
        key: str | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "metadata", "get"]
            + ([key] if key else [])
            + (["--branch", branch] if branch else []),
            **kwargs,
        )

    def branch_metadata_clear(
        self,
        keys: list[str] | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["branch", "metadata", "clear"]
            + (keys if keys else [])
            + (["--branch", branch] if branch else []),
            **kwargs,
        )

    @overload
    def revision_history(
        self,
        length: int | None = None,
        revision: str | None = None,
        branch: str | None = None,
        date: int | None = None,
        oneline: bool = False,
        only_branch: bool = False,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> list[RevisionInfo]: ...

    @overload
    def revision_history(
        self,
        length: int | None = None,
        revision: str | None = None,
        branch: str | None = None,
        date: int | None = None,
        oneline: bool = False,
        only_branch: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[RevisionInfo] | str: ...

    def revision_history(
        self,
        length: int | None = None,
        revision: str | None = None,
        branch: str | None = None,
        date: int | None = None,
        oneline: bool = False,
        only_branch: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[RevisionInfo] | str:
        output = self.run(
            ["revision", "history"]
            + ([str(length)] if length else [])
            + (["--revision", revision] if revision else [])
            + (["--branch", branch] if branch else [])
            + (["--date", str(date)] if date is not None else [])
            + (["--oneline"] if oneline else [])
            + (["--only-branch"] if only_branch else []),
            **kwargs,
        )

        if can_parse_output(kwargs):
            return parse_revision_list(output, oneline)
        return output

    @overload
    def revision_info(
        self,
        revision: str | None = None,
        delta: bool = False,
        metadata: bool = False,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> RevisionInfo: ...

    @overload
    def revision_info(
        self,
        revision: str | None = None,
        delta: bool = False,
        metadata: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> RevisionInfo | str: ...

    def revision_info(
        self,
        revision: str | None = None,
        delta: bool = False,
        metadata: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> RevisionInfo | str:
        output = self.run(
            ["revision", "info"]
            + ([revision] if revision else [])
            + (["--delta"] if delta else [])
            + (["--metadata"] if metadata else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_revision_list(output, False)[0]
        return output

    def revision_commit(
        self,
        message: str | None = None,
        stats: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["revision", "commit"]
            + ([message] if message else [])
            + (["--stats"] if stats else []),
            **kwargs,
        )

    def revision_amend(
        self,
        message: str | None = None,
        stats: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["revision", "amend"]
            + ([message] if message else [])
            + (["--stats"] if stats else []),
            **kwargs,
        )

    def revision_sync(
        self,
        revision: str | None = None,
        forward_changes: bool = False,
        reset: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["revision", "sync"]
            + ([revision] if revision else [])
            + (["--forward-changes"] if forward_changes else [])
            + (["--reset"] if reset else []),
            **kwargs,
        )

    @overload
    def revision_bisect(
        self,
        start: str | None = None,
        end: str | None = None,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> BisectResults: ...

    @overload
    def revision_bisect(
        self,
        start: str | None = None,
        end: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> BisectResults | str: ...

    def revision_bisect(
        self,
        start: str | None = None,
        end: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> BisectResults | str:
        output = self.run(
            ["revision", "bisect"]
            + (["--start", start] if start else [])
            + (["--end", end] if end else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_revision_bisect(output)
        return output

    def revision_diff(
        self,
        source: str | None = None,
        target: str | None = None,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        if paths is not None:
            paths = self._fix_paths(paths)
        return self.run(
            ["revision", "diff"]
            + ([source] if source else [])
            + (["--target", target] if target else [])
            + ([item for path in paths for item in ("--path", path)] if paths else [])
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_find(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["revision", "find"], **kwargs)

    def revision_find_metadata(
        self,
        key: str | None = None,
        value: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            [
                "revision",
                "find",
                "metadata",
                key if key else "",
                value if value else "",
            ],
            **kwargs,
        ).strip()

    def revision_find_number(
        self, number: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["revision", "find", "number"] + ([number] if number else []), **kwargs
        )

    def revision_restore(
        self, message: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["revision", "restore"] + ([message] if message else []), **kwargs
        )

    def revision_cherry_pick(
        self,
        revision: str | None = None,
        message: str | None = None,
        no_commit: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["revision", "cherry-pick"]
            + ([revision] if revision else [])
            + (["--message", message] if message else [])
            + (["--no-commit"] if no_commit else []),
            **kwargs,
        )

    def revision_cherry_pick_unresolve(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "cherry-pick", "unresolve"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_cherry_pick_restart(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "cherry-pick", "restart"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_cherry_pick_resolve(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "cherry-pick", "resolve"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_cherry_pick_resolve_mine(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "cherry-pick", "resolve", "mine"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_cherry_pick_resolve_theirs(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "cherry-pick", "resolve", "theirs"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_cherry_pick_abort(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["revision", "cherry-pick", "abort"], **kwargs)

    def revision_revert(
        self,
        revision: str | None = None,
        message: str | None = None,
        no_commit: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["revision", "revert"]
            + ([revision] if revision else [])
            + (["--message", message] if message else [])
            + (["--no-commit"] if no_commit else []),
            **kwargs,
        )

    def revision_revert_abort(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["revision", "revert", "abort"], **kwargs)

    def revision_revert_restart(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "revert", "restart"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_revert_resolve(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "revert", "resolve"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_revert_resolve_mine(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "revert", "resolve", "mine"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_revert_resolve_theirs(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "revert", "resolve", "theirs"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def revision_revert_unresolve(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["revision", "revert", "unresolve"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def repository_metadata_get(
        self,
        key: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["repository", "metadata", "get"] + ([key] if key else []),
            **kwargs,
        )

    def repository_metadata_set(
        self,
        pairs: list[str] | None = None,
        binary: bool = False,
        numeric: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["repository", "metadata", "set"]
            + (pairs if pairs else [])
            + (["--binary"] if binary else [])
            + (["--numeric"] if numeric else []),
            **kwargs,
        )

    def repository_metadata_clear(
        self,
        keys: list[str] | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["repository", "metadata", "clear"] + (keys if keys else []),
            **kwargs,
        )

    def revision_metadata_clear(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["revision", "metadata", "clear"], **kwargs)

    def revision_metadata_get(
        self,
        key: str | None = None,
        revision: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["revision", "metadata", "get"]
            + ([key] if key else [])
            + (["--revision", revision] if revision else []),
            **kwargs,
        )

    def revision_metadata_set(
        self,
        pairs: list[str] | None = None,
        binary: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["revision", "metadata", "set"]
            + (pairs if pairs else [])
            + (["--binary"] if binary else []),
            **kwargs,
        )

    @overload
    def file_info(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        revision: str | None = None,
        filtered: bool = False,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> list[FileDescription]: ...

    @overload
    def file_info(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        revision: str | None = None,
        filtered: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[FileDescription] | str: ...

    def file_info(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        revision: str | None = None,
        filtered: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[FileDescription] | str:
        paths = self._fix_paths(paths)
        output = self.run(
            ["file", "info"]
            + paths
            + (["--targets", targets] if targets else [])
            + (["--revision", revision] if revision else [])
            + (["--filtered"] if filtered else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_file_info(output)
        return output

    def file_metadata(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["file", "metadata"], **kwargs)

    def file_metadata_clear(
        self, path: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["file", "metadata", "clear"] + ([self._fix_path(path)] if path else []),
            **kwargs,
        )

    def file_metadata_get(
        self,
        path: str | None = None,
        key: str | None = None,
        revision: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["file", "metadata", "get"]
            + ([self._fix_path(path)] if path else [])
            + ([key] if key else [])
            + (["--revision", revision] if revision else []),
            **kwargs,
        )

    def file_metadata_set(
        self,
        path: str | None = None,
        pairs: list[str] | None = None,
        binary: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["file", "metadata", "set"]
            + ([self._fix_path(path)] if path else [])
            + (pairs if pairs else [])
            + (["--binary"] if binary else []),
            **kwargs,
        )

    def file_stage(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        case: str | None = None,
        targets: str | None = None,
        scan: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["file", "stage"]
            + paths
            + (["--case", case] if case else [])
            + (["--targets", targets] if targets else [])
            + (["--scan"] if scan else []),
            **kwargs,
        )

    def file_stage_move(
        self, from_path: str, to_path: str, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            [
                "file",
                "stage",
                "move",
                self._fix_path(from_path),
                self._fix_path(to_path),
            ],
            **kwargs,
        )

    def file_stage_copy(
        self, from_path: str, to_path: str, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            [
                "file",
                "stage",
                "copy",
                self._fix_path(from_path),
                self._fix_path(to_path),
            ],
            **kwargs,
        )

    def file_stage_merge(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["file", "stage", "merge"]
            + paths
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def file_unstage(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["file", "unstage"] + paths + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def file_reset(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        purge: bool = False,
        targets: str | None = None,
        revision: str | None = None,
        last_merged_from: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["file", "reset"]
            + paths
            + (["--purge"] if purge else [])
            + (["--targets", targets] if targets else [])
            + (["--revision", revision] if revision else [])
            + (["--last-merged-from", last_merged_from] if last_merged_from else []),
            **kwargs,
        )

    def file_obliterate(
        self,
        address: str | None = None,
        path: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["file", "obliterate"]
            + (["--address", address] if address else [])
            + (["--path", self._fix_path(path)] if path else []),
            **kwargs,
        )

    def file_dump(
        self,
        address: str | None = None,
        path: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["file", "dump"]
            + (["--address", address] if address else [])
            + (["--path", self._fix_path(path)] if path else []),
            **kwargs,
        )

    def file_history(
        self,
        path: str | None = None,
        length: int | None = None,
        revision: str | None = None,
        branch: str | None = None,
        depth: int | None = None,
        oneline: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["file", "history"]
            + ([self._fix_path(path)] if path else [])
            + ([str(length)] if length else [])
            + (["--revision", revision] if revision else [])
            + (["--branch", branch] if branch else [])
            + (["--depth", str(depth)] if depth else [])
            + (["--oneline"] if oneline else []),
            **kwargs,
        )

    def file_diff(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        source: str | None = None,
        target: str | None = None,
        targets: str | None = None,
        diff3: bool = False,
        context: int | None = None,
        ignore_space_at_eol: bool = False,
        ignore_space_change: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["file", "diff"]
            + paths
            + (["--source", source] if source else [])
            + (["--target", target] if target else [])
            + (["--targets", targets] if targets else [])
            + (["--diff3"] if diff3 else [])
            + (["--context", str(context)] if context is not None else [])
            + (["--ignore-space-at-eol"] if ignore_space_at_eol else [])
            + (["--ignore-space-change"] if ignore_space_change else []),
            **kwargs,
        )

    def file_write(
        self,
        address: str | None = None,
        path: str | None = None,
        revision: str | None = None,
        output: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["file", "write"]
            + (["--address", address] if address else [])
            + (["--path", self._fix_path(path)] if path else [])
            + (["--revision", revision] if revision else [])
            + (["--output", output] if output else []),
            **kwargs,
        )

    def file_hash(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["file", "hash"] + paths + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def file_dependency_add(
        self,
        source: str,
        dependencies: str | list[str],
        tags: list[str] | None = None,
        force: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        if isinstance(dependencies, str):
            dependencies = [dependencies]
        deps = [self._fix_path(d) for d in dependencies]
        tag_args = []
        if tags:
            for tag in tags:
                tag_args += ["--tag", tag]
        return self.run(
            ["file", "dependency", "add", self._fix_path(source)]
            + deps
            + tag_args
            + (["--force"] if force else []),
            **kwargs,
        )

    def file_dependency_remove(
        self,
        source: str,
        dependencies: str | list[str],
        tags: list[str] | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        if isinstance(dependencies, str):
            dependencies = [dependencies]
        deps = [self._fix_path(d) for d in dependencies]
        tag_args = []
        if tags:
            for tag in tags:
                tag_args += ["--tag", tag]
        return self.run(
            ["file", "dependency", "remove", self._fix_path(source)] + deps + tag_args,
            **kwargs,
        )

    def file_dependency_list(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        recursive: bool = False,
        reverse: bool = False,
        tags: list[str] | None = None,
        depth: int | None = None,
        revision: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        if paths is not None:
            if isinstance(paths, (str, Path)):
                paths = [self._fix_path(paths)]
            else:
                paths = [self._fix_path(p) for p in paths]
        else:
            paths = []
        tag_args = []
        if tags:
            for tag in tags:
                tag_args += ["--tag", tag]
        return self.run(
            ["file", "dependency", "list"]
            + paths
            + (["--recursive"] if recursive else [])
            + (["--reverse"] if reverse else [])
            + tag_args
            + (["--depth", str(depth)] if depth is not None else [])
            + (["--revision", revision] if revision else []),
            **kwargs,
        )

    def auth_login(
        self,
        remote_url: str | None = None,
        api_key: str | None = None,
        eg1_token: str | None = None,
        no_browser: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["auth", "login"]
            + ([remote_url] if remote_url else [])
            + (["--api-key", api_key] if api_key else [])
            + (["--eg1-token", eg1_token] if eg1_token else [])
            + (["--no-browser"] if no_browser else []),
            **kwargs,
        )

    def auth_info_with_token(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(
            ["auth", "info", "--with-token"],
            **kwargs,
        )

    def auth_user_info(
        self, remote_url: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["auth", "user-info"]
            + (["--remote-url", remote_url] if remote_url else []),
            **kwargs,
        )

    def layer_add(
        self,
        target_path: str,
        source_repository: Lore,
        source_path: str,
        metadata: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            [
                "layer",
                "add",
                self._fix_path(target_path),
                source_repository.name,
                source_path,
            ]
            + (["--metadata", metadata] if metadata else []),
            **kwargs,
        )

    def layer_remove(
        self,
        target_path: str,
        source_repository: Lore | None = None,
        purge: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            [
                "layer",
                "remove",
                self._fix_path(target_path),
            ]
            + ([source_repository.name] if source_repository is not None else [])
            + (["--purge"] if purge else []),
            **kwargs,
        )

    def layer_list(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["layer", "list"], **kwargs)

    def logfile_info(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["logfile", "info"], **kwargs)

    def login(
        self,
        remote_url: str | None = None,
        api_key: str | None = None,
        eg1_token: str | None = None,
        no_browser: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["login"]
            + ([remote_url] if remote_url else [])
            + (["--api-key", api_key] if api_key else [])
            + (["--eg1-token", eg1_token] if eg1_token else [])
            + (["--no-browser"] if no_browser else []),
            **kwargs,
        )

    def link_add(
        self,
        link_path: str,
        link_identifier: str,
        source_path: str,
        pin: str | None = None,
        disable_branching: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            [
                "link",
                "add",
                self._fix_path(link_path),
                link_identifier,
                source_path,
            ]
            + (["--pin", pin] if pin else [])
            + (["--disable-branching"] if disable_branching else []),
            **kwargs,
        )

    def link_remove(
        self, link_path: str | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["link", "remove"] + ([self._fix_path(link_path)] if link_path else []),
            **kwargs,
        )

    def link_list(
        self,
        staged: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["link", "list"] + (["--staged"] if staged else []),
            **kwargs,
        )

    def link_update(
        self,
        link_path: str,
        pin: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["link", "update", self._fix_path(link_path)]
            + (["--pin", pin] if pin else []),
            **kwargs,
        )

    def status(
        self,
        path: str | list[str] | Path | list[Path] | None = None,
        unstaged: bool = False,
        scan: bool = False,
        check_dirty: bool = False,
        reset: bool = False,
        revision_only: bool = False,
        count: bool = False,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        path = self._fix_paths(path)
        return self.run(
            ["status"]
            + path
            + (["--unstaged"] if unstaged else [])
            + (["--scan"] if scan else [])
            + (["--check-dirty"] if check_dirty else [])
            + (["--reset"] if reset else [])
            + (["--revision-only"] if revision_only else [])
            + (["--count"] if count else [])
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def clone(
        self,
        path: str | None = None,
        name: str | None = None,
        view: str | None = None,
        revision: str | None = None,
        branch: str | None = None,
        bare: bool = False,
        virtually: bool = False,
        direct_file_write: bool = False,
        direct_file_io: bool = False,
        flush_file: bool = False,
        layer: str | None = None,
        layer_metadata: str | None = None,
        prefetch: str | None = None,
        use_shared_store: bool = False,
        shared_store_path: str | None = None,
        root_files: list[str] | None = None,
        dependency_tags: list[str] | None = None,
        dependency_recursive: bool = False,
        dependency_depth_limit: int = 0,
        no_tracking: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        new_repo_name = name if name else self.generate_random_name()
        if path is None:
            parent = Path(self.path).parent
            new_repo_path = parent / new_repo_name
        else:
            new_repo_path = Path(path)
        if not kwargs.get("dry_run"):
            new_repo_path.mkdir(exist_ok=True)
        root_file_args = []
        for rf in root_files or []:
            root_file_args += ["--root-file", rf]
        dep_tag_args = []
        for dt in dependency_tags or []:
            dep_tag_args += ["--dependency-tag", dt]
        self.run(
            ["repository", "clone", self.remote_path, str(new_repo_path)]
            + (["--view", self._fix_path(view)] if view else [])
            + (["--revision", revision] if revision else [])
            + (["--branch", branch] if branch else [])
            + (["--bare"] if bare else [])
            + (["--virtually"] if virtually else [])
            + (["--direct-file-write"] if direct_file_write else [])
            + (["--direct-file-io"] if direct_file_io else [])
            + (["--flush-file"] if flush_file else [])
            + (["--layer", layer] if layer else [])
            + (["--layer-metadata", layer_metadata] if layer_metadata else [])
            + (["--prefetch", prefetch] if prefetch else [])
            + (["--use-shared-store"] if use_shared_store else [])
            + (["--shared-store-path", shared_store_path] if shared_store_path else [])
            + root_file_args
            + dep_tag_args
            + (["--dependency-recursive"] if dependency_recursive else [])
            + (
                ["--dependency-depth-limit", str(dependency_depth_limit)]
                if dependency_depth_limit > 0
                else []
            )
            + (["--no-tracking"] if no_tracking else []),
            **kwargs,
        )
        new_repo = Lore(
            global_dir=self.global_dir,
            remote_path=self.remote_path,
            remote_url=self.remote,
            lore_executable_path=self.lore_executable_path,
            path=str(new_repo_path),
            name=new_repo_name,
            create_repo=False,
        )
        new_repo._ensure_test_identity_in_config()
        return new_repo

    def stage(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        case: str | None = None,
        targets: str | None = None,
        scan: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["stage"]
            + paths
            + (["--case", case] if case else [])
            + (["--targets", targets] if targets else [])
            + (["--scan"] if scan else []),
            **kwargs,
        )

    def stage_move(self, from_path: str, to_path: str, **kwargs: Unpack[GlobalOptions]):
        return self.run(
            [
                "stage",
                "move",
                self._fix_path(from_path),
                self._fix_path(to_path),
            ],
            **kwargs,
        )

    def stage_merge(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["stage", "merge"] + paths + (["--targets", targets] if targets else []),
            **kwargs,
        )

    def dirty(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["dirty"] + paths + (["--targets", targets] if targets else []), **kwargs
        )

    def dirty_move(self, from_path: str, to_path: str, **kwargs: Unpack[GlobalOptions]):
        return self.run(
            ["file", "dirty", "move", self._fix_path(from_path), self._fix_path(to_path)],
            **kwargs,
        )

    def dirty_copy(self, from_path: str, to_path: str, **kwargs: Unpack[GlobalOptions]):
        return self.run(
            ["file", "dirty", "copy", self._fix_path(from_path), self._fix_path(to_path)],
            **kwargs,
        )

    def unstage(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["unstage"] + paths + (["--targets", targets] if targets else []), **kwargs
        )

    def reset(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        purge: bool = False,
        targets: str | None = None,
        revision: str | None = None,
        last_merged_from: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["reset"]
            + paths
            + (["--purge"] if purge else [])
            + (["--targets", targets] if targets else [])
            + (["--revision", revision] if revision else [])
            + (["--last-merged-from", last_merged_from] if last_merged_from else []),
            **kwargs,
        )

    def diff(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        source: str | None = None,
        target: str | None = None,
        targets: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        paths = self._fix_paths(paths)
        return self.run(
            ["diff"]
            + paths
            + (["--source", source] if source else [])
            + (["--target", target] if target else [])
            + (["--targets", targets] if targets else []),
            **kwargs,
        )

    @overload
    def history(
        self,
        length: int | str | None = None,
        revision: str | None = None,
        branch: str | None = None,
        date: int | None = None,
        oneline: bool = False,
        only_branch: bool = False,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> list[RevisionInfo]: ...

    @overload
    def history(
        self,
        length: int | str | None = None,
        revision: str | None = None,
        branch: str | None = None,
        date: int | None = None,
        oneline: bool = False,
        only_branch: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[RevisionInfo] | str: ...

    def history(
        self,
        length: int | str | None = None,
        revision: str | None = None,
        branch: str | None = None,
        date: int | None = None,
        oneline: bool = False,
        only_branch: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[RevisionInfo] | str:
        output = self.run(
            ["history"]
            + ([str(length)] if length else [])
            + (["--revision", revision] if revision else [])
            + (["--branch", branch] if branch else [])
            + (["--date", str(date)] if date is not None else [])
            + (["--oneline"] if oneline else [])
            + (["--only-branch"] if only_branch else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_revision_list(output, oneline)
        return output

    def commit(
        self,
        message: str | None = None,
        stats: bool = False,
        link: str | None = None,
        link_messages: dict[str, str] | None = None,
        layer: str | None = None,
        layer_messages: dict[str, str] | None = None,
        **kwargs: Unpack[GlobalOptions],
    ):
        if message is None:
            message = f"Test commit {self.test_commit_id}"
            self.test_commit_id += 1
        link_args = []
        if link_messages:
            for path, msg in link_messages.items():
                link_args.extend(["--link-message", path, msg])
        layer_args = []
        if layer_messages:
            for path, msg in layer_messages.items():
                layer_args.extend(["--layer-message", path, msg])
        return self.run(
            ["commit", message if message else ""]
            + (["--stats"] if stats else [])
            + (["--link", link] if link else [])
            + link_args
            + (["--layer", layer] if layer else [])
            + layer_args,
            **kwargs,
        )

    def sync(
        self,
        revision: str | None = None,
        forward_changes: bool = False,
        reset: bool = False,
        root_files: list[str] | None = None,
        dependency_tags: list[str] | None = None,
        dependency_recursive: bool = False,
        dependency_depth_limit: int = 0,
        **kwargs: Unpack[GlobalOptions],
    ):
        root_file_args = []
        for rf in root_files or []:
            root_file_args += ["--root-file", rf]
        dep_tag_args = []
        for dt in dependency_tags or []:
            dep_tag_args += ["--dependency-tag", dt]
        return self.run(
            ["sync"]
            + ([revision] if revision else [])
            + (["--forward-changes"] if forward_changes else [])
            + (["--reset"] if reset else [])
            + root_file_args
            + dep_tag_args
            + (["--dependency-recursive"] if dependency_recursive else [])
            + (
                ["--dependency-depth-limit", str(dependency_depth_limit)]
                if dependency_depth_limit > 0
                else []
            ),
            **kwargs,
        )

    def repository_list(self):
        return self.run(["repository", "list", self.remote])

    def push(
        self,
        name: str | None = None,
        fast_forward_merge: bool = False,
        **kwargs: Unpack[GlobalOptions],
    ):
        return self.run(
            ["push"]
            + ([name] if name else [])
            + (["--fast-forward-merge"] if fast_forward_merge else []),
            **kwargs,
        )

    @overload
    def lock_acquire(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> LockAcquire: ...

    @overload
    def lock_acquire(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> LockAcquire | str: ...

    def lock_acquire(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> LockAcquire | str:
        paths = self._fix_paths(paths)
        output = self.run(
            ["lock", "acquire"] + paths + (["--branch", branch] if branch else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_lock_acquire(output)
        return output

    @overload
    def lock_status(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> list[LockStatus]: ...

    @overload
    def lock_status(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[LockStatus] | str: ...

    def lock_status(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[LockStatus] | str:
        paths = self._fix_paths(paths)
        output = self.run(
            ["lock", "status"] + paths + (["--branch", branch] if branch else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_lock_status(output)
        return output

    @overload
    def lock_query(
        self,
        branch: str | None = None,
        owner: str | None = None,
        path: str | None = None,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> list[LockQuery]: ...

    @overload
    def lock_query(
        self,
        branch: str | None = None,
        owner: str | None = None,
        path: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[LockQuery] | str: ...

    def lock_query(
        self,
        branch: str | None = None,
        owner: str | None = None,
        path: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> list[LockQuery] | str:
        output = self.run(
            ["lock", "query"]
            + (["--branch", branch] if branch else [])
            + (["--owner", owner] if owner else [])
            + (["--path", self._fix_path(path)] if path else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_lock_query(output)
        return output

    @overload
    def lock_release(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        owner: str | None = None,
        **kwargs: Unpack[GlobalOptionsParseable],
    ) -> LockRelease: ...

    @overload
    def lock_release(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        owner: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> LockRelease | str: ...

    def lock_release(
        self,
        paths: str | list[str] | Path | list[Path] | None = None,
        branch: str | None = None,
        owner: str | None = None,
        **kwargs: Unpack[GlobalOptions],
    ) -> LockRelease | str:
        paths = self._fix_paths(paths)
        output = self.run(
            ["lock", "release"]
            + paths
            + (["--branch", branch] if branch else [])
            + (["--owner", owner] if owner else []),
            **kwargs,
        )
        if can_parse_output(kwargs):
            return parse_lock_release(output)
        return output

    def shared_store_create(
        self,
        remote_url: str,
        path: str | None = None,
        make_default: bool = True,
        **kwargs: Unpack[GlobalOptions],
    ) -> str:
        return self.run(
            ["shared-store", "create", remote_url]
            + (["--path", path] if path else [])
            + ([] if make_default else ["--make-default", "false"]),
            **kwargs,
        )

    @overload
    def shared_store_info(
        self, **kwargs: Unpack[GlobalOptionsParseable]
    ) -> SharedStoreInfo: ...

    @overload
    def shared_store_info(
        self, **kwargs: Unpack[GlobalOptions]
    ) -> SharedStoreInfo | str: ...

    def shared_store_info(
        self, **kwargs: Unpack[GlobalOptions]
    ) -> SharedStoreInfo | str:
        output = self.run(["shared-store", "info"])
        if can_parse_output(kwargs):
            return parse_shared_store_info(output)
        return output

    def shared_store_set_use_automatically(self, enabled: bool):
        return self.run(
            ["shared-store", "set-use-automatically", "true" if enabled else "false"]
        )

    def service_run(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["service", "run"], **kwargs)

    def service_start(self, **kwargs: Unpack[GlobalOptions]):
        return self.run(["service", "start"], **kwargs)

    def service_stop(self, stop_all: bool = False, **kwargs: Unpack[GlobalOptions]):
        return self.run(["service", "stop", "true" if stop_all else "false"], **kwargs)

    def notification_subscribe(
        self, timeout: int | None = None, **kwargs: Unpack[GlobalOptions]
    ):
        return self.run(
            ["notification", "subscribe"] + ([str(timeout)] if timeout else []),
            **kwargs,
        )

    def has_branch(self, branch):
        branch_list = self.branch_list()
        return (
            branch_list.current_branch == branch
            or branch in branch_list.local_branches
            or branch in branch_list.remote_branches
        )

    def write_files(
        self, files_and_contents: TypedDict[str | Path, bytes | str | Iterable[str]]
    ):
        for file_name, contents in files_and_contents.items():
            self.make_dirs(os.path.dirname(file_name))
            write_mode = "w+b" if type(contents) is bytes else "w+"
            with self.open_file(file_name, write_mode) as output_file:
                if type(contents) is bytes:
                    output_file.write(contents)
                elif type(contents) is str:
                    output_file.write(contents)
                else:
                    output_file.writelines(contents)

    def write_commit_push(
        self,
        commit_message: str | None,
        files_and_contents: TypedDict[str | Path, bytes | str | Iterable[str]],
        offline: bool = False,
    ) -> None:
        self.write_files(files_and_contents)

        for file_name in files_and_contents.keys():
            self.stage(file_name, offline=offline)

        self.commit(message=commit_message, offline=offline)
        if not offline:
            self.push()

    def compare_file(
        self,
        other_file_repo: Lore | None,
        file_path: str | Path,
        other_file_path: str | Path | None = None,
    ):
        if other_file_path is None:
            other_file_path = file_path

        source_path = self._fix_path(file_path)
        if other_file_repo:
            dest_path = other_file_repo._fix_path(other_file_path)
        else:
            dest_path = self._fix_path(other_file_path)

        filecmp.clear_cache()
        return filecmp.cmp(source_path, dest_path, shallow=False)

    def open_file(self, path, mode="r", encoding=None):
        return open(self._fix_path(path), mode, encoding=encoding)

    def move(self, src: str | Path, dst: str | Path):
        shutil.move(self._fix_path(src), self._fix_path(dst))

    def copy_file(self, from_path, to_path):
        shutil.copyfile(self._fix_path(from_path), self._fix_path(to_path))

    def copy2(self, from_path, to_path="", to_repo: Lore | None = None):
        if to_repo:
            to_path_rel = Path(to_repo.path) / to_path
        else:
            to_path_rel = Path(self.path) / to_path

        from_path_rel = Path(self.path) / from_path

        shutil.copy2(from_path_rel, to_path_rel)

    def copy_tree(self, src: str | Path, dst: str | Path):
        """Copy a directory tree from src to dst, with path resolution."""
        shutil.copytree(self._fix_path(src), self._fix_path(dst))

    def remove_file(self, path):
        os.remove(self._fix_path(path))

    def remove_dir(self, path):
        os.rmdir(self._fix_path(path))

    def rmtree(self, path):
        shutil.rmtree(self._fix_path(path), ignore_errors=True)

    def file_exists(self, path):
        return os.path.isfile(self._fix_path(path))

    def path_exists(self, path):
        return os.path.exists(os.path.join(self.path, path))

    def clear_local_files(self):
        """
        Remove all local repository files for this repository
        """
        import shutil

        shutil.rmtree(self.path, ignore_errors=True)

    def make_dirs(self, path):
        os.makedirs(self._fix_path(path), exist_ok=True)

    def get_id(self):
        raw_repo_id = bytes()
        with open(os.path.join(self.dot_path(), "id"), "rb") as id_file:
            raw_repo_id = id_file.read(32)
        processed_repo_id = raw_repo_id.hex()
        return processed_repo_id

    def get_name(self):
        return self.name

    def list_paths(self, prefix: Path | None = None) -> typing.List[Path]:
        if prefix is None:
            prefix = Path()
        if prefix == Path(".lore"):
            return []
        path = pathlib.Path(self.path)
        prefixed_path = path / prefix
        result = [prefix]
        if prefixed_path.is_dir():
            for child in prefixed_path.iterdir():
                result.extend(self.list_paths(child.relative_to(path)))
        return result

    @staticmethod
    def generate_random_name(base=""):
        name = (
            base
            + "SmokeTest_"
            + "".join(
                random.choice(string.ascii_uppercase + string.digits) for _ in range(12)
            )
        )
        logger.debug(f"Generated name: {name}")
        return name

    @staticmethod
    def generate_id():
        repo_id = uuid.uuid4().hex
        return repo_id

    def _fix_path(self, file: str | Path) -> str:
        if os.path.isabs(file):
            return str(file)
        else:
            import sys

            full_path = os.path.join(self.path, file)
            if sys.platform == "win32" and not full_path.startswith("\\\\?\\"):
                full_path = "\\\\?\\" + os.path.abspath(full_path)
            return full_path

    def _fix_paths(
        self, files: list[str] | str | list[Path] | Path | None
    ) -> list[str]:
        if files is None:
            result = [self.path]
        elif isinstance(files, (str, Path)):
            result = [self._fix_path(files)]
        elif isinstance(files, list):
            result = [self._fix_path(file) for file in files]
        else:
            raise TypeError("files should be a filename or a list of filenames")
        return result


class GlobalOptionsParseable(TypedDict, total=False):
    path: str
    cwd: str
    use_os_dir: bool
    check: bool
    level: str
    force: bool
    dry_run: bool
    no_pager: bool
    offline: bool
    remote: bool
    local: bool
    identity: str
    max_connections: int
    file_count_limit: int
    file_size_limit: int
    compress_limit: int
    search_limit: int
    search_nearest: bool
    no_gc: bool


class GlobalOptions(TypedDict, total=False):
    path: str
    cwd: str
    use_os_dir: bool
    check: bool
    level: str
    debug: bool
    force: bool
    dry_run: bool
    json: bool
    no_pager: bool
    offline: bool
    remote: bool
    local: bool
    identity: str
    max_connections: int
    file_count_limit: int
    file_size_limit: int
    compress_limit: int
    search_limit: int
    search_nearest: bool
    no_gc: bool
    non_interactive: bool
