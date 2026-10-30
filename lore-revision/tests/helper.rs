// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

#[allow(dead_code)]
pub async fn test_store_create() -> Result<
    (
        std::sync::Arc<dyn lore_storage::ImmutableStore>,
        std::sync::Arc<dyn lore_storage::MutableStore>,
        std::sync::Arc<lore_revision::interface::ExecutionContext>,
    ),
    lore_storage::StoreError,
> {
    let execution = setup_test_execution();
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let immutable = lore_storage::local::immutable_store::create(
                None::<&str>, /* No on disk path, in-memory only */
                lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
                false, /* Do not deserialize all buckets on start */
                lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
            )
            .await?;
            let mutable: std::sync::Arc<dyn lore_storage::MutableStore> =
                lore_storage::local::mutable_store::create(
                    None::<&str>, /* No on disk path, in-memory only */
                    lore_storage::MutableStoreSettings::default(),
                    immutable.clone(),
                )
                .await?;
            Ok((immutable, mutable, lore_revision::lore::execution_context()))
        })
        .await
}

#[allow(dead_code)]
pub fn default_repository_creation_args(
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
) -> lore_revision::repository::RepositoryContextCreationArgs {
    lore_revision::repository::RepositoryContextCreationArgs {
        paths: None,
        immutable_store,
        mutable_store,
        id: lore_base::types::Context::from(uuid::Uuid::now_v7()).into(),
        instance_id: lore_revision::instance::InstanceId::generate(),
        remote: Err(lore_transport::ProtocolError::from(
            lore_base::error::NoRemote,
        )),
        filter: std::sync::Arc::default(),
        filesystem_provider: None,
    }
}

#[allow(dead_code)]
pub trait RepositoryContextCreationArgsExt {
    fn with_path(self, path: impl AsRef<std::path::Path>) -> Self;
    fn with_id(self, id: lore_revision::lore::RepositoryId) -> Self;
    fn with_instance_id(self, id: lore_revision::instance::InstanceId) -> Self;
    fn with_remote(
        self,
        remote: Result<std::sync::Arc<lore_transport::Connection>, lore_transport::ProtocolError>,
    ) -> Self;
    fn with_filter(self, filter: std::sync::Arc<lore_revision::filter::Filter>) -> Self;
    fn with_filesystem_provider(
        self,
        filesystem_provider: std::sync::Arc<
            dyn lore_revision::fs::filesystem_provider::FilesystemProvider,
        >,
    ) -> Self;
}

impl RepositoryContextCreationArgsExt for lore_revision::repository::RepositoryContextCreationArgs {
    fn with_path(mut self, path: impl AsRef<std::path::Path>) -> Self {
        self.paths = Some(lore_revision::repository::RepositoryPaths::new(
            path.as_ref().to_path_buf(),
            path.as_ref().join(lore_revision::repository::DOT_LORE),
        ));
        self
    }

    fn with_id(mut self, id: lore_revision::lore::RepositoryId) -> Self {
        self.id = id;
        self
    }

    fn with_instance_id(mut self, id: lore_revision::instance::InstanceId) -> Self {
        self.instance_id = id;
        self
    }

    fn with_remote(
        mut self,
        remote: Result<std::sync::Arc<lore_transport::Connection>, lore_transport::ProtocolError>,
    ) -> Self {
        self.remote = remote;
        self
    }

    fn with_filter(mut self, filter: std::sync::Arc<lore_revision::filter::Filter>) -> Self {
        self.filter = filter;
        self
    }

    fn with_filesystem_provider(
        mut self,
        filesystem_provider: std::sync::Arc<
            dyn lore_revision::fs::filesystem_provider::FilesystemProvider,
        >,
    ) -> Self {
        self.filesystem_provider = Some(filesystem_provider);
        self
    }
}

#[allow(unused_imports)]
pub use lore_base::test_util::TempDir;

#[allow(dead_code)]
pub fn generate_tempdir() -> TempDir {
    TempDir::new("lore-stage-test-")
}

