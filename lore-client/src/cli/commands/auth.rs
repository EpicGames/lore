// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::sync::Arc;

use chrono::DateTime;
use clap::Args;
use clap::Subcommand;
use lore::auth;
use lore::auth::LoreAuthClearArgs;
use lore::auth::LoreAuthListArgs;
use lore::auth::LoreAuthLocalUserInfoArgs;
use lore::auth::LoreAuthLogoutArgs;
use lore::auth::LoreAuthUserInfoArgs;
use lore::interface::LoreArray;
use lore::interface::LoreAuthLoginInteractiveArgs;
use lore::interface::LoreAuthLoginWithTokenArgs;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreString;
use lore::runtime;
use parking_lot::Mutex;

#[derive(Clone)]
struct CollectedIdentity {
    auth_url: String,
    resource: String,
    user_id: String,
    authorized_domains: String,
    expires: u64,
    token: String,
}

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::cli::output_formatter;
use crate::println;
use crate::styling::CommonStyles;
use crate::util;

const LOGIN_REMOTE_REQUIRED: &str = "No remote URL supplied. Run `lore auth login <remote-url>` or run from a Lore repository with a configured remote.";

#[derive(Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommands,
}

#[derive(Args)]
pub struct AuthLoginArgs {
    /// Token type for non-interactive login (e.g. "api-key", "eg1", "lore")
    #[clap(long = "token-type")]
    token_type: Option<String>,
    /// Token value for non-interactive login (requires --token-type)
    #[clap(long = "token")]
    token: Option<String>,
    /// Auth service URL with scheme (e.g. `ucs-auth://auth.example.com`).
    /// Required when logging in with `--token` outside a repository without a remote-url.
    #[clap(long = "auth-url")]
    auth_url: Option<String>,
    /// Server URL
    #[clap(value_name = "remote-url")]
    remote_url: Option<String>,
    /// Avoid opening a browser to login
    #[clap(long = "no-browser")]
    no_browser: bool,
}

#[derive(Args)]
pub struct AuthLogoutCliArgs {
    /// Auth service URL (omit to use current repository's auth URL)
    #[clap(long = "auth-url", value_name = "auth-url")]
    auth_url: Option<String>,
    /// Resource ID to remove a specific authorization (e.g. "urc-{id}")
    #[clap(long, value_name = "resource")]
    resource: Option<String>,
    /// User ID to remove (omit to remove all identities)
    #[clap(long, value_name = "user-id")]
    user_id: Option<String>,
}

#[derive(Args)]
pub struct AuthListArgs {
    /// Include cached tokens in the output
    #[clap(long = "with-token")]
    with_token: bool,
}

#[derive(Args)]
pub struct AuthInfoCliArgs {
    /// User IDs to resolve (omit for current user)
    #[clap(value_name = "user-id")]
    user_ids: Vec<String>,
    /// Include cached tokens in the output
    #[clap(long = "with-token")]
    with_token: bool,
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// Authenticate the CLI
    Login(AuthLoginArgs),

    /// Display identity information for the current user or specified user IDs
    Info(AuthInfoCliArgs),

    /// List all stored authentication identities
    List(AuthListArgs),

    /// Remove stored authentication and authorization tokens
    Logout(AuthLogoutCliArgs),

    /// Clear all stored authentication data
    Clear,
}

fn configured_remote_url(repository_path: &str) -> Option<String> {
    lore_revision::repository::load_repository_config(repository_path)
        .ok()
        .and_then(|config| config.remote_url)
        .filter(|remote_url| !remote_url.is_empty())
}

fn login_with_token_can_use_explicit_auth_url(args: &AuthLoginArgs) -> bool {
    args.token_type.is_some()
        && args.token.is_some()
        && args
            .auth_url
            .as_deref()
            .is_some_and(|auth_url| !auth_url.is_empty())
}

