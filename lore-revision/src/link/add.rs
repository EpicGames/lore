// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;
use std::sync::Arc;

use lore_error_set::prelude::*;
use lore_transport::Connection;
use lore_transport::RepositoryData;

use super::LinkError;
use crate::branch;
use crate::errors::FileNotFound;
use crate::errors::InvalidArguments;
use crate::errors::is_refusal;
use crate::event;
use crate::filter::FilterMode;
use crate::fs::filesystem_provider::FileInfo;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::interface::LoreFileAction;
use crate::layer;
use crate::link;
use crate::link::LinkFlags;
use crate::link::LoreLinkChangeEventData;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Hash;
use crate::lore::execution_context;
use crate::lore_debug;
use crate::node::Node;
use crate::node::NodeFlags;
use crate::repository;
use crate::repository::MountKind;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::repository::clone;
use crate::repository::clone::CloneContext;
use crate::repository::clone::CloneStats;
use crate::repository::clone::LoreRepositoryCloneBeginEventData;
use crate::repository::clone::LoreRepositoryCloneCountData;
use crate::repository::clone::LoreRepositoryCloneEndEventData;
use crate::stage;
use crate::stage::KnownChild;
use crate::stage::StageOptions;
use crate::state::FoundNode;
use crate::state::LinkReference;
use crate::state::NodeMapping;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;
use crate::util::path::RelativePath;
use crate::util::path::RelativePathBuf;
use crate::util::url::same_server;