/// A repository on its own temporary directory, with its anchor branch stored
/// and its write token held.
///
/// The directory is removed when this is dropped, so it has to outlive every use
/// of `repository`.
#[allow(dead_code)]
pub struct TestRepository {
    pub repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    pub write_token: lore_revision::repository::RepositoryWriteToken,
    pub path: std::path::PathBuf,
    _tempdir: TempDir,
}

/// Creates a repository backed by `immutable_store` and `mutable_store` in a
/// fresh temporary directory, on a generated default branch.
///
/// Call from inside a `LORE_CONTEXT` scope; repository creation reads the
/// execution context.
#[allow(dead_code)]
pub async fn test_repository_create(
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    repository_id: lore_revision::lore::RepositoryId,
) -> TestRepository {
    let tempdir = generate_tempdir();
    let path = tempdir.to_path_buf();
    let default_branch_id = lore_base::types::Context::from(uuid::Uuid::now_v7());
    let write_token =
        lore_revision::repository::RepositoryWriteToken::acquire(path.as_path()).await;
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
                .with_instance_id(created.instance_id),
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

/// A context over `instance` whose view holds `globs` and whose ignore slot is empty, a leading
/// `!` re-including as it does in a view file.
///
/// Each call mints its own filter, so two contexts hold different ones whether or not their
/// rules agree -- which is what a diff reads as two views.
#[allow(dead_code)]
pub fn test_view_context(
    instance: &TestRepository,
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    globs: &[&str],
) -> std::sync::Arc<lore_revision::repository::RepositoryContext> {
    test_filter_context(instance, immutable_store, mutable_store, &[], globs)
}

/// [`test_view_context`] with `ignore` in the ignore slot, for a test that has to tell the two
/// slots apart. A repository opened for real holds rules in both.
#[allow(dead_code)]
pub fn test_filter_context(
    instance: &TestRepository,
    immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    ignore: &[&str],
    view: &[&str],
) -> std::sync::Arc<lore_revision::repository::RepositoryContext> {
    std::sync::Arc::new(lore_revision::repository::RepositoryContext::new(
        default_repository_creation_args(immutable_store, mutable_store)
            .with_path(&instance.path)
            .with_id(instance.repository.id)
            .with_instance_id(instance.repository.instance_id)
            .with_filter(std::sync::Arc::new(test_filter(ignore, view))),
    ))
}

/// A filter whose ignore slot holds `ignore` and whose view holds `view`, a leading `!`
/// re-including as it does in a filter file.
#[allow(dead_code)]
pub fn test_filter(ignore: &[&str], view: &[&str]) -> lore_revision::filter::Filter {
    let mut filter = lore_revision::filter::Filter::default();
    for (slot, globs) in [(&mut filter.ignore, ignore), (&mut filter.view, view)] {
        for glob in globs {
            match glob.strip_prefix('!') {
                Some(inclusion) => slot.add_inclusion(inclusion).expect("Filter inclusion"),
                None => slot.add_exclusion(glob).expect("Filter exclusion"),
            }
        }
    }
    filter
}

/// An OS filesystem that holds its state in a `State` as far as a command asks it
/// ([`lore_revision::fs::filesystem_provider::FilesystemProvider::holds_state`]), as its
/// operations do, while they read and write the files themselves. Counts the operations begun on
/// it.
#[allow(dead_code)]
pub struct HoldingFilesystem {
    pub inner: lore_revision::fs::os::OsFilesystem,
    pub begins: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl lore_revision::fs::filesystem_provider::FilesystemProvider for HoldingFilesystem {
    async fn begin_operation(
        &self,
    ) -> Result<
        std::sync::Arc<lore_revision::fs::filesystem_provider::InstanceOperationImpl>,
        lore_revision::fs::filesystem_provider::FsError,
    > {
        self.begins
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(std::sync::Arc::new(
            lore_revision::fs::filesystem_provider::InstanceOperationImpl::new(
                lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation::Os(
                    self.inner.begin_operation(),
                ),
                self.holds_state(),
            ),
        ))
    }

    fn holds_state(&self) -> bool {
        true
    }
}

/// `repository`, with its stores, filter and write token, reading its working tree through the
/// [`HoldingFilesystem`] answered beside it.
#[allow(dead_code)]
pub fn test_holding_context(
    repository: &lore_revision::repository::RepositoryContext,
) -> (
    std::sync::Arc<lore_revision::repository::RepositoryContext>,
    std::sync::Arc<HoldingFilesystem>,
) {
    let path = repository.require_path().expect("A repository on disk");
    let filesystem = std::sync::Arc::new(HoldingFilesystem {
        inner: lore_revision::fs::os::OsFilesystem::new(path),
        begins: std::sync::atomic::AtomicUsize::new(0),
    });
    let context = std::sync::Arc::new(
        lore_revision::repository::RepositoryContext::new(
            default_repository_creation_args(
                repository.immutable_store(),
                repository.mutable_store.clone(),
            )
            .with_path(path)
            .with_id(repository.id)
            .with_instance_id(repository.instance_id)
            .with_filter(repository.filter.clone())
            .with_filesystem_provider(filesystem.clone()),
        )
        .with_write_token(
            repository
                .try_write_token()
                .expect("A repository opened for writing")
                .share(),
        ),
    );
    (context, filesystem)
}

/// Stages the whole working tree of `instance` and commits it, answering the state of the
/// revision that produced.
///
/// Scans rather than reading dirty flags, so a fixture that wrote its files directly is staged
/// whole.
#[allow(dead_code)]
pub async fn test_commit_tree(
    instance: &TestRepository,
    message: &str,
) -> std::sync::Arc<lore_revision::state::State> {
    lore_revision::file::stage::stage(
        instance.repository.clone(),
        &instance.write_token,
        lore_revision::interface::LoreArray::from_vec(vec![
            lore_revision::interface::LoreString::from(&instance.path),
        ]),
        lore_revision::stage::StageOptions {
            scan: true,
            ..Default::default()
        },
    )
    .await
    .expect("Failed to stage the fixture");
    test_commit(instance, message).await
}

/// Commits what `instance` holds staged, answering the state of the revision that produced.
///
/// For a fixture that staged something narrower than the whole tree, such as a move.
#[allow(dead_code)]
pub async fn test_commit(
    instance: &TestRepository,
    message: &str,
) -> std::sync::Arc<lore_revision::state::State> {
    lore_revision::commit::commit_boxed(
        instance.repository.clone(),
        &instance.write_token,
        lore_revision::commit::CommitOptions::new(message.to_string()),
    )
    .await
    .expect("Commit failed");
    let (revision, _branch) =
        lore_revision::instance::load_current_anchor_boxed(&instance.repository)
            .await
            .expect("Failed to load current anchor");
    lore_revision::state::State::deserialize(instance.repository.clone(), revision)
        .await
        .expect("Failed to deserialize the committed state")
}

/// Creates (or truncates) the file at `path` and writes `contents` to it.
///
/// Panics on failure, since a fixture the test cannot write invalidates what it
/// would go on to assert.
#[allow(dead_code)]
pub fn test_file_write(path: &std::path::Path, contents: &[u8]) {
    // Test fixture writes; not subject to repository write-token discipline.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(path, contents)
        .unwrap_or_else(|_| panic!("Failed to write test file at {}", path.display()));
}

/// Reconciles the working tree against `state_staged`, mutating it in place as
/// `lore status --scan` does, and returns the changes detected.
#[allow(dead_code)]
pub async fn test_scan(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
) -> Vec<lore_revision::change::NodeChange> {
    test_scan_with_intent(
        repository,
        state_staged,
        state_current,
        lore_revision::fs::filesystem_provider::FilesystemDiffIntent::MarkDirty,
    )
    .await
}

/// [`test_scan`] under the given intent, for a walk that stages rather than marks.
#[allow(dead_code)]
pub async fn test_scan_with_intent(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
    intent: lore_revision::fs::filesystem_provider::FilesystemDiffIntent,
) -> Vec<lore_revision::change::NodeChange> {
    test_scan_path_with_intent(repository, state_staged, state_current, None, intent).await
}

/// [`test_scan_with_intent`] scoped to `path`, which is what makes the walk resolve the
/// ancestors of a path the tree does not hold.
#[allow(dead_code)]
pub async fn test_scan_path_with_intent(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
    path: Option<lore_revision::util::path::RelativePath>,
    intent: lore_revision::fs::filesystem_provider::FilesystemDiffIntent,
) -> Vec<lore_revision::change::NodeChange> {
    let operation = lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
        repository.file_system().as_ref(),
    )
    .await
    .expect("Failed to start filesystem operation");
    let changes = lore_revision::state::diff_filesystem(
        &operation,
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository: repository.clone(),
            state: state_staged,
        },
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository,
            state: state_current,
        },
        path,
        lore_revision::filter::FilterMode::Full,
        intent,
        std::sync::Arc::new(Vec::new()),
    )
    .await
    .expect("Failed to diff filesystem")
    .collect()
    .await
    .expect("Failed to diff filesystem");
    operation
        .finalize()
        .await
        .expect("Failed to finish filesystem operation");
    changes
}

