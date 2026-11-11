# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Minimal ctypes bindings for the public Lore C API (`liblore` / `lore.h`).

This is the same surface the SDK bindings are built on, so a test driving it
observes exactly the API-level behavior an SDK consumer sees — including
return codes for calls whose errors the CLI's human-oriented output layer
never surfaces.

Run as a script, this module is the driver a test invokes as a subprocess:

    python lore_ffi.py auth-user-info <library-path> <repository-path> [user-id...]
    python lore_ffi.py branch-latest-list <library-path> <repository-path> <keep-store-alive-seconds>
    python lore_ffi.py service-start <library-path>
    python lore_ffi.py service-stop <library-path>
    python lore_ffi.py service-status <library-path>
    python lore_ffi.py repository-delete <library-path> <repository-path> <repository-url>
    python lore_ffi.py revision-sync <library-path> <repository-path> <view-file>
    python lore_ffi.py revision-bisect <library-path> <repository-path> <start> <end> <keep-store-alive-seconds>
    python lore_ffi.py file-audit <library-path> <repository-path> <path>
                                  [--stop-after <n>]
    python lore_ffi.py file-metadata-set <library-path> <repository-path> <key> <value-file> <path>...

exiting with the call's FFI code. Tests go through `Lore`'s `*_capi` methods
rather than importing `LoreLibrary` directly:
loading the library into the pytest process would leak its global state
(connection and authz caches, the tokio runtime, a panic hook) across every
test sharing that xdist worker, let a panic in the library take the worker
down with it, and force environment setup through the worker's own `os.environ`.
Importing this module for its constants is safe — nothing loads the library
until `LoreLibrary` is constructed.

