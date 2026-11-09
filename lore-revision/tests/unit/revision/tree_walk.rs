// SPDX-FileCopyrightText: 2026 LoreLab.io
// SPDX-License-Identifier: MIT

//! The entries of a tree walk: the repository each is in, and the revision that last changed it.

use std::collections::HashMap;
use std::sync::Arc;

use lore_base::error::NotFound;
use lore_base::error::SlowDown;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::MetadataInherit;
use lore_revision::node::*;
use lore_revision::repository::RepositoryContext;
use lore_revision::revision::cherry_pick::CherryPickOptions;
use lore_revision::revision::cherry_pick::cherry_pick_boxed;
use lore_revision::revision::revert::RevertOptions;
use lore_revision::revision::revert::revert_boxed;
use lore_revision::revision::tree;
use lore_revision::state::AttributionFailures;
use lore_revision::state::State;
use lore_revision::state::StateError;
use lore_revision::state::TreeWalk;
use lore_revision::state::allow_all_repositories;
use lore_revision::state::skip_linked_subtree;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

use super::instance::Edit;
use super::instance::Instance;
use super::instance::commit_edits;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::run;
use super::instance::stage_and_commit;
use super::instance::switch_to;
use super::instance::working_tree;
use super::instance::write;
use crate::helper::in_memory;
use crate::helper::in_memory::add;
use crate::helper::in_memory::branch_id;
use crate::helper::in_memory::commit;
use crate::helper::in_memory::directory;
use crate::helper::in_memory::file;
use crate::helper::in_memory::modify;
use crate::helper::in_memory::revision_state;
use crate::helper::in_memory::token;
use crate::helper::in_memory::weave_as_divergent_merge;
use crate::helper::test_store_create;
use crate::state::HoldingStore;

/// The walk of the tree of `revision` below `path`, `max_depth` levels deep or all of them for 0,
/// attributed when `attribute` is set, after checking that every attribution read succeeded.
async fn walk_below(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    path: &str,
    max_depth: usize,
    attribute: bool,
) -> TreeWalk {
    let walk = tree(
        repository.clone(),
        revision,
        RelativePath::new_from_initial_path(path).expect("a relative path"),
        max_depth,
        allow_all_repositories(),
        attribute,
    )
    .await
    .expect("walking the tree");
    assert_eq!(
        walk.attribution_failures.count, 0,
        "{:?}",
        walk.attribution_failures.error
    );
    walk
}

/// [`walk_below`] the root.
async fn walk(
    repository: &Arc<RepositoryContext>,
    revision: Hash,
    max_depth: usize,
    attribute: bool,
) -> TreeWalk {
    walk_below(repository, revision, "", max_depth, attribute).await
}

/// The repository and revision each entry of `walk` is attributed to, by path, or zeros for an
/// entry it does not attribute.
fn attributed(walk: &TreeWalk) -> HashMap<String, (RepositoryId, Hash)> {
    walk.paths
        .iter()
        .map(|entry| {
            let named = match (entry.last_revision as usize).checked_sub(1) {
                Some(slot) => {
                    let revision = &walk.revisions[slot];
                    (revision.repository.id, revision.revision)
                }
                None => (RepositoryId::default(), Hash::default()),
            };
            (entry.path.as_str().to_string(), named)
        })
        .collect()
}

/// Commits directory `a` holding `touched.bin` and `untouched.bin`, then a change to `touched.bin`,
/// answering both revisions.
async fn commit_a_change(repository: &Arc<RepositoryContext>) -> (Hash, Hash) {
    let main = branch_id();
    let state = State::new();
    let dir = add(&state, repository.clone(), ROOT_NODE, directory("a"), "a").await;
    let touched = add(
        &state,
        repository.clone(),
        dir,
        file("touched.bin"),
        "touched.bin",
    )
    .await;
    add(
        &state,
        repository.clone(),
        dir,
        file("untouched.bin"),
        "untouched.bin",
    )
    .await;
    let first = commit(repository, &state, Hash::default(), main).await;

    let state = revision_state(repository, first).await;
    modify(
        &state,
        repository,
        touched,
        changed(0),
        NodeFlags::StagedModify,
    )
    .await;
    let second = commit(repository, &state, first, main).await;
    (first, second)
}