/// [`test_scan`] read one change at a time, for the walk's other consumption path.
///
/// Answers in arrival order, which interleaves subtrees by completion, and waits for the walk so
/// the answer is the whole of it.
#[allow(dead_code)]
pub async fn test_scan_streaming(
    repository: std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state_staged: std::sync::Arc<lore_revision::state::State>,
    state_current: std::sync::Arc<lore_revision::state::State>,
) -> Vec<lore_revision::change::NodeChange> {
    let operation = lore_revision::fs::filesystem_provider::FilesystemProvider::begin_operation(
        repository.file_system().as_ref(),
    )
    .await
    .expect("Failed to start filesystem operation");
    let mut stream = lore_revision::state::diff_filesystem(
        &operation,
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository: repository.clone(),
            state: state_staged,
        },
        lore_revision::fs::filesystem_provider::FilesystemDiffTree {
            repository,
            state: state_current,
        },
        None,
        lore_revision::filter::FilterMode::Full,
        lore_revision::fs::filesystem_provider::FilesystemDiffIntent::Report,
        std::sync::Arc::new(Vec::new()),
    )
    .await
    .expect("Failed to diff filesystem");

    let mut changes = Vec::new();
    while let Some(change) = stream.next().await {
        changes.push(change);
    }
    stream.finish().await.expect("Failed to diff filesystem");
    operation
        .finalize()
        .await
        .expect("Failed to finish filesystem operation");
    changes
}

