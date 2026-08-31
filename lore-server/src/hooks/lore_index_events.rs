// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::fs;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::thread;

use async_trait::async_trait;
use parking_lot::Mutex;
use postgres::Client;
use postgres::NoTls;
use uuid::Uuid;

use crate::hooks::Hook;
use crate::hooks::HookContext;
use crate::hooks::HookError;
use crate::hooks::HookFactory;
use crate::hooks::HookPoint;
use crate::hooks::HookRegistrationContext;
use crate::hooks::HookRegistry;
use crate::hooks::HookResponse;

const HOOK_NAME: &str = "lore_index_events";
const SOURCE: &str = "lore_hook";
const DEFAULT_DATABASE_URL_ENV: &str = "DATABASE_URL";
const INDEX_HOOK_POINTS: &[HookPoint] = &[
    HookPoint::BranchPush,
    HookPoint::BranchCreate,
    HookPoint::BranchDelete,
    HookPoint::RepositoryCreate,
];
const INSERT_EVENT_SQL: &str = r#"
insert into lore_index_events (
    id,
    source,
    event_key,
    event_type,
    lore_repository_id,
    lore_branch_id,
    lore_revision_signature,
    lore_revision_number,
    user_id,
    correlation_id,
    payload
) values (
    $1,
    $2,
    $3,
    $4,
    $5,
    $6,
    $7,
    $8,
    $9,
    $10,
    $11::text::jsonb
)
on conflict (event_key) do nothing
"#;

struct LoreIndexEventsHook {
    tx: Mutex<Sender<EventRecord>>,
}

#[async_trait]
impl Hook for LoreIndexEventsHook {
    fn name(&self) -> &'static str {
        HOOK_NAME
    }

    fn hook_points(&self) -> &'static [HookPoint] {
        INDEX_HOOK_POINTS
    }

    fn pre_handler(&self, _ctx: &HookContext) -> Result<(), HookError> {
        Ok(())
    }

    fn response_handler(&self, _ctx: &HookContext) -> Result<HookResponse, HookError> {
        Ok(HookResponse::empty())
    }

    async fn post_handler(&self, ctx: &HookContext) -> Result<(), HookError> {
        self.enqueue("post", ctx);
        Ok(())
    }
}

impl LoreIndexEventsHook {
    fn enqueue(&self, phase: &'static str, ctx: &HookContext) {
        let event = EventRecord::from_context(phase, ctx);
        if let Err(err) = self.tx.lock().send(event) {
            eprintln!("[loreserver-hook:{HOOK_NAME}] failed to enqueue event: {err}");
        }
    }
}

struct LoreIndexEventsHookFactory;

impl HookFactory for LoreIndexEventsHookFactory {
    fn name(&self) -> &'static str {
        HOOK_NAME
    }

    fn create(&self, config: &toml::Value) -> Result<Box<dyn Hook>, HookError> {
        let database_url = database_url_from_config(config)?;
        let tx = spawn_event_worker(database_url)?;
        Ok(Box::new(LoreIndexEventsHook { tx: Mutex::new(tx) }))
    }
}

#[derive(Debug, Clone)]
struct EventRecord {
    id: Uuid,
    event_key: String,
    event_type: String,
    lore_repository_id: Vec<u8>,
    lore_branch_id: Option<Vec<u8>>,
    lore_revision_signature: Option<Vec<u8>>,
    lore_revision_number: Option<i64>,
    user_id: Option<Uuid>,
    correlation_id: Option<Uuid>,
    payload: String,
}

