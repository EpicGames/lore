// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::Manager;
use deadpool_postgres::ManagerConfig;
use deadpool_postgres::Pool;
use deadpool_postgres::RecyclingMethod;
use deadpool_postgres::Runtime;
use lore_aws::store::fragment_catalog::BeginObliteration;
use lore_aws::store::fragment_catalog::FragmentCatalog;
use lore_aws::store::fragment_catalog::ObliterationLease;
use lore_aws::store::fragment_catalog::ReleaseAssociation;
use lore_base::error::AddressNotFound;
use lore_base::error::SlowDown;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use lore_storage::StoreQueryResult;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use tokio_postgres::IsolationLevel;
use tokio_postgres::Row;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::SqlState;
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::warn;

const DEFAULT_SCHEMA: &str = "lore";
const DEFAULT_MAX_CONNECTIONS: usize = 16;
const DEFAULT_CONNECT_TIMEOUT_MILLIS: u64 = 5_000;
const DEFAULT_BATCH_SIZE: usize = 10_000;
const OBLITERATION_MASK: u32 = FragmentFlags::PayloadObliteration.bits();

const MIGRATION_VERSION: i32 = 1;
const MIGRATION_SQL: &str = include_str!("../migrations/0001_fragment_catalog.sql");
#[cfg(test)]
const EXPECTED_MIGRATION_CHECKSUM: &str =
    "31cdf92ab669b9568bd81c600e676cc358e7a35110500e293c08418bbbc36c49";

fn default_schema() -> String {
    DEFAULT_SCHEMA.to_string()
}

fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
}

fn default_connect_timeout_millis() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MILLIS
}

fn default_batch_size() -> usize {
    DEFAULT_BATCH_SIZE
}

/// Connection and namespace settings for a PostgreSQL fragment catalog.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresFragmentCatalogConfig {
    /// libpq-style connection string or PostgreSQL URL.
    #[serde(alias = "url")]
    pub connection_string: String,
    /// Isolated PostgreSQL schema owned by this Lore deployment.
    #[serde(default = "default_schema")]
    pub schema: String,
    /// Maximum number of pooled PostgreSQL connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Connection and pool checkout timeout.
    #[serde(default = "default_connect_timeout_millis")]
    pub connect_timeout_millis: u64,
    /// Maximum address count accepted by one batch lookup.
    #[serde(default = "default_batch_size")]
    pub max_batch_size: usize,
}

impl fmt::Debug for PostgresFragmentCatalogConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresFragmentCatalogConfig")
            .field("connection_string", &"<redacted>")
            .field("schema", &self.schema)
            .field("max_connections", &self.max_connections)
            .field("connect_timeout_millis", &self.connect_timeout_millis)
            .field("max_batch_size", &self.max_batch_size)
            .finish()
    }
}

impl PostgresFragmentCatalogConfig {
    /// Create settings for a connection string using production-safe defaults.
    pub fn new(connection_string: String) -> Self {
        Self {
            connection_string,
            schema: default_schema(),
            max_connections: default_max_connections(),
            connect_timeout_millis: default_connect_timeout_millis(),
            max_batch_size: default_batch_size(),
        }
    }