/// The anchor revision's state, deserialized twice: once as the current
/// revision and once as the staged state a scan reconciles in place.
#[allow(dead_code)]
pub async fn test_anchor_states(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
) -> (
    std::sync::Arc<lore_revision::state::State>,
    std::sync::Arc<lore_revision::state::State>,
) {
    let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(repository)
        .await
        .expect("Failed to load current anchor");
    let current = lore_revision::state::State::deserialize(repository.clone(), revision)
        .await
        .expect("Failed to deserialize current state");
    let staged = lore_revision::state::State::deserialize(repository.clone(), revision)
        .await
        .expect("Failed to deserialize staged state");
    (current, staged)
}

pub fn setup_test_execution() -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    )
}

/// The action letter against the path for each change, as a walk's consumers read it.
///
/// The action is carried because two sides route a path by which of them admits it, and a path
/// alone does not say which route it took.
#[allow(dead_code)]
pub fn test_reported(changes: &[lore_revision::change::NodeChange]) -> Vec<(String, String)> {
    changes
        .iter()
        .map(|change| {
            (
                change.action.as_string_short().to_string(),
                change.path().as_str().to_string(),
            )
        })
        .collect()
}

/// The children of `parent` in `state` named `name`.
#[allow(dead_code)]
pub async fn children_named(
    repository: &std::sync::Arc<lore_revision::repository::RepositoryContext>,
    state: &std::sync::Arc<lore_revision::state::State>,
    parent: lore_revision::node::NodeID,
    name: &str,
) -> Vec<lore_revision::node::NodeID> {
    let name_hash = lore_storage::hash::hash_string(name);
    let mut named = Vec::new();
    for child in state
        .node_children(repository.clone(), parent)
        .await
        .expect("Failed to list the children")
    {
        let node = state
            .node(repository.clone(), child)
            .await
            .expect("Failed to read a child");
        if node.name_hash == name_hash {
            named.push(child);
        }
    }
    named
}

