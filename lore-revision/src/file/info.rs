// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use zerocopy::FromZeros;

use crate::MAX_CONCURRENT_TREE_TASKS;
use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::filter::FilterMode;
use crate::filter::FilterStates;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::immutable;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::lore::Context;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::metadata::Metadata;
use crate::node;
use crate::node::Node;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::repository::DOT_LORE;
use crate::repository::DOT_URC;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::state;
use crate::state::NodeComparison;
use crate::state::State;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

#[error_set]
pub enum InfoError {
    InvalidArguments,
    InvalidPath,
    RevisionNotFound,
    FileNotFound,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    LinkNotFound,
    Maintenance,
    NodeNotFound,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    SlowDown,
    WriteRequired,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for InfoError {
    fn translated(&self) -> LoreError {
        match self {
            InfoError::InvalidArguments(_) | InfoError::InvalidPath(_) => {
                LoreError::InvalidArguments
            }
            InfoError::RevisionNotFound(_) | InfoError::NotFound(_) => LoreError::NotFound,
            InfoError::FileNotFound(_) => LoreError::FileNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Data for the event reporting information about a single file or directory.
#[repr(C)]
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileInfoEventData {
    /// Path of the file or directory.
    pub path: LoreString,
    /// Context identifying the file or directory.
    pub context: Context,
    /// Content hash of the file or directory.
    pub hash: Hash,
    /// Set when the entry is a file.
    #[serde(with = "u8_as_bool")]
    pub is_file: u8,
    /// Set when the entry is a directory.
    #[serde(with = "u8_as_bool")]
    pub is_dir: u8,
    /// Set when the entry has been modified.
    #[serde(with = "u8_as_bool")]
    pub flag_modified: u8,
    /// Set when the entry has been deleted.
    #[serde(with = "u8_as_bool")]
    pub flag_deleted: u8,
    /// Set when the entry has been added.
    #[serde(with = "u8_as_bool")]
    pub flag_added: u8,
    /// Set when the entry is in conflict.
    #[serde(with = "u8_as_bool")]
    pub flag_conflict: u8,
    /// File mode bits.
    pub mode: u16,
    /// Size of the entry in the repository, in bytes.
    pub size: u64,
    /// Size of the entry on the local filesystem, in bytes.
    pub local_size: u64,
    /// Address the entry's local content hashes to, zero where nothing was compared.
    pub local_hash: Hash,
    /// Size of the entry after filters are applied, in bytes.
    pub filter_size: u64,
}

#[derive(Clone, Debug)]
pub struct InfoOptions {
    /// Optional revision specifier
    pub revision: Option<String>,
    /// Calculate the filtered local filesystem hash and size
    pub local: bool,
    /// Calculate the filtered repository size
    pub filtered: bool,
}

pub async fn info(
    repository: Arc<RepositoryContext>,
    paths: Vec<RelativePath>,
    options: InfoOptions,
) -> Result<(), InfoError> {
    let signature = if let Some(revision_spec) = options.revision {
        revision::resolve(
            repository.clone(),
            revision_spec.as_str(),
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|_err| {
            InfoError::from(RevisionNotFound {
                revision: revision_spec,
            })
        })?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<InfoError>("Failed deserializing revision state")?;
        current_revision
    };

    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<InfoError>("Failed deserializing revision state")?;

    with_operation(repository.file_system(), async |operation| {
        let mut tasks = JoinSet::new();
        for path in paths.iter() {
            lore_debug!("Info path: {path}");

            let operation = operation.clone();
            let repository = repository.clone();
            let state = state.clone();
            let path = path.clone();

            lore_spawn!(tasks, async move {
                info_path(
                    operation,
                    repository,
                    state,
                    path,
                    options.local,
                    options.filtered,
                )
                .await
            });
        }

        let mut failure: Option<InfoError> = None;
        while let Some(result) = tasks.join_next().await {
            let inner = result
                .internal("Internal task failure")
                .map_err(InfoError::from)
                .flatten();
            failure = failure.or(inner.err());
        }

        if let Some(err) = failure {
            return Err(err);
        }

        Ok(())
    })
    .await
}

async fn info_path(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    state_current: Arc<State>,
    path: RelativePath,
    local: bool,
    filtered: bool,
) -> Result<(), InfoError> {
    if let Ok(node_link) = state_current
        .find_node_link(repository.clone(), path.as_str())
        .await
    {
        // TODO(vri): UCS-19229 - Links: Handle link nodes in file info lookup
        let node_path = state_current
            .node_path(repository.clone(), node_link.node)
            .await
            .map_err(|_err| {
                InfoError::from(FileNotFound {
                    resource: path.to_string(),
                })
            })?;

        let node = state_current
            .node(repository.clone(), node_link.node)
            .await
            .map_err(|_err| {
                InfoError::from(FileNotFound {
                    resource: path.to_string(),
                })
            })?;

        let address_context = if node.is_file() {
            node.address.context
        } else {
            Context::new_zeroed()
        };

        if filtered {
            // We will be iterating the tree, cache the state fragments
            let _ = state_current.cache_fragments(repository.clone()).await;
        }

        lore_debug!("Info local {local} and filtered {filtered}");
        let mut local_filtered = if local || filtered {
            // Calculate the local size and hash
            calculate_local_filtered_size_hash(
                operation.clone(),
                repository.clone(),
                path.clone(),
                state_current.clone(),
                node,
                node_link.node,
                RequestedSizes { local, filtered },
            )
            .await?
        } else {
            LocalFiltered::default()
        };

        let mut is_deleted = false;
        if local_filtered.local_size == 0 {
            match operation.file_info(&path).await {
                Ok(info) if info.is_file() => {
                    let (comparison, local_hash) =
                        compare_to_node(&operation, &repository, &node, info.size(), &path).await?;
                    local_filtered.local_size = info.size();
                    local_filtered.local_hash = local_hash;
                    local_filtered.comparison = Some(comparison);
                }
                Ok(info) if info.exists() => {}
                _ => is_deleted = true,
            }
        }

        let is_modified = matches!(local_filtered.comparison, Some(NodeComparison::Differs));

        let node_size = if node_link.node == ROOT_NODE {
            let tree = state_current
                .tree(repository.clone())
                .await
                .forward::<InfoError>("Failed deserializing revision state")?;
            tree.size
        } else {
            node.size
        };
        event::LoreEvent::FileInfo(LoreFileInfoEventData {
            path: node_path.into(),
            context: address_context,
            hash: node.address.hash,
            is_file: node.is_file().into(),
            is_dir: (node.is_directory() || node.is_link()).into(),
            flag_modified: is_modified.into(),
            flag_deleted: is_deleted.into(),
            flag_added: 0,
            flag_conflict: 0,
            size: node_size,
            mode: node.mode,
            local_size: local_filtered.local_size,
            local_hash: local_filtered.local_hash,
            filter_size: local_filtered.filtered_size,
        })
        .send();

        let metadata_node = node::node_to_file_metadata(node_link.node);
        let metadata_block_index = NodeFileMetadataBlock::index(metadata_node);
        let metadata_node_index = NodeFileMetadata::index(metadata_node);

        let metadata_block = state_current
            .block_file_metadata(repository.clone(), metadata_block_index)
            .await
            .forward::<InfoError>("Deserialize metadata block failed")?;

        let metadata_hash = {
            let metadata_block_reader = metadata_block.read();
            let node = metadata_block_reader.node(metadata_node_index);

            node.metadata
        };

        if !metadata_hash.is_zero() {
            let metadata = Metadata::deserialize(repository.clone(), metadata_hash)
                .await
                .forward::<InfoError>("Deserialize metadata failed")?;

            event::metadata::send(&metadata);
        }
    } else {
        let info = operation
            .file_info(&path)
            .await
            .unwrap_or(FileInfo::NotExist);
        if info.exists() {
            event::LoreEvent::FileInfo(LoreFileInfoEventData {
                path: path.into(),
                context: Context::default(),
                hash: Hash::default(),
                is_file: info.is_file().into(),
                is_dir: info.is_dir().into(),
                flag_modified: 0,
                flag_deleted: 0,
                flag_added: 1,
                flag_conflict: 0,
                size: 0,
                mode: 0,
                local_size: info.size(),
                local_hash: Hash::default(),
                filter_size: 0,
            })
            .send();
        }
    }

    Ok(())
}

/// Which sizes a request asked to be walked for, neither of which is cheap on a directory.
#[derive(Clone, Copy)]
struct RequestedSizes {
    /// Walk the working tree below the path.
    local: bool,
    /// Walk the state below the path, counting what the filter admits.
    filtered: bool,
}

/// How the working tree's file at `relative_path` compares to `node`.
///
/// Measured through `operation` against the fragmentation the content was stored under, which is
/// what a hash of the file taken on its own cannot answer for. `file_size` is the size already
/// measured, which settles a differing one without reading anything.
async fn compare_to_node(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    node: &Node,
    file_size: u64,
    relative_path: &RelativePath,
) -> Result<(NodeComparison, Hash), InfoError> {
    let comparison = crate::state::file_matches_node(
        repository.clone(),
        node,
        file_size,
        relative_path,
        operation,
        &lore_storage::ContentHashes::default(),
    )
    .await
    .forward_with::<InfoError, _>(|| format!("Failed to compare local file: {relative_path}"))?;

    let local_hash = match comparison {
        NodeComparison::Matches => node.address.hash,
        NodeComparison::Differs => {
            immutable::hash_file(repository.clone(), &operation.content_source(relative_path))
                .await
                .forward_with::<InfoError, _>(|| {
                    format!("Failed to hash local file: {relative_path}")
                })?
        }
        NodeComparison::Unreadable => Hash::default(),
    };

    Ok((comparison, local_hash))
}

/// What a path holds locally, and how that compared to the node where it was compared at all.
///
/// `comparison` is `None` where nothing was compared: a directory, a path the filter excludes, or
/// one the working tree holds no file at, and `local_hash` is zero for the same.
#[derive(Default)]
struct LocalFiltered {
    local_size: u64,
    local_hash: Hash,
    comparison: Option<NodeComparison>,
    filtered_size: u64,
}

async fn calculate_local_filtered_size_hash(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    relative_path: RelativePath,
    state: Arc<State>,
    node: Node,
    node_id: NodeID,
    sizes: RequestedSizes,
) -> Result<LocalFiltered, InfoError> {
    let parent_states = repository.filter.parent_exclusion_states(&relative_path);
    let (_, excluded) = repository.filter.child_emit_excludes(
        parent_states,
        &relative_path,
        node.is_directory(),
        FilterMode::Full,
    );
    if excluded {
        return Ok(LocalFiltered::default());
    }

    if node.is_file() {
        let info = operation
            .file_info(&relative_path)
            .await
            .unwrap_or(FileInfo::NotExist);
        if !info.is_file() {
            return Ok(LocalFiltered {
                filtered_size: node.size,
                ..LocalFiltered::default()
            });
        }

        let (comparison, local_hash) = if info.size() > 0 {
            let compared =
                compare_to_node(&operation, &repository, &node, info.size(), &relative_path)
                    .await?;
            (Some(compared.0), compared.1)
        } else {
            (None, Hash::default())
        };
        Ok(LocalFiltered {
            local_size: info.size(),
            local_hash,
            comparison,
            filtered_size: node.size,
        })
    } else if node.is_directory() {
        // Get the local file sizes
        let local_size_repository = repository.clone();
        let local_size_relative_path = relative_path.clone();
        let local_size_operation = operation.clone();
        let local_size_task = lore_spawn!(async move {
            if sizes.local {
                lore_debug!("Calculating local size");
                let local_info = local_size_operation
                    .file_info(&local_size_relative_path)
                    .await
                    .unwrap_or(FileInfo::NotExist);
                calculate_local_size_recurse(
                    local_size_operation,
                    local_size_repository,
                    local_size_relative_path,
                    local_info,
                    parent_states,
                )
                .await
            } else {
                Ok(0)
            }
        });

        // Filter local directory and calculate sizes
        let filtered_size_repository = repository.clone();
        let filtered_size_relative_path = relative_path.clone();
        let filtered_state = state.clone();
        let filtered_size_task = lore_spawn!(async move {
            if sizes.filtered {
                lore_debug!("Calculating filtered size");
                calculate_filtered_size_recurse(
                    filtered_size_repository,
                    filtered_size_relative_path,
                    filtered_state,
                    node,
                    node_id,
                    parent_states,
                )
                .await
            } else {
                Ok(0)
            }
        });

        let local_result = local_size_task.await;
        let filtered_result = filtered_size_task.await;

        let local_size = local_result
            .internal("Internal task failure")
            .map_err(InfoError::from)
            .flatten()?;
        let filtered_size = filtered_result
            .internal("Internal task failure")
            .map_err(InfoError::from)
            .flatten()?;

        Ok(LocalFiltered {
            local_size,
            filtered_size,
            ..LocalFiltered::default()
        })
    } else if node.is_link() {
        // TODO(vri): UCS-19229 - Links: Handle link nodes in file info lookup
        Ok(LocalFiltered::default())
    } else {
        Ok(LocalFiltered::default())
    }
}

/// Whether `path` names the repository's own directory, which no walk measures or descends into.
fn is_dot_directory(path: &RelativePath) -> bool {
    path.as_str() == DOT_URC || path.as_str() == DOT_LORE
}

/// What the file at `path` adds to the local size, which is nothing where the repository's own
/// directory stands there or the filter leaves the path out.
///
/// Measured where it is listed rather than in a walk of its own: the listing carries what the file
/// holds, so nothing here reads the working tree and a file costs no task and no boxed walk.
fn local_file_size(
    repository: &RepositoryContext,
    path: &RelativePath,
    info: FileInfo,
    parent_states: FilterStates,
) -> u64 {
    if is_dot_directory(path) {
        return 0;
    }
    let (_, excluded) =
        repository
            .filter
            .child_emit_excludes(parent_states, path, false, FilterMode::Full);
    if excluded { 0 } else { info.size() }
}

/// The subtree walks a directory has in flight, each answering with what it measured.
type LocalSizeTasks = JoinSet<Result<u64, InfoError>>;

static LOCAL_SIZE_TASK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Process-wide rather than per-walk: `file info` measures every path it was given at once, so a
/// budget owned by one walk would let a run fan out once for every path it names.
fn local_size_task_semaphore() -> &'static Arc<Semaphore> {
    LOCAL_SIZE_TASK_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_TREE_TASKS)))
}