/// A link named `name` mounting revision `revision` of repository `target` at its root.
fn link(name: &str, target: RepositoryId, revision: Hash) -> Node {
    Node {
        flags: NodeFlags::Link.bits(),
        mode: 0o755,
        name_hash: hash_string(name),
        child: ROOT_NODE,
        address: Address {
            hash: revision,
            context: target.into(),
        },
        ..Default::default()
    }
}

/// The content of the `n`th change a test makes to a file.
fn changed(n: u64) -> Address {
    Address {
        hash: Hash::from_u64(0x100 + n),
        context: Context::default(),
    }
}

/// The walk of the whole tree of `revision`, attributed, whatever the attribution reads answer.
async fn walk_through_failures(repository: &Arc<RepositoryContext>, revision: Hash) -> TreeWalk {
    tree(
        repository.clone(),
        revision,
        RelativePath::default(),
        0,
        allow_all_repositories(),
        true,
    )
    .await
    .expect("walking the tree")
}

/// An entry the walked revision changed is attributed to it, another to its own last change, and a
/// directory to the last change below it. Without attribution, every entry carries zero.
#[tokio::test]
async fn a_walk_attributes_each_entry_to_its_last_change() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let (first, second) = commit_a_change(&repository).await;

        let unattributed = walk(&repository, second, 0, false).await;
        assert_eq!(unattributed.paths.len(), 3);
        assert!(unattributed.revisions.is_empty());
        for entry in &unattributed.paths {
            assert_eq!(entry.last_revision, 0, "{}", entry.path);
        }

        let attributed_walk = walk(&repository, second, 0, true).await;
        assert_eq!(
            attributed_walk.revisions.len(),
            2,
            "each revision is named once"
        );
        let entries = attributed(&attributed_walk);
        assert_eq!(entries["a/touched.bin"], (repository.id, second));
        assert_eq!(entries["a/untouched.bin"], (repository.id, first));
        assert_eq!(entries["a"], (repository.id, second));

        let shallow = attributed(&walk(&repository, second, 1, true).await);
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow["a"], (repository.id, second));
    }))
    .await
    .expect("Task failed");
}

/// An entry under a link is attributed in the linked repository, to the linked revision. The link
/// and the entries beside it are attributed in the walked one.
#[tokio::test]
async fn a_walk_attributes_an_entry_under_a_link_in_the_linked_repository() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);

        let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
        let target = repository.to_link_context(target_id).await;
        let state = State::new();
        add(
            &state,
            target.clone(),
            ROOT_NODE,
            file("inner.bin"),
            "inner.bin",
        )
        .await;
        let target_revision = commit(&target, &state, Hash::default(), branch_id()).await;

        let state = State::new();
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file("top.bin"),
            "top.bin",
        )
        .await;
        let vendor = link("vendor", target_id, target_revision);
        add(&state, repository.clone(), ROOT_NODE, vendor, "vendor").await;
        let revision = commit(&repository, &state, Hash::default(), branch_id()).await;

        let entries = attributed(&walk(&repository, revision, 0, true).await);
        assert_eq!(entries["top.bin"], (repository.id, revision));
        assert_eq!(entries["vendor"], (repository.id, revision));
        assert_eq!(entries["vendor/inner.bin"], (target_id, target_revision));
    }))
    .await
    .expect("Task failed");
}

/// The repository each entry of `walk` is in, by path, as its spans place it, the entries before
/// the first span in `requested`.
fn repositories(walk: &TreeWalk, requested: RepositoryId) -> HashMap<String, RepositoryId> {
    let mut spans = walk.repository_spans.iter().peekable();
    let mut repository = requested;
    walk.paths
        .iter()
        .enumerate()
        .map(|(entry, path)| {
            while let Some(span) = spans.next_if(|span| span.first <= entry) {
                repository = span.repository;
            }
            (path.path.as_str().to_string(), repository)
        })
        .collect()
}