/// Fixtures for revisions built in memory, on a path-less repository writing through an in-memory
/// token, which is the shape the memory-based revision tree handle runs on.
#[allow(dead_code)]
pub mod in_memory {
    use std::sync::Arc;

    use lore_base::types::Address;
    use lore_base::types::BranchId;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_revision::commit::commit_in_memory_revision;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::Node;
    use lore_revision::node::NodeFlags;
    use lore_revision::node::NodeID;
    use lore_revision::repository::InMemoryContext;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::state::State;
    use lore_storage::hash::hash_string;

    struct Marker;
    impl InMemoryContext for Marker {}
    const MARKER: Marker = Marker;

    pub fn token() -> RepositoryWriteToken {
        RepositoryWriteToken::in_memory(&MARKER)
    }

    /// A path-less repository on the stores, writing through [`token`].
    pub fn repository(
        immutable: Arc<dyn lore_storage::ImmutableStore>,
        mutable: Arc<dyn lore_storage::MutableStore>,
    ) -> Arc<RepositoryContext> {
        Arc::new(
            RepositoryContext::new(super::default_repository_creation_args(immutable, mutable))
                .with_write_token(token()),
        )
    }

    pub fn file(name: &str) -> Node {
        Node {
            flags: NodeFlags::File.bits(),
            mode: 0o644,
            size: 10,
            address: Address {
                hash: Hash::from_u64(0x5a),
                context: Context::from(uuid::Uuid::now_v7()),
            },
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    pub fn directory(name: &str) -> Node {
        Node {
            flags: NodeFlags::NoFlags.bits(),
            mode: 0o755,
            name_hash: hash_string(name),
            ..Default::default()
        }
    }

    pub fn branch_id() -> BranchId {
        Context::from(uuid::Uuid::now_v7())
    }

    /// Adds `node` as `name` under `parent`, staged as an add. Every write verb marks its staged
    /// action and dirties the ancestors, which is what the freeze walks; `node_add` alone leaves
    /// neither.
    pub async fn add(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
    ) -> NodeID {
        add_staged(state, repository, parent, node, name, NodeFlags::StagedAdd).await
    }

    /// Adds `node` as `name` under `parent`, staged with `staged`, such as an add a merge takes
    /// from its other side.
    pub async fn add_staged(
        state: &State,
        repository: Arc<RepositoryContext>,
        parent: NodeID,
        node: Node,
        name: &str,
        staged: NodeFlags,
    ) -> NodeID {
        let node_id = state
            .node_add(repository.clone(), parent, node, name)
            .await
            .expect("adding the node must succeed");
        state
            .node_mark_staged(repository, node_id, staged, NodeFlags::DirtyAdd)
            .await
            .expect("marking the addition must succeed");
        node_id
    }

    pub fn metadata_on(branch: BranchId) -> Metadata {
        let mut metadata = Metadata::new();
        metadata
            .set_branch(branch)
            .expect("setting the branch must succeed");
        metadata
    }

    /// Commits `state` on top of `current` on `branch`.
    pub async fn commit(
        repository: &Arc<RepositoryContext>,
        state: &Arc<State>,
        current: Hash,
        branch: BranchId,
    ) -> Hash {
        commit_in_memory_revision(
            repository.clone(),
            &token(),
            state.clone(),
            metadata_on(branch),
            current,
            branch,
        )
        .await
        .expect("committing the revision must succeed")
    }

    pub async fn revision_state(repository: &Arc<RepositoryContext>, revision: Hash) -> Arc<State> {
        State::deserialize(repository.clone(), revision)
            .await
            .expect("deserializing the revision must succeed")
    }
}