/// Measures the subtree at `path`, spawned while the budget allows and walked inline once it does
/// not, answering with what an inline walk measured and zero for one left to a task.
///
/// Inline rather than a blocking acquire: a parent holds its permit until its children finish, so
/// waiting on one would wait on a descendant that cannot start.
async fn local_size_subtree_dispatch(
    operation: &Arc<InstanceOperationImpl>,
    repository: &Arc<RepositoryContext>,
    path: RelativePath,
    info: FileInfo,
    states: FilterStates,
    tasks: &mut LocalSizeTasks,
) -> Result<u64, InfoError> {
    if let Ok(permit) = local_size_task_semaphore().clone().try_acquire_owned() {
        let operation = operation.clone();
        let repository = repository.clone();
        lore_spawn!(tasks, async move {
            let _permit = permit;
            calculate_local_size_recurse(operation, repository, path, info, states).await
        });
        return Ok(0);
    }
    calculate_local_size_recurse(operation.clone(), repository.clone(), path, info, states).await
}

/// `info` is what the working tree holds at `relative_path`, which the listing a directory is
/// reached through already measured: a child is not measured again to be walked. A path holding
/// nothing measures zero without the filter being asked about it.
///
/// `parent_states` is the filter verdict for the directory holding
/// `relative_path`, which this walk steps once per node rather than folding the
/// whole path per node.
fn calculate_local_size_recurse(
    operation: Arc<InstanceOperationImpl>,
    repository: Arc<RepositoryContext>,
    relative_path: RelativePath,
    info: FileInfo,
    parent_states: FilterStates,
) -> Pin<Box<dyn Future<Output = Result<u64, InfoError>> + Send>> {
    Box::pin(async move {
        if !info.exists() {
            return Ok(0);
        }
        if !info.is_dir() {
            return Ok(local_file_size(
                &repository,
                &relative_path,
                info,
                parent_states,
            ));
        }
        if is_dot_directory(&relative_path) {
            return Ok(0);
        }

        let (states, excluded) = repository.filter.child_emit_excludes(
            parent_states,
            &relative_path,
            true,
            FilterMode::Full,
        );
        if excluded {
            return Ok(0);
        }

        let mut local_size = 0;
        let mut local_size_tasks = LocalSizeTasks::new();
        let mut list = operation
            .read_directory(&relative_path)
            .await
            .forward_any_with::<InfoError, _>(|| {
                format!("Failed to list directory: {relative_path}")
            })?;

        while let Some(item) = list.next().await {
            let item = item.forward_any::<InfoError>("Unusable directory entry")?;
            let child_path = relative_path.push_into_buf(item.name.as_str()).freeze();
            if !item.info.is_dir() {
                local_size += local_file_size(&repository, &child_path, item.info, states);
                continue;
            }
            local_size += local_size_subtree_dispatch(
                &operation,
                &repository,
                child_path,
                item.info,
                states,
                &mut local_size_tasks,
            )
            .await?;
        }

        let mut failure: Option<InfoError> = None;
        while let Some(result) = local_size_tasks.join_next().await {
            let inner = result
                .internal("Internal task failure")
                .map_err(InfoError::from)
                .flatten();
            match inner {
                Ok(size) => {
                    local_size += size;
                }
                Err(err) => {
                    failure = failure.or(Some(err));
                }
            }
        }

        if let Some(err) = failure {
            return Err(err);
        }

        Ok(local_size)
    })
}

