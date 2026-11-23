// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What the served tree resolves each working-tree path to, across links and layers
//! (ADR-00036).
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
// The fixtures write and remove the files a mount holds directly, as a user would.
#![allow(clippy::disallowed_methods)]
use std::sync::Arc;

use dashmap::DashSet;
use lore_base::types::Hash;
use lore_revision::fs::swfs::file::SwfsFileArray;
use lore_revision::fs::swfs::frozen_files::FrozenFile;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::modified_tree::Coverage;
use lore_revision::fs::swfs::modified_tree::ModifiedTree;
use lore_revision::fs::swfs::paths::RelativePathKey;
use lore_revision::fs::swfs::served_tree::MountChange;
use lore_revision::fs::swfs::served_tree::Served;
use lore_revision::fs::swfs::served_tree::ServedTree;
use lore_revision::fs::swfs::served_tree::mount_changes;
use lore_revision::fs::swfs::state::ThawRemovals;
use lore_revision::fs::swfs::state::written_files;
use lore_revision::fs::swfs::view::ContentAnswer;
use lore_revision::fs::swfs::view::ListingAnswer;
use lore_revision::fs::swfs::view::SwfsView;
use lore_revision::layer::Layer;
use lore_revision::lore::RepositoryId;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::Held;
use super::listing_fixture::address_of;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::stored;
use super::listing_fixture::writable_test_repository;

fn linked_id() -> RepositoryId {
    RepositoryId::from([0x11; 16])
}

fn nested_id() -> RepositoryId {
    RepositoryId::from([0x22; 16])
}

fn layered_id() -> RepositoryId {
    RepositoryId::from([0x33; 16])
}

/// What a path resolves to, in a form a test can compare.
#[derive(Debug, PartialEq)]
enum Seen {
    File {
        repository: RepositoryId,
        content: Vec<u8>,
    },
    Directory {
        repository: RepositoryId,
    },
    Leading,
    Nothing,
}

fn seen(served: Option<Served>, contents: &[&[u8]]) -> Seen {
    match served {
        None => Seen::Nothing,
        Some(Served::Leading) => Seen::Leading,
        Some(Served::Node {
            repository, node, ..
        }) if node.is_directory() => Seen::Directory {
            repository: repository.id,
        },
        Some(Served::Node {
            repository, node, ..
        }) => Seen::File {
            repository: repository.id,
            content: contents
                .iter()
                .find(|content| address_of(content).hash == node.address.hash)
                .map_or_else(Vec::new, |content| content.to_vec()),
        },
    }
}

fn file(repository: RepositoryId, content: &[u8]) -> Seen {
    Seen::File {
        repository,
        content: content.to_vec(),
    }
}

fn directory(repository: RepositoryId) -> Seen {
    Seen::Directory { repository }
}

const CONTENTS: &[&[u8]] = &[
    b"parent",
    b"linked one",
    b"linked two",
    b"nested",
    b"layered",
];

struct Fixture {
    parent: Arc<RepositoryContext>,
    linked: Arc<RepositoryContext>,
    /// The linked repository's revision holding `src/lib.txt` ("linked one").
    pin_one: Hash,
    /// Its revision holding `src/lib.txt` ("linked two").
    pin_two: Hash,
}

impl Fixture {
    async fn new() -> Self {
        let parent = writable_test_repository().await;
        let linked = parent.to_link_context(linked_id()).await;
        let one = revision_state(&linked, &[("src/lib.txt", Held::File(b"linked one"))]).await;
        let two = revision_state(&linked, &[("src/lib.txt", Held::File(b"linked two"))]).await;
        Self {
            pin_one: stored(&linked, &one).await,
            pin_two: stored(&linked, &two).await,
            parent,
            linked,
        }
    }

    /// The node of `src` in the linked revision `pin`, which a link mounting `src` names.
    async fn src(&self, pin: Hash) -> u32 {
        State::deserialize(self.linked.clone(), pin)
            .await
            .expect("loading the linked revision")
            .find_node_link(self.linked.clone(), "src")
            .await
            .expect("finding src")
            .node
    }

