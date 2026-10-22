// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// A fixture edits its working tree directly, outside any repository write.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::event::LoreEvent;
use lore_revision::filter::Filter;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::HoldOptions;
use lore_revision::fs::hold;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::ROOT_NODE;
use lore_revision::relay::EventDispatcher;
use lore_revision::repository::RepositoryContext;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::state;
use lore_revision::state::State;
use parking_lot::Mutex;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::TestRepository;
use crate::helper::default_repository_creation_args;
use crate::helper::test_commit;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;

/// A repository and the revisions its fixture committed, the first holding `a.txt`,
/// `untouched.txt`, `dir/b.txt` and `dir/c.txt`.
struct Revisions {
    instance: TestRepository,
    first: Arc<State>,
    /// `a.txt` changed, `dir/` deleted and `new.txt` added.
    second: Arc<State>,
}

impl Revisions {
    fn repository(&self) -> Arc<RepositoryContext> {
        self.instance.repository.clone()
    }

    /// A State of `revision` to hold, apart from the one the fixture committed.
    async fn held(&self, revision: &State) -> Arc<State> {
        State::deserialize(self.repository(), revision.revision())
            .await
            .expect("the held State")
    }

    /// The fixture's repository under a view excluding `line`.
    fn excluding(&self, line: &str) -> RepositoryContext {
        let mut filter = Filter::default();
        filter.view.add_exclusion(line).expect("a view line");
        RepositoryContext::new(
            default_repository_creation_args(
                self.repository().immutable_store(),
                self.repository()
                    .try_mutable_store_arc()
                    .expect("the fixture's mutable store"),
            )
            .with_filter(Arc::new(filter)),
        )
    }

    /// The unfiltered diff from the first revision to `to`.
    async fn diff(&self, to: &Arc<State>) -> Vec<NodeChange> {
        state::diff_collect(
            self.repository(),
            self.first.clone(),
            self.repository(),
            to.clone(),
            None,
            FilterMode::empty(),
        )
        .await
        .expect("the diff")
    }

    /// Writes (`Some`) or deletes (`None`) each path, then commits the whole tree.
    async fn commit(&self, edits: &[(&str, Option<&[u8]>)]) -> Arc<State> {
        for (name, contents) in edits {
            let full = self.instance.path.join(name);
            match contents {
                Some(contents) => {
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent).expect("a parent directory");
                    }
                    test_file_write(&full, contents);
                }
                None => std::fs::remove_file(&full).expect("remove a fixture file"),
            }
        }
        test_commit_tree(&self.instance, "edit").await
    }
}

/// Runs `work` over the [`Revisions`] fixture.
async fn with_revisions(work: impl AsyncFnOnce(Revisions)) {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("test stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            std::fs::create_dir_all(instance.path.join("dir")).expect("dir/");
            for (name, contents) in [
                ("a.txt", &b"a"[..]),
                ("untouched.txt", b"u"),
                ("dir/b.txt", b"b"),
                ("dir/c.txt", b"c"),
            ] {
                test_file_write(&instance.path.join(name), contents);
            }
            let first = test_commit_tree(&instance, "first").await;
            test_file_write(&instance.path.join("a.txt"), b"a, changed");
            std::fs::remove_dir_all(instance.path.join("dir")).expect("remove dir/");
            test_file_write(&instance.path.join("new.txt"), b"new");
            let second = test_commit_tree(&instance, "second").await;
            work(Revisions {
                instance,
                first,
                second,
            })
            .await;
        })
        .await;
}

/// Marks the node at `path` of `held` with the local change `flags`, and its ancestors dirty.
async fn mark_locally(
    repository: &Arc<RepositoryContext>,
    held: &State,
    path: &str,
    flags: NodeFlags,
) {
    let node = held
        .find_node_link(repository.clone(), path)
        .await
        .expect("the marked path")
        .node;
    held.node_mark_dirty(repository.clone(), node, flags, true)
        .await
        .expect("mark the node dirty");
}

