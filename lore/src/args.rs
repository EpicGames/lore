// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(lore_unoptimized)]
use std::pin::Pin;

use lore_revision::interface::LoreGlobalArgs;

use crate::interface::LoreEventCallback;
use crate::remote::command::LoreCommand;

pub trait LoreArgs {
    fn to_command(self) -> LoreCommand;
}

// Separate from `LoreArgs`, which is public, so that running a handler stays internal to this crate.
pub(crate) trait InvokableLoreArgs: LoreArgs {
    // Calls the local implementation of the functionality associated with this arg type
    fn invoke_local(
        self,
        globals: LoreGlobalArgs,
        callback: LoreEventCallback,
    ) -> impl Future<Output = i32> + Send;
}

/// Starts `args`'s handler.
///
/// Unoptimized builds give every arm of `LoreCommand::invoke_local` a stack slot of its own for
/// the future it awaits, which puts every command's future in one frame. Boxing the future leaves
/// each arm a pointer. Optimized builds share the slots, so they await the future in place.
#[cfg(lore_unoptimized)]
pub(crate) fn invoke_args<A: InvokableLoreArgs + 'static>(
    args: A,
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
) -> Pin<Box<dyn Future<Output = i32> + Send>> {
    Box::pin(args.invoke_local(globals, callback))
}

/// Starts `args`'s handler. See the `lore_unoptimized` variant for why unoptimized builds box it.
#[cfg(not(lore_unoptimized))]
pub(crate) fn invoke_args<A: InvokableLoreArgs + 'static>(
    args: A,
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
) -> impl Future<Output = i32> + Send {
    args.invoke_local(globals, callback)
}
