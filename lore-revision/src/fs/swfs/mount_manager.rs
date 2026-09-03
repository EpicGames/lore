use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use lore_base::error::InvalidPath;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use lore_error_set::error_set;

use crate::global::external_dir::check_for_external_lore_dir;
use crate::global::external_dir::external_dir_for_instance;
use crate::instance::InstanceId;
use crate::instance::list_instances;
use crate::repository::DOT_LORE;
use crate::repository::load_repository_config_from_dot_dir;
use crate::shared_store::registry::SharedStoreRegistry;
use crate::util::config::SaveableConfig;

pub struct Mount {
    instance_id: InstanceId,
}

#[error_set]
pub enum MountManagerError {
    InvalidPath,
}

pub struct MountManager {
    mounts: DashMap<PathBuf, Mount>,
}

impl MountManager {
    pub async fn initialize() -> Result<Arc<Self>, MountManagerError> {
        let mounts = DashMap::new();
        let registry = SharedStoreRegistry::load()
            .await
            .forward::<MountManagerError>("Unable to load shared store registry")?;
        for entry in registry.entries() {
            let entry_repository = entry
                .create_null_repository_context()
                .await
                .internal("Unable to create null context for entry")?;
            for instance in &list_instances(&entry_repository)
                .await
                .internal("Unable to find instances for entry")?
            {
                if let Some(dot_lore) =
                    check_for_external_lore_dir(instance.instance_id)
                        .forward::<MountManagerError>("Unable to find .lore directory")?
                {
                    let config = load_repository_config_from_dot_dir(&dot_lore)
                        .internal("Unable to load repository config")?;
                    if config.vfs.unwrap_or_default().vfs_type.is_swfs() {
                        mounts.insert(
                            Self::canonicalize_repo_path(Path::new(&instance.path))?,
                            Mount {
                                instance_id: instance.instance_id,
                            },
                        );
                    }
                }
            }
        }
        Ok(Arc::new(MountManager { mounts }))
    }

    pub fn check_for_external_lore_dir(
        &self,
        repository_path: &Path,
    ) -> Result<Option<PathBuf>, MountManagerError> {
        let canonical_repository_path = Self::canonicalize_repo_path(repository_path)?;
        if let Some(mount) = self.mounts.get(&canonical_repository_path) {
            check_for_external_lore_dir(mount.instance_id).forward::<MountManagerError>(
                "Unable to find external lore directory for expected instance ID",
            )
        } else {
            Ok(None)
        }
    }

    pub fn create_mount(
        &self,
        repository_path: &Path,
        instance_id: InstanceId,
    ) -> Result<PathBuf, MountManagerError> {
        let canonical_repository_path = Self::canonicalize_repo_path(repository_path)?;
        if let Some(_mount) = self.mounts.get(&canonical_repository_path) {
            Err(MountManagerError::internal(format!(
                "Mount already exists for path {}",
                repository_path.display()
            )))
        } else {
            let dot_lore = external_dir_for_instance(instance_id)
                .forward::<MountManagerError>("Unable to get external directory")?
                .join(DOT_LORE);
            let existing = self
                .mounts
                .insert(canonical_repository_path, Mount { instance_id });
            if let Some(_existing) = existing {
                Err(MountManagerError::internal(format!(
                    "Mount double created for path {}",
                    repository_path.display()
                )))
            } else {
                Ok(dot_lore)
            }
        }
    }

    /// Canonicalizes the `repo_path`, requiring that any parent of it exist.
    fn canonicalize_repo_path(repo_path: &Path) -> Result<PathBuf, InvalidPath> {
        let file_name = repo_path.file_name().ok_or_else(|| InvalidPath {
            path: format!("{}", repo_path.display()),
        })?;
        let combined = if let Some(parent) = repo_path.parent() {
            parent
                .canonicalize()
                .map_err(|_err| InvalidPath {
                    path: format!("{}", repo_path.display()),
                })?
                .join(file_name)
        } else {
            PathBuf::from(file_name)
        };
        Ok(combined)
    }
}