/// Adds to `held` a local node named `name` below the root, a file where `file` and a directory
/// otherwise.
async fn add_locally(repository: &Arc<RepositoryContext>, held: &State, name: &str, file: bool) {
    let mut flags = NodeFlags::DirtyAdd;
    if file {
        flags |= NodeFlags::File;
    }
    let node = Node {
        flags: flags.bits(),
        name_hash: lore_storage::hash::hash_string(name),
        ..Default::default()
    };
    held.node_add(repository.clone(), ROOT_NODE, node, name)
        .await
        .expect("add the local node");
}

/// What [`hold::check`] answers for `changes` against `held` under `options`.
async fn check(
    repository: &Arc<RepositoryContext>,
    held: &Arc<State>,
    changes: &[NodeChange],
    options: HoldOptions,
) -> Result<(), FsError> {
    let mut paths = hold::Resolver::new((repository, held));
    hold::check(&mut paths, changes, options).await
}

fn paths_of(changes: &[NodeChange]) -> Vec<(FileAction, String)> {
    changes
        .iter()
        .map(|change| (change.action, change.path().to_string()))
        .collect()
}

#[tokio::test]
async fn a_held_state_at_the_target_takes_no_changes() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.second).await;
        let changes = hold::changes(
            (&repository, &held),
            (&repository, &r.second),
            &BTreeMap::new(),
        )
        .await
        .expect("the changes");
        assert!(changes.is_empty(), "{:?}", paths_of(&changes));
    })
    .await;
}

/// The changes are the unfiltered diff from the revision the held State stands at, whatever its
/// local changes.
#[tokio::test]
async fn the_changes_are_the_diff_from_the_held_revision() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "a.txt", NodeFlags::DirtyModify).await;
        add_locally(&repository, &held, "local.txt", true).await;
        let changes = hold::changes(
            (&repository, &held),
            (&repository, &r.second),
            &BTreeMap::new(),
        )
        .await
        .expect("the changes");
        assert_eq!(paths_of(&changes), paths_of(&r.diff(&r.second).await));
        assert!(!changes.is_empty());
    })
    .await;
}

/// A path an apply that did not complete changed is re-placed from what the held State holds
/// there: set where it may hold what the target does, added back where it may lack it.
#[tokio::test]
async fn an_unsettled_path_is_re_placed() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.second).await;
        for (adds_back, action) in [(false, FileAction::Keep), (true, FileAction::Add)] {
            let unsettled = BTreeMap::from([("a.txt".to_string(), adds_back)]);
            let changes = hold::changes((&repository, &held), (&repository, &r.second), &unsettled)
                .await
                .expect("the changes");
            assert_eq!(paths_of(&changes), vec![(action, "a.txt".to_string())]);
        }
    })
    .await;
}

/// The view decides which changes a hold reports: a change outside it is dropped.
#[tokio::test]
async fn the_changes_in_view_drop_what_the_view_excludes() {
    with_revisions(async |r: Revisions| {
        let changes = r.diff(&r.second).await;
        let shown = hold::in_view(&r.excluding("dir/"), &changes);
        assert!(
            shown
                .iter()
                .all(|change| !change.path().as_str().starts_with("dir")),
            "{:?}",
            paths_of(&shown)
        );
        assert_eq!(
            shown.len(),
            changes
                .iter()
                .filter(|change| !change.path().as_str().starts_with("dir"))
                .count()
        );
        assert!(matches!(
            hold::in_view(&r.repository(), &changes),
            std::borrow::Cow::Borrowed(_)
        ));
    })
    .await;
}

#[tokio::test]
async fn a_held_state_without_local_changes_passes_the_check() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await
        .expect("nothing local to refuse");
    })
    .await;
}