Only the types needed by the tests are bound. Struct layouts mirror the
cbindgen-generated `lore.h` next to the built library; the synchronous entry
points return `0` on success or the failing error's FFI code (see
lore-base/src/error.rs for the code registry).
"""

import ctypes
import json
import os
import re
import sys
from ctypes import (
    CFUNCTYPE,
    POINTER,
    Structure,
    Union,
    c_char_p,
    c_int,
    c_int32,
    c_size_t,
    c_uint8,
    c_uint32,
    c_uint64,
    c_void_p,
)
from pathlib import Path

# FFI codes from lore-base/src/error.rs (`#[ffi_code(...)]`), which the
# header does not export as constants. That module allocates codes in blocks
# by error group: 16-27 is authentication and authorization, 3-15 is input and
# validation, 28-39 is connectivity and availability, 80-95 is not-found.
NOT_AUTHENTICATED = 16
NOT_SUPPORTED = 9
OVERSIZED = 118
DISCONNECTED = 28
NOT_CONNECTED = 29
FILE_NOT_FOUND = 82

# The generated header the structs below mirror, checked against them by
# test_lore_ffi.py. Relative to this file so it resolves wherever the tests run.
HEADER_PATH = Path(__file__).parents[2] / "lore-capi" / "lore.h"


def library_filename() -> str:
    if sys.platform == "win32":
        return "lore.dll"
    if sys.platform == "darwin":
        return "liblore.dylib"
    return "liblore.so"


class LoreString(Structure):
    """`lore_string_t`: pointer + length, no terminator requirement."""

    _fields_ = [("string", c_char_p), ("length", c_size_t)]


class LoreStringArray(Structure):
    """`lore_string_array_t`: pointer to first element + count."""

    _fields_ = [("ptr", POINTER(LoreString)), ("count", c_size_t)]


class LoreGlobalArgs(Structure):
    """`lore_global_args_t`. Field order and types must match lore.h."""

    _fields_ = [
        ("repository_path", LoreString),
        ("working_directory", LoreString),
        ("correlation_id", LoreString),
        ("identity", LoreString),
        ("force", c_uint8),
        ("offline", c_uint8),
        ("local", c_uint8),
        ("remote", c_uint8),
        ("dry_run", c_uint8),
        ("max_connections", c_uint32),
        ("search_limit", c_uint32),
        ("search_nearest", c_uint8),
        ("no_gc", c_uint8),
        ("in_memory", c_uint8),
        ("file_count_limit", c_uint64),
        ("file_size_limit", c_uint64),
        ("compress_task_limit", c_uint64),
        ("store_keep_alive", c_uint8),
        ("store_keep_alive_seconds", c_uint64),
        ("sync_data", c_uint8),
        ("cache", c_uint8),
        ("identity_token", LoreString),
        ("access_token", LoreString),
        ("stats", c_uint32),
        ("event_interval_ms", c_uint64),
        ("auth_mode", c_int),
        ("trust_remote", c_uint8),
        ("fast_flush", c_uint8),
    ]


class LoreUint32Array(Structure):
    """`lore_uint32_array_t`: pointer to first element + count."""

    _fields_ = [("ptr", POINTER(c_uint32)), ("count", c_size_t)]


class LoreMetadataTypeArray(Structure):
    """`lore_metadata_type_array_t`. `lore_metadata_type_t` is a C enum, so each
    element is an `int`."""

    _fields_ = [("ptr", POINTER(c_int)), ("count", c_size_t)]


class LoreEventCallbackConfig(Structure):
    """`lore_event_callback_config_t`. A null `func` receives no events."""

    _fields_ = [("user_context", c_uint64), ("func", c_void_p)]


class LoreAuthUserInfoArgs(Structure):
    """`lore_auth_user_info_args_t`."""

    _fields_ = [("user_ids", LoreStringArray)]


class LoreBranchLatestListArgs(Structure):
    """`lore_branch_latest_list_args_t`. An empty `branch` lists the current
    branch, a zero `limit` the default number of entries."""

    _fields_ = [("branch", LoreString), ("limit", c_uint32)]


class LoreServiceStartArgs(Structure):
    """`lore_service_start_args_t`. Carries no arguments of its own.

    cbindgen gives a field-less struct an `int _unused;`, so the mirror has one
    too and the layout check compares like with like.
    """

    _fields_ = [("_unused", c_int)]


class LoreServiceStopArgs(Structure):
    """`lore_service_stop_args_t`. Carries no arguments of its own."""

    _fields_ = [("_unused", c_int)]


class LoreServiceStatusArgs(Structure):
    """`lore_service_status_args_t`. Carries no arguments of its own."""

    _fields_ = [("_unused", c_int)]


class LoreRepositoryDeleteArgs(Structure):
    """`lore_repository_delete_args_t`."""

    _fields_ = [("repository_url", LoreString)]


class LoreRevisionSyncArgs(Structure):
    """`lore_revision_sync_args_t`. `view` names the view filter file the
    working tree is left materialized under, empty to keep the instance's own."""

    _fields_ = [
        ("revision", LoreString),
        ("forward_changes", c_uint8),
        ("reset", c_uint8),
        ("root_files", LoreStringArray),
        ("dependency_tags", LoreStringArray),
        ("dependency_recursive", c_uint8),
        ("dependency_depth_limit", c_uint32),
        ("view", LoreString),
    ]


class LoreRevisionBisectArgs(Structure):
    """`lore_revision_bisect_args_t`. `start` is the latest revision known not to
    hold the change, `end` the earliest known to hold it."""

    _fields_ = [("start", LoreString), ("end", LoreString)]


class LoreHash(Structure):
    """`lore_hash_t`: the raw 32 bytes of a hash digest."""

    _fields_ = [("data", c_uint8 * 32)]

    def hex(self) -> str:
        return bytes(self.data).hex()


class LorePartition(Structure):
    """`lore_partition_t`, which `lore_repository_id_t` aliases."""

    _fields_ = [("data", c_uint8 * 16)]

    def hex(self) -> str:
        return bytes(self.data).hex()


class LoreTraceLocationArray(Structure):
    """`lore_trace_location_array_t`. The locations themselves are not read
    here, so the pointer is left opaque."""

    _fields_ = [("ptr", c_void_p), ("count", c_size_t)]


