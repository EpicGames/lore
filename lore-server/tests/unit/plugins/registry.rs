// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::AddressNotFound;
use lore_base::error::PluginConfigError;
use lore_base::error::PluginInitError;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_base::types::LockData;
use lore_base::types::LockResource;
use lore_base::types::Partition;
use lore_revision::cluster::peer::PeerInfo;
use lore_revision::cluster::topology::RefreshLoopError;
use lore_revision::cluster::topology::Topology;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_server::plugins::registry::*;
use lore_server::plugins::traits::ImmutableStorePluginFactory;
use lore_server::plugins::traits::LockStorePluginFactory;
use lore_server::plugins::traits::MutableStorePluginFactory;
use lore_server::plugins::traits::NotificationPlugin;
use lore_server::plugins::traits::NotificationPluginContext;
use lore_server::plugins::traits::NotificationPluginFactory;
use lore_server::plugins::traits::PluginError;
use lore_server::plugins::traits::TopologyPluginFactory;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::KeyValueStream;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use opentelemetry_sdk::resource::ResourceDetector;
use tokio::runtime::Handle;
use tokio::sync::broadcast::Receiver;

/// Mock resource detector for testing detector registration.
struct MockResourceDetector;

impl ResourceDetector for MockResourceDetector {
    fn detect(&self) -> opentelemetry_sdk::resource::Resource {
        opentelemetry_sdk::resource::Resource::builder_empty().build()
    }
}

/// Mock immutable store for testing
struct MockImmutableStore;

#[async_trait]
impl ImmutableStore for MockImmutableStore {
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    async fn query(
        self: Arc<Self>,
        _partition: Partition,
        _addresses: &[Address],
        _results: &mut [lore_storage::StoreMatchResult],
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn get_metadata(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
    ) -> Result<StoreGetData, StoreError> {
        Ok(StoreGetData::default())
    }

    async fn get(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
    ) -> Result<StoreGetData, StoreError> {
        Err(StoreError::from(AddressNotFound::from(Address::default())))
    }

    async fn put(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
        _fragment: Fragment,
        _payload: Option<Bytes>,
        _force: bool,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn obliterate(
        self: Arc<Self>,
        _partition: Partition,
        _address: Address,
        _stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn evict(
        self: Arc<Self>,
        _max_capacity: usize,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        Ok(0)
    }

    async fn compact(
        self: Arc<Self>,
        _max_size: usize,
        _at: Option<usize>,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        Ok(None)
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        None
    }

    fn max_query_batch(&self) -> Option<usize> {
        None
    }

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
        Ok(())
    }

    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        Ok(())
    }

    async fn copy(
        self: Arc<Self>,
        _source_partition: Partition,
        _source_address: Address,
        _destination_partition: Partition,
        _destination_context: Context,
        _behavior: CopyBehavior,
    ) -> Result<(), StoreError> {
        Err(StoreError::internal("Copy not supported by this store"))
    }
}

/// Mock mutable store for testing
struct MockMutableStore;

#[async_trait]
impl MutableStore for MockMutableStore {
    /// Nothing on disk to claim.
    async fn hold_for_command(
        self: std::sync::Arc<Self>,
    ) -> std::result::Result<
        Option<lore_storage::local::store_lock::StoreGuard>,
        lore_storage::StoreError,
    > {
        Ok(None)
    }

    async fn load(
        self: Arc<Self>,
        _partition: Partition,
        _key: Hash,
        _key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        Err(StoreError::from(AddressNotFound::from(Address::default())))
    }

    async fn store(
        self: Arc<Self>,
        _partition: Partition,
        _key: Hash,
        _value: Hash,
        _key_type: KeyType,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        _partition: Partition,
        _key: Hash,
        _expected: Hash,
        _value: Hash,
        _key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        Ok(Hash::default())
    }

    async fn list(
        self: Arc<Self>,
        _partition: Partition,
        _key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        Err(StoreError::internal("Not implemented"))
    }

