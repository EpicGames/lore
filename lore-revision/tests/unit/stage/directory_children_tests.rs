// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_revision::fs::filesystem_provider::DirectoryEntry;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::stage::*;
use lore_revision::state::NodeMapping;
use lore_revision::state::State;
use lore_revision::state::step_counting;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

use crate::fs::filesystem_provider::test_store_create;
use crate::helper::test_commit_tree;
use crate::helper::test_file_write;
use crate::helper::test_repository_create;
use crate::state::add;
use crate::state::directory;
use crate::state::file;
use crate::state::with_state;

/// The children in sibling order, as `stage_directory` indexes them.
fn children(entries: &[(NodeID, u64)]) -> DirectoryChildren {
    DirectoryChildren::new(entries.to_vec(), None)
}

#[test]
fn a_claim_finds_the_child_with_that_name() {
    let mut children = children(&[(10, 0xAA), (11, 0xBB), (12, 0xCC)]);
    assert_eq!(children.claim(0xBB).map(|(node, _)| node), Some(11));
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), Some(10));
    assert_eq!(children.claim(0xCC).map(|(node, _)| node), Some(12));
    assert_eq!(
        children.unclaimed().collect::<Vec<_>>(),
        Vec::<NodeID>::new()
    );
}

#[test]
fn a_name_the_directory_does_not_hold_claims_nothing() {
    let mut children = children(&[(10, 0xAA)]);
    assert_eq!(children.claim(0xBB).map(|(node, _)| node), None);
    assert_eq!(children.unclaimed().collect::<Vec<_>>(), vec![10]);
}

/// Two names differing only in case hash the same, and a claim takes the
/// first child not already claimed. Sibling order is what makes that choice
/// reproducible, so it has to survive the sort.
#[test]
fn equal_hashes_are_claimed_in_sibling_order() {
    let mut children = children(&[(10, 0xAA), (11, 0xAA), (12, 0xAA)]);
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), Some(10));
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), Some(11));
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), Some(12));
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), None);
}

/// A run of children sharing a hash, wide enough that a sort keyed on the
/// hash alone reorders it.
fn wide_runs_of_equal_hashes() -> Vec<(NodeID, u64)> {
    (0..64u64)
        .map(|index| (index as NodeID + 100, index % 3))
        .collect()
}

/// The index keys on the sibling position as well as the hash, so a run of
/// equal hashes carries no ties for the sort to order as it likes.
#[test]
fn the_index_leaves_no_ties_among_equal_hashes() {
    let children = children(&wide_runs_of_equal_hashes());
    assert!(
        children
            .by_name_hash
            .0
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "the index must be strictly ordered: {:?}",
        children.by_name_hash.0
    );
}

/// A claim takes the child out of what the listing offers, not out of what
/// it holds: a search of the chain still reaches it, so the index still
/// answers for it.
#[test]
fn a_claimed_child_is_still_held() {
    let mut children = children(&[(10, 0xAA), (11, 0xAA), (12, 0xBB)]);
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), Some(10));
    assert_eq!(children.holds(0xAA), Some(10));
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), Some(11));
    assert_eq!(
        children.claim(0xAA).map(|(node, _)| node),
        None,
        "the listing offers no more"
    );
    assert_eq!(children.holds(0xAA), Some(10), "the listing still holds it");
    assert_eq!(children.holds(0xCC), None, "a name it never held");
}

#[test]
fn what_nothing_claimed_comes_back_in_sibling_order() {
    let mut children = children(&[(10, 0xAA), (11, 0xBB), (12, 0xAA), (13, 0xCC)]);
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), Some(10));
    assert_eq!(children.claim(0xCC).map(|(node, _)| node), Some(13));
    assert_eq!(children.unclaimed().collect::<Vec<_>>(), vec![11, 12]);
}

#[test]
fn an_empty_directory_claims_nothing_and_deletes_nothing() {
    let mut children = children(&[]);
    assert_eq!(children.claim(0xAA).map(|(node, _)| node), None);
    assert_eq!(children.unclaimed().count(), 0);
}

