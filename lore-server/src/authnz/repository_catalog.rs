// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use async_trait::async_trait;
use lore_base::types::Context;
use lore_base::types::RepositoryId;
use lore_proto::auth::LookupUserPermissionsRequest;
use lore_proto::auth::LookupUserPermissionsResponse;
use lore_revision::lore_debug;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use tokio_stream::StreamExt;
use tonic::Code;
use tonic::Status;
use tracing::info;
use tracing::warn;

use super::auth::grpc_get_auth_client;
use super::common::create_request_with_authorization;
use super::repository_authorizer::VerifiedToken;
use super::repository_authorizer::bearer_header;
use super::repository_authorizer::select_repository_authorizer;
use crate::grpc::FilterSlowDownExt;
use crate::grpc::ServerResultExt;
use crate::settings::AuthSettings;
use crate::settings::BaselineAccess;
use crate::settings::RepositoryCatalogMode;

/// Which partitions a caller may see. Enumeration needs to do a search over the
/// grant store. The information is not available in the tokens, and this is not
/// a standard OIDC feature. Therefore this functionality lives off the
/// authorization path on this optional trait.
#[async_trait]
pub trait RepositoryCatalog: Send + Sync {
    /// The partitions `token` may see, paginated. Returns a list of repository IDs,
    /// and an `Option<String>` continuation token (`None`, if this was the last
    /// page). `page_size` of `None` or zero means no limit.
    async fn list_repositories(
        &self,
        token: Option<&VerifiedToken<'_>>,
        page_size: Option<u32>,
        page_token: Option<&str>,
    ) -> Result<(Vec<RepositoryId>, Option<String>), Status>;
}

impl dyn RepositoryCatalog {
    /// Every partition the caller may see, following continuation tokens
    /// until the catalog returns none, within `budget` for the whole walk.
    /// The v1 list handler streams and so runs under no request timeout, so
    /// this is what bounds an upstream that never stops issuing tokens; a
    /// token seen before is refused at once rather than paid for in time.
    pub async fn list_all(
        &self,
        token: Option<&VerifiedToken<'_>>,
        budget: Duration,
    ) -> Result<Vec<RepositoryId>, Status> {
        let deadline = Instant::now() + budget;
        let mut repositories = Vec::new();
        let mut page_token: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Status::deadline_exceeded(
                    "Repository catalog listing exceeded the request deadline",
                ));
            }
            let (page, next) = tokio::time::timeout(
                remaining,
                self.list_repositories(token, None, page_token.as_deref()),
            )
            .await
            .map_err(|_elapsed| {
                Status::deadline_exceeded(
                    "Repository catalog listing exceeded the request deadline",
                )
            })??;
            repositories.extend(page);
            match next {
                Some(next) => {
                    if !seen.insert(next.clone()) {
                        return Err(Status::internal(
                            "Repository catalog repeated a page token; the listing does not progress",
                        ));
                    }
                    page_token = Some(next);
                }
                None => return Ok(repositories),
            }
        }
    }
}

/// The default catalog, driven by `baseline_access`: `Denied` lists
/// nothing, `Reachable` lists every partition the server holds, enumerated
/// from the mutable store.
pub struct BaselineRepositoryCatalog {
    baseline: BaselineAccess,
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
}

impl BaselineRepositoryCatalog {
    pub fn new(
        baseline: BaselineAccess,
        immutable_store: Arc<dyn ImmutableStore>,
        mutable_store: Arc<dyn MutableStore>,
    ) -> Self {
        Self {
            baseline,
            immutable_store,
            mutable_store,
        }
    }

    async fn list_held(&self) -> Result<Vec<RepositoryId>, Status> {
        let repository = Arc::new(RepositoryContext::new_server_context(
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            Context::default().into(),
        ));
        let mut stream = repository::list_local(repository)
            .await
            .filter_slow_down()?
            .warn_map_err(|err| Status::internal(format!("Failed to list repositories: {err}")))?;
        let mut held = Vec::new();
        while let Some(id) = stream.next().await {
            held.push(id.into());
        }
        Ok(held)
    }
}

