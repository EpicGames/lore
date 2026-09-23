// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod exchange;
pub mod token_only;
pub mod ucs_auth;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;

use parking_lot::Mutex;

use crate::error::ProtocolError;
use crate::traits::Authentication;
use crate::traits::UserService;

static REGISTER_BUILTIN: Once = Once::new();

fn register_builtin() {
    REGISTER_BUILTIN.call_once(|| {
        let ucs_auth = Arc::new(ucs_auth::UcsAuthentication);
        for scheme in ucs_auth::SCHEMES {
            let _ = authentication::add(scheme, ucs_auth.clone());
            let _ = user_service::add(scheme, ucs_auth.clone());
        }
    });
}

/// Extracts the scheme from an auth URL (the part before `://`).
pub fn parse_scheme(auth_url: &str) -> Result<&str, ProtocolError> {
    auth_url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .ok_or_else(|| {
            ProtocolError::internal(format!("invalid auth URL (missing scheme): '{auth_url}'"))
        })
}

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

    /// Finds the `Authentication` implementation for the given auth URL by
    /// parsing its scheme. Registers builtin implementations on first call.
    ///
    /// The full auth URL (including scheme) is passed to trait methods as-is --
    /// the implementation decides how to interpret it.
    pub fn find(auth_url: &str) -> Result<Arc<dyn Authentication>, ProtocolError> {
        register_builtin();

        let scheme = parse_scheme(auth_url)?;
        // The lookup and the scheme list the error reports share one lock
        // acquisition, but the list is only collected on a miss, so a hit never
        // pays to clone every registered key.
        let found = {
            let map = AUTHENTICATION_MAP.lock();
            let auth = map.as_ref().and_then(|m| m.get(scheme).cloned());
            auth.ok_or_else(|| {
                map.as_ref()
                    .map(|m| m.keys().cloned().collect::<Vec<String>>())
                    .unwrap_or_default()
            })
        };
        found.map_err(|available| {
            ProtocolError::internal(format!(
                "no authentication implementation registered for scheme '{scheme}' (available: {available:?})",
            ))
        })
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

/// Scheme-based registry for `UserService` implementations, keyed like
/// [`authentication`] but on the service URL, which a server may advertise
/// separately from its auth URL.
///
/// Unlike authentication, a service is optional: a scheme with none
/// registered gets [`token_only::TokenOnlyUserService`], which names the
/// bearer from their own token and echoes every other ID.
pub mod user_service {
    use super::*;

    static USER_SERVICE_MAP: Mutex<Option<HashMap<String, Arc<dyn UserService>>>> =
        Mutex::new(None);

    /// The `UserService` for the given user service URL.
    /// Registers builtin implementations on first call.
    pub fn find(user_url: &str) -> Arc<dyn UserService> {
        register_builtin();

        let registered = parse_scheme(user_url).ok().and_then(|scheme| {
            USER_SERVICE_MAP
                .lock()
                .as_ref()
                .and_then(|m| m.get(scheme).cloned())
        });
        registered.unwrap_or_else(|| Arc::new(token_only::TokenOnlyUserService))
    }

    /// Registers a `UserService` implementation for the given scheme.
    pub fn add(scheme: &str, service: Arc<dyn UserService>) -> Result<(), ProtocolError> {
        let mut map = USER_SERVICE_MAP.lock();
        if map.is_none() {
            *map = Some(HashMap::new());
        }
        map.as_mut().unwrap().insert(scheme.to_string(), service);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scheme with no user service still resolves users: the
    /// bearer from their token, everyone else as their ID.
    #[tokio::test]
    async fn an_unregistered_scheme_gets_the_token_only_service() {
        /// `{"iss":"lore","sub":"alice","name":"Alice","exp":2000000000,"aud":["example.com"]}`
        const ALICE_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJsb3JlIiwic3ViIjoiYWxpY2UiLCJuYW1lIjoiQWxpY2UiLCJleHAiOjIwMDAwMDAwMDAsImF1ZCI6WyJleGFtcGxlLmNvbSJdfQ.signature";
        const AUTH_URL: &str = "no-service-here://auth.test.invalid";

        let users = user_service::find(AUTH_URL)
            .get_user_info(
                AUTH_URL,
                ALICE_TOKEN,
                lore_base::types::RepositoryId::default(),
                &["alice".to_string(), "bob".to_string()],
                "corr",
            )
            .await
            .expect("the default service never fails");
        let names: Vec<&str> = users.iter().map(|u| u.user_name.as_str()).collect();
        assert_eq!(names, ["Alice", "bob"]);
    }

    /// The builtin schemes resolve through the auth service, exactly as
    /// before the service was split from authentication.
    #[test]
    fn builtin_schemes_are_served_by_the_auth_service() {
        for scheme in ucs_auth::SCHEMES {
            let auth_url = format!("{scheme}://auth.example.com");
            let service = user_service::find(&auth_url);
            let auth = authentication::find(&auth_url).expect("a builtin scheme");
            assert!(
                std::ptr::addr_eq(Arc::as_ptr(&service), Arc::as_ptr(&auth)),
                "scheme {scheme} should resolve users through the same auth service instance"
            );
        }
    }
}