pub async fn add(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    link_path: RelativePath,
    link_identifier: String,
    source_path: RelativePath,
    pin: Option<String>,
    disable_branching: bool,
) -> Result<(), LinkError> {
    // A provider holding its state serves a link from the link's node and takes none of the link
    // through its operations (`FilesystemProvider::holds_state`), which the clone below writes.
    if repository.file_system().holds_state() {
        return Err(crate::fs::hold::refused("add a link").into());
    }

    if let Some(layer) = layer::list(repository.clone())
        .await
        .forward_any::<LinkError>("listing layers")?
        .into_iter()
        .find(|layer| {
            let Ok(target_path) = RelativePath::from_str(&layer.target_path);
            link_path.overlaps_ignore_case(&target_path)
        })
    {
        return Err(MountKind::Link
            .overlap(link_path.as_str(), &layer.target_path)
            .into());
    }

    let repository_data = resolve_identifier(&repository, link_identifier).await?;
    let link = repository_data.id;

    if link == repository.id {
        return Err(LinkError::internal(
            "Invalid link, a link cannot link to itself",
        ));
    }

    let (state_current, state_staged, current_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| state_current.clone());

    lore_debug!("Resolve link {link} {source_path}");
    let link = repository.to_link_context(link).await;

    let link_remote = link
        .remote()
        .await
        .forward_with::<LinkError, _>(|| link::unreachable(link.id))?;

    let link_label = || format!("{} ({})", repository_data.name, link.id);

    // Resolve through any parent links so the link lands in the innermost
    // containing repository (empty chain for a plain top-level link).
    let chain = link::resolve_link_chain(
        NodeMapping::root(repository.clone(), state_staged.clone()),
        state_current.clone(),
        link_path.clone(),
        current_branch,
    )
    .await?;

    let inner_repository = chain.innermost.repository.clone();
    let inner_state = chain.innermost.state.clone();
    let remainder_path = chain.remainder_path.clone();

    let mounted = inner_state
        .link_list(inner_repository.clone())
        .await
        .forward::<LinkError>("Failed reading the link registry")?
        .into_iter()
        .find(|row| row.repository() == link.id);

    let resolve_pin = async |pin: Option<String>| match pin {
        Some(pin) => link::resolve_pin(link.clone(), pin)
            .await
            .map(Some)
            .forward_with::<LinkError, _>(|| link::unreachable(link.id)),
        None => Ok(None),
    };
    let (link_revision, base_branch, branching) = if let Some(row) = mounted {
        let (revision, branch) = mounted_pin(
            &link,
            row,
            pin,
            disable_branching,
            chain.owner_branch(current_branch),
        )
        .await?;
        (revision, branch, LinkBranch::Off)
    } else if disable_branching {
        let (revision, branch) = match resolve_pin(pin).await? {
            Some(pinned) => pinned,
            None => default_branch_latest(&link, &link_remote).await?,
        };
        (revision, branch, LinkBranch::Off)
    } else {
        let (existing, pinned) = tokio::try_join!(
            async {
                branch::remote_latest_if_present(link_remote.clone(), link.id, current_branch)
                    .await
                    .forward_with::<LinkError, _>(|| link::unreachable(link.id))
            },
            resolve_pin(pin),
        )?;
        if let Some(latest) = existing {
            let (revision, branch) = pinned.unwrap_or((latest, current_branch));
            (revision, branch, LinkBranch::Reuse(latest))
        } else {
            let (parent_latest, parent) = default_branch_latest(&link, &link_remote).await?;
            let follows_parent = pinned.is_none();
            let (revision, branch) = pinned.unwrap_or((parent_latest, parent));
            (
                revision,
                branch,
                LinkBranch::Create {
                    parent,
                    parent_latest,
                    follows_parent,
                },
            )
        }
    };
    lore_debug!("Validating link revision {link_revision} on branch {base_branch}");
    let link_branch = if matches!(branching, LinkBranch::Off) {
        base_branch
    } else {
        current_branch
    };

    if link_revision.is_zero() {
        return Err(InvalidArguments {
            reason: format!(
                "linked repository {} has no revision on branch {}; commit and push to it first",
                link_label(),
                branch::display_name(link.clone(), base_branch).await
            ),
        }
        .into());
    }

    lore_debug!("Load link revision state");
    let link_state = State::deserialize(link.clone(), link_revision)
        .await
        .forward_with::<LinkError, _>(|| link::unreachable(link.id))?;

    lore_debug!("Find link target node for {source_path}");
    let found = link_state
        .locate_node(link.clone(), source_path.as_str())
        .await;
    let not_found = || {
        source_path_not_found(
            &link,
            link_label(),
            &source_path,
            link_revision,
            base_branch,
        )
    };
    let link_found = match found {
        Ok(found) if found.is_valid_or_root() => found,
        Ok(_) => return Err(not_found().await.into()),
        Err(err) if err.is_node_not_found() => {
            return Err(LinkError::FileNotFound(not_found().await.chain_err_from(
                err,
                "finding the source path in the linked repository",
            )));
        }
        Err(err) => {
            return Err(err).forward_with::<LinkError, _>(|| {
                format!(
                    "Failed to find {source_path} in linked repository {}",
                    link.id
                )
            });
        }
    };

    lore_debug!("Link target node is {:?}", link_found.identity());

    // Target node must be in the given link repository, not a link itself
    let Some(link_target) = link_found.own_node() else {
        return Err(LinkError::internal(
            "Link path is a link itself, link to the target repository directly",
        ));
    };

    // Target node must be a directory
    let link_node = link_state
        .node(link.clone(), link_target)
        .await
        .forward_with::<LinkError, _>(|| link::unreachable(link.id))?;

    if !link_node.is_directory() {
        return Err(InvalidArguments {
            reason: format!(
                "source path {} is not a directory in linked repository {}",
                link::display_path(source_path.as_str()),
                link_label(),
            ),
        }
        .into());
    }

    let clone_path = link_path.clone();

    // The stored path rather than the argument, which resolved case-insensitively.
    let resolved_source_path = link::link_source_path(link.clone(), &link_state, link_target)
        .await
        .forward_with::<LinkError, _>(|| link::unreachable(link.id))?;

    let (link_flags, stored_branch) = if let Some(row) = mounted {
        (LinkFlags::from_bits_retain(row.flags()), row.branch())
    } else if disable_branching {
        lore_debug!("Disabled auto-follow for link {}", link.id);
        (LinkFlags::DisableAutoFollow, link_branch)
    } else {
        (LinkFlags::NoFlags, BranchId::default())
    };

    link::check_source_path_overlap(
        &inner_state,
        inner_repository.clone(),
        link.clone(),
        resolved_source_path,
        &clone_path,
        Some(&chain.innermost.path),
        crate::node::INVALID_NODE,
        link::SameFolder::Refuse,
    )
    .await
    .forward::<LinkError>("Failed checking link source paths")?;

    if let Ok(node_id) = inner_state
        .locate_node_below(
            inner_repository.clone(),
            chain.innermost.node,
            remainder_path.as_str(),
        )
        .await
        .and_then(FoundNode::into_own_node)
        && let Ok(node) = inner_state.node(inner_repository.clone(), node_id).await
        // Allow re-adding a link to a path that is staged for delete
        && !node.is_staged_delete()
    {
        // Prevent linking into file or other link
        if !node.is_directory() {
            return Err(mount_refused("is already a link", &clone_path));
        }

        let mut children =
            StateNodeChildrenIterator::new(inner_state.clone(), inner_repository.clone(), node_id)
                .await
                .forward::<LinkError>("Failed deserializing state node block")?;

        // Prevent the directory having children
        if let Ok(child) = children.next().await
            && child.is_some()
        {
            return Err(mount_refused("already has children", &clone_path));
        }
    };

    // Intermediate directories leading to the link, relative to the innermost repo.
    let mut remainder_parent = remainder_path.clone();
    remainder_parent.pop();

    // One operation covers the mount point, the staging, the clone, and storing the staged state:
    // an SWFS instance serves the link from what is stored when the operation finalizes, so the
    // link has to be stored by then, or its files are written onto the mount as local changes
    // (ADR-00042).
    with_operation::<_, LinkError, _>(repository.file_system(), async |operation| {
        let mount = check_link_mount(&operation, &clone_path).await?;

        let outcome = match branching {
            LinkBranch::Off => None,
            LinkBranch::Reuse(latest) => Some(link::LinkBranchOutcome {
                revision: latest,
                reused: true,
            }),
            LinkBranch::Create {
                parent,
                parent_latest,
                follows_parent,
            } => {
                let outcome = create_link_branch(
                    &repository,
                    &link,
                    link_remote.clone(),
                    current_branch,
                    parent,
                    parent_latest,
                )
                .await?;
                // The add validated `parent_latest`, which a branch another client created
                // meanwhile need not hold.
                if follows_parent && outcome.revision != parent_latest {
                    return Err(InvalidArguments {
                        reason: format!(
                            "branch {} was created in linked repository {} while the link was \
                             added; run the command again",
                            branch::display_name(repository.clone(), current_branch).await,
                            link_label(),
                        ),
                    }
                    .into());
                }
                Some(outcome)
            }
        };
        if let Some(outcome) = outcome {
            link::report_branch_outcome(
                link_path.as_str(),
                link.id,
                current_branch,
                outcome.revision,
                outcome.reused,
            );
        }

        // A dry run does not create a missing link branch, which takes the name of the branch it
        // is created for in this repository.
        let branch_metadata = match branch::metadata(link.clone(), link_branch).await {
            Err(err) if err.is_branch_not_found() && execution_context().globals().dry_run() => {
                branch::metadata(repository.clone(), link_branch).await
            }
            metadata => metadata,
        }
        .forward_with::<LinkError, _>(|| link::unreachable(link.id))?;
        let branch_name = branch::name(&branch_metadata)
            .forward::<LinkError>("Failed getting branch metadata")?;

        create_link_mount(
            &operation,
            chain.innermost.clone(),
            remainder_parent,
            &clone_path,
            mount,
        )
        .await?;

        lore_debug!("Staging link node");
        let node = Node {
            flags: NodeFlags::Link.bits(),
            child: link_target,
            address: Address {
                hash: link_revision,
                context: link.id.into(),
            },
            ..Default::default()
        };
        let link_node = stage::stage_single_node(
            inner_repository.clone(),
            inner_state.clone(),
            remainder_path.clone().freeze(),
            node,
            Arc::default(),
            None, // No link tracking when adding links
            FilterMode::Full,
        )
        .await
        .forward::<LinkError>("Failed staging the link node")?;

        inner_state
            .link_add(
                inner_repository.clone(),
                link.id,
                stored_branch,
                link_revision,
                link_node.node,
                link_flags,
            )
            .await
            .forward::<LinkError>("Failed to add link")?;

        // Clone the link in the path
        lore_debug!("Connecting remote storage");
        let correlation_id = execution_context().globals().correlation_id.to_string();
        let storage = link_remote
            .session(link.id, &correlation_id)
            .await
            .forward_with::<LinkError, _>(|| {
                format!("Failed to open a session on linked repository {}", link.id)
            })?;

        lore_debug!("Clone link in {}", link_path);

        event::LoreEvent::RepositoryCloneBegin(LoreRepositoryCloneBeginEventData {
            repository: link.id,
            branch: branch_name.into(),
            revision: link_state.revision(),
            path: repository.require_path()?.into(),
        })
        .send();

        let stats = Arc::new(CloneStats::default());
        let clone_states = link.filter.mount_states(&clone_path);
        let clone_ctx = CloneContext {
            repository: link.clone(),
            state: link_state,
            operation: operation.clone(),
            options: Arc::default(),
            stats: stats.clone(),
            modified_times: operation.modified_times(),
        };
        clone::clone_node(clone_ctx, storage, clone_path, link_target, clone_states)
            .await
            .forward_with::<LinkError, _>(|| {
                format!("Failed cloning linked repository {}", link.id)
            })?;

        event::LoreEvent::RepositoryCloneEnd(LoreRepositoryCloneEndEventData {
            branch: branch_name.into(),
            revision: link_revision,
            count: LoreRepositoryCloneCountData::new(&stats),
        })
        .send();

        // Fold nested link revisions up into the top-level state (no-op if flat).
        link::propagate_link_chain(&chain, token, link::LinkPinRealize::StateOnly).await?;

        state_staged.reparent_onto(state_current.revision());

        // Serialize the staged state
        let signature = state_staged
            .serialize(repository.clone(), token)
            .await
            .forward::<LinkError>("Failed to serialize state")?;

        crate::instance::store_staged_anchor(&repository, signature)
            .await
            .forward::<LinkError>("Failed to serialize anchor")?;
        operation.modified_times().store(repository.clone()).await;
        Ok(())
    })
    .await?;

    event::LoreEvent::LinkChange(LoreLinkChangeEventData::new(
        link_path.as_str(),
        link.id,
        link_branch,
        link_revision,
        LoreFileAction::Add,
    ))
    .send();

    Ok(())
}

