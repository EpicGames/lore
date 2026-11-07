// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::pin;

use lore_revision::interface::LoreGlobalArgs;

use crate::call_delegation::block_on_command;
use crate::interface::LoreEventCallback;
use crate::remote::command::LoreCommand;

pub trait LoreArgs {
    fn to_command(self) -> LoreCommand;

    /// Whether this call's own arguments require the Lore service, whatever instance it names:
    /// `true` for a clone or a create whose `vfs` resolves to FUSE, a mount only the service
    /// makes. `false` for every other call, whose instance decides instead (see
    /// `instance_requires_service_given` in `call_delegation`).
    ///
    /// Read without building a command, so the caller that only wants this answer never
    /// pays for one.
    fn requires_service(&self) -> bool {
        false
    }
}

// Separate from `LoreArgs`, which is public, so that running a handler stays internal to this crate.
#[lore_macro::test_pub]
pub(crate) trait InvokableLoreArgs: LoreArgs {
    /// The future of this arg type's handler, named by its `#[handler]` attribute.
    ///
    /// The handler's own future, not one awaiting it: a future that only forwards to another holds
    /// its arguments beside the future they moved into. A handler that only calls one of the
    /// wrappers in `crate::call` returns that wrapper's future for the same reason.
    fn invoke_local(
        self,
        globals: LoreGlobalArgs,
        callback: LoreEventCallback,
    ) -> impl Future<Output = i32> + Send;
}

/// Runs `command`'s handler to completion on the calling thread, with the arguments `take_args`
/// moves out of it.
///
/// The handler's future is built in place in this function's frame, sized for this command
/// alone. Not inlined, so the dispatcher's arms, which only pass the command through, hold no
/// command's future whatever the optimizer makes of them.
#[inline(never)]
pub(crate) fn run_handler<A: InvokableLoreArgs>(
    command: LoreCommand,
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    take_args: impl FnOnce(LoreCommand) -> A,
) -> i32 {
    block_on_command(pin!(take_args(command).invoke_local(globals, callback)))
}
