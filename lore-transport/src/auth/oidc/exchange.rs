// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Token exchange (RFC 8693): the login token traded at the advertised exchange issuer for
//! one scoped to its target, named by RFC 8707 `resource` values or by a scope value, and
//! an external credential such as an API key traded at the login issuer for a login token.

use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::lore_debug;
use lore_base::lore_warn;
use lore_base::types::RepositoryId;
use lore_credential::domain_in_root_domains;
use lore_credential::get_domain_or_empty;
use lore_credential::insecure_decode_token;
use serde::Deserialize;

use super::body_excerpt;
use super::discovery;
use super::token;
use super::token::Outcome;
use crate::error::ProtocolError;
use crate::types::AuthenticationToken;
use crate::types::AuthorizationToken;
use crate::types::Oidc;

pub const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
pub const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const JWT_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:jwt";
/// Where a partition template puts the partition ID.
const PARTITION_PLACEHOLDER: &str = "{id}";

/// What an exchange asks for: one `resource` parameter per value (RFC 8707 §2), or one
/// `scope`. Never both, since a provider that rejects `resource` fails an exchange carrying
/// both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Resources(Vec<String>),
    Scope(String),
}

/// A successful exchange response, RFC 8693 §2.2.1.
#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    access_token: String,
    issued_token_type: Option<String>,
    token_type: String,
}

/// The target naming `repository` under the template the server advertises, or `None` when
/// it advertises neither template: Tier 1, where the login token is sent everywhere and
/// nothing is exchanged.
pub fn partition_target(
    oidc: &Oidc,
    repository: RepositoryId,
) -> Result<Option<Target>, ProtocolError> {
    let render = |setting: &str, template: &str| {
        if !template.contains(PARTITION_PLACEHOLDER) {
            return Err(ProtocolError::internal(format!(
                "the server advertises a {setting} without {PARTITION_PLACEHOLDER}, so it names \
                 no partition"
            )));
        }
        Ok(template.replace(PARTITION_PLACEHOLDER, &repository.to_string()))
    };
    match (&oidc.resource_template, &oidc.scope_template) {
        (Some(template), None) => Ok(Some(Target::Resources(vec![render(
            "resource_template",
            template,
        )?]))),
        (None, Some(template)) => Ok(Some(Target::Scope(render("scope_template", template)?))),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(ProtocolError::internal(
            "the server advertises both resource_template and scope_template; exactly one names \
             a partition",
        )),
    }
}

/// Whether the deployment exchanges at all, Tier 2.
pub fn is_exchanging(oidc: &Oidc) -> bool {
    oidc.resource_template.is_some() || oidc.scope_template.is_some()
}

/// The authorization token on Tier 1: the login token itself.
pub fn unexchanged(subject_token: &str) -> Result<AuthorizationToken, ProtocolError> {
    let claims = insecure_decode_token(subject_token)
        .map_err(|source| ProtocolError::internal_with_context(source, "reading the login token"))?
        .claims;
    Ok(AuthorizationToken {
        token: subject_token.to_owned(),
        expires_ms: claims.expires * 1000,
        // Populated by the orchestration layer from the token's claims.
        acceptable_root_domains: Vec::new(),
    })
}

/// Exchanges `subject_token`, the login token, for one scoped to `target` at the exchange
/// issuer, which is the login issuer unless the server names another.
///
/// The login token goes to another issuer only if every host it would reach there, the
/// issuer's and its token endpoint's, is one the token itself names: its `root_domains`
/// claim, or its `aud`. The server advertises the exchange issuer, so a rogue server could
/// otherwise collect the token by pairing the victim's issuer with an exchange host it
/// controls. The issuer's host is checked before any request is made.
pub async fn exchange(
    oidc: &Oidc,
    subject_token: &str,
    target: Target,
) -> Result<AuthorizationToken, ProtocolError> {
    let exchange_issuer = oidc
        .token_exchange_issuer
        .as_deref()
        .unwrap_or(&oidc.issuer);
    let permitted_hosts = if exchange_issuer == oidc.issuer {
        None
    } else {
        let hosts = login_token_hosts(oidc, subject_token)?;
        may_receive_login_token(&hosts, exchange_issuer)?;
        Some(hosts)
    };
    let document = discovery::discover(exchange_issuer).await?;
    if let Some(hosts) = &permitted_hosts {
        may_receive_login_token(hosts, &document.token_endpoint)?;
    }

    let mut form = vec![
        ("grant_type", GRANT_TYPE.to_owned()),
        ("client_id", oidc.client_id.clone()),
        ("subject_token", subject_token.to_owned()),
        ("subject_token_type", ACCESS_TOKEN_TYPE.to_owned()),
        ("requested_token_type", ACCESS_TOKEN_TYPE.to_owned()),
    ];
    match &target {
        Target::Resources(resources) => form.extend(
            resources
                .iter()
                .map(|resource| ("resource", resource.clone())),
        ),
        Target::Scope(scope) => form.push(("scope", scope.clone())),
    }

    let response: ExchangeResponse =
        match token::request("token exchange", &document.token_endpoint, form).await? {
            Outcome::Token(response) => response,
            Outcome::Refused(refusal) => return Err(refused(exchange_issuer, &target, &refusal)),
        };
    authorization_token(response)
}