    fn link(pin: Hash, source: u32) -> Held<'static> {
        Held::LinkTo {
            repository: linked_id(),
            pin,
            source,
        }
    }

    async fn state(&self, revision: &[(&str, Held<'_>)]) -> Arc<State> {
        revision_state(&self.parent, revision).await
    }

    async fn tree(
        &self,
        current: Arc<State>,
        staged: Option<Arc<State>>,
        layers: &[Layer],
    ) -> ServedTree {
        ServedTree::load(self.parent.clone(), Some(current), staged, layers)
            .await
            .expect("loading the served tree")
    }
}

async fn resolved(tree: &ServedTree, path: &str) -> Seen {
    seen(
        tree.resolve(&node(path)).await.expect("resolving the path"),
        CONTENTS,
    )
}

async fn listed(tree: &ServedTree, path: &str) -> Option<Vec<(String, Seen)>> {
    let children = tree
        .children(&node(path))
        .await
        .expect("listing the path")?;
    let mut listed: Vec<(String, Seen)> = children
        .into_iter()
        .map(|child| (child.name, seen(Some(child.served), CONTENTS)))
        .collect();
    listed.sort_by(|a, b| a.0.cmp(&b.0));
    Some(listed)
}

#[tokio::test]
async fn a_parent_path_is_served_from_the_current_revision() {
    in_context(async {
        let fixture = Fixture::new().await;
        let current = fixture
            .state(&[("dir/file.txt", Held::File(b"parent"))])
            .await;
        let tree = fixture.tree(current, None, &[]).await;
        let parent = fixture.parent.id;

        assert_eq!(
            resolved(&tree, "dir/file.txt").await,
            file(parent, b"parent")
        );
        assert_eq!(resolved(&tree, "dir").await, directory(parent));
        assert_eq!(resolved(&tree, "").await, directory(parent));
        assert_eq!(resolved(&tree, "missing.txt").await, Seen::Nothing);
    })
    .await;
}

#[tokio::test]
async fn a_top_level_link_is_served_from_the_linked_repository() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let current = fixture
            .state(&[("lib", Fixture::link(fixture.pin_one, src))])
            .await;
        let tree = fixture.tree(current, None, &[]).await;

        assert_eq!(resolved(&tree, "lib").await, directory(linked_id()));
        assert_eq!(
            resolved(&tree, "lib/lib.txt").await,
            file(linked_id(), b"linked one")
        );
        assert_eq!(
            resolved(&tree, "LIB/Lib.txt").await,
            file(linked_id(), b"linked one")
        );
        assert_eq!(resolved(&tree, "lib/missing.txt").await, Seen::Nothing);
    })
    .await;
}

#[tokio::test]
async fn a_link_is_served_at_the_pin_the_staged_state_holds() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src_one = fixture.src(fixture.pin_one).await;
        let src_two = fixture.src(fixture.pin_two).await;
        let current = fixture
            .state(&[("lib", Fixture::link(fixture.pin_one, src_one))])
            .await;
        let staged = fixture
            .state(&[("lib", Fixture::link(fixture.pin_two, src_two))])
            .await;
        let tree = fixture.tree(current, Some(staged), &[]).await;

        assert_eq!(
            resolved(&tree, "lib/lib.txt").await,
            file(linked_id(), b"linked two")
        );
    })
    .await;
}

#[tokio::test]
async fn a_link_only_the_staged_state_holds_is_served_with_the_directories_leading_to_it() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let current = fixture.state(&[("file.txt", Held::File(b"parent"))]).await;
        let staged = fixture
            .state(&[
                ("file.txt", Held::File(b"parent")),
                ("new/dir/lib", Fixture::link(fixture.pin_one, src)),
            ])
            .await;
        let tree = fixture.tree(current, Some(staged), &[]).await;

        assert_eq!(resolved(&tree, "new").await, Seen::Leading);
        assert_eq!(resolved(&tree, "new/dir").await, Seen::Leading);
        assert_eq!(resolved(&tree, "new/dir/lib").await, directory(linked_id()));
        assert_eq!(
            resolved(&tree, "new/dir/lib/lib.txt").await,
            file(linked_id(), b"linked one")
        );
        assert_eq!(resolved(&tree, "new/other").await, Seen::Nothing);
    })
    .await;
}