/// A move shows where either of its sides does: a file moved into a directory the view excludes
/// stays among the changes in view.
#[tokio::test]
async fn a_move_out_of_the_view_stays_in_view() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let from = r.instance.path.join("untouched.txt");
        let to = r.instance.path.join("hidden/untouched.txt");
        std::fs::create_dir_all(r.instance.path.join("hidden")).expect("hidden/");
        std::fs::rename(&from, &to).expect("move untouched.txt");
        lore_revision::file::stage::stage_move(
            repository.clone(),
            &r.instance.write_token,
            from.to_string_lossy().into_owned(),
            to.to_string_lossy().into_owned(),
            StageOptions {
                case_change: StageCaseChange::Error,
                node_flags: NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            },
        )
        .await
        .expect("stage the move");
        let moved = test_commit(&r.instance, "move").await;
        let held = r.held(&r.second).await;
        let changes = hold::changes(
            (&repository, &held),
            (&repository, &moved),
            &BTreeMap::new(),
        )
        .await
        .expect("the changes");
        let is_the_move = |change: &NodeChange| {
            change
                .move_source()
                .is_some_and(|source| source.as_str() == "untouched.txt")
        };
        assert!(changes.iter().any(is_the_move), "{:?}", paths_of(&changes));
        let shown = hold::in_view(&r.excluding("hidden/"), &changes);
        assert!(shown.iter().any(is_the_move), "{:?}", paths_of(&shown));
    })
    .await;
}

/// A local edit the target changes refuses the hold unless it keeps or overwrites local changes.
#[tokio::test]
async fn a_local_edit_the_target_changes_refuses_the_hold() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "a.txt", NodeFlags::DirtyModify).await;
        let changes = r.diff(&r.second).await;
        let refused = check(&repository, &held, &changes, HoldOptions::default()).await;
        assert!(
            matches!(refused, Err(FsError::LocalModifications(_))),
            "{refused:?}"
        );
        for options in [
            HoldOptions {
                keep_local: true,
                ..Default::default()
            },
            HoldOptions {
                overwrite_local: true,
                ..Default::default()
            },
        ] {
            check(&repository, &held, &changes, options)
                .await
                .expect("kept or overwritten");
        }
    })
    .await;
}

#[tokio::test]
async fn a_local_edit_the_target_leaves_passes_the_check() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "untouched.txt", NodeFlags::DirtyModify).await;
        check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await
        .expect("the edit stays where the target changes nothing");
    })
    .await;
}

#[tokio::test]
async fn a_local_edit_in_a_directory_the_target_deletes_refuses_the_hold() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "dir/b.txt", NodeFlags::DirtyModify).await;
        let refused = check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await;
        assert!(
            matches!(refused, Err(FsError::LocalModifications(_))),
            "{refused:?}"
        );
    })
    .await;
}

/// A file added locally in a directory the target deletes stays with its directory, so it
/// refuses nothing.
#[tokio::test]
async fn a_local_addition_in_a_directory_the_target_deletes_passes_the_check() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        let dir = held
            .find_node_link(repository.clone(), "dir")
            .await
            .expect("dir/")
            .node;
        let added = held
            .node_add(
                repository.clone(),
                dir,
                Node {
                    flags: NodeFlags::File.bits(),
                    ..Node::default()
                },
                "added.txt",
            )
            .await
            .expect("add a file");
        held.node_mark_dirty(repository.clone(), added, NodeFlags::DirtyAdd, true)
            .await
            .expect("mark the addition");
        check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await
        .expect("a local addition refuses nothing");
    })
    .await;
}

/// A file the user deleted, held as no node, that the target changes is restored by a hold
/// keeping no local change, so the deletion refuses nothing.
#[tokio::test]
async fn a_local_deletion_of_a_file_the_target_changes_passes_the_check() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let target = r
            .commit(&[
                ("dir/b.txt", Some(b"b, changed")),
                ("dir/c.txt", Some(b"c")),
            ])
            .await;
        let deleted = r.commit(&[("dir/b.txt", None)]).await;
        let held = r.held(&deleted).await;
        mark_locally(&repository, &held, "untouched.txt", NodeFlags::DirtyModify).await;
        let changes = r.diff(&target).await;
        assert!(
            paths_of(&changes).contains(&(FileAction::Keep, "dir/b.txt".to_string())),
            "{:?}",
            paths_of(&changes)
        );
        check(&repository, &held, &changes, HoldOptions::default())
            .await
            .expect("a local deletion refuses nothing");
    })
    .await;
}

