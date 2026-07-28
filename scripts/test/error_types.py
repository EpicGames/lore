# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import re
from subprocess import CalledProcessError


class ServerException(Exception): ...


class LoreException(Exception): ...


class ServerConnectionError(LoreException): ...


class UnknownLoreError(LoreException): ...


class CommitFailed(LoreException): ...


class MergeRequired(LoreException): ...


class RepositoryAlreadyExistsError(LoreException): ...


class UninitializedRepositoryError(LoreException): ...


class ImproperArgumentsError(LoreException): ...


class NotFound(LoreException): ...


class RevisionNotFound(NotFound): ...


class BranchBehindLatest(LoreException): ...


class CaseMismatch(LoreException): ...


class CaseVariantConflict(LoreException): ...


class LocalChanges(LoreException): ...


class LocalModificationsError(LocalChanges):
    """Tracked files have been locally modified and the operation refuses to
    proceed without `--force`. Subclass of `LocalChanges` so existing call
    sites that catch `LocalChanges` keep working."""


class FileAlreadyExist(LoreException): ...


class LockInvalidPath(LoreException): ...


class UserNotAuthenticated(LoreException): ...


class LockQueryFailed(LoreException): ...


class InvalidBranch(LoreException): ...


class BranchError(LoreException): ...


class ZeroRevisionError(BranchError): ...


class DeleteCurrentError(BranchError): ...


class DeleteDefaultError(BranchError): ...


class DeleteProtectedError(BranchError): ...


class ProtectedError(BranchError): ...


class BranchDivergedError(BranchError): ...


class BranchAlreadyExistsError(BranchError): ...


class BisectError(LoreException): ...


class BisectDivergentRevisions(BisectError): ...


class NotInProgress(LoreException): ...


class ServiceCallError(LoreException): ...


class ExistingSharedStore(LoreException): ...


class PathExistChildrenLinkError(LoreException): ...


class NestedLinkError(LoreException): ...


class PathExistLinkError(LoreException): ...


class MissingSharedStore(LoreException): ...


class InvalidRepositoryPath(LoreException): ...


class WrongSharedStoreRemote(LoreException): ...


class LocalMutableStoreWithSharedStore(LoreException): ...


class BranchAdvanced(LoreException): ...


class NothingStagedError(LoreException): ...


class LinkConflicts(LoreException): ...


class NotALinkError(LoreException): ...


class LinkNotFoundError(LoreException): ...


class NotALayerError(LoreException): ...


class BadSharedStoreRemoteUrl(LoreException): ...


class MissingIdentityError(LoreException):
    """Raised when a commit-producing operation runs without a configured
    identity (no --identity arg, no config.toml identity, no cached auth)."""


class NotSupportedError(LoreException):
    """Raised when an operation is not supported in the current environment,
    e.g. an auth command run against a server with no auth endpoint
    configured."""


ERROR_MAP: list[tuple[str | re.Pattern, type[LoreException]]] = [
    ("Unable to commit", CommitFailed),
    (
        "Target branch to merge into has a newer revision, merge target branch first",
        MergeRequired,
    ),
    ("Failed to connect to repo", ServerConnectionError),
    ("Repository already exist", RepositoryAlreadyExistsError),
    (re.compile(r"error: the argument.*cannot be used with"), ImproperArgumentsError),
    ("expected a non-negative integer", ImproperArgumentsError),
    ("metadata set requires <key> <value> pairs", ImproperArgumentsError),
    ("Repository not found", UninitializedRepositoryError),
    ("Failed to find revision", RevisionNotFound),
    ("revision not found:", RevisionNotFound),
    ("Not found", NotFound),
    ("Branch behind latest revision", BranchBehindLatest),
    ("Multiple files with case-only differences", CaseVariantConflict),
    ("A name case mismatch", CaseMismatch),
    ("File has local changes", LocalChanges),
    ("File already exist", FileAlreadyExist),
    ("Invalid path:", LockInvalidPath),
    # Strict-forward (UCS-20162) preserves the inner typed variant for lock-path
    # validation failures, so the user-facing `[Error] ...` now shows the
    # discrete type's Display instead of the previous contextual collapse.
    ("invalid path:", LockInvalidPath),
    ("Node not found", LockInvalidPath),
    ("Failed to resolve user id", UserNotAuthenticated),
    ("Failed to query lock status", LockQueryFailed),
    ("Invalid branch:", InvalidBranch),
    ("Unable to create a branch without a previous revision", ZeroRevisionError),
    ("Cannot delete the current branch", DeleteCurrentError),
    ("Unable to delete default branch", DeleteDefaultError),
    ("Unable to delete a protected branch", DeleteProtectedError),
    ("Not authorized to access repository", ProtectedError),
    ("Branch has diverged", BranchDivergedError),
    ("already exists, use switch instead", BranchAlreadyExistsError),
    ("Failed to find path between", BisectDivergentRevisions),
    ("No merge is in progress", NotInProgress),
    ("Failed to send command to Lore service because", ServiceCallError),
    ("Found existing shared store at ", ExistingSharedStore),
    ("Link path already has children", PathExistChildrenLinkError),
    ("Link path is already a link", PathExistLinkError),
    ("Nested link", NestedLinkError),
    ("A shared store was supposed to exist at", MissingSharedStore),
    ("Invalid repository path", InvalidRepositoryPath),
    ("Loading the shared store for a repo with remote url", WrongSharedStoreRemote),
    ("has conflicts that must be resolved directly", LinkConflicts),
    (
        "local mutable store but is configured to use a shared store",
        LocalMutableStoreWithSharedStore,
    ),
    ("Branch has been advanced by another instance", BranchAdvanced),
    ("Nothing staged for commit", NothingStagedError),
    ("Path is not a link", NotALinkError),
    ("Link not found", LinkNotFoundError),
    ("Path is not a layer", NotALayerError),
    ("Failed to connect to remote URL", BadSharedStoreRemoteUrl),
    ("Local modifications prevent synchronization", LocalModificationsError),
    ("No commit identity configured", MissingIdentityError),
    ("Operation not supported", NotSupportedError),
]


def get_error_type(e: CalledProcessError) -> type[LoreException]:
    output = (e.stdout or "") + (e.stderr or "")
    for pattern, error_class in ERROR_MAP:
        if isinstance(pattern, re.Pattern):
            if re.search(pattern, output):
                return error_class
        elif pattern in output:
            return error_class
    return UnknownLoreError
