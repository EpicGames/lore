// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// A fixture builds the working tree directly; what these test is what the revision holds.
#![allow(clippy::disallowed_methods)]
//! What status, stage, commit, unstage and reset make of the removal of a directory that a
//! repository instance holds under its view, where the directory holds what the view leaves out.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::runtime::LORE_CONTEXT;
use lore_revision::change::FileAction;
use lore_revision::commit::CommitOptions;
use lore_revision::commit::commit_boxed;
use lore_revision::event::LoreEvent;
use lore_revision::file::reset::ResetOptions;
use lore_revision::file::unstage::UnstageOptions;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::interface::LoreString;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::relay::EventDispatcher;
use lore_revision::stage::Absence;
use lore_revision::stage::StageOptions;
use lore_revision::stage::StageStats;
use lore_revision::stage::stage_absent_delete;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use parking_lot::Mutex;

use crate::dirty_move_fixture::assert_no_change_marked;
use crate::dirty_move_fixture::committed;
use crate::dirty_move_fixture::node_at;
use crate::dirty_move_fixture::removed_directory;
use crate::dirty_move_fixture::scanned;
use crate::dirty_move_fixture::status_changes;
use crate::dirty_move_fixture::tree_paths;
use crate::dirty_move_fixture::with_instance;
use crate::dirty_move_fixture::written;
use crate::helper::TestRepository;
use crate::helper::mount_intermediate_at;
use crate::helper::test_commit;
use crate::helper::test_filter;
use crate::helper::test_staged_by_intent;

/// A view that keeps `a/b` and leaves out everything in it, keeping `k.txt` and `o` beside it.
const EMPTIED_VIEW: &[&str] = &["**", "!/a/b/", "/a/b/*", "!/k.txt", "!/o/"];
/// The files [`EMPTIED_VIEW`] is cloned over: it leaves out those in `a/b`, `a/h.txt` and
/// `h/z.txt`.
const EMPTIED_FILES: &[&str] = &[
    "a/b/x.txt",
    "a/b/c/y.txt",
    "a/h.txt",
    "h/z.txt",
    "k.txt",
    "o/p.txt",
];
/// What cloning under [`EMPTIED_VIEW`] leaves out of the working tree of [`EMPTIED_FILES`].
const EMPTIED_LEFT_OUT: &[&str] = &["a/b/x.txt", "a/b/c", "a/h.txt", "h"];

/// A directory the instance holds under a view, which a test removes from the working tree: `a`,
/// which the view excludes and which holds the kept `a/b`, `a/b` itself, `a` holding the kept
/// `a/v.txt` beside `a/b`, `src`, which holds `src/x.txt`, which the view keeps, and `src/y.bat`,
/// which `*.bat` leaves out, `bin`, which the view keeps and which holds only `bin/y.bat`, which
/// `*.bat` leaves out, `foo/bar/something`, which the view keeps and which holds only the excluded
/// `specific`, and `a/b` replaced by a file the view leaves out.
#[derive(Clone, Copy, Debug)]
enum Removed {
    ExcludedParent,
    Emptied,
    ExcludedHoldingFile,
    Kept,
    OnlyHiddenFiles,
    OnlyHiddenDirectory,
    ReplacedByFile,
}

impl Removed {
    const EACH: [Self; 6] = [
        Self::ExcludedParent,
        Self::Emptied,
        Self::ExcludedHoldingFile,
        Self::Kept,
        Self::OnlyHiddenFiles,
        Self::OnlyHiddenDirectory,
    ];

    /// The view, the files committed, what cloning under the view leaves out of the working tree,
    /// and the directory removed.
    fn case(
        self,
    ) -> (
        Vec<&'static str>,
        Vec<&'static str>,
        &'static [&'static str],
        &'static str,
    ) {
        match self {
            Self::ExcludedParent => (
                EMPTIED_VIEW.to_vec(),
                EMPTIED_FILES.to_vec(),
                EMPTIED_LEFT_OUT,
                "a",
            ),
            Self::Emptied | Self::ReplacedByFile => (
                EMPTIED_VIEW.to_vec(),
                EMPTIED_FILES.to_vec(),
                EMPTIED_LEFT_OUT,
                "a/b",
            ),
            Self::ExcludedHoldingFile => (
                [EMPTIED_VIEW, &["!/a/v.txt"]].concat(),
                [EMPTIED_FILES, &["a/v.txt"]].concat(),
                EMPTIED_LEFT_OUT,
                "a",
            ),
            Self::Kept => (
                vec!["*.bat"],
                vec!["src/x.txt", "src/y.bat", "k.txt"],
                &["src/y.bat"],
                "src",
            ),
            Self::OnlyHiddenFiles => (
                vec!["*.bat"],
                vec!["bin/y.bat", "k.txt"],
                &["bin/y.bat"],
                "bin",
            ),
            Self::OnlyHiddenDirectory => (
                vec!["/foo/", "!/foo/bar/", "/foo/bar/something/specific/"],
                vec![
                    "foo/bar/b.txt",
                    "foo/bar/something/specific/s.txt",
                    "foo/top.txt",
                    "k.txt",
                ],
                &["foo/top.txt", "foo/bar/something/specific"],
                "foo/bar/something",
            ),
        }
    }
}