impl EventRecord {
    fn from_context(phase: &'static str, ctx: &HookContext) -> Self {
        let lore_repository_id = ctx.repository().as_ref().to_vec();
        let lore_branch_id = ctx.branch().map(|branch| branch.as_ref().to_vec());
        let lore_revision_signature = ctx.revision().map(|revision| revision.as_ref().to_vec());
        let lore_revision_number = ctx
            .revision_number()
            .map(|revision_number| i64::try_from(revision_number).unwrap_or(i64::MAX));
        let raw_user_id = non_empty(ctx.user());
        let user_id = raw_user_id.as_deref().and_then(uuid_value);

        let raw_correlation_id = non_empty(Some(ctx.correlation_id()));
        let correlation_id = raw_correlation_id.as_deref().and_then(uuid_value);
        let hook_point = canonical_hook_point(ctx.hook_point());
        let event_type = format!("{hook_point}.{phase}");
        let metadata: BTreeMap<String, String> = ctx
            .metadata()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let event_key = event_key(
            &event_type,
            &lore_repository_id,
            lore_branch_id.as_deref(),
            lore_revision_signature.as_deref(),
            lore_revision_number,
            raw_user_id.as_deref(),
            raw_correlation_id.as_deref(),
            &metadata,
        );
        let payload = payload_json(
            phase,
            hook_point,
            &lore_repository_id,
            lore_branch_id.as_deref(),
            lore_revision_signature.as_deref(),
            lore_revision_number,
            user_id.as_ref(),
            raw_user_id.as_deref(),
            correlation_id.as_ref(),
            raw_correlation_id.as_deref(),
            metadata,
        );

        Self {
            id: Uuid::now_v7(),
            event_key,
            event_type,
            lore_repository_id,
            lore_branch_id,
            lore_revision_signature,
            lore_revision_number,
            user_id,
            correlation_id,
            payload,
        }
    }
}

fn canonical_hook_point(hook_point: HookPoint) -> &'static str {
    match hook_point {
        HookPoint::BranchPush => "branch_push",
        HookPoint::BranchCreate => "branch_create",
        HookPoint::BranchDelete => "branch_delete",
        HookPoint::RepositoryCreate => "repository_create",
        HookPoint::Obliterate => "obliterate",
    }
}