/// A walk places an entry under a link in the linked repository, one under a nested link in its
/// own, and the entries after a linked subtree, one that lists nothing included, in the repository
/// around it. A walk below a path inside a link starts in the linked repository.
#[tokio::test]
async fn a_walk_places_each_entry_in_its_repository() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);

        let inner_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
        let inner = repository.to_link_context(inner_id).await;
        let state = State::new();
        add(
            &state,
            inner.clone(),
            ROOT_NODE,
            file("deep.bin"),
            "deep.bin",
        )
        .await;
        let inner_revision = commit(&inner, &state, Hash::default(), branch_id()).await;

        let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
        let target = repository.to_link_context(target_id).await;
        let state = State::new();
        add(&state, target.clone(), ROOT_NODE, file("t1.bin"), "t1.bin").await;
        let nested = link("nested", inner_id, inner_revision);
        add(&state, target.clone(), ROOT_NODE, nested, "nested").await;
        add(&state, target.clone(), ROOT_NODE, file("t2.bin"), "t2.bin").await;
        let sub = add(&state, target.clone(), ROOT_NODE, directory("sub"), "sub").await;
        add(&state, target.clone(), sub, file("s.bin"), "s.bin").await;
        let hollow = add(
            &state,
            target.clone(),
            ROOT_NODE,
            directory("hollow"),
            "hollow",
        )
        .await;
        let target_revision = commit(&target, &state, Hash::default(), branch_id()).await;

        let state = State::new();
        let after = file("after.bin");
        add(&state, repository.clone(), ROOT_NODE, after, "after.bin").await;
        let vendor = link("vendor", target_id, target_revision);
        add(&state, repository.clone(), ROOT_NODE, vendor, "vendor").await;
        let empty = Node {
            child: hollow,
            ..link("empty", target_id, target_revision)
        };
        add(&state, repository.clone(), ROOT_NODE, empty, "empty").await;
        let before = file("before.bin");
        add(&state, repository.clone(), ROOT_NODE, before, "before.bin").await;
        let revision = commit(&repository, &state, Hash::default(), branch_id()).await;

        let placed = repositories(&walk(&repository, revision, 0, false).await, repository.id);
        let mut expected: HashMap<String, RepositoryId> = HashMap::new();
        for path in ["after.bin", "vendor", "empty", "before.bin"] {
            expected.insert(path.to_string(), repository.id);
        }
        for path in ["t1.bin", "nested", "t2.bin", "sub", "sub/s.bin", "hollow"] {
            expected.insert(format!("vendor/{path}"), target_id);
        }
        expected.insert("vendor/nested/deep.bin".to_string(), inner_id);
        assert_eq!(placed, expected);

        let below = walk_below(&repository, revision, "vendor/sub", 0, false).await;
        assert_eq!(
            repositories(&below, repository.id),
            HashMap::from([("vendor/sub/s.bin".to_string(), target_id)])
        );
    }))
    .await
    .expect("Task failed");
}

/// The merge a sync stages for divergent history, with the remote latest as first parent and the
/// local latest as second, where each side adds a different file at the same node ID. Each entry is
/// attributed by its path, not its node ID.
#[tokio::test]
async fn a_walk_attributes_a_divergent_sync_merge_by_path() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let main = branch_id();
        let remote = branch_id();

        let state = State::new();
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file("base.txt"),
            "base.txt",
        )
        .await;
        let early = file("early.txt");
        let early_id = add(&state, repository.clone(), ROOT_NODE, early, "early.txt").await;
        let early_changed = Address {
            hash: Hash::from_u64(0x44),
            context: early.address.context,
        };
        let base = commit(&repository, &state, Hash::default(), main).await;

        let state = revision_state(&repository, base).await;
        let local_id = add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file("local.txt"),
            "local.txt",
        )
        .await;
        let local = commit(&repository, &state, base, main).await;

        let state = revision_state(&repository, base).await;
        modify(
            &state,
            &repository,
            early_id,
            early_changed,
            NodeFlags::StagedModify,
        )
        .await;
        let remote_early = commit(&repository, &state, base, remote).await;
        let remote_file = file("remote.txt");
        let state = revision_state(&repository, remote_early).await;
        let remote_id = add(
            &state,
            repository.clone(),
            ROOT_NODE,
            remote_file,
            "remote.txt",
        )
        .await;
        let remote_latest = commit(&repository, &state, remote_early, remote).await;
        assert_eq!(remote_id, local_id);

        let state = revision_state(&repository, local).await;
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            remote_file,
            "remote.txt",
        )
        .await;
        modify(
            &state,
            &repository,
            early_id,
            early_changed,
            NodeFlags::StagedModify | NodeFlags::StagedMerge,
        )
        .await;
        let merged = commit(&repository, &state, local, main).await;
        let sync = weave_as_divergent_merge(&repository, merged, remote_latest, local).await;
        let sync_merge = sync
            .serialize(repository.clone(), &token())
            .await
            .expect("serializing the merge");

        let entries = attributed(&walk(&repository, sync_merge, 0, true).await);
        assert_eq!(entries["base.txt"], (repository.id, base));
        assert_eq!(entries["local.txt"], (repository.id, local));
        assert_eq!(entries["early.txt"], (repository.id, sync_merge));
        assert_eq!(entries["remote.txt"], (repository.id, sync_merge));
    }))
    .await
    .expect("Task failed");
}

