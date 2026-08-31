// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Integration test harness: tasks spawned here are scoped to the test
// body and never call back into the lore runtime, so LORE_CONTEXT
// propagation is not required.
#![allow(clippy::disallowed_methods)]

use lore_revision::lore::*;

mod test_util;

mod tests {
    use std::sync::Arc;
    use std::sync::LazyLock;

    use lore::interface::LoreString;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::repository::LoreRepositoryStatusArgs;
    use lore_revision::event::LoreEvent;
    use lore_revision::event::convert_event_callback;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreEventCallbackConfig;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::repository::LoreSharedStoreMode;
    use parking_lot::Mutex;
    use rand::Rng;
    use rand::distr::Alphanumeric;
    use serial_test::serial;

    use super::test_util::TempDir;
    use super::*;

    static REPOSITORY_CREATE_COMPLETE: LazyLock<Arc<Mutex<bool>>> =
        LazyLock::new(|| Arc::new(Mutex::new(false)));
    static REPOSITORY_CREATE_C_COMPLETE: LazyLock<Arc<Mutex<bool>>> =
        LazyLock::new(|| Arc::new(Mutex::new(false)));
    // Set if any `Error` event is seen on the failing create.
    static REPOSITORY_CREATE_FAIL_ERROR_EVENT_SEEN: LazyLock<Arc<Mutex<bool>>> =
        LazyLock::new(|| Arc::new(Mutex::new(false)));
    // Set when the failing create reports its failure through the enriched
    // `Complete` event (non-zero status with a populated detail).
    static REPOSITORY_CREATE_FAIL_COMPLETE_IS_FAILURE: LazyLock<Arc<Mutex<bool>>> =
        LazyLock::new(|| Arc::new(Mutex::new(false)));
    static REPOSITORY_CREATE_FAIL_COMPLETE: LazyLock<Arc<Mutex<bool>>> =
        LazyLock::new(|| Arc::new(Mutex::new(false)));
    static REPOSITORY_STATUS_COMPLETE: LazyLock<Arc<Mutex<bool>>> =
        LazyLock::new(|| Arc::new(Mutex::new(false)));

    /// Runs `repository::create` to completion, returning once its callback has
    /// received every event.
    ///
    /// The call ends in [`EventDispatcher::complete`], which awaits the
    /// forwarder's completion token while it holds the last strong sender, so
    /// `Complete` and `End` have both been delivered by the time this returns.
    /// A caller therefore asserts on what the callback recorded rather than
    /// polling for it against a deadline.
    async fn repository_create(callback: LoreEventCallback, repository_path: std::path::PathBuf) {
        let globals = LoreGlobalArgs {
            repository_path: repository_path.into(),
            offline: 1,
            ..Default::default()
        };

        let name: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();
        let args = LoreRepositoryCreateArgs {
            repository_url: name.into(),
            id: LoreString::default(),
            description: LoreString::default(),
            use_shared_store: LoreSharedStoreMode::Disabled,
            shared_store_path: LoreString::default(),
        };

        // Run repo create command
        runtime()
            .spawn(async move {
                let _ = lore::repository::create(globals, args, callback).await;
            })
            .await
            .unwrap();
    }

    /// Runs `repository::status` to completion, returning once its callback has
    /// received every event. Completes through the same dispatcher path as
    /// [`repository_create`], including for a path that holds no repository.
    async fn repository_status(callback: LoreEventCallback, repository_path: std::path::PathBuf) {
        let globals = LoreGlobalArgs {
            repository_path: repository_path.into(),
            offline: 1,
            ..Default::default()
        };

        let args = LoreRepositoryStatusArgs {
            staged: 1,
            scan: 1,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: LoreArray::default(),
        };

        // Run repo status command
        runtime()
            .spawn(async move {
                let _ = lore::repository::status(globals, args, callback).await;
            })
            .await
            .unwrap();
    }

    fn repository_create_callback(event: &LoreEvent) {
        match event {
            LoreEvent::Error(error) => {
                println!(
                    "Received ErrorEvent! (error_type: {} | error_inner: {})",
                    error.error_type,
                    error.error_inner.as_str()
                );
            }
            LoreEvent::Complete(complete) => {
                println!("Received CompleteEvent! (status: {})", complete.status);
                *REPOSITORY_CREATE_COMPLETE.lock() = true;
            }
            _ => (),
        }
    }