#[tokio::test]
async fn a_link_the_staged_state_removed_is_not_served() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let current = fixture
            .state(&[
                ("file.txt", Held::File(b"parent")),
                ("lib", Fixture::link(fixture.pin_one, src)),
            ])
            .await;
        let staged = fixture.state(&[("file.txt", Held::File(b"parent"))]).await;
        let tree = fixture.tree(current, Some(staged), &[]).await;

        assert_eq!(resolved(&tree, "lib").await, Seen::Nothing);
        assert_eq!(resolved(&tree, "lib/lib.txt").await, Seen::Nothing);
        assert_eq!(
            listed(&tree, "").await,
            Some(vec![(
                "file.txt".to_owned(),
                file(fixture.parent.id, b"parent")
            )])
        );
    })
    .await;
}

#[tokio::test]
async fn a_nested_link_is_crossed_at_the_pin_its_containing_revision_holds() {
    in_context(async {
        let fixture = Fixture::new().await;
        let nested = fixture.parent.to_link_context(nested_id()).await;
        let nested_state = revision_state(&nested, &[("deep.txt", Held::File(b"nested"))]).await;
        let nested_pin = stored(&nested, &nested_state).await;
        let outer = revision_state(
            &fixture.linked,
            &[
                ("src/lib.txt", Held::File(b"linked one")),
                (
                    "src/inner",
                    Held::LinkTo {
                        repository: nested_id(),
                        pin: nested_pin,
                        source: ROOT_NODE,
                    },
                ),
            ],
        )
        .await;
        let outer_pin = stored(&fixture.linked, &outer).await;
        let src = fixture.src(outer_pin).await;
        let current = fixture
            .state(&[("lib", Fixture::link(outer_pin, src))])
            .await;
        let tree = fixture.tree(current, None, &[]).await;

        assert_eq!(resolved(&tree, "lib/inner").await, directory(nested_id()));
        assert_eq!(
            resolved(&tree, "lib/inner/deep.txt").await,
            file(nested_id(), b"nested")
        );
        assert_eq!(
            listed(&tree, "lib").await,
            Some(vec![
                ("inner".to_owned(), directory(nested_id())),
                ("lib.txt".to_owned(), file(linked_id(), b"linked one")),
            ])
        );
    })
    .await;
}

async fn layer(fixture: &Fixture, target: &str) -> Layer {
    let layered = fixture.parent.to_layer_context(layered_id()).await;
    let state = revision_state(&layered, &[("lay/layered.txt", Held::File(b"layered"))]).await;
    Layer {
        target_path: target.to_owned(),
        source_path: "lay/".to_owned(),
        repository: layered_id(),
        metadata: None,
        current: stored(&layered, &state).await,
        staged: Hash::default(),
    }
}

#[tokio::test]
async fn a_layer_is_served_at_its_target_path() {
    in_context(async {
        let fixture = Fixture::new().await;
        let current = fixture.state(&[("file.txt", Held::File(b"parent"))]).await;
        let layers = [layer(&fixture, "deps/lay").await];
        let tree = fixture.tree(current, None, &layers).await;

        assert_eq!(resolved(&tree, "deps").await, Seen::Leading);
        assert_eq!(resolved(&tree, "deps/lay").await, directory(layered_id()));
        assert_eq!(
            resolved(&tree, "deps/lay/layered.txt").await,
            file(layered_id(), b"layered")
        );
        assert_eq!(
            listed(&tree, "deps/lay").await,
            Some(vec![(
                "layered.txt".to_owned(),
                file(layered_id(), b"layered")
            )])
        );
    })
    .await;
}