#[async_trait]
impl RepositoryCatalog for BaselineRepositoryCatalog {
    async fn list_repositories(
        &self,
        _token: Option<&VerifiedToken<'_>>,
        page_size: Option<u32>,
        page_token: Option<&str>,
    ) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
        match self.baseline {
            BaselineAccess::Denied => Ok((Vec::new(), None)),
            BaselineAccess::Reachable => page(self.list_held().await?, page_size, page_token),
        }
    }
}

/// One page of a full listing, in identifier order. The continuation token
/// is the last identifier on the page, so a partition created between two
/// pages lands at its position in the order and nothing already listed
/// repeats.
fn page(
    mut held: Vec<RepositoryId>,
    page_size: Option<u32>,
    page_token: Option<&str>,
) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
    held.sort_unstable();
    held.dedup();
    if let Some(token) = page_token {
        let after = RepositoryId::from_str(token)
            .map_err(|err| Status::invalid_argument(format!("Malformed page token: {err}")))?;
        held.drain(..held.partition_point(|id| *id <= after));
    }
    let limit = page_size.map(|size| size as usize).filter(|size| *size > 0);
    let next = match limit {
        Some(limit) if held.len() > limit => {
            held.truncate(limit);
            held.last().map(ToString::to_string)
        }
        _ => None,
    };
    Ok((held, next))
}

/// A `RepositoryCatalog` implementation using`UrcAuthApi`.
/// Calls `LookupUserPermissions`.
pub struct AuthClientRepositoryCatalog {
    auth_url: String,
}

impl AuthClientRepositoryCatalog {
    pub fn new(auth_url: String) -> Self {
        Self { auth_url }
    }
}

#[async_trait]
impl RepositoryCatalog for AuthClientRepositoryCatalog {
    async fn list_repositories(
        &self,
        token: Option<&VerifiedToken<'_>>,
        page_size: Option<u32>,
        page_token: Option<&str>,
    ) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
        lore_debug!("Repository fetch authorized repositories");

        let mut client = grpc_get_auth_client(self.auth_url.clone()).await?;
        let request = lookup_request(bearer_header(token), page_size, page_token)?;

        let permissions = client
            .lookup_user_permissions(request)
            .await
            .warn_map_err(|err| {
                if err.code() == Code::PermissionDenied {
                    return Status::permission_denied("List resources denied");
                } else if err.code() == Code::Unauthenticated {
                    return Status::unauthenticated("list resource failed - unauthenticated");
                }
                Status::internal(format!(
                    "Failed to call auth lookup_user_permissions: {err}"
                ))
            })?;

        Ok(repositories_from_response(permissions.into_inner()))
    }
}

fn lookup_request(
    authorization: Option<String>,
    page_size: Option<u32>,
    page_token: Option<&str>,
) -> Result<tonic::Request<LookupUserPermissionsRequest>, Status> {
    create_request_with_authorization(
        LookupUserPermissionsRequest {
            resource_filter: "urc".to_string(),
            context_filter: None,
            page_size: page_size.map(|size| i32::try_from(size).unwrap_or(i32::MAX)),
            page_token: page_token.map(ToString::to_string),
        },
        authorization,
    )
}

/// The partitions a `LookupUserPermissions` response names. Entries outside
/// the `urc-{id}` shape are not partitions and are skipped. An empty
/// continuation token is the end of the listing.
fn repositories_from_response(
    response: LookupUserPermissionsResponse,
) -> (Vec<RepositoryId>, Option<String>) {
    let repositories = response
        .resource_permission
        .iter()
        .filter_map(|permission| {
            permission
                .resource_id
                .strip_prefix("urc-")
                .and_then(|repository_id| Context::from_str(repository_id).ok())
                .map(RepositoryId::from)
        })
        .collect();
    let next_page_token = response.next_page_token.filter(|token| !token.is_empty());
    (repositories, next_page_token)
}