    /// Validate settings without opening a database connection.
    pub fn validate(&self) -> Result<(), StoreError> {
        validate_identifier(&self.schema)?;
        if self.max_connections == 0 {
            return Err(StoreError::internal(
                "PostgreSQL max_connections must be greater than zero",
            ));
        }
        if self.max_batch_size == 0 {
            return Err(StoreError::internal(
                "PostgreSQL max_batch_size must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// PostgreSQL implementation of Lore's fragment metadata and association catalog.
#[derive(Clone)]
pub struct PostgresFragmentCatalog {
    pool: Pool,
    schema: Arc<str>,
    max_batch_size: usize,
}

impl fmt::Debug for PostgresFragmentCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresFragmentCatalog")
            .field("schema", &self.schema)
            .field("max_batch_size", &self.max_batch_size)
            .finish_non_exhaustive()
    }
}

impl PostgresFragmentCatalog {
    /// Connect, verify the pool, and apply catalog migrations transactionally.
    pub async fn connect(config: PostgresFragmentCatalogConfig) -> Result<Self, StoreError> {
        config.validate()?;

        let mut pg_config =
            tokio_postgres::Config::from_str(&config.connection_string).map_err(|error| {
                StoreError::internal_with_context(
                    error,
                    "Failed to parse PostgreSQL connection settings",
                )
            })?;
        pg_config.connect_timeout(Duration::from_millis(config.connect_timeout_millis));
        pg_config.application_name("lore-fragment-catalog");

        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = match pg_config.get_ssl_mode() {
            SslMode::Disable => MakeRustlsConnect::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth(),
            ),
            SslMode::Prefer | SslMode::Require => {
                let (connector, certificate_errors) = MakeRustlsConnect::with_native_certs()
                    .map_err(|errors| {
                        StoreError::internal(format!(
                            "Failed to load PostgreSQL TLS trust roots: {errors:?}"
                        ))
                    })?;
                if !certificate_errors.is_empty() {
                    warn!(
                        errors = ?certificate_errors,
                        "Some PostgreSQL TLS trust roots could not be loaded"
                    );
                }
                connector
            }
            _ => {
                return Err(StoreError::internal(
                    "Unsupported PostgreSQL sslmode; use disable, prefer, or require",
                ));
            }
        };

        let manager = Manager::from_config(
            pg_config,
            tls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Verified,
            },
        );
        let pool = Pool::builder(manager)
            .runtime(Runtime::Tokio1)
            .max_size(config.max_connections)
            .wait_timeout(Some(Duration::from_millis(config.connect_timeout_millis)))
            .build()
            .map_err(|error| {
                StoreError::internal_with_context(error, "Failed to build PostgreSQL pool")
            })?;

        let catalog = Self {
            pool,
            schema: Arc::from(config.schema),
            max_batch_size: config.max_batch_size,
        };
        drop(catalog.connection().await?);
        catalog.migrate().await?;
        Ok(catalog)
    }

    /// Create a uniquely namespaced catalog using `LORE_POSTGRES_TEST_URL`.
    #[doc(hidden)]
    pub async fn connect_for_test() -> Result<Self, StoreError> {
        let connection_string = std::env::var("LORE_POSTGRES_TEST_URL").map_err(|error| {
            StoreError::internal_with_context(
                error,
                "LORE_POSTGRES_TEST_URL is required for PostgreSQL integration tests",
            )
        })?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| {
                StoreError::internal_with_context(error, "System clock precedes Unix epoch")
            })?
            .as_nanos();
        let mut config = PostgresFragmentCatalogConfig::new(connection_string);
        config.schema = format!("lore_test_{}_{nonce}", std::process::id());
        Self::connect(config).await
    }

    async fn connection(&self) -> Result<deadpool_postgres::Client, StoreError> {
        self.pool.get().await.map_err(|error| {
            warn!(?error, "Failed to acquire PostgreSQL catalog connection");
            StoreError::from(SlowDown)
        })
    }

    fn metadata_table(&self) -> String {
        format!("\"{}\".fragment_metadata", self.schema)
    }

    fn association_table(&self) -> String {
        format!("\"{}\".fragment_association", self.schema)
    }