fn uuid_value(value: &str) -> Option<Uuid> {
    Uuid::try_parse(value).ok()
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn event_key(
    event_type: &str,
    lore_repository_id: &[u8],
    lore_branch_id: Option<&[u8]>,
    lore_revision_signature: Option<&[u8]>,
    lore_revision_number: Option<i64>,
    user_id: Option<&str>,
    correlation_id: Option<&str>,
    metadata: &BTreeMap<String, String>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    update_str(&mut hasher, "source", SOURCE);
    update_str(&mut hasher, "event_type", event_type);
    update_bytes(&mut hasher, "lore_repository_id", lore_repository_id);
    update_optional_bytes(&mut hasher, "lore_branch_id", lore_branch_id);
    update_optional_bytes(
        &mut hasher,
        "lore_revision_signature",
        lore_revision_signature,
    );
    update_optional_i64(&mut hasher, "lore_revision_number", lore_revision_number);
    update_optional_str(&mut hasher, "user_id", user_id);
    update_optional_str(&mut hasher, "correlation_id", correlation_id);
    for (key, value) in metadata {
        update_str(&mut hasher, "metadata_key", key);
        update_str(&mut hasher, "metadata_value", value);
    }
    hasher.finalize().to_hex().to_string()
}

fn update_str(hasher: &mut blake3::Hasher, field: &str, value: &str) {
    update_bytes(hasher, field, value.as_bytes());
}

fn update_optional_str(hasher: &mut blake3::Hasher, field: &str, value: Option<&str>) {
    match value {
        Some(value) => update_str(hasher, field, value),
        None => update_bytes(hasher, field, b"<none>"),
    }
}

fn update_optional_i64(hasher: &mut blake3::Hasher, field: &str, value: Option<i64>) {
    match value {
        Some(value) => update_bytes(hasher, field, &value.to_le_bytes()),
        None => update_bytes(hasher, field, b"<none>"),
    }
}

fn update_optional_bytes(hasher: &mut blake3::Hasher, field: &str, value: Option<&[u8]>) {
    match value {
        Some(value) => update_bytes(hasher, field, value),
        None => update_bytes(hasher, field, b"<none>"),
    }
}

fn update_bytes(hasher: &mut blake3::Hasher, field: &str, value: &[u8]) {
    hasher.update(&(field.len() as u64).to_le_bytes());
    hasher.update(field.as_bytes());
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[allow(clippy::too_many_arguments)]
fn payload_json(
    phase: &str,
    hook_point: &str,
    lore_repository_id: &[u8],
    lore_branch_id: Option<&[u8]>,
    lore_revision_signature: Option<&[u8]>,
    lore_revision_number: Option<i64>,
    user_id: Option<&Uuid>,
    raw_user_id: Option<&str>,
    correlation_id: Option<&Uuid>,
    raw_correlation_id: Option<&str>,
    metadata: BTreeMap<String, String>,
) -> String {
    let user_id = user_id.map(Uuid::to_string);
    let correlation_id = correlation_id.map(Uuid::to_string);
    let payload = serde_json::json!({
        "hook_name": HOOK_NAME,
        "phase": phase,
        "hook_point": hook_point,
        "lore_repository_id": hex::encode(lore_repository_id),
        "lore_branch_id": lore_branch_id.map(hex::encode),
        "lore_revision_signature": lore_revision_signature.map(hex::encode),
        "lore_revision_number": lore_revision_number,
        "user_id": user_id,
        "raw_user_id": raw_user_id,
        "correlation_id": correlation_id,
        "raw_correlation_id": raw_correlation_id,
        "metadata": metadata,
    });

    serde_json::to_string(&payload).unwrap_or_else(|err| {
        eprintln!("[loreserver-hook:{HOOK_NAME}] failed to serialize payload: {err}");
        "{}".to_string()
    })
}

fn spawn_event_worker(database_url: String) -> Result<Sender<EventRecord>, HookError> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("lore-index-events".to_string())
        .spawn(move || run_event_worker(database_url, rx))
        .map_err(|err| {
            HookError::init_error(HOOK_NAME, format!("failed to spawn worker: {err}"))
        })?;
    Ok(tx)
}

fn run_event_worker(database_url: String, rx: Receiver<EventRecord>) {
    let mut client = None;
    for event in rx {
        insert_with_reconnect(&database_url, &mut client, &event);
    }
}

fn insert_with_reconnect(database_url: &str, client: &mut Option<Client>, event: &EventRecord) {
    for attempt in 0..2 {
        if client.is_none() {
            match Client::connect(database_url, NoTls) {
                Ok(connected) => {
                    *client = Some(connected);
                }
                Err(err) => {
                    eprintln!("[loreserver-hook:{HOOK_NAME}] failed to connect to Postgres: {err}");
                    return;
                }
            }
        }

        let Some(connected) = client.as_mut() else {
            return;
        };

        match insert_event(connected, event) {
            Ok(_) => return,
            Err(err) => {
                eprintln!(
                    "[loreserver-hook:{HOOK_NAME}] failed to insert event {}: {err:?}",
                    event.event_key
                );
                if !err.is_closed() {
                    return;
                }
                *client = None;
                if attempt == 1 {
                    return;
                }
            }
        }
    }
}

fn insert_event(client: &mut Client, event: &EventRecord) -> Result<u64, postgres::Error> {
    client.execute(
        INSERT_EVENT_SQL,
        &[
            &event.id,
            &SOURCE,
            &event.event_key,
            &event.event_type,
            &event.lore_repository_id,
            &event.lore_branch_id,
            &event.lore_revision_signature,
            &event.lore_revision_number,
            &event.user_id,
            &event.correlation_id,
            &event.payload,
        ],
    )
}

fn database_url_from_config(config: &toml::Value) -> Result<String, HookError> {
    if let Some(database_url) = config_str(config, "database_url")? {
        return Ok(database_url.to_string());
    }

    let env_name = config_str(config, "database_url_env")?.unwrap_or(DEFAULT_DATABASE_URL_ENV);
    if let Ok(database_url) = std::env::var(env_name)
        && !database_url.trim().is_empty()
    {
        return Ok(database_url);
    }

    if let Some(path) = config_str(config, "database_url_file")?
        && let Some(database_url) = database_url_from_file(path, env_name)?
    {
        return Ok(database_url);
    }

    Err(HookError::config_error(
        HOOK_NAME,
        format!(
            "missing database URL; set database_url, {env_name}, or database_url_file containing {env_name}",
        ),
    ))
}

fn config_str<'a>(config: &'a toml::Value, key: &str) -> Result<Option<&'a str>, HookError> {
    match config.get(key) {
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| HookError::config_error(HOOK_NAME, format!("'{key}' must be a string"))),
        None => Ok(None),
    }
}

