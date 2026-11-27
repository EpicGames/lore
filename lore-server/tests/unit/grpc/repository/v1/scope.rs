// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The scope verbs of the repository service: what each answers, what each refuses, and who may
//! call it, with authentication on and off.
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::repository::v1::ScopeArchiveRequest;
use lore_proto::lore::repository::v1::ScopeCreateRequest;
use lore_proto::lore::repository::v1::ScopeGetRequest;
use lore_proto::lore::repository::v1::ScopeRenameRequest;
use lore_proto::lore::repository::v1::scope_get_request::Query;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::scope;
use lore_revision::scope::ScopeId;
use lore_revision::state::allow_all_repositories;
use lore_server::authnz::repository_authorizer::RepositoryAuthorizer;
use lore_server::grpc::repository::v1::scope_archive;
use lore_server::grpc::repository::v1::scope_create;
use lore_server::grpc::repository::v1::scope_get;
use lore_server::grpc::repository::v1::scope_rename;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::grpc::push_protected::TIERS;
use crate::grpc::push_protected::Tier;
use crate::grpc::push_protected::allow_all;
use crate::store::test_support::test_store_create;

/// The identity `Tier::with_token` puts in its tokens.
const CALLER: &str = "pusher";
/// The identity of a user other than the caller.
const OTHER_USER: &str = "alice";

#[derive(Clone)]
struct TestInstrumentProvider;

impl InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
}

/// The stores of a test, a repository in them, and the caller the requests carry.
struct Fixture {
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: RepositoryId,
    authorizer: Arc<dyn RepositoryAuthorizer>,
    /// The caller's tier and the actions its token holds, `None` for a request without a token,
    /// as a server without authentication sees it.
    caller: Option<(Tier, &'static [&'static str])>,
}

impl Fixture {
    /// The server context of the fixture's repository.
    fn context(&self) -> Arc<RepositoryContext> {
        Arc::new(RepositoryContext::new_server_context(
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            self.repository,
            allow_all_repositories(),
        ))
    }

    /// `message` as a request naming the fixture's repository, from the fixture's caller.
    fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(self.repository.data()),
        );
        match self.caller {
            Some((tier, actions)) => tier.with_token(request, self.repository, actions),
            None => request,
        }
    }

    /// Stores scope `byte` named `name`, created by `creator`, as a create by the library does.
    async fn seed(&self, byte: u8, name: &str, creator: &str) {
        scope::create(self.context(), scope_id(byte), name, creator, 7)
            .await
            .expect("seeding a scope")
            .expect("the seeded scope is new");
    }

    async fn create(&self, byte: u8, name: &str) -> Result<model_v1::Scope, Status> {
        self.create_id(scope_id(byte), name).await
    }

    async fn create_id(&self, id: ScopeId, name: &str) -> Result<model_v1::Scope, Status> {
        let request = self.request(ScopeCreateRequest {
            id: id.into(),
            name: name.to_string(),
        });
        let response = scope_create::handler(
            request,
            &self.authorizer,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            &TestInstrumentProvider,
        )
        .await?;
        Ok(response.into_inner().scope.expect("a scope"))
    }

    async fn get(&self, query: Query) -> Result<model_v1::Scope, Status> {
        let request = self.request(ScopeGetRequest { query: Some(query) });
        let response = scope_get::handler(
            request,
            &self.authorizer,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
        )
        .await?;
        Ok(response.into_inner().scope.expect("a scope"))
    }

    async fn rename(&self, id: ScopeId, name: &str) -> Result<model_v1::Scope, Status> {
        let request = self.request(ScopeRenameRequest {
            id: id.into(),
            name: name.to_string(),
        });
        let response = scope_rename::handler(
            request,
            &self.authorizer,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            &TestInstrumentProvider,
        )
        .await?;
        Ok(response.into_inner().scope.expect("a scope"))
    }

    async fn archive(&self, id: ScopeId) -> Result<model_v1::Scope, Status> {
        let request = self.request(ScopeArchiveRequest { id: id.into() });
        let response = scope_archive::handler(
            request,
            &self.authorizer,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            &TestInstrumentProvider,
        )
        .await?;
        Ok(response.into_inner().scope.expect("a scope"))
    }
}

