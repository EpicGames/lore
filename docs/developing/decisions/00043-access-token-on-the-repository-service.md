---
status: accepted
date: 2026-11-22
---

# ADR-00043: Fall back to access token on the repository service

## Context and Problem Statement

The client sent the identity token to the repository service for calls that don't target a specific repository (`RepositoryGet`, `RepositoryQuery`, `RepositoryList`, `RepositoryCreate`). On the legacy auth-service path, it also sent the identity token for single-repository calls (`RepositoryDelete`, `RepositoryMetadataGet`, `RepositoryMetadataSet`). When the caller supplied only an access token, the client sent no token at all.

A process scoped to a single repository, such as an agent, only receives an access token for that repository. Passing an identity token isn't safe because an identity token can be exchanged for access tokens to any of the user's repositories. Without a token on `RepositoryGet`, the client couldn't resolve a repository name to its ID, so `lore repository clone --access-token` failed.

Which token should the client send to the repository service when it has no identity token, and how should the server handle it?

## Decision Drivers

- `lore clone` and `lore push` must work with only `--access-token`, which requires resolving a repository name to its ID.
- Existing tokens must keep their current permissions. On OIDC Tier 2, when a single issuer mints both login tokens and exchanged access tokens, the server can't reliably tell them apart. Any rule that restricts tokens based on per-repository grant claims would also break Tier 2 login tokens that carry those claims.
- Repository listing belongs to the configured repository catalog, which decides what each token can see.

## Considered Options

- Send the access token as a fallback and keep the server's authorization behavior unchanged.
- Send the access token as a fallback, and restrict tokens that carry per-repository grants to looking up and listing only those repositories while blocking repository creation.
- Keep the repository service identity-token-only and add another way for the client to resolve repository names.

## Decision Outcome

Chosen option: "Send the access token as a fallback and keep the server's authorization behavior unchanged," because every auth tier already handles repository lookups for access tokens, and restricting tokens by grant claims would remove behavior from Tier 2 login tokens that carry per-repository grants.

When the client has an identity token, it sends that token to the repository service as before. When only an access token is supplied, the client sends the access token instead. Server handling stays unchanged:

- `RepositoryGet` and `RepositoryQuery` run the authorizer's lookup check. On the legacy auth-service tier, the server evaluates an access token against its `resources` claim, so it resolves only the repositories named in that claim and returns not found for anything else.
- `RepositoryList` delegates to the configured repository catalog, which decides what to return for the token.
- `RepositoryCreate` and single-repository calls stay unchanged.

### Consequences

- Good, because `lore repository clone <repository> --access-token <token>` and pushing from that instance work without a login, and no existing token loses capabilities.
- Good, because on the legacy auth-service tier, lookups for repositories outside the access token's `resources` claim return not found. (`RepositoryCreate` still rejects taken names, which is an accepted way to check whether a name exists.)
- Bad, because on Tier 2 an access token can resolve the name and ID of any repository on the server, just like any other verified token. It still can't read any repository contents. Restricting lookup on Tier 2 would require a way to distinguish exchanged access tokens from login tokens, such as a separate `token_exchange_issuer` or a catalog that filters per token.
- Neutral, because `lore repository list` with an access token depends on the configured catalog. If the catalog's upstream rejects an access token as a credential, listing fails.