/// The first child carrying `name_hash` that nothing has taken, taken, found
/// by scanning `scanned` in sibling order.
fn scan_claim(scanned: &mut [Option<(NodeID, u64)>], name_hash: u64) -> Option<NodeID> {
    let position = scanned
        .iter()
        .position(|entry| entry.is_some_and(|(_, hash)| hash == name_hash))?;
    scanned[position].take().map(|(node, _)| node)
}

/// The index has to agree with a scan of the same children on every input,
/// not just the ones written out above: same children, same sequence of
/// claims, same answers and same leftovers.
///
/// The claims cover every hash the children hold and two they do not, each
/// asked for more times than the children can answer, so duplicates, misses
/// and exhausted runs all occur.
#[test]
fn the_index_answers_exactly_as_a_scan_of_the_same_children_would() {
    let entries = wide_runs_of_equal_hashes();
    let mut indexed = children(&entries);
    let mut scanned: Vec<Option<(NodeID, u64)>> = entries.iter().copied().map(Some).collect();

    for step in 0..160u64 {
        let name_hash = (step * 7) % 5;
        assert_eq!(
            indexed.claim(name_hash).map(|(node, _)| node),
            scan_claim(&mut scanned, name_hash),
            "claim({name_hash}) at step {step}"
        );
    }
    assert_eq!(
        indexed.unclaimed().collect::<Vec<_>>(),
        scanned
            .iter()
            .filter_map(|entry| entry.map(|(node, _)| node))
            .collect::<Vec<_>>()
    );
}

/// File system entries of files named `names`, as a directory's listing yields them.
fn file_entries<S: AsRef<str>>(names: &[S]) -> Vec<DirectoryEntry> {
    names
        .iter()
        .map(|name| DirectoryEntry {
            name: name.as_ref().to_string(),
            info: FileInfo::File {
                executable: None,
                size: 0,
                mtime: 0,
            },
            name_hash: hash_string(name.as_ref()),
        })
        .collect()
}

/// The children of `dir` as `stage_directory` lists them, with the mark of its chain.
async fn listed(
    state: &State,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
) -> DirectoryChildren {
    let (listing, mark) = state
        .node_children_with_name_hash_marked(repository.clone(), dir)
        .await
        .expect("the listing");
    DirectoryChildren::new(listing, mark)
}

/// The stage's claim of `name_hash` in `dir`, as `stage_directory` makes it: from the listing
/// where that answers alone, else by a search of the chain.
async fn claim(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
    children: &mut DirectoryChildren,
    entries: &[DirectoryEntry],
    name_hash: u64,
) -> Option<NodeID> {
    match claim_from_listing(children, state, entries, name_hash) {
        Some(claimed) => claimed,
        None => claim_child(repository, state, dir, children, name_hash)
            .await
            .expect("the claim runs"),
    }
    .map(|claimed| claimed.node)
}

/// Claims each of `entries` in `dir` in turn, as `stage_directory` does, and adds a file for each
/// claim that found no child, counting the add as the stage's own. Each claim's answer with the
/// child it leaves its entry.
async fn claim_each(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    dir: NodeID,
    entries: &[DirectoryEntry],
) -> Vec<(Option<NodeID>, NodeID)> {
    let mut children = listed(state, repository, dir).await;
    let mut claims = Vec::with_capacity(entries.len());
    for entry in entries {
        let claimed = claim(
            state,
            repository,
            dir,
            &mut children,
            entries,
            entry.name_hash,
        )
        .await;
        let child = if let Some(child) = claimed {
            child
        } else {
            let added = add(state, repository, dir, file(&entry.name), &entry.name).await;
            children.count_own_add();
            added
        };
        claims.push((claimed, child));
    }
    claims
}