/// Exchanges `subject_token`, an external credential of the type `subject_token_type` names,
/// for the login token of the user it is bound to. The answer is the device grant's (RFC
/// 6749 §5.1), and nothing but `subject_token` names the target.
pub async fn login(
    oidc: &Oidc,
    document: &discovery::DiscoveryDocument,
    subject_token: &str,
    subject_token_type: &str,
) -> Result<AuthenticationToken, ProtocolError> {
    let form = vec![
        ("grant_type", GRANT_TYPE.to_owned()),
        ("client_id", oidc.client_id.clone()),
        ("subject_token", subject_token.to_owned()),
        ("subject_token_type", subject_token_type.to_owned()),
        ("requested_token_type", ACCESS_TOKEN_TYPE.to_owned()),
    ];
    match token::request("token exchange", &document.token_endpoint, form).await? {
        Outcome::Token(response) => token::authentication_token(response),
        // A refusal of the credential itself: unknown, revoked, or bound to a user who may
        // not log in, which the provider does not tell apart.
        Outcome::Refused(refusal) if refusal.error == "invalid_grant" => {
            lore_debug!(
                "OIDC token exchange at {} refused the {subject_token_type} credential: {}",
                oidc.issuer,
                refusal.describe()
            );
            Err(NotAuthenticated.into())
        }
        Outcome::Refused(refusal) => Err(ProtocolError::internal(format!(
            "OIDC token exchange at {} refused the {} credential: {}",
            oidc.issuer,
            body_excerpt(subject_token_type),
            refusal.describe()
        ))),
    }
}

/// The hosts the login token may be sent to: those its claims name, and the login
/// issuer's own, which the token store also records for it.
fn login_token_hosts(oidc: &Oidc, subject_token: &str) -> Result<Vec<String>, ProtocolError> {
    let claims = insecure_decode_token(subject_token)
        .map_err(|source| ProtocolError::internal_with_context(source, "reading the login token"))?
        .claims;
    let mut hosts = claims.acceptable_root_domains();
    let login_host = get_domain_or_empty(&oidc.issuer);
    if !login_host.is_empty() {
        hosts.push(login_host);
    }
    Ok(hosts)
}

/// Refuses the exchange unless the host of `url` is one of `permitted_hosts`.
fn may_receive_login_token(permitted_hosts: &[String], url: &str) -> Result<(), ProtocolError> {
    let host = get_domain_or_empty(url);
    if !host.is_empty() && domain_in_root_domains(&host, permitted_hosts) {
        return Ok(());
    }
    lore_warn!(
        "Not sending the login token to token exchange host '{host}': the token's \
         'root_domains' claim does not name it"
    );
    Err(ProtocolError::internal(format!(
        "The login token may not be sent to token exchange host '{host}', which its \
         'root_domains' claim does not name: have the issuer add '{host}' to the token's \
         'root_domains' claim, or set the server's token_exchange_issuer to the login issuer"
    )))
}

/// The error a refused exchange maps to. A refusal of the subject token is
/// [`NotAuthenticated`]; a refusal of what it asked for is [`NotAuthorized`], since that is
/// how a provider declines a resource the user holds no grant on.
fn refused(issuer: &str, target: &Target, refusal: &token::ErrorResponse) -> ProtocolError {
    match refusal.error.as_str() {
        "invalid_grant" => {
            lore_debug!(
                "OIDC token exchange at {issuer} refused the login token: {}",
                refusal.describe()
            );
            NotAuthenticated.into()
        }
        "access_denied" | "invalid_target" | "invalid_scope" => {
            lore_debug!(
                "OIDC token exchange at {issuer} refused {target:?}: {}",
                refusal.describe()
            );
            NotAuthorized.into()
        }
        _ => ProtocolError::internal(format!(
            "OIDC token exchange at {issuer} refused the request: {}",
            refusal.describe()
        )),
    }
}

fn authorization_token(response: ExchangeResponse) -> Result<AuthorizationToken, ProtocolError> {
    let ExchangeResponse {
        access_token,
        issued_token_type,
        token_type,
    } = response;
    if let Some(issued) = issued_token_type
        && issued != ACCESS_TOKEN_TYPE
        && issued != JWT_TOKEN_TYPE
    {
        return Err(ProtocolError::internal(format!(
            "OIDC token exchange issued a token of type '{}'; an access token was requested",
            body_excerpt(&issued)
        )));
    }
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(ProtocolError::internal(format!(
            "OIDC token exchange issued a token of type '{}'; only Bearer is supported",
            body_excerpt(&token_type)
        )));
    }
    let Ok(decoded) = insecure_decode_token(&access_token) else {
        return Err(ProtocolError::internal(
            "OIDC token exchange issued an access token that is not a JWT carrying the \
             standard claims",
        ));
    };
    Ok(AuthorizationToken {
        token: access_token,
        expires_ms: decoded.claims.expires * 1000,
        // Populated by the orchestration layer from the token's claims.
        acceptable_root_domains: Vec::new(),
    })
}