/// The scope id made of `byte`.
fn scope_id(byte: u8) -> ScopeId {
    ScopeId::from([byte; 16])
}

/// Runs `test` on a fixture whose requests carry `caller`'s token, or none.
async fn with_fixture<F, Fut>(caller: Option<(Tier, &'static [&'static str])>, test: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let fixture = Fixture {
        immutable_store,
        mutable_store,
        repository: random::<RepositoryId>(),
        authorizer: caller.map_or_else(allow_all, |(tier, _)| tier.authorizer()),
        caller,
    };
    Box::pin(LORE_CONTEXT.scope(execution, test(fixture))).await;
}

/// The code `result` failed with.
fn code<T: std::fmt::Debug>(result: Result<T, Status>) -> Code {
    result.expect_err("the call is refused").code()
}

#[tokio::test]
async fn a_created_scope_reads_by_id_and_by_name() {
    with_fixture(None, async |fixture| {
        let created = fixture.create(1, "Release").await.unwrap();
        assert_eq!(created.id, scope_id(1).data().as_slice());
        assert_eq!(created.name, "Release");
        assert!(created.created > 0);
        assert!(!created.archived);
        assert_eq!(
            fixture.get(Query::Id(scope_id(1).into())).await.unwrap(),
            created
        );
        assert_eq!(
            fixture
                .get(Query::Name("RELEASE".to_string()))
                .await
                .unwrap(),
            created
        );
    })
    .await;
}

#[tokio::test]
async fn the_creator_is_the_callers_identity() {
    for tier in TIERS {
        with_fixture(Some((tier, &["write"])), async |fixture| {
            assert_eq!(fixture.create(1, "release").await.unwrap().creator, CALLER);
        })
        .await;
    }
}

#[tokio::test]
async fn a_repeated_create_answers_the_scope_and_a_different_one_is_refused() {
    with_fixture(None, async |fixture| {
        let created = fixture.create(1, "release").await.unwrap();
        assert_eq!(fixture.create(1, "release").await.unwrap(), created);
        assert_eq!(code(fixture.create(1, "other").await), Code::AlreadyExists);
        assert_eq!(
            code(fixture.create(2, "Release").await),
            Code::AlreadyExists
        );
    })
    .await;
}

#[tokio::test]
async fn a_reserved_or_invalid_name_or_id_is_an_invalid_argument() {
    with_fixture(None, async |fixture| {
        for name in ["", "default", "DEFAULT"] {
            assert_eq!(
                code(fixture.create(1, name).await),
                Code::InvalidArgument,
                "{name:?}"
            );
        }
        assert_eq!(code(fixture.create(0, "zero").await), Code::InvalidArgument);
        assert_eq!(
            code(
                fixture
                    .create_id(scope::default_scope_id(fixture.repository), "other")
                    .await
            ),
            Code::InvalidArgument
        );
        fixture.create(1, "release").await.unwrap();
        assert_eq!(
            code(fixture.rename(scope_id(1), "Default").await),
            Code::InvalidArgument
        );
    })
    .await;
}

#[tokio::test]
async fn the_default_scope_answers_without_a_record_and_takes_no_write() {
    with_fixture(None, async |fixture| {
        let default = scope::default_scope_id(fixture.repository);
        for query in [
            Query::Id(default.into()),
            Query::Name("default".to_string()),
            Query::Name("DeFault".to_string()),
        ] {
            let found = fixture.get(query).await.unwrap();
            assert_eq!(found.id, default.data().as_slice());
            assert_eq!(found.name, "default");
            assert_eq!((found.creator.as_str(), found.created), ("", 0));
            assert!(!found.archived);
        }
        assert_eq!(
            code(fixture.rename(default, "other").await),
            Code::FailedPrecondition
        );
        assert_eq!(
            code(fixture.archive(default).await),
            Code::FailedPrecondition
        );
    })
    .await;
}

#[tokio::test]
async fn a_rename_shows_in_get_and_frees_the_old_name() {
    with_fixture(None, async |fixture| {
        fixture.create(1, "release").await.unwrap();
        let renamed = fixture.rename(scope_id(1), "shipped").await.unwrap();
        assert_eq!(renamed.name, "shipped");
        assert_eq!(
            fixture.get(Query::Id(scope_id(1).into())).await.unwrap(),
            renamed
        );
        assert_eq!(
            code(fixture.get(Query::Name("release".to_string())).await),
            Code::NotFound
        );
        fixture.create(2, "release").await.unwrap();
        assert_eq!(
            code(fixture.rename(scope_id(2), "SHIPPED").await),
            Code::AlreadyExists
        );
    })
    .await;
}

