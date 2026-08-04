// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod test_util;

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::str::FromStr;
    use std::sync::Arc;

    use lore::file::LoreFileStageArgs;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::repository::LoreRepositoryReleaseArgs;
    use lore::repository::LoreRepositoryStatusArgs;
    use lore::revision::LoreRevisionCommitArgs;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use parking_lot::Mutex;
    use rand::distr::Alphanumeric;
    use rand::distr::SampleString;

    use super::test_util::TempDir;

    fn in_memory_globals(repository_path: &std::path::Path) -> LoreGlobalArgs {
        LoreGlobalArgs {
            repository_path: repository_path.into(),
            offline: 1,
            in_memory: 1,
            identity: "test-user".into(),
            ..Default::default()
        }
    }

    /// Create a repository, stage a file, then call status in a second library
    /// call and verify the staged file is visible — proving in-memory store data
    /// persists across sequential calls.
    #[tokio::test]
    async fn in_memory_data_persists_across_calls() {
        let tempdir = TempDir::new("lore-in-memory-test-");
        let repository_path = tempdir.path().to_path_buf();

        let globals = in_memory_globals(&repository_path);

        // Call 1: create repository
        let name: String = Alphanumeric.sample_string(&mut rand::rng(), 16);
        let mut url = String::from_str("lore://localhost/").unwrap_or_default();
        url.push_str(name.as_str());
        let args = LoreRepositoryCreateArgs {
            repository_url: url.into(),
            id: LoreString::default(),
            description: LoreString::default(),
            default_branch_name: LoreString::default(),
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        };

        let result = lore::repository::create(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to create repository");

        // Create a file on disk to stage
        let file_path = repository_path.join("test.txt");
        {
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&file_path)
                .expect("Failed to create test file");
            file.write_all(b"hello world")
                .expect("Failed to write test file");
        }

        // Call 2: stage the file (uses the same in-memory stores via cache)
        let args = LoreFileStageArgs {
            paths: LoreArray::from_vec(vec![LoreString::from(&file_path)]),
            case_change: 0,
            scan: 1,
        };
        let result = lore::file::stage(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to stage file");

        // Call 3: status — verify staged file is reported
        let staged_files: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let staged_files_ = Arc::clone(&staged_files);
        let status_ok: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let status_ok_ = Arc::clone(&status_ok);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryStatusFile(data) if data.flag_staged != 0 => {
                staged_files_.lock().push(data.path.as_str().to_string());
            }
            LoreEvent::Complete(data) => {
                *status_ok_.lock() = data.status == 0;
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreRepositoryStatusArgs {
            staged: 1,
            scan: 0,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: LoreArray::default(),
        };
        let result = lore::repository::status(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Status call failed");
        assert!(*status_ok.lock(), "Status did not complete successfully");
        assert!(
            !staged_files.lock().is_empty(),
            "No staged files reported — in-memory store data did not persist across calls"
        );

        // Verify no .urc/immutable or .urc/mutable directories were created
        let immutable_dir = repository_path.join(".urc").join("immutable");
        let mutable_dir = repository_path.join(".urc").join("mutable");
        assert!(
            !immutable_dir.exists(),
            "immutable directory should not exist for in-memory stores"
        );
        assert!(
            !mutable_dir.exists(),
            "mutable directory should not exist for in-memory stores"
        );
    }

    /// Create a repository with in-memory stores, stage + commit, release,
    /// then verify a subsequent status call no longer sees the committed data.
    #[tokio::test]
    async fn release_clears_in_memory_data() {
        let tempdir = TempDir::new("lore-in-memory-test-");
        let repository_path = tempdir.path().to_path_buf();

        let globals = in_memory_globals(&repository_path);

        // Create repository
        let name: String = Alphanumeric.sample_string(&mut rand::rng(), 16);
        let mut url = String::from_str("lore://localhost/").unwrap_or_default();
        url.push_str(name.as_str());
        let args = LoreRepositoryCreateArgs {
            repository_url: url.into(),
            id: LoreString::default(),
            description: LoreString::default(),
            default_branch_name: LoreString::default(),
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        };

        let result = lore::repository::create(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to create repository");

        // Create and stage a file
        let file_path = repository_path.join("release_test.txt");
        {
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&file_path)
                .expect("Failed to create test file");
            file.write_all(b"release test data")
                .expect("Failed to write test file");
        }

        let args = LoreFileStageArgs {
            paths: LoreArray::from_vec(vec![LoreString::from(&file_path)]),
            case_change: 0,
            scan: 1,
        };
        let result = lore::file::stage(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to stage file");

        // Commit
        let args = LoreRevisionCommitArgs {
            message: LoreString::from("in-memory commit"),
            ..Default::default()
        };
        let result = lore::revision::commit(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to commit");

        // Release the in-memory cache
        let result =
            lore::repository::release(globals.clone(), LoreRepositoryReleaseArgs {}, None).await;
        assert_eq!(result, 0, "Failed to release repository");

        // Status after release should fail to open repository (stores are gone,
        // no files on disk to fall back to)
        let post_release_error: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let post_release_error_ = Arc::clone(&post_release_error);

        let callback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::Error(_) = event {
                *post_release_error_.lock() = true;
            }
        }) as Box<_>);

        let args = LoreRepositoryStatusArgs {
            staged: 1,
            scan: 0,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: LoreArray::default(),
        };
        let result = lore::repository::status(globals.clone(), args, callback).await;
        // After release, opening the repository with in-memory stores should
        // give us a fresh (empty) store, so status should either fail or
        // return with no revision data. Either way the old commit is gone.
        assert!(
            result != 0 || *post_release_error.lock(),
            "Status after release should error — old in-memory data should be gone"
        );
    }

    /// Stage a nested directory tree, then call status with `count` set and
    /// verify the reported directory and file totals match the staged tree.
    /// Exercises the bounded parallel subtree counter: totals are exact even
    /// though the workers process subtrees in nondeterministic order.
    #[tokio::test]
    async fn status_count_reports_staged_tree_totals() {
        let tempdir = TempDir::new("lore-in-memory-test-");
        let repository_path = tempdir.path().to_path_buf();
        let globals = in_memory_globals(&repository_path);

        let name: String = Alphanumeric.sample_string(&mut rand::rng(), 16);
        let mut url = String::from_str("lore://localhost/").unwrap_or_default();
        url.push_str(name.as_str());
        let args = LoreRepositoryCreateArgs {
            repository_url: url.into(),
            id: LoreString::default(),
            description: LoreString::default(),
            default_branch_name: LoreString::default(),
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        };
        let result = lore::repository::create(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to create repository");

        let relative_files = [
            "a.txt",
            "dir1/b.txt",
            "dir1/dir1a/c.txt",
            "dir2/d.txt",
            "dir2/e.txt",
        ];
        let mut file_paths = Vec::new();
        for relative in relative_files {
            let file_path = repository_path.join(relative);
            std::fs::create_dir_all(file_path.parent().expect("file has a parent"))
                .expect("Failed to create directory");
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&file_path)
                .expect("Failed to create test file");
            file.write_all(b"data").expect("Failed to write test file");
            file_paths.push(LoreString::from(&file_path));
        }

        let args = LoreFileStageArgs {
            paths: LoreArray::from_vec(file_paths),
            case_change: 0,
            scan: 1,
        };
        let result = lore::file::stage(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to stage files");

        let directories: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let files: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let directories_ = Arc::clone(&directories);
        let files_ = Arc::clone(&files);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryStatusCount(data) => {
                *directories_.lock() = Some(data.directories);
                *files_.lock() = Some(data.files);
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreRepositoryStatusArgs {
            staged: 1,
            scan: 0,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 1,
            paths: LoreArray::default(),
        };
        let result = lore::repository::status(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Status call failed");

        assert_eq!(
            *directories.lock(),
            Some(3),
            "expected directories dir1, dir1/dir1a, dir2"
        );
        assert_eq!(
            *files.lock(),
            Some(5),
            "expected files a.txt, dir1/b.txt, dir1/dir1a/c.txt, dir2/d.txt, dir2/e.txt"
        );
    }
}
