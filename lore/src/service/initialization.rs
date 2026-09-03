use lore_base::error::InvalidPath;
use lore_error_set::ForwardStrict;
use lore_error_set::error_set;
use lore_revision::fs::swfs::mount_manager_state::MountManagerState;

#[error_set]
pub enum ServiceInitializationError {
    InvalidPath,
}

pub async fn initialize_service() -> Result<(), ServiceInitializationError> {
    MountManagerState::initialize()
        .await
        .forward::<ServiceInitializationError>("Failed initializing SWFS repositories")?;
    Ok(())
}
