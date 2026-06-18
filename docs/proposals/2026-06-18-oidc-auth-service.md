---
lep: 2026-06-18-oidc-auth-service
title: OIDC authentication service for Lore Server
authors:
  - dzmitryj (Dimitri Mitchell)
status: Draft
created: 2026-06-18
updated: 2026-06-18
discussion: <TBD — fill in CR link when the discussion CR is opened>
---

# OIDC Authentication Service for Lore Server

## Summary

This proposal adds a login service that lets users sign in to Lore through an OpenID Connect (OIDC) provider. The service authenticates the user and issues a Lore token that Lore Server already verifies. It defines the token the service issues, asymmetric signing so the public keys can be shared safely, the login flow, and how the provider is configured. `lore-auth-server` is the implementation.

## Motivation

Lore Server verifies a bearer JWT on every authenticated request. `lore-server/src/auth/jwt.rs` decodes the token and `lore-server/src/auth/jwk.rs` fetches its signing keys from a configured JWKS endpoint. Nothing in the tree issues those tokens from an external identity provider, so there is no way for a real user to log in. The verifier exists; nothing fills it.

The driver is running Lore in production for UEFN, where users authenticate through an OIDC provider. The design stays provider-generic so the same service works for any conformant OIDC IdP, with the UEFN provider as the first configured one.

An early prototype signed tokens with HS256 and published the symmetric secret in its JWKS as an `oct` key. Anyone who can read `/.well-known/jwks.json` then holds the signing key and can forge tokens Lore Server trusts. That rules out any internet-facing deployment. Asymmetric signing, where the JWKS carries only a public key, is the requirement that unblocks production.

## Goals / Non-Goals

### Goals

1. **Mint Lore JWTs from an OIDC identity.** The tokens decode under the existing paths in `lore-server/src/auth/jwt.rs` and `lore-credential/src/jwt.rs`, so Lore Server's verification does not change.

2. **Sign asymmetrically and publish only public keys.** Use an asymmetric algorithm Lore Server's verifier already accepts, and serve a JWKS that exposes the public key and never the secret.

3. **Reuse Lore's existing key material.** Use PEM keys, the kind Lore already generates for transport TLS (`lore-transport/src/tls.rs`), rather than a new key format.

4. **Support standard OIDC login.** Authorization code with PKCE (S256), plus non-interactive exchange of an upstream ID token, with `state`, `nonce`, and a configured claim policy.

5. **Use an exact, registered callback URL.** The redirect URI comes from configuration and is never taken from the request.

6. **Keep providers in configuration.** Issuer, endpoints, credentials, claim policy, and resource policy live in config. Lore Server and the Lore CLI carry no provider-specific code.

### Non-Goals

- **A general authorization model.** Resource policy is a configured allowlist; the permission RPCs (`CheckUserPermission`, `LookupUserPermissions`) are not implemented.
- **A horizontally scalable session store.** This proposal does not specify how login sessions are shared across replicas.
- **Refresh tokens.**
- **Rate limiting** on the public RPCs.
- **In-process TLS.** The service is expected to run behind a TLS-terminating proxy.
- **Key rotation tooling** beyond a single active signing key.
- **New Lore wire-protocol verbs or Lore-Server-side auth changes.**

## Proposed Design

The service sits between an OIDC provider and Lore Server. It authenticates the user, then issues Lore tokens over the existing UCS Auth API. The requirements below cover what the service must do to run Lore with single sign-on.

### Token contract

The tokens the service issues must decode under `lore-server/src/auth/jwt.rs` (server side) and `lore-credential/src/jwt.rs` (client side). The authentication token carries `sub`, `iss`, `iat`, `exp`, `aud`, `name`, `preferred_username`, `idp`, and optional `groups`. The authorization token adds `resources: [{ resource_id, permission }]`. Lore Server already enforces this contract; it does not change.

This addresses **Goal 1**.

### Asymmetric signing and JWKS

Signing must be asymmetric, and the JWKS must publish only public keys. A symmetric scheme is unacceptable because the JWKS would expose the signing secret. The algorithm must be one Lore Server's verifier already accepts: `lore-server/src/auth/jwk.rs` reads each key's `alg` and builds the decoding key with `DecodingKey::from_jwk`, so any JWKS-expressible asymmetric algorithm (for example ES256 or RS256) works with no Lore Server change. Reusing Lore's existing PEM key material is preferred over introducing a new key format.

This addresses **Goal 2** and **Goal 3**.

### OIDC login