    async fn migrate(&self) -> Result<(), StoreError> {
        let migration_checksum = migration_checksum();
        let mut client = self.connection().await?;
        let transaction = client.transaction().await.map_err(|error| {
            database_error(error, "Failed to begin PostgreSQL catalog migration")
        })?;
        transaction
            .query_one(
                "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
                &[&format!("lore-fragment-catalog:{}", self.schema)],
            )
            .await
            .map_err(|error| database_error(error, "Failed to lock catalog migrations"))?;

        transaction
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS \"{schema}\";
                 CREATE TABLE IF NOT EXISTS \"{schema}\".schema_migrations (
                     version INTEGER PRIMARY KEY,
                     checksum TEXT NOT NULL,
                     applied_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );",
                schema = self.schema,
            ))
            .await
            .map_err(|error| database_error(error, "Failed to create migration catalog"))?;

        let existing = transaction
            .query_opt(
                &format!(
                    "SELECT checksum FROM \"{}\".schema_migrations WHERE version = $1",
                    self.schema
                ),
                &[&MIGRATION_VERSION],
            )
            .await
            .map_err(|error| database_error(error, "Failed to inspect catalog migrations"))?;

        if let Some(row) = existing {
            let checksum: String = row.get(0);
            if checksum != migration_checksum {
                return Err(StoreError::internal(format!(
                    "PostgreSQL catalog migration {MIGRATION_VERSION} checksum mismatch"
                )));
            }
        } else {
            transaction
                .batch_execute(&format!("SET LOCAL search_path TO \"{}\"", self.schema))
                .await
                .map_err(|error| database_error(error, "Failed to select catalog schema"))?;
            transaction
                .batch_execute(MIGRATION_SQL)
                .await
                .map_err(|error| database_error(error, "Failed to apply catalog migration 1"))?;
            transaction
                .execute(
                    &format!(
                        "INSERT INTO \"{}\".schema_migrations (version, checksum) VALUES ($1, $2)",
                        self.schema
                    ),
                    &[&MIGRATION_VERSION, &migration_checksum],
                )
                .await
                .map_err(|error| database_error(error, "Failed to record catalog migration 1"))?;
        }

        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, "Failed to commit catalog migrations"))
    }

    async fn locked_metadata(
        transaction: &deadpool_postgres::Transaction<'_>,
        table: &str,
        hash: Hash,
    ) -> Result<Option<Fragment>, StoreError> {
        transaction
            .query_opt(
                &format!(
                    "SELECT flags, size_payload, size_content::text FROM {table}
                     WHERE hash = $1 FOR UPDATE"
                ),
                &[&hash.as_ref()],
            )
            .await
            .map_err(|error| database_error(error, "Failed to lock fragment metadata"))?
            .map(|row| fragment_from_row(&row))
            .transpose()
    }
}