#[tokio::test]
async fn a_layer_target_the_parent_holds_is_served_from_the_parent() {
    in_context(async {
        let fixture = Fixture::new().await;
        let current = fixture
            .state(&[("lay/own.txt", Held::File(b"parent"))])
            .await;
        let layers = [layer(&fixture, "lay").await];
        let tree = fixture.tree(current, None, &layers).await;
        let parent = fixture.parent.id;

        assert_eq!(resolved(&tree, "lay").await, directory(parent));
        assert_eq!(
            resolved(&tree, "lay/own.txt").await,
            file(parent, b"parent")
        );
        assert_eq!(resolved(&tree, "lay/layered.txt").await, Seen::Nothing);
    })
    .await;
}

#[tokio::test]
async fn a_directory_holding_a_link_lists_it_as_a_directory() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let current = fixture
            .state(&[
                ("dir/file.txt", Held::File(b"parent")),
                ("dir/lib", Fixture::link(fixture.pin_one, src)),
            ])
            .await;
        let tree = fixture.tree(current, None, &[]).await;

        assert_eq!(
            listed(&tree, "dir").await,
            Some(vec![
                ("file.txt".to_owned(), file(fixture.parent.id, b"parent")),
                ("lib".to_owned(), directory(linked_id())),
            ])
        );
        assert_eq!(
            listed(&tree, "dir/lib").await,
            Some(vec![(
                "lib.txt".to_owned(),
                file(linked_id(), b"linked one")
            )])
        );
    })
    .await;
}

#[tokio::test]
async fn a_directory_leading_only_to_a_staged_link_is_listed() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let current = fixture.state(&[("file.txt", Held::File(b"parent"))]).await;
        let staged = fixture
            .state(&[
                ("file.txt", Held::File(b"parent")),
                ("new/lib", Fixture::link(fixture.pin_one, src)),
            ])
            .await;
        let tree = fixture.tree(current, Some(staged), &[]).await;

        assert_eq!(
            listed(&tree, "").await,
            Some(vec![
                ("file.txt".to_owned(), file(fixture.parent.id, b"parent")),
                ("new".to_owned(), Seen::Leading),
            ])
        );
        assert_eq!(
            listed(&tree, "new").await,
            Some(vec![("lib".to_owned(), directory(linked_id()))])
        );
    })
    .await;
}

#[tokio::test]
async fn a_file_lists_nothing() {
    in_context(async {
        let fixture = Fixture::new().await;
        let current = fixture.state(&[("file.txt", Held::File(b"parent"))]).await;
        let tree = fixture.tree(current, None, &[]).await;

        assert_eq!(listed(&tree, "file.txt").await, None);
        assert_eq!(listed(&tree, "missing").await, None);
    })
    .await;
}