/// The revisions of a merge of branch `side` into branch `ahead`, on an instance holding
/// `main-advance.txt` on the main branch: `ahead`, branched from it, adds `ahead-feature.txt`,
/// `side`, branched from `ahead`, adds `side-work.txt`, and `ahead` then adds `ahead-second.txt`.
struct AheadSide {
    main: Hash,
    ahead_first: Hash,
    ahead_second: Hash,
    merge: Hash,
}

async fn merge_side_into_ahead(instance: &Instance) -> AheadSide {
    let main = current(instance).await;
    create_branch(instance, "ahead").await;
    write(instance, "ahead-feature.txt", "one");
    let ahead_first = stage_and_commit(instance, &instance.path, "Add ahead-feature.txt").await;
    create_branch(instance, "side").await;
    write(instance, "side-work.txt", "two");
    stage_and_commit(instance, &instance.path, "Add side-work.txt").await;
    switch_to(instance, "ahead").await;
    write(instance, "ahead-second.txt", "three");
    let ahead_second = stage_and_commit(instance, &instance.path, "Add ahead-second.txt").await;
    assert!(merge_branch(instance, "side").await, "the merge committed");
    AheadSide {
        main,
        ahead_first,
        ahead_second,
        merge: current(instance).await,
    }
}

/// An entry the merge brought in from the merged branch is attributed to the merge, as `lore file
/// history` reports it.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_attributes_a_merged_in_entry_to_the_merge() {
    run(
        "a_walk_attributes_a_merged_in_entry_to_the_merge",
        &["main-advance.txt"],
        |instance| async move {
            let revisions = merge_side_into_ahead(&instance).await;
            let entries = attributed(&walk(&instance.repository, revisions.merge, 0, true).await);
            assert_eq!(
                entries["side-work.txt"],
                (instance.repository.id, revisions.merge)
            );
        },
    )
    .await;
}

/// An entry a merge did not change keeps the revision that last changed it on the first parent's
/// line.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_attributes_first_parent_entries_across_a_merge() {
    run(
        "a_walk_attributes_first_parent_entries_across_a_merge",
        &["main-advance.txt"],
        |instance| async move {
            let revisions = merge_side_into_ahead(&instance).await;
            let entries = attributed(&walk(&instance.repository, revisions.merge, 0, true).await);
            let id = instance.repository.id;
            assert_eq!(entries["main-advance.txt"], (id, revisions.main));
            assert_eq!(entries["ahead-feature.txt"], (id, revisions.ahead_first));
            assert_eq!(entries["ahead-second.txt"], (id, revisions.ahead_second));
        },
    )
    .await;
}