    async fn flush(self: Arc<Self>, _options: FlushOptions) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Mock topology for testing
#[derive(Debug)]
struct MockTopology {
    sender: tokio::sync::broadcast::Sender<HashSet<PeerInfo>>,
}

impl MockTopology {
    fn new() -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(1);
        Self { sender }
    }
}

#[async_trait]
impl Topology for MockTopology {
    fn supports_refresh_loop(&self) -> bool {
        false
    }

    async fn refresh_loop(self: Arc<Self>) -> Result<(), RefreshLoopError> {
        Err(RefreshLoopError::internal("not supported"))
    }

    fn subscribe_to_peer_refreshes(self: Arc<Self>) -> Receiver<HashSet<PeerInfo>> {
        self.sender.subscribe()
    }
}

/// Mock lock store for testing
struct MockLockStore;

#[async_trait]
impl LockStore for MockLockStore {
    async fn lock_resources(
        &self,
        _owner_id: &str,
        _repository: RepositoryId,
        _resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        Ok(vec![])
    }

    async fn query_locks(&self, _query: LockQuery) -> Result<Vec<LockData>, LockError> {
        Ok(vec![])
    }

    async fn check_locks_status(
        &self,
        _repository: RepositoryId,
        _resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        Ok(vec![])
    }

    async fn unlock_resources(
        &self,
        _owner_id: &str,
        _validate_user: bool,
        _repository: RepositoryId,
        _resources: &[LockResource],
    ) -> Result<Vec<LockResource>, LockError> {
        Ok(vec![])
    }
}

struct MockImmutableStoreFactory {
    name: &'static str,
    should_fail_config: bool,
    should_fail_init: bool,
}

impl MockImmutableStoreFactory {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            should_fail_config: false,
            should_fail_init: false,
        }
    }

    fn with_config_error(mut self) -> Self {
        self.should_fail_config = true;
        self
    }

    fn with_init_error(mut self) -> Self {
        self.should_fail_init = true;
        self
    }
}

impl ImmutableStorePluginFactory for MockImmutableStoreFactory {
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        Ok(())
    }

    fn create(&self, config: &toml::Value) -> Result<Arc<dyn ImmutableStore>, PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        if self.should_fail_init {
            return Err(PluginInitError {
                plugin_name: self.name.to_string(),
                message: "Failed to initialize".to_string(),
            }
            .into());
        }
        Ok(Arc::new(MockImmutableStore))
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct MockNotificationPluginFactory {
    name: &'static str,
    should_fail_config: bool,
    should_fail_init: bool,
}

impl MockNotificationPluginFactory {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            should_fail_config: false,
            should_fail_init: false,
        }
    }

    fn with_config_error(mut self) -> Self {
        self.should_fail_config = true;
        self
    }

    fn with_init_error(mut self) -> Self {
        self.should_fail_init = true;
        self
    }
}

struct MockNotificationSender;

#[async_trait]
impl lore_revision::notification::NotificationSender for MockNotificationSender {
    async fn branch_created(&self, _repository: RepositoryId, _branch: BranchId) {}
    async fn branch_pushed(
        &self,
        _repository: RepositoryId,
        _branch: BranchId,
        _user_id: &str,
        _revision: Hash,
        _revision_number: u64,
    ) {
    }
    async fn branch_deleted(&self, _repository: RepositoryId, _branch: BranchId) {}
    async fn resource_locked(
        &self,
        _repository: RepositoryId,
        _branch: BranchId,
        _user_id: &str,
        _resources: &[LockResource],
    ) {
    }
    async fn resource_unlocked(
        &self,
        _repository: RepositoryId,
        _branch: BranchId,
        _user_id: &str,
        _resources: &[LockResource],
    ) {
    }
    async fn obliterate(
        &self,
        _repository: RepositoryId,
        _address: Address,
    ) -> Result<(), lore_revision::notification::NotificationError> {
        Ok(())
    }
    async fn compliance_check(
        &self,
        _stream_name: &str,
        _repository: RepositoryId,
        _branch: BranchId,
        _user_id: &str,
        _revision: Hash,
        _revision_number: u64,
        _ip_addr: Option<String>,
    ) {
    }
}

