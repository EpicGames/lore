// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A repository does not need a remote.
//!
//! Creating one without a URL used to be impossible outside `--offline`, and the network
//! commands that followed reported the absent remote as a connection fault. These tests
//! pin both halves of the fix: the create succeeds with no URL and no offline flag, and
//! the network commands that follow answer `NoRemote` rather than `Disconnected` — the
//! difference between "there is nothing to reach" and "it could not be reached".

// Integration test harness: tasks spawned here are scoped to the test
// body and never call back into the lore runtime, so LORE_CONTEXT
// propagation is not required.
#![allow(clippy::disallowed_methods)]

mod test_util;

mod tests {
    use std::sync::Arc;

    use lore::interface::LoreString;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::repository::LoreVfsType;
    use lore_base::error::Disconnected;
    use lore_base::error::InvalidArguments;
    use lore_base::error::NoRemote;
    use lore_error_set::FfiError;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::repository::LoreSharedStoreMode;
    use parking_lot::Mutex;
    use serial_test::serial;

    use super::test_util::TempDir;

    /// The status carried by the terminating `Complete` event, which is where a command
    /// reports its outcome.
    #[derive(Default)]
    struct Outcome {
        status: i32,
        error_code: i32,
    }

    fn callback_capturing(
        outcome: Arc<Mutex<Outcome>>,
    ) -> lore_revision::interface::LoreEventCallback {
        Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::Complete(complete) = event {
                let mut outcome = outcome.lock();
                outcome.status = complete.status;
                outcome.error_code = complete.error.error_code;
            }
        }) as Box<_>)
    }

    /// Create at `path` from `repository_url` without asking for a local repository,
    /// returning the status. For the cases that should be refused rather than quietly
    /// reinterpreted as a repository name.
    async fn create_online(path: &std::path::Path, repository_url: &str) -> i32 {
        let args = LoreRepositoryCreateArgs {
            repository_url: LoreString::from(repository_url),
            id: LoreString::default(),
            description: LoreString::default(),
            vfs: LoreVfsType::default(),
            use_shared_store: LoreSharedStoreMode::Disabled,
            shared_store_path: LoreString::default(),
        };

        lore::repository::create(globals(path), args, None).await
    }

    fn globals(repository_path: &std::path::Path) -> LoreGlobalArgs {
        LoreGlobalArgs {
            repository_path: repository_path.into(),
            ..Default::default()
        }
    }

    /// Create a repository at `path` from `repository_url`, which may be empty. Note the
    /// absence of `offline`: a remote-less create is an ordinary create, not an offline one.
    async fn create_repository(path: &std::path::Path, repository_url: &str) -> i32 {
        let args = LoreRepositoryCreateArgs {
            repository_url: LoreString::from(repository_url),
            id: LoreString::default(),
            description: LoreString::default(),
            vfs: LoreVfsType::default(),
            use_shared_store: LoreSharedStoreMode::Disabled,
            shared_store_path: LoreString::default(),
        };

        // `--offline` is what asks for a local repository now, so a remote-less
        // create says so rather than being inferred from an absent URL.
        let mut globals = globals(path);
        globals.offline = 1;
        lore::repository::create(globals, args, None).await
    }

    /// Create a local repository at `path` from `repository_url`, reporting the status and
    /// the name it was created with as carried by the `RepositoryCreate` event.
    async fn create_reporting_name(path: &std::path::Path, repository_url: &str) -> (i32, String) {
        let args = LoreRepositoryCreateArgs {
            repository_url: LoreString::from(repository_url),
            id: LoreString::default(),
            description: LoreString::default(),
            vfs: LoreVfsType::default(),
            use_shared_store: LoreSharedStoreMode::Disabled,
            shared_store_path: LoreString::default(),
        };

        let mut globals = globals(path);
        globals.offline = 1;

        let name: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let captured = name.clone();
        let callback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::RepositoryCreate(data) = event {
                *captured.lock() = data.name.to_string();
            }
        }) as Box<_>);

        let status = lore::repository::create(globals, args, callback).await;
        let name = name.lock().clone();
        (status, name)
    }

    fn configured_remote(path: &std::path::Path) -> Option<String> {
        lore_revision::repository::load_repository_config(path)
            .expect("the created repository should have a readable config")
            .remote_url
    }

    // The headline of the change: no URL, no offline flag, and the repository is created
    // with no remote recorded for later commands to try to reach.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn create_without_a_url_records_no_remote() {
        let tempdir = TempDir::new("lore-no-remote-");
        let path = tempdir.path();

        let status = create_repository(path, "").await;
        assert_eq!(
            status, 0,
            "creating a repository without a URL should succeed"
        );

        let remote = configured_remote(path);
        assert!(
            remote.as_deref().unwrap_or_default().is_empty(),
            "expected no remote URL, found {remote:?}"
        );
    }

    // A bare name with no host is likewise a repository with no remote, rather than the
    // "URL must include a host name" refusal it used to be outside offline mode.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn create_from_a_bare_name_records_no_remote() {
        let tempdir = TempDir::new("lore-no-remote-named-");
        let path = tempdir.path();

        let status = create_repository(path, "my-project").await;
        assert_eq!(status, 0, "a bare repository name should be accepted");

        let remote = configured_remote(path);
        assert!(
            remote.as_deref().unwrap_or_default().is_empty(),
            "expected no remote URL, found {remote:?}"
        );
    }

    // `is_valid_name` supports slash-separated names, so `org/project` is a name and not a
    // host and a path. Reading the first segment as a host truncated the name to `project`
    // and recorded `lores://org` as the remote — inventing a remote the caller never
    // configured, for a repository they asked to have none.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_slash_separated_name_is_a_name_not_a_host() {
        let tempdir = TempDir::new("lore-no-remote-slash-");
        let path = tempdir.path();

        let (status, name) = create_reporting_name(path, "org/project").await;
        assert_eq!(status, 0, "a slash-separated name should be accepted");
        assert_eq!(name, "org/project", "the whole argument is the name");

        let remote = configured_remote(path);
        assert!(
            remote.as_deref().unwrap_or_default().is_empty(),
            "the first segment is part of the name, not a host, found remote {remote:?}"
        );
    }

    // A URL that does name a host is still recorded, so the remote-less path did not
    // swallow the ordinary case.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn create_with_a_url_still_records_the_remote() {
        let tempdir = TempDir::new("lore-with-remote-");
        let path = tempdir.path();

        // Offline, because this URL names a host that no server answers on and the point
        // here is only what gets written to the config.
        let args = LoreRepositoryCreateArgs {
            repository_url: LoreString::from("lore://127.0.0.1:41337/my-project"),
            id: LoreString::default(),
            description: LoreString::default(),
            vfs: LoreVfsType::default(),
            use_shared_store: LoreSharedStoreMode::Disabled,
            shared_store_path: LoreString::default(),
        };
        let mut offline = globals(path);
        offline.offline = 1;
        let status = lore::repository::create(offline, args, None).await;
        assert_eq!(status, 0, "creating a repository with a URL should succeed");

        assert_eq!(
            configured_remote(path).as_deref(),
            Some("lore://127.0.0.1:41337"),
            "a URL naming a host should still be recorded as the remote"
        );
    }

    // The other half of the ticket: a network command against a remote-less repository
    // reports `NoRemote`, and specifically not `Disconnected`.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn login_without_a_remote_is_no_remote() {
        let tempdir = TempDir::new("lore-no-remote-login-");
        let path = tempdir.path();

        assert_eq!(
            create_repository(path, "").await,
            0,
            "create should succeed"
        );

        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let status = lore::auth::login_with_token(
            globals(path),
            lore::auth::LoreAuthLoginWithTokenArgs {
                // Empty, so the remote is resolved from the repository config — which
                // is exactly the "created without a URL" state under test.
                remote_url: LoreString::default(),
                token: LoreString::from("irrelevant"),
                token_type: LoreString::from("lore"),
                auth_url: LoreString::default(),
            },
            callback_capturing(outcome.clone()),
        )
        .await;

        let outcome = outcome.lock();
        assert_eq!(
            status,
            NoRemote.ffi_code(),
            "a login against a repository with no remote should report NoRemote"
        );
        assert_eq!(outcome.status, NoRemote.ffi_code());
        assert_eq!(outcome.error_code, NoRemote.ffi_code());
        assert_ne!(
            outcome.error_code,
            Disconnected.ffi_code(),
            "no remote is configured, so nothing was unreachable"
        );
    }

    // Resolving the auth endpoint takes the same path, and is what `lore auth` reaches
    // for before it has any endpoint of its own to work from.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn local_user_info_without_a_remote_is_no_remote() {
        let tempdir = TempDir::new("lore-no-remote-userinfo-");
        let path = tempdir.path();

        assert_eq!(
            create_repository(path, "").await,
            0,
            "create should succeed"
        );

        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let status = lore::auth::local_user_info(
            globals(path),
            lore::auth::LoreAuthLocalUserInfoArgs {
                auth_endpoint: LoreString::default(),
                user_ids: LoreArray::default(),
                with_token: 0,
            },
            callback_capturing(outcome.clone()),
        )
        .await;

        assert_eq!(
            status,
            NoRemote.ffi_code(),
            "resolving an auth endpoint with no remote should report NoRemote"
        );
        assert_eq!(outcome.lock().error_code, NoRemote.ffi_code());
    }

    // Asking for a local repository is explicit. Without `--offline` an argument naming no
    // host is refused rather than quietly becoming a repository name — the reinterpretation
    // that made an unset `LORE_REMOTE_URL` indistinguishable from a deliberate local create.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_repository_has_to_be_asked_for() {
        let invalid_arguments = InvalidArguments {
            reason: String::new(),
        }
        .ffi_code();

        let bare_name = TempDir::new("lore-needs-offline-name-");
        assert_eq!(
            create_online(bare_name.path(), "my-project").await,
            invalid_arguments,
            "a bare name without --offline names no host, so it should be refused"
        );

        let no_url = TempDir::new("lore-needs-offline-empty-");
        assert_eq!(
            create_online(no_url.path(), "").await,
            invalid_arguments,
            "an omitted URL without --offline should be refused"
        );

        for refused in [bare_name.path(), no_url.path()] {
            assert!(
                !refused.join(".lore").exists(),
                "a refused create should leave nothing behind at {}",
                refused.display()
            );
        }
    }

    // `repository info` builds its query URL out of the configured remote. With none it
    // used to assemble a hostless URL and report the result as a malformed one, which
    // described the code's own construction rather than anything the user configured.
    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn repository_info_without_a_remote_is_no_remote() {
        let tempdir = TempDir::new("lore-no-remote-info-");
        let path = tempdir.path();

        assert_eq!(
            create_repository(path, "").await,
            0,
            "create should succeed"
        );

        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let status = lore::repository::info(
            globals(path),
            lore::repository::LoreRepositoryInfoArgs {
                repository_url: LoreString::default(),
            },
            callback_capturing(outcome.clone()),
        )
        .await;

        assert_eq!(
            status,
            NoRemote.ffi_code(),
            "querying info with no remote should report NoRemote"
        );
        assert_eq!(outcome.lock().error_code, NoRemote.ffi_code());
    }
}