/// Staging a directory the tree does not hold, holding 256 new files, through the stage's walk
/// of the path on this thread: the listing and the claims step through no child, where searching
/// what the stage prepended for each claim would step through 32,640.
#[tokio::test]
async fn staging_a_new_directory_of_new_files_steps_through_no_child() {
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            test_file_write(&instance.path.join("seed.txt"), b"seed");
            let state = test_commit_tree(&instance, "First").await;
            let repository = instance.repository.clone();
            #[allow(clippy::disallowed_methods)]
            std::fs::create_dir(instance.path.join("d")).expect("mkdir d");
            for index in 0..256 {
                test_file_write(&instance.path.join("d").join(format!("f{index}")), b"f");
            }
            let operation = repository
                .file_system()
                .begin_operation()
                .await
                .expect("a filesystem operation");
            let before = walk_steps_on_this_thread();
            let staged = stage_filesystem_path(
                operation.clone(),
                NodeMapping::root(repository.clone(), state.clone()),
                RelativePath::new().join("d"),
                KnownChild::Unresolved,
                Arc::default(),
                StageOptions::default(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("the stage");
            let walked = walk_steps_on_this_thread() - before;
            operation.finalize().await.expect("the operation finishes");
            assert!(staged.is_valid(), "d is staged");
            let children = state
                .node_children(repository.clone(), staged.node)
                .await
                .expect("d's children");
            assert_eq!(children.len(), 256, "every file is staged");
            assert_eq!(walked, 0, "the stage stepped through {walked} children");
        })
        .await;
}

/// The chain steps the calling thread's children walks have taken.
fn walk_steps_on_this_thread() -> u64 {
    step_counting::set_chain_step_counting(true);
    step_counting::thread_walk_steps()
}

/// Claims missing the listing before the stage added a child answer from the listing, with no
/// search and no check of the entries' name hashes, though two of them share one.
#[tokio::test]
async fn claims_before_the_stage_adds_answer_from_the_listing() {
    with_state(|repository, state| async move {
        let dir = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let held = add(&state, &repository, dir, file("Rock.mesh"), "Rock.mesh").await;
        let entries = file_entries(&["Rock.mesh", "rock.mesh", "new.file"]);
        let mut children = listed(&state, &repository, dir).await;
        let mut from_listing = |index: usize| {
            claim_from_listing(&mut children, &state, &entries, entries[index].name_hash)
                .map(|claimed| claimed.map(|claimed| claimed.node))
        };
        assert_eq!(from_listing(0), Some(Some(held)), "the listed child");
        assert_eq!(
            from_listing(1),
            Some(Some(held)),
            "the listed child, claimed"
        );
        assert_eq!(from_listing(2), Some(None), "no child");
        assert_eq!(
            children.shared_hashes, None,
            "no claim checked the entries' name hashes"
        );
    })
    .await;
}

/// Two entries of one name hash, as two names differing only in case are: the claim of the
/// second searches what the stage prepended and finds the node it added for the first, among
/// entries whose hashes sort on the stack and among more.
#[tokio::test]
async fn a_claim_finds_the_add_of_an_entry_of_its_name_hash() {
    with_state(|repository, state| async move {
        for count in [2, 300] {
            let name = format!("d{count}");
            let dir = add(&state, &repository, ROOT_NODE, directory(&name), &name).await;
            let mut names = vec!["Rock.mesh".to_string()];
            names.extend((2..count).map(|index| format!("f{index}")));
            names.push("rock.mesh".to_string());
            let claims = claim_each(&state, &repository, dir, &file_entries(&names)).await;
            let (first, added) = claims[0];
            assert_eq!(first, None, "{count}: the first is new");
            assert_eq!(
                claims[count - 1].0,
                Some(added),
                "{count}: the second finds the first one's add"
            );
        }
    })
    .await;
}