#[async_trait]
impl FragmentCatalog for PostgresFragmentCatalog {
    async fn query(
        &self,
        repository: Context,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        if match_requested == StoreMatch::MatchNone {
            return Ok(StoreQueryResult::default());
        }

        let client = self.connection().await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT m.flags, m.size_payload, m.size_content::text,
                            EXISTS (
                                SELECT 1 FROM {associations} a
                                WHERE a.hash = m.hash AND a.repository = $2 AND a.context = $3
                            ) AS exact_match,
                            EXISTS (
                                SELECT 1 FROM {associations} a
                                WHERE a.hash = m.hash AND a.repository = $2
                            ) AS repository_match,
                            EXISTS (
                                SELECT 1 FROM {associations} a WHERE a.hash = m.hash
                            ) AS hash_match
                     FROM {metadata} m WHERE m.hash = $1",
                    associations = self.association_table(),
                    metadata = self.metadata_table(),
                ),
                &[
                    &address.hash.as_ref(),
                    &repository.as_ref(),
                    &address.context.as_ref(),
                ],
            )
            .await
            .map_err(|error| database_error(error, "Failed to query fragment catalog"))?;

        let Some(row) = row else {
            return Ok(StoreQueryResult::default());
        };
        let exact_match: bool = row.get(3);
        let repository_match: bool = row.get(4);
        let hash_match: bool = row.get(5);
        let match_made = choose_match(match_requested, exact_match, repository_match, hash_match);
        if match_made == StoreMatch::MatchNone {
            return Ok(StoreQueryResult::default());
        }
        Ok(StoreQueryResult {
            fragment: fragment_from_row(&row)?,
            match_made,
        })
    }

    async fn query_batch(
        &self,
        repository: Context,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        if addresses.len() > self.max_batch_size {
            return Err(StoreError::internal(format!(
                "PostgreSQL catalog batch has {} addresses; maximum is {}",
                addresses.len(),
                self.max_batch_size
            )));
        }
        if addresses.is_empty() || match_requested == StoreMatch::MatchNone {
            return Ok(vec![StoreMatch::MatchNone; addresses.len()]);
        }

        let hashes: Vec<Vec<u8>> = addresses
            .iter()
            .map(|address| address.hash.as_ref().to_vec())
            .collect();
        let contexts: Vec<Vec<u8>> = addresses
            .iter()
            .map(|address| address.context.as_ref().to_vec())
            .collect();
        let client = self.connection().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT input.ordinality,
                            EXISTS (
                                SELECT 1 FROM {associations} a
                                WHERE a.hash = input.hash
                                  AND a.repository = $1
                                  AND a.context = input.context
                            ) AS exact_match,
                            EXISTS (
                                SELECT 1 FROM {associations} a
                                WHERE a.hash = input.hash AND a.repository = $1
                            ) AS repository_match,
                            EXISTS (
                                SELECT 1 FROM {associations} a WHERE a.hash = input.hash
                            ) AS hash_match
                     FROM unnest($2::bytea[], $3::bytea[])
                         WITH ORDINALITY AS input(hash, context, ordinality)
                     ORDER BY input.ordinality",
                    associations = self.association_table(),
                ),
                &[&repository.as_ref(), &hashes, &contexts],
            )
            .await
            .map_err(|error| database_error(error, "Failed to batch query fragment catalog"))?;

        if rows.len() != addresses.len() {
            return Err(StoreError::internal(
                "PostgreSQL batch query returned an unexpected row count",
            ));
        }
        Ok(rows
            .iter()
            .map(|row| choose_match(match_requested, row.get(1), row.get(2), row.get(3)))
            .collect())
    }

    async fn load_metadata(&self, hash: Hash) -> Result<Fragment, StoreError> {
        let client = self.connection().await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT flags, size_payload, size_content::text FROM {}
                     WHERE hash = $1",
                    self.metadata_table()
                ),
                &[&hash.as_ref()],
            )
            .await
            .map_err(|error| database_error(error, "Failed to load fragment metadata"))?;
        row.map(|row| fragment_from_row(&row))
            .transpose()?
            .ok_or_else(|| {
                StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
            })
    }

    async fn register_fragment(
        &self,
        repository: Context,
        address: Address,
        fragment: Fragment,
    ) -> Result<(), StoreError> {
        let mut client = self.connection().await?;
        let transaction = client
            .build_transaction()
            .isolation_level(IsolationLevel::Serializable)
            .start()
            .await
            .map_err(|error| database_error(error, "Failed to begin fragment registration"))?;
        let metadata = self.metadata_table();
        let hash = address.hash.as_ref();
        let flags = i64::from(fragment.flags);
        let size_payload = i64::from(fragment.size_payload);
        let size_content = fragment.size_content.to_string();
        transaction
            .execute(
                &format!(
                    "INSERT INTO {metadata} (hash, flags, size_payload, size_content)
                     VALUES ($1, $2, $3, $4::text::numeric) ON CONFLICT (hash) DO NOTHING"
                ),
                &[&hash, &flags, &size_payload, &size_content],
            )
            .await
            .map_err(|error| database_error(error, "Failed to insert fragment metadata"))?;

        let stored = Self::locked_metadata(&transaction, &metadata, address.hash)
            .await?
            .ok_or_else(|| StoreError::internal("Fragment metadata disappeared during insert"))?;
        if stored != fragment {
            return Err(StoreError::internal(
                if stored.flags & OBLITERATION_MASK != 0 {
                    "Cannot register an obliterating or obliterated fragment"
                } else {
                    "Hash collision: existing fragment metadata differs"
                },
            ));
        }

        transaction
            .execute(
                &format!(
                    "INSERT INTO {} (hash, repository, context) VALUES ($1, $2, $3)
                     ON CONFLICT DO NOTHING",
                    self.association_table()
                ),
                &[
                    &address.hash.as_ref(),
                    &repository.as_ref(),
                    &address.context.as_ref(),
                ],
            )
            .await
            .map_err(|error| database_error(error, "Failed to register fragment association"))?;
        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, "Failed to commit fragment registration"))
    }

    async fn associate_fragment(
        &self,
        repository: Context,
        address: Address,
    ) -> Result<(), StoreError> {
        let mut client = self.connection().await?;
        let transaction = client
            .transaction()
            .await
            .map_err(|error| database_error(error, "Failed to begin fragment association"))?;
        let stored = Self::locked_metadata(&transaction, &self.metadata_table(), address.hash)
            .await?
            .ok_or_else(|| StoreError::from(AddressNotFound::from(address)))?;
        if stored.flags & OBLITERATION_MASK != 0 {
            return Err(StoreError::internal(
                "Cannot associate an obliterating or obliterated fragment",
            ));
        }
        transaction
            .execute(
                &format!(
                    "INSERT INTO {} (hash, repository, context) VALUES ($1, $2, $3)
                     ON CONFLICT DO NOTHING",
                    self.association_table()
                ),
                &[
                    &address.hash.as_ref(),
                    &repository.as_ref(),
                    &address.context.as_ref(),
                ],
            )
            .await
            .map_err(|error| database_error(error, "Failed to associate fragment"))?;
        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, "Failed to commit fragment association"))
    }

    async fn begin_obliteration(&self, hash: Hash) -> Result<BeginObliteration, StoreError> {
        let mut client = self.connection().await?;
        let transaction = client
            .transaction()
            .await
            .map_err(|error| database_error(error, "Failed to begin obliteration transaction"))?;
        let metadata = self.metadata_table();
        let stored = Self::locked_metadata(&transaction, &metadata, hash)
            .await?
            .ok_or_else(|| {
                StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
            })?;
        lore_storage::validate_fragment_size(&stored)?;
        if stored.flags & FragmentFlags::PayloadObliterated.bits() != 0 {
            transaction.commit().await.map_err(|error| {
                database_error(error, "Failed to commit terminal obliteration lookup")
            })?;
            return Ok(BeginObliteration::AlreadyObliterated);
        }

        let mut original = stored;
        original.flags &= !OBLITERATION_MASK;
        let marker = if stored.flags & FragmentFlags::PayloadObliterating.bits() != 0 {
            stored
        } else {
            let mut marker = original;
            marker.flags |= FragmentFlags::PayloadObliterating.bits();
            update_metadata(&transaction, &metadata, hash, marker).await?;
            marker
        };
        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, "Failed to commit obliteration marker"))?;
        Ok(BeginObliteration::Acquired(ObliterationLease::new(
            original, marker,
        )))
    }

    async fn release_association(
        &self,
        repository: Context,
        address: Address,
        lease: ObliterationLease,
    ) -> Result<ReleaseAssociation, StoreError> {
        let mut client = self.connection().await?;
        let transaction = client
            .transaction()
            .await
            .map_err(|error| database_error(error, "Failed to begin association release"))?;
        let metadata = self.metadata_table();
        let stored = Self::locked_metadata(&transaction, &metadata, address.hash)
            .await?
            .ok_or_else(|| StoreError::from(AddressNotFound::from(address)))?;
        if stored != lease.marker() {
            return Err(StoreError::internal(
                "Obliteration lease no longer matches fragment metadata",
            ));
        }

        transaction
            .execute(
                &format!(
                    "DELETE FROM {} WHERE hash = $1 AND repository = $2 AND context = $3",
                    self.association_table()
                ),
                &[
                    &address.hash.as_ref(),
                    &repository.as_ref(),
                    &address.context.as_ref(),
                ],
            )
            .await
            .map_err(|error| database_error(error, "Failed to release fragment association"))?;
        let remains: bool = transaction
            .query_one(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM {} WHERE hash = $1)",
                    self.association_table()
                ),
                &[&address.hash.as_ref()],
            )
            .await
            .map_err(|error| database_error(error, "Failed to count fragment associations"))?
            .get(0);

        let outcome = if remains {
            update_metadata(&transaction, &metadata, address.hash, lease.original()).await?;
            ReleaseAssociation::ReferencesRemain
        } else {
            ReleaseAssociation::PayloadUnreferenced
        };
        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, "Failed to commit association release"))?;
        Ok(outcome)
    }

    async fn finalize_obliteration(
        &self,
        hash: Hash,
        lease: ObliterationLease,
    ) -> Result<(), StoreError> {
        let mut client = self.connection().await?;
        let transaction = client
            .transaction()
            .await
            .map_err(|error| database_error(error, "Failed to begin obliteration finalization"))?;
        let metadata = self.metadata_table();
        let stored = Self::locked_metadata(&transaction, &metadata, hash)
            .await?
            .ok_or_else(|| {
                StoreError::from(AddressNotFound::from(Address::zero_context_hash(hash)))
            })?;
        let terminal = Fragment {
            flags: FragmentFlags::PayloadObliterated.bits(),
            size_payload: 0,
            size_content: 0,
        };
        if stored == terminal {
            transaction.commit().await.map_err(|error| {
                database_error(error, "Failed to commit idempotent finalization")
            })?;
            return Ok(());
        }
        if stored != lease.marker() {
            return Err(StoreError::internal(
                "Obliteration lease no longer matches fragment metadata",
            ));
        }
        update_metadata(&transaction, &metadata, hash, terminal).await?;
        transaction
            .commit()
            .await
            .map_err(|error| database_error(error, "Failed to commit obliteration finalization"))
    }

    fn max_query_batch(&self) -> Option<usize> {
        Some(self.max_batch_size)
    }
}

