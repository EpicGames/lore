// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The environment variables that locate a caller's configuration and credentials.
//!
//! A call relayed to the Lore service carries its caller's values of these and reads them in
//! place of the service's own, so that it finds the configuration and credentials it would have
//! found in the caller's process.

use std::any::Any;
use std::sync::OnceLock;

use crate::runtime::LORE_CONTEXT;

/// Directory holding the global configuration and data, in place of the per-user default.
pub const GLOBAL_PATH_VAR: &str = "LORE_GLOBAL_PATH";

/// Directory holding the token store, in place of the per-user default.
pub const AUTH_PATH_VAR: &str = "LORE_AUTH_PATH";

/// The variables a call carries.
const CALL_VARIABLES: [&str; 2] = [GLOBAL_PATH_VAR, AUTH_PATH_VAR];

/// A process's values of the variables a call carries, unset ones included, so that a variable
/// its caller left unset is not read from the service's environment either.
#[lore_macro::test_pub]
#[derive(Clone, Debug, Default, PartialEq, bitcode::Encode, bitcode::Decode)]
pub struct CallEnvironment {
    values: [Option<String>; CALL_VARIABLES.len()],
}

impl CallEnvironment {
    /// This process's values.
    pub fn capture() -> Self {
        Self {
            values: CALL_VARIABLES.map(|name| std::env::var(name).ok()),
        }
    }

    /// The value carried for `name`, or `None` for a variable a call does not carry.
    fn get(&self, name: &str) -> Option<&Option<String>> {
        let index = CALL_VARIABLES
            .iter()
            .position(|variable| *variable == name)?;
        Some(&self.values[index])
    }
}

/// Finds the environment a [`LORE_CONTEXT`] value carries.
pub type ContextEnvironment =
    for<'a> fn(&'a (dyn Any + Send + Sync)) -> Option<&'a CallEnvironment>;

static CONTEXT_ENVIRONMENT: OnceLock<ContextEnvironment> = OnceLock::new();

/// Sets how [`var`] finds the environment a [`LORE_CONTEXT`] value carries, for the crate that
/// defines the value: this crate propagates the context without knowing its type.
pub fn find_context_environment_with(find: ContextEnvironment) {
    CONTEXT_ENVIRONMENT.get_or_init(|| find);
}

/// The value of `name` for the call this task runs: the one the call carries, and this process's
/// for a variable it does not carry or outside any call.
pub fn var(name: &str) -> Option<String> {
    let carried = CONTEXT_ENVIRONMENT.get().and_then(|find| {
        LORE_CONTEXT
            .try_with(|context| find(context.as_ref())?.get(name).cloned())
            .ok()
            .flatten()
    });
    carried.unwrap_or_else(|| std::env::var(name).ok())
}
