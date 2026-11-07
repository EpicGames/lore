// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::fs;
    use std::io::Write;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Context;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::ROOT_NODE;
    use lore_revision::repository;
    use lore_revision::revision::sync;
    use lore_revision::revision::sync::SyncOptions;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;

    include!("helper.rs");

    /// The file the executable bit test carries across two revisions.
    const SCRIPT: &str = "script.sh";
    const FIRST: &[u8] = b"#!/bin/sh\necho first";
    const SECOND: &[u8] = b"#!/bin/sh\necho second";

    /// Syncs `instance` to `revision`, discarding local modifications where `reset` asks it to.
    async fn sync_to(instance: &TestRepository, revision: lore_base::types::Hash, reset: bool) {
        sync::sync_boxed(
            instance.repository.clone(),
            &instance.write_token,
            SyncOptions {
                revision: Some(revision.to_string()),
                reset,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to sync to the revision");
    }

    /// A repository whose second revision rewrites [`SCRIPT`], answered beside the working tree
    /// standing on the first revision with the file marked executable by hand.
    #[cfg(target_family = "unix")]
    async fn a_chmodded_working_tree(
        immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    ) -> (TestRepository, lore_base::types::Hash) {
        use std::os::unix::fs::PermissionsExt;

        let instance = test_repository_create(
            immutable_store,
            mutable_store,
            RepositoryId::from(uuid::Uuid::now_v7()),
        )
        .await;
        let script = instance.path.join(SCRIPT);

        test_file_write(script.as_path(), FIRST);
        let first = test_commit_tree(&instance, "First").await.revision();
        test_file_write(script.as_path(), SECOND);
        let second = test_commit_tree(&instance, "Second").await.revision();

        sync_to(&instance, first, false).await;
        std::fs::set_permissions(script.as_path(), std::fs::Permissions::from_mode(0o755))
            .expect("Failed to set the executable bit");

        (instance, second)
    }

    /// Whether the working file at `path` carries the executable bit.
    #[cfg(target_family = "unix")]
    fn working_executable(path: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path)
            .expect("The working file must be readable")
            .permissions()
            .mode()
            & 0o111
            != 0
    }

    /// A chmod is a modification of the executable bit and nothing else, which the content an
    /// incoming revision carries answers nothing about. A sync to that revision writes the content
    /// and leaves the bit, so the change the user made stands as a local modification against the
    /// revision the tree lands on rather than being reverted by the write.
    #[cfg(target_family = "unix")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sync_writes_new_content_and_keeps_a_local_executable_bit() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let (instance, second) =
                    a_chmodded_working_tree(immutable_store, mutable_store).await;
                let script = instance.path.join(SCRIPT);

                sync_to(&instance, second, false).await;

                assert_eq!(
                    fs::read(script.as_path()).expect("The synced file must be readable"),
                    SECOND,
                    "the sync has to carry the content the revision it lands on holds"
                );
                assert!(
                    working_executable(script.as_path()),
                    "the bit the user set has to survive the write"
                );

                let (current, staged) = test_anchor_states(&instance.repository).await;
                let scanned =
                    test_reported(&test_scan(instance.repository.clone(), staged, current).await);
                assert!(
                    scanned.contains(&("M".to_string(), SCRIPT.to_string())),
                    "the bit has to stand as a local modification against the revision synced \
                     to, reported {scanned:?}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A reset discards local modifications, which the executable bit is one of: the file is left
    /// holding the mode its revision names.
    #[cfg(target_family = "unix")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_sync_discards_a_local_executable_bit() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let (instance, second) =
                    a_chmodded_working_tree(immutable_store, mutable_store).await;
                let script = instance.path.join(SCRIPT);

                sync_to(&instance, second, true).await;

                assert_eq!(
                    fs::read(script.as_path()).expect("The synced file must be readable"),
                    SECOND,
                    "a reset carries the content the revision it lands on holds"
                );
                assert!(
                    !working_executable(script.as_path()),
                    "a reset leaves the mode the revision names, the local bit with it"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A path that is a file in one revision and a directory in the next. A reset sync back to the
    /// file diffs the target against the working tree, where the directory and everything under
    /// it has to be reported so it is deleted before the file is written in its place; reporting
    /// only the directory left it in the way and the write failed with "Is a directory".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_sync_turns_a_directory_back_into_a_file() {
        const ENTRY: &str = "entry";
        const CONTENT: &[u8] = b"entry as a file";

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = Box::pin(test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                ))
                .await;
                let entry = instance.path.join(ENTRY);

                test_file_write(entry.as_path(), CONTENT);
                let as_file = test_commit_tree(&instance, "Entry is a file")
                    .await
                    .revision();

                fs::remove_file(entry.as_path()).expect("Failed to remove the file");
                fs::create_dir_all(entry.join("sub")).expect("Failed to create the directory");
                test_file_write(entry.join("inner").as_path(), b"inner");
                test_file_write(entry.join("sub").join("deep").as_path(), b"deep");
                let as_directory = test_commit_tree(&instance, "Entry is a directory")
                    .await
                    .revision();

                sync_to(&instance, as_file, true).await;
                assert_eq!(
                    fs::read(entry.as_path()).expect("The entry must be a readable file"),
                    CONTENT,
                    "a reset sync has to replace the directory with the file the revision holds"
                );

                sync_to(&instance, as_directory, true).await;
                assert_eq!(
                    fs::read(entry.join("sub").join("deep").as_path())
                        .expect("The nested file must be readable"),
                    b"deep",
                    "a reset sync forward has to bring the directory and its content back"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The directory that replaced the file is the root of a nested repository: the walk never
    /// descends into one, so a reset sync reports nothing under it, the non-recursive directory
    /// delete keeps it, and the sync fails without touching the nested repository's working files.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_sync_leaves_a_nested_repository_that_replaced_a_file_alone() {
        const ENTRY: &str = "entry";

        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = Box::pin(test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                ))
                .await;
                let entry = instance.path.join(ENTRY);

                test_file_write(entry.as_path(), b"entry as a file");
                let as_file = test_commit_tree(&instance, "Entry is a file")
                    .await
                    .revision();

                fs::remove_file(entry.as_path()).expect("Failed to remove the file");
                fs::create_dir_all(entry.join(".lore"))
                    .expect("Failed to mark a nested repository");
                test_file_write(entry.join("work").as_path(), b"another repository's work");

                let result = sync::sync_boxed(
                    instance.repository.clone(),
                    &instance.write_token,
                    SyncOptions {
                        revision: Some(as_file.to_string()),
                        reset: true,
                        ..Default::default()
                    },
                )
                .await;
                assert!(
                    result.is_err(),
                    "a nested repository in the way has to fail the sync rather than be emptied"
                );
                assert_eq!(
                    fs::read(entry.join("work").as_path())
                        .expect("The nested repository's file must still be there"),
                    b"another repository's work",
                    "a reset sync must not delete another repository's working files"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A reset sync to another revision carries its change to a committed file an ignore rule
    /// matches over a local edit, and its deletes of others, one with its directory, as a sync
    /// does. It restores one deleted locally and one a directory of build output replaced, and
    /// leaves untracked files alone whether a rule matches them or not, one in the deleted
    /// directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_sync_carries_a_revision_over_ignored_committed_files() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = Box::pin(test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                ))
                .await;
                let path = |relative: &str| instance.path.join(relative);

                fs::create_dir_all(path("saved/old")).expect("Failed to create the directories");
                for relative in [
                    "cache.tmp",
                    "gone.tmp",
                    "swap.tmp",
                    "saved/config.ini",
                    "saved/old/a.ini",
                ] {
                    test_file_write(&path(relative), b"committed");
                }
                let first = test_commit_tree(&instance, "Committed").await.revision();
                test_file_write(&path("cache.tmp"), b"incoming");
                fs::remove_file(path("gone.tmp")).expect("Failed to delete the file");
                fs::remove_dir_all(path("saved/old")).expect("Failed to delete the directory");
                let second = test_commit_tree(&instance, "Changed and deleted")
                    .await
                    .revision();

                sync_to(&instance, first, false).await;
                test_file_write(&path("cache.tmp"), b"local");
                fs::remove_file(path("saved/config.ini")).expect("Failed to delete the file");
                fs::remove_file(path("swap.tmp")).expect("Failed to delete the file");
                fs::create_dir_all(path("swap.tmp")).expect("Failed to create the directory");
                test_file_write(&path("swap.tmp/out.bin"), b"output");
                for relative in ["build.tmp", "notes.txt", "saved/old/run.log"] {
                    test_file_write(&path(relative), b"untracked");
                }

                let ignoring = test_filter_context(
                    &instance,
                    instance.repository.immutable_store(),
                    instance.repository.mutable_store.clone(),
                    &["*.tmp", "saved/"],
                    &[],
                );
                sync::sync_boxed(
                    ignoring,
                    &instance.write_token,
                    SyncOptions {
                        revision: Some(second.to_string()),
                        reset: true,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to reset the working tree");

                for (relative, expected) in [
                    ("cache.tmp", Some(&b"incoming"[..])),
                    ("saved/config.ini", Some(&b"committed"[..])),
                    ("swap.tmp", Some(&b"committed"[..])),
                    ("build.tmp", Some(&b"untracked"[..])),
                    ("notes.txt", Some(&b"untracked"[..])),
                    ("saved/old/run.log", Some(&b"untracked"[..])),
                    ("gone.tmp", None),
                    ("saved/old/a.ini", None),
                ] {
                    assert_eq!(
                        fs::read(path(relative)).ok().as_deref(),
                        expected,
                        "{relative} after the reset"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A reset sync counts an entry tracked only where the current revision holds one of the
    /// same name and kind, so it leaves alone untracked entries that took the place of a committed
    /// one: a file where a directory was, a directory where a file was, and a file named as the
    /// directory it sits in, which stands where the current revision holds a file of that name and
    /// the incoming one a directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_sync_counts_an_entry_tracked_only_as_the_kind_the_revision_holds() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = Box::pin(test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                ))
                .await;
                let path = |relative: &str| instance.path.join(relative);

                fs::create_dir_all(path("cache")).expect("Failed to create the directory");
                for relative in ["cache/a.bin", "old", "dir"] {
                    test_file_write(&path(relative), b"committed");
                }
                let first = test_commit_tree(&instance, "Committed").await.revision();
                fs::remove_dir_all(path("cache")).expect("Failed to delete the directory");
                fs::remove_file(path("old")).expect("Failed to delete the file");
                fs::remove_file(path("dir")).expect("Failed to delete the file");
                fs::create_dir_all(path("dir")).expect("Failed to create the directory");
                test_file_write(&path("dir/keep.ini"), b"incoming");
                let second = test_commit_tree(&instance, "Deleted and replaced")
                    .await
                    .revision();

                sync_to(&instance, first, false).await;
                fs::remove_dir_all(path("cache")).expect("Failed to delete the directory");
                test_file_write(&path("cache"), b"untracked");
                fs::remove_file(path("old")).expect("Failed to delete the file");
                fs::create_dir_all(path("old")).expect("Failed to create the directory");
                test_file_write(&path("old/old"), b"untracked");
                fs::remove_file(path("dir")).expect("Failed to delete the file");
                fs::create_dir_all(path("dir")).expect("Failed to create the directory");
                test_file_write(&path("dir/dir"), b"untracked");

                sync_to(&instance, second, true).await;

                for (relative, expected) in [
                    ("cache", &b"untracked"[..]),
                    ("old/old", b"untracked"),
                    ("dir/dir", b"untracked"),
                    ("dir/keep.ini", b"incoming"),
                ] {
                    assert_eq!(
                        fs::read(path(relative)).ok().as_deref(),
                        Some(expected),
                        "{relative} after the reset"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A reset sync to a revision that lacks a committed file deletes the entry spelled as the
    /// current revision holds it and leaves an untracked one spelled in another case beside it.
    /// Where the working tree holds the file in one other case only, that entry stands for it, and
    /// where it holds the file in several other cases, none does and all stay.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_sync_deletes_only_the_spelling_the_current_revision_tracks() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = Box::pin(test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                ))
                .await;
                let path = |relative: &str| instance.path.join(relative);

                test_file_write(&path("keep.txt"), b"committed");
                let first = test_commit_tree(&instance, "First").await.revision();
                for relative in ["Foo", "Bar", "Baz"] {
                    test_file_write(&path(relative), b"committed");
                }
                test_commit_tree(&instance, "Second").await;
                test_file_write(&path("foo"), b"untracked");
                fs::rename(path("Bar"), path("bar")).expect("Failed to rename the file");
                fs::rename(path("Baz"), path("baz")).expect("Failed to rename the file");
                test_file_write(&path("BAZ"), b"untracked");

                sync_to(&instance, first, true).await;

                for (relative, expected) in [
                    ("keep.txt", Some(&b"committed"[..])),
                    ("foo", Some(&b"untracked"[..])),
                    ("Foo", None),
                    ("bar", None),
                    ("baz", Some(&b"committed"[..])),
                    ("BAZ", Some(&b"untracked"[..])),
                ] {
                    assert_eq!(
                        fs::read(path(relative)).ok().as_deref(),
                        expected,
                        "{relative} after the reset"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A provider that holds its state in a `State` refuses a sync restricted to a dependency set,
    /// as it holds whole revisions, and one resetting local changes, before the instance moves.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_holding_its_state_refuses_a_dependency_set_and_a_reset() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                )
                .await;
                let script = instance.path.join(SCRIPT);
                test_file_write(script.as_path(), FIRST);
                let first = test_commit_tree(&instance, "First").await.revision();
                test_file_write(script.as_path(), SECOND);
                let second = test_commit_tree(&instance, "Second").await.revision();

                for refused in [
                    SyncOptions {
                        root_files: vec![SCRIPT.to_string()],
                        ..Default::default()
                    },
                    SyncOptions {
                        reset: true,
                        ..Default::default()
                    },
                ] {
                    let (holding, _) = test_holding_context(&instance.repository);
                    let result = sync::sync_boxed(
                        holding,
                        &instance.write_token,
                        SyncOptions {
                            revision: Some(first.to_string()),
                            ..refused
                        },
                    )
                    .await;

                    assert!(
                        matches!(result, Err(sync::SyncError::InvalidArguments(_))),
                        "the sync is refused, got {result:?}"
                    );
                    let (current, _) =
                        lore_revision::instance::load_current_anchor_boxed(&instance.repository)
                            .await
                            .expect("Failed to load current anchor");
                    assert_eq!(
                        current, second,
                        "the instance stays at the revision it stood on"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// The directory the case tests delete, and the file in it a revision renames in case alone.
    const PLUGIN: &str = "Plugin";
    const BUILD_RULES: &str = "VerseInput.Build.cs";
    const BUILD_RULES_RENAMED: &str = "VerseInput.build.cs";

    /// Commits `instance` holding [`BUILD_RULES`] in [`PLUGIN`] and then a revision renaming it to
    /// [`BUILD_RULES_RENAMED`], answering both.
    async fn commit_a_rename_in_case_alone(
        instance: &TestRepository,
    ) -> (lore_base::types::Hash, lore_base::types::Hash) {
        let directory = instance.path.join(PLUGIN);
        fs::create_dir_all(directory.as_path()).expect("Failed to create the directory");
        test_file_write(directory.join(BUILD_RULES).as_path(), b"build rules");
        let first = test_commit_tree(instance, "First").await.revision();

        fs::rename(
            directory.join(BUILD_RULES),
            directory.join(BUILD_RULES_RENAMED),
        )
        .expect("Failed to rename the file");
        file::stage::stage(
            instance.repository.clone(),
            &instance.write_token,
            LoreArray::from_vec(vec![LoreString::from(&instance.path)]),
            StageOptions {
                scan: true,
                case_change: stage::StageCaseChange::Rename,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to stage the rename");
        let second = test_commit(instance, "Second").await.revision();
        (first, second)
    }

    /// A file the working tree spells in another case than the revision does goes with the
    /// directory a merged branch deletes. The repository keeps its own stores, which hold the
    /// branch metadata a new branch reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_merge_deletes_a_directory_holding_a_case_variant_of_its_file() {
        let _ = test_store_create().await.expect("Failed to create stores");
        let offline =
            test_execution_with(lore_revision::interface::LoreGlobalArgs::default().set_offline());

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(offline, async move {
                let tempdir = generate_tempdir();
                let path = tempdir.to_path_buf();
                fs::create_dir_all(path.as_path()).expect("Create directory failed");
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let repository = repository::create_local(
                    path.as_path(),
                    &write_token,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                    Context::from(uuid::Uuid::now_v7()),
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to create repository");
                let instance = TestRepository {
                    repository,
                    write_token,
                    path,
                    _tempdir: tempdir,
                };
                let (first, second) = Box::pin(commit_a_rename_in_case_alone(&instance)).await;
                let directory = instance.path.join(PLUGIN);

                branch::create::create(
                    instance.repository.clone(),
                    &instance.write_token,
                    "feature".to_string(),
                    None,
                    String::new(),
                    false,
                )
                .await
                .expect("Failed to create the branch");
                let (_, feature) =
                    lore_revision::instance::load_current_anchor_boxed(&instance.repository)
                        .await
                        .expect("Failed to load the current anchor");
                fs::remove_dir_all(directory.as_path()).expect("Failed to remove the directory");
                test_commit_tree(&instance, "Delete the directory").await;

                sync_to(&instance, first, false).await;
                sync_to(&instance, second, false).await;
                fs::rename(
                    directory.join(BUILD_RULES_RENAMED),
                    directory.join(BUILD_RULES),
                )
                .expect("Failed to spell the file in another case");
                let unrelated = instance.path.join("unrelated.txt");
                test_file_write(unrelated.as_path(), b"main moves on");
                file::stage::stage(
                    instance.repository.clone(),
                    &instance.write_token,
                    LoreArray::from_vec(vec![LoreString::from(&unrelated)]),
                    StageOptions::default(),
                )
                .await
                .expect("Failed to stage the unrelated file");
                test_commit(&instance, "Main moves on").await;

                Box::pin(branch::merge::merge_start(
                    instance.repository.clone(),
                    &instance.write_token,
                    feature,
                    branch::merge::MergeStartOptions {
                        message: "merge feature into main".to_string(),
                        link_messages: std::collections::HashMap::new(),
                        no_commit: true,
                        scope: branch::merge::MergeScope::MainOnly,
                        inherit_metadata: lore_revision::metadata::MetadataInherit::default(),
                    },
                ))
                .await
                .expect("The merge has to take the directory");

                assert!(
                    !directory.exists(),
                    "the merge has to delete the directory with the file it holds"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The file a revision moves out of [`PLUGIN`] as it deletes the directory, the spelling the
    /// working tree holds it under, and the directory it moves to.
    #[cfg(target_os = "linux")]
    const RULES: &str = "Rules.cs";
    #[cfg(target_os = "linux")]
    const RULES_VARIANT: &str = "rules.cs";
    #[cfg(target_os = "linux")]
    const SHARED: &str = "Shared";

    /// Commits `instance` holding `content` as [`RULES`] in [`PLUGIN`] and then a revision moving
    /// it into [`SHARED`] as `carried` and deleting [`PLUGIN`], answering the second. The working
    /// tree is left on the first revision, holding `variant` as [`RULES_VARIANT`] in place of the
    /// file.
    #[cfg(target_os = "linux")]
    async fn a_move_out_of_a_deleted_directory(
        instance: &TestRepository,
        content: &[u8],
        carried: &[u8],
        variant: &[u8],
    ) -> lore_base::types::Hash {
        let directory = instance.path.join(PLUGIN);
        let shared = instance.path.join(SHARED);
        fs::create_dir_all(directory.as_path()).expect("Failed to create the directory");
        test_file_write(directory.join(RULES).as_path(), content);
        let first = test_commit_tree(instance, "First").await.revision();

        fs::create_dir_all(shared.as_path()).expect("Failed to create the destination");
        fs::rename(directory.join(RULES), shared.join(RULES)).expect("Failed to move the file");
        test_file_write(shared.join(RULES).as_path(), carried);
        file::stage::stage_move(
            instance.repository.clone(),
            &instance.write_token,
            directory.join(RULES).to_string_lossy().into_owned(),
            shared.join(RULES).to_string_lossy().into_owned(),
            StageOptions {
                scan: true,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to stage the move");
        fs::remove_dir(directory.as_path()).expect("Failed to remove the directory");
        file::stage::stage(
            instance.repository.clone(),
            &instance.write_token,
            LoreArray::from_vec(vec![LoreString::from(&directory)]),
            StageOptions {
                scan: true,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to stage the directory delete");
        let second = test_commit(instance, "Second").await.revision();

        sync_to(instance, first, false).await;
        fs::remove_file(directory.join(RULES)).expect("Failed to remove the file");
        test_file_write(directory.join(RULES_VARIANT).as_path(), variant);
        second
    }

    /// A case variant of a file a revision moves out of a directory it deletes goes with the
    /// directory once the move has written its destination, where it holds the content the move
    /// starts from or the content it carries.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_directory_takes_a_case_variant_of_a_file_moved_out_of_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let content: &[u8] = b"rules";
                let edited: &[u8] = b"rules, edited";
                for (carried, variant) in [(content, content), (edited, content), (edited, edited)]
                {
                    let instance = test_repository_create(
                        immutable_store.clone(),
                        mutable_store.clone(),
                        RepositoryId::from(uuid::Uuid::now_v7()),
                    )
                    .await;
                    let second = Box::pin(a_move_out_of_a_deleted_directory(
                        &instance, content, carried, variant,
                    ))
                    .await;

                    sync_to(&instance, second, false).await;

                    assert_eq!(
                        fs::read(instance.path.join(SHARED).join(RULES)).expect("The moved file"),
                        carried
                    );
                    assert!(
                        !instance.path.join(PLUGIN).exists(),
                        "A variant holding {} goes, the move carrying {}",
                        String::from_utf8_lossy(variant),
                        String::from_utf8_lossy(carried)
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A forced sync keeps a case variant of a file the revision moves out of a directory it
    /// deletes where the variant holds other content of the same size, and the directory with it.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_forced_sync_keeps_an_edited_case_variant_of_a_file_moved_out_of_a_deleted_directory()
    {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                )
                .await;
                let content = b"rules";
                let edited = b"mine!";
                let second = Box::pin(a_move_out_of_a_deleted_directory(
                    &instance, content, content, edited,
                ))
                .await;

                LORE_CONTEXT
                    .scope(forced_execution(), sync_to(&instance, second, false))
                    .await;

                assert_eq!(
                    fs::read(instance.path.join(SHARED).join(RULES)).expect("The moved file"),
                    content
                );
                assert_eq!(
                    fs::read(instance.path.join(PLUGIN).join(RULES_VARIANT))
                        .expect("The edited variant"),
                    edited
                );
            }))
            .await
            .expect("Test task failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_explicit_revision() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let tempdir = generate_tempdir();
        let temp_path = tempdir.to_path_buf();
        let path = temp_path.clone();

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                std::fs::create_dir_all(path.as_path()).expect("Create directory failed");
                let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
                let repository = repository::create_local(
                    path.as_path(),
                    &write_token,
                    repository_id,
                    Context::from(uuid::Uuid::now_v7()),
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("Failed to create repository");

                let file_path = path.as_path().join("test.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4])
                        .expect("Failed to write test file");
                }

                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Failed to stage file");

                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                };
                let first_signature =
                    commit::commit_boxed(repository.clone(), &write_token, options)
                        .await
                        .expect("Failed to commit revision");

                let other_file_path = path.as_path().join("second.test.file");
                {
                    let mut file = std::fs::File::options()
                        .create(true)
                        .truncate(true)
                        .read(true)
                        .write(true)
                        .open(other_file_path.as_path())
                        .expect("Failed to create test file");
                    file.write_all(&[0, 1, 2, 3, 4])
                        .expect("Failed to write test file");
                }

                let _signature = file::stage::stage(
                    repository.clone(),
                    &write_token,
                    LoreArray::from_vec(vec![LoreString::from(&path)]),
                    StageOptions {
                        case_change: stage::StageCaseChange::Error,
                        node_flags: NodeFlags::NoFlags,
                        file_id: None,
                        no_children: false,
                        scan: true,
                    },
                )
                .await
                .expect("Failed to stage file");

                let options = CommitOptions {
                    message: String::new(),
                    link_messages: std::collections::HashMap::new(),
                    link: None,
                    layer_messages: std::collections::HashMap::new(),
                    layer: None,
                };
                let second_signature =
                    commit::commit_boxed(repository.clone(), &write_token, options)
                        .await
                        .expect("Failed to commit revision");

                // Sync back to first revision
                sync::sync_boxed(
                    repository.clone(),
                    &write_token,
                    SyncOptions {
                        revision: Some(first_signature.to_string()),
                        filter_mode: lore_revision::filter::FilterMode::Full,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to sync back to first revision");

                // Verify file added in first revision is still there
                assert!(
                    fs::metadata(file_path.as_path())
                        .expect("Failed to find first file as expected")
                        .is_file()
                );

                // Verify file added in second revision is gone
                fs::metadata(other_file_path.as_path()).expect_err(
                    "File added in second revision was not removed as expected after sync back",
                );

                // Sync forward to second revision
                sync::sync_boxed(
                    repository.clone(),
                    &write_token,
                    SyncOptions {
                        revision: Some(second_signature.to_string()),
                        filter_mode: lore_revision::filter::FilterMode::Full,
                        ..Default::default()
                    },
                )
                .await
                .expect("Failed to sync forward to second revision");

                // Verify file added in first revision is still there
                assert!(
                    fs::metadata(file_path.as_path())
                        .expect("Failed to find first file as expected")
                        .is_file()
                );

                // Verify file added in second revision is restored
                assert!(
                    fs::metadata(other_file_path.as_path())
                        .expect("Failed to find first file as expected")
                        .is_file()
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The node a scan recorded for an untracked file leaves the staged tree once an ignore rule
    /// excludes the file.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_scanned_untracked_entry_drops_out_once_ignored() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        #[allow(clippy::disallowed_methods)]
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = test_repository_create(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    RepositoryId::from(uuid::Uuid::now_v7()),
                )
                .await;
                test_file_write(instance.path.join("seed.txt").as_path(), b"seed");
                test_commit_tree(&instance, "First").await;

                test_file_write(instance.path.join("built.log").as_path(), b"log");

                let (current, staged) = test_anchor_states(&instance.repository).await;
                test_scan(instance.repository.clone(), staged.clone(), current.clone()).await;
                assert!(
                    !children_named(&instance.repository, &staged, ROOT_NODE, "built.log")
                        .await
                        .is_empty(),
                    "the scan records the untracked file"
                );

                let ignoring = test_filter_context(
                    &instance,
                    immutable_store,
                    mutable_store,
                    &["built.log"],
                    &[],
                );
                test_scan(ignoring, staged.clone(), current).await;
                assert!(
                    children_named(&instance.repository, &staged, ROOT_NODE, "built.log")
                        .await
                        .is_empty(),
                    "its node leaves the staged tree once the file is ignored"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