fn validate_identifier(identifier: &str) -> Result<(), StoreError> {
    let valid_length = !identifier.is_empty() && identifier.len() <= 63;
    let mut characters = identifier.bytes();
    let valid_first = characters
        .next()
        .is_some_and(|value| value == b'_' || value.is_ascii_alphabetic());
    let valid_rest = characters.all(|value| value == b'_' || value.is_ascii_alphanumeric());
    if valid_length && valid_first && valid_rest {
        Ok(())
    } else {
        Err(StoreError::internal(
            "PostgreSQL schema must be a 1-63 character SQL identifier",
        ))
    }
}

fn choose_match(requested: StoreMatch, exact: bool, repository: bool, hash: bool) -> StoreMatch {
    match requested {
        StoreMatch::MatchFull if exact => StoreMatch::MatchFull,
        StoreMatch::MatchFull => StoreMatch::MatchNone,
        StoreMatch::MatchPartition if repository => StoreMatch::MatchPartition,
        StoreMatch::MatchPartition if hash => StoreMatch::MatchHash,
        StoreMatch::MatchPartition => StoreMatch::MatchNone,
        StoreMatch::MatchHash if hash => StoreMatch::MatchHash,
        StoreMatch::MatchHash | StoreMatch::MatchNone => StoreMatch::MatchNone,
    }
}