/// A directory standing in for the mount, holding the empty files named by `files`.
fn mount_holding(test: &str, files: &[&str]) -> std::path::PathBuf {
    let mount =
        std::env::temp_dir().join(format!("lore-served-tree-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&mount).expect("making the stand-in mount");
    for file in files {
        std::fs::write(mount.join(file), b"").expect("making a file on the stand-in mount");
    }
    mount
}

/// A commit's removal is spelled as SWFS lists it, here below the spelling `Dir` had before the
/// user renamed it, and `gone` is a directory the user deleted that the revision still holds: an
/// entry naming either directory would respell the one and commit the deletion of the other.
#[tokio::test]
async fn the_directories_leading_to_what_a_commit_removed_are_not_named() {
    in_context(async {
        let fixture = Fixture::new().await;
        let current = fixture
            .state(&[
                ("DIR/kept.txt", Held::File(b"kept")),
                ("gone/kept.txt", Held::File(b"kept")),
            ])
            .await;
        let tree = fixture.tree(current, None, &[]).await;
        let written = DashSet::new();
        let mut removals = ThawRemovals::default();
        for path in ["dir/removed.txt", "gone/removed.txt"] {
            written.insert(RelativePathKey(node(path)));
            removals.committed.insert(RelativePathKey(node(path)));
        }
        written.insert(RelativePathKey(node("DIR/kept.txt")));

        let mount = mount_holding("the_directories_leading_to_a_removal", &[]);
        let mut files = Vec::new();
        written_files(
            &mut files,
            &tree,
            &mount,
            &removals,
            &DashSet::new(),
            &written,
            0,
        )
        .await
        .expect("describing the written paths");
        std::fs::remove_dir_all(&mount).expect("removing the stand-in mount");

        let mut described: Vec<String> = SwfsFileArray::new(files)
            .entries()
            .into_iter()
            .map(|(path, _, _)| path)
            .collect();
        described.sort();
        assert_eq!(
            described,
            [
                r"\DIR",
                r"\DIR\kept.txt",
                r"\dir\removed.txt",
                r"\gone\removed.txt"
            ]
        );
    })
    .await;
}

#[tokio::test]
async fn a_written_path_below_a_link_is_described_from_the_linked_state() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let current = fixture
            .state(&[
                ("file.txt", Held::File(b"parent")),
                ("lib", Fixture::link(fixture.pin_one, src)),
            ])
            .await;
        let tree = fixture.tree(current, None, &[]).await;
        let written = DashSet::new();
        for path in ["lib/lib.txt", "gone.txt"] {
            written.insert(RelativePathKey(node(path)));
        }

        let mount = mount_holding("a_written_path_below_a_link", &["gone.txt"]);
        let mut files = Vec::new();
        let named = written_files(
            &mut files,
            &tree,
            &mount,
            &ThawRemovals::default(),
            &DashSet::new(),
            &written,
            0,
        )
        .await
        .expect("describing the written paths");
        std::fs::remove_dir_all(&mount).expect("removing the stand-in mount");

        let mut described = SwfsFileArray::new(files).entries();
        described.sort();
        assert_eq!(
            described,
            vec![
                (r"\gone.txt".to_owned(), false, 0),
                (r"\lib".to_owned(), true, 0),
                (
                    r"\lib\lib.txt".to_owned(),
                    false,
                    b"linked one".len() as u64
                ),
            ]
        );
        // The link's mount path is described as a directory, which keeps what lies below it.
        let mut named: Vec<String> = named.iter().map(|path| path.as_str().to_owned()).collect();
        named.sort();
        assert_eq!(named, vec!["gone.txt", "lib/lib.txt"]);
    })
    .await;
}

#[tokio::test]
async fn a_removed_path_the_mount_holds_nothing_at_or_swfs_holds_deleted_is_not_described() {
    in_context(async {
        let fixture = Fixture::new().await;
        let current = fixture.state(&[("file.txt", Held::File(b"parent"))]).await;
        let tree = fixture.tree(current, None, &[]).await;
        let written = DashSet::new();
        for path in ["gone.txt", "absent.txt", "kept.txt"] {
            written.insert(RelativePathKey(node(path)));
        }
        let mut removals = ThawRemovals::default();
        removals
            .held_deleted
            .insert(RelativePathKey(node("kept.txt")));

        // `gone.txt` is on the mount, and is named. `absent.txt` is not, and `kept.txt` is held
        // deleted by SWFS, which lists it still: an entry naming either would read to SWFS as
        // something that exists.
        let mount = mount_holding(
            "a_removed_path_the_mount_holds_nothing_at",
            &["gone.txt", "kept.txt"],
        );
        let mut files = Vec::new();
        let named = written_files(
            &mut files,
            &tree,
            &mount,
            &removals,
            &DashSet::new(),
            &written,
            0,
        )
        .await
        .expect("describing the written paths");
        std::fs::remove_dir_all(&mount).expect("removing the stand-in mount");

        assert_eq!(
            SwfsFileArray::new(files).entries(),
            vec![(r"\gone.txt".to_owned(), false, 0)]
        );
        assert_eq!(named, vec![node("gone.txt")]);
    })
    .await;
}