#[tokio::test]
async fn an_archived_scope_reads_by_id_only() {
    with_fixture(None, async |fixture| {
        fixture.create(1, "release").await.unwrap();
        let archived = fixture.archive(scope_id(1)).await.unwrap();
        assert!(archived.archived);
        assert_eq!(archived.name, "");
        assert_eq!(
            fixture.get(Query::Id(scope_id(1).into())).await.unwrap(),
            archived
        );
        assert_eq!(
            code(fixture.get(Query::Name("release".to_string())).await),
            Code::NotFound
        );
    })
    .await;
}

/// A create of an archived scope's id restores the scope, keeping its creator and creation time,
/// and a repeat of it answers the scope. A rename of an archived scope is refused.
#[tokio::test]
async fn a_create_of_an_archived_scopes_id_restores_it_and_a_rename_is_refused() {
    with_fixture(None, async |fixture| {
        let created = fixture.create(1, "release").await.unwrap();
        fixture.archive(scope_id(1)).await.unwrap();
        assert_eq!(
            code(fixture.rename(scope_id(1), "other").await),
            Code::FailedPrecondition
        );
        let restored = fixture.create(1, "revived").await.unwrap();
        assert_eq!(
            restored,
            model_v1::Scope {
                name: "revived".to_string(),
                ..created
            }
        );
        assert_eq!(
            fixture
                .get(Query::Name("revived".to_string()))
                .await
                .unwrap(),
            restored
        );
        assert_eq!(fixture.create(1, "revived").await.unwrap(), restored);
        assert_eq!(code(fixture.create(1, "other").await), Code::AlreadyExists);
    })
    .await;
}

#[tokio::test]
async fn a_restore_under_a_taken_name_is_refused() {
    with_fixture(None, async |fixture| {
        fixture.create(1, "release").await.unwrap();
        fixture.archive(scope_id(1)).await.unwrap();
        fixture.create(2, "release").await.unwrap();
        assert_eq!(
            code(fixture.create(1, "release").await),
            Code::AlreadyExists
        );
        assert!(
            fixture
                .get(Query::Id(scope_id(1).into()))
                .await
                .unwrap()
                .archived
        );
    })
    .await;
}

#[tokio::test]
async fn a_write_or_get_of_an_unknown_scope_is_not_found() {
    with_fixture(None, async |fixture| {
        let unknown = scope_id(9);
        assert_eq!(
            code(fixture.get(Query::Id(unknown.into())).await),
            Code::NotFound
        );
        assert_eq!(code(fixture.rename(unknown, "other").await), Code::NotFound);
        assert_eq!(code(fixture.archive(unknown).await), Code::NotFound);
    })
    .await;
}

/// Renames, archives and restores, with a create of its id, scope 1, which `creator` created, as
/// the fixture's caller.
async fn administer(fixture: &Fixture, creator: &str) -> Result<(), Status> {
    fixture.seed(1, "release", creator).await;
    fixture.rename(scope_id(1), "shipped").await?;
    fixture.archive(scope_id(1)).await?;
    fixture.create(1, "revived").await?;
    Ok(())
}

/// A create needs write access, the one that restores a scope included, so the callers hold it.
#[tokio::test]
async fn owner_or_admin_administers_a_scope_another_user_created() {
    for tier in TIERS {
        for actions in [&["owner", "write"][..], &["admin", "write"][..]] {
            with_fixture(Some((tier, actions)), async |fixture| {
                administer(&fixture, OTHER_USER)
                    .await
                    .unwrap_or_else(|err| panic!("{tier:?} {actions:?}: {err}"));
            })
            .await;
        }
    }
}

