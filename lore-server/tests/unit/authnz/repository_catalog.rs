// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::types::Context;
use lore_base::types::RepositoryId;
use lore_proto::auth::LookupUserPermissionsRequest;
use lore_proto::auth::LookupUserPermissionsResponse;
use lore_proto::auth::ResourcePermission;
use lore_proto::lore::user::v1::PartitionAddRequest;
use lore_proto::lore::user::v1::PartitionAddResponse;
use lore_proto::lore::user::v1::PartitionListRequest;
use lore_proto::lore::user::v1::PartitionListResponse;
use lore_proto::lore::user::v1::PartitionRemoveRequest;
use lore_proto::lore::user::v1::PartitionRemoveResponse;
use lore_proto::lore::user::v1::UserFindRequest;
use lore_proto::lore::user::v1::UserFindResponse;
use lore_proto::lore::user::v1::UserGetRequest;
use lore_proto::lore::user::v1::UserGetResponse;
use lore_proto::lore::user::v1::user_service_server::UserService;
use lore_proto::lore::user::v1::user_service_server::UserServiceServer;
use lore_revision::repository;
use lore_revision::repository::RepositoryContext;
use lore_server::auth::jwt::AuthorizationToken;
use lore_server::authnz::repository_authorizer::Action;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::authnz::repository_authorizer::VerifiedToken;
use lore_server::authnz::repository_catalog::*;
use lore_server::authnz::resource_grants_authorizer::ResourceGrantsAuthorizer;
use lore_server::settings::AuthSettings;
use lore_server::settings::BaselineAccess;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::Status;

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
async fn stores_holding(ids: &[RepositoryId]) -> (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>) {
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

async fn baseline(baseline: BaselineAccess, held: &[RepositoryId]) -> Arc<dyn RepositoryCatalog> {
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
        resources: Some(vec![lore_server::auth::jwt::ResourcePermission {
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
        .check_repository_access(Some(&token), granted, Action::Read)
        .await
        .unwrap();
    authorizer
        .check_repository_access(Some(&token), granted, Action::Write)
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
            select_repository_catalog(auth.as_ref(), auth_url, None).unwrap(),
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
    select_repository_catalog(None, AUTH_URL, None).unwrap_err();
    select_repository_catalog(
        Some(&auth_settings("resource_claim = \"resources\"")),
        AUTH_URL,
        None,
    )
    .unwrap_err();
    let message = select_repository_catalog(
        Some(&auth_settings("repository_catalog = \"auth_service\"")),
        None,
        USER_URL,
    )
    .unwrap_err()
    .to_string();
    assert!(message.contains("repository_catalog_url"), "{message}");
    assert!(message.contains("auth_url"), "{message}");
}

const USER_URL: Option<&str> = Some("https://directory.example.com");

/// `user_url` picks the `user_service` catalog ahead of `auth_url`, and an
/// explicit mode wins over either.
#[test]
fn user_url_selects_the_user_service_catalog() {
    let user_service = CatalogSelection::UserService(USER_URL.unwrap().to_string());
    let table = [
        (auth_settings(""), AUTH_URL, USER_URL, user_service.clone()),
        (auth_settings(""), None, USER_URL, user_service.clone()),
        (
            auth_settings("resource_claim = \"resources\""),
            None,
            USER_URL,
            user_service.clone(),
        ),
        (
            auth_settings("repository_catalog = \"user_service\""),
            AUTH_URL,
            USER_URL,
            user_service.clone(),
        ),
        // `repository_catalog_url` belongs to `auth_service` alone.
        (
            auth_settings(
                "repository_catalog = \"user_service\"\nrepository_catalog_url = \"https://catalog.example.com\"",
            ),
            AUTH_URL,
            USER_URL,
            user_service.clone(),
        ),
        (
            auth_settings("repository_catalog = \"auth_service\""),
            AUTH_URL,
            USER_URL,
            CatalogSelection::AuthClient(AUTH_URL.unwrap().to_string()),
        ),
        (
            auth_settings("repository_catalog = \"baseline\""),
            AUTH_URL,
            USER_URL,
            CatalogSelection::Baseline(BaselineAccess::Denied),
        ),
        // An empty `user_url` is unset, as clients read it.
        (
            auth_settings(""),
            AUTH_URL,
            Some(""),
            CatalogSelection::AuthClient(AUTH_URL.unwrap().to_string()),
        ),
        (
            auth_settings(""),
            None,
            Some(""),
            CatalogSelection::Baseline(BaselineAccess::Denied),
        ),
    ];
    for (auth, auth_url, user_url, expected) in table {
        assert_eq!(
            select_repository_catalog(Some(&auth), auth_url, user_url).unwrap(),
            expected,
            "auth: {auth:?}, auth_url: {auth_url:?}, user_url: {user_url:?}"
        );
    }
    // A server with no `[server.auth]` has no token to forward.
    assert_eq!(
        select_repository_catalog(None, None, USER_URL).unwrap(),
        CatalogSelection::Baseline(BaselineAccess::Reachable)
    );
}

#[test]
fn user_service_without_user_url_is_refused() {
    for user_url in [None, Some("")] {
        let message = select_repository_catalog(
            Some(&auth_settings("repository_catalog = \"user_service\"")),
            AUTH_URL,
            user_url,
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("user_url"), "{message}");
    }
}

#[tokio::test]
async fn construction_agrees_with_selection() {
    let (immutable, mutable) = stores_holding(&[]).await;
    for (auth, auth_url, user_url) in [
        (None, None, None),
        (Some(auth_settings("")), AUTH_URL, None),
        (Some(auth_settings("")), None, None),
        (Some(auth_settings("")), AUTH_URL, USER_URL),
    ] {
        repository_catalog(
            auth.as_ref(),
            auth_url,
            user_url,
            immutable.clone(),
            mutable.clone(),
        )
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
    assert_eq!(
        CatalogSelection::UserService("https://directory.example.com".to_string()).to_string(),
        "UserServiceRepositoryCatalog(https://directory.example.com)"
    );
}

#[test]
fn partition_list_response_yields_the_partition_bytes() {
    let partition = repository("0194b726b34e72b0b45550b88a967076");
    let response = |partition: &[u8]| PartitionListResponse {
        partition: partition.to_vec().into(),
        permission: vec!["read".to_string()],
    };
    assert_eq!(
        repository_from_response(response(partition.as_ref())),
        Some(partition)
    );
    assert_eq!(repository_from_response(response(&[])), None);
    assert_eq!(repository_from_response(response(&[7; 15])), None);
    assert_eq!(repository_from_response(response(&[7; 17])), None);
}

/// Streams `partitions` from `PartitionList`, recording the
/// `authorization` header of each call.
struct Directory {
    partitions: Vec<Vec<u8>>,
    authorizations: Mutex<Vec<Option<String>>>,
}

#[tonic::async_trait]
impl UserService for Directory {
    async fn user_get(
        &self,
        _request: tonic::Request<UserGetRequest>,
    ) -> Result<tonic::Response<UserGetResponse>, Status> {
        Err(Status::unimplemented("not under test"))
    }

    async fn user_find(
        &self,
        _request: tonic::Request<UserFindRequest>,
    ) -> Result<tonic::Response<UserFindResponse>, Status> {
        Err(Status::unimplemented("not under test"))
    }

    type PartitionListStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<PartitionListResponse, Status>>>;

    async fn partition_list(
        &self,
        request: tonic::Request<PartitionListRequest>,
    ) -> Result<tonic::Response<Self::PartitionListStream>, Status> {
        let authorization = request
            .metadata()
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_string());
        self.authorizations.lock().unwrap().push(authorization);
        let messages: Vec<_> = self
            .partitions
            .iter()
            .map(|partition| {
                Ok(PartitionListResponse {
                    partition: partition.clone().into(),
                    permission: vec!["read".to_string()],
                })
            })
            .collect();
        Ok(tonic::Response::new(tokio_stream::iter(messages)))
    }

    async fn partition_add(
        &self,
        _request: tonic::Request<PartitionAddRequest>,
    ) -> Result<tonic::Response<PartitionAddResponse>, Status> {
        Err(Status::unimplemented("not under test"))
    }

    async fn partition_remove(
        &self,
        _request: tonic::Request<PartitionRemoveRequest>,
    ) -> Result<tonic::Response<PartitionRemoveResponse>, Status> {
        Err(Status::unimplemented("not under test"))
    }
}

async fn serve(partitions: Vec<Vec<u8>>) -> (String, Arc<Directory>) {
    let directory = Arc::new(Directory {
        partitions,
        authorizations: Mutex::new(Vec::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = directory.clone();
    lore_base::lore_spawn!(async move {
        tonic::transport::Server::builder()
            .add_service(UserServiceServer::from_arc(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    (format!("http://{address}"), directory)
}

/// The stream is paged by the catalog: every streamed partition is listed
/// once across the pages, a duplicate or malformed entry is not, and each
/// page asks the directory with the caller's token.
#[tokio::test]
async fn user_service_catalog_pages_the_stream() {
    let ids = identifiers(5);
    let mut streamed: Vec<Vec<u8>> = ids.iter().map(|id| id.as_ref().to_vec()).collect();
    streamed.push(ids[2].as_ref().to_vec());
    streamed.push(vec![1, 2, 3]);
    let (url, directory) = serve(streamed).await;
    let catalog = UserServiceRepositoryCatalog::new(url);
    let claims = AuthorizationToken::default();
    let token = VerifiedToken {
        raw: "raw.jwt",
        claims: &claims,
    };

    let mut listed = Vec::new();
    let mut page_token = None;
    loop {
        let (page, next) = catalog
            .list_repositories(Some(&token), Some(2), page_token.as_deref())
            .await
            .unwrap();
        assert!(page.len() <= 2);
        listed.extend(page);
        match next {
            Some(next) => page_token = Some(next),
            None => break,
        }
    }

    let mut expected = ids.clone();
    expected.sort_unstable();
    assert_eq!(listed, expected);
    let authorizations = directory.authorizations.lock().unwrap();
    assert_eq!(authorizations.len(), 3, "one call per page");
    assert!(
        authorizations
            .iter()
            .all(|header| header.as_deref() == Some("Bearer raw.jwt"))
    );
}

#[tokio::test]
async fn user_service_catalog_lists_everything_through_list_all() {
    let ids = identifiers(3);
    let (url, _) = serve(ids.iter().map(|id| id.as_ref().to_vec()).collect()).await;
    let catalog: Arc<dyn RepositoryCatalog> = Arc::new(UserServiceRepositoryCatalog::new(url));
    let mut expected = ids.clone();
    expected.sort_unstable();
    assert_eq!(catalog.list_all(None, BUDGET).await.unwrap(), expected);
}

/// A directory streaming more partitions than the limit fails the listing
/// rather than growing it; exactly the limit is listed.
#[tokio::test]
async fn user_service_catalog_refuses_a_stream_over_the_limit() {
    let ids = identifiers(4);
    let (url, _) = serve(ids.iter().map(|id| id.as_ref().to_vec()).collect()).await;
    let err = UserServiceRepositoryCatalog::with_limit(url.clone(), 3)
        .list_repositories(None, None, None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted);

    let (listed, _) = UserServiceRepositoryCatalog::with_limit(url, 4)
        .list_repositories(None, None, None)
        .await
        .unwrap();
    assert_eq!(listed.len(), 4);
}

/// Pages that add up to more than `MAX_LISTED_REPOSITORIES` fail the walk,
/// so a paged upstream is bounded by count as well as by the budget.
#[tokio::test]
async fn list_all_refuses_more_than_the_maximum() {
    let ids: Vec<RepositoryId> = (1..=MAX_LISTED_REPOSITORIES as u128 + 1)
        .map(|n| RepositoryId::from(n.to_be_bytes()))
        .collect();
    let (within, over) = ids.split_at(MAX_LISTED_REPOSITORIES);

    let full: Arc<dyn RepositoryCatalog> = Arc::new(Paged(vec![within.to_vec()]));
    assert_eq!(
        full.list_all(None, BUDGET).await.unwrap().len(),
        MAX_LISTED_REPOSITORIES
    );

    let overfull: Arc<dyn RepositoryCatalog> =
        Arc::new(Paged(vec![within.to_vec(), over.to_vec()]));
    assert_eq!(
        overfull.list_all(None, BUDGET).await.unwrap_err().code(),
        Code::ResourceExhausted
    );
}