#[async_trait]
impl NotificationPluginFactory for MockNotificationPluginFactory {
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: "mock_notification".to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        Ok(())
    }

    async fn create(
        &self,
        config: &toml::Value,
        _context: &NotificationPluginContext,
    ) -> Result<NotificationPlugin, PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: "mock_notification".to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        if self.should_fail_init {
            return Err(PluginInitError {
                plugin_name: "mock_notification".to_string(),
                message: "Failed to initialize".to_string(),
            }
            .into());
        }
        Ok(NotificationPlugin {
            sender: Arc::new(MockNotificationSender),
            receivers: vec![],
        })
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct MockMutableStoreFactory {
    name: &'static str,
    should_fail_config: bool,
    should_fail_init: bool,
}

impl MockMutableStoreFactory {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            should_fail_config: false,
            should_fail_init: false,
        }
    }

    fn with_config_error(mut self) -> Self {
        self.should_fail_config = true;
        self
    }

    fn with_init_error(mut self) -> Self {
        self.should_fail_init = true;
        self
    }
}

impl MutableStorePluginFactory for MockMutableStoreFactory {
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        Ok(())
    }

    fn create(
        &self,
        config: &toml::Value,
        _immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<Arc<dyn MutableStore>, PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        if self.should_fail_init {
            return Err(PluginInitError {
                plugin_name: self.name.to_string(),
                message: "Failed to initialize".to_string(),
            }
            .into());
        }
        Ok(Arc::new(MockMutableStore))
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct MockTopologyFactory {
    name: &'static str,
    should_fail_config: bool,
    should_fail_init: bool,
}

impl MockTopologyFactory {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            should_fail_config: false,
            should_fail_init: false,
        }
    }

    fn with_config_error(mut self) -> Self {
        self.should_fail_config = true;
        self
    }

    fn with_init_error(mut self) -> Self {
        self.should_fail_init = true;
        self
    }
}

impl TopologyPluginFactory for MockTopologyFactory {
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        Ok(())
    }

    fn create(&self, config: &toml::Value) -> Result<Arc<dyn Topology + Send + Sync>, PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        if self.should_fail_init {
            return Err(PluginInitError {
                plugin_name: self.name.to_string(),
                message: "Failed to initialize".to_string(),
            }
            .into());
        }
        Ok(Arc::new(MockTopology::new()))
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct MockLockStoreFactory {
    name: &'static str,
    should_fail_config: bool,
    should_fail_init: bool,
}

impl MockLockStoreFactory {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            should_fail_config: false,
            should_fail_init: false,
        }
    }

    fn with_config_error(mut self) -> Self {
        self.should_fail_config = true;
        self
    }

    fn with_init_error(mut self) -> Self {
        self.should_fail_init = true;
        self
    }
}

impl LockStorePluginFactory for MockLockStoreFactory {
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        Ok(())
    }

    fn create(&self, config: &toml::Value) -> Result<Arc<dyn LockStore>, PluginError> {
        if self.should_fail_config {
            return Err(PluginConfigError {
                plugin_name: self.name.to_string(),
                message: format!("Invalid config: {config:?}"),
            }
            .into());
        }
        if self.should_fail_init {
            return Err(PluginInitError {
                plugin_name: self.name.to_string(),
                message: "Failed to initialize".to_string(),
            }
            .into());
        }
        Ok(Arc::new(MockLockStore))
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

#[tokio::test]
async fn test_resource_detectors_empty_without_registered_detectors() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("mock")));

    // Registering a store plugin does not register a detector; detectors are
    // contributed only by explicit `register_resource_detector` calls.
    assert!(registry.resource_detectors(Handle::current()).is_empty());
}

#[tokio::test]
async fn test_register_resource_detector_collects_each_registered_detector() {
    let mut registry = PluginRegistry::new();
    registry.register_resource_detector(|_handle| {
        Box::new(MockResourceDetector) as Box<dyn ResourceDetector>
    });
    registry.register_resource_detector(|_handle| {
        Box::new(MockResourceDetector) as Box<dyn ResourceDetector>
    });

    assert_eq!(registry.resource_detectors(Handle::current()).len(), 2);
}