Browser login uses the authorization-code flow with PKCE (S256 challenge), `state`, and `nonce`. The redirect URI must be configured and server-controlled: the same value in the authorization request and the token exchange, validated as an absolute http or https URL, and never read from the inbound request. Non-interactive callers exchange an upstream OIDC ID token, which the service validates against the provider's JWKS, issuer, audience, and nonce before applying the claim policy.

This addresses **Goal 4** and **Goal 5**.

### Configuration

Provider issuer, endpoints, and credentials, the claim policy (`require_email_verified`, `allowed_email_domains`, `allowed_hosted_domains`), and the resource policy (`allowed_resources`, `default_permissions`) all come from configuration. Lore Server and the CLI carry none of it.

This addresses **Goal 6**.

### Reference implementation

`lore-auth-server` implements the requirements above and is the service targeted at the initial UEFN deployment. Its main choices:

- A new workspace crate running two listeners: a gRPC server for the UCS Auth API (`urc_auth_api_server::UrcAuthApi`, generated from the existing `auth_api.proto`) and an axum HTTP server for `GET /oidc/callback` and `GET /.well-known/jwks.json`.
- ES256 over ECDSA P-256, loaded from a PEM PKCS#8 key file, with an ephemeral `rcgen` key for local development. The JWKS publishes the public EC key only, and the service verifies its own tokens through `DecodingKey::from_jwk` against the key it publishes.
- An exact `oidc.redirect_uri`, used verbatim when set and otherwise derived from `public_base_url` and `redirect_path`, validated at startup.
- In-memory session and user stores for pending logins and resolved users. A multi-replica deployment needs a shared store instead.

## Compatibility

- **Wire format** - N/A. No Lore wire messages, framing, or serialization change.

- **Client/server protocols** - The proposal adds a server implementation of the existing UCS Auth API; no RPC request or response shapes change. Signing must be asymmetric, so the JWKS serves a public key rather than the symmetric `oct` key the early prototype published. Lore Server's JWKS client reads `alg` and calls `DecodingKey::from_jwk`, so it verifies the new tokens without code changes. The `ucs-auth://` scheme is already registered in `lore-transport/src/auth/mod.rs`.

- **On-disk format** - N/A. Repositories, fragments, and revision records are untouched.

- **CLI and public API** - Additive. The service ships as a separate binary with its own configuration file; it adds no `lore` subcommand syntax, exit code, or output-format change. The auth-service configuration surface (signing key, callback URL, provider, policy) is new and is owned by the service, not by the `lore` CLI.

## Non-Functional Considerations

- **Concurrency** - Token issuance is independent per request. The only shared mutable state is the session and user store; a single-writer model is sufficient, and signing needs no lock once the key is loaded.

- **Memory** - Small and bounded. Tokens and session records are tiny, and nothing buffers data proportional to repository or file size.

- **Statelessness** - The service holds process-local state: pending login sessions and resolved users. Whether that state stays in process or moves to a shared store determines whether the service can run more than one replica.

- **Determinism** - Not applicable to Lore history. Token contents depend on the clock (`iat`, `exp`) and the upstream claims; a given key and claim set produce a stable signature.

## Migration Plan

N/A - no breaking changes to the Lore wire format, on-disk format, or CLI, so no data migration is required. A deployment moving off the early prototype swaps its symmetric signing secret for an asymmetric key and points `[server.auth.jwk].endpoint` at the service. The service is not yet in production, so no live tokens need migrating; outstanding tokens expire on their own.

## Security Considerations

The service mints the tokens Lore Server trusts, so it is a trust boundary of its own.

Asymmetric signing is the core security requirement. With it, the JWKS carries only a public key, so forging a token requires the private key, which stays on the service. A symmetric scheme that publishes its key in JWKS, as the early prototype did, does not meet that requirement.

The signing key is held outside the repository and supplied from a secret manager with restricted permissions. Any local-development convenience key must not be usable in production.

The OIDC flow uses PKCE S256, `state`, and `nonce`. The redirect URI is exact, configured, validated, and identical in the authorization request and the token exchange, so a crafted request cannot steer it to an attacker's URL.

The service is expected to speak plaintext behind a TLS-terminating proxy, with its listeners bound to loopback so only the proxy reaches them.

The initial deployment carries three known gaps: no rate limiting on `StartAuthSession` or the token-exchange RPCs, process-local session and user state, and a permissive default resource policy (`allowed_resources = ["urc-*"]`) that grants every authenticated user every repository. They are tolerable for a single-tenant, single-replica launch behind a rate-limiting proxy. The service must not log OIDC codes, ID tokens, Lore JWTs, client secrets, or bearer headers.

## Privacy Considerations

