//! What an SWFS mount serves at each path of the working tree (ADR-00036): the parent's current
//! revision, and below each link and layer mount path the repository mounted there. The mount's
//! callbacks, an operation's reads and the thaw's descriptions all resolve paths through it, so
//! they cannot disagree about what a path is.
use std::collections::HashMap;
use std::sync::Arc;

use lore_base::types::Hash;
use lore_error_set::ForwardStrict;
use lore_storage::hash;
use parking_lot::Mutex;

use crate::fs::filesystem_provider::FileInfo;
use crate::fs::swfs::paths::rest_below;
use crate::layer::Layer;
use crate::lore::RepositoryId;
use crate::lore_warn;
use crate::node::Node;
use crate::node::NodeID;
use crate::node::ROOT_NODE;
use crate::node::SiblingCycleGuard;
use crate::repository::RepositoryContext;
use crate::repository::is_reserved_node_name;
use crate::state::MAX_LINK_DEPTH;
use crate::state::State;
use crate::state::StateError;
use crate::state::file_modified_time;
use crate::util::path::RelativePath;

/// A link or layer the mount serves: the subtree of `state`, of `repository`, from `source` down,
/// at `path` in the working tree.
#[derive(Clone)]
pub struct ServedMount {
    pub path: RelativePath,
    pub repository: Arc<RepositoryContext>,
    pub state: Arc<State>,
    pub source: NodeID,
}

impl ServedMount {
    /// Whether `other` serves what this does: the same repository's revision, from the same node.
    pub fn serves_as(&self, other: &ServedMount) -> bool {
        self.repository.id == other.repository.id
            && self.state.revision() == other.state.revision()
            && self.source == other.source
    }

    /// What is served at the mount path itself: the directory the link or layer mounts.
    async fn root(&self) -> Result<Served, StateError> {
        let node = self
            .state
            .node(self.repository.clone(), self.source)
            .await?;
        Ok(Served::Node {
            repository: self.repository.clone(),
            state: self.state.clone(),
            node_id: self.source,
            node,
            mount: Some(self.path.clone()),
        })
    }
}

/// What the mount serves at a path.
#[derive(Clone)]
pub enum Served {
    /// The node `node_id` of `state`, of `repository`. A link is entered, so what is served at a
    /// link's path is the directory it mounts. `mount` is the path of the link or layer the node
    /// lies under, `None` for the parent's own tree.
    Node {
        repository: Arc<RepositoryContext>,
        state: Arc<State>,
        node_id: NodeID,
        node: Node,
        mount: Option<RelativePath>,
    },
    /// A directory no state holds, which leads to a link or layer mount path, such as the new
    /// directory a staged link was added under.
    Leading,
}

impl Served {
    pub fn is_directory(&self) -> bool {
        match self {
            Served::Node { node, .. } => node.is_directory(),
            Served::Leading => true,
        }
    }

    /// What is served at `path`, as SWFS and a listing are told of it: a file with the time its
    /// repository recorded for it, or a directory.
    pub async fn file_info(&self, path: &RelativePath) -> FileInfo {
        match self {
            Served::Node {
                repository, node, ..
            } if !node.is_directory() => FileInfo::from_node_and_mtime(
                node,
                file_modified_time(repository.clone(), path).await,
            ),
            _ => FileInfo::Directory,
        }
    }
}

/// A child of a served directory, by the name it is listed under.
pub struct ServedChild {
    pub name: String,
    pub served: Served,
}

/// A linked repository's state at a pin, entered at a nested link.
type Entered = (Arc<RepositoryContext>, Arc<State>);

/// Maps each path of the working tree to what serves it.
pub struct ServedTree {
    repository: Arc<RepositoryContext>,
    current: Option<Arc<State>>,
    /// The links of the parent and its layers, which are served in place of what the parent's
    /// current revision holds at and below their paths.
    mounts: Vec<ServedMount>,
    /// The states of nested links, by linked repository and pin, entered once.
    entered: Mutex<HashMap<(RepositoryId, Hash), Entered>>,
}