class LoreErrorDetail(Structure):
    """`lore_error_detail_t`: the failure's own FFI code, message and trace."""

    _fields_ = [
        ("error_code", c_int32),
        ("message", LoreString),
        ("trace_locations", LoreTraceLocationArray),
    ]


class LoreCompleteEventData(Structure):
    """`lore_complete_event_data_t`, the one event a terminal failure is
    reported on (ADR-00017)."""

    _fields_ = [("status", c_int32), ("error", LoreErrorDetail)]


class LoreFileAuditArgs(Structure):
    """`lore_file_audit_args_t`. `max_revisions` of `UINT32_MAX` asks for the
    whole chain and `0` means the default of 100, so a zero-initialized struct
    is a bounded walk."""

    _fields_ = [
        ("path", LoreString),
        ("revision", LoreString),
        ("max_revisions", c_uint32),
        ("no_ignore_space_at_eol", c_uint8),
        ("ignore_space_change", c_uint8),
    ]


class LoreFileMetadataSetArgs(Structure):
    """`lore_file_metadata_set_args_t`. `entries` holds, per path, how many of
    the keys, values and formats that follow in order are set on it."""

    _fields_ = [
        ("paths", LoreStringArray),
        ("keys", LoreStringArray),
        ("values", LoreStringArray),
        ("formats", LoreMetadataTypeArray),
        ("entries", LoreUint32Array),
    ]


class LoreFileAuditEventData(Structure):
    """`lore_file_audit_event_data_t`: one range of lines and what it resolved
    to. `outcome` is a C enum, so it is `int`."""

    _fields_ = [
        ("repository", LorePartition),
        ("revision", LoreHash),
        ("line_first", c_uint32),
        ("line_last", c_uint32),
        ("outcome", c_int),
    ]


class LoreEventData(Union):
    """The payload union of `lore_event_t`.

    Only the members this module reads are declared. Every member of a C union
    starts at the same offset, and the offset the union itself sits at follows
    the widest alignment among its members, which is a pointer's whether the
    union holds two members or two hundred. So the members below sit where the
    library wrote them, and the size difference does not matter: the library
    allocates the event and this module only reads it.
    """

    _fields_ = [
        ("complete", LoreCompleteEventData),
        ("file_audit", LoreFileAuditEventData),
    ]


class LoreEvent(Structure):
    """`lore_event_t`: the tag naming the event, then its payload."""

    _fields_ = [("tag", c_uint32), ("data", LoreEventData)]


# `void (*)(const lore_event_t *event, uint64_t user_context)`, the callback
# `lore_event_callback_config_t::func` holds.
EVENT_CALLBACK = CFUNCTYPE(None, POINTER(LoreEvent), c_uint64)

# Every struct above, paired with the header type it mirrors. A struct bound
# here belongs in this list: it is what test_lore_ffi.py checks the mirrors
# against, so a field added to the C API is reported as a named mismatch rather
# than read past the end of an allocation at the next call.
#
# A struct with an array field is left out: `header_struct_fields` reads a field
# name from the end of its declaration and an array's ends in `]`, so
# `lore_hash_t` and `lore_partition_t` cannot be compared this way. Both are
# fixed-width byte buffers that no change to a verb can widen.
MIRRORED_STRUCTS = [
    ("lore_string_t", LoreString),
    ("lore_string_array_t", LoreStringArray),
    ("lore_global_args_t", LoreGlobalArgs),
    ("lore_event_callback_config_t", LoreEventCallbackConfig),
    ("lore_auth_user_info_args_t", LoreAuthUserInfoArgs),
    ("lore_branch_latest_list_args_t", LoreBranchLatestListArgs),
    ("lore_service_start_args_t", LoreServiceStartArgs),
    ("lore_service_stop_args_t", LoreServiceStopArgs),
    ("lore_service_status_args_t", LoreServiceStatusArgs),
    ("lore_repository_delete_args_t", LoreRepositoryDeleteArgs),
    ("lore_revision_sync_args_t", LoreRevisionSyncArgs),
    ("lore_revision_bisect_args_t", LoreRevisionBisectArgs),
    ("lore_trace_location_array_t", LoreTraceLocationArray),
    ("lore_error_detail_t", LoreErrorDetail),
    ("lore_complete_event_data_t", LoreCompleteEventData),
    ("lore_file_audit_args_t", LoreFileAuditArgs),
    ("lore_file_audit_event_data_t", LoreFileAuditEventData),
    ("lore_uint32_array_t", LoreUint32Array),
    ("lore_metadata_type_array_t", LoreMetadataTypeArray),
    ("lore_file_metadata_set_args_t", LoreFileMetadataSetArgs),
]

