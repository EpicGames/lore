// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Display;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// Which authentication path a client takes if a server advertises both its
/// gRPC auth service and an OIDC issuer. A path the server does not
/// advertise cannot be taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Follow the server's preference: the OIDC path if the server marks it
    /// preferred, the gRPC path otherwise.
    #[default]
    Auto = 0,
    /// The gRPC path, through the auth service at `auth_url`.
    Grpc = 1,
    /// The OIDC path, through the advertised OIDC issuer.
    Oidc = 2,
}

/// One of the two paths, once [`AuthMode::Auto`] has been resolved against the
/// server's preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPath {
    Grpc,
    Oidc,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not an auth mode. Choose one of [auto, grpc, oidc].")]
pub struct UnknownAuthMode(pub String);

impl AuthMode {
    /// The name the mode is written as.
    pub fn name(self) -> &'static str {
        match self {
            AuthMode::Auto => "auto",
            AuthMode::Grpc => "grpc",
            AuthMode::Oidc => "oidc",
        }
    }

    pub fn path(self, oidc_preferred: bool) -> AuthPath {
        match self {
            AuthMode::Grpc => AuthPath::Grpc,
            AuthMode::Oidc => AuthPath::Oidc,
            AuthMode::Auto => {
                if oidc_preferred {
                    AuthPath::Oidc
                } else {
                    AuthPath::Grpc
                }
            }
        }
    }
}

impl FromStr for AuthMode {
    type Err = UnknownAuthMode;

    /// Reads a name case-insensitively, ignoring surrounding space.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(AuthMode::Auto),
            "grpc" => Ok(AuthMode::Grpc),
            "oidc" => Ok(AuthMode::Oidc),
            _ => Err(UnknownAuthMode(value.trim().to_string())),
        }
    }
}

impl Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