/// How a case reaches staging: through `lore dirty .` and staging the marks, or through
/// `lore status --scan` and `lore stage --scan .`.
#[derive(Clone, Copy, Debug)]
enum Reached {
    Marked,
    Scanned,
}

impl Reached {
    const EACH: [Self; 2] = [Self::Marked, Self::Scanned];
}

/// A removed directory in a tree that registers a link: `a` under [`EMPTIED_VIEW`] and `src` under
/// `*.bat`, each beside a link at `z/deep/lib`, which the view leaves out, and `a` holding a link
/// at `a/lib` beside `a/v.txt`, both of which the view keeps.
#[derive(Clone, Copy, Debug)]
enum Linked {
    ExcludedParent,
    Kept,
    LinkBelow,
}

impl Linked {
    const EACH: [Self; 3] = [Self::ExcludedParent, Self::Kept, Self::LinkBelow];

    /// The view, the files committed, what cloning under the view leaves out of the working tree,
    /// the directory removed, the link's mount path, and the deletions staging the removal stages
    /// at or below the directory.
    #[allow(clippy::type_complexity)]
    fn case(
        self,
    ) -> (
        Vec<&'static str>,
        Vec<&'static str>,
        &'static [&'static str],
        &'static str,
        &'static str,
        Vec<&'static str>,
    ) {
        match self {
            Self::ExcludedParent => (
                EMPTIED_VIEW.to_vec(),
                [EMPTIED_FILES, &["z/deep/f.txt"]].concat(),
                &["a/b/x.txt", "a/b/c", "a/h.txt", "h", "z"],
                "a",
                "z/deep/lib",
                vec![],
            ),
            Self::Kept => (
                vec!["*.bat", "/z/"],
                vec!["src/x.txt", "src/y.bat", "k.txt", "z/deep/f.txt"],
                &["src/y.bat", "z"],
                "src",
                "z/deep/lib",
                vec!["src/x.txt"],
            ),
            Self::LinkBelow => (
                vec!["**", "!/a/", "!/a/**", "!/k.txt"],
                vec!["a/v.txt", "k.txt"],
                &[],
                "a",
                "a/lib",
                vec!["a/v.txt"],
            ),
        }
    }
}

/// An instance under the view of `removed`, with the files of its case committed, the working
/// tree holding what cloning under the view creates, less the removed directory, and that
/// removal reported as `reached` says. Answers the committed revision.
async fn removed_under_view(
    instance: &mut TestRepository,
    removed: Removed,
    reached: Reached,
) -> Arc<State> {
    let (view, files, left_out, directory) = removed.case();
    let first = committed(instance, &files).await;
    let replaced = matches!(removed, Removed::ReplacedByFile);
    left_out_and_removed(instance, &view, left_out, directory, replaced, reached).await;
    first
}

/// An instance under the view of `linked`, with the files of its case and its link committed, the
/// working tree holding what cloning under the view creates, less the removed directory, and that
/// removal reported as `reached` says. Answers the committed revision.
async fn removed_beside_a_link(
    instance: &mut TestRepository,
    linked: Linked,
    reached: Reached,
) -> Arc<State> {
    let (view, files, left_out, directory, mount, _) = linked.case();
    committed(instance, &files).await;
    let linked_id = lore_revision::lore::RepositoryId::from(uuid::Uuid::now_v7());
    mount_intermediate_at(&instance.repository, mount, linked_id, true).await;
    written(instance, &[&format!("{mount}/src/Intermediate")]);
    let first = test_commit(instance, "Linked").await;
    assert!(
        first.link_list_hash() != Default::default(),
        "{linked:?}: a link registered"
    );
    left_out_and_removed(instance, &view, left_out, directory, false, reached).await;
    first
}

