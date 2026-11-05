// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::auth::RepositoryRemote;
use lore::auth::read_repository_remote;
use lore::auth::resolve_auth_endpoint;
use lore_base::error::NoRemote;
use lore_base::error::RepositoryNotFound;
use lore_error_set::FfiError;

/// A repository directory with the given `config.toml` body, or none at all when
/// `config` is `None`. Returns the repository root.
fn repository_with_config(label: &str, config: Option<&str>) -> lore_base::test_util::TempDir {
    let root = lore_base::test_util::TempDir::new(&format!("lore-auth-{label}-"));
    let dot_dir = root.join(lore_revision::repository::DOT_LORE);
    std::fs::create_dir_all(&dot_dir).expect("creating the repository directory");
    if let Some(config) = config {
        std::fs::write(dot_dir.join("config.toml"), config).expect("writing the config");
    }
    root
}

// An explicit endpoint is returned verbatim without touching the
// repository config or the network.
#[tokio::test]
async fn returns_explicit_endpoint_verbatim() {
    let (endpoint, remote) =
        resolve_auth_endpoint("ucs-auth://auth.example.com", "/does/not/exist")
            .await
            .expect("an explicit endpoint should resolve");
    assert_eq!(endpoint, "ucs-auth://auth.example.com");
    assert!(remote.is_none());
}

// Run outside a repository with no explicit endpoint, the answer names the actual
// problem: there is no repository here to read a remote from, and the fix is to run
// this from a repository or pass an endpoint.
#[tokio::test]
async fn missing_repository_is_repository_not_found() {
    let err = resolve_auth_endpoint("", "/does/not/exist")
        .await
        .expect_err("a missing endpoint must be an error");

    let repository_not_found_code = RepositoryNotFound {
        repository: String::new(),
    }
    .ffi_code();
    assert_eq!(err.ffi_code(), repository_not_found_code, "{err:?}");
}

// A repository created without a URL has a remote-less config. Asking it for an auth
// endpoint is `NoRemote`, not `NotSupported` and not a connection failure:
// there is no remote to reach, as opposed to one that could not be reached.
#[tokio::test]
async fn repository_without_a_remote_is_no_remote() {
    let root = repository_with_config("empty-remote", Some("remote_url = \"\"\n"));

    let err = resolve_auth_endpoint("", &root.display().to_string())
        .await
        .expect_err("a repository with no remote must be an error");

    assert_eq!(err.ffi_code(), NoRemote.ffi_code(), "{err:?}");
}

// Same answer when the config omits the key outright rather than writing it empty,
// which is what an older or hand-edited config looks like.
#[tokio::test]
async fn repository_with_no_remote_key_is_no_remote() {
    let root = repository_with_config("absent-remote", Some("identity = \"me\"\n"));

    let err = resolve_auth_endpoint("", &root.display().to_string())
        .await
        .expect_err("a repository with no remote must be an error");

    assert_eq!(err.ffi_code(), NoRemote.ffi_code(), "{err:?}");
}

// A missing config file parses as the default config, so repository presence has to
// come from the tracking directory. Without that check every path in the filesystem
// would report as a remote-less repository and mask the `NotSupported` case above.
#[test]
fn a_path_outside_a_repository_is_not_a_remote_less_repository() {
    assert!(matches!(
        read_repository_remote("/does/not/exist"),
        RepositoryRemote::NoRepository
    ));

    let root = repository_with_config("no-config", None);
    assert!(
        matches!(
            read_repository_remote(&root.display().to_string()),
            RepositoryRemote::NoRemote
        ),
        "a repository whose config is absent still has no remote"
    );
}

#[test]
fn a_configured_remote_is_returned() {
    let root = repository_with_config(
        "with-remote",
        Some("remote_url = \"lore://127.0.0.1:41337\"\n"),
    );

    let remote = read_repository_remote(&root.display().to_string());
    assert!(
        matches!(remote, RepositoryRemote::Remote(ref url) if url == "lore://127.0.0.1:41337"),
        "a configured remote should be reported verbatim"
    );
}
