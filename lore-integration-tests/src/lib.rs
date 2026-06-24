// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod aws_store_test;
mod common;
mod dynamodb_test;
mod hashicorp;
mod locks_test;
mod presign_test;
mod remote_store_test;
mod replication_service_test;
mod storage_mutable_test;
mod storage_remote_test;
mod storage_test;
mod store_fan_out_test;
mod store_keep_alive_test;

#[cfg(test)]
pub fn setup_execution(
    user_id: String,
) -> std::sync::Arc<lore_revision::interface::ExecutionContext> {
    std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_server(
        lore_revision::interface::LoreGlobalArgs::default(),
        lore_revision::relay::EventDispatcher::no_dispatch(),
        user_id,
    ))
}