/// Removes from the working tree what `view` leaves out, `left_out`, and the directory `directory`,
/// with a file in its place where `replaced`, applies the view, and reports the removal as
/// `reached` says.
async fn left_out_and_removed(
    instance: &mut TestRepository,
    view: &[&str],
    left_out: &[&str],
    directory: &str,
    replaced: bool,
    reached: Reached,
) {
    for path in left_out {
        let path = instance.path.join(path);
        if path.is_dir() {
            std::fs::remove_dir_all(path).expect("the directory left out");
        } else {
            std::fs::remove_file(path).expect("the file left out");
        }
    }
    removed_directory(instance, directory);
    if replaced {
        std::fs::write(instance.path.join(directory), b"file").expect("the file in its place");
    }
    instance.repository = Arc::new(
        instance
            .repository
            .to_filter_context(Arc::new(test_filter(&[], view))),
    );
    match reached {
        Reached::Marked => {
            lore_revision::file::dirty::dirty(
                instance.repository.clone(),
                LoreArray::from_vec(vec![LoreString::from(&instance.path)]),
            )
            .await
            .expect("the walk marking the tree");
        }
        Reached::Scanned => scanned(instance).await,
    }
}

/// What staging names: the whole working tree, or the removed directory itself.
#[derive(Clone, Copy, Debug)]
enum Target {
    Root,
    Directory,
}

/// Stages the whole working tree as `reached` says.
async fn staged_whole_tree(instance: &TestRepository, reached: Reached) {
    staged_at(instance, reached, "").await;
}

/// Stages `path` of the working tree as `reached` says.
async fn staged_at(instance: &TestRepository, reached: Reached, path: &str) {
    lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(path))]),
        StageOptions {
            scan: matches!(reached, Reached::Scanned),
            ..Default::default()
        },
    )
    .await
    .expect("staging");
}

/// The instance's staged tree.
async fn staged_state(instance: &TestRepository) -> Arc<State> {
    let (_, staged, _) = State::deserialize_current_and_staged(instance.repository.clone())
        .await
        .expect("the states");
    staged.expect("a staged state")
}

/// Whether `path` is `directory` or a path below it.
fn under(path: &str, directory: &str) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Asserts that a status of the instance lists `directory` as deleted, and nothing at or below it
/// but deletions.
async fn assert_listed_deleted(
    instance: &TestRepository,
    first: &Arc<State>,
    directory: &str,
    what: &str,
) {
    let listed: Vec<(u16, String)> = status_changes(instance, first, &staged_state(instance).await)
        .await
        .into_iter()
        .filter(|(_, path, _)| under(path, directory))
        .map(|(action, path, _)| (action, path))
        .collect();
    assert!(
        listed.contains(&(FileAction::Delete as u16, directory.to_string()))
            && listed
                .iter()
                .all(|(action, _)| *action == FileAction::Delete as u16),
        "{what}: listed {listed:?}"
    );
}

/// Unstages `directory`.
async fn unstaged(instance: &TestRepository, directory: &str, what: &str) {
    lore_revision::file::unstage::unstage(
        instance.repository.clone(),
        &instance.write_token,
        LoreArray::from_vec(vec![LoreString::from(&instance.path.join(directory))]),
        UnstageOptions::default(),
    )
    .await
    .unwrap_or_else(|error| panic!("{what}: the unstage: {error}"));
}

/// Asserts that the staged tree stages nothing at or below `directory`.
async fn assert_nothing_staged_at(instance: &TestRepository, directory: &str, what: &str) {
    let staged = staged_state(instance).await;
    for path in tree_paths(instance, &staged).await.into_keys() {
        if under(&path, directory) {
            let node = node_at(instance, &staged, &path).await;
            assert!(!node.is_staged(), "{what}: {path} flags {:#x}", node.flags);
        }
    }
}

/// The paths at or below `directory` whose deletion `staged` stages.
async fn deletions_staged_at_or_below(
    instance: &TestRepository,
    staged: &State,
    directory: &str,
) -> Vec<String> {
    let mut paths = Vec::new();
    for path in tree_paths(instance, staged).await.into_keys() {
        if under(&path, directory) && node_at(instance, staged, &path).await.is_staged_delete() {
            paths.push(path);
        }
    }
    paths
}