/// The repository `link_identifier` names: a URL, or a bare name or ID.
///
/// Only a scheme tells a URL from a bare name or ID, since `is_valid_name` permits scoped names
/// like `org/project`. Every link is fetched through this repository's remote, so the identifier
/// is resolved there, and a URL naming another server is refused. Takes the identifier by value so
/// that it is not held across the rest of [`add`].
async fn resolve_identifier(
    repository: &RepositoryContext,
    link_identifier: String,
) -> Result<RepositoryData, LinkError> {
    let parent_remote = repository::load_repository_config_from_dot_dir(repository.dot_dir_path()?)
        .forward::<LinkError>("reading the repository remote")?
        .remote_url
        .unwrap_or_default();
    if parent_remote.is_empty() {
        return Err(LinkError::from(crate::errors::NoRemote));
    }
    let url_name;
    let name = if link_identifier.contains("://") {
        let (remote_url, name) = repository::parse_url(&link_identifier, false)
            .forward_with::<LinkError, _>(|| {
                format!("Invalid repository URL or ID: {link_identifier}")
            })?;
        if !same_server(&remote_url, &parent_remote) {
            return Err(InvalidArguments {
                reason: format!(
                    "{link_identifier} is not on this repository's remote ({parent_remote}); a link must be on the same remote"
                ),
            }
            .into());
        }
        url_name = name;
        &url_name
    } else {
        &link_identifier
    };

    let context = execution_context();
    let identity = context.globals().identity().unwrap_or_default();
    repository::resolve_by_name(&parent_remote, name, identity)
        .await
        .forward_with::<LinkError, _>(|| format!("Repository not found: {link_identifier}"))
}