    fn repository_create_fail_callback(event: &LoreEvent) {
        match event {
            LoreEvent::Error(error) => {
                println!(
                    "Received ErrorEvent! (error_type: {} | error_inner: {})",
                    error.error_type,
                    error.error_inner.as_str()
                );
                // Record that an `Error` event arrived on the failing create.
                *REPOSITORY_CREATE_FAIL_ERROR_EVENT_SEEN.lock() = true;
            }
            LoreEvent::Complete(complete) => {
                println!("Received CompleteEvent! (status: {})", complete.status);
                *REPOSITORY_CREATE_FAIL_COMPLETE.lock() = true;
                // A failing create now reports failure through the enriched
                // `Complete`: non-zero status that matches the carried detail.
                if complete.status != 0 && complete.status == complete.error.error_code {
                    *REPOSITORY_CREATE_FAIL_COMPLETE_IS_FAILURE.lock() = true;
                }
            }
            _ => (),
        }
    }

    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn repository_basic_create() {
        // Construct the callback closure
        let callback = Some(Box::new(move |event: &LoreEvent| {
            repository_create_callback(event);
        }) as Box<_>);

        // Construct the fail callback closure
        let fail_callback = Some(Box::new(move |event: &LoreEvent| {
            repository_create_fail_callback(event);
        }) as Box<_>);

        // Generate a tempdir to create in
        let tempdir = TempDir::new("lore-events-test-");
        let repository_path = tempdir.path().to_path_buf();

        repository_create(callback, repository_path.clone()).await;

        assert!(
            *REPOSITORY_CREATE_COMPLETE.lock(),
            "create must deliver a Complete event"
        );

        // Run again to fail on purpose
        repository_create(fail_callback, repository_path.clone()).await;

        assert!(
            *REPOSITORY_CREATE_FAIL_COMPLETE.lock(),
            "failing create must deliver a Complete event"
        );
        assert!(
            *REPOSITORY_CREATE_FAIL_COMPLETE_IS_FAILURE.lock(),
            "failing create's Complete must carry a non-zero status matching its detail"
        );
        assert!(
            !*REPOSITORY_CREATE_FAIL_ERROR_EVENT_SEEN.lock(),
            "failing create must not emit an Error event; the failure is carried by Complete"
        );
    }

    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn repository_create_no_callback() {
        // Generate a tempdir to create in
        let tempdir = TempDir::new("lore-events-test-");
        let repository_path = tempdir.path().to_path_buf();

        repository_create(None, repository_path).await;
    }

    fn repository_status_callback(event: &LoreEvent) {
        match event {
            LoreEvent::Error(error) => {
                println!(
                    "Received ErrorEvent! (error_type: {} | error_inner: {})",
                    error.error_type,
                    error.error_inner.as_str()
                );
            }
            LoreEvent::Complete(complete) => {
                println!("Received CompleteEvent! (status: {})", complete.status);
                *REPOSITORY_STATUS_COMPLETE.lock() = true;
            }
            _ => (),
        }
    }

    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn repository_status_invalid_repository() {
        // Construct the callback closure
        let callback = Some(Box::new(move |event: &LoreEvent| {
            repository_status_callback(event);
        }) as Box<_>);

        // Generate a tempdir that does not have a Lore repository
        let tempdir = TempDir::new("lore-events-test-");
        let repository_path = tempdir.path().to_path_buf();

        repository_status(callback, repository_path.clone()).await;

        assert!(
            *REPOSITORY_STATUS_COMPLETE.lock(),
            "status on a path holding no repository must deliver a Complete event"
        );
    }

    #[unsafe(no_mangle)]
    extern "C" fn repository_create_c_callback(event: &LoreEvent, _user_context: u64) {
        match event {
            LoreEvent::Error(error) => {
                println!(
                    "Received ErrorEvent! (error_type: {} | error_inner: {})",
                    error.error_type,
                    error.error_inner.as_str()
                );
            }
            LoreEvent::Complete(complete) => {
                println!("Received CompleteEvent! (status: {})", complete.status);
                *REPOSITORY_CREATE_C_COMPLETE.lock() = true;
            }
            _ => (),
        }
    }

    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn repo_create_c() {
        // Construct the C callback
        let callback_config = LoreEventCallbackConfig {
            user_context: 0,
            func: Some(repository_create_c_callback),
        };

        let callback = convert_event_callback(callback_config);

        // Generate a tempdir to create in
        let tempdir = TempDir::new("lore-events-test-");
        let repository_path = tempdir.path().to_path_buf();

        repository_create(callback, repository_path).await;

        assert!(
            *REPOSITORY_CREATE_C_COMPLETE.lock(),
            "create must deliver a Complete event to a C callback"
        );
    }
}
