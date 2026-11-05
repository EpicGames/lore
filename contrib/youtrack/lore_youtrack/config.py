# Copyright 2026 RedRatInHat
# SPDX-License-Identifier: MIT
"""Operator-owned configuration; credentials never enter the delivery database."""

from __future__ import annotations

import os
import re
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


class ConfigError(ValueError):
    """An invalid configuration that can be reported without exposing secrets."""


@dataclass(frozen=True)
class Secret:
    env: str | None = None
    file: Path | None = None

    def read(self) -> str:
        """Read an environment variable or a bounded, UTF-8 secret file."""
        if self.env is not None:
            value = os.environ.get(self.env, "")
        else:
            try:
                if self.file is None:
                    raise ConfigError("A secret source is required")
                with self.file.open("rb") as stream:
                    raw = stream.read(8193)
                if len(raw) > 8192:
                    raise ConfigError("Secret file exceeds 8192 bytes")
                value = raw.decode("utf-8").removesuffix("\n").removesuffix("\r")
            except (OSError, UnicodeError) as error:
                raise ConfigError("Cannot read the configured secret file") from error
        if (
            not value
            or not value.strip()
            or any(ord(c) < 32 or ord(c) > 126 for c in value)
        ):
            raise ConfigError(
                "A configured secret is absent or is not a printable HTTP header"
            )
        return value


@dataclass(frozen=True)
class Config:
    repository: Path
    database: Path
    receiver_url: str
    signature: Secret
    authors: dict[str, dict[str, str]]
    lore_binary: str = "lore"
    branches: tuple[str, ...] = ("main",)
    poll_seconds: float = 30.0
    lore_timeout: float = 60.0
    request_timeout: float = 15.0
    scan_limit: int = 1000
    send_limit: int = 100
    allow_http_loopback: bool = False
    commit_url_template: str | None = None
    read_token: Secret | None = None
    project_keys: tuple[str, ...] = ()


def _string(table: dict[str, Any], key: str, default: str | None = None) -> str:
    value = table.get(key, default)
    if not isinstance(value, str) or not value.strip() or "\x00" in value:
        raise ConfigError(f"{key} must be a nonempty string")
    return value


def _number(
    table: dict[str, Any], key: str, default: float, maximum: float
) -> int | float:
    value = table.get(key, default)
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not 0 < value <= maximum
    ):
        raise ConfigError(f"{key} must be positive and at most {maximum:g}")
    if isinstance(default, int) and not isinstance(value, int):
        raise ConfigError(f"{key} must be an integer")
    return value


def _strings(table: dict[str, Any], key: str, default: list[str]) -> tuple[str, ...]:
    values = table.get(key, default)
    if not isinstance(values, list) or any(
        not isinstance(v, str) or not v for v in values
    ):
        raise ConfigError(f"{key} must be an array of nonempty strings")
    return tuple(values)


def _outside(path: Path, repository: Path, name: str) -> None:
    if path.is_relative_to(repository):
        raise ConfigError(f"{name} must be outside the monitored Lore repository")


def _secret(
    table: dict[str, Any], prefix: str, base: Path, repository: Path, required: bool
) -> Secret | None:
    env, file = table.get(f"{prefix}_env"), table.get(f"{prefix}_file")
    if env is not None and file is not None:
        raise ConfigError(f"Set only one of {prefix}_env and {prefix}_file")
    if env is not None:
        if not isinstance(env, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", env):
            raise ConfigError(f"{prefix}_env is not an environment variable name")
        return Secret(env=env)
    if file is not None:
        path = (base / _string(table, f"{prefix}_file")).resolve()
        _outside(path, repository, f"{prefix}_file")
        return Secret(file=path)
    if required:
        raise ConfigError(f"Set {prefix}_env or {prefix}_file")
    return None


def load_config(path: str | Path) -> Config:
    """Load strict TOML from an administrator-controlled file."""
    path = Path(path).resolve()
    try:
        with path.open("rb") as stream:
            data = tomllib.load(stream)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ConfigError("Cannot read valid TOML configuration") from error
    if set(data) - {"bridge", "youtrack", "authors"}:
        raise ConfigError("Unknown configuration section")
    bridge, youtrack, authors = (
        data.get(key, {}) for key in ("bridge", "youtrack", "authors")
    )
    if not all(isinstance(t, dict) for t in (bridge, youtrack, authors)):
        raise ConfigError("bridge, youtrack and authors must be TOML tables")
    if set(bridge) - {
        "repository",
        "database",
        "lore_binary",
        "branches",
        "poll_seconds",
        "lore_timeout",
        "scan_limit",
        "send_limit",
    }:
        raise ConfigError("Unknown bridge setting")
    if set(youtrack) - {
        "receiver_url",
        "signature_env",
        "signature_file",
        "request_timeout",
        "allow_http_loopback",
        "commit_url_template",
        "read_token_env",
        "read_token_file",
        "project_keys",
    }:
        raise ConfigError("Unknown YouTrack setting")
    repository = (path.parent / _string(bridge, "repository")).resolve()
    database = (path.parent / _string(bridge, "database")).resolve()
    _outside(path, repository, "Configuration")
    _outside(database, repository, "Database")
    signature = _secret(youtrack, "signature", path.parent, repository, True)
    if signature is None:
        raise ConfigError("A signature secret is required")
    read_token = _secret(youtrack, "read_token", path.parent, repository, False)
    template = youtrack.get("commit_url_template")
    if template is not None and (
        not isinstance(template, str) or template.count("$hash$") != 1
    ):
        raise ConfigError(
            "commit_url_template must contain exactly one $hash$ placeholder"
        )
    if read_token is not None and template is None:
        raise ConfigError("REST reconciliation requires commit_url_template")
    branches = _strings(bridge, "branches", ["main"])
    if not branches:
        raise ConfigError("At least one branch pattern is required")
    project_keys = _strings(youtrack, "project_keys", [])
    if any(not re.fullmatch(r"[A-Za-z][A-Za-z0-9_]*", key) for key in project_keys):
        raise ConfigError("project_keys must contain issue project prefixes")
    identities: dict[str, dict[str, str]] = {}
    for identity, mapping in authors.items():
        if (
            not identity
            or not isinstance(mapping, dict)
            or set(mapping) - {"user_name", "email"}
        ):
            raise ConfigError("Each author must map to user_name and/or email")
        mapped = {
            ("userName" if key == "user_name" else "email"): _string(mapping, key)
            for key in mapping
        }
        if not mapped:
            raise ConfigError("Each author needs at least one YouTrack identity field")
        identities[identity] = mapped
    loopback = youtrack.get("allow_http_loopback", False)
    if not isinstance(loopback, bool):
        raise ConfigError("allow_http_loopback must be a boolean")
    return Config(
        repository=repository,
        database=database,
        receiver_url=_string(youtrack, "receiver_url"),
        signature=signature,
        authors=identities,
        lore_binary=_string(bridge, "lore_binary", "lore"),
        branches=branches,
        poll_seconds=float(_number(bridge, "poll_seconds", 30.0, 86400)),
        lore_timeout=float(_number(bridge, "lore_timeout", 60.0, 3600)),
        request_timeout=float(_number(youtrack, "request_timeout", 15.0, 300)),
        scan_limit=int(_number(bridge, "scan_limit", 1000, 100000)),
        send_limit=int(_number(bridge, "send_limit", 100, 10000)),
        allow_http_loopback=loopback,
        commit_url_template=template,
        read_token=read_token,
        project_keys=project_keys,
    )