/// An entry both sides changed, which the merge reconciles, is attributed to the merge.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_attributes_a_reconciled_entry_to_the_merge() {
    run(
        "a_walk_attributes_a_reconciled_entry_to_the_merge",
        &["common.txt"],
        |instance| async move {
            create_branch(&instance, "ahead").await;
            write(&instance, "common.txt", "base line\nahead line\n");
            stage_and_commit(&instance, &instance.path, "Extend common.txt").await;
            create_branch(&instance, "side").await;
            write(
                &instance,
                "common.txt",
                "base line\nahead line\nside line\n",
            );
            stage_and_commit(&instance, &instance.path, "Append to common.txt").await;
            switch_to(&instance, "ahead").await;
            write(
                &instance,
                "common.txt",
                "ahead first line\nbase line\nahead line\n",
            );
            stage_and_commit(&instance, &instance.path, "Prefix common.txt").await;
            assert!(merge_branch(&instance, "side").await, "the merge committed");
            let merge = current(&instance).await;

            let files = working_tree(&instance.path);
            let merged = String::from_utf8_lossy(&files["common.txt"]);
            assert!(
                merged.contains("ahead first line") && merged.contains("side line"),
                "{merged:?}"
            );
            let entries = attributed(&walk(&instance.repository, merge, 0, true).await);
            assert_eq!(entries["common.txt"], (instance.repository.id, merge));
        },
    )
    .await;
}

/// An entry changed only on the merged branch is attributed to the merge, and so is its directory,
/// at the merge and at a later revision that leaves them unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_attributes_an_entry_changed_only_on_the_merged_branch_to_the_merge() {
    run(
        "a_walk_attributes_an_entry_changed_only_on_the_merged_branch_to_the_merge",
        &["src/shared.txt"],
        |instance| async move {
            create_branch(&instance, "target").await;
            create_branch(&instance, "side").await;
            write(&instance, "src/shared.txt", "changed on side");
            stage_and_commit(&instance, &instance.path, "Change src/shared.txt").await;
            switch_to(&instance, "target").await;
            write(&instance, "target.txt", "target");
            let target = stage_and_commit(&instance, &instance.path, "Add target.txt").await;
            assert!(merge_branch(&instance, "side").await, "the merge committed");
            let merge = current(&instance).await;
            write(&instance, "later.txt", "later");
            let later = stage_and_commit(&instance, &instance.path, "Add later.txt").await;

            let id = instance.repository.id;
            for walked in [merge, later] {
                let entries = attributed(&walk(&instance.repository, walked, 0, true).await);
                assert_eq!(entries["src/shared.txt"], (id, merge));
                assert_eq!(entries["src"], (id, merge));
                assert_eq!(entries["target.txt"], (id, target));
            }
        },
    )
    .await;
}

/// A walk matches the walked revision's delta against the smaller of its listing and the delta,
/// and attributes alike either way: a shallow walk of a revision that changes more entries than it
/// lists, and a full walk of a revision that changes fewer.
#[tokio::test]
async fn a_walk_attributes_alike_from_either_side_of_the_delta() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);
        let main = branch_id();
        let names = ["w0", "w1", "w2", "w3", "w4", "w5"];

        let state = State::new();
        let wide = add(
            &state,
            repository.clone(),
            ROOT_NODE,
            directory("wide"),
            "wide",
        )
        .await;
        let mut files = Vec::new();
        for name in names {
            files.push(add(&state, repository.clone(), wide, file(name), name).await);
        }
        let top = add(&state, repository.clone(), ROOT_NODE, file("top"), "top").await;
        let first = commit(&repository, &state, Hash::default(), main).await;

        let state = revision_state(&repository, first).await;
        for (n, &node) in (1..).zip(&files) {
            modify(
                &state,
                &repository,
                node,
                changed(n),
                NodeFlags::StagedModify,
            )
            .await;
        }
        let second = commit(&repository, &state, first, main).await;

        let state = revision_state(&repository, second).await;
        modify(
            &state,
            &repository,
            top,
            changed(0),
            NodeFlags::StagedModify,
        )
        .await;
        let third = commit(&repository, &state, second, main).await;

        let shallow = attributed(&walk(&repository, second, 1, true).await);
        assert_eq!(shallow.len(), 2);
        assert_eq!(shallow["wide"], (repository.id, second));
        assert_eq!(shallow["top"], (repository.id, first));

        let full = attributed(&walk(&repository, third, 0, true).await);
        assert_eq!(full.len(), names.len() + 2);
        assert_eq!(full["top"], (repository.id, third));
        assert_eq!(full["wide"], (repository.id, second));
        for name in names {
            assert_eq!(
                full[&format!("wide/{name}")],
                (repository.id, second),
                "{name}"
            );
        }
    }))
    .await
    .expect("Task failed");
}