#[tokio::test]
async fn a_path_a_commit_removed_is_described_where_the_revision_holds_nothing_there() {
    in_context(async {
        let fixture = Fixture::new().await;
        let current = fixture.state(&[("kept.txt", Held::File(b"kept"))]).await;
        let tree = fixture.tree(current, None, &[]).await;
        let written = DashSet::new();
        let mut removals = ThawRemovals::default();
        for path in ["origin.txt", "kept.txt"] {
            written.insert(RelativePathKey(node(path)));
            removals.committed.insert(RelativePathKey(node(path)));
        }
        // SWFS holds `origin.txt` deleted and the mount holds nothing there, yet it is named, which
        // takes the deletion off SWFS's list. The revision holds `kept.txt`, so the commit did not
        // take in its removal, and it is not named.
        let mount = mount_holding("a_path_a_commit_removed", &[]);
        let mut files = Vec::new();
        let named = written_files(
            &mut files,
            &tree,
            &mount,
            &removals,
            &DashSet::new(),
            &written,
            0,
        )
        .await
        .expect("describing the written paths");
        std::fs::remove_dir_all(&mount).expect("removing the stand-in mount");

        assert_eq!(
            SwfsFileArray::new(files).entries(),
            vec![(r"\origin.txt".to_owned(), false, 0)]
        );
        assert_eq!(named, vec![node("origin.txt")]);
    })
    .await;
}

/// A view over `tree` with a trusted record holding `changes`, and nothing frozen.
async fn view_over(fixture: &Fixture, tree: ServedTree, changes: Vec<Change>) -> SwfsView {
    let record = Arc::new(ModifiedTree::new(fixture.parent.clone()));
    for change in changes {
        record.record(change).await;
    }
    record.set_trusted(true);
    SwfsView::new(
        record,
        FrozenSnapshot::capture(Arc::new(FakeFrozen::default())).expect("capturing the snapshot"),
        Arc::new(tree),
    )
}

async fn linked_tree(fixture: &Fixture) -> ServedTree {
    let src = fixture.src(fixture.pin_one).await;
    let current = fixture
        .state(&[
            ("file.txt", Held::File(b"parent")),
            ("lib", Fixture::link(fixture.pin_one, src)),
        ])
        .await;
    fixture.tree(current, None, &[]).await
}

#[tokio::test]
async fn the_view_lists_a_link_and_reads_an_unmodified_linked_file_from_its_store() {
    in_context(async {
        let fixture = Fixture::new().await;
        let view = view_over(&fixture, linked_tree(&fixture).await, Vec::new()).await;

        let mut root = match view
            .read_directory(&node(""))
            .await
            .expect("listing the root")
        {
            ListingAnswer::Listing(listing) => listing,
            ListingAnswer::Fallback(reason) => panic!("the root listing fell back: {reason}"),
        };
        let mut names = Vec::new();
        while let Some(entry) = root.next().await {
            let entry = entry.expect("a root entry");
            names.push((entry.name, entry.info.is_dir()));
        }
        names.sort();
        assert_eq!(
            names,
            vec![("file.txt".to_owned(), false), ("lib".to_owned(), true)]
        );

        let content = match view
            .content_source(&node("lib/lib.txt"))
            .await
            .expect("reading the linked file")
        {
            ContentAnswer::Source(source) => source.read_all().await.expect("reading").to_vec(),
            ContentAnswer::Fallback(reason) => panic!("the read fell back: {reason}"),
        };
        assert_eq!(content, b"linked one");
        assert!(
            view.revision_holds(&node("lib/lib.txt"))
                .await
                .expect("asking the view")
        );
        assert!(
            !view
                .revision_holds(&node("lib/missing.txt"))
                .await
                .expect("asking the view")
        );
    })
    .await;
}