#[tokio::test]
async fn test_aws_and_hashicorp_modules_each_register_one_resource_detector() {
    let mut registry = PluginRegistry::new();
    lore_server::plugins::aws::register(&mut registry);
    lore_server::plugins::hashicorp::register(&mut registry);

    let detectors = registry.resource_detectors(Handle::current());

    // The AWS module registers a single detector (despite registering three
    // store factories); the HashiCorp module registers the Nomad detector.
    // Total: two.
    assert_eq!(detectors.len(), 2);
}

#[test]
fn test_register_immutable_store_plugin() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("test")));

    let plugins = registry.list_immutable_store_plugins();
    assert_eq!(plugins.len(), 1);
    assert!(plugins.contains(&"test".to_string()));
}

#[test]
fn test_register_multiple_immutable_store_plugins() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("plugin1")));
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("plugin2")));

    let plugins = registry.list_immutable_store_plugins();
    assert_eq!(plugins.len(), 2);
    assert!(plugins.contains(&"plugin1".to_string()));
    assert!(plugins.contains(&"plugin2".to_string()));
}

#[test]
#[should_panic(expected = "already registered")]
fn test_register_duplicate_immutable_store_plugin_panics() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("test")));
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("test")));
}

#[test]
fn test_create_immutable_store_success() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("test")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_immutable_store("test", &config);
    assert!(result.is_ok());
}

#[test]
fn test_create_immutable_store_not_found() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("other")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_immutable_store("missing", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let not_found = e.as_plugin_not_found().expect("should be PluginNotFound");
    assert_eq!(not_found.plugin_name, "missing");
    assert!(not_found.available_plugins.contains(&"other".to_string()));
}

#[test]
fn test_create_immutable_store_config_error() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(
        MockImmutableStoreFactory::new("test").with_config_error(),
    ));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_immutable_store("test", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let config_err = e
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "test");
}

#[test]
fn test_create_immutable_store_init_error() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(
        MockImmutableStoreFactory::new("test").with_init_error(),
    ));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_immutable_store("test", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let init_err = e.as_plugin_init_error().expect("should be PluginInitError");
    assert_eq!(init_err.plugin_name, "test");
}

#[test]
fn test_register_mutable_store_plugin() {
    let mut registry = PluginRegistry::new();
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("test")));

    let plugins = registry.list_mutable_store_plugins();
    assert_eq!(plugins.len(), 1);
    assert!(plugins.contains(&"test".to_string()));
}

#[test]
fn test_register_multiple_mutable_store_plugins() {
    let mut registry = PluginRegistry::new();
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("plugin1")));
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("plugin2")));

    let plugins = registry.list_mutable_store_plugins();
    assert_eq!(plugins.len(), 2);
    assert!(plugins.contains(&"plugin1".to_string()));
    assert!(plugins.contains(&"plugin2".to_string()));
}

#[test]
#[should_panic(expected = "already registered")]
fn test_register_duplicate_mutable_store_plugin_panics() {
    let mut registry = PluginRegistry::new();
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("test")));
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("test")));
}

#[test]
fn test_create_mutable_store_success() {
    let mut registry = PluginRegistry::new();
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("test")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_mutable_store("test", &config, Arc::new(MockImmutableStore));
    assert!(result.is_ok());
}

#[test]
fn test_create_mutable_store_not_found() {
    let mut registry = PluginRegistry::new();
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("other")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_mutable_store("missing", &config, Arc::new(MockImmutableStore));

    let Err(e) = result else {
        panic!("expected error");
    };
    let not_found = e.as_plugin_not_found().expect("should be PluginNotFound");
    assert_eq!(not_found.plugin_name, "missing");
    assert!(not_found.available_plugins.contains(&"other".to_string()));
}

#[test]
fn test_create_mutable_store_config_error() {
    let mut registry = PluginRegistry::new();
    registry.register_mutable_store_plugin(Box::new(
        MockMutableStoreFactory::new("test").with_config_error(),
    ));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_mutable_store("test", &config, Arc::new(MockImmutableStore));

    let Err(e) = result else {
        panic!("expected error");
    };
    let config_err = e
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "test");
}