fn database_url_from_file(path: &str, env_name: &str) -> Result<Option<String>, HookError> {
    let content = fs::read_to_string(path).map_err(|err| {
        HookError::config_error(
            HOOK_NAME,
            format!("failed to read database_url_file '{path}': {err}"),
        )
    })?;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == env_name {
            let value = unquote_env_value(value.trim());
            if !value.trim().is_empty() {
                return Ok(Some(value));
            }
        }
    }

    Ok(None)
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

pub fn register(registry: &mut HookRegistry, _ctx: &HookRegistrationContext) {
    registry.register_hook(Box::new(LoreIndexEventsHookFactory));
}

#[cfg(test)]
mod tests {
    use lore_revision::lore::RepositoryId;

    use super::*;

    fn config_with_database_url() -> toml::Value {
        toml::toml! {
            database_url = "postgres://postgres@localhost/test"
        }
        .into()
    }

    fn test_context() -> HookContext {
        HookContext::builder()
            .correlation_id("corr-123")
            .hook_point(HookPoint::BranchPush)
            .repository(RepositoryId::default())
            .user("local-user")
            .metadata("branch_name", "main")
            .build()
    }

    #[test]
    fn factory_creates_hook_for_indexable_hook_points() {
        let hook = LoreIndexEventsHookFactory
            .create(&config_with_database_url())
            .unwrap();

        assert_eq!(hook.name(), HOOK_NAME);
        assert_eq!(hook.hook_points(), INDEX_HOOK_POINTS);
    }

    #[test]
    fn event_key_is_deterministic_but_id_is_unique() {
        let ctx = test_context();

        let first = EventRecord::from_context("post", &ctx);
        let second = EventRecord::from_context("post", &ctx);

        assert_eq!(first.event_type, "branch_push.post");
        assert_eq!(first.event_key, second.event_key);
        assert_ne!(first.id, second.id);
        assert!(first.user_id.is_none());
        let payload: serde_json::Value = serde_json::from_str(&first.payload).unwrap();
        assert_eq!(payload["raw_user_id"], "local-user");
        assert_eq!(first.id.get_version_num(), 7);
    }

    #[test]
    fn response_handler_does_not_modify_client_response() {
        let (tx, rx) = mpsc::channel();
        let hook = LoreIndexEventsHook { tx: Mutex::new(tx) };
        let ctx = test_context();

        let response = hook.response_handler(&ctx).unwrap();

        assert!(response.message.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    #[ignore = "requires DATABASE_URL and a local lore_index_events table"]
    fn inserts_event_into_postgres() {
        let database_url = std::env::var(DEFAULT_DATABASE_URL_ENV).unwrap();
        let mut client = Client::connect(&database_url, NoTls).unwrap();
        let mut event = EventRecord::from_context("post", &test_context());
        let unknown_user_id = Uuid::now_v7();
        event.user_id = Some(unknown_user_id);
        event.event_key = format!("test:{SOURCE}:{}", Uuid::now_v7());

        insert_event(&mut client, &event).unwrap();
        let rows = client
            .query(
                "select source, event_type, payload->>'phase', user_id from lore_index_events where event_key = $1",
                &[&event.event_key],
            )
            .unwrap();
        client
            .execute(
                "delete from lore_index_events where event_key = $1",
                &[&event.event_key],
            )
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, String>(0), SOURCE);
        assert_eq!(rows[0].get::<_, String>(1), "branch_push.post");
        assert_eq!(rows[0].get::<_, String>(2), "post");
        assert_eq!(rows[0].get::<_, Uuid>(3), unknown_user_id);
    }
}