/// `parent_states` is the filter verdict for the directory holding
/// `relative_path`, which this walk steps once per node rather than folding the
/// whole path per node.
fn calculate_filtered_size_recurse(
    repository: Arc<RepositoryContext>,
    relative_path: RelativePath,
    state: Arc<State>,
    node: Node,
    node_id: NodeID,
    parent_states: FilterStates,
) -> Pin<Box<dyn Future<Output = Result<u64, InfoError>> + Send>> {
    Box::pin(async move {
        let (states, excluded) = repository.filter.child_emit_excludes(
            parent_states,
            &relative_path,
            node.is_directory(),
            FilterMode::Full,
        );
        if excluded {
            return Ok(0);
        }
        if node.is_file() {
            return Ok(node.size);
        }

        let mut filtered_size = 0;
        let mut filtered_size_tasks = JoinSet::new();

        let mut failure: Option<InfoError> = None;
        let mut child_iter = node.child();
        let mut cycle = SiblingCycleGuard::new(node_id);
        while let Some(child) = child_iter {
            let repository = repository.clone();
            let state = state.clone();
            let relative_path = relative_path.clone();
            let Ok(child_node) = state.node(repository.clone(), child).await else {
                failure = Some(InfoError::internal(
                    "Failed to calculate filtered size, encountered an invalid node",
                ));
                break;
            };
            if let Err(err) = child_node
                .walk_step(child, node_id, &mut cycle)
                .forward::<InfoError>("Failed to calculate filtered size, invalid node hierarchy")
            {
                failure = Some(err);
                break;
            }
            let sibling = child_node.sibling();
            lore_spawn!(filtered_size_tasks, async move {
                let name = state
                    .node_name_ref(repository.clone(), child)
                    .await
                    .forward::<InfoError>(
                        "Failed to calculate filtered size, encountered an invalid node",
                    )?;
                let relative_path = relative_path.push_into_buf(name).freeze();
                calculate_filtered_size_recurse(
                    repository,
                    relative_path,
                    state,
                    child_node,
                    child,
                    states,
                )
                .await
            });

            child_iter = sibling;
        }

        while let Some(result) = filtered_size_tasks.join_next().await {
            let inner = result
                .internal("Internal task failure")
                .map_err(InfoError::from)
                .flatten();
            match inner {
                Ok(size) => {
                    filtered_size += size;
                }
                Err(err) => {
                    failure = failure.or(Some(err));
                }
            }
        }

        if let Some(err) = failure {
            return Err(err);
        }

        Ok(filtered_size)
    })
}