/// Once the working tree lacks a directory the instance holds under its view, or holds a file the
/// view leaves out in its place, status lists the directory as deleted, staging the whole tree or
/// the directory's own path stages its deletion, and committing records it: the revision holds
/// nothing at or below the directory, the files the view leaves out included, and every other path
/// as it was, `h/z.txt`, in a directory the view does not create, among them. Status is clean
/// afterwards.
#[tokio::test]
async fn staging_a_removed_directory_deletes_it_with_everything_below_it() {
    let removals = Removed::EACH.into_iter().chain([Removed::ReplacedByFile]);
    for (removed, reached, target) in removals.flat_map(|removed| {
        Reached::EACH.into_iter().flat_map(move |reached| {
            [Target::Root, Target::Directory].map(|target| (removed, reached, target))
        })
    }) {
        with_instance(|mut instance| async move {
            let what = format!("{removed:?}, {reached:?}, {target:?}");
            let first = removed_under_view(&mut instance, removed, reached).await;
            let directory = removed.case().3;
            assert_listed_deleted(&instance, &first, directory, &format!("{what}: reported")).await;

            match target {
                Target::Root => staged_whole_tree(&instance, reached).await,
                Target::Directory => staged_at(&instance, reached, directory).await,
            }
            assert_listed_deleted(&instance, &first, directory, &format!("{what}: staged")).await;
            let staged = staged_state(&instance).await;
            let node = node_at(&instance, &staged, directory).await;
            assert!(node.is_staged_delete(), "{what}: flags {:#x}", node.flags);

            commit_boxed(
                instance.repository.clone(),
                &instance.write_token,
                CommitOptions::new("Removed".to_string()),
            )
            .await
            .unwrap_or_else(|error| panic!("{what}: committing: {error}"));
            let (revision, _) =
                lore_revision::instance::load_current_anchor_boxed(&instance.repository)
                    .await
                    .expect("the current anchor");
            let revision = State::deserialize(instance.repository.clone(), revision)
                .await
                .expect("the revision");
            let mut kept = tree_paths(&instance, &first).await;
            kept.retain(|path, _| !under(path, directory));
            assert_eq!(tree_paths(&instance, &revision).await, kept, "{what}");
            scanned(&instance).await;
            assert_no_change_marked(&instance).await;
        })
        .await;
    }
}

/// `lore unstage` of a removed directory whose deletion is staged takes the whole deletion back:
/// nothing at or below the directory stays staged, and status lists the directory as deleted.
#[tokio::test]
async fn unstaging_a_removed_directory_takes_back_its_whole_deletion() {
    for removed in Removed::EACH {
        for reached in Reached::EACH {
            with_instance(|mut instance| async move {
                let what = format!("{removed:?}, {reached:?}");
                let first = removed_under_view(&mut instance, removed, reached).await;
                let directory = removed.case().3;
                staged_whole_tree(&instance, reached).await;
                unstaged(&instance, directory, &what).await;

                assert_nothing_staged_at(&instance, directory, &what).await;
                assert_listed_deleted(&instance, &first, directory, &what).await;
            })
            .await;
        }
    }
}

/// `lore reset` of a removed directory, its staged deletion taken back, creates the directory
/// again, and status is clean afterwards.
#[tokio::test]
async fn resetting_a_removed_directory_creates_it_again() {
    for removed in Removed::EACH {
        for reached in Reached::EACH {
            with_instance(|mut instance| async move {
                let what = format!("{removed:?}, {reached:?}");
                removed_under_view(&mut instance, removed, reached).await;
                let directory = removed.case().3;
                staged_whole_tree(&instance, reached).await;
                unstaged(&instance, directory, &what).await;

                lore_revision::file::reset::reset(
                    instance.repository.clone(),
                    &instance.write_token,
                    LoreArray::from_vec(vec![LoreString::from(&instance.path.join(directory))]),
                    LoreString::default(),
                    ResetOptions::default(),
                )
                .await
                .unwrap_or_else(|error| panic!("{what}: the reset: {error}"));

                assert!(instance.path.join(directory).is_dir(), "{what}");
                scanned(&instance).await;
                assert_no_change_marked(&instance).await;
            })
            .await;
        }
    }
}

/// Where the tree registers a link, staging a removed directory stages the deletion of what the
/// working tree held below it alone, and keeps the directory wherever the view leaves out a node
/// below it or a link lies below it: beside a link the view leaves out at `z/deep/lib`, `a` and
/// `src` keep what the view hides, and `a` keeps the link the view keeps at `a/lib`.
#[tokio::test]
async fn with_a_link_registered_staging_a_removed_directory_keeps_what_the_view_hides() {
    for linked in Linked::EACH {
        for reached in Reached::EACH {
            with_instance(|mut instance| async move {
                let what = format!("{linked:?}, {reached:?}");
                removed_beside_a_link(&mut instance, linked, reached).await;
                let (_, _, _, directory, _, expected) = linked.case();

                staged_whole_tree(&instance, reached).await;
                let staged = staged_state(&instance).await;
                assert_eq!(
                    deletions_staged_at_or_below(&instance, &staged, directory).await,
                    expected,
                    "{what}"
                );
            })
            .await;
        }
    }
}