#[tokio::test]
async fn the_rebuild_judges_a_path_below_a_link_against_the_linked_state() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let current = fixture.state(&[("file.txt", Held::File(b"parent"))]).await;
        let staged = fixture
            .state(&[
                ("file.txt", Held::File(b"parent")),
                ("new/lib", Fixture::link(fixture.pin_one, src)),
            ])
            .await;
        let tree = fixture.tree(current, Some(staged), &[]).await;
        let record = ModifiedTree::new(fixture.parent.clone());
        // The flags SWFS gives a deletion.
        const DELETED: u32 = 0x000a;
        let deleted = |path: &str| FrozenFile {
            path: path.to_owned(),
            mod_flags: DELETED,
            file_index: 0,
            size: 0,
            time_create: 0,
            time_write: 0,
            is_directory: false,
        };

        record
            .rebuild(
                &[deleted(r"\new\lib\lib.txt"), deleted(r"\new\lib\gone.txt")],
                Some(&tree),
            )
            .await
            .expect("rebuilding the record");

        // A deletion is kept where what is served there holds the path, and one of a path never
        // served, created and deleted since, deleted nothing.
        assert_eq!(
            record.coverage("new/lib/lib.txt").await.expect("asking"),
            Coverage::Deleted
        );
        assert_eq!(
            record.coverage("new/lib/gone.txt").await.expect("asking"),
            Coverage::Unchanged
        );
    })
    .await;
}

fn named(changes: Vec<(RelativePath, MountChange)>) -> Vec<(String, MountChange)> {
    let mut named: Vec<(String, MountChange)> = changes
        .into_iter()
        .map(|(path, change)| (path.as_str().to_owned(), change))
        .collect();
    named.sort_by(|a, b| a.0.cmp(&b.0));
    named
}

#[tokio::test]
async fn mount_changes_name_added_and_removed_links_and_layers() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src = fixture.src(fixture.pin_one).await;
        let link = Fixture::link(fixture.pin_one, src);
        let before = fixture
            .tree(
                fixture.state(&[("kept", link), ("gone", link)]).await,
                None,
                &[layer(&fixture, "old_layer").await],
            )
            .await;
        let after = fixture
            .tree(
                fixture.state(&[("kept", link), ("new", link)]).await,
                None,
                &[layer(&fixture, "new_layer").await],
            )
            .await;

        assert_eq!(
            named(mount_changes(&before, &after)),
            vec![
                ("gone".to_owned(), MountChange::Removed),
                ("new".to_owned(), MountChange::Added),
                ("new_layer".to_owned(), MountChange::Added),
                ("old_layer".to_owned(), MountChange::Removed),
            ]
        );
        assert!(mount_changes(&after, &after).is_empty());
    })
    .await;
}

#[tokio::test]
async fn mount_changes_name_a_repinned_link() {
    in_context(async {
        let fixture = Fixture::new().await;
        let src_one = fixture.src(fixture.pin_one).await;
        let src_two = fixture.src(fixture.pin_two).await;
        let before = fixture
            .tree(
                fixture
                    .state(&[("lib", Fixture::link(fixture.pin_one, src_one))])
                    .await,
                None,
                &[],
            )
            .await;
        let after = fixture
            .tree(
                fixture
                    .state(&[("lib", Fixture::link(fixture.pin_two, src_two))])
                    .await,
                None,
                &[],
            )
            .await;

        assert_eq!(
            named(mount_changes(&before, &after)),
            vec![("lib".to_owned(), MountChange::Repinned)]
        );
    })
    .await;
}

#[tokio::test]
async fn what_a_staged_pin_deletes_is_not_served() {
    in_context(async {
        let fixture = Fixture::new().await;
        let staged_pin_state = revision_state(
            &fixture.linked,
            &[
                ("src/lib.txt", Held::File(b"linked one")),
                ("src/gone.txt", Held::StagedDeleteFile(b"linked two")),
            ],
        )
        .await;
        let pin = stored(&fixture.linked, &staged_pin_state).await;
        let src = fixture.src(pin).await;
        let current = fixture.state(&[("lib", Fixture::link(pin, src))]).await;
        let tree = fixture.tree(current, None, &[]).await;

        assert_eq!(resolved(&tree, "lib/gone.txt").await, Seen::Nothing);
        assert_eq!(
            listed(&tree, "lib").await,
            Some(vec![(
                "lib.txt".to_owned(),
                file(linked_id(), b"linked one")
            )])
        );
    })
    .await;
}