The service handles identity from the IdP: subject, email, display name, groups, and the hosted-domain claim. It writes a subset (`sub`, `name`, `preferred_username`, `idp`, `groups`) into the token. That is the identity Lore tokens already carry, so no new category of data enters the token. Process-local user state holds it for the process lifetime and adds no durable store. OIDC codes, ID tokens, JWTs, secrets, and bearer headers are never logged.

## Risks and Assumptions

**Assumptions**

- **Assumption:** Lore Server keeps an algorithm-agnostic verifier that reads `alg` and builds keys with `DecodingKey::from_jwk`. *Invalidated if:* a later change pins an HMAC algorithm or stops accepting JWK-supplied keys.

- **Assumption:** The target IdP is a conformant OIDC provider with standard authorization, token, and JWKS endpoints. *Invalidated if:* a deployment must use an IdP that needs non-standard flows.

**Risks**

- **Risk:** Process-local session state does not survive across replicas, so a load-balanced deployment loses the session on whichever replica did not start the login. *Mitigation:* run one replica, or pin sessions, until a shared store lands.

- **Risk:** The public RPCs have no rate limiting, so login traffic can exhaust the service. *Mitigation:* rate-limit at the fronting proxy for now; in-service limiting is follow-on.

- **Risk:** A permissive default resource policy grants every authenticated user every repository. *Mitigation:* deployments set an explicit allowlist; per-repository authorization depends on a permission model this proposal does not define.

- **Risk:** A single signing key with no rotation procedure means a compromise forces a hard cutover. *Mitigation:* the JWKS format already allows several keys by `kid`; a rotation procedure is follow-on.

## Drawbacks

- Another service to deploy, configure, run behind TLS, and operate.
- A token and JWKS contract (an asymmetric algorithm, the Lore claim shape) that Lore Server and clients now depend on.

## Alternatives Considered

### Keep symmetric signing with a shared secret

Continue signing with a symmetric secret shared between the service and every verifier.

*Trade-off:* the secret has to reach every verifier, and the early prototype published it in JWKS. Either way, anyone with the JWKS or a verifier's config can mint tokens Lore Server trusts. This proposal builds on asymmetric signing to avoid that.

### Verify upstream IdP tokens in Lore Server

Drop the issuer and have Lore Server validate provider ID tokens directly.

*Trade-off:* this moves OIDC discovery, JWKS handling, and claim policy into Lore Server and every client, and ties Lore Server's releases to provider specifics. A separate issuer keeps Lore Server's token model fixed and provider-agnostic.

### Use an off-the-shelf OAuth2 proxy to issue tokens

Front Lore with a generic OAuth2 gateway.

*Trade-off:* Lore tokens have a specific claim shape (`resources`, `env`, `idp`) and a resource-scoped exchange step. A generic proxy does not produce them, so a Lore-specific mapping layer is needed regardless.

### RS256 instead of ES256 in the reference implementation

Sign with RSA rather than ECDSA.

*Trade-off:* ES256 over P-256 reuses the key material Lore already generates for TLS and produces a smaller key and JWKS. RS256 works where an IdP or HSM requires it; the publisher and verifier accept either, so this is a configuration choice rather than a structural one.

## Prior Art

- **OIDC and OAuth 2.0.** The login is OpenID Connect Core over the OAuth 2.0 authorization-code flow with PKCE ([RFC 7636](https://www.rfc-editor.org/rfc/rfc7636)).
- **JOSE.** Tokens are JWTs ([RFC 7519](https://www.rfc-editor.org/rfc/rfc7519)) signed with an asymmetric algorithm such as ES256 ([RFC 7518](https://www.rfc-editor.org/rfc/rfc7518)); keys publish as a JWK Set ([RFC 7517](https://www.rfc-editor.org/rfc/rfc7517)).
- **Token brokers.** Issuing a service's own tokens from an upstream identity is common. [Dex](https://dexidp.io/) and [oauth2-proxy](https://oauth2-proxy.github.io/oauth2-proxy/) broker upstream OIDC identity, and OAuth 2.0 Token Exchange ([RFC 8693](https://www.rfc-editor.org/rfc/rfc8693)) standardizes swapping an external token for a service token. This service is a narrow, Lore-shaped version of that pattern.

## Unresolved Questions

- What backs a shared session and user store for more than one replica, and how are sessions expired and protected against replay?
- Where does rate limiting belong, and at what limits?
- What is the real authorization model? Answering it means a policy source behind `CheckUserPermission` and `LookupUserPermissions` and dropping the `urc-*` default.
- What is the key-rotation procedure, and should signing move to an HSM- or KMS-backed key?
- Are refresh tokens needed, or do short-lived tokens with re-login suffice?