fn fragment_from_row(row: &Row) -> Result<Fragment, StoreError> {
    let flags: i64 = row.get(0);
    let size_payload: i64 = row.get(1);
    let size_content: String = row.get(2);
    Ok(Fragment {
        flags: flags.try_into().map_err(|error| {
            StoreError::internal_with_context(error, "PostgreSQL fragment flags are out of range")
        })?,
        size_payload: size_payload.try_into().map_err(|error| {
            StoreError::internal_with_context(
                error,
                "PostgreSQL fragment payload size is out of range",
            )
        })?,
        size_content: size_content.parse().map_err(|error| {
            StoreError::internal_with_context(error, "PostgreSQL fragment content size is invalid")
        })?,
    })
}

async fn update_metadata(
    transaction: &deadpool_postgres::Transaction<'_>,
    table: &str,
    hash: Hash,
    fragment: Fragment,
) -> Result<(), StoreError> {
    let flags = i64::from(fragment.flags);
    let size_payload = i64::from(fragment.size_payload);
    let size_content = fragment.size_content.to_string();
    let updated = transaction
        .execute(
            &format!(
                "UPDATE {table} SET flags = $2, size_payload = $3, size_content = $4::text::numeric
                 WHERE hash = $1"
            ),
            &[&hash.as_ref(), &flags, &size_payload, &size_content],
        )
        .await
        .map_err(|error| database_error(error, "Failed to update fragment metadata"))?;
    if updated == 1 {
        Ok(())
    } else {
        Err(StoreError::internal(
            "Fragment metadata disappeared during update",
        ))
    }
}

fn database_error(error: tokio_postgres::Error, context: &'static str) -> StoreError {
    let retryable = error.is_closed()
        || error.as_db_error().is_some_and(|database_error| {
            matches!(
                database_error.code(),
                &SqlState::T_R_SERIALIZATION_FAILURE
                    | &SqlState::T_R_DEADLOCK_DETECTED
                    | &SqlState::QUERY_CANCELED
                    | &SqlState::CONNECTION_EXCEPTION
                    | &SqlState::CONNECTION_FAILURE
                    | &SqlState::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION
            )
        });
    warn!(
        ?error,
        context, "PostgreSQL fragment catalog operation failed"
    );
    if retryable {
        StoreError::from(SlowDown)
    } else {
        StoreError::internal_with_context(error, context)
    }
}

fn migration_checksum() -> String {
    Sha256::digest(MIGRATION_SQL.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_migration_checksum_is_stable() {
        assert_eq!(migration_checksum(), EXPECTED_MIGRATION_CHECKSUM);
    }
}
