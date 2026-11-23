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
- Existing tokens must keep their current permissions. On OIDC Tier 2, a single issuer can mint both login tokens and exchanged access tokens, so the server can only tell them apart by their grant entries: login tokens carry none, and exchanged access tokens carry at least one.
- An access token should resolve only the repositories it grants access to.
- Repository listing belongs to the configured repository catalog, which decides what each token can see.

## Considered Options

- Send the access token as a fallback and keep the server's authorization behavior unchanged.
- Send the access token as a fallback, and on Tier 2 limit repository lookup for tokens with per-repository grants to the repositories those grants match.
- Send the access token as a fallback, and limit tokens with per-repository grants to looking up and listing only those repositories while blocking repository creation.
- Keep the repository service identity-token-only and add another way for the client to resolve repository names.

## Decision Outcome

Chosen option: "Send the access token as a fallback, and on Tier 2 limit repository lookup for tokens with per-repository grants to the repositories those grants match," because it matches the legacy auth-service tier's lookup scope on Tier 2 without affecting Tier 2 login tokens, which carry no per-repository grants. Restricting listing and creation isn't needed for clone and push.

When the client has an identity token, it sends that token to the repository service as before. When only an access token is supplied, the client sends the access token instead. Server handling works as follows:

- `RepositoryGet` and `RepositoryQuery` run the authorizer's lookup check and return not found when lookup is denied:
  - On the legacy auth-service tier, the server evaluates an access token against its `resources` claim, so it resolves only the repositories named in that claim.
  - On Tier 2, a token with nothing at the configured resource claim (the claim is absent, null or an empty array) resolves any repository. A token with anything at the claim resolves only the repositories an entry matches, including a wildcard entry, so it resolves none when no entry is readable.
- `RepositoryList` delegates to the configured repository catalog, which decides what to return for the token.
- `RepositoryCreate` and single-repository calls stay unchanged.

### Consequences

- Good, because `lore repository clone <repository> --access-token <token>` and pushing from that instance work without a login, while Tier 2 login tokens and auth-service tokens keep their capabilities.
- Good, because on both the legacy auth-service tier and Tier 2, lookups for repositories outside the access token's grants return not found. (`RepositoryCreate` still rejects taken names, which is an accepted way to check whether a name exists.)
- Bad, because the Tier 2 rule relies on issuer conventions: an access token minted with an empty resource claim can resolve any repository, while a login token that carries grants for only a subset of repositories can't resolve the rest (preventing the client from exchanging for them).
- Neutral, because on a Tier 2 deployment whose identity provider puts all of a user's grants directly in the login token, lookups for repositories without a grant now return not found instead of resolving and then failing on repository calls.
- Neutral, because `lore repository list` with an access token depends on the configured catalog. If the catalog's upstream rejects an access token as a credential, listing fails.