# One field per line, either a function pointer (`void (*func)(...)`) or a plain
# declaration ending in the field name (`uint8_t force;`).
_HEADER_FIELD = re.compile(r"\(\*(?P<pointer>\w+)\)|(?P<plain>\w+)\s*;$")


def header_struct_fields(struct_name: str) -> list[str]:
    """The field names of `struct_name` in `lore.h`, in declaration order.

    Reading the header rather than restating it keeps the mirrors below honest:
    they are hand-written, and a field added to the C API is invisible to them
    until something dereferences the memory past their end.
    """
    header = HEADER_PATH.read_text()
    body = re.search(
        rf"typedef struct {struct_name} {{(.*?)\n}} {struct_name};", header, re.S
    )
    if body is None:
        raise LookupError(f"{struct_name} is not declared in {HEADER_PATH}")

    fields = []
    for line in body.group(1).splitlines():
        line = line.strip()
        if not line or line.startswith("//"):
            continue
        field = _HEADER_FIELD.search(line)
        if field is None:
            raise ValueError(f"cannot read a field name from {struct_name}: {line}")
        fields.append(field.group("pointer") or field.group("plain"))
    return fields


def header_enum_value(enum_name: str, enumerator: str) -> int:
    """The value of `enumerator` in `enum_name` as `lore.h` declares it.

    The event tags are not exported as constants and carry no explicit values,
    so a tag's value is its position in the enum — which moves whenever a tag is
    added ahead of it. Reading it from the header is what keeps a driver reading
    the member the library actually wrote.
    """
    header = HEADER_PATH.read_text()
    body = re.search(rf"enum {enum_name} \{{(.*?)\n\}}", header, re.S)
    if body is None:
        raise LookupError(f"{enum_name} is not declared in {HEADER_PATH}")

    value = 0
    for line in body.group(1).splitlines():
        line = line.strip().rstrip(",")
        if not line or line.startswith("//"):
            continue
        name, _, assigned = (part.strip() for part in line.partition("="))
        if assigned:
            value = int(assigned, 0)
        if name == enumerator:
            return value
        value += 1
    raise LookupError(f"{enum_name} declares no {enumerator}")


def audit_record(data: LoreFileAuditEventData) -> dict:
    """One `lore_file_audit_event_data_t` as a plain dict, so the driver can
    hand it to a test as a JSON line.

    The event is valid only while the callback runs, so every field is copied
    out here rather than referenced.
    """
    return {
        "repository": data.repository.hex(),
        "revision": data.revision.hex(),
        "lineFirst": data.line_first,
        "lineLast": data.line_last,
        "outcome": data.outcome,
    }


