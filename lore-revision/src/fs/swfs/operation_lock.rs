// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Gives one command at a time the operation on a mount.

use std::sync::Arc;

use lore_base::error::NotSupported;
use tokio::sync::OwnedMutexGuard;

/// Which command holds a mount's operation, from the freeze to the thaw that ends it.
///
/// SWFS freezes a mount as a whole, and a thaw by one operation ends the snapshot another is
/// still reading, so a command that begins an operation waits until the command holding the
/// mount's has finished (ADR-00026). A command that begins one inside its own would wait for
/// itself, so it is refused at once instead. A command is told apart by its execution context; one
/// made outside any, a holder of `None`, is never refused, and waits.
#[derive(Debug, Default)]
pub struct OperationLock {
    gate: Arc<tokio::sync::Mutex<()>>,
    holder: Arc<parking_lot::Mutex<Option<usize>>>,
}

/// The mount's operation, held until it is dropped.
#[derive(Debug)]
pub struct OperationGuard {
    _gate: OwnedMutexGuard<()>,
    holder: Arc<parking_lot::Mutex<Option<usize>>>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        // Cleared before the gate is released, which its field drops after this.
        *self.holder.lock() = None;
    }
}

impl OperationLock {
    /// The current command, as a holder.
    pub fn current_holder() -> Option<usize> {
        crate::runtime::try_execution_context()
            .map(|context| Arc::as_ptr(&context).cast::<()>() as usize)
    }

    /// Waits for the mount's operation, for the current command. Refused where the current command
    /// holds it already.
    pub async fn acquire(&self) -> Result<OperationGuard, NotSupported> {
        self.acquire_as(Self::current_holder()).await
    }

    /// [`Self::acquire`] for `holder`.
    #[lore_macro::test_pub]
    async fn acquire_as(&self, holder: Option<usize>) -> Result<OperationGuard, NotSupported> {
        if holder.is_some() && *self.holder.lock() == holder {
            return Err(NotSupported {
                operation: "an operation inside another on an SWFS instance, which would wait for \
                            itself: the inner one must use the outer one's operation"
                    .into(),
            });
        }
        let gate = self.gate.clone().lock_owned().await;
        *self.holder.lock() = holder;
        Ok(OperationGuard {
            _gate: gate,
            holder: self.holder.clone(),
        })
    }
}
