// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! What a scan of an unchanged tree costs in allocations, per file.
//!
//! Reports rather than asserts: the number is a measurement to compare across changes, and
//! a threshold would either be loose enough to catch nothing or tight enough to break on an
//! unrelated allocation somewhere else in the process.
//!
//! Counting needs a glibc interposer preloaded and the system allocator selected, since the
//! default rpmalloc takes its pages from `mmap` and never reaches `malloc`:
//!
//! ```text
//! gcc -shared -fPIC -O2 -o /tmp/libcount.so scripts/allocations/allocount.c
//! LORE_ALLOCATOR=system LD_PRELOAD=/tmp/libcount.so LORE_SCAN_FILES=2000 \
//!     cargo test --test path_allocations -- --nocapture
//! ```
//!
//! These report rather than assert, so they are ignored by default and asked for by name:
//!
//! ```text
//! cargo nextest run -p lore-revision --run-ignored all -E 'test(reports_its_allocations)'
//! ```
//!
//! They stay compiled by every build, which is what catches a platform they no longer build
//! on. Without the interposer the counts read as absent and the walk still runs.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to write-token discipline.

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::lore::RepositoryId;

    include!("helper.rs");

    /// Allocations the process has made so far, or `None` where nothing is counting them.
    ///
    /// Resolved at run time rather than linked, so the test builds and runs whether or not
    /// the interposer is preloaded.
    #[cfg(unix)]
    fn allocations() -> Option<u64> {
        type Total = unsafe extern "C" fn() -> u64;
        let symbol = unsafe { libc::dlsym(std::ptr::null_mut(), c"allocount_total".as_ptr()) };
        if symbol.is_null() {
            return None;
        }
        let total: Total = unsafe { std::mem::transmute::<*mut libc::c_void, Total>(symbol) };
        Some(unsafe { total() })
    }

    /// Nothing counts allocations where the interposer cannot be preloaded, which leaves the
    /// tests as what they are without it: a walk over a real tree, checked for what it reports.
    #[cfg(not(unix))]
    fn allocations() -> Option<u64> {
        None
    }

    /// How many files the fixture holds. Two runs at different sizes give the per-file cost
    /// without the fixture's own allocations in it.
    fn scan_file_count() -> usize {
        std::env::var("LORE_SCAN_FILES")
            .ok()
            .and_then(|count| count.parse().ok())
            .unwrap_or(64)
    }

    /// How many files a directory in the fixture holds. Two runs at different sizes tell what
    /// a walk spends per directory from what it spends per file, which one shape cannot.
    fn scan_directory_size() -> usize {
        std::env::var("LORE_SCAN_DIR_SIZE")
            .ok()
            .and_then(|count| count.parse().ok())
            .filter(|size| *size > 0)
            .unwrap_or(10)
    }

    /// A block of a size nothing else asks for, allocated either side of the measured work so
    /// an allocation dump can be cut down to the window: `LORE_ALLOCATOR=tracking` records
    /// every allocation the process makes, and the fixture makes far more than the walk does.
    ///
    /// Called outside the counter reads, so the two markers bracket the work in a dump without
    /// being counted as part of it.
    fn mark_window() {
        let marker: Vec<u8> = Vec::with_capacity(WINDOW_MARKER_BYTES);
        std::hint::black_box(&marker);
    }

    /// The marker's size, which only has to be one nothing else asks for. An allocator is
    /// asked for round sizes and powers of two, so this is neither.
    const WINDOW_MARKER_BYTES: usize = 0xABCD;

    /// Print what the scan cost, or say that nothing was counting.
    fn report(shape: &str, files: usize, before: Option<u64>, after: Option<u64>) {
        let directories = files.div_ceil(scan_directory_size());
        match (before, after) {
            (Some(before), Some(after)) => println!(
                "scan of {files} {shape} files in {directories} directories: {} allocations, \
                 {:.2} per file",
                after - before,
                (after - before) as f64 / files as f64
            ),
            _ => println!("scan of {files} {shape} files: no allocation counter preloaded"),
        }
    }

    /// A tree of `files` files spread over directories of [`scan_directory_size`], which is
    /// shallow enough to keep the fixture quick.
    fn write_tree(root: &std::path::Path, files: usize) {
        let directory_size = scan_directory_size();
        for index in 0..files {
            let directory = root.join(format!("dir{:04}", index / directory_size));
            std::fs::create_dir_all(&directory).expect("Create directory failed");
            test_file_write(&directory.join(format!("file{index:04}.txt")), b"contents");
        }
    }

    /// A first scan of a tree nothing tracks yet, which is the other shape a walk meets: the
    /// current revision holds none of it, so every entry takes the new-file path rather than
    /// being matched against a node.
    #[ignore = "reports allocation counts; run deliberately with the interposer preloaded"]
    #[tokio::test]
    async fn a_scan_of_an_untracked_tree_reports_its_allocations() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let files = scan_file_count();

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                write_tree(&fixture.path, files);

                let (current, staged) = test_anchor_states(&repository).await;
                mark_window();
                let before = allocations();
                let changes = test_scan(repository.clone(), staged, current).await;
                let after = allocations();
                mark_window();

                assert!(
                    changes.len() >= files,
                    "a scan must report every untracked file, reported {} of {files}",
                    changes.len()
                );
                report("untracked", files, before, after);
            }))
            .await
            .expect("Test task failed");
    }

    /// Stage and commit everything the fixture holds, so a later walk meets a tracked tree.
    async fn commit_tree(
        repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
        fixture: &TestRepository,
    ) {
        lore_revision::file::stage::stage(
            repository.clone(),
            &fixture.write_token,
            lore_revision::interface::LoreArray::from_vec(vec![
                lore_revision::interface::LoreString::from(&fixture.path),
            ]),
            lore_revision::stage::StageOptions {
                scan: true,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to stage the fixture");
        Box::pin(lore_revision::commit::commit(
            repository.clone(),
            &fixture.write_token,
            lore_revision::commit::CommitOptions {
                message: String::new(),
                link_messages: std::collections::HashMap::new(),
                link: None,
                layer_messages: std::collections::HashMap::new(),
                layer: None,
            },
        ))
        .await
        .expect("Failed to commit the fixture");
    }

    /// Putting files back on disk, which is the other side of the path handling: a walk that
    /// writes rather than compares, and the one the filesystem operation is named for.
    #[ignore = "reports allocation counts; run deliberately with the interposer preloaded"]
    #[tokio::test]
    async fn a_reset_of_a_deleted_tree_reports_its_allocations() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let files = scan_file_count();

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                write_tree(&fixture.path, files);
                commit_tree(&repository, &fixture).await;

                for index in 0..files {
                    let directory = fixture
                        .path
                        .join(format!("dir{:04}", index / scan_directory_size()));
                    std::fs::remove_file(directory.join(format!("file{index:04}.txt")))
                        .expect("Remove file failed");
                }

                mark_window();
                let before = allocations();
                lore_revision::file::reset::reset(
                    repository.clone(),
                    &fixture.write_token,
                    lore_revision::interface::LoreArray::from_vec(vec![
                        lore_revision::interface::LoreString::from(&fixture.path),
                    ]),
                    lore_revision::interface::LoreString::default(),
                    lore_revision::file::reset::ResetOptions::default(),
                )
                .await
                .expect("Failed to reset the fixture");
                let after = allocations();
                mark_window();

                let restored = fixture.path.join("dir0000").join("file0000.txt");
                assert!(restored.exists(), "a reset must put the files back");
                report("reset", files, before, after);
            }))
            .await
            .expect("Test task failed");
    }

    #[ignore = "reports allocation counts; run deliberately with the interposer preloaded"]
    #[tokio::test]
    async fn a_scan_of_an_unchanged_tree_reports_its_allocations() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let files = scan_file_count();

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                write_tree(&fixture.path, files);

                commit_tree(&repository, &fixture).await;

                let (current, staged) = test_anchor_states(&repository).await;
                mark_window();
                let before = allocations();
                let changes = test_scan(repository.clone(), staged, current).await;
                let after = allocations();
                mark_window();

                assert!(
                    changes.is_empty(),
                    "a scan of the tree just committed must find nothing, found {}",
                    changes.len()
                );
                report("unchanged", files, before, after);
            }))
            .await
            .expect("Test task failed");
    }
}
