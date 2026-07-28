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
    use std::time::Duration;

    use lore::interface::LoreString;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::repository::LoreRepositoryStatusArgs;
    use lore_revision::event::LoreEvent;
    use lore_revision::event::convert_event_callback;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreEventCallbackConfig;
    use lore_revision::interface::LoreGlobalArgs;
    use parking_lot::Mutex;
    use rand::Rng;
    use rand::distr::Alphanumeric;
    use serial_test::serial;
    use tokio::task::JoinHandle;
    use tokio::time::error::Elapsed;
    use tokio::time::timeout;

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

    static TIMEOUT_DURATION: Duration = Duration::from_secs(5);

    async fn repository_create(
        callback: LoreEventCallback,
        callback_task: JoinHandle<Result<(), Elapsed>>,
        repository_path: std::path::PathBuf,
    ) -> Result<(), Elapsed> {
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
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        };

        // Run repo create command
        let worker = runtime().spawn(async move {
            let _ = lore::repository::create(globals, args, callback).await;
        });

        let callback_result = callback_task.await.unwrap();
        worker.await.unwrap();

        callback_result
    }

    async fn repository_status(
        callback: LoreEventCallback,
        callback_task: JoinHandle<Result<(), Elapsed>>,
        repository_path: std::path::PathBuf,
    ) -> Result<(), Elapsed> {
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
        let worker = runtime().spawn(async move {
            let _ = lore::repository::status(globals, args, callback).await;
        });

        let callback_result = callback_task.await.unwrap();
        worker.await.unwrap();

        callback_result
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

        // Run task to check until create is complete
        let callback_task = tokio::spawn(timeout(TIMEOUT_DURATION, async {
            while !(*REPOSITORY_CREATE_COMPLETE.lock()) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }));

        repository_create(callback, callback_task, repository_path.clone())
            .await
            .expect("[Repository create CompleteEvent not received in time.");

        // Run the task to check until the failing create reports its failure
        // through the enriched `Complete` event.
        let fail_callback_task = tokio::spawn(timeout(TIMEOUT_DURATION, async {
            while !(*REPOSITORY_CREATE_FAIL_COMPLETE.lock()
                && *REPOSITORY_CREATE_FAIL_COMPLETE_IS_FAILURE.lock())
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }));

        // Run again to fail on purpose
        repository_create(fail_callback, fail_callback_task, repository_path.clone())
            .await
            .expect(
                "[repo_initialize_fail] Repository initialize CompleteEvent not received in time.",
            );

        assert!(
            !*REPOSITORY_CREATE_FAIL_ERROR_EVENT_SEEN.lock(),
            "failing create must not emit an Error event; the failure is carried by Complete"
        );
    }

    #[serial]
    #[tokio::test(flavor = "multi_thread")]
    async fn repository_create_no_callback() {
        // Callback task that does nothing
        let callback_task = tokio::spawn(timeout(TIMEOUT_DURATION, async {}));

        // Generate a tempdir to create in
        let tempdir = TempDir::new("lore-events-test-");
        let repository_path = tempdir.path().to_path_buf();

        repository_create(None, callback_task, repository_path)
            .await
            .expect("Failed to create repository without callback");
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

        // Run task to check until create is complete
        let callback_task = tokio::spawn(timeout(TIMEOUT_DURATION, async {
            while !(*REPOSITORY_STATUS_COMPLETE.lock()) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }));

        repository_status(callback, callback_task, repository_path.clone())
            .await
            .expect(
                "Repository status CompleteEvent for invalid repository path not received in time.",
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

        // Run task to check until create is complete
        let callback_task = tokio::spawn(timeout(TIMEOUT_DURATION, async {
            while !(*REPOSITORY_CREATE_C_COMPLETE.lock()) {
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
        }));

        // Generate a tempdir to create in
        let tempdir = TempDir::new("lore-events-test-");
        let repository_path = tempdir.path().to_path_buf();

        repository_create(callback, callback_task, repository_path)
            .await
            .expect("Repository create CompleteEvent not received in time.");
    }
}