/// The delete events and counts staging reports: each path a `FileStageFile` event names deleted,
/// in order, and the directories and files the closing count gives deleted.
#[derive(Debug, PartialEq)]
struct StageReport {
    deleted: Vec<String>,
    directories_deleted: u64,
    files_deleted: u64,
}

/// Stages the whole working tree as `reached` says, in an execution context of its own, and
/// answers its delete events and counts.
async fn stage_report(instance: &TestRepository, reached: Reached) -> StageReport {
    let report = Arc::new(Mutex::new(StageReport {
        deleted: Vec::new(),
        directories_deleted: 0,
        files_deleted: 0,
    }));
    let sent = report.clone();
    let callback: lore_revision::interface::LoreEventCallback = Some(Box::new(move |event| {
        let mut report = sent.lock();
        match event {
            LoreEvent::FileStageFile(data)
                if data.action == lore_revision::interface::LoreFileAction::Delete =>
            {
                report.deleted.push(data.path.to_string());
            }
            LoreEvent::FileStageEnd(data) => {
                report.directories_deleted = data.count.directory_delete_count;
                report.files_deleted = data.count.file_delete_count;
            }
            _ => {}
        }
    }));
    let execution = Arc::new(ExecutionContext::new_client_with_user_id(
        LoreGlobalArgs::default(),
        EventDispatcher::new(callback),
        "test-user".to_string(),
    ));
    LORE_CONTEXT
        .scope(execution.clone(), staged_whole_tree(instance, reached))
        .await;
    execution.dispatcher.drain().await;
    let mut report = std::mem::replace(
        &mut *report.lock(),
        StageReport {
            deleted: Vec::new(),
            directories_deleted: 0,
            files_deleted: 0,
        },
    );
    report.deleted.sort();
    report
}

/// Staging a removed directory the view keeps, `a` under `**`, `!/a/`, `!/k.txt`, reports each
/// file the view shows below it deleted, `a/v.txt`, `a/w.txt` and `a/s/z.txt`, by a delete event
/// and in the count, with `a` and `a/s` counted as deleted directories.
#[tokio::test]
async fn staging_a_removed_kept_directory_reports_each_file_the_view_shows() {
    for reached in Reached::EACH {
        with_instance(|mut instance| async move {
            committed(&instance, &["a/v.txt", "a/w.txt", "a/s/z.txt", "k.txt"]).await;
            left_out_and_removed(
                &mut instance,
                &["**", "!/a/", "!/k.txt"],
                &[],
                "a",
                false,
                reached,
            )
            .await;

            assert_eq!(
                stage_report(&instance, reached).await,
                StageReport {
                    deleted: vec![
                        "a/s/z.txt".to_string(),
                        "a/v.txt".to_string(),
                        "a/w.txt".to_string(),
                    ],
                    directories_deleted: 2,
                    files_deleted: 3,
                },
                "{reached:?}"
            );
        })
        .await;
    }
}

/// Staging a removed directory the view keeps, `a` under `**`, `!/a/`, `/a/s/h.txt`, `!/k.txt`,
/// counts `a/s` as a deleted directory though it holds `a/s/h.txt`, which the view leaves out, as
/// committing removes it with `a`; `a/s/h.txt` is deleted without an event.
#[tokio::test]
async fn staging_a_removed_kept_directory_counts_a_shown_directory_holding_what_the_view_hides() {
    for reached in Reached::EACH {
        with_instance(|mut instance| async move {
            committed(&instance, &["a/v.txt", "a/s/z.txt", "a/s/h.txt", "k.txt"]).await;
            left_out_and_removed(
                &mut instance,
                &["**", "!/a/", "/a/s/h.txt", "!/k.txt"],
                &["a/s/h.txt"],
                "a",
                false,
                reached,
            )
            .await;

            assert_eq!(
                stage_report(&instance, reached).await,
                StageReport {
                    deleted: vec!["a/s/z.txt".to_string(), "a/v.txt".to_string()],
                    directories_deleted: 2,
                    files_deleted: 2,
                },
                "{reached:?}"
            );
        })
        .await;
    }
}