/// A file the user deleted, held as a node marked deleted, refuses nothing where the target
/// changes it or deletes its directory: a hold that keeps no local change restores it.
#[tokio::test]
async fn a_local_deletion_held_as_a_deleted_node_passes_the_check() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "a.txt", NodeFlags::DirtyDelete).await;
        mark_locally(&repository, &held, "dir/b.txt", NodeFlags::DirtyDelete).await;
        check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await
        .expect("a local deletion refuses nothing");
    })
    .await;
}

/// A tracked file the user replaced with a directory, held as the file marked deleted beside the
/// added directory under the same name, refuses a target change to the file: the directory is the
/// live node, and it is local.
#[tokio::test]
async fn a_local_replacement_beside_the_node_it_displaced_refuses_the_hold() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "a.txt", NodeFlags::DirtyDelete).await;
        add_locally(&repository, &held, "a.txt", false).await;
        let refused = check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await;
        assert!(
            matches!(refused, Err(FsError::LocalModifications(_))),
            "{refused:?}"
        );
    })
    .await;
}

/// A directory the user added where the target adds a file refuses the hold: the file would
/// replace the directory.
#[tokio::test]
async fn a_local_directory_where_the_target_adds_a_file_refuses_the_hold() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        add_locally(&repository, &held, "new.txt", false).await;
        let refused = check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await;
        assert!(
            matches!(refused, Err(FsError::LocalModifications(_))),
            "{refused:?}"
        );
    })
    .await;
}

/// A change below a directory the user replaced with a file refuses the hold: applying it needs
/// the directory back in place of the local file.
#[tokio::test]
async fn a_change_below_a_directory_replaced_by_a_local_file_refuses_the_hold() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let target = r
            .commit(&[
                ("dir/b.txt", Some(b"b, changed")),
                ("dir/c.txt", Some(b"c")),
            ])
            .await;
        let held = r.held(&r.first).await;
        for path in ["dir", "dir/b.txt", "dir/c.txt"] {
            mark_locally(&repository, &held, path, NodeFlags::DirtyDelete).await;
        }
        add_locally(&repository, &held, "dir", true).await;
        let changes: Vec<NodeChange> = r
            .diff(&target)
            .await
            .into_iter()
            .filter(|change| change.path().as_str() == "dir/b.txt")
            .collect();
        assert_eq!(
            paths_of(&changes),
            [(FileAction::Keep, "dir/b.txt".to_string())]
        );
        let refused = check(&repository, &held, &changes, HoldOptions::default()).await;
        assert!(
            matches!(refused, Err(FsError::LocalModifications(_))),
            "{refused:?}"
        );
    })
    .await;
}

/// A local edit below a directory marked deleted refuses the target's deletion of it: any dirty
/// child of the root is checked, whatever its mark.
#[tokio::test]
async fn a_local_edit_below_a_directory_marked_deleted_refuses_the_hold() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "dir", NodeFlags::DirtyDelete).await;
        mark_locally(&repository, &held, "dir/b.txt", NodeFlags::DirtyModify).await;
        let refused = check(
            &repository,
            &held,
            &r.diff(&r.second).await,
            HoldOptions::default(),
        )
        .await;
        assert!(
            matches!(refused, Err(FsError::LocalModifications(_))),
            "{refused:?}"
        );
    })
    .await;
}

