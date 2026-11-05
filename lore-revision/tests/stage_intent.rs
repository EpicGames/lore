// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::change::FileAction;
    use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
    use lore_revision::fs::filesystem_provider::StageIntent;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;

    include!("helper.rs");

    /// A staging walk records what a staged add carries: the action, the size and mode the
    /// file was measured with, and an identity so metadata can be attached to it before a
    /// commit assigns one. The fixture carries the executable bit, which is the only mode
    /// a node records and the only one a default node does not already have.
    #[tokio::test]
    async fn a_staging_walk_records_an_add_with_its_identity() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                let contents = b"#!/bin/sh\necho staged";
                let script = fixture.path.join("script.sh");
                test_file_write(&script, contents);
                #[cfg(target_family = "unix")]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                        .expect("Failed to set the executable bit");
                }

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let added = changes
                    .iter()
                    .find(|change| change.path().as_str() == "script.sh")
                    .expect("the walk must report the new file");
                assert_eq!(FileAction::Add, added.action);

                let node = staged
                    .node(repository.clone(), added.to.mapping.node)
                    .await
                    .expect("the staged node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "a staged add must record the action, flags {:x}",
                    node.flags
                );
                assert!(
                    flags.contains(NodeFlags::DirtyAdd),
                    "a staged add is dirty too, flags {:x}",
                    node.flags
                );
                assert_eq!(
                    contents.len() as u64,
                    node.size,
                    "the node must record the size the file was measured with"
                );
                assert!(
                    !node.address.context.is_zero(),
                    "a staged file node must carry an identity"
                );
                #[cfg(target_family = "unix")]
                assert_eq!(
                    lore_revision::node::NodeFileMode::Executable.bits(),
                    node.mode & lore_revision::node::NodeFileMode::Executable.bits(),
                    "the node must record the mode the file was measured with"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A marking walk records the action without the staged one, which is what separates
    /// `status --scan` from staging.
    #[tokio::test]
    async fn a_marking_walk_records_no_staged_action() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                test_file_write(&fixture.path.join("script.sh"), b"marked only");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;

                let added = changes
                    .iter()
                    .find(|change| change.path().as_str() == "script.sh")
                    .expect("the walk must report the new file");
                let node = staged
                    .node(repository.clone(), added.to.mapping.node)
                    .await
                    .expect("the marked node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::DirtyAdd),
                    "a marked add records the dirty action, flags {:x}",
                    node.flags
                );
                assert!(
                    !flags.contains(NodeFlags::StagedAdd),
                    "a marked add records no staged action, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A repository whose ignore filter holds `ignore` and whose view holds `view`, so a walk has
    /// content the filter leaves out to answer for.
    async fn test_repository_excluding(
        immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
        repository_id: RepositoryId,
        ignore: &[&str],
        view: &[&str],
    ) -> TestRepository {
        let tempdir = generate_tempdir();
        let path = tempdir.to_path_buf();
        let write_token =
            lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
        let default_branch_id = lore_base::types::Context::from(uuid::Uuid::now_v7());
        let created = lore_revision::repository::create_local(
            path.as_path(),
            &write_token,
            repository_id,
            default_branch_id,
            lore_revision::branch::DEFAULT_DEFAULT_NAME.to_string(),
            lore_revision::repository::RepositoryConfig::default(),
            false,
        )
        .await
        .expect("Failed to initialize repository");

        let repository = std::sync::Arc::new(
            lore_revision::repository::RepositoryContext::new(
                default_repository_creation_args(immutable_store, mutable_store)
                    .with_path(&path)
                    .with_id(repository_id)
                    .with_instance_id(created.instance_id)
                    .with_filter(std::sync::Arc::new(test_filter(ignore, view))),
            )
            .with_write_token(write_token.share()),
        );
        lore_revision::instance::store_current_anchor_branch(&repository, default_branch_id)
            .await
            .expect("Failed to store anchor branch");

        TestRepository {
            repository,
            write_token,
            path,
            _tempdir: tempdir,
        }
    }

    /// Stage and commit the whole fixture under force, which is what puts a path the filter
    /// excludes into the tree.
    async fn force_commit_fixture(fixture: &TestRepository) {
        LORE_CONTEXT
            .scope(
                forced_execution(),
                lore_revision::file::stage::stage(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    lore_revision::interface::LoreArray::from_vec(vec![
                        lore_revision::interface::LoreString::from(&fixture.path),
                    ]),
                    lore_revision::stage::StageOptions {
                        scan: true,
                        ..Default::default()
                    },
                ),
            )
            .await
            .expect("Failed to stage the fixture");
        lore_revision::commit::commit_boxed(
            fixture.repository.clone(),
            &fixture.write_token,
            lore_revision::commit::CommitOptions {
                message: String::new(),
                link_messages: std::collections::HashMap::new(),
                link: None,
                layer_messages: std::collections::HashMap::new(),
                layer: None,
            },
        )
        .await
        .expect("Failed to commit the fixture");
    }

    /// A repository ignoring by `ignore` whose working tree holds `paths`, none of it committed.
    async fn repository_holding(
        immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
        ignore: &[&str],
        paths: &[&str],
    ) -> TestRepository {
        let fixture = test_repository_excluding(
            immutable_store,
            mutable_store,
            RepositoryId::from(uuid::Uuid::now_v7()),
            ignore,
            &[],
        )
        .await;
        write_paths(&fixture, paths);
        fixture
    }

    /// Writes `paths` into the working tree of `fixture`: a file at each path, holding the path,
    /// and an empty directory at each ending in `/`.
    fn write_paths(fixture: &TestRepository, paths: &[&str]) {
        for path in paths {
            let absolute = fixture.path.join(path);
            if path.ends_with('/') {
                std::fs::create_dir_all(&absolute).expect("Create directory failed");
            } else {
                std::fs::create_dir_all(absolute.parent().expect("A path below the root"))
                    .expect("Create directory failed");
                test_file_write(&absolute, path.as_bytes());
            }
        }
    }

    /// Whether the node `state` holds at each of `paths` carries `flag`.
    async fn carries(
        repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
        state: &lore_revision::state::State,
        paths: &[&str],
        flag: NodeFlags,
    ) -> Vec<bool> {
        let mut carried = Vec::new();
        for path in paths {
            let node = state
                .find_node(repository.clone(), path)
                .await
                .unwrap_or_else(|err| panic!("No node at {path}: {err}"));
            carried.push(NodeFlags::from_bits_retain(node.flags).contains(flag));
        }
        carried
    }

    /// Which of `probes` carry `flag` once a walk under `intent` has run, and what the walk
    /// reported, over a [`repository_holding`] `paths` committed under force that then loses
    /// `removed` from the working tree.
    async fn walk_after_removal(
        intent: FilesystemDiffIntent,
        ignore: &'static [&'static str],
        paths: &'static [&'static str],
        removed: &'static str,
        probes: &'static [&'static str],
        flag: NodeFlags,
    ) -> (Vec<bool>, Vec<(String, String)>) {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    repository_holding(immutable_store, mutable_store, ignore, paths).await;
                let repository = fixture.repository.clone();
                force_commit_fixture(&fixture).await;
                std::fs::remove_dir_all(fixture.path.join(removed))
                    .expect("Remove directory failed");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes =
                    test_scan_with_intent(repository.clone(), staged.clone(), current, intent)
                        .await;
                (
                    carries(&repository, &staged, probes, flag).await,
                    test_reported(&changes),
                )
            }))
            .await
            .expect("Test task failed")
    }

    /// The deletes `reported` names, in order of path.
    fn deletes(reported: Vec<(String, String)>) -> Vec<String> {
        let mut paths: Vec<String> = reported
            .into_iter()
            .map(|(action, path)| {
                assert_eq!(
                    action, "D",
                    "a removal reports only deletes, got one at {path}"
                );
                path
            })
            .collect();
        paths.sort();
        paths
    }

    /// A staged delete of a directory the working tree lost deletes a child the ignore file names
    /// with the rest: the tree tracks the child, so the ignore file has no say over it.
    #[tokio::test]
    async fn a_staged_delete_deletes_a_tracked_child_the_ignore_file_names() {
        let (deleted, reported) = walk_after_removal(
            FilesystemDiffIntent::Stage(StageIntent::default()),
            &["dir/hidden.txt"],
            &["dir/shown.txt", "dir/hidden.txt"],
            "dir",
            &["dir", "dir/hidden.txt"],
            NodeFlags::StagedDelete,
        )
        .await;
        assert_eq!(
            (deleted, deletes(reported)),
            (
                vec![true, true],
                vec![
                    "dir".to_string(),
                    "dir/hidden.txt".to_string(),
                    "dir/shown.txt".to_string()
                ]
            ),
            "the directory is deleted with both files"
        );
    }

    /// A staged delete deletes a directory the working tree lost whose tracked content the ignore
    /// file names, whether that content is a file directly below it or a directory deeper down.
    #[tokio::test]
    async fn a_staged_delete_deletes_a_directory_holding_only_tracked_content_the_ignore_file_names()
     {
        let (deleted, reported) = walk_after_removal(
            FilesystemDiffIntent::Stage(StageIntent::default()),
            &["*.log", "/dir/sub/"],
            &["dir/x.log", "dir/sub/y.txt"],
            "dir",
            &["dir", "dir/x.log", "dir/sub", "dir/sub/y.txt"],
            NodeFlags::StagedDelete,
        )
        .await;
        assert_eq!(
            (deleted, deletes(reported)),
            (
                vec![true; 4],
                vec![
                    "dir".to_string(),
                    "dir/sub".to_string(),
                    "dir/sub/y.txt".to_string(),
                    "dir/x.log".to_string()
                ]
            ),
            "everything the directory held is deleted with it"
        );
    }

    /// A marking walk marks the removal of a tracked child the ignore file names, as a staging
    /// walk deletes it.
    #[tokio::test]
    async fn a_marking_delete_marks_a_tracked_child_the_ignore_file_names() {
        let (deleted, _) = walk_after_removal(
            FilesystemDiffIntent::MarkDirty,
            &["dir/hidden.txt"],
            &["dir/shown.txt", "dir/hidden.txt"],
            "dir",
            &["dir/hidden.txt"],
            NodeFlags::DirtyDelete,
        )
        .await;
        assert_eq!(
            deleted,
            [true],
            "the ignore slot has no say over a tracked node"
        );
    }

    /// A file the tree holds is past the ignore slot, so a marking walk reports it modified,
    /// while an untracked file the same rule names beside it stays out.
    #[tokio::test]
    async fn a_marking_walk_reports_a_tracked_file_the_ignore_file_names() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    repository_holding(immutable_store, mutable_store, &["*.log"], &["debug.log"])
                        .await;
                let repository = fixture.repository.clone();
                force_commit_fixture(&fixture).await;

                test_file_write(&fixture.path.join("debug.log"), b"modified since");
                test_file_write(&fixture.path.join("new.log"), b"never tracked");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;

                let tracked = changes
                    .iter()
                    .find(|change| change.path().as_str() == "debug.log")
                    .expect("the walk must report the tracked file");
                assert_eq!(FileAction::Keep, tracked.action);
                assert!(
                    tracked.flags.is_dirty(),
                    "the tracked file must be reported modified"
                );
                assert!(
                    !changes
                        .iter()
                        .any(|change| change.path().as_str() == "new.log"),
                    "an untracked file the ignore file names must stay out"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Ignore rules excluding `Engine` and re-including every `Build` directory below it, by a
    /// rule no literal prefix bounds, so a walk enters each directory below `Engine`.
    const REACHING_IGNORE: &[&str] = &["/Engine/", "!/Engine/**/Build/"];
    /// A file [`REACHING_IGNORE`] excludes and one it re-includes, each in new directories.
    const REACHING_PATHS: &[&str] = &[
        "Engine/Docs/readme.md",
        "Engine/Source/Runtime/Build/build.cs",
    ];
    /// The new directories below `Engine`: one holding only the file [`REACHING_IGNORE`] excludes,
    /// and one on the path to the file it re-includes.
    const NEW_DIRECTORIES: [&str; 2] = ["Engine/Docs", "Engine/Source/Runtime"];

    /// Which of [`NEW_DIRECTORIES`] `state` holds.
    async fn held_directories(
        repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
        state: &lore_revision::state::State,
    ) -> Vec<bool> {
        let mut held = Vec::new();
        for path in NEW_DIRECTORIES {
            held.push(state.find_node_link(repository.clone(), path).await.is_ok());
        }
        held
    }

    /// Stages `target` in the working tree of `fixture` with a scan, the whole tree where `target`
    /// is empty, answering the staged state and the number of directories the stage reports added.
    async fn scanning_stage(
        fixture: &TestRepository,
        target: &str,
    ) -> (std::sync::Arc<lore_revision::state::State>, u64) {
        let target = if target.is_empty() {
            fixture.path.clone()
        } else {
            fixture.path.join(target)
        };
        let added = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reported = added.clone();
        let execution = std::sync::Arc::new(
            lore_revision::interface::ExecutionContext::new_client_with_user_id(
                lore_revision::interface::LoreGlobalArgs::default(),
                lore_revision::relay::EventDispatcher::new(Some(Box::new(
                    move |event: &lore_revision::interface::LoreEvent| {
                        if let lore_revision::interface::LoreEvent::FileStageEnd(end) = event {
                            reported.store(
                                end.count.directory_add_count,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                    },
                ))),
                "test-user".to_string(),
            ),
        );
        let staged = LORE_CONTEXT
            .scope(execution.clone(), async {
                let staged = lore_revision::file::stage::stage(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    lore_revision::interface::LoreArray::from_vec(vec![
                        lore_revision::interface::LoreString::from(&target),
                    ]),
                    lore_revision::stage::StageOptions {
                        scan: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to stage the fixture");
                execution.dispatcher.drain().await;
                staged
            })
            .await;
        let state = lore_revision::state::State::deserialize(fixture.repository.clone(), staged)
            .await
            .expect("Failed to deserialize the staged state");
        (state, added.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// `stage --scan` keeps a new directory the filter excludes only for what it stages below it,
    /// and counts the directories it keeps alone.
    #[tokio::test]
    async fn a_scanning_stage_keeps_an_excluded_directory_only_for_content_below_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = repository_holding(
                    immutable_store,
                    mutable_store,
                    REACHING_IGNORE,
                    REACHING_PATHS,
                )
                .await;

                let (state, added) = scanning_stage(&fixture, "").await;

                assert_eq!(
                    (held_directories(&fixture.repository, &state).await, added),
                    (vec![false, true], 4),
                    "the directory holding only an excluded file is neither staged nor counted"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// `stage --scan` naming a new directory the filter excludes stages it, and the new directories
    /// above it the filter excludes, only for what it stages below it, and counts them only then.
    #[tokio::test]
    async fn a_scanning_stage_of_a_named_excluded_directory_stages_it_only_for_content_below_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let mut staged = Vec::new();
                for (paths, target) in [
                    (&REACHING_PATHS[..1], NEW_DIRECTORIES[0]),
                    (REACHING_PATHS, "Engine/Source"),
                ] {
                    let fixture = repository_holding(
                        immutable_store.clone(),
                        mutable_store.clone(),
                        REACHING_IGNORE,
                        paths,
                    )
                    .await;
                    let (state, added) = scanning_stage(&fixture, target).await;
                    let held = state
                        .find_node_link(fixture.repository.clone(), "Engine")
                        .await
                        .is_ok();
                    staged.push((held, added));
                }

                assert_eq!(
                    staged,
                    [(false, 0), (true, 4)],
                    "the directories are staged and counted only above what is staged"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory a scan marked as new that the filter excludes is discarded by `stage --scan`
    /// once what it held leaves the working tree, as one the stage adds itself. The marks are
    /// persisted as `status --scan` persists them.
    #[tokio::test]
    async fn a_scanning_stage_discards_an_excluded_directory_a_scan_marked() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    repository_holding(immutable_store, mutable_store, REACHING_IGNORE, &["top"])
                        .await;
                force_commit_fixture(&fixture).await;
                write_paths(&fixture, REACHING_PATHS);
                let repository = fixture.repository.clone();
                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                staged.reparent_onto(current.revision());
                let marked = staged
                    .serialize(
                        repository.clone(),
                        repository.try_write_token().expect("A writable repository"),
                    )
                    .await
                    .expect("Failed to serialize the marked state");
                lore_revision::instance::store_staged_anchor(&repository, marked)
                    .await
                    .expect("Failed to store the staged anchor");
                std::fs::remove_dir_all(fixture.path.join("Engine/Source/Runtime/Build"))
                    .expect("Remove directory failed");

                let (state, added) = scanning_stage(&fixture, "").await;

                assert_eq!(
                    (held_directories(&fixture.repository, &state).await, added),
                    (vec![false, false], 0),
                    "no directory holding nothing to stage is staged or counted"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// `stage --scan` and a marking walk keep a directory the filter excludes whose add a forced
    /// stage made, with the file below it the forced stage added, though each leaves that file out.
    #[tokio::test]
    async fn a_scanning_stage_keeps_a_forced_add_below_an_excluded_directory() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                const FORCED: &str = "Engine/Docs/readme.md";
                let fixture =
                    repository_holding(immutable_store, mutable_store, REACHING_IGNORE, &[FORCED])
                        .await;
                LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        lore_revision::file::stage::stage(
                            fixture.repository.clone(),
                            &fixture.write_token,
                            lore_revision::interface::LoreArray::from_vec(vec![
                                lore_revision::interface::LoreString::from(
                                    &fixture.path.join(FORCED),
                                ),
                            ]),
                            lore_revision::stage::StageOptions::default(),
                        ),
                    )
                    .await
                    .expect("Failed to stage the file under force");

                let (current, _) = test_anchor_states(&fixture.repository).await;
                let staged_revision =
                    lore_revision::instance::load_staged_revision(&fixture.repository)
                        .await
                        .expect("Failed to load the staged anchor")
                        .expect("A forced stage anchors a staged state");
                let marked = lore_revision::state::State::deserialize(
                    fixture.repository.clone(),
                    staged_revision,
                )
                .await
                .expect("Failed to deserialize the staged state");
                test_scan_with_intent(
                    fixture.repository.clone(),
                    marked.clone(),
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                let (scanned, _) = scanning_stage(&fixture, "").await;

                let mut staged = Vec::new();
                for state in [marked, scanned] {
                    let node = state.find_node(fixture.repository.clone(), FORCED).await;
                    staged.push(node.is_ok_and(|node| {
                        NodeFlags::from_bits_retain(node.flags).contains(NodeFlags::StagedAdd)
                    }));
                }
                assert_eq!(
                    staged,
                    [true, true],
                    "the forced add stays staged through a marking walk and a scanning stage"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A forced `stage --scan` stages a new empty directory the view excludes, as it leaves nothing
    /// out, whether it walks the whole tree or names the directory.
    #[tokio::test]
    async fn a_forced_scanning_stage_keeps_an_excluded_empty_directory() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                const SCRATCH: &str = "Engine/Scratch";
                let mut staged_adds = Vec::new();
                for target in ["", SCRATCH] {
                    let fixture = test_repository_excluding(
                        immutable_store.clone(),
                        mutable_store.clone(),
                        RepositoryId::from(uuid::Uuid::now_v7()),
                        &[],
                        &["/Engine/"],
                    )
                    .await;
                    write_paths(&fixture, &["Engine/Scratch/"]);

                    let staged = LORE_CONTEXT
                        .scope(
                            forced_execution(),
                            lore_revision::file::stage::stage(
                                fixture.repository.clone(),
                                &fixture.write_token,
                                lore_revision::interface::LoreArray::from_vec(vec![
                                    lore_revision::interface::LoreString::from(
                                        &fixture.path.join(target),
                                    ),
                                ]),
                                lore_revision::stage::StageOptions {
                                    scan: true,
                                    ..Default::default()
                                },
                            ),
                        )
                        .await
                        .expect("Failed to stage under force");
                    let state = lore_revision::state::State::deserialize(
                        fixture.repository.clone(),
                        staged,
                    )
                    .await
                    .expect("Failed to deserialize the staged state");

                    let node = state.find_node(fixture.repository.clone(), SCRATCH).await;
                    staged_adds.push(node.is_ok_and(|node| {
                        NodeFlags::from_bits_retain(node.flags).contains(NodeFlags::StagedAdd)
                    }));
                }

                assert_eq!(
                    staged_adds,
                    [true, true],
                    "the excluded empty directory is staged"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// `stage --scan` deletes a directory the working tree lost that the filter excludes along with
    /// the content below it, where it keeps none of that content.
    #[tokio::test]
    async fn a_scanning_stage_deletes_an_excluded_directory_with_its_content() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = repository_holding(
                    immutable_store,
                    mutable_store,
                    REACHING_IGNORE,
                    &["Engine/Source/Runtime/Build/build.cs"],
                )
                .await;
                force_commit_fixture(&fixture).await;
                std::fs::remove_dir_all(fixture.path.join("Engine"))
                    .expect("Remove directory failed");

                let (state, _) = scanning_stage(&fixture, "").await;

                assert_eq!(
                    carries(
                        &fixture.repository,
                        &state,
                        &[
                            "Engine",
                            "Engine/Source",
                            "Engine/Source/Runtime",
                            "Engine/Source/Runtime/Build/build.cs",
                        ],
                        NodeFlags::StagedDelete,
                    )
                    .await,
                    [true; 4],
                    "the excluded directories are deleted with what they held"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// `stage --scan` deletes a committed empty directory the ignore file names where the working
    /// tree lacks it: the tree tracks it, so the ignore file has no say over it. A view keeps such a
    /// directory, which nothing materializes; see `sync_views.rs`.
    #[tokio::test]
    async fn a_scanning_stage_deletes_a_tracked_empty_directory_the_ignore_file_names() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = repository_holding(
                    immutable_store,
                    mutable_store,
                    REACHING_IGNORE,
                    &["Engine/Empty/", "Engine/Source/Runtime/Build/build.cs"],
                )
                .await;
                force_commit_fixture(&fixture).await;
                std::fs::remove_dir_all(fixture.path.join("Engine/Empty"))
                    .expect("Remove directory failed");

                let (state, _) = scanning_stage(&fixture, "").await;

                assert_eq!(
                    carries(
                        &fixture.repository,
                        &state,
                        &["Engine/Empty"],
                        NodeFlags::StagedDelete
                    )
                    .await,
                    [true],
                    "the tracked empty directory is deleted"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A marking walk keeps a new directory the filter excludes only for what it marks below it,
    /// and reports it only where it keeps it.
    #[tokio::test]
    async fn a_marking_walk_keeps_an_excluded_directory_only_for_content_below_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = repository_holding(
                    immutable_store,
                    mutable_store,
                    REACHING_IGNORE,
                    REACHING_PATHS,
                )
                .await;
                let repository = fixture.repository.clone();

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;

                assert_eq!(
                    held_directories(&repository, &staged).await,
                    [false, true],
                    "the directory holding only an excluded file is not marked"
                );
                let reported = test_reported(&changes);
                assert!(
                    !reported.contains(&("A".to_string(), NEW_DIRECTORIES[0].to_string()))
                        && reported.contains(&("A".to_string(), NEW_DIRECTORIES[1].to_string())),
                    "only the kept directory is reported, reported {reported:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A marking walk discards a directory the filter excludes that an earlier marking walk added,
    /// once what it held leaves the working tree, and reports it no more.
    #[tokio::test]
    async fn a_marking_walk_discards_an_excluded_directory_an_earlier_walk_marked() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = repository_holding(
                    immutable_store,
                    mutable_store,
                    REACHING_IGNORE,
                    REACHING_PATHS,
                )
                .await;
                let repository = fixture.repository.clone();
                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                std::fs::remove_dir_all(fixture.path.join("Engine/Source/Runtime/Build"))
                    .expect("Remove directory failed");

                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;

                assert_eq!(
                    (
                        held_directories(&repository, &staged).await,
                        staged
                            .find_node_link(repository.clone(), "Engine")
                            .await
                            .is_ok(),
                        test_reported(&changes),
                    ),
                    (vec![false, false], false, Vec::new()),
                    "no directory holding nothing to mark is held or reported"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A node the walk already marked keeps its dirty action, and a staging walk over it
    /// still records the staged one: skipping the mark because the dirty action is there
    /// would leave the file reported as an add and never staged.
    #[tokio::test]
    async fn staging_a_file_a_scan_already_marked_records_the_staged_action() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                test_file_write(&fixture.path.join("script.sh"), b"scanned first");

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let added = changes
                    .iter()
                    .find(|change| change.path().as_str() == "script.sh")
                    .expect("the walk must report the file the scan marked");
                let node = staged
                    .node(repository.clone(), added.to.mapping.node)
                    .await
                    .expect("the staged node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "a scan's mark must not suppress the staged action, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A rename the walk reports has to leave a staged move behind it: the walk matches a
    /// node to a filesystem entry by folded name, so a spelling that differs is reported
    /// as a move whether or not the content changed, and a staging walk that reports one
    /// without settling it describes a state the tree does not hold.
    #[tokio::test]
    async fn a_staged_rename_of_unchanged_content_settles_the_move() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let write_token = &fixture.write_token;

                let contents = b"unchanged content";
                test_file_write(&fixture.path.join("Script.sh"), contents);
                lore_revision::file::stage::stage(
                    repository.clone(),
                    write_token,
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
                lore_revision::commit::commit_boxed(
                    repository.clone(),
                    write_token,
                    lore_revision::commit::CommitOptions {
                        message: String::new(),
                        link_messages: std::collections::HashMap::new(),
                        link: None,
                        layer_messages: std::collections::HashMap::new(),
                        layer: None,
                    },
                )
                .await
                .expect("Failed to commit the fixture");

                std::fs::rename(
                    fixture.path.join("Script.sh"),
                    fixture.path.join("script.sh"),
                )
                .expect("Failed to rename the fixture");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let moved = changes
                    .iter()
                    .find(|change| change.action == FileAction::Move)
                    .expect("the walk must report the rename");
                let node = staged
                    .node(repository.clone(), moved.from.mapping.node)
                    .await
                    .expect("the renamed node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedMove),
                    "a reported move must be settled as one, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Stage and commit the whole fixture, so the tree holds a committed base for the walk
    /// to compare a replacement against.
    async fn commit_fixture(fixture: &TestRepository) {
        lore_revision::file::stage::stage(
            fixture.repository.clone(),
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
        lore_revision::commit::commit_boxed(
            fixture.repository.clone(),
            &fixture.write_token,
            lore_revision::commit::CommitOptions {
                message: String::new(),
                link_messages: std::collections::HashMap::new(),
                link: None,
                layer_messages: std::collections::HashMap::new(),
                layer: None,
            },
        )
        .await
        .expect("Failed to commit the fixture");
    }

    /// The staged flags `node` carries.
    async fn staged_flags(
        repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
        state: &std::sync::Arc<lore_revision::state::State>,
        node: lore_revision::node::NodeID,
    ) -> NodeFlags {
        let node = state
            .node(repository.clone(), node)
            .await
            .expect("the node must read back");
        NodeFlags::from_bits_retain(node.flags)
    }

    /// A file the file system replaced with a directory stages as both halves of the
    /// replacement: the displaced node carries the delete a commit needs to drop the old
    /// content, and the directory that took its place is staged along with what it holds.
    #[tokio::test]
    async fn a_staged_file_replaced_by_a_directory_settles_both() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                test_file_write(&fixture.path.join("thing"), b"a file first");
                commit_fixture(&fixture).await;

                std::fs::remove_file(fixture.path.join("thing")).expect("Remove file failed");
                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/inner.txt"), b"content below");

                let (current, staged) = test_anchor_states(&repository).await;
                let displaced = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the tree must hold the committed file")
                    .node;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let flags = staged_flags(&repository, &staged, displaced).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the displaced file must be staged for delete, flags {flags:?}"
                );

                let inner = changes
                    .iter()
                    .find(|change| change.path().as_str() == "thing/inner.txt")
                    .expect("the walk must descend into the directory that replaced the file");
                assert_eq!(FileAction::Add, inner.action);
                let flags = staged_flags(&repository, &staged, inner.to.mapping.node).await;
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "content below the replacement must be staged, flags {flags:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A staged replacement sits in the tree beside the node it displaced, both spelled the
    /// same. A second pass has to claim the replacement rather than the node carrying the
    /// delete, or it replaces the replacement.
    #[tokio::test]
    async fn staging_a_replacement_twice_replaces_it_once() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                test_file_write(&fixture.path.join("thing"), b"a file first");
                commit_fixture(&fixture).await;

                std::fs::remove_file(fixture.path.join("thing")).expect("Remove file failed");
                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/inner.txt"), b"content below");

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                let replacement = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the walk must mint the replacement")
                    .node;

                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let after = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the replacement must still be there")
                    .node;
                assert_eq!(
                    replacement, after,
                    "the second pass must claim the replacement, not the node it displaced"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory the file system replaced with a file stages the whole displaced subtree
    /// for delete, and the file that took its place as an add carrying its identity.
    #[tokio::test]
    async fn a_staged_directory_replaced_by_a_file_settles_both() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/inner.txt"), b"content below");
                commit_fixture(&fixture).await;

                std::fs::remove_dir_all(fixture.path.join("thing"))
                    .expect("Remove directory failed");
                test_file_write(&fixture.path.join("thing"), b"a file now");

                let (current, staged) = test_anchor_states(&repository).await;
                let displaced = staged
                    .find_node_link(repository.clone(), "thing")
                    .await
                    .expect("the tree must hold the committed directory")
                    .node;
                let displaced_child = staged
                    .find_node_link(repository.clone(), "thing/inner.txt")
                    .await
                    .expect("the tree must hold the committed file below it")
                    .node;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let flags = staged_flags(&repository, &staged, displaced).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the displaced directory must be staged for delete, flags {flags:?}"
                );
                let flags = staged_flags(&repository, &staged, displaced_child).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the whole displaced subtree must be staged for delete, flags {flags:?}"
                );

                let added = changes
                    .iter()
                    .find(|change| {
                        change.path().as_str() == "thing" && change.action == FileAction::Add
                    })
                    .expect("the walk must report the file that replaced the directory");
                let node = staged
                    .node(repository.clone(), added.to.mapping.node)
                    .await
                    .expect("the replacement must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedAdd),
                    "the replacement must be staged as an add, flags {flags:?}"
                );
                assert!(
                    node.is_file(),
                    "the replacement must take the type the file system holds, flags {flags:?}"
                );
                assert!(
                    !node.address.context.is_zero(),
                    "a staged file node must carry an identity"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Force reports what the view leaves out, all the way down a subtree it is reporting
    /// the delete of. A forced walk consults no filter slot, so the hierarchy under a change
    /// is reported whole rather than folded against rules force put it past.
    #[tokio::test]
    async fn force_reports_an_excluded_path_under_a_deleted_subtree() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = test_repository_excluding(
                    immutable_store,
                    mutable_store,
                    repository_id,
                    &["thing/hidden.txt"],
                    &[],
                )
                .await;
                let repository = fixture.repository.clone();
                std::fs::create_dir_all(fixture.path.join("thing"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("thing/shown.txt"), b"in view");
                test_file_write(&fixture.path.join("thing/hidden.txt"), b"out of view");
                force_commit_fixture(&fixture).await;

                // A file where the directory was, so the whole subtree is reported deleted
                // through the change hierarchy rather than through the walk.
                std::fs::remove_dir_all(fixture.path.join("thing"))
                    .expect("Remove directory failed");
                test_file_write(&fixture.path.join("thing"), b"a file now");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        test_scan_with_intent(
                            repository.clone(),
                            staged,
                            current,
                            FilesystemDiffIntent::Stage(StageIntent::default()),
                        ),
                    )
                    .await;

                for path in ["thing/shown.txt", "thing/hidden.txt"] {
                    assert!(
                        changes.iter().any(|change| change.path().as_str() == path
                            && change.action == FileAction::Delete),
                        "a forced walk must report {path} as deleted, reported {:?}",
                        changes
                            .iter()
                            .map(|change| (change.path().as_str(), change.action))
                            .collect::<Vec<_>>()
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A node already carrying its staged action is left alone, so staging the same tree
    /// twice reports the second pass as staging nothing.
    #[tokio::test]
    async fn staging_a_tree_twice_settles_it_once() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                test_file_write(&fixture.path.join("script.sh"), b"staged once");

                let (current, staged) = test_anchor_states(&repository).await;
                let first = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                assert!(
                    first
                        .iter()
                        .any(|change| change.path().as_str() == "script.sh"),
                    "the first pass must stage the file"
                );

                let second = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                assert!(
                    !second
                        .iter()
                        .any(|change| change.path().as_str() == "script.sh"),
                    "a node already staged must not be staged again, reported {:?}",
                    second.iter().map(|c| c.path().as_str()).collect::<Vec<_>>()
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A file the tree holds staged for delete and the file system still has is taken back:
    /// a staged delete the working tree contradicts would drop the file on commit.
    #[tokio::test]
    async fn staging_a_file_the_tree_holds_deleted_takes_the_delete_back() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                let script = fixture.path.join("script.sh");
                test_file_write(&script, b"committed");
                commit_fixture(&fixture).await;

                let (current, staged) = test_anchor_states(&repository).await;
                let node_id = staged
                    .find_node_link(repository.clone(), "script.sh")
                    .await
                    .expect("the tree must hold the committed file")
                    .node;

                std::fs::remove_file(&script).expect("Remove file failed");
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                let flags = staged_flags(&repository, &staged, node_id).await;
                assert!(
                    flags.contains(NodeFlags::StagedDelete),
                    "the fixture must stage the delete first, flags {flags:?}"
                );

                test_file_write(&script, b"back again");
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let flags = staged_flags(&repository, &staged, node_id).await;
                assert!(
                    !flags.contains(NodeFlags::StagedDelete),
                    "the delete must be taken back, flags {flags:?}"
                );
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "the file must be settled as a modification, flags {flags:?}"
                );
                assert!(
                    changes
                        .iter()
                        .any(|change| change.path().as_str() == "script.sh"
                            && change.action == FileAction::Add),
                    "the file must be reported back"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Force stages what the working copy already matches, the directory included: the user
    /// asked for those nodes to carry the action, and a walk that reported nothing would
    /// leave the ask unanswered.
    #[tokio::test]
    async fn force_stages_what_the_working_copy_already_matches() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                std::fs::create_dir_all(fixture.path.join("dir")).expect("Create directory failed");
                test_file_write(&fixture.path.join("dir/script.sh"), b"unchanged");
                commit_fixture(&fixture).await;

                let (current, staged) = test_anchor_states(&repository).await;
                let node_id = staged
                    .find_node_link(repository.clone(), "dir/script.sh")
                    .await
                    .expect("the tree must hold the committed file")
                    .node;
                let directory_id = staged
                    .find_node_link(repository.clone(), "dir")
                    .await
                    .expect("the tree must hold the committed directory")
                    .node;

                let changes = LORE_CONTEXT
                    .scope(
                        forced_execution(),
                        test_scan_with_intent(
                            repository.clone(),
                            staged.clone(),
                            current,
                            FilesystemDiffIntent::Stage(StageIntent::default()),
                        ),
                    )
                    .await;

                let flags = staged_flags(&repository, &staged, node_id).await;
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "force must settle the file it was asked to stage, flags {flags:?}"
                );
                let flags = staged_flags(&repository, &staged, directory_id).await;
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "force must settle the directory too, which holds no content to compare, flags {flags:?}"
                );
                for path in ["dir", "dir/script.sh"] {
                    assert!(
                        changes.iter().any(|change| change.path().as_str() == path),
                        "what force staged must be reported, missing {path}"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A staged modification records the size the file was measured with. The mode is left to
    /// the commit, which is what compares it against the one the revision holds.
    #[tokio::test]
    async fn a_staged_modification_records_what_the_file_carries() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                let script = fixture.path.join("script.sh");
                test_file_write(&script, b"short");
                commit_fixture(&fixture).await;

                let grown = b"a good deal longer than the committed content";
                test_file_write(&script, grown);

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let node_id = staged
                    .find_node_link(repository.clone(), "script.sh")
                    .await
                    .expect("the tree must hold the file")
                    .node;
                let node = staged
                    .node(repository.clone(), node_id)
                    .await
                    .expect("the staged node must read back");
                assert_eq!(
                    grown.len() as u64,
                    node.size,
                    "the node must record the size the file was measured with"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The executable bit is part of what a file is, so a change to it alone is a
    /// modification even where every byte of the content stands. A chmod moves neither the
    /// size nor the modification time, so the bit is what the walk has to compare.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_mode_change_alone_is_a_modification() {
        use std::os::unix::fs::PermissionsExt;

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                let script = fixture.path.join("script.sh");
                test_file_write(&script, b"#!/bin/sh\necho unchanged");
                commit_fixture(&fixture).await;

                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("Failed to set the executable bit");

                let (current, staged) = test_anchor_states(&repository).await;
                let marked = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                assert!(
                    marked
                        .iter()
                        .any(|change| change.path().as_str() == "script.sh"),
                    "a marking walk must report the file the chmod changed, reported {:?}",
                    marked.iter().map(|c| c.path().as_str()).collect::<Vec<_>>()
                );

                let (current, staged) = test_anchor_states(&repository).await;
                let node_id = staged
                    .find_node_link(repository.clone(), "script.sh")
                    .await
                    .expect("the tree must hold the committed file")
                    .node;
                test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let flags = staged_flags(&repository, &staged, node_id).await;
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "a mode change must be settled as a modification, flags {flags:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Staging leaves a merge sibling alone, which a resolution has not consumed yet. A
    /// marking walk reports it, since the working tree does hold it.
    #[tokio::test]
    async fn staging_leaves_a_merge_sibling_alone() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let sibling = format!("script.sh{}", lore_revision::repository::THEIRS_SUFFIX);
                test_file_write(&fixture.path.join(&sibling), b"their version");

                let (current, staged) = test_anchor_states(&repository).await;
                let staged_changes = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current.clone(),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;
                assert!(
                    !staged_changes
                        .iter()
                        .any(|change| change.path().as_str() == sibling),
                    "staging must leave the merge sibling alone"
                );

                let (_, staged) = test_anchor_states(&repository).await;
                let marked = test_scan_with_intent(
                    repository.clone(),
                    staged,
                    current,
                    FilesystemDiffIntent::MarkDirty,
                )
                .await;
                assert!(
                    marked
                        .iter()
                        .any(|change| change.path().as_str() == sibling),
                    "a marking walk must report what the working tree holds"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A path-scoped staging walk creates the ancestors its target needs, and those have
    /// to be staged along with it: `commit` discards a dirty-only add directory and the
    /// subtree under it, which would take the staged file with it.
    #[tokio::test]
    async fn a_staged_nested_path_settles_the_ancestors_it_creates() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();

                std::fs::create_dir_all(fixture.path.join("new-dir"))
                    .expect("Create directory failed");
                test_file_write(&fixture.path.join("new-dir/file.txt"), b"nested");

                let (current, staged) = test_anchor_states(&repository).await;
                test_scan_path_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    Some(
                        lore_revision::util::path::RelativePath::new_from_initial_path(
                            "new-dir/file.txt",
                        )
                        .expect("path"),
                    ),
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let directory = staged
                    .find_node_link(repository.clone(), "new-dir")
                    .await
                    .expect("the walk must create the ancestor");
                let node = staged
                    .node(repository.clone(), directory.node)
                    .await
                    .expect("the ancestor must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::Staged),
                    "an ancestor a staging walk creates must be staged, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A staging walk records a tracked file it finds changed as both staged and dirty
    /// modified: the dirty action stands beside the staged one, as every stage records it.
    #[tokio::test]
    async fn a_staging_walk_records_a_modify_staged_and_dirty() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture =
                    test_repository_create(immutable_store, mutable_store, repository_id).await;
                let repository = fixture.repository.clone();
                let file = fixture.path.join("tracked.txt");
                test_file_write(&file, b"committed");
                test_commit_tree(&fixture, "Seed").await;
                test_file_write(&file, b"changed since the commit");

                let (current, staged) = test_anchor_states(&repository).await;
                let changes = test_scan_with_intent(
                    repository.clone(),
                    staged.clone(),
                    current,
                    FilesystemDiffIntent::Stage(StageIntent::default()),
                )
                .await;

                let modified = changes
                    .iter()
                    .find(|change| change.path().as_str() == "tracked.txt")
                    .expect("the walk must report the changed file");
                let node = staged
                    .node(repository.clone(), modified.from.mapping.node)
                    .await
                    .expect("the staged node must read back");
                let flags = NodeFlags::from_bits_retain(node.flags);
                assert!(
                    flags.contains(NodeFlags::StagedModify),
                    "the staged action, flags {:x}",
                    node.flags
                );
                assert!(
                    flags.contains(NodeFlags::DirtyModify),
                    "the dirty action beside it, flags {:x}",
                    node.flags
                );
            }))
            .await
            .expect("Test task failed");
    }
}