/// A walk below a path, plain or below a link, attributes each entry as the walk of the whole tree
/// does.
#[tokio::test]
async fn a_walk_below_a_path_attributes_as_the_whole_walk() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let repository = in_memory::repository(immutable, mutable);

        let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
        let target = repository.to_link_context(target_id).await;
        let target_branch = branch_id();
        let state = State::new();
        let sub = add(&state, target.clone(), ROOT_NODE, directory("sub"), "sub").await;
        add(&state, target.clone(), sub, file("inner.bin"), "inner.bin").await;
        let outer = add(
            &state,
            target.clone(),
            ROOT_NODE,
            file("outer.bin"),
            "outer.bin",
        )
        .await;
        let target_first = commit(&target, &state, Hash::default(), target_branch).await;
        let state = revision_state(&target, target_first).await;
        modify(&state, &target, outer, changed(0), NodeFlags::StagedModify).await;
        let target_second = commit(&target, &state, target_first, target_branch).await;

        let state = State::new();
        let dir = add(
            &state,
            repository.clone(),
            ROOT_NODE,
            directory("dir"),
            "dir",
        )
        .await;
        add(&state, repository.clone(), dir, file("top.bin"), "top.bin").await;
        let vendor = link("vendor", target_id, target_second);
        add(&state, repository.clone(), ROOT_NODE, vendor, "vendor").await;
        let revision = commit(&repository, &state, Hash::default(), branch_id()).await;

        let whole = attributed(&walk(&repository, revision, 0, true).await);
        assert_eq!(whole["vendor/sub/inner.bin"], (target_id, target_first));
        assert_eq!(whole["vendor/outer.bin"], (target_id, target_second));
        for (path, listed) in [("dir", 1), ("vendor/sub", 1)] {
            let below = attributed(&walk_below(&repository, revision, path, 0, true).await);
            assert_eq!(below.len(), listed, "{path}");
            for (entry, named) in &below {
                assert_eq!(*named, whole[entry], "{entry} below {path}");
            }
        }
    }))
    .await
    .expect("Task failed");
}

/// A directory is attributed to a deletion below it, and the entries the deletion leaves keep
/// their last change.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_attributes_a_directory_to_a_deletion_below_it() {
    run(
        "a_walk_attributes_a_directory_to_a_deletion_below_it",
        &["src/kept.txt", "src/gone.txt", "other/x.txt"],
        |instance| async move {
            let first = current(&instance).await;
            let deletion = commit_edits(
                &instance,
                &[Edit::Delete("src/gone.txt")],
                "Delete gone.txt",
            )
            .await;

            let entries = attributed(&walk(&instance.repository, deletion, 0, true).await);
            let id = instance.repository.id;
            assert!(!entries.contains_key("src/gone.txt"));
            assert_eq!(entries["src"], (id, deletion));
            assert_eq!(entries["src/kept.txt"], (id, first));
            assert_eq!(entries["other"], (id, first));
            assert_eq!(entries["other/x.txt"], (id, first));
        },
    )
    .await;
}

/// A move is attributed to the moved entry and to the directories it leaves and enters, and the
/// entries beside it keep their last change.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_attributes_a_move_to_the_entry_and_both_directories() {
    run(
        "a_walk_attributes_a_move_to_the_entry_and_both_directories",
        &["from/moved.txt", "from/stays.txt", "to/x.txt"],
        |instance| async move {
            let first = current(&instance).await;
            let moved = commit_edits(
                &instance,
                &[Edit::Move("from/moved.txt", "to/moved.txt")],
                "Move moved.txt",
            )
            .await;

            let entries = attributed(&walk(&instance.repository, moved, 0, true).await);
            let id = instance.repository.id;
            assert!(!entries.contains_key("from/moved.txt"));
            assert_eq!(entries["to/moved.txt"], (id, moved));
            assert_eq!(entries["from"], (id, moved));
            assert_eq!(entries["to"], (id, moved));
            assert_eq!(entries["from/stays.txt"], (id, first));
            assert_eq!(entries["to/x.txt"], (id, first));
        },
    )
    .await;
}