#[test]
fn test_create_mutable_store_init_error() {
    let mut registry = PluginRegistry::new();
    registry.register_mutable_store_plugin(Box::new(
        MockMutableStoreFactory::new("test").with_init_error(),
    ));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_mutable_store("test", &config, Arc::new(MockImmutableStore));

    let Err(e) = result else {
        panic!("expected error");
    };
    let init_err = e.as_plugin_init_error().expect("should be PluginInitError");
    assert_eq!(init_err.plugin_name, "test");
}

#[test]
fn test_register_topology_plugin() {
    let mut registry = PluginRegistry::new();
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("test")));

    let plugins = registry.list_topology_plugins();
    assert_eq!(plugins.len(), 1);
    assert!(plugins.contains(&"test".to_string()));
}

#[test]
fn test_register_multiple_topology_plugins() {
    let mut registry = PluginRegistry::new();
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("plugin1")));
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("plugin2")));

    let plugins = registry.list_topology_plugins();
    assert_eq!(plugins.len(), 2);
    assert!(plugins.contains(&"plugin1".to_string()));
    assert!(plugins.contains(&"plugin2".to_string()));
}

#[test]
#[should_panic(expected = "already registered")]
fn test_register_duplicate_topology_plugin_panics() {
    let mut registry = PluginRegistry::new();
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("test")));
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("test")));
}

#[test]
fn test_create_topology_success() {
    let mut registry = PluginRegistry::new();
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("test")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_topology("test", &config);
    assert!(result.is_ok());
}

#[test]
fn test_create_topology_not_found() {
    let mut registry = PluginRegistry::new();
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("other")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_topology("missing", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let not_found = e.as_plugin_not_found().expect("should be PluginNotFound");
    assert_eq!(not_found.plugin_name, "missing");
    assert!(not_found.available_plugins.contains(&"other".to_string()));
}

#[test]
fn test_create_topology_config_error() {
    let mut registry = PluginRegistry::new();
    registry.register_topology_plugin(Box::new(
        MockTopologyFactory::new("test").with_config_error(),
    ));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_topology("test", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let config_err = e
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "test");
}

#[test]
fn test_create_topology_init_error() {
    let mut registry = PluginRegistry::new();
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("test").with_init_error()));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_topology("test", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let init_err = e.as_plugin_init_error().expect("should be PluginInitError");
    assert_eq!(init_err.plugin_name, "test");
}

#[test]
fn test_empty_registry() {
    let registry = PluginRegistry::new();
    assert!(registry.list_immutable_store_plugins().is_empty());
    assert!(registry.list_mutable_store_plugins().is_empty());
    assert!(registry.list_topology_plugins().is_empty());
    assert!(registry.list_lock_store_plugins().is_empty());
    assert!(registry.list_notification_plugins().is_empty());
}

#[test]
fn test_registry_debug() {
    let mut registry = PluginRegistry::new();
    registry.register_immutable_store_plugin(Box::new(MockImmutableStoreFactory::new("is1")));
    registry.register_mutable_store_plugin(Box::new(MockMutableStoreFactory::new("ms1")));
    registry.register_topology_plugin(Box::new(MockTopologyFactory::new("tp1")));
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("ls1")));
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("np1")));

    let debug_str = format!("{registry:?}");
    assert!(debug_str.contains("is1"));
    assert!(debug_str.contains("ms1"));
    assert!(debug_str.contains("tp1"));
    assert!(debug_str.contains("ls1"));
    assert!(debug_str.contains("np1"));
}

#[test]
fn test_list_empty_immutable_store_plugins() {
    let registry = PluginRegistry::new();
    assert!(registry.list_immutable_store_plugins().is_empty());
}

#[test]
fn test_list_empty_mutable_store_plugins() {
    let registry = PluginRegistry::new();
    assert!(registry.list_mutable_store_plugins().is_empty());
}

#[test]
fn test_list_empty_topology_plugins() {
    let registry = PluginRegistry::new();
    assert!(registry.list_topology_plugins().is_empty());
}

