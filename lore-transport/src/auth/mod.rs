// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod exchange;
pub mod ucs_auth;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;

use parking_lot::Mutex;

use crate::error::ProtocolError;
use crate::traits::Authentication;

/// Scheme-based registry for `Authentication` implementations.
///
/// The auth URL scheme identifies which implementation handles a given auth
/// endpoint. The server sets the auth URL (including scheme) in
/// `EnvironmentEndpoint.auth_url`. The client parses the scheme to look up
/// the implementation, and passes the full auth URL through to it.
pub mod authentication {
    use super::*;

    static AUTHENTICATION_MAP: Mutex<Option<HashMap<String, Arc<dyn Authentication>>>> =
        Mutex::new(None);

    static REGISTER_BUILTIN_AUTHENTICATION: Once = Once::new();

    /// Extracts the scheme from an auth URL (the part before `://`).
    pub fn parse_scheme(auth_url: &str) -> Result<&str, ProtocolError> {
        auth_url
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .ok_or(ProtocolError::internal(format!(
                "invalid auth URL (missing scheme): '{auth_url}'"
            )))
    }

    /// Finds the `Authentication` implementation for the given auth URL by
    /// parsing its scheme. Registers builtin implementations on first call.
    ///
    /// The full auth URL (including scheme) is passed to trait methods as-is --
    /// the implementation decides how to interpret it.
    pub fn find(auth_url: &str) -> Result<Arc<dyn Authentication>, ProtocolError> {
        REGISTER_BUILTIN_AUTHENTICATION.call_once(|| {
            let ucs_auth = Arc::new(ucs_auth::UcsAuthentication);
            let _ = add("ucs-auth", ucs_auth.clone());
            let _ = add("https", ucs_auth.clone()); // transition fallback
            let _ = add("http", ucs_auth); // local development fallback
        });

        let scheme = parse_scheme(auth_url)?;
        // Collect result and available schemes under a single lock acquisition
        let (result, available) = {
            let map = AUTHENTICATION_MAP.lock();
            let result = map.as_ref().and_then(|m| m.get(scheme).cloned());
            let available: Vec<String> = map
                .as_ref()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            (result, available)
        };
        match result {
            Some(auth) => Ok(auth),
            None => Err(ProtocolError::internal(format!(
                "no authentication implementation registered for scheme '{scheme}' (available: {available:?})",
            ))),
        }
    }

    /// Registers an `Authentication` implementation for the given scheme.
    pub fn add(scheme: &str, auth: Arc<dyn Authentication>) -> Result<(), ProtocolError> {
        let mut map = AUTHENTICATION_MAP.lock();
        if map.is_none() {
            *map = Some(HashMap::new());
        }
        map.as_mut().unwrap().insert(scheme.to_string(), auth);
        Ok(())
    }

    /// Lists registered scheme names (for diagnostics).
    pub fn schemes() -> Vec<String> {
        let map = AUTHENTICATION_MAP.lock();
        match map.as_ref() {
            Some(m) => m.keys().cloned().collect(),
            None => Vec::new(),
        }
    }
}