impl std::fmt::Debug for ServedTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServedTree")
            .field(
                "current",
                &self.current.as_ref().map(|state| state.revision()),
            )
            .field(
                "mounts",
                &self
                    .mounts
                    .iter()
                    .map(|mount| mount.path.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ServedTree {
    /// The tree of `repository`, serving `current` with `mounts` in place of what it holds at
    /// their paths.
    pub fn new(
        repository: Arc<RepositoryContext>,
        current: Option<Arc<State>>,
        mounts: Vec<ServedMount>,
    ) -> Self {
        Self {
            repository,
            current,
            mounts,
            entered: Mutex::default(),
        }
    }

    /// The tree a working tree on `current` holds: each link at the pin `staged` holds, or
    /// `current` where nothing is staged, as a plain working tree has it after `link add`,
    /// `link update` or a sync; and each of `layers` at its current revision, where `current`
    /// holds nothing at its path. A link or layer whose state cannot be read is left out, and
    /// what lies at its path with it.
    pub async fn load(
        repository: Arc<RepositoryContext>,
        current: Option<Arc<State>>,
        staged: Option<Arc<State>>,
        layers: &[Layer],
    ) -> Result<Self, StateError> {
        let mut mounts = Vec::new();
        if let Some(links_state) = staged.as_ref().or(current.as_ref()) {
            for reference in links_state.link_list(repository.clone()).await? {
                match link_mount(&repository, links_state, reference.local_node()).await {
                    Ok(Some(mount)) => mounts.push(mount),
                    Ok(None) => {}
                    Err(error) => lore_warn!(
                        "SWFS: not serving the link at node {}: {error}",
                        reference.local_node()
                    ),
                }
            }
        }
        for layer in layers {
            match layer_mount(&repository, current.as_ref(), layer).await {
                Ok(Some(mount)) => mounts.push(mount),
                Ok(None) => {}
                Err(error) => lore_warn!(
                    "SWFS: not serving the layer at {}: {error}",
                    layer.target_path
                ),
            }
        }
        Ok(Self::new(repository, current, mounts))
    }

    pub fn repository(&self) -> &Arc<RepositoryContext> {
        &self.repository
    }

    pub fn current(&self) -> Option<&Arc<State>> {
        self.current.as_ref()
    }

    pub fn mounts(&self) -> &[ServedMount] {
        &self.mounts
    }

    /// The mount at exactly `path`, in any spelling.
    pub fn mount_at(&self, path: &RelativePath) -> Option<&ServedMount> {
        self.mounts
            .iter()
            .find(|mount| mount.path.as_str().eq_ignore_ascii_case(path.as_str()))
    }

    /// Whether `path` lies at or below a mount path.
    pub fn is_mounted(&self, path: &RelativePath) -> bool {
        self.mount_of(path).is_some()
    }

    /// The innermost mount `path` lies at or below, with the rest of the path below it.
    fn mount_of(&self, path: &RelativePath) -> Option<(&ServedMount, RelativePath)> {
        self.mounts
            .iter()
            .filter_map(|mount| Some((mount, rest_below(path.as_str(), mount.path.as_str())?)))
            .max_by_key(|(mount, _)| mount.path.as_str().len())
            .map(|(mount, rest)| {
                (
                    mount,
                    RelativePath::new_from_initial_path(rest).unwrap_or_default(),
                )
            })
    }

    /// Whether the tree serves any link or layer.
    pub fn has_mounts(&self) -> bool {
        !self.mounts.is_empty()
    }

    /// Whether `path` is a directory above a mount path.
    fn leads_to_a_mount(&self, path: &RelativePath) -> bool {
        self.mounts
            .iter()
            .any(|mount| next_component(path, &mount.path).is_some())
    }

    /// What the mount serves at `path`, or `None` where it serves nothing.
    pub async fn resolve(&self, path: &RelativePath) -> Result<Option<Served>, StateError> {
        if let Some((mount, rest)) = self.mount_of(path) {
            let node = mount
                .state
                .node(mount.repository.clone(), mount.source)
                .await?;
            return self
                .walk(
                    mount.repository.clone(),
                    mount.state.clone(),
                    mount.source,
                    node,
                    &rest,
                    Some(mount.path.clone()),
                )
                .await;
        }
        let Some(current) = &self.current else {
            return Ok((path.is_empty() || self.leads_to_a_mount(path)).then_some(Served::Leading));
        };
        let root = current.node(self.repository.clone(), ROOT_NODE).await?;
        match self
            .walk(
                self.repository.clone(),
                current.clone(),
                ROOT_NODE,
                root,
                path,
                None,
            )
            .await?
        {
            Some(served) => Ok(Some(served)),
            None if self.leads_to_a_mount(path) => Ok(Some(Served::Leading)),
            None => Ok(None),
        }
    }

    /// The children of the served directory at `path`, or `None` where the mount serves no
    /// directory there. A link is listed as the directory it mounts, and a directory leading to a
    /// link or layer mount path is listed whether or not a state holds it.
    pub async fn children(
        &self,
        path: &RelativePath,
    ) -> Result<Option<Vec<ServedChild>>, StateError> {
        let Some(served) = self.resolve(path).await? else {
            return Ok(None);
        };
        let mut children = Vec::new();
        if let Served::Node {
            repository,
            state,
            node_id,
            node,
            mount,
        } = served
        {
            if !node.is_directory() {
                return Ok(None);
            }
            let mut child_id = node.child();
            let mut cycle = SiblingCycleGuard::new(node_id);
            while let Some(id) = child_id {
                let child = state.node(repository.clone(), id).await?;
                child.walk_step(id, node_id, &mut cycle)?;
                child_id = child.sibling();
                if child.is_staged_delete() {
                    continue;
                }
                let Some(name) = state.node_name_ref_or_skip(repository.clone(), id).await? else {
                    continue;
                };
                let name: String = AsRef::<str>::as_ref(&name).to_owned();
                // The parent's own links are listed from the mounts, which leave out one the
                // staged state removed. A link nested in a mounted tree is entered.
                if child.is_link() && mount.is_none() {
                    continue;
                }
                let Some((repository, state, id, node)) = self
                    .enter(repository.clone(), state.clone(), id, child)
                    .await?
                else {
                    continue;
                };
                children.push(ServedChild {
                    name,
                    served: Served::Node {
                        repository,
                        state,
                        node_id: id,
                        node,
                        mount: mount.clone(),
                    },
                });
            }
        }
        for mount in &self.mounts {
            let Some(name) = next_component(path, &mount.path) else {
                continue;
            };
            if children
                .iter()
                .any(|child| child.name.eq_ignore_ascii_case(name))
            {
                continue;
            }
            let served = if mount.path.as_str().len() == path.join(name).as_str().len() {
                mount.root().await?
            } else {
                Served::Leading
            };
            children.push(ServedChild {
                name: name.to_owned(),
                served,
            });
        }
        Ok(Some(children))
    }

    /// Walks `path` down from `node_id` in `state`. Within a mount (`mount` set) a link is
    /// entered, at the pin its containing revision holds; in the parent's own tree a link is
    /// served only as a mount, so reaching one serves nothing.
    async fn walk(
        &self,
        mut repository: Arc<RepositoryContext>,
        mut state: Arc<State>,
        mut node_id: NodeID,
        mut node: Node,
        path: &RelativePath,
        mount: Option<RelativePath>,
    ) -> Result<Option<Served>, StateError> {
        for name in path.as_str().split('/').filter(|name| !name.is_empty()) {
            if is_reserved_node_name(name) {
                return Ok(None);
            }
            // In the parent's own tree a link is served only as a mount.
            if node.is_link() && mount.is_none() {
                return Ok(None);
            }
            let Some(entered) = self.enter(repository, state, node_id, node).await? else {
                return Ok(None);
            };
            (repository, state, node_id, node) = entered;
            if !node.is_directory() {
                return Ok(None);
            }
            node_id = match state
                .find_subnode(repository.clone(), node_id, hash::hash_string(name))
                .await
            {
                Ok(id) => id,
                Err(error) if error.is_node_not_found() => return Ok(None),
                Err(error) => return Err(error),
            };
            node = state.node(repository.clone(), node_id).await?;
            // A link or layer served at a staged revision keeps what the stage deletes as a node
            // flagged for it, which a working tree no longer holds.
            if node.is_staged_delete() {
                return Ok(None);
            }
        }
        if node.is_link() && mount.is_none() {
            return Ok(None);
        }
        let Some(entered) = self.enter(repository, state, node_id, node).await? else {
            return Ok(None);
        };
        (repository, state, node_id, node) = entered;
        Ok(Some(Served::Node {
            repository,
            state,
            node_id,
            node,
            mount,
        }))
    }

    /// The directory the link `node` mounts: its source node in the linked repository's state at
    /// the link's pin. A link whose source is itself a link is followed, up to the nesting
    /// depth links allow.
    async fn enter(
        &self,
        mut repository: Arc<RepositoryContext>,
        mut state: Arc<State>,
        mut node_id: NodeID,
        mut node: Node,
    ) -> Result<Option<(Arc<RepositoryContext>, Arc<State>, NodeID, Node)>, StateError> {
        for _ in 0..MAX_LINK_DEPTH {
            if !node.is_link() {
                return Ok(Some((repository, state, node_id, node)));
            }
            let link = node.linked_node();
            let key = (link.repository, link.revision);
            let entered = self.entered.lock().get(&key).cloned();
            (repository, state) = if let Some(entered) = entered {
                entered
            } else {
                let entered =
                    Box::pin(open_link(&repository, link.repository, link.revision)).await?;
                self.entered.lock().insert(key, entered.clone());
                entered
            };
            node_id = link.node;
            node = state.node(repository.clone(), node_id).await?;
        }
        Ok(None)
    }
}

/// The linked repository `linked` at `revision`, reached from `repository`.
///
/// Cold: a nested link's pin is opened once per tree, and boxed where it is awaited. Inline, its
/// connection to the remote would make every resolve as large as opening a link.
async fn open_link(
    repository: &Arc<RepositoryContext>,
    linked: RepositoryId,
    revision: Hash,
) -> Result<Entered, StateError> {
    let linked = repository.to_link_context(linked).await;
    let state = State::deserialize(linked.clone(), revision).await?;
    Ok((linked, state))
}

/// How a link or layer mount path changed between two served trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountChange {
    /// Only the later tree mounts something there.
    Added,
    /// Both do, from another repository, revision or source node.
    Repinned,
    /// Only the earlier tree mounts something there.
    Removed,
}

/// The link and layer mount paths whose mount differs between `old` and `new`, each spelled as
/// the tree that mounts it spells it, the later one where both do.
pub fn mount_changes(old: &ServedTree, new: &ServedTree) -> Vec<(RelativePath, MountChange)> {
    let mut changes = Vec::new();
    for mount in new.mounts() {
        match old.mount_at(&mount.path) {
            None => changes.push((mount.path.clone(), MountChange::Added)),
            Some(before) if !before.serves_as(mount) => {
                changes.push((mount.path.clone(), MountChange::Repinned));
            }
            Some(_) => {}
        }
    }
    for mount in old.mounts() {
        if new.mount_at(&mount.path).is_none() {
            changes.push((mount.path.clone(), MountChange::Removed));
        }
    }
    changes
}

/// The name of the child of `directory` that leads to `mount_path`, where `mount_path` lies
/// strictly below `directory`.
fn next_component<'a>(directory: &RelativePath, mount_path: &'a RelativePath) -> Option<&'a str> {
    let rest = rest_below(mount_path.as_str(), directory.as_str())?;
    let name = rest.split('/').next()?;
    (!name.is_empty()).then_some(name)
}