/// Staging a removed directory the view keeps by a scan reports no deletion below it that is
/// staged already: with `a/s` removed and staged first, removing `a` under `**`, `!/a/`, `!/k.txt`
/// reports `a/v.txt` alone, and `a` as the one deleted directory.
#[tokio::test]
async fn staging_a_removed_kept_directory_reports_no_deletion_staged_already() {
    let reached = Reached::Scanned;
    with_instance(|mut instance| async move {
        committed(&instance, &["a/v.txt", "a/s/z.txt", "a/s/y.txt", "k.txt"]).await;
        let view = ["**", "!/a/", "!/k.txt"];
        left_out_and_removed(&mut instance, &view, &[], "a/s", false, reached).await;
        staged_at(&instance, reached, "a/s").await;
        left_out_and_removed(&mut instance, &view, &[], "a", false, reached).await;

        assert_eq!(
            stage_report(&instance, reached).await,
            StageReport {
                deleted: vec!["a/v.txt".to_string()],
                directories_deleted: 1,
                files_deleted: 1,
            },
        );
    })
    .await;
}

/// The scan walk that stages, which `lore stage --scan` does not take, settles a removed directory
/// as staging does: in a tree that registers no link it stages the deletion of each directory of
/// [`Removed`], and beside a registered link the deletions of [`Linked`] alone.
#[tokio::test]
async fn a_staging_scan_walk_settles_a_removed_directory_as_staging_does() {
    for removed in Removed::EACH {
        with_instance(|mut instance| async move {
            removed_under_view(&mut instance, removed, Reached::Marked).await;
            let directory = removed.case().3;
            let staged = test_staged_by_intent(&instance.repository).await;
            let node = node_at(&instance, &staged, directory).await;
            assert!(
                node.is_staged_delete(),
                "{removed:?}: flags {:#x}",
                node.flags
            );
        })
        .await;
    }
    for linked in Linked::EACH {
        with_instance(|mut instance| async move {
            removed_beside_a_link(&mut instance, linked, Reached::Marked).await;
            let (_, _, _, directory, _, expected) = linked.case();
            let staged = test_staged_by_intent(&instance.repository).await;
            assert_eq!(
                deletions_staged_at_or_below(&instance, &staged, directory).await,
                expected,
                "{linked:?}"
            );
        })
        .await;
    }
}

/// Staging the deletion of a directory the view keeps and hides everything below, `a/b` under
/// [`EMPTIED_VIEW`], stages it at once and reads nothing of what it holds: no block holding a node
/// below `a/b` has its names read, and `a/b` alone is counted.
#[tokio::test]
async fn staging_a_removed_directory_the_view_hides_everything_below_reads_nothing_below_it() {
    with_instance(|mut instance| async move {
        let first = committed(&instance, EMPTIED_FILES).await;
        instance.repository = Arc::new(
            instance
                .repository
                .to_filter_context(Arc::new(test_filter(&[], EMPTIED_VIEW))),
        );
        let paths = tree_paths(&instance, &first).await;
        let below: Vec<NodeID> = paths
            .iter()
            .filter(|(path, _)| under(path, "a/b") && path.as_str() != "a/b")
            .map(|(_, node)| *node)
            .collect();
        assert!(!below.is_empty(), "the tree holds nodes below a/b");
        let directory = RelativePath::new_from_initial_path("a/b").expect("a path");
        let state = State::deserialize(instance.repository.clone(), first.revision())
            .await
            .expect("the revision");
        let stats = Arc::new(StageStats::default());
        let held = state
            .node(instance.repository.clone(), paths["a/b"])
            .await
            .expect("the node")
            .flags;

        let absence = stage_absent_delete(
            instance.repository.clone(),
            state.clone(),
            directory.clone(),
            paths["a/b"],
            NodeFlags::from_bits_retain(held),
            NodeFlags::NoFlags,
            stats.clone(),
            None,
            instance.repository.filter.exclusion_states(&directory),
            false,
            false,
        )
        .await
        .expect("the stage");

        assert_eq!(absence, Absence::Deleted);

        for node in below {
            assert!(
                state
                    .resident_block(NodeBlock::index(node))
                    .is_none_or(|block| !block.is_nametable_deserialized()),
                "the names of node {node}'s block were read"
            );
        }
        assert_eq!(stats.file_delete_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.directory_delete_count.load(Ordering::Relaxed), 1);
    })
    .await;
}