class LoreLibrary:
    """A loaded `liblore` with the bound entry points."""

    def __init__(self, library_path: str | Path):
        self._lib = ctypes.CDLL(str(library_path))
        self._lib.lore_auth_user_info.restype = c_int32
        self._lib.lore_auth_user_info.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreAuthUserInfoArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_branch_latest_list.restype = c_int32
        self._lib.lore_branch_latest_list.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreBranchLatestListArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_service_start.restype = c_int32
        self._lib.lore_service_start.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreServiceStartArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_service_stop.restype = c_int32
        self._lib.lore_service_stop.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreServiceStopArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_service_status.restype = c_int32
        self._lib.lore_service_status.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreServiceStatusArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_repository_delete.restype = c_int32
        self._lib.lore_repository_delete.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreRepositoryDeleteArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_revision_sync.restype = c_int32
        self._lib.lore_revision_sync.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreRevisionSyncArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_revision_bisect.restype = c_int32
        self._lib.lore_revision_bisect.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreRevisionBisectArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_file_audit.restype = c_int32
        self._lib.lore_file_audit.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreFileAuditArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_file_metadata_set.restype = c_int32
        self._lib.lore_file_metadata_set.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreFileMetadataSetArgs),
            LoreEventCallbackConfig,
        ]

    def auth_user_info(self, repository_path: str, user_ids: list[str]) -> int:
        """Call `lore_auth_user_info` (the SDK's `authUserInfo`) without an
        event callback and return its FFI code: 0 on success, the failing
        error's code otherwise."""
        # Encoded buffers must outlive the call; keep references on the stack.
        path_bytes = repository_path.encode()
        id_bytes = [user_id.encode() for user_id in user_ids]

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))

        ids = (LoreString * len(id_bytes))(
            *(LoreString(encoded, len(encoded)) for encoded in id_bytes)
        )
        args = LoreAuthUserInfoArgs(LoreStringArray(ids, len(id_bytes)))

        no_callback = LoreEventCallbackConfig(0, None)
        return self._lib.lore_auth_user_info(
            ctypes.byref(globals_args), ctypes.byref(args), no_callback
        )

    def branch_latest_list(
            self, repository_path: str, keep_store_alive_seconds: int
    ) -> int:
        """Call `lore_branch_latest_list` for the current branch, returning its
        FFI code. The stores stay open for `keep_store_alive_seconds` after the
        call, none if `0`."""
        path_bytes = repository_path.encode()

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))
        globals_args.store_keep_alive = keep_store_alive_seconds != 0
        globals_args.store_keep_alive_seconds = keep_store_alive_seconds

        return self._lib.lore_branch_latest_list(
            ctypes.byref(globals_args),
            ctypes.byref(LoreBranchLatestListArgs()),
            LoreEventCallbackConfig(0, None),
        )

    def service_start(self) -> int:
        """Call `lore_service_start`, returning its FFI code.

        No repository: a service serves whichever ones its callers name, so
        starting one is not about any of them.
        """
        return self._lib.lore_service_start(
            ctypes.byref(LoreGlobalArgs()),
            ctypes.byref(LoreServiceStartArgs()),
            LoreEventCallbackConfig(0, None),
        )

    def service_stop(self) -> int:
        """Call `lore_service_stop`, returning its FFI code.

        `0` whether or not one was running: a stop asks for none to be, and none
        running is that state.
        """
        return self._lib.lore_service_stop(
            ctypes.byref(LoreGlobalArgs()),
            ctypes.byref(LoreServiceStopArgs()),
            LoreEventCallbackConfig(0, None),
        )

    def service_status(self) -> int:
        """Call `lore_service_status`, returning its FFI code.
        `0` whether or not one is running: nothing listening is an answer to
        what was asked rather than a failure to reach anything.
        """
        return self._lib.lore_service_status(
            ctypes.byref(LoreGlobalArgs()),
            ctypes.byref(LoreServiceStatusArgs()),
            LoreEventCallbackConfig(0, None),
        )

    def repository_delete(self, repository_path: str, repository_url: str) -> int:
        """Call `lore_repository_delete` for `repository_url`, returning its FFI
        code."""
        # Encoded buffers must outlive the call; keep references on the stack.
        path_bytes = repository_path.encode()
        url_bytes = repository_url.encode()

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))

        args = LoreRepositoryDeleteArgs(LoreString(url_bytes, len(url_bytes)))

        return self._lib.lore_repository_delete(
            ctypes.byref(globals_args),
            ctypes.byref(args),
            LoreEventCallbackConfig(0, None),
        )

    def revision_sync(self, repository_path: str, view: str) -> int:
        """Call `lore_revision_sync` with `view` and nothing else set, returning
        its FFI code.
        The entry point an SDK consumer reaches a view change through. `view`
        empty is the call every consumer that does not want one makes, and has
        to leave the instance's own view standing.
        """
        # Encoded buffers must outlive the call; keep references on the stack.
        path_bytes = repository_path.encode()
        view_bytes = view.encode()

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))

        args = LoreRevisionSyncArgs()
        args.view = LoreString(view_bytes, len(view_bytes))

        return self._lib.lore_revision_sync(
            ctypes.byref(globals_args),
            ctypes.byref(args),
            LoreEventCallbackConfig(0, None),
        )

    def revision_bisect(
            self, repository_path: str, start: str, end: str, keep_store_alive_seconds: int
    ) -> int:
        """Call `lore_revision_bisect` over the range from `start` to `end`,
        returning its FFI code. The stores stay open for
        `keep_store_alive_seconds` after the call, none if `0`."""
        # Encoded buffers must outlive the call; keep references on the stack.
        path_bytes = repository_path.encode()
        start_bytes = start.encode()
        end_bytes = end.encode()

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))
        globals_args.store_keep_alive = keep_store_alive_seconds != 0
        globals_args.store_keep_alive_seconds = keep_store_alive_seconds

        args = LoreRevisionBisectArgs(
            LoreString(start_bytes, len(start_bytes)),
            LoreString(end_bytes, len(end_bytes)),
        )

        return self._lib.lore_revision_bisect(
            ctypes.byref(globals_args),
            ctypes.byref(args),
            LoreEventCallbackConfig(0, None),
        )

    def file_audit(
        self,
        repository_path: str,
        path: str,
        stop_after: int | None = None,
    ) -> int:
        """Call `lore_file_audit`, printing one JSON line per event the callback
        received and returning the call's FFI code.

        This is the entry point an editor integration reaches attribution
        through, so the records it prints are exactly the structured data such a
        consumer reads — no terminal output is parsed anywhere in the path.

        `stop_after` stops the process from inside the callback once that many
        range events have arrived, which is a caller that took what it needed and
        walked away: the records it already has stand, and nothing waits for the
        walk to reach the end of the chain.
        """
        # Encoded buffers must outlive the call; keep references on the stack.
        path_bytes = repository_path.encode()
        audited_bytes = path.encode()

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))

        args = LoreFileAuditArgs()
        args.path = LoreString(audited_bytes, len(audited_bytes))

        file_audit_tag = header_enum_value("lore_event_id_t", "LORE_EVENT_FILE_AUDIT")
        complete_tag = header_enum_value("lore_event_id_t", "LORE_EVENT_COMPLETE")
        ranges = 0

        def on_event(event, _user_context):
            nonlocal ranges
            if event[0].tag == file_audit_tag:
                print(json.dumps(audit_record(event[0].data.file_audit)), flush=True)
                ranges += 1
                if stop_after is not None and ranges >= stop_after:
                    # Not a graceful return: the point is that the caller is gone
                    # before the walk finished, so nothing after this line runs.
                    sys.stdout.flush()
                    os._exit(0)
            elif event[0].tag == complete_tag:
                complete = event[0].data.complete
                print(
                    json.dumps(
                        {
                            "complete": complete.status,
                            "errorCode": complete.error.error_code,
                        }
                    ),
                    flush=True,
                )

        # The callback must outlive the call, so it is held in a local.
        callback = EVENT_CALLBACK(on_event)
        return self._lib.lore_file_audit(
            ctypes.byref(globals_args),
            ctypes.byref(args),
            LoreEventCallbackConfig(0, ctypes.cast(callback, c_void_p).value),
        )

    def file_metadata_set(
        self, repository_path: str, key: str, value: str, paths: list[str]
    ) -> int:
        """Call `lore_file_metadata_set` with one string entry, `key` set to
        `value`, on every path in one call, returning its FFI code."""
        # Encoded buffers must outlive the call; keep references on the stack.
        repository_bytes = repository_path.encode()
        key_bytes = key.encode()
        value_bytes = value.encode()
        path_bytes = [path.encode() for path in paths]
        count = len(path_bytes)

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(
            repository_bytes, len(repository_bytes)
        )

        path_strings = (LoreString * count)(
            *(LoreString(encoded, len(encoded)) for encoded in path_bytes)
        )
        keys = (LoreString * count)(*([LoreString(key_bytes, len(key_bytes))] * count))
        values = (LoreString * count)(
            *([LoreString(value_bytes, len(value_bytes))] * count)
        )
        string_type = header_enum_value(
            "lore_metadata_type_t", "LORE_METADATA_TYPE_STRING"
        )
        formats = (c_int * count)(*([string_type] * count))
        entries = (c_uint32 * count)(*([1] * count))

        args = LoreFileMetadataSetArgs(
            LoreStringArray(path_strings, count),
            LoreStringArray(keys, count),
            LoreStringArray(values, count),
            LoreMetadataTypeArray(formats, count),
            LoreUint32Array(entries, count),
        )

        return self._lib.lore_file_metadata_set(
            ctypes.byref(globals_args),
            ctypes.byref(args),
            LoreEventCallbackConfig(0, None),
        )