/// The link at node `local_node` of `state`, served from its pin.
async fn link_mount(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    local_node: NodeID,
) -> Result<Option<ServedMount>, StateError> {
    let node = state.node(repository.clone(), local_node).await?;
    if !node.is_link() {
        return Ok(None);
    }
    let path = state.node_path(repository.clone(), local_node).await?;
    let path = RelativePath::new_from_initial_path(&path).forward_with::<StateError, _>(|| {
        format!("Parsing the path of the link at node {local_node}")
    })?;
    let link = node.linked_node();
    let (linked, linked_state) = open_link(repository, link.repository, link.revision).await?;
    Ok(Some(ServedMount {
        path,
        repository: linked,
        state: linked_state,
        source: link.node,
    }))
}

/// `layer`, served from its current revision, unless `current` holds its target path: the
/// parent's tree is served there, as a status of the working tree reports it.
async fn layer_mount(
    repository: &Arc<RepositoryContext>,
    current: Option<&Arc<State>>,
    layer: &Layer,
) -> Result<Option<ServedMount>, StateError> {
    let path = RelativePath::new_from_initial_path(&layer.target_path)
        .forward::<StateError>("Parsing a layer's target path")?;
    let source_path = RelativePath::new_from_initial_path(&layer.source_path)
        .forward::<StateError>("Parsing a layer's source path")?;
    if path.is_empty() {
        return Err(StateError::internal("A layer's target path is the root"));
    }
    if let Some(current) = current {
        match current.find_node(repository.clone(), path.as_str()).await {
            Ok(_) => return Ok(None),
            Err(error) if error.is_node_not_found() => {}
            Err(error) => return Err(error),
        }
    }
    let layered = repository.to_layer_context(layer.repository).await;
    let layered_state = State::deserialize(layered.clone(), layer.current).await?;
    let source = if source_path.is_empty() {
        ROOT_NODE
    } else {
        layered_state
            .find_node_link(layered.clone(), source_path.as_str())
            .await?
            .node
    };
    Ok(Some(ServedMount {
        path,
        repository: layered,
        state: layered_state,
        source,
    }))
}