/// Which implementation [`repository_catalog`] selects for a
/// configuration, named in the startup log beside the authorizer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogSelection {
    /// Answers per `baseline_access` from the server's own store. A server
    /// with no `[server.auth]` gets `Reachable` and lists everything it holds.
    Baseline(BaselineAccess),
    /// `LookupUserPermissions` at this `UrcAuthApi` endpoint, forwarding the
    /// caller's token.
    AuthClient(String),
}

impl fmt::Display for CatalogSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Baseline(BaselineAccess::Denied) => {
                f.write_str("BaselineRepositoryCatalog(denied)")
            }
            Self::Baseline(BaselineAccess::Reachable) => {
                f.write_str("BaselineRepositoryCatalog(reachable)")
            }
            Self::AuthClient(url) => write!(f, "AuthClientRepositoryCatalog({url})"),
        }
    }
}

/// Selects the `RepositoryCatalog` implementation. The rules:
/// - `repository_catalog` config can be used to select the mode
/// - if unset, uses `AuthService` if `auth_url` is defined,
///   `Baseline` otherwise
/// - if using `AuthService` catalog, the gRPC server url is
///   read primarily from `repository_catalog_url`, or from
///   `auth_url` if unset
pub fn select_repository_catalog(
    auth: Option<&AuthSettings>,
    auth_url: Option<&str>,
) -> anyhow::Result<CatalogSelection> {
    select_repository_authorizer(auth, auth_url)?;
    let Some(auth) = auth else {
        return Ok(CatalogSelection::Baseline(BaselineAccess::Reachable));
    };
    let mode = auth.repository_catalog.unwrap_or(if auth_url.is_some() {
        RepositoryCatalogMode::AuthService
    } else {
        RepositoryCatalogMode::Baseline
    });
    Ok(match mode {
        RepositoryCatalogMode::Baseline => CatalogSelection::Baseline(auth.baseline_access),
        RepositoryCatalogMode::AuthService => {
            let url = auth
                .repository_catalog_url
                .as_deref()
                .or(auth_url)
                .ok_or_else(|| {
                    anyhow!(
                        "[server.auth] repository_catalog = \"auth_service\" needs an endpoint to \
                         ask: set repository_catalog_url, or [environment.endpoint] auth_url."
                    )
                })?;
            CatalogSelection::AuthClient(url.to_string())
        }
    })
}

/// Creates the catalog [`select_repository_catalog`] picks for this
/// configuration. Built once at startup beside the authorizer.
pub fn repository_catalog(
    auth: Option<&AuthSettings>,
    auth_url: Option<&str>,
    immutable_store: Arc<dyn ImmutableStore>,
    mutable_store: Arc<dyn MutableStore>,
) -> anyhow::Result<Arc<dyn RepositoryCatalog>> {
    let selection = select_repository_catalog(auth, auth_url)?;
    info!("Repository catalog: {selection}");
    Ok(match selection {
        CatalogSelection::Baseline(baseline) => Arc::new(BaselineRepositoryCatalog::new(
            baseline,
            immutable_store,
            mutable_store,
        )),
        CatalogSelection::AuthClient(url) => {
            if auth.is_some_and(|auth| auth.baseline_access == BaselineAccess::Reachable) {
                warn!("[server.auth] baseline_access is not consulted by the auth_service catalog");
            }
            Arc::new(AuthClientRepositoryCatalog::new(url))
        }
    })
}

#[cfg(test)]
mod tests {
    use lore_proto::auth::ResourcePermission;

    use super::*;
    use crate::auth::jwt::AuthorizationToken;
    use crate::authnz::repository_authorizer::RepositoryAuthorizer;
    use crate::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;

    /// Generous enough that no test here hits it by accident.
    const BUDGET: Duration = Duration::from_secs(10);

    fn repository(hex: &str) -> RepositoryId {
        Context::from_str(hex).unwrap().into()
    }

    /// `count` distinct identifiers whose store order is not their sort
    /// order, so a listing that comes back sorted was sorted by the code.
    fn identifiers(count: u128) -> Vec<RepositoryId> {
        (1..=count)
            .map(|n| repository(&format!("{:032x}", n.wrapping_mul(0x9e37_79b9_7f4a_7c15))))
            .collect()
    }