USAGE = """usage:
  lore_ffi.py auth-user-info <library-path> <repository-path> [user-id...]
  lore_ffi.py branch-latest-list <library-path> <repository-path> <keep-store-alive-seconds>
  lore_ffi.py service-start <library-path>
  lore_ffi.py service-stop <library-path>
  lore_ffi.py service-status <library-path>
  lore_ffi.py revision-sync <library-path> <repository-path> <view-file>
  lore_ffi.py repository-delete <library-path> <repository-path> <repository-url>
  lore_ffi.py revision-sync <library-path> <repository-path> <view-file>
  lore_ffi.py revision-bisect <library-path> <repository-path> <start> <end> <keep-store-alive-seconds>
  lore_ffi.py file-audit <library-path> <repository-path> <path>
                         [--stop-after <n>]
  lore_ffi.py file-metadata-set <library-path> <repository-path> <key> <value-file> <path>..."""


def main(argv: list[str]) -> int:
    match argv:
        case ["auth-user-info", library_path, repository_path, *user_ids]:
            return LoreLibrary(library_path).auth_user_info(repository_path, user_ids)
        case ["branch-latest-list", library_path, repository_path, keep_alive]:
            return LoreLibrary(library_path).branch_latest_list(
                repository_path, int(keep_alive)
            )
        case ["service-start", library_path]:
            return LoreLibrary(library_path).service_start()
        case ["service-stop", library_path]:
            return LoreLibrary(library_path).service_stop()
        case ["service-status", library_path]:
            return LoreLibrary(library_path).service_status()
        case ["repository-delete", library_path, repository_path, repository_url]:
            return LoreLibrary(library_path).repository_delete(
                repository_path, repository_url
            )
        case ["revision-sync", library_path, repository_path, view]:
            return LoreLibrary(library_path).revision_sync(repository_path, view)
        case ["revision-bisect", library_path, repository_path, start, end, keep_alive]:
            return LoreLibrary(library_path).revision_bisect(
                repository_path, start, end, int(keep_alive)
            )
        case ["file-audit", library_path, repository_path, path, *options]:
            stop_after = None
            while options:
                match options:
                    case ["--stop-after", count, *rest]:
                        stop_after = int(count)
                        options = rest
                    case _:
                        print(USAGE, file=sys.stderr)
                        return 2
            return LoreLibrary(library_path).file_audit(
                repository_path, path, stop_after
            )
        case [
            "file-metadata-set",
            library_path,
            repository_path,
            key,
            value_file,
            *paths,
        ]:
            return LoreLibrary(library_path).file_metadata_set(
                repository_path, key, Path(value_file).read_text(), paths
            )
        case _:
            print(USAGE, file=sys.stderr)
            return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