/// A committed cherry-pick or revert has one parent, so it is attributed as any revision is: an
/// entry it changed to it, and every other entry to its own last change.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_attributes_a_cherry_pick_and_a_revert() {
    run(
        "a_walk_attributes_a_cherry_pick_and_a_revert",
        &["picked.txt", "kept.txt"],
        |instance| async move {
            let base = current(&instance).await;
            create_branch(&instance, "feature").await;
            let feature = commit_edits(
                &instance,
                &[Edit::Write("picked.txt", "changed on feature")],
                "Change picked.txt",
            )
            .await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            let main = commit_edits(
                &instance,
                &[Edit::Write("main.txt", "main")],
                "Add main.txt",
            )
            .await;

            cherry_pick_boxed(
                instance.repository.clone(),
                &instance.write_token,
                feature,
                CherryPickOptions {
                    message: "Cherry-pick picked.txt".to_string(),
                    no_commit: false,
                    inherit_metadata: MetadataInherit::default(),
                },
            )
            .await
            .expect("cherry-picking");
            let cherry_pick = current(&instance).await;
            revert_boxed(
                instance.repository.clone(),
                &instance.write_token,
                cherry_pick,
                RevertOptions {
                    message: "Revert the cherry-pick".to_string(),
                    no_commit: false,
                },
            )
            .await
            .expect("reverting");
            let revert = current(&instance).await;

            let id = instance.repository.id;
            for walked in [cherry_pick, revert] {
                let state = State::deserialize(instance.repository.clone(), walked)
                    .await
                    .expect("reading the revision");
                assert!(state.parent_other().is_zero(), "one parent");
                let entries = attributed(&walk(&instance.repository, walked, 0, true).await);
                assert_eq!(entries["picked.txt"], (id, walked));
                assert_eq!(entries["kept.txt"], (id, base));
                assert_eq!(entries["main.txt"], (id, main));
            }
        },
    )
    .await;
}

/// A repository reading through a [`HoldingStore`] over `immutable`, and the store.
fn refusing_repository(
    immutable: Arc<dyn lore_storage::ImmutableStore>,
    mutable: Arc<dyn lore_storage::MutableStore>,
) -> (Arc<HoldingStore>, Arc<RepositoryContext>) {
    let store = HoldingStore::over(immutable);
    (store.clone(), in_memory::repository(store, mutable))
}

/// A delta that fails to read leaves every entry of the walked revision unattributed, as which of
/// them it changed is unknown. The walk still lists every entry, and counts the failure.
#[tokio::test]
async fn a_delta_that_fails_to_read_leaves_the_entries_unattributed() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let (store, repository) = refusing_repository(immutable, mutable);
        let (_, second) = commit_a_change(&repository).await;
        let tree = revision_state(&repository, second)
            .await
            .tree(repository.clone())
            .await
            .expect("the tree of the revision");
        store.refuse(tree.hash_delta);

        let walk = walk_through_failures(&repository, second).await;
        assert_eq!(walk.paths.len(), 3);
        for entry in &walk.paths {
            assert_eq!(entry.last_revision, 0, "{}", entry.path);
        }
        assert_eq!(walk.attribution_failures.count, 1);
        assert!(walk.attribution_failures.error.is_some());
    }))
    .await
    .expect("Task failed");
}

/// File history that fails to read leaves the entries it would attribute unattributed, and is read
/// once per block, while the walked revision's delta still attributes the entries it changed.
#[tokio::test]
async fn file_history_that_fails_to_read_leaves_its_entries_unattributed() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let (store, repository) = refusing_repository(immutable, mutable);
        let (_, second) = commit_a_change(&repository).await;
        let tree = revision_state(&repository, second)
            .await
            .tree(repository.clone())
            .await
            .expect("the tree of the revision");
        store.refuse(tree.hash_file_metadata);

        let walk = walk_through_failures(&repository, second).await;
        assert_eq!(walk.paths.len(), 3);
        assert_eq!(
            walk.attribution_failures.count, 1,
            "one failed read per block"
        );
        let entries = attributed(&walk);
        assert_eq!(entries["a/touched.bin"], (repository.id, second));
        assert_eq!(
            entries["a/untouched.bin"],
            (RepositoryId::default(), Hash::default())
        );
    }))
    .await
    .expect("Task failed");
}