/// What the filesystem holds at and above the mount point `clone_path`.
#[derive(Clone, Copy)]
struct LinkMount {
    mount_exists: bool,
    parent_exists: bool,
}

/// Refuses a mount point holding a file or a directory with children, which the link would
/// displace, and one below a file, which leaves no directory to create.
async fn check_link_mount(
    operation: &Arc<InstanceOperationImpl>,
    clone_path: &RelativePath,
) -> Result<LinkMount, LinkError> {
    let check_failed = || format!("Failed to check link path {clone_path}");
    match operation
        .file_info(clone_path)
        .await
        .forward_with::<LinkError, _>(check_failed)?
    {
        FileInfo::NotExist => {
            let parent = clone_path.parent_path();
            let mut ancestor = parent.clone();
            while !ancestor.is_empty() {
                match operation
                    .file_info(&ancestor)
                    .await
                    .forward_with::<LinkError, _>(check_failed)?
                {
                    FileInfo::NotExist => ancestor = ancestor.parent_path(),
                    FileInfo::Directory => break,
                    FileInfo::File { .. } => {
                        return Err(InvalidArguments {
                            reason: format!(
                                "mount path {} is below the file {}",
                                link::display_path(clone_path.as_str()),
                                link::display_path(ancestor.as_str()),
                            ),
                        }
                        .into());
                    }
                }
            }
            Ok(LinkMount {
                mount_exists: false,
                parent_exists: ancestor == parent,
            })
        }
        FileInfo::Directory => {
            let mut entries = operation
                .read_directory(clone_path)
                .await
                .forward_with::<LinkError, _>(check_failed)?;
            if entries
                .next()
                .await
                .transpose()
                .forward_with::<LinkError, _>(check_failed)?
                .is_some()
            {
                return Err(mount_refused("already has children", clone_path));
            }
            Ok(LinkMount {
                mount_exists: true,
                parent_exists: true,
            })
        }
        FileInfo::File { .. } => Err(mount_refused("is a file", clone_path)),
    }
}