#[cfg(test)]
// A fixture builds working-tree state directly, outside any revision; what these test is what the
// walk measures of it.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fs::filesystem_provider::tests::test_store_create;
    use crate::repository::test_helpers::RepositoryContextCreationArgsExt;
    use crate::repository::test_helpers::default_repository_creation_args;

    /// A repository over the working tree at `root`, admitting every path in it.
    async fn os_repository(root: &Path) -> Arc<RepositoryContext> {
        let (immutable_store, mutable_store, _context) =
            test_store_create().await.expect("making test stores");
        Arc::new(RepositoryContext::new(
            default_repository_creation_args(immutable_store, mutable_store).with_path(root),
        ))
    }

    /// A directory holding two files and a subdirectory holding a third, 175 bytes in all.
    fn write_tree(root: &Path) {
        let tree = root.join("tree");
        std::fs::create_dir_all(tree.join("inner")).expect("create directory");
        std::fs::write(tree.join("first.txt"), vec![b'a'; 100]).expect("write file");
        std::fs::write(tree.join("third.txt"), vec![b'c'; 25]).expect("write file");
        std::fs::write(tree.join("inner").join("second.txt"), vec![b'b'; 50]).expect("write file");
    }

    /// What the walk measures at `path`, under the working tree at `root`.
    async fn measure(root: &Path, path: RelativePath) -> u64 {
        let repository = os_repository(root).await;
        let operation = repository
            .file_system()
            .begin_operation()
            .await
            .expect("an operation over the working tree");
        calculate_local_size_recurse(
            operation,
            repository,
            path,
            FileInfo::Directory,
            FilterStates::ROOT,
        )
        .await
        .expect("a measured tree")
    }

    /// What the walk measures of the tree [`write_tree`] wrote under `root`.
    async fn measure_tree(root: &Path) -> u64 {
        measure(
            root,
            RelativePath::new_from_initial_path("tree").expect("relative path"),
        )
        .await
    }

    #[tokio::test]
    async fn a_directory_measures_every_file_below_it() {
        let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
        write_tree(dir.path());

        assert_eq!(175, measure_tree(dir.path()).await);
    }

    /// The repository's own directory adds nothing, whatever the working tree holds at its name.
    #[tokio::test]
    async fn the_repository_directory_is_not_measured() {
        let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
        write_tree(dir.path());
        std::fs::write(dir.path().join(DOT_LORE), vec![b'd'; 40]).expect("write file");

        assert_eq!(175, measure(dir.path(), RelativePath::default()).await);
    }

    /// A subtree is walked inline once the fan-out budget is spent, measuring what a walk of its
    /// own would have.
    #[tokio::test]
    async fn a_subtree_is_measured_inline_once_the_budget_is_spent() {
        let dir = lore_base::test_util::TempDir::new("lore-info-local-size-");
        write_tree(dir.path());

        let _permits = local_size_task_semaphore()
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_TREE_TASKS as u32)
            .await
            .expect("the whole fan-out budget");

        assert_eq!(175, measure_tree(dir.path()).await);
    }
}
