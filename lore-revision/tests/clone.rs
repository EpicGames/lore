// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::clone::CLONE_FILE_DISCOVERY;
    use lore_revision::repository::clone::CLONE_FILE_MAX;
    use lore_revision::repository::clone::CloneContext;
    use lore_revision::repository::clone::CloneStats;
    use lore_revision::repository::clone::CloneWorkItem;
    use lore_revision::repository::clone::clone_execute;
    use lore_revision::util::path::RelativePath;
    use lore_revision::util::path::RepositoryPath;
    use lore_storage::local::immutable_store;
    use lore_storage::local::mutable_store;
    use tokio::sync::Semaphore;
    use tokio::sync::mpsc;

    include!("helper.rs");

    fn generate_clone_tempdir() -> TempDir {
        TempDir::new("lore-clone-test-")
    }

    async fn new_test_context(path: impl AsRef<Path>) -> CloneContext {
        let immutable_store = immutable_store::LocalImmutableStore::new(
            None,
            immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create immutable store");
        let repository = Arc::new(RepositoryContext::new(
            default_repository_creation_args(
                immutable_store.clone(),
                Arc::new(
                    mutable_store::LocalMutableStore::new(
                        None::<&Path>,
                        lore_storage::MutableStoreSettings::default(),
                        immutable_store,
                    )
                    .await
                    .expect("Failed to create mutable store"),
                ),
            )
            .with_path(path),
        ));
        CloneContext {
            operation: repository.file_system().begin_operation().await.unwrap(),
            repository,
            state: Arc::default(),
            options: Arc::default(),
            stats: Arc::default(),
            modified_times: Arc::default(),
        }
    }

    fn zero_size_file_node() -> Node {
        Node {
            flags: NodeFlags::File.bits(),
            mode: 0,
            _unused: 0,
            child: 0,
            parent: 0,
            sibling: 0,
            name_offset: 0,
            name_length: 0,
            reserved: 0,
            name_hash: 0,
            size: 0,
            address: Address::default(),
        }
    }

    fn make_work_item(repository: &Arc<RepositoryContext>, name: &str) -> CloneWorkItem {
        CloneWorkItem {
            repository: repository.clone(),
            node: zero_size_file_node(),
            repository_path: RepositoryPath::from_relative(
                repository,
                RelativePath::new_from_initial_path(name).expect("valid relative path"),
            )
            .expect("valid repository path"),
        }
    }

    /// `clone_execute` processes all items and creates zero-size files on disk.
    #[tokio::test]
    async fn test_clone_execute_creates_files() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                let temp_dir = generate_clone_tempdir();
                let context = new_test_context(&temp_dir).await;

                let file_count = 5;
                let (tx, rx) = mpsc::channel(file_count);

                for i in 0..file_count {
                    let name = format!("file_{i}.txt");
                    tx.send(make_work_item(&context.repository, &name))
                        .await
                        .unwrap();
                }
                drop(tx);

                clone_execute(rx, context.clone()).await.unwrap();

                assert_eq!(
                    context.stats.complete.file_complete.load(Ordering::Relaxed),
                    file_count as u64,
                );
                assert_eq!(
                    context.stats.complete.file_count.load(Ordering::Relaxed),
                    file_count as u64,
                );

                // Verify files were actually created on disk.
                for i in 0..file_count {
                    let path = temp_dir.join(format!("file_{i}.txt"));
                    assert!(path.exists(), "file {path:?} should exist");
                }
            })
            .await;
    }

    /// `clone_execute` with more items than initial permits still processes all
    /// items, proving permits are recycled after tasks complete.
    #[tokio::test]
    async fn test_clone_execute_recycles_permits() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                let temp_dir = generate_clone_tempdir();
                // Start with only 3 permits; send 20 items.
                let permit_count = 3;
                let context = CloneContext {
                    stats: Arc::new(CloneStats {
                        file_inflight: Arc::new(Semaphore::new(permit_count)),
                        ..Default::default()
                    }),
                    ..new_test_context(&temp_dir).await
                };

                let file_count = 20usize;
                let (tx, rx) = mpsc::channel(file_count);

                for i in 0..file_count {
                    let name = format!("recycle_{i}.txt");
                    tx.send(make_work_item(&context.repository, &name))
                        .await
                        .unwrap();
                }
                drop(tx);

                clone_execute(rx, context.clone()).await.unwrap();

                // All files processed despite permit_count < file_count.
                assert_eq!(
                    context.stats.complete.file_complete.load(Ordering::Relaxed),
                    file_count as u64,
                );
                // No files should be in-flight after completion.
                assert_eq!(context.stats.file_inflight_count.load(Ordering::Relaxed), 0);
            })
            .await;
    }

    /// When discovery is marked complete before `clone_execute` runs, the file
    /// limit is raised to `CLONE_FILE_MAX`.
    #[tokio::test]
    async fn test_clone_execute_raises_limit_on_discovery_complete() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                let temp_dir = generate_clone_tempdir();
                let context = new_test_context(&temp_dir).await;

                let (tx, rx) = mpsc::channel(16);
                tx.send(make_work_item(&context.repository, "discovered.txt"))
                    .await
                    .unwrap();
                // Mark discovery complete before the consumer runs.
                context
                    .stats
                    .discovery
                    .complete
                    .store(true, Ordering::Relaxed);
                drop(tx);

                clone_execute(rx, context.clone()).await.unwrap();

                // After processing, the semaphore should have been raised.
                assert_eq!(
                    context.stats.file_inflight.available_permits(),
                    CLONE_FILE_MAX
                );
            })
            .await;
    }

    /// When the queue backlog reaches `CLONE_FILE_MAX`, `clone_execute`
    /// progressively ramps the permit cap to `CLONE_FILE_MAX` even if
    /// discovery has not finished.
    #[tokio::test]
    async fn test_clone_execute_ramps_permits_to_max_on_large_backlog() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                let temp_dir = generate_clone_tempdir();
                let context = new_test_context(&temp_dir).await;

                // Enqueue enough items that the first-iteration target
                // (rx.len() + 1 + CLONE_FILE_DISCOVERY) is >= CLONE_FILE_MAX.
                let backlog = CLONE_FILE_MAX;
                let (tx, rx) = mpsc::channel(backlog + 1);
                for i in 0..backlog {
                    let name = format!("ramp_{i}.txt");
                    tx.send(make_work_item(&context.repository, &name))
                        .await
                        .unwrap();
                }
                // Discovery is NOT complete -- ramp path only.
                assert!(!context.stats.discovery.complete.load(Ordering::Relaxed));
                drop(tx);

                clone_execute(rx, context.clone()).await.unwrap();

                assert_eq!(
                    context.stats.file_inflight.available_permits(),
                    CLONE_FILE_MAX
                );
                assert_eq!(
                    context.stats.complete.file_complete.load(Ordering::Relaxed),
                    backlog as u64,
                );
            })
            .await;
    }

    /// With a small queue that never comes close to `CLONE_FILE_MAX`, the
    /// ramp keeps the permit cap near `CLONE_FILE_DISCOVERY` -- no cliff to
    /// `CLONE_FILE_MAX` happens just because the channel closed.
    #[tokio::test]
    async fn test_clone_execute_small_queue_does_not_reach_max() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async {
                let temp_dir = generate_clone_tempdir();
                let context = new_test_context(&temp_dir).await;

                // One item, large channel: backlog is tiny, ramp target
                // after the first recv is rx.len() + 1 + HEADROOM =
                // 0 + 1 + CLONE_FILE_DISCOVERY. Should stay well below MAX.
                let (tx, rx) = mpsc::channel(1000);
                tx.send(make_work_item(&context.repository, "small.txt"))
                    .await
                    .unwrap();
                drop(tx);

                clone_execute(rx, context.clone()).await.unwrap();

                let permits = context.stats.file_inflight.available_permits();
                assert!(
                    (CLONE_FILE_DISCOVERY..CLONE_FILE_MAX).contains(&permits),
                    "expected permits in [{CLONE_FILE_DISCOVERY}, {CLONE_FILE_MAX}), got {permits}",
                );
                assert_eq!(
                    context.stats.complete.file_complete.load(Ordering::Relaxed),
                    1
                );
            })
            .await;
    }

    /// Default `CloneStats` initialises the semaphore with `CLONE_FILE_DISCOVERY` permits.
    #[test]
    fn test_clone_stats_default_permits() {
        let stats = CloneStats::default();
        assert_eq!(
            stats.file_inflight.available_permits(),
            CLONE_FILE_DISCOVERY,
        );
    }
}
