// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod store_keep_alive_tests {
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;

    use lore::branch;
    use lore::file;
    use lore::repository;
    use lore::revision;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lore-keep-alive-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn globals(repo_path: &PathBuf) -> LoreGlobalArgs {
        LoreGlobalArgs {
            repository_path: repo_path.into(),
            offline: 1,
            store_keep_alive: 1,
            store_keep_alive_seconds: 5,
            identity: "test-user".into(),
            ..Default::default()
        }
    }

    fn stage_all(repo_path: &PathBuf) -> (LoreGlobalArgs, file::LoreFileStageArgs) {
        (
            globals(repo_path),
            file::LoreFileStageArgs {
                paths: LoreArray::from_vec(vec![LoreString::from(".")]),
                case_change: 0,
                // Force a recursive filesystem scan: this test writes files
                // directly with `fs::write` and runs offline with no file
                // watcher, so the repository's dirty flags are never set. Without
                // a scan, staging the `.` directory path would reconcile nothing.
                scan: 1,
            },
        )
    }

    fn commit_args(message: &str) -> revision::LoreRevisionCommitArgs {
        revision::LoreRevisionCommitArgs {
            message: message.into(),
            link: Default::default(),
            link_paths: Default::default(),
            link_messages: Default::default(),
            layer: Default::default(),
            layer_paths: Default::default(),
            layer_messages: Default::default(),
            stats: Default::default(),
        }
    }

    fn write_file(repo_path: &Path, name: &str, content: &str) {
        if let Some(parent) = PathBuf::from(name).parent() {
            fs::create_dir_all(repo_path.join(parent)).unwrap();
        }
        fs::write(repo_path.join(name), content).unwrap();
    }

    /// Exercises multiple consecutive repository API calls with store keep-alive enabled.
    /// All operations run offline (no server required). The keep-alive holds strong
    /// references to immutable and mutable stores between calls, avoiding repeated
    /// store open/close cycles.
    #[tokio::test]
    async fn test_store_keep_alive_multiple_calls() {
        let repo_path = test_dir();

        // Create repository
        let result = repository::create(
            globals(&repo_path),
            repository::LoreRepositoryCreateArgs {
                repository_url: "lore://localhost/test-keep-alive".into(),
                description: LoreString::default(),
                id: LoreString::default(),
                use_shared_store: 0,
                shared_store_path: LoreString::default(),
            },
            None,
        )
        .await;
        assert_eq!(result, 0, "repository create failed");

        // Create initial files
        write_file(&repo_path, "README.md", "# Test Repository\n");
        write_file(&repo_path, "src/main.rs", "fn main() {}\n");

        // Stage and commit initial files
        let (g, a) = stage_all(&repo_path);
        let result = file::stage(g, a, None).await;
        assert_eq!(result, 0, "initial stage failed");

        let result =
            revision::commit(globals(&repo_path), commit_args("Initial commit"), None).await;
        assert_eq!(result, 0, "initial commit failed");

        // Create first feature branch (automatically switches to it)
        let result = branch::create(
            globals(&repo_path),
            branch::LoreBranchCreateArgs {
                branch: "feature/first".into(),
                category: LoreString::default(),
                id: LoreString::default(),
            },
            None,
        )
        .await;
        assert_eq!(result, 0, "create feature/first failed");

        // Edit on feature/first
        write_file(&repo_path, "feature.txt", "Feature work\n");

        let (g, a) = stage_all(&repo_path);
        let result = file::stage(g, a, None).await;
        assert_eq!(result, 0, "stage on feature/first failed");

        let result =
            revision::commit(globals(&repo_path), commit_args("Add feature file"), None).await;
        assert_eq!(result, 0, "commit on feature/first failed");

        // Switch back to main
        let result = branch::switch(
            globals(&repo_path),
            branch::LoreBranchSwitchArgs {
                branch: "main".into(),
                revision: LoreString::default(),
                reset: 0,
                bare: 0,
            },
            None,
        )
        .await;
        assert_eq!(result, 0, "switch to main failed");

        // Edit on main
        write_file(
            &repo_path,
            "README.md",
            "# Test Repository\n\nUpdated on main.\n",
        );

        let (g, a) = stage_all(&repo_path);
        let result = file::stage(g, a, None).await;
        assert_eq!(result, 0, "stage on main failed");

        let result = revision::commit(
            globals(&repo_path),
            commit_args("Update README on main"),
            None,
        )
        .await;
        assert_eq!(result, 0, "commit on main failed");

        // Create second branch (from main)
        let result = branch::create(
            globals(&repo_path),
            branch::LoreBranchCreateArgs {
                branch: "feature/second".into(),
                category: LoreString::default(),
                id: LoreString::default(),
            },
            None,
        )
        .await;
        assert_eq!(result, 0, "create feature/second failed");

        // Edit on second branch
        write_file(&repo_path, "second.txt", "Second feature\n");

        let (g, a) = stage_all(&repo_path);
        let result = file::stage(g, a, None).await;
        assert_eq!(result, 0, "stage on feature/second failed");

        let result = revision::commit(
            globals(&repo_path),
            commit_args("Add second feature file"),
            None,
        )
        .await;
        assert_eq!(result, 0, "commit on feature/second failed");

        // Switch to first feature branch
        let result = branch::switch(
            globals(&repo_path),
            branch::LoreBranchSwitchArgs {
                branch: "feature/first".into(),
                revision: LoreString::default(),
                reset: 0,
                bare: 0,
            },
            None,
        )
        .await;
        assert_eq!(result, 0, "switch to feature/first failed");

        // Edit on first branch again
        write_file(&repo_path, "feature.txt", "Feature work\nMore work\n");

        let (g, a) = stage_all(&repo_path);
        let result = file::stage(g, a, None).await;
        assert_eq!(result, 0, "stage on feature/first again failed");

        let result = revision::commit(
            globals(&repo_path),
            commit_args("Update feature file"),
            None,
        )
        .await;
        assert_eq!(result, 0, "commit on feature/first again failed");

        // Merge second branch into first (current)
        let result = branch::merge_start(
            globals(&repo_path),
            branch::LoreBranchMergeStartArgs {
                branch: "feature/second".into(),
                message: "Merge feature/second into feature/first".into(),
                no_commit: 0,
                link: Default::default(),
                ignore_links: 0,
            },
            None,
        )
        .await;
        assert_eq!(result, 0, "merge failed");

        // Verify merged files exist
        assert!(
            repo_path.join("feature.txt").exists(),
            "feature.txt missing after merge"
        );
        assert!(
            repo_path.join("second.txt").exists(),
            "second.txt missing after merge"
        );

        // Cleanup
        let _ = fs::remove_dir_all(&repo_path);
    }
}