#[test]
fn test_register_lock_store_plugin() {
    let mut registry = PluginRegistry::new();
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("test")));

    let plugins = registry.list_lock_store_plugins();
    assert_eq!(plugins.len(), 1);
    assert!(plugins.contains(&"test".to_string()));
}

#[test]
fn test_register_multiple_lock_store_plugins() {
    let mut registry = PluginRegistry::new();
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("plugin1")));
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("plugin2")));

    let plugins = registry.list_lock_store_plugins();
    assert_eq!(plugins.len(), 2);
    assert!(plugins.contains(&"plugin1".to_string()));
    assert!(plugins.contains(&"plugin2".to_string()));
}

#[test]
#[should_panic(expected = "already registered")]
fn test_register_duplicate_lock_store_plugin_panics() {
    let mut registry = PluginRegistry::new();
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("test")));
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("test")));
}

#[test]
fn test_create_lock_store_success() {
    let mut registry = PluginRegistry::new();
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("test")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_lock_store("test", &config);
    assert!(result.is_ok());
}

#[test]
fn test_create_lock_store_not_found() {
    let mut registry = PluginRegistry::new();
    registry.register_lock_store_plugin(Box::new(MockLockStoreFactory::new("other")));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_lock_store("missing", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let not_found = e.as_plugin_not_found().expect("should be PluginNotFound");
    assert_eq!(not_found.plugin_name, "missing");
    assert!(not_found.available_plugins.contains(&"other".to_string()));
}

#[test]
fn test_create_lock_store_config_error() {
    let mut registry = PluginRegistry::new();
    registry.register_lock_store_plugin(Box::new(
        MockLockStoreFactory::new("test").with_config_error(),
    ));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_lock_store("test", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let config_err = e
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "test");
}

#[test]
fn test_create_lock_store_init_error() {
    let mut registry = PluginRegistry::new();
    registry.register_lock_store_plugin(Box::new(
        MockLockStoreFactory::new("test").with_init_error(),
    ));

    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.create_lock_store("test", &config);

    let Err(e) = result else {
        panic!("expected error");
    };
    let init_err = e.as_plugin_init_error().expect("should be PluginInitError");
    assert_eq!(init_err.plugin_name, "test");
}

#[test]
fn test_list_empty_lock_store_plugins() {
    let registry = PluginRegistry::new();
    assert!(registry.list_lock_store_plugins().is_empty());
}

// ========================================================================
// Notification plugin tests
// ========================================================================

#[test]
fn test_register_notification_plugin() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("test")));
    let plugins = registry.list_notification_plugins();
    assert_eq!(plugins.len(), 1);
    assert!(plugins.contains(&"test".to_string()));
}

#[test]
fn test_register_multiple_notification_plugins() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("plugin1")));
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("plugin2")));
    let plugins = registry.list_notification_plugins();
    assert_eq!(plugins.len(), 2);
    assert!(plugins.contains(&"plugin1".to_string()));
    assert!(plugins.contains(&"plugin2".to_string()));
}

#[test]
#[should_panic(expected = "already registered")]
fn test_register_duplicate_notification_plugin_panics() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("test")));
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("test")));
}

#[tokio::test]
async fn test_create_notification_success() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("test")));
    let config = toml::Value::Table(toml::map::Map::new());
    let context = NotificationPluginContext {
        environment: None,
        immutable_store: None,
    };
    let result = registry
        .create_notification("test", &config, &context)
        .await;
    assert!(result.is_ok());
    let output = result.expect("create_notification should succeed");
    assert!(output.receivers.is_empty());
}

#[tokio::test]
async fn test_create_notification_not_found() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("other")));
    let config = toml::Value::Table(toml::map::Map::new());
    let context = NotificationPluginContext {
        environment: None,
        immutable_store: None,
    };
    let result = registry
        .create_notification("missing", &config, &context)
        .await;
    let Err(e) = result else {
        panic!("expected error");
    };
    let not_found = e.as_plugin_not_found().expect("should be PluginNotFound");
    assert_eq!(not_found.plugin_name, "missing");
    assert!(not_found.available_plugins.contains(&"other".to_string()));
}

