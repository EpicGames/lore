use std::sync::Arc;
use std::sync::LazyLock;

use parking_lot::RwLock;

use crate::fs::swfs::mount_manager::MountManager;
use crate::fs::swfs::mount_manager::MountManagerError;

static MOUNT_MANAGER_STATE: LazyLock<RwLock<MountManagerState>> =
    LazyLock::new(|| RwLock::new(MountManagerState::make_global()));

pub struct MountManagerState {
    manager: Option<Arc<MountManager>>,
}

impl MountManagerState {
    fn make_global() -> Self {
        Self { manager: None }
    }

    /// Only meant to be called from a single place during startup so that `MountManager` isn't
    /// created twice if two threads run this concurrently.
    pub async fn initialize() -> Result<(), MountManagerError> {
        {
            let manager = MOUNT_MANAGER_STATE.read();
            if manager.manager.is_some() {
                return Err(MountManagerError::internal(
                    "Double-initializing the mount manager",
                ));
            }
        }
        let manager = MountManager::initialize().await?;
        MOUNT_MANAGER_STATE.write().manager = Some(manager);
        Ok(())
    }

    pub fn mount_manager() -> Option<Arc<MountManager>> {
        MOUNT_MANAGER_STATE.read().manager.clone()
    }
}
