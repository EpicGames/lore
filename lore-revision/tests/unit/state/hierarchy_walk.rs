// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The walk below a directory deleted as a whole, beside changes to the directory.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lore_revision::change::FileAction;
use lore_revision::filter::FilterMode;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::AttachOutcome;
use lore_revision::state::ChangeStream;
use lore_revision::state::Detach;
use lore_revision::state::DiffWalkStats;
use lore_revision::state::NameChange;
use lore_revision::state::ReattachCheck;
use lore_revision::state::State;

use super::add;
use super::directory;
use super::file;
use super::null_repository;
use super::record;

/// A State holding `d` with the empty directory `d/e` and the files `d/a0` to `d/a6`, served
/// where `served` says, and the walk of `d`'s delete, taken up to its first child.
///
/// The walk emits through a channel of one change, so it reads a child's record at most two
/// children ahead of the last change taken from it: what changes after the directory's first
/// child is taken reaches children the walk has yet to read. A served level collects its children
/// when it opens, before its first child is emitted, and reads each again as it emits it, within
/// the same window. `d/e`, added first, is the last child walked, so its own level opens after
/// the changes.
struct Walk {
    repository: Arc<RepositoryContext>,
    from: Arc<State>,
    directory: NodeID,
    subdirectory: NodeID,
    children: Vec<NodeID>,
    stream: ChangeStream<DiffWalkStats>,
    emitted: Vec<String>,
}

impl Walk {
    async fn started(served: bool) -> Self {
        let repository = null_repository().await;
        let from = State::new();
        let d = add(&from, &repository, ROOT_NODE, directory("d"), "d").await;
        let e = add(&from, &repository, d, directory("e"), "e").await;
        let mut children = Vec::new();
        for index in 0..7 {
            let name = format!("a{index}");
            children.push(add(&from, &repository, d, file(&name), &name).await);
        }
        if served {
            from.mark_served();
        }

        let (from_repository, to_repository) = (repository.clone(), repository.clone());
        let walked = from.clone();
        let mut stream =
            ChangeStream::spawn_with_lookahead(NonZeroUsize::MIN, async move |changes| {
                lore_revision::state::diff(
                    from_repository,
                    walked,
                    to_repository,
                    State::new(),
                    None,
                    None,
                    &changes,
                    FilterMode::Full,
                )
                .await
            });
        let mut emitted = Vec::new();
        for _ in 0..2 {
            let change = stream
                .next()
                .await
                .expect("the directory and its first child");
            emitted.push(change.path().as_str().to_string());
        }
        assert_eq!(
            emitted,
            ["d", "d/a6"],
            "the directory's delete precedes its children, the newest first"
        );
        Self {
            repository,
            from,
            directory: d,
            subdirectory: e,
            children,
            stream,
            emitted,
        }
    }

    /// Every path the walk emitted, sorted.
    async fn finished(mut self) -> Vec<String> {
        while let Some(change) = self.stream.next().await {
            assert_eq!(change.action, FileAction::Delete, "{}", change.path());
            self.emitted.push(change.path().as_str().to_string());
        }
        self.stream.finish().await.expect("the walk");
        self.emitted.sort();
        self.emitted
    }
}

/// On a served State, a directory deleted as a whole is enumerated from the children its level
/// collected, each read again as it is emitted: a child reattached to the head of the directory
/// since is emitted once, one renamed or detached since is left out, and one moved to a directory
/// the walk has yet to open is emitted there, so no path is emitted twice and none at a name its
/// node has left.
#[tokio::test]
async fn a_served_deleted_directory_emits_no_child_twice_or_at_a_name_it_left() {
    let execution = crate::fs::filesystem_provider::setup_test_execution();
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async {
            let walk = Walk::started(true).await;
            let (repository, from, d) = (&walk.repository, &walk.from, walk.directory);
            let children = &walk.children;
            let state = from.clone();
            let moves = state.walk_moves();

            let reattached = from
                .served()
                .node_reattach(
                    repository.clone(),
                    children[3],
                    d,
                    "a3",
                    ReattachCheck::Walk,
                )
                .await
                .expect("reattaching a3 to the head");
            assert_eq!(reattached, AttachOutcome::Attached(children[3]));
            assert_eq!(
                record(from, repository, d).await.child(),
                Some(children[3]),
                "the reattached child heads the directory"
            );
            let renamed = from
                .served()
                .node_reattach(
                    repository.clone(),
                    children[2],
                    d,
                    "b2",
                    ReattachCheck::Walk,
                )
                .await
                .expect("renaming a2");
            assert_eq!(renamed, AttachOutcome::Attached(children[2]));
            assert!(
                from.served()
                    .node_detach(repository.clone(), children[1], Detach::Always)
                    .await
                    .expect("detaching a1"),
                "the child detaches"
            );
            let moved = from
                .served()
                .node_reattach(
                    repository.clone(),
                    children[0],
                    walk.subdirectory,
                    "a0",
                    ReattachCheck::Walk,
                )
                .await
                .expect("moving a0 into e");
            assert_eq!(moved, AttachOutcome::Attached(children[0]));

            assert_eq!(
                walk.finished().await,
                ["d", "d/a3", "d/a4", "d/a5", "d/a6", "d/e", "d/e/a0"],
                "each child once at its collected name, none renamed, detached or moved since"
            );
            assert_eq!(
                state.walk_moves(),
                moves + 3,
                "the renamed, the detached and the moved child are counted as left out"
            );
        })
        .await;
}

/// On a State no VFS provider serves, a directory deleted as a whole is enumerated by walking its
/// chain as the walk goes: a child renamed in place before the walk reaches it is emitted at its
/// new name, and one detached before is left out.
#[tokio::test]
async fn an_unserved_deleted_directory_emits_each_child_as_the_walk_reaches_it() {
    let execution = crate::fs::filesystem_provider::setup_test_execution();
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async {
            let walk = Walk::started(false).await;
            let (repository, from) = (&walk.repository, &walk.from);
            let children = &walk.children;

            from.node_rename(repository.clone(), children[2], NameChange::Rename("b2"))
                .await
                .expect("renaming a2");
            assert!(
                from.node_detach(repository.clone(), children[1], Detach::Always)
                    .await
                    .expect("detaching a1"),
                "the child detaches"
            );

            assert_eq!(
                walk.finished().await,
                ["d", "d/a0", "d/a3", "d/a4", "d/a5", "d/a6", "d/b2", "d/e"],
                "each child the walk reached, at the name it held then"
            );
        })
        .await;
}
