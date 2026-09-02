use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_error_set::ForwardStrict;
use lore_error_set::error_set;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;
use serde::Deserialize;
use serde::Serialize;

use crate::global::get_global_data_dir;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryError;
use crate::repository::create_immutable_store_at_path;
use crate::repository::create_mutable_store_at_path;
use crate::shared_store::SHARED_STORE_DIR;
use crate::shared_store::SHARED_STORES_DIR;
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

    pub async fn create_stores(
        &self,
    ) -> Result<(Arc<dyn ImmutableStore>, Arc<dyn MutableStore>), RepositoryError> {
        let path_buf = PathBuf::from(&self.path).join(SHARED_STORE_DIR);
        let immutable_store = create_immutable_store_at_path(
            path_buf.clone(),
            ImmutableStoreCreateOptions::none(),
            false,
        )
        .await?;
        let mutable_store = create_mutable_store_at_path(path_buf, immutable_store.clone()).await?;
        Ok((immutable_store, mutable_store))
    }

    pub async fn create_null_repository_context(
        &self,
    ) -> Result<Arc<RepositoryContext>, RepositoryError> {
        let (immutable_store, mutable_store) = self.create_stores().await?;
        Ok(Arc::new(RepositoryContext::new_null_context(
            immutable_store,
            mutable_store,
        )))
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