fn resolve_login_remote_url(
    globals: &LoreGlobalArgs,
    args: &AuthLoginArgs,
) -> Result<LoreString, &'static str> {
    if let Some(remote_url) = args
        .remote_url
        .as_deref()
        .filter(|remote_url| !remote_url.is_empty())
    {
        return Ok(LoreString::from(remote_url));
    }

    if let Some(remote_url) = configured_remote_url(globals.repository_path.as_str()) {
        return Ok(LoreString::from(remote_url.as_str()));
    }

    if login_with_token_can_use_explicit_auth_url(args) {
        return Ok(LoreString::default());
    }

    Err(LOGIN_REMOTE_REQUIRED)
}

pub fn handle_login_command(globals: LoreGlobalArgs, args: &AuthLoginArgs) -> u8 {
    if args.token_type.is_some() ^ args.token.is_some() {
        crate::eprintln!("Both --token-type and --token are required for non-interactive login");
        return 1;
    }

    let remote_url = match resolve_login_remote_url(&globals, args) {
        Ok(remote_url) => remote_url,
        Err(message) => {
            crate::eprintln!("{message}");
            return 1;
        }
    };

    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::AuthUrl(data) => {
                println!(
                    "Login at: {}{}{}",
                    CommonStyles::LINK,
                    data.url.as_str(),
                    anstyle::Reset
                );
            }
            LoreEvent::Complete(data) if data.status == 0 => {
                println!(
                    "{}Authentication successful{}",
                    CommonStyles::SUCCESS,
                    anstyle::Reset
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    if let (Some(token_type), Some(token)) = (args.token_type.as_deref(), args.token.as_deref()) {
        let args = LoreAuthLoginWithTokenArgs {
            remote_url,
            token: token.into(),
            token_type: LoreString::from(token_type),
            auth_url: args.auth_url.as_deref().into(),
        };
        runtime().block_on(auth::login_with_token(globals, args, callback)) as u8
    } else {
        let args = LoreAuthLoginInteractiveArgs {
            remote_url,
            no_browser: if args.no_browser { 1 } else { 0 },
        };
        runtime().block_on(auth::login_interactive(globals, args, callback)) as u8
    }
}

/// Resolves user IDs to display names via the auth identity info API.
///
/// Groups identities by auth endpoint, batches user IDs per endpoint, and
/// resolves them in a single call per endpoint. Returns a map from `user_id`
/// to display name.
fn resolve_identity_names(
    globals: &LoreGlobalArgs,
    identities: &[CollectedIdentity],
) -> HashMap<String, String> {
    let mut names: HashMap<String, String> = HashMap::new();

    // Group unique user IDs by auth endpoint
    let mut endpoint_ids: HashMap<String, Vec<String>> = HashMap::new();
    for identity in identities {
        endpoint_ids
            .entry(identity.auth_url.clone())
            .or_default()
            .push(identity.user_id.clone());
    }

    // Deduplicate user IDs per endpoint
    for ids in endpoint_ids.values_mut() {
        ids.sort();
        ids.dedup();
    }

    for (endpoint, ids) in &endpoint_ids {
        let names_store: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let names_cb = names_store.clone();

        // Sub-operation callback; safe to ignore error events.
        let callback: lore::interface::LoreEventCallback =
            Some(Box::new(move |event: &LoreEvent| {
                if let LoreEvent::AuthUserInfo(data) = event {
                    names_cb
                        .lock()
                        .insert(data.id.to_string(), data.name.to_string());
                }
            }));

        let args = LoreAuthLocalUserInfoArgs {
            auth_endpoint: LoreString::from(endpoint.as_str()),
            user_ids: LoreArray::from_vec(
                ids.iter().map(|s| LoreString::from(s.as_str())).collect(),
            ),
            with_token: 0,
        };

        runtime().block_on(auth::local_user_info(globals.clone(), args, callback));

        if let Ok(resolved) = Arc::try_unwrap(names_store) {
            for (id, name) in resolved.into_inner() {
                names.insert(id, name);
            }
        }
    }

    names
}

pub fn handle_list_command(globals: LoreGlobalArgs, cli_args: &AuthListArgs) -> u8 {
    let with_token = cli_args.with_token;

    // In JSON mode, stream events directly
    if let Some(callback) = output_formatter() {
        let args = LoreAuthListArgs {
            with_token: u8::from(with_token),
        };
        return runtime().block_on(auth::list(globals, args, callback)) as u8;
    }

    // Collect identity events first so we can resolve names before printing
    let collected: Arc<Mutex<Vec<CollectedIdentity>>> = Arc::new(Mutex::new(vec![]));
    let collected_clone = collected.clone();

    let callback: lore::interface::LoreEventCallback = Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::AuthIdentity(data) => {
                collected_clone.lock().push(CollectedIdentity {
                    auth_url: data.auth_url.to_string(),
                    resource: data.resource.to_string(),
                    user_id: data.user_id.to_string(),
                    authorized_domains: data.authorized_domains.to_string(),
                    expires: data.expires,
                    token: data.token.to_string(),
                });
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    );

    let args = LoreAuthListArgs {
        with_token: u8::from(with_token),
    };
    let status = runtime().block_on(auth::list(globals.clone(), args, callback)) as u8;

    let identities = collected.lock().clone();
    let names = resolve_identity_names(&globals, &identities);

    for data in &identities {
        println!(
            "{}Auth URL:{} {}",
            CommonStyles::HEADERS,
            anstyle::Reset,
            data.auth_url.as_str()
        );
        if !data.resource.is_empty() {
            println!(
                "  {}Resource:{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                data.resource.as_str()
            );
        }
        let user_id = &data.user_id;
        if let Some(name) = names.get(user_id) {
            println!(
                "  {}User:{} {} ({})",
                CommonStyles::HEADERS,
                anstyle::Reset,
                name,
                user_id
            );
        } else {
            println!(
                "  {}User ID:{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                user_id
            );
        }
        if !data.authorized_domains.is_empty() {
            println!(
                "  {}Domains:{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                data.authorized_domains.as_str()
            );
        }
        if data.expires > 0
            && let Some(time) = DateTime::from_timestamp_millis(data.expires as i64)
        {
            println!(
                "  {}Expires:{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                time.to_rfc2822()
            );
        }
        if !data.token.is_empty() {
            println!(
                "  {}Token:{} {}",
                CommonStyles::HEADERS,
                anstyle::Reset,
                data.token.as_str()
            );
        }
    }

    status
}

pub fn handle_logout_command(globals: LoreGlobalArgs, args: &AuthLogoutCliArgs) -> u8 {
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(data) if data.status == 0 => {
                println!("{}Logged out{}", CommonStyles::SUCCESS, anstyle::Reset);
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let api_args = LoreAuthLogoutArgs {
        auth_url: LoreString::from(&args.auth_url),
        resource: args.resource.clone().into(),
        user_id: LoreString::from(&args.user_id),
    };

    runtime().block_on(auth::logout(globals, api_args, callback)) as u8
}

pub fn handle_clear_command(globals: LoreGlobalArgs) -> u8 {
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Complete(data) if data.status == 0 => {
                println!(
                    "{}Auth store cleared{}",
                    CommonStyles::SUCCESS,
                    anstyle::Reset
                );
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let args = LoreAuthClearArgs::default();

    runtime().block_on(auth::clear(globals, args, callback)) as u8
}

pub fn handle_info_command(globals: LoreGlobalArgs, args: &AuthInfoCliArgs) -> u8 {
    let callback = output_formatter().unwrap_or(Some(
        (Box::new(move |event: &LoreEvent| match event {
            LoreEvent::AuthUserInfo(data) => {
                println!(
                    "{}ID:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.id.as_str()
                );
                println!(
                    "{}Name:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    data.name.as_str()
                );
            }
            LoreEvent::AuthUserToken(user_token) => {
                println!(
                    "{}ID:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    user_token.id.as_str()
                );
                println!(
                    "{}Name:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    user_token.name.as_str()
                );
                println!(
                    "{}Username:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    user_token.preferred_username.as_str()
                );
                if user_token.flag_service_account != 0 {
                    println!("Service account");
                }
                if user_token.expires > 0
                    && let Some(time) = DateTime::from_timestamp_millis(user_token.expires as i64)
                {
                    println!(
                        "{}Expires:{} {}",
                        CommonStyles::HEADERS,
                        anstyle::Reset,
                        time.to_rfc2822()
                    );
                }
                println!(
                    "{}Token:{} {}",
                    CommonStyles::HEADERS,
                    anstyle::Reset,
                    user_token.token.as_str()
                );
            }
            LoreEvent::Complete(_) => {}
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        }) as EventCallbackFn)
            .with_defaults(),
    ));

    let api_args = LoreAuthLocalUserInfoArgs {
        auth_endpoint: LoreString::default(),
        user_ids: LoreArray::from_vec(
            args.user_ids
                .iter()
                .map(|s| LoreString::from(s.as_str()))
                .collect(),
        ),
        with_token: u8::from(args.with_token),
    };

    runtime().block_on(auth::local_user_info(globals, api_args, callback)) as u8
}

pub fn handle_auth_commands(cmd: &AuthCommands, globals: LoreGlobalArgs) -> u8 {
    match cmd {
        AuthCommands::Login(args) => handle_login_command(globals, args),
        AuthCommands::Info(args) => handle_info_command(globals, args),
        AuthCommands::List(args) => handle_list_command(globals, args),
        AuthCommands::Logout(args) => handle_logout_command(globals, args),
        AuthCommands::Clear => handle_clear_command(globals),
    }
}

pub fn resolve_user_ids(
    globals: LoreGlobalArgs,
    user_ids: &[LoreString],
) -> HashMap<String, String> {
    let auth_args = LoreAuthUserInfoArgs {
        user_ids: LoreArray::from_vec(user_ids.to_vec()),
    };

    let auth_data: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::default()));

    let auth_data_clone = auth_data.clone();
    // Sub-operation callback; safe to ignore error events.
    let callback =
        output_formatter().unwrap_or(Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::AuthUserInfo(data) => {
                auth_data_clone
                    .lock()
                    .insert(data.id.to_string(), data.name.to_string());
            }
            LoreEvent::Maintenance(data) => {
                util::handle_maintenance_event(data);
            }
            _ => (),
        })));

    let result = runtime().block_on(auth::resolve_user_info(
        globals.clone(),
        auth_args,
        callback,
    )) as u8;

    if result != 0 {
        return HashMap::default();
    }

    match Arc::try_unwrap(auth_data) {
        Ok(auth_data) => auth_data.into_inner(),
        Err(_) => HashMap::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globals_without_repo() -> LoreGlobalArgs {
        let repository_path = std::env::temp_dir().join(format!(
            "lore-auth-login-missing-repo-{}",
            std::process::id()
        ));

        LoreGlobalArgs {
            repository_path: repository_path.display().to_string().into(),
            ..Default::default()
        }
    }

    fn login_args(
        remote_url: Option<&str>,
        token_type: Option<&str>,
        token: Option<&str>,
        auth_url: Option<&str>,
    ) -> AuthLoginArgs {
        AuthLoginArgs {
            token_type: token_type.map(str::to_owned),
            token: token.map(str::to_owned),
            auth_url: auth_url.map(str::to_owned),
            remote_url: remote_url.map(str::to_owned),
            no_browser: false,
        }
    }

    #[test]
    fn interactive_login_without_remote_errors() {
        let globals = globals_without_repo();
        let args = login_args(None, None, None, None);

        assert_eq!(
            resolve_login_remote_url(&globals, &args).unwrap_err(),
            LOGIN_REMOTE_REQUIRED
        );
    }

    #[test]
    fn token_login_with_auth_url_allows_missing_remote() {
        let globals = globals_without_repo();
        let args = login_args(
            None,
            Some("api-key"),
            Some("token-value"),
            Some("ucs-auth://auth.example.com"),
        );

        assert!(
            resolve_login_remote_url(&globals, &args)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn login_uses_remote_argument_when_present() {
        let globals = globals_without_repo();
        let args = login_args(Some("lore.example.com"), None, None, None);

        assert_eq!(
            resolve_login_remote_url(&globals, &args).unwrap().as_str(),
            "lore.example.com"
        );
    }
}
