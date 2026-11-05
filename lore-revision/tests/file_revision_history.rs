// SPDX-FileCopyrightText: 2026 LoreLab.io
// SPDX-License-Identifier: MIT

//! Exercises per-entry change attribution, which `TreeNode` last-revision
//! builds on: the `revision[0]` back-pointer, directory propagation and an
//! unattributed root, then `tree()` attribution across links and merges.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Address;
    use lore_base::types::BranchId;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::commit::commit_in_memory_revision;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::metadata::MetadataInherit;
    use lore_revision::node::*;
    use lore_revision::repository;
    use lore_revision::repository::InMemoryContext;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::revision::tree;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state::State;
    use lore_revision::state::allow_all_repositories;
    use lore_revision::util::path::RelativePath;
    use lore_storage::hash::hash_string;
    use lore_storage::local::immutable_store::LocalImmutableStore;

    include!("helper.rs");

    struct InMemoryMarker;
    impl InMemoryContext for InMemoryMarker {}
    const IN_MEMORY_MARKER: InMemoryMarker = InMemoryMarker;

    /// Path-less context, matching the shape the in-memory revision tree builds on.
    async fn test_repository(
        mutable_store: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        let immutable_store = LocalImmutableStore::new(
            None,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("Failed to create store");
        Arc::new(
            RepositoryContext::new(default_repository_creation_args(
                immutable_store,
                mutable_store,
            ))
            .with_write_token(RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER)),
        )
    }

    fn file(name: &str, content: u64) -> Node {
        Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 10,
            address: Address {
                hash: Hash::from_u64(content),
                context: Context::from(uuid::Uuid::now_v7()),
            },
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    fn directory(name: &str) -> Node {
        Node {
            flags: NodeFlags::NoFlags.bits(),
            mode: 0o755,
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    async fn add(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
    ) -> NodeID {
        let node_id = state
            .node_add(repository.clone(), parent, node, name)
            .await
            .expect("adding the node must succeed");
        state
            .node_mark_staged(
                repository,
                node_id,
                NodeFlags::StagedAdd,
                NodeFlags::DirtyAdd,
            )
            .await
            .expect("marking the addition must succeed");
        node_id
    }

    fn metadata_on(branch: BranchId) -> Metadata {
        let mut metadata = Metadata::new();
        metadata
            .set_branch(branch)
            .expect("setting the branch must succeed");
        metadata
    }

    fn token() -> RepositoryWriteToken {
        RepositoryWriteToken::in_memory(&IN_MEMORY_MARKER)
    }

    fn branch_id() -> BranchId {
        Context::from(uuid::Uuid::now_v7())
    }

    /// Slot 0 of a node's file-metadata record. Slot 1 carries the other side of
    /// a merge, which this test does not exercise.
    ///
    /// Note: `node` and `action` are only read if the println block at the end
    ///       of the test is enabled.
    #[allow(dead_code)]
    struct FileMetadataRecord {
        revision: Hash,
        node: u32,
        action: u16,
    }

    /// The file-metadata record for a node, read as `file::history` does.
    async fn file_metadata_of(
        state: &State,
        repository: Arc<RepositoryContext>,
        node_id: NodeID,
    ) -> FileMetadataRecord {
        let metadata_node_id = node_to_file_metadata(node_id);
        let block_index = NodeFileMetadataBlock::index(metadata_node_id);
        let node_index = NodeFileMetadata::index(metadata_node_id);
        let block = state
            .block_file_metadata(repository, block_index)
            .await
            .expect("the file-metadata block must read back");
        let reader = block.read();
        let record = reader.node(node_index);
        FileMetadataRecord {
            revision: record.revision[0],
            node: record.node[0],
            action: record.action[0],
        }
    }

    async fn node_id_for(state: &State, repository: Arc<RepositoryContext>, path: &str) -> NodeID {
        state
            .find_node_link(repository, path)
            .await
            .unwrap_or_else(|err| panic!("path {path} must resolve at this revision: {err:?}"))
            .node
    }

    /// r1 adds both files, r2 and r3 modify `touched.bin`, r4 modifies
    /// `untouched.bin`. Reading the records at each tip separates a per-entry
    /// back-pointer from a parent-revision stamp.
    ///
    /// Four revisions are needed. With only r1 and r2 both files point at r1,
    /// which hides that difference. r4 covers an entry unchanged at the tip.
    #[tokio::test]
    async fn file_metadata_revision_attribution_at_the_tip() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();

                // r1: add a/touched.bin and a/untouched.bin
                let staged = State::new();
                let dir = add(&staged, repository.clone(), ROOT_NODE, directory("a"), "a").await;
                add(
                    &staged,
                    repository.clone(),
                    dir,
                    file("touched.bin", 0x11),
                    "touched.bin",
                )
                .await;
                add(
                    &staged,
                    repository.clone(),
                    dir,
                    file("untouched.bin", 0x22),
                    "untouched.bin",
                )
                .await;

                let r1 = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    staged,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("committing r1 must succeed");

                // r2: modify a/touched.bin only
                let staged2 = State::deserialize(repository.clone(), r1)
                    .await
                    .expect("r1 must deserialize");
                let touched_id = node_id_for(&staged2, repository.clone(), "a/touched.bin").await;
                staged2
                    .node_modify(
                        repository.clone(),
                        touched_id,
                        0o644,
                        4096,
                        Address {
                            hash: Hash::from_u64(0x33),
                            context: Context::default(),
                        },
                    )
                    .await
                    .expect("modifying the file must succeed");
                staged2
                    .node_mark_staged(
                        repository.clone(),
                        touched_id,
                        NodeFlags::StagedModify,
                        NodeFlags::DirtyModify,
                    )
                    .await
                    .expect("marking the modification must succeed");

                let r2 = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    staged2,
                    metadata_on(branch),
                    r1,
                    branch,
                )
                .await
                .expect("committing r2 must succeed");

                assert_ne!(r1, r2, "the two commits must be distinct revisions");

                // r3: Modify a/touched.bin again so we diverge
                let staged3 = State::deserialize(repository.clone(), r2)
                    .await
                    .expect("r2 must deserialize");
                let touched_id3 = node_id_for(&staged3, repository.clone(), "a/touched.bin").await;
                staged3
                    .node_modify(
                        repository.clone(),
                        touched_id3,
                        0o644,
                        8192,
                        Address {
                            hash: Hash::from_u64(0x44),
                            context: Context::default(),
                        },
                    )
                    .await
                    .expect("modifying the file again must succeed");
                staged3
                    .node_mark_staged(
                        repository.clone(),
                        touched_id3,
                        NodeFlags::StagedModify,
                        NodeFlags::DirtyModify,
                    )
                    .await
                    .expect("marking the second modification must succeed");

                let r3 = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    staged3,
                    metadata_on(branch),
                    r2,
                    branch,
                )
                .await
                .expect("committing r3 must succeed");

                assert_ne!(r2, r3, "r3 must be a distinct revision");
                assert_ne!(r1, r3, "r3 must also differ from r1");
                assert_eq!(
                    branch::load_latest(repository.clone(), branch)
                        .await
                        .expect("the branch tip must read back"),
                    r3,
                    "the branch tip must be r3"
                );

                // Observe the metadata records at the tip
                let tip = State::deserialize(repository.clone(), r3)
                    .await
                    .expect("r3 must deserialize");

                let touched = node_id_for(&tip, repository.clone(), "a/touched.bin").await;
                let untouched = node_id_for(&tip, repository.clone(), "a/untouched.bin").await;
                let dir_at_tip = node_id_for(&tip, repository.clone(), "a").await;

                let touched_meta = file_metadata_of(&tip, repository.clone(), touched).await;
                let untouched_meta = file_metadata_of(&tip, repository.clone(), untouched).await;
                let dir_meta = file_metadata_of(&tip, repository.clone(), dir_at_tip).await;
                let root_meta = file_metadata_of(&tip, repository.clone(), ROOT_NODE).await;

                // Without the modification, every metadata reading is pointless
                let touched_node = tip
                    .node(repository.clone(), touched)
                    .await
                    .expect("the touched node must read back");
                let untouched_node = tip
                    .node(repository.clone(), untouched)
                    .await
                    .expect("the untouched node must read back");

                assert_eq!(
                    touched_node.size, 8192,
                    "r3's modification must be in the committed tree"
                );
                assert_eq!(untouched_node.size, 10, "untouched.bin must be untouched");

                // A failure below means Lore's attribution semantics have changed
                assert_eq!(
                    touched_meta.revision, r2,
                    "touched.bin changed in r3, so it must back-point at r2"
                );
                assert_eq!(
                    untouched_meta.revision, r1,
                    "untouched.bin is unchanged since r1, so it must still point at r1"
                );
                // Directories propagate, so folder rows can be attributed directly
                assert_eq!(
                    dir_meta.revision, r2,
                    "directory 'a' must move with its changed child, not stay at r1"
                );
                assert!(
                    root_meta.revision.is_zero(),
                    "the root node carries no attribution"
                );

                // r4: Modify untouched.bin only, so touched.bin is not changed at the tip
                let staged4 = State::deserialize(repository.clone(), r3)
                    .await
                    .expect("r3 must deserialize");
                let untouched_id4 =
                    node_id_for(&staged4, repository.clone(), "a/untouched.bin").await;
                staged4
                    .node_modify(
                        repository.clone(),
                        untouched_id4,
                        0o644,
                        2048,
                        Address {
                            hash: Hash::from_u64(0x55),
                            context: Context::default(),
                        },
                    )
                    .await
                    .expect("modifying untouched.bin must succeed");
                staged4
                    .node_mark_staged(
                        repository.clone(),
                        untouched_id4,
                        NodeFlags::StagedModify,
                        NodeFlags::DirtyModify,
                    )
                    .await
                    .expect("marking the r4 modification must succeed");

                let r4 = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    staged4,
                    metadata_on(branch),
                    r3,
                    branch,
                )
                .await
                .expect("committing r4 must succeed");

                let tip4 = State::deserialize(repository.clone(), r4)
                    .await
                    .expect("r4 must deserialize");
                let touched4 = node_id_for(&tip4, repository.clone(), "a/touched.bin").await;
                let touched_meta4 = file_metadata_of(&tip4, repository.clone(), touched4).await;

                assert_eq!(
                    touched_meta4.revision, r3,
                    "at r4, touched.bin must still resolve to its last change (r3)"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// Attribution follows the walker across link boundaries. `TreeAttribution` is
    /// built per `enumerate_children`, so each side of the boundary attributes
    /// against its own state. The linked-subtree entries report the linked
    /// repository's revisions, not the walked repository's.
    #[tokio::test]
    async fn tree_attributes_across_a_link() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;

                // The link target: its own repository, sharing this one's stores
                // and write token, holding a single file at its root.
                let target_id = Context::from(uuid::Uuid::now_v7()).into();
                let target = repository.to_link_context(target_id).await;
                let target_branch = branch_id();
                let target_staged = State::new();
                add(
                    &target_staged,
                    target.clone(),
                    ROOT_NODE,
                    file("inner.bin", 0x77),
                    "inner.bin",
                )
                .await;
                let target_revision = commit_in_memory_revision(
                    target.clone(),
                    &token(),
                    target_staged,
                    metadata_on(target_branch),
                    Hash::default(),
                    target_branch,
                )
                .await
                .expect("committing the link target must succeed");

                // The walked repository: one ordinary file, plus a link at the
                // root pointing into the target's root.
                let branch = branch_id();
                let staged = State::new();
                add(
                    &staged,
                    repository.clone(),
                    ROOT_NODE,
                    file("top.bin", 0x88),
                    "top.bin",
                )
                .await;
                let link = Node {
                    flags: NodeFlags::Link.bits(),
                    mode: 0o755,
                    name_hash: hash_string("vendor"),
                    child: ROOT_NODE,
                    address: Address {
                        hash: target_revision,
                        context: target_id.into(),
                    },
                    ..Default::default()
                };
                add(&staged, repository.clone(), ROOT_NODE, link, "vendor").await;
                let revision = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    staged,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("committing the linking revision must succeed");

                let paths = tree(
                    repository.clone(),
                    revision,
                    RelativePath::default(),
                    0,
                    allow_all_repositories(),
                    true,
                )
                .await
                .expect("the tree walk must succeed")
                .paths;

                let entry_for = |name: &str| {
                    paths
                        .iter()
                        .find(|entry| entry.path.as_str() == name)
                        .unwrap_or_else(|| {
                            panic!(
                                "{name} must appear in the walk, got {:?}",
                                paths.iter().map(|e| e.path.as_str()).collect::<Vec<_>>()
                            )
                        })
                };

                let top = entry_for("top.bin");
                let vendor = entry_for("vendor");
                let inner = entry_for("vendor/inner.bin");

                assert_eq!(
                    top.last_revision, revision,
                    "an ordinary entry is attributed against the walked revision"
                );
                assert_eq!(
                    top.last_revision_repository, repository.id,
                    "top-level attribution names the walked repository"
                );

                assert_eq!(
                    vendor.last_revision, revision,
                    "the link node itself lives in the walked state, so it is attributed \
                     against the walked revision"
                );
                assert_eq!(
                    vendor.last_revision_repository, repository.id,
                    "the link node's attribution names the walked repository"
                );

                // The linked subtree's entries live in the linked state. The
                // walker crosses the boundary, so we get real attribution:
                // the linked repository's revision, in the linked repository.
                assert_eq!(
                    inner.last_revision, target_revision,
                    "content behind a link is attributed against the linked state's revision"
                );
                assert_eq!(
                    inner.last_revision_repository, target_id,
                    "linked-subtree attribution names the linked repository, not the walked one"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// `revision::tree` applies the rules above. Entries changed at the walked
    /// revision report it, the rest report their own last change (off by default).
    #[tokio::test]
    async fn tree_attributes_with_last_revision() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let branch = branch_id();

                // r1 adds both files, r2 modifies only touched.bin
                let staged = State::new();
                let dir = add(&staged, repository.clone(), ROOT_NODE, directory("a"), "a").await;
                add(
                    &staged,
                    repository.clone(),
                    dir,
                    file("touched.bin", 0x11),
                    "touched.bin",
                )
                .await;
                add(
                    &staged,
                    repository.clone(),
                    dir,
                    file("untouched.bin", 0x22),
                    "untouched.bin",
                )
                .await;
                let r1 = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    staged,
                    metadata_on(branch),
                    Hash::default(),
                    branch,
                )
                .await
                .expect("committing r1 must succeed");

                let staged2 = State::deserialize(repository.clone(), r1)
                    .await
                    .expect("r1 must deserialize");
                let touched_id = node_id_for(&staged2, repository.clone(), "a/touched.bin").await;
                staged2
                    .node_modify(
                        repository.clone(),
                        touched_id,
                        0o644,
                        4096,
                        Address {
                            hash: Hash::from_u64(0x33),
                            context: Context::default(),
                        },
                    )
                    .await
                    .expect("modifying the file must succeed");
                staged2
                    .node_mark_staged(
                        repository.clone(),
                        touched_id,
                        NodeFlags::StagedModify,
                        NodeFlags::DirtyModify,
                    )
                    .await
                    .expect("marking the modification must succeed");
                let r2 = commit_in_memory_revision(
                    repository.clone(),
                    &token(),
                    staged2,
                    metadata_on(branch),
                    r1,
                    branch,
                )
                .await
                .expect("committing r2 must succeed");

                let walk_depth = async |max_depth, include_last_revision| {
                    tree(
                        repository.clone(),
                        r2,
                        RelativePath::default(),
                        max_depth,
                        allow_all_repositories(),
                        include_last_revision,
                    )
                    .await
                    .expect("the tree walk must succeed")
                    .paths
                };
                // 0 = unbounded
                let walk = async |include_last_revision| walk_depth(0, include_last_revision).await;

                // Off: nothing attributed. `last_revision_repository` also stays
                // zero, since a zero revision has no meaningful repository.
                for entry in walk(false).await {
                    assert!(
                        entry.last_revision.is_zero(),
                        "{} must be unattributed when the walk is not asked",
                        entry.path
                    );
                    assert!(
                        entry.last_revision_repository.is_zero(),
                        "{} must carry a zero repository when unattributed",
                        entry.path
                    );
                }

                let attributed = walk(true).await;
                let entry_for = |name: &str| {
                    attributed
                        .iter()
                        .find(|entry| entry.path.as_str() == name)
                        .unwrap_or_else(|| panic!("{name} must appear in the walk"))
                };
                let of = |name: &str| entry_for(name).last_revision;
                let repo_of = |name: &str| entry_for(name).last_revision_repository;

                assert_eq!(of("a/touched.bin"), r2, "changed at the walked revision");
                assert_eq!(of("a/untouched.bin"), r1, "unchanged since r1");
                assert_eq!(of("a"), r2, "a directory follows its changed descendant");

                // Attribution stays within the walked repository: no links here,
                // so every entry names the same repository.
                assert_eq!(repo_of("a/touched.bin"), repository.id);
                assert_eq!(repo_of("a/untouched.bin"), repository.id);
                assert_eq!(repo_of("a"), repository.id);

                // Depth 1 is what a single-level directory listing uses, so it is
                // worth asserting directly rather than inferring from the
                // unbounded walk: only `a` is reached, and it is still attributed.
                let shallow = walk_depth(1, true).await;
                let names: Vec<&str> = shallow.iter().map(|e| e.path.as_str()).collect();
                assert_eq!(names, ["a"], "depth 1 from root reaches only the directory");
                assert_eq!(
                    shallow[0].last_revision, r2,
                    "attribution applies at depth 1, not only on an unbounded walk"
                );
                assert_eq!(
                    shallow[0].last_revision_repository, repository.id,
                    "attribution carries the repository at depth 1 too"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// Identity every merge-fixture operation runs as. Without one the commit
    /// path never writes `created-by` or `committed-by`.
    const MERGE_OPERATOR: &str = "operator@example.com";

    /// `merge_start` reads the remote unless `globals.offline` is set.
    async fn offline_execution() -> Arc<lore_revision::interface::ExecutionContext> {
        let _ = test_store_create().await.expect("Failed to create stores");
        let execution = Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ));
        execution.set_user_id(MERGE_OPERATOR).await;
        execution
    }

    /// A filesystem-backed repository that drives the real commit/branch/merge
    /// primitives, so a walk sees what a client would see. In-memory helpers
    /// above cannot produce a merge revision because
    /// `commit_in_memory_revision` hard-sets `parent_other = default`.
    struct MergeFixture {
        repository: Arc<RepositoryContext>,
        write_token: RepositoryWriteToken,
        repo_path: PathBuf,
        _tempdir: TempDir,
    }

    impl MergeFixture {
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
                _tempdir: tempdir,
            }
        }

        fn write_file(&self, relative: &str, content: &[u8]) {
            let absolute = self.repo_path.join(relative);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create parent dir");
            }
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(absolute.as_path())
                .expect("Failed to open file");
            file.write_all(content).expect("Failed to write file");
        }

        fn delete_file(&self, relative: &str) {
            let _ = std::fs::remove_file(self.repo_path.join(relative));
        }

        async fn stage_all(&self) {
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
        }

        async fn commit(&self, message: &str) -> Hash {
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

        async fn stage_and_commit(&self, message: &str) -> Hash {
            self.stage_all().await;
            self.commit(message).await
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

        async fn merge(&self, branch_id: BranchId, message: &str) -> Hash {
            Box::pin(branch::merge::merge_start(
                self.repository.clone(),
                &self.write_token,
                branch_id,
                branch::merge::MergeStartOptions {
                    message: message.to_string(),
                    link_messages: std::collections::HashMap::new(),
                    no_commit: false,
                    scope: branch::merge::MergeScope::MainOnly,
                    inherit_metadata: MetadataInherit::default(),
                },
            ))
            .await
            .expect("merge_start failed")
        }
    }

    /// Build the `ahead`/`side` merge fixture: main → ahead (branched, commits
    /// `ahead-feature.txt`) → side (branched off ahead, commits `side-work.txt`);
    /// ahead diverges with `ahead-second.txt`; side is merged back into ahead.
    /// Returns the revisions the tests need to name in assertions.
    struct AheadSideFixture {
        fixture: MergeFixture,
        ahead1: Hash,
        ahead2: Hash,
        merge_rev: Hash,
        main_rev: Hash,
    }

    async fn build_ahead_side_fixture() -> AheadSideFixture {
        let fixture = MergeFixture::new().await;

        // main: main-advance.txt
        fixture.write_file("main-advance.txt", b"zero\n");
        let main_rev = fixture
            .stage_and_commit("Add main-advance.txt on main")
            .await;

        // ahead: branch off main; commit ahead-feature.txt.
        let ahead_branch = fixture.create_branch("ahead").await;
        fixture.write_file("ahead-feature.txt", b"one\n");
        let ahead1 = fixture
            .stage_and_commit("Add ahead-feature.txt on ahead")
            .await;

        // side: branch off ahead; commit side-work.txt.
        let side_branch = fixture.create_branch("side").await;
        fixture.write_file("side-work.txt", b"two\n");
        fixture.stage_and_commit("Add side-work.txt on side").await;

        // Switch anchor back to ahead@ahead1. Working tree still has
        // side-work.txt from the side commit; delete it so the ahead commit
        // that follows does not re-add it.
        fixture.switch_to(ahead_branch, ahead1).await;
        fixture.delete_file("side-work.txt");

        // ahead: commit ahead-second.txt so ahead diverges from side.
        fixture.write_file("ahead-second.txt", b"three\n");
        let ahead2 = fixture
            .stage_and_commit("Add ahead-second.txt on ahead")
            .await;

        // Merge side into ahead.
        let merge_rev = fixture.merge(side_branch, "merge side into ahead").await;

        AheadSideFixture {
            fixture,
            ahead1,
            ahead2,
            merge_rev,
            main_rev,
        }
    }

    /// Walk a revision's tree with attribution, indexed by path.
    async fn walk_by_path(
        repository: Arc<RepositoryContext>,
        revision: Hash,
    ) -> std::collections::HashMap<String, lore_revision::state::TreePath> {
        tree(
            repository,
            revision,
            RelativePath::default(),
            0,
            allow_all_repositories(),
            true,
        )
        .await
        .expect("the tree walk must succeed")
        .paths
        .into_iter()
        .map(|entry| (entry.path.as_str().to_string(), entry))
        .collect()
    }

    /// An entry a merge brought in from the other parent attributes to the
    /// merge, as `lore file history` reports it. The merge stands for what it
    /// merged in.
    #[tokio::test]
    async fn tree_attributes_a_merged_in_entry_to_the_merge() {
        let execution = offline_execution().await;
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let f = build_ahead_side_fixture().await;
                let entries = walk_by_path(f.fixture.repository.clone(), f.merge_rev).await;

                let side_work = entries
                    .get("side-work.txt")
                    .expect("side-work.txt must appear in the walk");

                assert_eq!(
                    side_work.last_revision, f.merge_rev,
                    "side-work.txt arrived with the merge"
                );
                assert_eq!(
                    side_work.last_revision_repository, f.fixture.repository.id,
                    "attribution stays within the walked repository (no links here)"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// Entries the merge did not change keep the first-parent revision that last
    /// changed them. Only the merge's own changes attribute to the merge.
    #[tokio::test]
    async fn tree_attributes_first_parent_entries_across_a_merge() {
        let execution = offline_execution().await;
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let f = build_ahead_side_fixture().await;
                let entries = walk_by_path(f.fixture.repository.clone(), f.merge_rev).await;

                let attribution_of = |path: &str| -> Hash {
                    entries
                        .get(path)
                        .unwrap_or_else(|| panic!("{path} must appear in the walk"))
                        .last_revision
                };

                assert_eq!(
                    attribution_of("main-advance.txt"),
                    f.main_rev,
                    "main-advance.txt must attribute to the main-side revision that added it"
                );
                assert_eq!(
                    attribution_of("ahead-feature.txt"),
                    f.ahead1,
                    "ahead-feature.txt must attribute to the ahead revision that added it"
                );
                assert_eq!(
                    attribution_of("ahead-second.txt"),
                    f.ahead2,
                    "ahead-second.txt must attribute to the ahead revision that added it, \
                     not to the merge revision"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// An entry both sides changed, which the merge reconciles, attributes to
    /// the merge. The merge made that content.
    #[tokio::test]
    async fn tree_attributes_a_reconciled_entry_to_the_merge() {
        let execution = offline_execution().await;
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = MergeFixture::new().await;

                // base: common.txt on main.
                fixture.write_file("common.txt", b"base line\n");
                let _base = fixture.stage_and_commit("Add common.txt on main").await;

                // ahead: branch off main; modify common.txt on ahead.
                let ahead_branch = fixture.create_branch("ahead").await;
                fixture.write_file("common.txt", b"base line\nahead line\n");
                let ahead1 = fixture.stage_and_commit("Extend common.txt on ahead").await;

                // side: branch off ahead's current tip; modify common.txt
                // differently.
                let side_branch = fixture.create_branch("side").await;
                fixture.write_file("common.txt", b"base line\nahead line\nside line\n");
                let _side1 = fixture
                    .stage_and_commit("Extend common.txt further on side")
                    .await;

                // ahead changes common.txt again, on another line, so both sides
                // have changed it since side branched.
                fixture.switch_to(ahead_branch, ahead1).await;
                fixture.write_file("common.txt", b"ahead first line\nbase line\nahead line\n");
                fixture.stage_and_commit("Prefix common.txt on ahead").await;

                let merge_rev = fixture.merge(side_branch, "merge side into ahead").await;

                let entries = walk_by_path(fixture.repository.clone(), merge_rev).await;
                let common = entries
                    .get("common.txt")
                    .expect("common.txt must appear in the walk");

                let merged = std::fs::read_to_string(fixture.repo_path.join("common.txt"))
                    .expect("common.txt must read back");
                assert!(
                    merged.contains("ahead first line") && merged.contains("side line"),
                    "the case needs the merge to combine both sides, got {merged:?}"
                );
                assert_eq!(
                    common.last_revision, merge_rev,
                    "the merge combined both sides' changes"
                );
            }))
            .await
            .expect("Task failed");
    }

    /// An entry modified only on the merged-in side attributes to the merge, and
    /// so does its directory. A later revision that leaves them alone keeps that
    /// answer, so attribution does not shift as history moves on.
    #[tokio::test]
    async fn tree_attributes_an_entry_modified_on_the_merged_in_side() {
        let execution = offline_execution().await;
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = MergeFixture::new().await;

                fixture.write_file("src/shared.txt", b"base\n");
                let base = fixture.stage_and_commit("Add src/shared.txt").await;

                let target_branch = fixture.create_branch("target").await;
                let side_branch = fixture.create_branch("side").await;
                fixture.write_file("src/shared.txt", b"changed on side\n");
                fixture
                    .stage_and_commit("Change src/shared.txt on side")
                    .await;

                // The target diverges, so the merge cannot fast-forward.
                fixture.switch_to(target_branch, base).await;
                fixture.write_file("src/shared.txt", b"base\n");
                fixture.write_file("target.txt", b"target\n");
                let target1 = fixture.stage_and_commit("Add target.txt on target").await;

                let merge_rev = fixture.merge(side_branch, "merge side into target").await;
                fixture.write_file("later.txt", b"later\n");
                let later = fixture
                    .stage_and_commit("Add later.txt after the merge")
                    .await;

                for walked in [merge_rev, later] {
                    let entries = walk_by_path(fixture.repository.clone(), walked).await;
                    let of = |name: &str| {
                        entries
                            .get(name)
                            .unwrap_or_else(|| panic!("{name} must appear in the walk"))
                            .last_revision
                    };
                    assert_eq!(of("src/shared.txt"), merge_rev, "it arrived with the merge");
                    assert_eq!(of("src"), merge_rev, "a directory follows its descendant");
                    assert_eq!(of("target.txt"), target1, "the target's own change");
                }
            }))
            .await
            .expect("Task failed");
    }

    /// A sync of divergent history builds its merge on the local tree, with the
    /// remote latest as first parent and the local latest as second. The local
    /// and remote sides each add a different file at the same node ID, which is
    /// where weaving by node ID would credit the local file to the remote.
    #[tokio::test]
    async fn tree_attributes_a_divergent_sync_merge() {
        let (_immutable, mutable, execution) =
            test_store_create().await.expect("Failed to create stores");
        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let repository = test_repository(mutable).await;
                let main = branch_id();
                let remote = branch_id();

                async fn modify(
                    state: &State,
                    repository: Arc<RepositoryContext>,
                    node_id: NodeID,
                    address: Address,
                    staged: NodeFlags,
                ) {
                    state
                        .node_modify(repository.clone(), node_id, 0o644, 10, address)
                        .await
                        .expect("modifying the file must succeed");
                    state
                        .node_mark_staged(repository, node_id, staged, NodeFlags::DirtyModify)
                        .await
                        .expect("marking the modification must succeed");
                }
                let commit_on = async |staged: Arc<State>, parent: Hash, branch: BranchId| {
                    commit_in_memory_revision(
                        repository.clone(),
                        &token(),
                        staged,
                        metadata_on(branch),
                        parent,
                        branch,
                    )
                    .await
                    .expect("committing must succeed")
                };
                let load = async |revision: Hash| {
                    State::deserialize(repository.clone(), revision)
                        .await
                        .expect("the revision must deserialize")
                };

                let staged = State::new();
                add(
                    &staged,
                    repository.clone(),
                    ROOT_NODE,
                    file("base.txt", 0x11),
                    "base.txt",
                )
                .await;
                let early = file("early.txt", 0x22);
                let early_id =
                    add(&staged, repository.clone(), ROOT_NODE, early, "early.txt").await;
                // A change keeps the file ID, which is how the weave matches a counterpart.
                let early_changed = Address {
                    hash: Hash::from_u64(0x44),
                    context: early.address.context,
                };
                let base = commit_on(staged, Hash::default(), main).await;

                let staged = load(base).await;
                let local_id = add(
                    &staged,
                    repository.clone(),
                    ROOT_NODE,
                    file("local.txt", 0x33),
                    "local.txt",
                )
                .await;
                let local = commit_on(staged, base, main).await;

                // The remote changes early.txt, then adds remote.txt in its latest.
                let staged = load(base).await;
                modify(
                    &staged,
                    repository.clone(),
                    early_id,
                    early_changed,
                    NodeFlags::StagedModify,
                )
                .await;
                let remote_early = commit_on(staged, base, remote).await;
                let remote_file = file("remote.txt", 0x55);
                let staged = load(remote_early).await;
                let remote_id = add(
                    &staged,
                    repository.clone(),
                    ROOT_NODE,
                    remote_file,
                    "remote.txt",
                )
                .await;
                let remote_latest = commit_on(staged, remote_early, remote).await;
                assert_eq!(
                    remote_id, local_id,
                    "the case needs both sides to add at the same node ID"
                );

                // The merged tree is the local one plus the remote's changes. A change
                // the merge takes is staged as one, as a sync stages it.
                let staged = load(local).await;
                add(
                    &staged,
                    repository.clone(),
                    ROOT_NODE,
                    remote_file,
                    "remote.txt",
                )
                .await;
                modify(
                    &staged,
                    repository.clone(),
                    early_id,
                    early_changed,
                    NodeFlags::StagedModify | NodeFlags::StagedMerge,
                )
                .await;
                let merged = commit_on(staged, local, main).await;

                // Woven again as a sync stages it, as upstream's weave tests do.
                let sync = load(merged).await;
                sync.set_parent_self(remote_latest);
                sync.set_parent_other(local);
                commit::weave_history(repository.clone(), sync.clone(), commit::BasedOn::Other)
                    .await
                    .expect("weaving the merge history must succeed");
                let sync_merge = sync
                    .serialize(repository.clone(), &token())
                    .await
                    .expect("serializing the merge must succeed");

                let entries = walk_by_path(repository.clone(), sync_merge).await;
                let of = |name: &str| {
                    entries
                        .get(name)
                        .unwrap_or_else(|| panic!("{name} must appear in the walk"))
                        .last_revision
                };
                assert_eq!(of("base.txt"), base, "untouched on both sides");
                assert_eq!(
                    of("local.txt"),
                    local,
                    "the local add, not the remote's at its node ID"
                );
                // The merge applied the remote's changes onto the local tree.
                assert_eq!(
                    of("early.txt"),
                    sync_merge,
                    "a remote change before its latest"
                );
                assert_eq!(of("remote.txt"), sync_merge, "the remote add");
            }))
            .await
            .expect("Task failed");
    }
}
