// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod immutable_store;
pub mod lock_store;
pub mod mutable_store;
pub mod object_metadata;

#[cfg(test)]
pub mod test_util;

#[cfg(test)]
pub fn address_with_random_context(address: lore_storage::Address) -> lore_storage::Address {
    lore_storage::Address {
        context: rand::random::<lore_storage::Context>(),
        hash: address.hash,
    }
}

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