/// A local edit in a directory the target moves refuses the hold: the edited file's move takes it
/// from where the user edited it.
#[tokio::test]
async fn a_local_edit_in_a_directory_the_target_moves_refuses_the_hold() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let third = r
            .commit(&[("dir/b.txt", Some(b"b")), ("dir/c.txt", Some(b"c"))])
            .await;
        let from = r.instance.path.join("dir");
        let to = r.instance.path.join("moved");
        std::fs::rename(&from, &to).expect("move dir/");
        lore_revision::file::stage::stage_move(
            repository.clone(),
            &r.instance.write_token,
            from.to_string_lossy().into_owned(),
            to.to_string_lossy().into_owned(),
            StageOptions {
                case_change: StageCaseChange::Error,
                node_flags: NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            },
        )
        .await
        .expect("stage the move");
        let fourth = test_commit(&r.instance, "move").await;
        let held = r.held(&third).await;
        mark_locally(&repository, &held, "dir/b.txt", NodeFlags::DirtyModify).await;
        let changes = hold::changes(
            (&repository, &held),
            (&repository, &fourth),
            &BTreeMap::new(),
        )
        .await
        .expect("the changes");
        assert!(
            changes.iter().any(|change| change
                .move_source()
                .is_some_and(|source| source.as_str() == "dir/b.txt")),
            "{:?}",
            paths_of(&changes)
        );
        let refused = check(&repository, &held, &changes, HoldOptions::default()).await;
        assert!(
            matches!(refused, Err(FsError::LocalModifications(_))),
            "{refused:?}"
        );
    })
    .await;
}

/// The check reads each directory once, however many of its paths it checks: a second path in a
/// directory already read costs the read of its own node alone.
#[tokio::test]
async fn the_check_reads_a_directory_once_however_many_of_its_paths_it_checks() {
    with_revisions(async |r: Revisions| {
        let repository = r.repository();
        let target = r
            .commit(&[
                ("dir/b.txt", Some(b"b, changed")),
                ("dir/c.txt", Some(b"c, changed")),
            ])
            .await;
        let held = r.held(&r.first).await;
        mark_locally(&repository, &held, "untouched.txt", NodeFlags::DirtyModify).await;
        let changes: Vec<NodeChange> = r
            .diff(&target)
            .await
            .into_iter()
            .filter(|change| change.path().as_str().starts_with("dir/"))
            .collect();
        assert_eq!(
            paths_of(&changes),
            [
                (FileAction::Keep, "dir/b.txt".to_string()),
                (FileAction::Keep, "dir/c.txt".to_string()),
            ]
        );
        let reads = async |changes: &[NodeChange]| {
            let mut paths = hold::Resolver::new((&repository, &held));
            hold::check(&mut paths, changes, HoldOptions::default())
                .await
                .expect("nothing local is changed");
            paths.reads()
        };
        assert_eq!(
            reads(&changes).await,
            reads(&changes[..1]).await + 1,
            "the second path reads its own node alone"
        );
    })
    .await;
}

/// The report sends an event for each change, carrying the target's size, and the totals.
#[tokio::test]
async fn the_report_sends_each_change_and_the_totals() {
    with_revisions(async |r: Revisions| {
        let changes = r.diff(&r.second).await;
        let files = Arc::new(Mutex::new(Vec::new()));
        let totals = Arc::new(Mutex::new(None));
        let execution = Arc::new(ExecutionContext::new_client_with_user_id(
            LoreGlobalArgs::default(),
            EventDispatcher::new(Some(Box::new({
                let (files, totals) = (files.clone(), totals.clone());
                move |event: &LoreEvent| match event {
                    LoreEvent::RevisionSyncFile(file) => {
                        files.lock().push((file.path.to_string(), file.size));
                    }
                    LoreEvent::RevisionSyncProgress(progress) => {
                        *totals.lock() = Some((
                            progress.file_update,
                            progress.bytes_update,
                            progress.file_delete,
                            progress.discovery_complete,
                        ));
                    }
                    _ => {}
                }
            }))),
            "test-user".to_string(),
        ));
        LORE_CONTEXT
            .scope(execution.clone(), hold::report(&changes))
            .await
            .expect("the report");
        execution.dispatcher.drain().await;

        let files = files.lock().clone();
        assert_eq!(files.len(), changes.len());
        assert!(files.contains(&("a.txt".to_string(), 10)), "{files:?}");
        assert!(files.contains(&("new.txt".to_string(), 3)), "{files:?}");
        let deleted = changes
            .iter()
            .filter(|change| change.action == FileAction::Delete)
            .count();
        assert_eq!(*totals.lock(), Some((2, 13, deleted, 1)));
    })
    .await;
}