#[tokio::test]
async fn test_create_notification_config_error() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(
        MockNotificationPluginFactory::new("test").with_config_error(),
    ));
    let config = toml::Value::Table(toml::map::Map::new());
    let context = NotificationPluginContext {
        environment: None,
        immutable_store: None,
    };
    let result = registry
        .create_notification("test", &config, &context)
        .await;
    let Err(e) = result else {
        panic!("expected error");
    };
    let config_err = e
        .as_plugin_config_error()
        .expect("should be PluginConfigError");
    assert_eq!(config_err.plugin_name, "mock_notification");
}

#[tokio::test]
async fn test_create_notification_init_error() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(
        MockNotificationPluginFactory::new("test").with_init_error(),
    ));
    let config = toml::Value::Table(toml::map::Map::new());
    let context = NotificationPluginContext {
        environment: None,
        immutable_store: None,
    };
    let result = registry
        .create_notification("test", &config, &context)
        .await;
    let Err(e) = result else {
        panic!("expected error");
    };
    let init_err = e.as_plugin_init_error().expect("should be PluginInitError");
    assert_eq!(init_err.plugin_name, "mock_notification");
}

#[test]
fn test_validate_notification_config_success() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactory::new("test")));
    let config = toml::Value::Table(toml::map::Map::new());
    assert!(
        registry
            .validate_notification_config("test", &config)
            .is_ok()
    );
}

#[test]
fn test_validate_notification_config_not_found() {
    let registry = PluginRegistry::new();
    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.validate_notification_config("missing", &config);
    assert!(result.expect_err("should fail").is_plugin_not_found());
}

#[test]
fn test_validate_notification_config_error() {
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(
        MockNotificationPluginFactory::new("test").with_config_error(),
    ));
    let config = toml::Value::Table(toml::map::Map::new());
    let result = registry.validate_notification_config("test", &config);
    assert!(result.expect_err("should fail").is_plugin_config_error());
}

#[test]
fn test_list_empty_notification_plugins() {
    let registry = PluginRegistry::new();
    assert!(registry.list_notification_plugins().is_empty());
}

// ========================================================================
// NotificationPluginOutput with background tasks test
// ========================================================================

struct MockNotificationPluginFactoryWithTask {
    name: &'static str,
}

#[async_trait]
impl NotificationPluginFactory for MockNotificationPluginFactoryWithTask {
    fn validate_config(&self, _config: &toml::Value) -> Result<(), PluginError> {
        Ok(())
    }

    async fn create(
        &self,
        _config: &toml::Value,
        _context: &NotificationPluginContext,
    ) -> Result<NotificationPlugin, PluginError> {
        let receiver: lore_server::plugins::traits::NotificationReceiver =
            Box::pin(async { Ok(()) });
        Ok(NotificationPlugin {
            sender: Arc::new(MockNotificationSender),
            receivers: vec![receiver],
        })
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

#[tokio::test]
async fn test_notification_output_with_background_task_completes() {
    // Create a notification plugin output with a background task that completes successfully
    let mut registry = PluginRegistry::new();
    registry.register_notification_plugin(Box::new(MockNotificationPluginFactoryWithTask {
        name: "test_with_task",
    }));
    let config = toml::Value::Table(toml::map::Map::new());
    let context = NotificationPluginContext {
        environment: None,
        immutable_store: None,
    };
    let output = registry
        .create_notification("test_with_task", &config, &context)
        .await
        .expect("Should succeed");

    // The mock should create exactly one background task
    assert_eq!(output.receivers.len(), 1);

    // Spawn the background receiver and verify it completes successfully
    let mut join_set = tokio::task::JoinSet::new();
    for receiver in output.receivers {
        lore_spawn!(join_set, receiver);
    }
    let result = join_set
        .join_next()
        .await
        .expect("Should have a task")
        .expect("Task should not panic");
    assert!(result.is_ok());

    // Verify the sender is still valid
    assert_eq!(Arc::strong_count(&output.sender), 1);
}