/// The refusal of the mount point `clone_path`, which `reason` says is taken.
fn mount_refused(reason: &str, clone_path: &RelativePath) -> LinkError {
    InvalidArguments {
        reason: format!(
            "Link path {reason} {}",
            link::display_path(clone_path.as_str())
        ),
    }
    .into()
}

/// Creates the directory the link mounts at and the one holding it, as [`check_link_mount`] found
/// them missing, and stages the intermediate path against the innermost repository.
///
/// One operation covers all of it: the mount point, the directory the link is placed in, the path
/// staged against the innermost repository, and the mount directory itself are in the same
/// filesystem.
async fn create_link_mount(
    operation: &Arc<InstanceOperationImpl>,
    innermost: NodeMapping,
    remainder_parent: RelativePathBuf,
    clone_path: &RelativePath,
    mount: LinkMount,
) -> Result<(), LinkError> {
    let parent_path = clone_path.parent_path();
    if !mount.parent_exists {
        lore_debug!("Creating directory {parent_path}");
        operation
            .create_dir_all(&parent_path)
            .await
            .forward_with::<LinkError, _>(|| format!("Failed to create directory {parent_path}"))?;
    }
    if !remainder_parent.is_empty() {
        lore_debug!("Staging link parent path in innermost repository");
        Box::pin(stage::stage_filesystem_path(
            operation.clone(),
            innermost,
            remainder_parent.freeze(),
            KnownChild::Unresolved,
            Arc::default(),
            StageOptions {
                no_children: true,
                ..Default::default()
            },
            None, // No link tracking when adding links
            None, // No layer mask
            None, // Prefixes resolved for the outer repository do not apply
            None, // Node ids here index the inner repository's own state
        ))
        .await
        .forward::<LinkError>("Failed staging the link node")?;
    }

    if !mount.mount_exists {
        lore_debug!("Creating directory {clone_path}");
        operation
            .create_dir_all(clone_path)
            .await
            .forward_with::<LinkError, _>(|| format!("Failed to create directory {clone_path}"))?;
    }

    Ok(())
}

/// The pin a new mount of a repository the parent already mounts takes, on the branch `row`, one
/// of its rows, resolves to: the committed revision its mounts are built on. Refuses a `pin`
/// resolving to another revision, and `disable_branching` where its mounts follow branches.
async fn mounted_pin(
    link: &Arc<RepositoryContext>,
    row: LinkReference,
    pin: Option<String>,
    disable_branching: bool,
    parent_branch: BranchId,
) -> Result<(Hash, BranchId), LinkError> {
    if disable_branching && row.auto_follows() {
        return Err(InvalidArguments {
            reason: format!(
                "linked repository {} is already mounted following branches; add this mount \
                 without --disable-branching",
                link.id,
            ),
        }
        .into());
    }
    let revision = State::deserialize(link.clone(), row.signature())
        .await
        .forward_with::<LinkError, _>(|| link::unreachable(link.id))?
        .committed_revision();
    if let Some(pin) = pin {
        let (pinned, _) = link::resolve_pin(link.clone(), pin)
            .await
            .forward_with::<LinkError, _>(|| link::unreachable(link.id))?;
        if pinned != revision {
            return Err(InvalidArguments {
                reason: format!(
                    "linked repository {} is already mounted at {revision}; every mount of it \
                     pins the same revision",
                    link.id
                ),
            }
            .into());
        }
    }
    Ok((revision, row.resolve_branch(parent_branch)))
}

