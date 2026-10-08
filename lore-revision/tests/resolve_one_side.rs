// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Resolving a conflict to one side runs within one task budget, whatever the shape of the tree
//! it is given. Its own binary, because the probe it reads counts every resolution in the process.
#![allow(clippy::disallowed_methods)] // Test fixtures write to the filesystem outside the repo write-token discipline.
#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::LORE_CONTEXT;
    use lore_revision::lore::RepositoryId;
    use lore_revision::lore::runtime;
    use lore_revision::metadata::MetadataInherit;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;

    include!("helper.rs");

    /// Paths resolved at once, each a directory of [`FILES`] files on the side resolved to and a
    /// file on the other, and the budget the resolution runs under: smaller than the directories,
    /// so the tree is small and still exceeds it.
    const DIRECTORIES: usize = 25;
    const FILES: usize = 4;
    const BUDGET: usize = 8;

    /// `merge_start` reads the remote unless `globals.offline` is set.
    async fn offline_execution() -> Arc<lore_revision::interface::ExecutionContext> {
        let _ = test_store_create().await.expect("Failed to create stores");
        let execution = Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ));
        execution.set_user_id("operator@example.com").await;
        execution
    }

    struct Fixture {
        repository: Arc<RepositoryContext>,
        write_token: RepositoryWriteToken,
        repo_path: PathBuf,
        main_branch_id: BranchId,
        _tempdir: TempDir,
    }

    impl Fixture {
        async fn new() -> Self {
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let tempdir = generate_tempdir();
            let repo_path = tempdir.to_path_buf();
            std::fs::create_dir_all(repo_path.as_path()).expect("Create repo directory failed");
            let main_branch_id = BranchId::from(uuid::Uuid::now_v7());
            let write_token = repository::RepositoryWriteToken::acquire(repo_path.as_path()).await;
            let repository = repository::create_local(
                repo_path.as_path(),
                &write_token,
                repository_id,
                main_branch_id,
                branch::DEFAULT_DEFAULT_NAME.to_string(),
                repository::RepositoryConfig::default(),
                false,
            )
            .await
            .expect("Failed to initialize repository");
            Self {
                repository,
                write_token,
                repo_path,
                main_branch_id,
                _tempdir: tempdir,
            }
        }

        fn file(directory: usize, file: usize) -> String {
            format!("d{directory:02}/f{file:02}.txt")
        }

        fn write_file(&self, relative: &str, content: &[u8]) {
            let absolute = self.repo_path.join(relative);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create parent dir");
            }
            let mut file = std::fs::File::create(absolute.as_path()).expect("Failed to open file");
            file.write_all(content).expect("Failed to write file");
        }

        fn directory(directory: usize) -> String {
            format!("d{directory:02}")
        }

        /// Writes `content` into every file of every directory, removing a file standing where
        /// a directory goes.
        fn write_all(&self, content: &[u8]) {
            for directory in 0..DIRECTORIES {
                let path = self.repo_path.join(Self::directory(directory));
                if path.is_file() {
                    std::fs::remove_file(&path).expect("Remove failed");
                }
                for file in 0..FILES {
                    self.write_file(&Self::file(directory, file), content);
                }
            }
        }

        /// Replaces every directory with a file of its name.
        fn replace_directories_with_files(&self) {
            for directory in 0..DIRECTORIES {
                let path = self.repo_path.join(Self::directory(directory));
                std::fs::remove_dir_all(&path).expect("Remove failed");
                self.write_file(&Self::directory(directory), b"a file now\n");
            }
        }

        async fn stage_and_commit(&self, message: &str) -> Hash {
            file::stage::stage(
                self.repository.clone(),
                &self.write_token,
                LoreArray::from_vec(vec![LoreString::from(&self.repo_path)]),
                StageOptions {
                    case_change: stage::StageCaseChange::Error,
                    node_flags: NodeFlags::NoFlags,
                    file_id: None,
                    no_children: false,
                    scan: true,
                },
            )
            .await
            .expect("Failed to stage repository");
            commit::commit_boxed(
                self.repository.clone(),
                &self.write_token,
                CommitOptions {
                    message: message.to_string(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                },
            )
            .await
            .expect("Failed to commit revision")
        }

        async fn create_branch(&self, name: &str) -> BranchId {
            branch::create::create(
                self.repository.clone(),
                &self.write_token,
                name.to_string(),
                None,
                String::new(),
                false,
            )
            .await
            .expect("Failed to create branch");
            let (_revision, branch_id) =
                lore_revision::instance::load_current_anchor_boxed(&self.repository)
                    .await
                    .expect("Failed to load current anchor after branch create");
            branch_id
        }

        async fn switch_to(&self, branch_id: BranchId, revision: Hash) {
            lore_revision::instance::store_current_anchor_branch(&self.repository, branch_id)
                .await
                .expect("Failed to store anchor branch");
            lore_revision::instance::store_current_anchor(&self.repository, revision)
                .await
                .expect("Failed to store anchor revision");
        }
    }

    /// **A resolution to one side has at most the budget's tasks live at once.** A path that is a
    /// directory on the side resolved to is resolved in a task of its own and each change below
    /// it in another, so a bound kept per task set would let every directory's changes run at
    /// once on top of each other. The feature branch replaces each directory with a file, which
    /// the merge brings into the working tree; resolving the paths to this side finds the change
    /// below each. The changes are held until the count of live tasks has settled, since tasks
    /// that finish as fast as they start never pile up, and the budget is made small so a small
    /// tree exceeds it.
    #[tokio::test]
    async fn a_resolution_to_one_side_keeps_its_tasks_within_one_budget() {
        let execution = offline_execution().await;
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::new().await;
                fixture.write_all(b"base\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                let feature_branch = fixture.create_branch("feature").await;
                fixture.replace_directories_with_files();
                fixture.stage_and_commit("feature work").await;

                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_all(b"base\n");
                Box::pin(branch::merge::merge_start(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    feature_branch,
                    branch::merge::MergeStartOptions {
                        message: "merge feature into main".to_string(),
                        no_commit: true,
                        scope: branch::merge::MergeScope::MainOnly,
                        inherit_metadata: MetadataInherit::default(),
                    },
                ))
                .await
                .expect("merge_start failed");
                assert!(
                    fixture.repo_path.join(Fixture::directory(0)).is_file(),
                    "the merge brought the feature branch's files into the working tree"
                );

                stage::resolve_tasks::reset();
                stage::resolve_tasks::set_budget(BUDGET);
                stage::resolve_tasks::hold();
                let settled = tokio::spawn(async {
                    let deadline = Instant::now() + Duration::from_secs(60);
                    let (mut last, mut since) = (0, Instant::now());
                    loop {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let live = stage::resolve_tasks::live();
                        if live != last {
                            (last, since) = (live, Instant::now());
                        } else if live > 0 && since.elapsed() > Duration::from_millis(500) {
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "the resolution never settled, {live} tasks live"
                        );
                    }
                    let peak = stage::resolve_tasks::peak();
                    stage::resolve_tasks::release();
                    peak
                });
                let directories = (0..DIRECTORIES)
                    .map(|directory| {
                        let path = fixture.repo_path.join(Fixture::directory(directory));
                        LoreString::from(path.to_string_lossy().as_ref())
                    })
                    .collect();
                branch::merge::merge_resolve_mine(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    LoreArray::from_vec(directories),
                )
                .await
                .expect("merge_resolve_mine failed");

                let peak = settled.await.expect("the observer completes");
                let spawned = stage::resolve_tasks::spawned();
                assert!(
                    peak <= BUDGET,
                    "at most the budget is live at once: peak {peak}, spawned {spawned}"
                );
                assert!(
                    spawned >= DIRECTORIES,
                    "every directory was resolved in a task: spawned {spawned}"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