/// A linked subtree whose walk fails part-way leaves the entries it listed from the linked revision
/// unattributed, and counts the failure, while the entries outside it stay attributed.
#[tokio::test]
async fn a_linked_subtree_that_fails_part_way_leaves_its_entries_unattributed() {
    let (immutable, mutable, execution) = test_store_create().await.expect("creating the stores");
    lore_spawn!(LORE_CONTEXT.scope(execution, async move {
        let (store, repository) = refusing_repository(immutable, mutable);

        let target_id: RepositoryId = Context::from(uuid::Uuid::now_v7()).into();
        let target = repository.to_link_context(target_id).await;
        let state = State::new();
        let dir = add(&state, target.clone(), ROOT_NODE, directory("d"), "d").await;
        for n in 0..BLOCK_NODE_COUNT {
            let name = format!("f{n:04}");
            add(&state, target.clone(), dir, file(&name), &name).await;
        }
        let target_revision = commit(&target, &state, Hash::default(), branch_id()).await;

        let state = State::new();
        add(
            &state,
            repository.clone(),
            ROOT_NODE,
            file("top.bin"),
            "top.bin",
        )
        .await;
        let vendor = link("vendor", target_id, target_revision);
        add(&state, repository.clone(), ROOT_NODE, vendor, "vendor").await;
        let revision = commit(&repository, &state, Hash::default(), branch_id()).await;

        let tree = revision_state(&target, target_revision)
            .await
            .tree(target.clone())
            .await
            .expect("the tree of the linked revision");
        let blocks = lore_revision::immutable::read(
            target.clone(),
            Address::zero_context_hash(tree.hash_node),
            None,
            lore_revision::immutable::read_options_from_repository(&target),
        )
        .await
        .expect("the node block addresses");
        store.refuse(Hash::from(
            &blocks[size_of::<Hash>()..2 * size_of::<Hash>()],
        ));

        let walk = walk_through_failures(&repository, revision).await;
        assert_eq!(walk.attribution_failures.count, 1);
        let entries = attributed(&walk);
        assert_eq!(entries["top.bin"], (repository.id, revision));
        assert_eq!(entries["vendor"], (repository.id, revision));
        let linked: Vec<_> = entries
            .iter()
            .filter(|(path, _)| path.starts_with("vendor/"))
            .collect();
        assert!(
            !linked.is_empty(),
            "the linked walk lists an entry before it fails"
        );
        assert!(linked.len() <= BLOCK_NODE_COUNT, "the linked walk fails");
        for (path, named) in linked {
            assert_eq!(*named, (RepositoryId::default(), Hash::default()), "{path}");
        }
    }))
    .await
    .expect("Task failed");
}

/// A linked subtree that fails is skipped, and counted where the walk attributes, unless the store
/// refused a read in it as `SlowDown`, which fails the walk uncounted.
#[test]
fn a_linked_subtree_refused_as_slow_down_fails_the_walk() {
    let (repository, revision) = (RepositoryId::default(), Hash::default());
    let mut failures = AttributionFailures::default();
    let not_found = || StateError::from(NotFound);
    assert!(skip_linked_subtree("", repository, revision, not_found(), None).is_ok());
    assert!(
        skip_linked_subtree("", repository, revision, not_found(), Some(&mut failures)).is_ok()
    );
    assert_eq!(failures.count, 1);

    for counted in [None, Some(&mut failures)] {
        let refused = StateError::from(SlowDown);
        let skipped = skip_linked_subtree("", repository, revision, refused, counted);
        assert!(skipped.is_err_and(|err| err.is_slow_down()));
    }
    assert_eq!(failures.count, 1);
}