/// Renaming and archiving a scope need administration alone, and a create, one that restores a
/// scope included, needs write access. The global tier grants `write` to every token holder, so
/// only the per-repository tiers have a caller without it.
#[tokio::test]
async fn owner_or_admin_without_write_renames_and_archives_but_does_not_restore() {
    for tier in [Tier::Resource, Tier::AuthClient] {
        for actions in [&["owner"][..], &["admin"][..]] {
            with_fixture(Some((tier, actions)), async |fixture| {
                fixture.seed(1, "release", OTHER_USER).await;
                fixture
                    .rename(scope_id(1), "shipped")
                    .await
                    .unwrap_or_else(|err| panic!("{tier:?} {actions:?}: {err}"));
                fixture
                    .archive(scope_id(1))
                    .await
                    .unwrap_or_else(|err| panic!("{tier:?} {actions:?}: {err}"));
                assert_eq!(
                    code(fixture.create(1, "revived").await),
                    Code::PermissionDenied,
                    "{tier:?} {actions:?}"
                );
            })
            .await;
        }
    }
}

#[tokio::test]
async fn the_creator_administers_its_scope_with_write() {
    for tier in TIERS {
        with_fixture(Some((tier, &["write"])), async |fixture| {
            administer(&fixture, CALLER)
                .await
                .unwrap_or_else(|err| panic!("{tier:?}: {err}"));
        })
        .await;
    }
}

/// A create that restores a scope needs write access alone, as a create of an archived branch's
/// id does.
#[tokio::test]
async fn write_restores_but_does_not_rename_or_archive_a_scope_another_user_created() {
    for tier in TIERS {
        with_fixture(Some((tier, &["read", "write"])), async |fixture| {
            fixture.seed(1, "release", OTHER_USER).await;
            assert_eq!(
                code(fixture.rename(scope_id(1), "shipped").await),
                Code::PermissionDenied,
                "{tier:?}"
            );
            assert_eq!(
                code(fixture.archive(scope_id(1)).await),
                Code::PermissionDenied,
                "{tier:?}"
            );
            scope::archive(fixture.context(), scope_id(1))
                .await
                .unwrap()
                .unwrap();
            fixture
                .create(1, "revived")
                .await
                .unwrap_or_else(|err| panic!("{tier:?}: {err}"));
            fixture
                .create(2, "mine")
                .await
                .unwrap_or_else(|err| panic!("{tier:?}: {err}"));
        })
        .await;
    }
}

/// The global tier grants `write` to every token holder, so only the per-repository tiers have a
/// caller with `read` alone.
#[tokio::test]
async fn read_alone_reads_scopes_and_writes_none() {
    for tier in [Tier::Resource, Tier::AuthClient] {
        with_fixture(Some((tier, &["read"])), async |fixture| {
            fixture.seed(1, "release", CALLER).await;
            fixture
                .get(Query::Id(scope_id(1).into()))
                .await
                .unwrap_or_else(|err| panic!("{tier:?}: {err}"));
            assert_eq!(
                code(fixture.create(2, "other").await),
                Code::PermissionDenied,
                "{tier:?}"
            );
            assert_eq!(
                code(fixture.rename(scope_id(1), "shipped").await),
                Code::PermissionDenied,
                "{tier:?}"
            );
            assert_eq!(
                code(fixture.archive(scope_id(1)).await),
                Code::PermissionDenied,
                "{tier:?}"
            );
        })
        .await;
    }
}

/// Without authentication every caller is `<unknown>` and has `write`, which administers a scope
/// whatever creator it records.
#[tokio::test]
async fn without_authentication_any_caller_administers_a_scope() {
    with_fixture(None, async |fixture| {
        administer(&fixture, OTHER_USER).await.unwrap();
        fixture.create(2, "mine").await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_creator_only_caller_writing_an_unknown_scope_finds_it_not_found() {
    for tier in TIERS {
        with_fixture(Some((tier, &["write"])), async |fixture| {
            assert_eq!(
                code(fixture.archive(scope_id(9)).await),
                Code::NotFound,
                "{tier:?}"
            );
        })
        .await;
    }
}

#[tokio::test]
async fn a_caller_denied_administration_is_refused_before_any_read() {
    for tier in [Tier::Resource, Tier::AuthClient] {
        with_fixture(Some((tier, &["read"])), async |fixture| {
            assert_eq!(
                code(fixture.archive(scope_id(9)).await),
                Code::PermissionDenied,
                "{tier:?}"
            );
        })
        .await;
    }
}
