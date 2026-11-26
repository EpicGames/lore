use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_error_set::ForwardStrict;
use lore_error_set::error_set;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use serde::Deserialize;
use serde::Serialize;

use crate::global::get_global_data_dir;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryError;
use crate::repository::claim_opened_stores;
use crate::repository::create_immutable_store_at_path;
use crate::repository::create_mutable_store_at_path;
use crate::shared_store::SHARED_STORE_DIR;
use crate::shared_store::SHARED_STORES_DIR;
use crate::state::allow_all_repositories;
use crate::util::config::SaveableConfig;

#[error_set]
pub enum SharedStoreRegistryError {}

pub const REGISTRY: &str = "registry.toml";

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SharedStoreRegistry {
    entries: Vec<SharedStoreRegistryEntry>,
}

impl SharedStoreRegistry {
    pub fn register(
        &mut self,
        remote_url: String,
        path: &Path,
    ) -> Result<(), SharedStoreRegistryError> {
        let cleaned_path = crate::util::path::clean(path.display().to_string());
        for entry in &self.entries {
            if entry.path == cleaned_path {
                return Ok(());
            }
        }
        self.entries.push(SharedStoreRegistryEntry {
            remote_url,
            path: cleaned_path,
        });
        Ok(())
    }

    pub fn entries(&self) -> &[SharedStoreRegistryEntry] {
        &self.entries
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SharedStoreRegistryEntry {
    remote_url: String,
    path: String,
}

impl SharedStoreRegistryEntry {
    pub fn remote_url(&self) -> &str {
        &self.remote_url
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The directory of this shared store's stores.
    pub fn store_dir(&self) -> PathBuf {
        Path::new(&self.path).join(SHARED_STORE_DIR)
    }

    /// A context on the zero repository over this shared store's stores, which it claims for
    /// its lifetime as a command does.
    ///
    /// # Errors
    ///
    /// [`RepositoryError`] if the stores cannot be opened or claimed.
    pub async fn create_null_repository_context(
        &self,
    ) -> Result<Arc<RepositoryContext>, RepositoryError> {
        let path_buf = self.store_dir();
        let (immutable_store, opening) = create_immutable_store_at_path(
            path_buf.clone(),
            ImmutableStoreCreateOptions::none(),
            false,
        )
        .await?;
        let (immutable_store, mutable_store, stores) =
            claim_opened_stores(immutable_store, opening, |immutable_store| {
                create_mutable_store_at_path(path_buf, immutable_store)
            })
            .await?;
        Ok(Arc::new(
            RepositoryContext::new_null_context(
                immutable_store,
                mutable_store,
                allow_all_repositories(),
            )
            .with_store_hold(stores),
        ))
    }

    /// The entry of the shared store in the directory `dir`, which holds its `shared_store`
    /// directory, with no remote named.
    pub fn in_directory(dir: String) -> SharedStoreRegistryEntry {
        SharedStoreRegistryEntry {
            remote_url: String::new(),
            path: dir,
        }
    }
}

impl SaveableConfig for SharedStoreRegistry {
    type ErrorType = SharedStoreRegistryError;

    fn file_location() -> Result<PathBuf, Self::ErrorType> {
        get_global_data_dir()
            .forward::<SharedStoreRegistryError>("Unable to get global data dir")
            .map(|path| path.join(SHARED_STORES_DIR).join(REGISTRY))
    }
}
