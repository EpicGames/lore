// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

fn main() -> anyhow::Result<()> {
    let mut plugin_registry = lore_server::plugins::PluginRegistry::new();
    lore_postgres_server::register(&mut plugin_registry);

    lore_server::server::server_main(lore_server::server_config::ServerConfig {
        plugin_registry,
        ..Default::default()
    })
}
