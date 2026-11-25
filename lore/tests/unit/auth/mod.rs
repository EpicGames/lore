// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod local_user_info_tests;
mod resolve_auth_endpoint_tests;
mod revoke_refresh_tokens;

/// A repository directory with the given `config.toml` body, or none at all when
/// `config` is `None`. Returns the repository root.
pub(crate) fn repository_with_config(
    label: &str,
    config: Option<&str>,
) -> lore_base::test_util::TempDir {
    let root = lore_base::test_util::TempDir::new(&format!("lore-auth-{label}-"));
    let dot_dir = root.join(lore_revision::repository::DOT_LORE);
    std::fs::create_dir_all(&dot_dir).expect("creating the repository directory");
    if let Some(config) = config {
        std::fs::write(dot_dir.join("config.toml"), config).expect("writing the config");
    }
    root
}