async fn default_branch(link: &Arc<RepositoryContext>) -> Result<BranchId, LinkError> {
    let metadata_failed = || format!("Failed to load metadata of linked repository {}", link.id);
    let hash = match repository::metadata_hash(link.clone()).await {
        Ok(hash) => hash,
        Err(err) => {
            // `metadata_hash` reports any failure of its remote query as the local miss, which
            // hides a refusal of the caller's credentials.
            let answer = async {
                link.remote()
                    .await?
                    .repository()
                    .await?
                    .query(Some(link.id), None)
                    .await
            }
            .await;
            return match answer {
                Err(refusal) if is_refusal(&refusal) => {
                    Err(refusal).forward_with::<LinkError, _>(metadata_failed)
                }
                _ => Err(err).forward_with::<LinkError, _>(metadata_failed),
            };
        }
    };
    let metadata = repository::metadata(link.clone(), hash)
        .await
        .forward_with::<LinkError, _>(metadata_failed)?;
    Ok(metadata.default_branch)
}

/// How `link add` treats the branch of this repository in the linked repository.
#[derive(Clone, Copy)]
enum LinkBranch {
    /// The link pins a revision without following branches.
    Off,
    /// The linked repository holds the branch, at the given latest revision.
    Reuse(Hash),
    /// The linked repository lacks the branch, created at `parent_latest` of `parent`.
    /// `follows_parent` where the link pins `parent_latest` rather than a pin given.
    Create {
        parent: BranchId,
        parent_latest: Hash,
        follows_parent: bool,
    },
}

/// The linked repository's default branch and its latest revision on the remote.
async fn default_branch_latest(
    link: &Arc<RepositoryContext>,
    link_remote: &Arc<Connection>,
) -> Result<(Hash, BranchId), LinkError> {
    let default_branch_id = default_branch(link).await?;
    let latest = branch::load_remote_latest(link_remote.clone(), link.id, default_branch_id)
        .await
        .forward_with::<LinkError, _>(|| link::unreachable(link.id))?;
    Ok((latest, default_branch_id))
}

/// Creates the branch `branch_id` of this repository in the linked repository `link` at
/// `parent_latest` of `parent`, or reuses the one it holds.
async fn create_link_branch(
    repository: &Arc<RepositoryContext>,
    link: &Arc<RepositoryContext>,
    link_remote: Arc<Connection>,
    branch_id: BranchId,
    parent: BranchId,
    parent_latest: Hash,
) -> Result<link::LinkBranchOutcome, LinkError> {
    let branch_metadata = branch::metadata(repository.clone(), branch_id)
        .await
        .forward::<LinkError>("Failed getting branch metadata")?;
    let branch_name =
        branch::name(&branch_metadata).forward::<LinkError>("Failed getting branch metadata")?;
    let branch_category = branch::category(&branch_metadata).unwrap_or_default();

    let outcome = link::create_branch(
        link.clone(),
        link_remote,
        branch_id,
        branch_name,
        branch_category,
        parent,
        parent_latest,
    )
    .await?;
    lore_debug!(
        "Created branch {branch_id} at LATEST ({}) in linked repo",
        outcome.revision
    );
    Ok(outcome)
}

/// The refusal of a `source_path` that `revision` of the linked repository does not hold.
async fn source_path_not_found(
    link: &Arc<RepositoryContext>,
    link_label: String,
    source_path: &RelativePath,
    revision: Hash,
    branch: BranchId,
) -> FileNotFound {
    FileNotFound {
        resource: format!(
            "{} in linked repository {link_label} at revision {revision} on branch {}; it may not \
             be committed and pushed there yet",
            link::display_path(source_path.as_str()),
            branch::display_name(link.clone(), branch).await,
        ),
    }
}