    /// In-memory stores holding `ids`, registered the way `repository
    /// create` registers a new partition.
    async fn stores_holding(
        ids: &[RepositoryId],
    ) -> (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>) {
        let (immutable, mutable) = repository::create_client_memory_stores().await.unwrap();
        let context = Arc::new(RepositoryContext::new_server_context(
            immutable.clone(),
            mutable.clone(),
            Context::default().into(),
        ));
        for (index, id) in ids.iter().enumerate() {
            repository::store_name_to_id(context.clone(), format!("repository-{index}"), *id)
                .await
                .unwrap();
        }
        (immutable, mutable)
    }

    async fn baseline(
        baseline: BaselineAccess,
        held: &[RepositoryId],
    ) -> Arc<dyn RepositoryCatalog> {
        let (immutable, mutable) = stores_holding(held).await;
        Arc::new(BaselineRepositoryCatalog::new(baseline, immutable, mutable))
    }

    #[tokio::test]
    async fn denied_lists_nothing_the_server_holds() {
        let held = identifiers(3);
        let catalog = baseline(BaselineAccess::Denied, &held).await;
        assert_eq!(
            catalog.list_repositories(None, None, None).await.unwrap(),
            (Vec::new(), None)
        );
        assert!(catalog.list_all(None, BUDGET).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reachable_lists_everything_the_server_holds() {
        let held = identifiers(3);
        let catalog = baseline(BaselineAccess::Reachable, &held).await;
        let (listed, next) = catalog.list_repositories(None, None, None).await.unwrap();
        assert_eq!(next, None);
        assert_eq!(
            listed.into_iter().collect::<HashSet<_>>(),
            held.iter().copied().collect::<HashSet<_>>()
        );
    }

    /// Two pages of a listing return every partition once: no overlap,
    /// nothing dropped.
    #[tokio::test]
    async fn a_page_token_round_trips() {
        let held = identifiers(5);
        let catalog = baseline(BaselineAccess::Reachable, &held).await;

        let (first, token) = catalog
            .list_repositories(None, Some(3), None)
            .await
            .unwrap();
        assert_eq!(first.len(), 3);
        let token = token.expect("more remain after the first page");

        let (second, end) = catalog
            .list_repositories(None, Some(3), Some(&token))
            .await
            .unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(end, None, "the second page is the last");

        let listed: Vec<_> = first.into_iter().chain(second).collect();
        assert_eq!(listed.iter().collect::<HashSet<_>>().len(), listed.len());
        assert_eq!(
            listed.into_iter().collect::<HashSet<_>>(),
            held.into_iter().collect::<HashSet<_>>()
        );
    }

    /// The token is the last identifier on the page, so a partition created
    /// between two pages appears at its position in the order and nothing
    /// already listed repeats.
    #[test]
    fn a_partition_created_between_pages_is_listed_once() {
        let mut held = identifiers(4);
        held.sort_unstable();
        let (first, token) = page(held.clone(), Some(2), None).unwrap();
        assert_eq!(first, held[..2]);
        let token = token.unwrap();
        assert_eq!(token, held[1].to_string());

        let created = repository(&format!(
            "{:032x}",
            u128::from_str_radix(&token, 16).unwrap() + 1
        ));
        held.push(created);
        let (second, token) = page(held.clone(), Some(2), Some(&token)).unwrap();
        assert_eq!(second, vec![created, held[2]]);
        let (third, end) = page(held.clone(), Some(2), Some(&token.unwrap())).unwrap();
        assert_eq!(third, vec![held[3]]);
        assert_eq!(end, None);
    }

    #[test]
    fn a_zero_page_size_is_unbounded() {
        let held = identifiers(4);
        let (listed, next) = page(held.clone(), Some(0), None).unwrap();
        assert_eq!(listed.len(), held.len());
        assert_eq!(next, None);
    }

    #[test]
    fn a_malformed_page_token_is_refused() {
        let err = page(identifiers(2), Some(1), Some("not-a-partition")).unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    /// `baseline_access` gates no access decision: an operation on a
    /// partition the catalog did not list still succeeds when the caller
    /// holds the grant.
    #[tokio::test]
    async fn denied_listing_does_not_deny_a_granted_partition() {
        let granted = repository("0194b726b34e72b0b45550b88a967076");
        let catalog = baseline(BaselineAccess::Denied, &[granted]).await;
        let authorizer = ResourceGrantsAuthorizer::new(
            "resources".to_string(),
            "resource_id".to_string(),
            None,
            "urc-{id}".to_string(),
            "urc-*".to_string(),
        );
        let claims = AuthorizationToken {
            resources: Some(vec![crate::auth::jwt::ResourcePermission {
                resource_id: format!("urc-{granted}"),
                permission: vec!["write".to_string()],
            }]),
            ..Default::default()
        };
        let token = VerifiedToken {
            raw: "raw.jwt",
            claims: &claims,
        };

        assert!(
            catalog
                .list_all(Some(&token), BUDGET)
                .await
                .unwrap()
                .is_empty()
        );
        authorizer
            .check_repository_access(Some(&token), granted, None)
            .await
            .unwrap();
        authorizer
            .check_repository_access(Some(&token), granted, Some("write"))
            .await
            .unwrap();
    }

    /// Pages by a fixed schedule: page `n` is answered for token `n`, with
    /// a continuation token while pages remain.
    struct Paged(Vec<Vec<RepositoryId>>);

    #[async_trait]
    impl RepositoryCatalog for Paged {
        async fn list_repositories(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _page_size: Option<u32>,
            page_token: Option<&str>,
        ) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
            let index: usize = page_token.map_or(0, |token| token.parse().unwrap());
            let next = (index + 1 < self.0.len()).then(|| (index + 1).to_string());
            Ok((self.0[index].clone(), next))
        }
    }

    /// Pages by a token map: `None` starts at `first`, and each token names
    /// the next; a token that maps to itself or back to an earlier one never
    /// ends.
    struct Chained {
        first: String,
        next: Vec<(&'static str, Option<&'static str>)>,
    }

    #[async_trait]
    impl RepositoryCatalog for Chained {
        async fn list_repositories(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _page_size: Option<u32>,
            page_token: Option<&str>,
        ) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
            let Some(current) = page_token else {
                return Ok((vec![], Some(self.first.clone())));
            };
            let (_, next) = self
                .next
                .iter()
                .find(|(token, _)| *token == current)
                .expect("every issued token is mapped");
            Ok((vec![], next.map(ToString::to_string)))
        }
    }

    /// A catalog that repeats a token, or cycles through a set of them, is
    /// refused after the first repeat instead of being asked forever: the v1
    /// handler runs with no timeout, so the loop is the only guard.
    #[tokio::test]
    async fn list_all_refuses_a_repeated_or_cyclic_token() {
        let repeating: Arc<dyn RepositoryCatalog> = Arc::new(Chained {
            first: "a".to_string(),
            next: vec![("a", Some("a"))],
        });
        assert_eq!(
            repeating.list_all(None, BUDGET).await.unwrap_err().code(),
            Code::Internal
        );

        let cyclic: Arc<dyn RepositoryCatalog> = Arc::new(Chained {
            first: "a".to_string(),
            next: vec![("a", Some("b")), ("b", Some("a"))],
        });
        assert_eq!(
            cyclic.list_all(None, BUDGET).await.unwrap_err().code(),
            Code::Internal
        );

        // The same shape terminating normally is followed to the end.
        let chain: Arc<dyn RepositoryCatalog> = Arc::new(Chained {
            first: "a".to_string(),
            next: vec![("a", Some("b")), ("b", None)],
        });
        assert!(chain.list_all(None, BUDGET).await.unwrap().is_empty());
    }

    /// Issues a fresh token on every page and never ends, counting the
    /// pages it was asked for.
    struct Endless(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl RepositoryCatalog for Endless {
        async fn list_repositories(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _page_size: Option<u32>,
            _page_token: Option<&str>,
        ) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
            let page = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok((vec![], Some(format!("page-{page}"))))
        }
    }

    /// Distinct tokens without end are bounded by the budget: the walk stops
    /// with `DeadlineExceeded` after following some of them, rather than
    /// accumulating pages for as long as the upstream keeps answering.
    #[tokio::test]
    async fn list_all_stops_an_endless_chain_at_the_deadline() {
        let endless = Arc::new(Endless(std::sync::atomic::AtomicUsize::new(0)));
        let catalog: Arc<dyn RepositoryCatalog> = endless.clone();
        let err = catalog
            .list_all(None, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::DeadlineExceeded);
        assert!(
            endless.0.load(std::sync::atomic::Ordering::Relaxed) > 1,
            "the chain must have been followed before the deadline stopped it"
        );
    }

    /// One page that outlives the budget is cut off by it too, so a hanging
    /// upstream cannot hold the request open.
    struct Hanging;

    #[async_trait]
    impl RepositoryCatalog for Hanging {
        async fn list_repositories(
            &self,
            _token: Option<&VerifiedToken<'_>>,
            _page_size: Option<u32>,
            _page_token: Option<&str>,
        ) -> Result<(Vec<RepositoryId>, Option<String>), Status> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn list_all_stops_a_hanging_page_at_the_deadline() {
        let catalog: Arc<dyn RepositoryCatalog> = Arc::new(Hanging);
        let err = catalog
            .list_all(None, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::DeadlineExceeded);
    }

    #[tokio::test]
    async fn list_all_follows_continuation_tokens() {
        let ids = identifiers(5);
        let catalog: Arc<dyn RepositoryCatalog> = Arc::new(Paged(vec![
            ids[..2].to_vec(),
            ids[2..4].to_vec(),
            ids[4..].to_vec(),
        ]));
        assert_eq!(catalog.list_all(None, BUDGET).await.unwrap(), ids);
    }

    /// With no paging asked for, the upstream request is the one the handler
    /// has always sent: the `urc` filter and the caller's own header.
    #[test]
    fn lookup_request_matches_the_legacy_call() {
        let request = lookup_request(Some("Bearer tok".into()), None, None).unwrap();
        assert_eq!(
            request.get_ref(),
            &LookupUserPermissionsRequest {
                resource_filter: "urc".to_string(),
                ..Default::default()
            }
        );
        assert_eq!(
            request
                .metadata()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok"
        );

        let paged = lookup_request(None, Some(50), Some("cursor")).unwrap();
        assert_eq!(paged.get_ref().page_size, Some(50));
        assert_eq!(paged.get_ref().page_token.as_deref(), Some("cursor"));
    }

    #[test]
    fn response_yields_partitions_and_skips_other_resources() {
        let partition = repository("0194b726b34e72b0b45550b88a967076");
        let entry = |resource_id: &str| ResourcePermission {
            resource_id: resource_id.to_string(),
            permission: vec!["read".to_string()],
        };
        let response = LookupUserPermissionsResponse {
            resource_permission: vec![
                entry(&format!("urc-{partition}")),
                entry("urc-not-a-partition"),
                entry("something-else"),
            ],
            next_page_token: Some(String::new()),
        };
        assert_eq!(
            repositories_from_response(response),
            (vec![partition], None)
        );

        let more = LookupUserPermissionsResponse {
            resource_permission: vec![],
            next_page_token: Some("more".to_string()),
        };
        assert_eq!(
            repositories_from_response(more),
            (vec![], Some("more".to_string()))
        );
    }

    fn auth_settings(extra: &str) -> AuthSettings {
        toml::from_str(&format!(
            "jwt_issuer = \"https://auth.example.com\"\njwt_audience = [\"lore\"]\n{extra}"
        ))
        .unwrap()
    }

    const AUTH_URL: Option<&str> = Some("https://legacy-auth.example.com");

    /// The catalog follows the authorizer's flowchart: a no-auth server
    /// lists everything it holds, a `UrcAuthApi` deployment asks the auth
    /// service, and the token-claim tiers answer per `baseline_access`.
    #[test]
    fn selection_follows_the_authorizer_flowchart() {
        let table = [
            (
                None,
                None,
                CatalogSelection::Baseline(BaselineAccess::Reachable),
            ),
            (
                Some(auth_settings("")),
                AUTH_URL,
                CatalogSelection::AuthClient(AUTH_URL.unwrap().to_string()),
            ),
            // An explicit choice overrides what `auth_url` implies, either way.
            (
                Some(auth_settings("repository_catalog = \"baseline\"")),
                AUTH_URL,
                CatalogSelection::Baseline(BaselineAccess::Denied),
            ),
            (
                Some(auth_settings(
                    "repository_catalog = \"auth_service\"\nrepository_catalog_url = \"https://catalog.example.com\"",
                )),
                None,
                CatalogSelection::AuthClient("https://catalog.example.com".to_string()),
            ),
            (
                Some(auth_settings(
                    "resource_claim = \"resources\"\nrepository_catalog = \"auth_service\"\nrepository_catalog_url = \"https://catalog.example.com\"",
                )),
                None,
                CatalogSelection::AuthClient("https://catalog.example.com".to_string()),
            ),
            // The catalog endpoint wins over `auth_url` when both are set.
            (
                Some(auth_settings(
                    "repository_catalog_url = \"https://catalog.example.com\"",
                )),
                AUTH_URL,
                CatalogSelection::AuthClient("https://catalog.example.com".to_string()),
            ),
            (
                Some(auth_settings("")),
                None,
                CatalogSelection::Baseline(BaselineAccess::Denied),
            ),
            (
                Some(auth_settings("resource_claim = \"resources\"")),
                None,
                CatalogSelection::Baseline(BaselineAccess::Denied),
            ),
            (
                Some(auth_settings("baseline_access = \"reachable\"")),
                None,
                CatalogSelection::Baseline(BaselineAccess::Reachable),
            ),
            (
                Some(auth_settings(
                    "resource_claim = \"resources\"\nbaseline_access = \"reachable\"",
                )),
                None,
                CatalogSelection::Baseline(BaselineAccess::Reachable),
            ),
        ];
        for (auth, auth_url, expected) in table {
            assert_eq!(
                select_repository_catalog(auth.as_ref(), auth_url).unwrap(),
                expected,
                "auth: {auth:?}, auth_url: {auth_url:?}"
            );
        }
    }

    /// The pairings the authorizer refuses are refused here too, so the
    /// catalog never exists for a configuration the server will not run.
    /// The auth-service catalog with no endpoint to ask is refused naming
    /// both settings that could supply one.
    #[test]
    fn refused_pairings_are_refused() {
        select_repository_catalog(None, AUTH_URL).unwrap_err();
        select_repository_catalog(
            Some(&auth_settings("resource_claim = \"resources\"")),
            AUTH_URL,
        )
        .unwrap_err();
        let message = select_repository_catalog(
            Some(&auth_settings("repository_catalog = \"auth_service\"")),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("repository_catalog_url"), "{message}");
        assert!(message.contains("auth_url"), "{message}");
    }

    #[tokio::test]
    async fn construction_agrees_with_selection() {
        let (immutable, mutable) = stores_holding(&[]).await;
        for (auth, auth_url) in [
            (None, None),
            (Some(auth_settings("")), AUTH_URL),
            (Some(auth_settings("")), None),
        ] {
            repository_catalog(auth.as_ref(), auth_url, immutable.clone(), mutable.clone())
                .unwrap();
        }
    }

    #[test]
    fn selection_display_names_the_implementation() {
        assert_eq!(
            CatalogSelection::Baseline(BaselineAccess::Denied).to_string(),
            "BaselineRepositoryCatalog(denied)"
        );
        assert_eq!(
            CatalogSelection::Baseline(BaselineAccess::Reachable).to_string(),
            "BaselineRepositoryCatalog(reachable)"
        );
        assert_eq!(
            CatalogSelection::AuthClient("https://auth.example.com".to_string()).to_string(),
            "AuthClientRepositoryCatalog(https://auth.example.com)"
        );
    }
}