/// A child another writer linked into the chain since the listing, which the stage did not count,
/// is found by the claim of its name, though the entries carry distinct name hashes.
#[tokio::test]
async fn a_claim_finds_a_child_another_writer_linked_in_since_the_listing() {
    with_state(|repository, state| async move {
        let dir = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let entries = file_entries(&["x", "y"]);
        let mut children = listed(&state, &repository, dir).await;
        let linked = add(&state, &repository, dir, file("x"), "x").await;
        let claimed = claim(
            &state,
            &repository,
            dir,
            &mut children,
            &entries,
            entries[0].name_hash,
        )
        .await;
        assert_eq!(claimed, Some(linked));
    })
    .await;
}

/// After the stage's own add, the claim of an entry whose name hash no other entry carries answers
/// from the listing, though two other entries share one: only the second of those searches.
#[tokio::test]
async fn an_own_add_leaves_the_claims_of_unshared_names_to_the_listing() {
    with_state(|repository, state| async move {
        let dir = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let entries = file_entries(&["Rock.mesh", "rock.mesh", "new.file"]);
        let mut children = listed(&state, &repository, dir).await;
        let from_listing = |children: &mut DirectoryChildren, index: usize| {
            claim_from_listing(children, &state, &entries, entries[index].name_hash)
                .map(|claimed| claimed.map(|claimed| claimed.node))
        };
        assert_eq!(
            from_listing(&mut children, 0),
            Some(None),
            "the first is new"
        );
        add(&state, &repository, dir, file("Rock.mesh"), "Rock.mesh").await;
        children.count_own_add();
        assert_eq!(
            from_listing(&mut children, 2),
            Some(None),
            "no child, from the listing"
        );
        assert_eq!(from_listing(&mut children, 1), None, "its variant searches");
    })
    .await;
}

/// Each entry of a name hash two listed children carry finds the first of them, the child a search
/// of the chain reaches, and leaves neither to the pass over what nothing claimed.
#[tokio::test]
async fn a_find_answers_each_entry_with_the_first_child_of_its_name_hash() {
    with_state(|repository, state| async move {
        let dir = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        add(&state, &repository, dir, file("x"), "x").await;
        let first = add(&state, &repository, dir, directory("x"), "x").await;
        let other = add(&state, &repository, dir, file("other"), "other").await;
        let entries = file_entries(&["X", "x"]);
        let mut children = listed(&state, &repository, dir).await;
        for entry in &entries {
            assert_eq!(
                find_from_listing(&mut children, &state, &entries, entry.name_hash)
                    .map(|found| found.map(|found| found.node)),
                Some(Some(first)),
                "{}",
                entry.name
            );
            assert_eq!(
                state
                    .find_subnode(repository.clone(), dir, entry.name_hash)
                    .await
                    .ok(),
                Some(first),
                "{}",
                entry.name
            );
        }
        assert_eq!(children.unclaimed().collect::<Vec<_>>(), vec![other]);
    })
    .await;
}

/// A directory lists its children with the mark of its chain; a file, and a link to a revision the
/// store lacks, list none, the link without its State.
#[tokio::test]
async fn only_a_directory_lists_children() {
    with_state(|repository, state| async move {
        let dir = add(&state, &repository, ROOT_NODE, directory("d"), "d").await;
        let child = add(&state, &repository, dir, file("f"), "f").await;
        let link = Node {
            flags: NodeFlags::Link.bits(),
            name_hash: hash_string("link"),
            child: ROOT_NODE,
            address: Address {
                hash: Hash::from_u64(0x11b),
                context: uuid::Uuid::now_v7().into(),
            },
            ..Default::default()
        };
        let link = add(&state, &repository, ROOT_NODE, link, "link").await;
        let listed = directory_children(&repository, &state, dir)
            .await
            .expect("the directory's listing")
            .map(|(listed, _)| listed);
        assert_eq!(listed, Some(vec![(child, hash_string("f"))]));
        for node in [child, link] {
            assert!(
                directory_children(&repository, &state, node)
                    .await
                    .expect("the listing")
                    .is_none()
            );
        }
    })
    .await;
}
