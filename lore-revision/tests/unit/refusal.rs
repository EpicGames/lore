// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Lookups and commands whose remote refuses the caller fail with the refusal.

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::NotAuthenticated;
use lore_base::error::NotAuthorized;
use lore_base::error::NotFound;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_base::types::TypedBytes;
use lore_error_set::prelude::*;
use lore_revision::branch;
use lore_revision::branch::merge::MergeIntoOptions;
use lore_revision::commit;
use lore_revision::commit::CommitOptions;
use lore_revision::file::audit::AuditError;
use lore_revision::file::audit::LoreFileAuditOutcome;
use lore_revision::file::audit::Reachability;
use lore_revision::file::audit::gap;
use lore_revision::file::diff;
use lore_revision::file::diff::DiffOptions;
use lore_revision::file::history;
use lore_revision::file::history::HistoryOptions;
use lore_revision::file::info;
use lore_revision::file::info::InfoOptions;
use lore_revision::file::obliterate;
use lore_revision::file::reset;
use lore_revision::file::reset::ResetMergeSide;
use lore_revision::file::reset::ResetOptions;
use lore_revision::file::stage;
use lore_revision::file::write;
use lore_revision::file::write::WriteFileOptions;
use lore_revision::instance::InstanceId;
use lore_revision::interface::LoreString;
use lore_revision::layer;
use lore_revision::layer::Layer;
use lore_revision::link::LinkFlags;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::MetadataInherit;
use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::revision::ResolveSearchLocation;
use lore_revision::revision::restore::RestoreOptions;
use lore_revision::stage::StageOptions;
use lore_revision::state::State;
use lore_transport::BranchListResponse;
use lore_transport::BranchPushResponse;
use lore_transport::BranchQueryResponse;
use lore_transport::BranchRecord;
use lore_transport::Connection;
use lore_transport::EnvironmentConfig;
use lore_transport::MetadataSetResult;
use lore_transport::ProtocolError;
use lore_transport::Revision;
use lore_transport::RevisionListResponse;
use lore_transport::RevisionListStart;
use lore_transport::SuppliedCredentials;
use lore_transport::auth::AuthEndpoint;
use lore_transport::auth::AuthMode;
use lore_transport::traits::Admin;
use lore_transport::traits::Environment;
use lore_transport::traits::Lock;
use lore_transport::traits::Protocol;
use lore_transport::traits::Repository;
use lore_transport::traits::Storage;
use serde::Serialize;
use zerocopy::IntoBytes;

use crate::branch::store_unreadable_metadata;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::TempDir;
use crate::helper::TestRepository;
use crate::helper::committed_repository;
use crate::helper::create_branch_on_instance;
use crate::helper::default_repository_creation_args;
use crate::helper::in_memory;
use crate::helper::path_arguments;
use crate::helper::relative_path;
use crate::helper::setup_test_execution;
use crate::helper::stage_paths;
use crate::helper::test_file_write;
use crate::helper::test_repository_create_with_remote;
use crate::helper::test_store_create;

fn refusal() -> ProtocolError {
    ProtocolError::from(NotAuthorized)
}

/// What a [`RefusingRevision`] answers a branch query with.
#[derive(Default, Clone, Copy)]
enum BranchQueryAnswer {
    /// The refusal.
    #[default]
    Refusal,
    /// The branch, at this latest revision.
    Latest(Hash),
    /// No such branch.
    NotFound,
}

/// A remote revision service that refuses the caller on every request but a branch query, which it
/// answers with `answer`, and counts its branch queries.
#[derive(Default)]
struct RefusingRevision {
    queries: AtomicUsize,
    answer: BranchQueryAnswer,
}

#[async_trait]
impl Revision for RefusingRevision {
    async fn branch_create(
        &self,
        _: BranchId,
        _: &str,
        _: &str,
        _: &str,
        _: &[BranchPoint],
    ) -> Result<BranchRecord, ProtocolError> {
        Err(refusal())
    }

    async fn branch_delete(&self, _: BranchId) -> Result<(), ProtocolError> {
        Err(refusal())
    }

    async fn branch_query(
        &self,
        branch: Option<BranchId>,
        _: Option<&str>,
    ) -> Result<BranchQueryResponse, ProtocolError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        let latest = match self.answer {
            BranchQueryAnswer::Refusal => return Err(refusal()),
            BranchQueryAnswer::Latest(latest) => latest,
            BranchQueryAnswer::NotFound => return Err(ProtocolError::from(NotFound)),
        };
        Ok(BranchQueryResponse {
            id: branch.unwrap_or_default(),
            latest,
            ..Default::default()
        })
    }

    async fn branch_push(
        &self,
        _: BranchId,
        _: Hash,
        _: bool,
        _: bool,
    ) -> Result<BranchPushResponse, ProtocolError> {
        Err(refusal())
    }

    async fn branch_list(
        &self,
        _: Option<&str>,
        _: bool,
    ) -> Result<BranchListResponse, ProtocolError> {
        Err(refusal())
    }

    async fn revision_list(
        &self,
        _: RevisionListStart,
    ) -> Result<RevisionListResponse, ProtocolError> {
        Err(refusal())
    }

    async fn branch_metadata_get(&self, _: BranchId) -> Result<Hash, ProtocolError> {
        Err(refusal())
    }

    async fn branch_metadata_set(
        &self,
        _: BranchId,
        _: Hash,
        _: Hash,
    ) -> Result<MetadataSetResult, ProtocolError> {
        Err(refusal())
    }
}

/// A connection whose revision service for `repository` refuses every request.
fn refusing_connection(repository: RepositoryId) -> Arc<Connection> {
    Connection::detached_revision(
        Vec::new(),
        Ok(()),
        repository,
        Arc::new(RefusingRevision::default()),
    )
}

/// A protocol whose server advertises no authentication, and which refuses the caller the connect
/// of every service but these, where set: the admin service, which it answers with `admin`, and
/// the revision service of the repository `served` names, which it answers with the service beside
/// it. It records the repository of each revision service connect.
#[derive(Default)]
struct RefusingProtocol {
    admin: Option<Arc<dyn Admin>>,
    served: Option<(RepositoryId, Arc<dyn Revision>)>,
    revision_connects: parking_lot::Mutex<Vec<RepositoryId>>,
}

/// A server environment that advertises no authentication and no endpoint of its own.
struct Unauthenticated;

#[async_trait]
impl Environment for Unauthenticated {
    async fn get(&self) -> Result<EnvironmentConfig, ProtocolError> {
        Ok(EnvironmentConfig::default())
    }
}

#[async_trait]
impl Protocol for RefusingProtocol {
    async fn storage(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: Partition,
        _: usize,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        Err(refusal())
    }

    async fn revision(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        repository: RepositoryId,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Revision>, ProtocolError> {
        self.revision_connects.lock().push(repository);
        match &self.served {
            Some((served, revision)) if *served == repository => Ok(revision.clone()),
            _ => Err(refusal()),
        }
    }

    async fn repository(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Repository>, ProtocolError> {
        Err(refusal())
    }

    async fn admin(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: RepositoryId,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        self.admin.clone().ok_or_else(refusal)
    }

    async fn lock(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: RepositoryId,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Lock>, ProtocolError> {
        Err(refusal())
    }

    async fn environment(
        &self,
        _: Weak<Connection>,
        _: &str,
    ) -> Result<Arc<dyn Environment>, ProtocolError> {
        Ok(Arc::new(Unauthenticated))
    }
}

/// A connection whose services all refuse the caller's connect.
fn refusing_services() -> Arc<Connection> {
    Connection::detached_protocol(Arc::new(RefusingProtocol::default()))
}

/// A connection the connection cache holds, which is taken out of the cache when this is dropped.
struct CachedConnection(Arc<Connection>);

impl Drop for CachedConnection {
    fn drop(&mut self) {
        lore_transport::connection::remove_connection(self.0.clone());
    }
}

/// A connection over `protocol` to a remote of its own, opened for `repository` as a command opens
/// one. The connection cache holds it as it holds a command's, so a link or a layer context
/// connecting from it is handed it, until the [`CachedConnection`] is dropped. The scheme it
/// registers for `protocol` stays registered, holding `protocol`, until the process ends: the
/// transport has no way to remove a scheme.
async fn connected(protocol: Arc<RefusingProtocol>, repository: RepositoryId) -> CachedConnection {
    // The protocol a scheme names is process-wide, so each protocol takes a scheme of its own.
    let scheme = format!("refusal-{}", uuid::Uuid::now_v7().simple());
    lore_transport::connection::add(&scheme, protocol).expect("registering the scheme");
    CachedConnection(
        lore_transport::connection::connect(
            &format!("{scheme}://remote.invalid"),
            "",
            repository,
            1,
            "",
            "",
            AuthMode::Auto,
        )
        .await
        .expect("the remote advertises no authentication"),
    )
}

/// An admin service that answers every obliterate with its error.
struct FailingAdmin(ProtocolError);

#[async_trait]
impl Admin for FailingAdmin {
    async fn obliterate(&self, _: Address) -> Result<(), ProtocolError> {
        Err(self.0.clone())
    }
}

/// A connection whose admin service answers every obliterate with `error`, and whose other services
/// refuse the caller's connect.
fn failing_admin(error: ProtocolError) -> Arc<Connection> {
    Connection::detached_protocol(Arc::new(RefusingProtocol {
        admin: Some(Arc::new(FailingAdmin(error))),
        ..RefusingProtocol::default()
    }))
}

/// A writable context for `repository` on in-memory stores, holding nothing, whose remote is
/// `remote`.
///
/// Each test gives its contexts a repository of their own: a read joins a read of the same address
/// in the same repository already in flight, in any test, and takes its failure.
async fn context_with_remote(
    repository: RepositoryId,
    remote: RemoteState,
) -> Arc<RepositoryContext> {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    Arc::new(
        RepositoryContext::new_with_state(
            None,
            immutable,
            mutable,
            repository,
            InstanceId::default(),
            remote,
            Arc::default(),
            None,
        )
        .with_write_token(in_memory::token()),
    )
}

/// Each way the remote of `repository` refuses the caller: the connect, the connect of a service,
/// and every request to a service it connected.
fn refusing_remotes(
    repository: RepositoryId,
) -> [(&'static str, Result<Arc<Connection>, ProtocolError>); 3] {
    [
        ("refused connect", Err(refusal())),
        ("refused service", Ok(refusing_services())),
        ("refused request", Ok(refusing_connection(repository))),
    ]
}

/// A context holding nothing over each of the [`refusing_remotes`], all for one repository of their
/// own, which a test reads through one context at a time.
async fn refusing_contexts() -> Vec<(&'static str, Arc<RepositoryContext>)> {
    let repository = RepositoryId::from(uuid::Uuid::now_v7());
    let mut contexts = Vec::new();
    for (name, remote) in refusing_remotes(repository) {
        let remote = match remote {
            Ok(connection) => RemoteState::Connected(connection),
            Err(refusal) => RemoteState::Failed(refusal),
        };
        contexts.push((name, context_with_remote(repository, remote).await));
    }
    contexts
}

/// An instance on its default branch at no revision, whose remote refused the connect.
async fn refused_instance() -> TestRepository {
    let (immutable, mutable, _execution) = test_store_create()
        .await
        .expect("in-memory stores should be creatable");
    test_repository_create_with_remote(
        immutable,
        mutable,
        Context::from(uuid::Uuid::now_v7()).into(),
        Err(refusal()),
    )
    .await
}

/// The repository the linked state of [`linked_committed_instance`] links.
fn linked_repository() -> RepositoryId {
    RepositoryId::from([9; 16])
}

/// [`linked_committed_instance`] opened again over a remote that refuses the connect.
async fn refused_committed_instance() -> (Arc<RepositoryContext>, Hash, TempDir) {
    let (repository, linked, tempdir) = linked_committed_instance().await;
    (
        reopened(&repository, &tempdir, Err(refusal())),
        linked,
        tempdir,
    )
}

/// [`linked_committed_instance`] opened again over a remote that serves the instance's repository,
/// holding no branch of it, and refuses the caller every other repository, beside the protocol that
/// remote runs and the cached connection to it.
async fn served_committed_instance() -> (
    Arc<RepositoryContext>,
    Hash,
    TempDir,
    Arc<RefusingProtocol>,
    CachedConnection,
) {
    let (repository, linked, tempdir) = linked_committed_instance().await;
    let served: Arc<dyn Revision> = Arc::new(RefusingRevision {
        answer: BranchQueryAnswer::NotFound,
        ..RefusingRevision::default()
    });
    let protocol = Arc::new(RefusingProtocol {
        served: Some((repository.id, served)),
        ..RefusingProtocol::default()
    });
    let remote = connected(protocol.clone(), repository.id).await;
    (
        reopened(&repository, &tempdir, Ok(remote.0.clone())),
        linked,
        tempdir,
        protocol,
        remote,
    )
}

/// A committed instance, and the state of its revision with a link to [`linked_repository`] added,
/// stored and anchored nowhere. The instance's directory is removed with the [`TempDir`].
async fn linked_committed_instance() -> (Arc<RepositoryContext>, Hash, TempDir) {
    let (repository, tempdir) = committed_repository("", &[]).await;
    let token = repository
        .try_write_token()
        .expect("The repository was opened for writing");
    let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(&repository)
        .await
        .expect("loading the current anchor");
    let state = State::deserialize(repository.clone(), revision)
        .await
        .expect("reading the committed state");
    state
        .link_add(
            repository.clone(),
            linked_repository(),
            BranchId::default(),
            Hash::from([9; 32]),
            1,
            LinkFlags::NoFlags,
        )
        .await
        .expect("adding a link");
    state.mark_dirty();
    let linked = state
        .serialize(repository.clone(), token)
        .await
        .expect("storing the linked state");
    (repository, linked, tempdir)
}

/// `repository`, rooted at `tempdir`, opened again over `remote`.
fn reopened(
    repository: &RepositoryContext,
    tempdir: &TempDir,
    remote: Result<Arc<Connection>, ProtocolError>,
) -> Arc<RepositoryContext> {
    Arc::new(
        RepositoryContext::new(
            default_repository_creation_args(
                repository.immutable_store(),
                repository.mutable_store.clone(),
            )
            .with_path(tempdir.path())
            .with_id(repository.id)
            .with_instance_id(repository.instance_id)
            .with_remote(remote),
        )
        .with_write_token(token_of(repository)),
    )
}

/// Creates the branch `feature` through `repository`.
async fn create_feature_branch(
    repository: Arc<RepositoryContext>,
) -> Result<(), branch::create::CreateError> {
    create_branch_on_instance(&repository, "feature", None, "", false).await
}

async fn with_execution<F: Future>(body: F) -> F::Output {
    LORE_CONTEXT.scope(setup_test_execution(), body).await
}

/// A revision's state the instance does not hold is read from the remote, whose refusal is the
/// read's failure.
#[tokio::test]
async fn a_state_read_the_remote_refuses_reports_the_refusal() {
    with_execution(async {
        for refusal in [
            ProtocolError::from(NotAuthenticated),
            ProtocolError::from(NotAuthorized),
        ] {
            let code = refusal.ffi_code();
            let repository = context_with_remote(
                RepositoryId::from(uuid::Uuid::now_v7()),
                RemoteState::Failed(refusal),
            )
            .await;
            let Err(error) = State::deserialize(repository, Hash::from([6; 32])).await else {
                panic!("the remote refuses the read");
            };
            assert_eq!(error.ffi_code(), code, "{error:?}");
        }
    })
    .await;
}

/// An audit whose chain crosses into a linked repository reports that repository's state, refused
/// by the remote, as an unavailable link.
#[tokio::test]
async fn a_refused_linked_state_is_an_unavailable_link_to_an_audit() {
    with_execution(async {
        let linked = context_with_remote(
            RepositoryId::from(uuid::Uuid::now_v7()),
            RemoteState::Failed(refusal()),
        )
        .await;
        let read = State::deserialize(linked, Hash::from([5; 32]))
            .await
            .map(|_state| ())
            .forward::<AuditError>("reading the linked state");
        let outcome = gap(
            read,
            Reachability {
                linked: true,
                offline: false,
            },
        );
        assert!(
            matches!(outcome, Ok(Err(LoreFileAuditOutcome::LinkUnavailable))),
            "{outcome:?}"
        );
    })
    .await;
}

/// The remote's answer for a branch's latest revision is the refusal, not a branch the remote
/// does not hold.
#[tokio::test]
async fn a_remote_latest_lookup_reports_the_refusal() {
    with_execution(async {
        let error = branch::load_remote_latest(
            refusing_connection(RepositoryId::default()),
            RepositoryId::default(),
            BranchId::from([1; 16]),
        )
        .await
        .expect_err("the remote refuses the query");
        assert!(error.is_not_authorized(), "{error:?}");
    })
    .await;
}

/// A branch the instance holds nothing for is looked up on the remote: its id by name, the branch
/// by name and by id, its metadata, whether it exists without a latest revision, and the stacks a
/// diff base is resolved from.
#[tokio::test]
async fn a_branch_lookup_on_the_remote_reports_the_refusal() {
    Box::pin(with_execution(async {
        let (source, target) = (BranchId::from([1; 16]), BranchId::from([2; 16]));
        for (name, repository) in refusing_contexts().await {
            let error = branch::load_name_to_id(repository.clone(), "feature")
                .await
                .expect_err(name);
            assert!(error.is_not_authorized(), "{name}, by name: {error:?}");

            for specifier in ["feature".to_string(), source.to_string()] {
                let error = branch::resolve(repository.clone(), &specifier)
                    .await
                    .expect_err(name);
                assert!(error.is_not_authorized(), "{name}, {specifier}: {error:?}");
            }

            let error = branch::metadata_hash(repository.clone(), source)
                .await
                .expect_err(name);
            assert!(
                error.is_not_authorized(),
                "{name}, metadata hash: {error:?}"
            );

            let error = branch::metadata(repository.clone(), source)
                .await
                .expect_err(name);
            assert!(error.is_not_authorized(), "{name}, metadata: {error:?}");

            let error = branch::load_latest(repository.clone(), source)
                .await
                .expect_err(name);
            assert!(error.is_not_authorized(), "{name}, latest: {error:?}");

            let error = branch::resolve_diff3_base(
                repository,
                source,
                Hash::from([1; 32]),
                target,
                Hash::from([2; 32]),
            )
            .await
            .expect_err(name);
            assert!(error.is_not_authorized(), "{name}, diff base: {error:?}");
        }
    }))
    .await;
}

/// A context holding nothing whose remote's revision service is the [`RefusingRevision`] returned
/// beside it.
async fn counting_refusal_context() -> (Arc<RepositoryContext>, Arc<RefusingRevision>) {
    let repository = RepositoryId::from(uuid::Uuid::now_v7());
    let revision = Arc::new(RefusingRevision::default());
    let connection =
        Connection::detached_revision(Vec::new(), Ok(()), repository, revision.clone());
    (
        context_with_remote(repository, RemoteState::Connected(connection)).await,
        revision,
    )
}

/// A branch resolution the remote refuses asks the remote once, by name and by id.
#[tokio::test]
async fn a_refused_branch_resolution_asks_the_remote_once() {
    with_execution(async {
        for specifier in ["feature".to_string(), BranchId::from([1; 16]).to_string()] {
            let (repository, revision) = counting_refusal_context().await;
            let error = branch::resolve(repository, &specifier)
                .await
                .expect_err(&specifier);
            assert!(error.is_not_authorized(), "{specifier}: {error:?}");
            assert_eq!(revision.queries.load(Ordering::SeqCst), 1, "{specifier}");
        }
    })
    .await;
}

/// A branch metadata lookup the remote refuses asks the remote once.
#[tokio::test]
async fn a_refused_branch_metadata_lookup_asks_the_remote_once() {
    with_execution(async {
        let (repository, revision) = counting_refusal_context().await;
        let error = branch::metadata(repository, BranchId::from([1; 16]))
            .await
            .expect_err("the remote refuses the query");
        assert!(error.is_not_authorized(), "{error:?}");
        assert_eq!(revision.queries.load(Ordering::SeqCst), 1);
    })
    .await;
}

/// A branch whose metadata hash the instance holds, over local bytes that are not metadata, has
/// its metadata looked up on the remote.
#[tokio::test]
async fn a_branch_metadata_lookup_past_an_unreadable_local_copy_reports_the_refusal() {
    with_execution(async {
        let branch = BranchId::from([1; 16]);
        for (name, repository) in refusing_contexts().await {
            store_unreadable_metadata(&repository, branch).await;
            let error = branch::metadata(repository, branch).await.expect_err(name);
            assert!(error.is_not_authorized(), "{name}: {error:?}");
        }
    })
    .await;
}

/// A batch of a line that leaves the local store, over each refusing remote, holds the revisions
/// the walk of the local store read: the refusal ends the line there.
#[tokio::test]
async fn a_batch_over_a_refusing_remote_holds_the_local_revisions() {
    with_execution(async {
        for (name, repository) in refusing_contexts().await {
            let line = crate::branch::write_line(&repository, Hash::from([9; 32]), 2, 2).await;

            let history = lore_revision::find::batch_load_history(repository, line[1])
                .await
                .expect(name);

            assert_eq!(history, vec![(3, line[1]), (2, line[0])], "{name}");
        }
    })
    .await;
}

/// The diff base of two revisions on one branch whose histories don't meet is the branch point,
/// from the branch's metadata. Metadata the instance does not hold is read from the remote, whose
/// refusal is the failure rather than no branch point.
#[tokio::test]
async fn a_diff_base_on_one_branch_reports_the_refusal_of_its_metadata() {
    Box::pin(with_execution(async {
        let (refused, linked, _tempdir) = refused_committed_instance().await;
        let (revision, current_branch) =
            lore_revision::instance::load_current_anchor_boxed(&refused)
                .await
                .expect("loading the current anchor");
        let metadata = branch::metadata_hash(refused.clone(), current_branch)
            .await
            .expect("the instance holds the metadata hash");
        drop_fragment(&refused, metadata).await;

        let error =
            branch::resolve_diff3_base(refused, current_branch, revision, current_branch, linked)
                .await
                .expect_err("the remote refuses the metadata");
        assert!(error.is_not_authorized(), "{error:?}");
    }))
    .await;
}

/// A create looks the new branch's metadata up, on the remote where the instance holds none. The
/// remote's refusal of the connect, of the revision service's connect or of the query fails the
/// create before it writes the branch or moves the instance onto it.
#[tokio::test]
async fn creating_a_branch_the_remote_refuses_writes_nothing() {
    Box::pin(with_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        let anchor = lore_revision::instance::load_current_anchor_boxed(&repository)
            .await
            .expect("loading the current anchor");
        let feature = BranchId::from([5; 16]);
        let mut differing = Vec::new();
        for (case, remote) in refusing_remotes(repository.id) {
            let refused = reopened(&repository, &tempdir, remote);
            let created =
                create_branch_on_instance(&refused, "feature", Some(feature), "", false).await;
            expect_refused(&mut differing, case, created);
            match branch::load_name_to_id_local(refused.clone(), "feature").await {
                Err(err) if err.is_branch_not_found() => {}
                mapped => differing.push(format!("{case}: the name maps to {mapped:?}")),
            }
            if branch::exist_local(refused.clone(), feature).await {
                differing.push(format!("{case}: the branch has metadata"));
            }
            let current = lore_revision::instance::load_current_anchor_boxed(&refused).await;
            if current.as_ref().ok() != Some(&anchor) {
                differing.push(format!("{case}: the current anchor is {current:?}"));
            }
        }
        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}

/// A create makes the new branch in each linked repository, once the instance's own lookups are
/// answered. A linked repository the remote refuses the caller fails the create with the refusal.
#[tokio::test]
async fn creating_a_branch_a_linked_repository_refuses_reports_the_refusal() {
    Box::pin(with_execution(async {
        let (instance, linked, _tempdir, protocol, _remote) = served_committed_instance().await;
        lore_revision::instance::store_staged_anchor(&instance, linked)
            .await
            .expect("staging the link");

        let error = create_feature_branch(instance.clone())
            .await
            .expect_err("the remote refuses the linked repository");
        assert!(error.is_not_authorized(), "{error:?}");
        assert_eq!(
            *protocol.revision_connects.lock(),
            [instance.id, linked_repository()],
            "the revision services connected"
        );
    }))
    .await;
}

/// A create makes the new branch in each layer, once the instance holds it. A layer the remote
/// refuses the caller fails the create with the refusal.
#[tokio::test]
async fn creating_a_branch_a_layer_refuses_reports_the_refusal() {
    #[derive(Serialize)]
    struct LayerConfig {
        layers: Vec<Layer>,
    }

    Box::pin(with_execution(async {
        let (instance, _linked, _tempdir, protocol, _remote) = served_committed_instance().await;
        let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(&instance)
            .await
            .expect("loading the current anchor");
        let layer_id = RepositoryId::from([8; 16]);
        lore_revision::util::config::save(
            &LayerConfig {
                layers: vec![Layer {
                    target_path: "layer".to_string(),
                    source_path: String::new(),
                    repository: layer_id,
                    metadata: None,
                    current: revision,
                    staged: Hash::default(),
                }],
            },
            layer::layer_config_path(&instance).expect("layer config path"),
        )
        .await
        .expect("writing the layer set");

        let error = create_feature_branch(instance.clone())
            .await
            .expect_err("the remote refuses the layer");
        assert!(error.is_not_authorized(), "{error:?}");
        let connects = protocol.revision_connects.lock().clone();
        assert!(connects.contains(&layer_id), "{connects:?}");
        branch::load_name_to_id_local(instance, "feature")
            .await
            .expect("the instance holds the branch");
    }))
    .await;
}

/// A forced create of a branch, under an id the instance holds for another branch, looks the new
/// name up to replace the branch it names. A name the instance does not hold is looked up on the
/// remote, whose refusal fails the create rather than reporting the branch as existing.
#[tokio::test]
async fn force_creating_a_branch_the_remote_refuses_reports_the_refusal() {
    Box::pin(with_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        create_feature_branch(repository.clone())
            .await
            .expect("creating the feature branch");
        let feature = branch::load_name_to_id_local(repository.clone(), "feature")
            .await
            .expect("the instance holds the feature branch");
        let refused = reopened(&repository, &tempdir, Err(refusal()));

        let error = create_branch_on_instance(&refused, "renamed", Some(feature), "", true)
            .await
            .expect_err("the remote refuses the lookup of the new name");
        assert!(error.is_not_authorized(), "{error:?}");
    }))
    .await;
}

/// An archive of a branch the instance holds no metadata for reads the metadata on the remote,
/// whose refusal fails the archive rather than reporting the branch as not found.
#[tokio::test]
async fn archiving_a_branch_the_remote_refuses_reports_the_refusal() {
    Box::pin(with_execution(async {
        for (name, repository) in refusing_contexts().await {
            let error = branch::delete(repository, BranchId::from([1; 16]))
                .await
                .expect_err(name);
            assert!(error.is_not_authorized(), "{name}: {error:?}");
        }
    }))
    .await;
}

#[tokio::test]
async fn a_layer_latest_lookup_reports_the_refusal() {
    with_execution(async {
        for (name, repository) in refusing_contexts().await {
            let error = lore_revision::layer::latest_revision(repository, BranchId::from([1; 16]))
                .await
                .expect_err(name);
            assert!(error.is_not_authorized(), "{name}: {error:?}");
        }
    })
    .await;
}

#[tokio::test]
async fn merging_into_a_branch_reports_the_refusal() {
    Box::pin(with_execution(async {
        let instance = refused_instance().await;
        let error = branch::merge::merge_into(
            instance.repository.clone(),
            &instance.write_token,
            BranchId::from([1; 16]),
            MergeIntoOptions {
                message: String::new(),
                link: None,
                ignore_links: false,
                inherit_metadata: MetadataInherit::default(),
            },
        )
        .await
        .expect_err("the remote refuses the connect");
        assert!(error.is_not_authorized(), "{error:?}");
    }))
    .await;
}

#[tokio::test]
async fn restoring_a_revision_reports_the_refusal() {
    Box::pin(with_execution(async {
        let instance = refused_instance().await;
        let error = lore_revision::revision::restore::restore_boxed(
            instance.repository.clone(),
            &instance.write_token,
            RestoreOptions { message: None },
        )
        .await
        .expect_err("the remote refuses the connect");
        assert!(error.is_not_authorized(), "{error:?}");
    }))
    .await;
}

/// A branch diff asks the remote for the latest revision of each branch the instance holds, so
/// the remote's refusal of the connect, of a service's connect or of the query is the command's
/// failure.
#[tokio::test]
async fn diffing_branches_reports_the_refusal() {
    Box::pin(with_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        create_feature_branch(repository.clone())
            .await
            .expect("creating the feature branch");
        let mut differing = Vec::new();
        for (case, remote) in refusing_remotes(repository.id) {
            let diffed = branch::diff::diff(
                reopened(&repository, &tempdir, remote),
                branch::DEFAULT_DEFAULT_NAME.to_string(),
                "feature".to_string(),
                String::new(),
                false,
            );
            expect_refused(&mut differing, case, diffed.await);
        }
        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}

/// A branch diff and a layer's latest revision lookup take the higher-numbered of a branch's local
/// and remote latest revisions, read from their states. A state the instance does not hold is read
/// from the remote, whose refusal is the failure rather than the local latest.
#[tokio::test]
async fn a_refused_read_of_a_remote_latest_state_fails_the_lookup() {
    Box::pin(with_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        create_feature_branch(repository.clone())
            .await
            .expect("creating the feature branch");
        let (_revision, current_branch) =
            lore_revision::instance::load_current_anchor_boxed(&repository)
                .await
                .expect("loading the current anchor");
        let storage_refused = Connection::detached_revision(
            Vec::new(),
            Err(refusal()),
            repository.id,
            Arc::new(RefusingRevision {
                answer: BranchQueryAnswer::Latest(Hash::from([7; 32])),
                ..RefusingRevision::default()
            }),
        );
        let refused = reopened(&repository, &tempdir, Ok(storage_refused));

        let mut differing = Vec::new();
        let diffed = branch::diff::diff(
            refused.clone(),
            branch::DEFAULT_DEFAULT_NAME.to_string(),
            "feature".to_string(),
            String::new(),
            false,
        );
        expect_refused(&mut differing, "branch diff", diffed.await);
        let latest = lore_revision::layer::latest_revision(refused, current_branch);
        expect_refused(&mut differing, "layer latest", latest.await);
        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}

/// A revision named on the branch the instance is on, by its latest or by a number, is looked up
/// from the remote's latest revision of the branch as well as the local one, and a number from the
/// remote's list of the branch's revisions. The remote's refusal of the connect, of a service's
/// connect, of the query or of the list fails the lookup, rather than leaving it to the local
/// latest.
#[tokio::test]
async fn resolving_a_revision_on_the_current_branch_reports_the_refusal() {
    Box::pin(with_execution(async {
        let (repository, tempdir) = committed_repository("", &[]).await;
        let mut differing = Vec::new();
        for (case, remote) in refusing_remotes(repository.id) {
            let refused = reopened(&repository, &tempdir, remote);
            for specifier in ["@LATEST", "@1"] {
                let resolved = lore_revision::revision::resolve_boxed(
                    refused.clone(),
                    specifier,
                    ResolveSearchLocation::RemoteOrLocal,
                );
                expect_refused(
                    &mut differing,
                    &format!("{case}, {specifier}"),
                    resolved.await,
                );
            }
        }

        let (latest, _branch) = lore_revision::instance::load_current_anchor_boxed(&repository)
            .await
            .expect("loading the current anchor");
        let listing_refused = Connection::detached_revision(
            Vec::new(),
            Ok(()),
            repository.id,
            Arc::new(RefusingRevision {
                answer: BranchQueryAnswer::Latest(latest),
                ..RefusingRevision::default()
            }),
        );
        let resolved = lore_revision::revision::resolve_boxed(
            reopened(&repository, &tempdir, Ok(listing_refused)),
            "@1",
            ResolveSearchLocation::RemoteOrLocal,
        );
        expect_refused(&mut differing, "refused list, @1", resolved.await);

        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}

/// The address an obliterate test stores its payload at and obliterates.
fn obliterated() -> Address {
    Address {
        hash: Hash::from([3; 32]),
        context: Context::from([3; 16]),
    }
}

/// A context over `remote`, for a repository of its own, holding `payload` at [`obliterated`].
async fn context_holding(remote: RemoteState, payload: &Bytes) -> Arc<RepositoryContext> {
    let repository = context_with_remote(RepositoryId::from(uuid::Uuid::now_v7()), remote).await;
    repository
        .immutable_store()
        .put(
            repository.id,
            obliterated(),
            Fragment {
                flags: 0,
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(payload.clone()),
            false,
        )
        .await
        .expect("storing the content to obliterate");
    repository
}

/// An obliterate asks the remote before it changes the local store, so a remote that refuses the
/// connect, the admin service or the obliterate request fails it with the local content kept.
#[tokio::test]
async fn a_refused_obliterate_keeps_the_local_content() {
    Box::pin(with_execution(async {
        let address = obliterated();
        let payload = Bytes::from_static(b"payload");
        for (name, remote) in [
            ("refused connect", RemoteState::Failed(refusal())),
            ("refused admin", RemoteState::Connected(refusing_services())),
            (
                "refused request",
                RemoteState::Connected(failing_admin(refusal())),
            ),
        ] {
            let repository = context_holding(remote, &payload).await;

            let error = lore_revision::file::obliterate::obliterate_address_boxed(
                repository.clone(),
                address,
            )
            .await
            .expect_err(name);
            assert!(error.is_not_authorized(), "{name}: {error:?}");

            let held = repository
                .immutable_store()
                .get(repository.id, address)
                .await
                .unwrap_or_else(|err| panic!("{name}, local: {err:?}"));
            assert_eq!(held.payload.as_ref(), Some(&payload), "{name}");
        }
    }))
    .await;
}

/// A remote that does not hold the address counts it as obliterated, so the obliterate removes the
/// local content.
#[tokio::test]
async fn obliterating_content_the_remote_lacks_removes_the_local_content() {
    Box::pin(with_execution(async {
        let repository = context_holding(
            RemoteState::Connected(failing_admin(ProtocolError::from(NotFound))),
            &Bytes::from_static(b"payload"),
        )
        .await;

        lore_revision::file::obliterate::obliterate_address_boxed(
            repository.clone(),
            obliterated(),
        )
        .await
        .expect("the remote does not hold the address");

        let held = repository
            .immutable_store()
            .get(repository.id, obliterated())
            .await
            .expect_err("the local content is obliterated");
        assert!(held.is_address_not_found(), "{held:?}");
    }))
    .await;
}

/// Records `case` in `differing` unless `result` failed with `NotAuthorized`.
fn expect_refused<T, E: FfiError + std::fmt::Debug>(
    differing: &mut Vec<String>,
    case: &str,
    result: Result<T, E>,
) {
    match result {
        Err(error) if error.ffi_code() == NotAuthorized.ffi_code() => {}
        Err(error) => differing.push(format!("{case}: {error:?}")),
        Ok(_) => differing.push(format!("{case}: succeeded")),
    }
}

/// What [`refused_staged_instance`] drops from the store.
enum Dropped {
    /// The tree the staged state shares with the committed one.
    Tree,
    /// The staged state's link list.
    LinkList,
}

/// [`refused_committed_instance`] staged on the state holding the link, after the store drops
/// `dropped`, so a read of it goes to the remote.
async fn refused_staged_instance(dropped: Dropped) -> (Arc<RepositoryContext>, TempDir) {
    let (refused, linked, tempdir) = refused_committed_instance().await;
    lore_revision::instance::store_staged_anchor(&refused, linked)
        .await
        .expect("staging the link");
    let Ok(state) = State::deserialize(refused.clone(), linked).await else {
        panic!("the staged state is held locally");
    };
    let hash = match dropped {
        Dropped::Tree => state.tree_hash(),
        Dropped::LinkList => {
            let links = state
                .link_list(refused.clone())
                .await
                .expect("the link list is held locally");
            lore_storage::hash::hash_slice(links.as_bytes())
        }
    };
    drop_fragment(&refused, hash).await;
    (refused, tempdir)
}

/// The write token `repository` holds.
fn token_of(repository: &RepositoryContext) -> RepositoryWriteToken {
    repository
        .try_write_token()
        .expect("The repository was opened for writing")
        .share()
}

/// A command naming a revision on a branch the instance does not hold looks the branch up on the
/// remote, whose refusal is the command's failure.
#[tokio::test]
async fn a_revision_named_on_a_refused_branch_fails_the_command() {
    Box::pin(with_execution(async {
        let (refused, _linked, tempdir) = refused_committed_instance().await;
        let token = token_of(&refused);
        let file = tempdir
            .path()
            .join("seed.txt")
            .to_string_lossy()
            .to_string();
        let output = TempDir::new("refusal-write-");
        let named = || Some("feature@1".to_string());
        let diff_options = DiffOptions {
            context_lines: 3,
            ignore_whitespace_eol: false,
            ignore_whitespace_inline: false,
        };
        let mut differing = Vec::new();

        let diffed = diff::diff(
            refused.clone(),
            named(),
            None,
            vec![relative_path("seed.txt")],
            false,
            diff_options,
        );
        expect_refused(&mut differing, "file diff source", diffed.await);
        let diffed = diff::diff(
            refused.clone(),
            None,
            named(),
            vec![relative_path("seed.txt")],
            false,
            diff_options,
        );
        expect_refused(&mut differing, "file diff target", diffed.await);
        let listed = history::history(
            refused.clone(),
            file.clone(),
            HistoryOptions {
                revision: named(),
                branch: None,
                length: 1,
                depth: 0,
            },
        );
        expect_refused(&mut differing, "file history", listed.await);
        let written = write::write_file_boxed(
            refused.clone(),
            Some(&token),
            file.clone(),
            output.child("seed.txt").to_string_lossy().to_string(),
            WriteFileOptions { revision: named() },
        );
        expect_refused(&mut differing, "file write", written.await);
        let described = info::info(
            refused.clone(),
            vec![relative_path("seed.txt")],
            InfoOptions {
                revision: named(),
                local: false,
                filtered: false,
            },
        );
        expect_refused(&mut differing, "file info", described.await);
        let reset = reset::reset(
            refused.clone(),
            &token,
            path_arguments(tempdir.path(), &["seed.txt"]),
            LoreString::from("feature@1"),
            ResetOptions::default(),
        );
        expect_refused(&mut differing, "file reset", reset.await);

        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}

/// A command reading a part of the staged state the instance does not hold, its tree or its link
/// list, reads it from the remote, whose refusal is the command's failure.
#[tokio::test]
async fn a_refused_read_of_the_staged_state_fails_the_command() {
    Box::pin(with_execution(async {
        let mut differing = Vec::new();

        let (refused, tempdir) = refused_staged_instance(Dropped::Tree).await;

        let token = token_of(&refused);
        let output = TempDir::new("refusal-write-");
        let written = write::write_file_boxed(
            refused.clone(),
            Some(&token),
            tempdir
                .path()
                .join("seed.txt")
                .to_string_lossy()
                .to_string(),
            output.child("seed.txt").to_string_lossy().to_string(),
            WriteFileOptions { revision: None },
        );
        expect_refused(&mut differing, "file write", written.await);

        let (refused, tempdir) = refused_staged_instance(Dropped::Tree).await;

        let token = token_of(&refused);
        let obliterated = obliterate::obliterate_file_boxed(
            refused.clone(),
            &token,
            tempdir
                .path()
                .join("seed.txt")
                .to_string_lossy()
                .to_string(),
        );
        expect_refused(&mut differing, "file obliterate", obliterated.await);

        let (refused, tempdir) = refused_staged_instance(Dropped::LinkList).await;

        let token = token_of(&refused);
        let reset = reset::reset(
            refused.clone(),
            &token,
            path_arguments(tempdir.path(), &["seed.txt"]),
            LoreString::from(""),
            ResetOptions::default(),
        );
        expect_refused(&mut differing, "file reset", reset.await);

        let (refused, _tempdir) = refused_staged_instance(Dropped::Tree).await;

        let token = token_of(&refused);
        let committed = commit::commit_boxed(
            refused.clone(),
            &token,
            CommitOptions {
                link: Some("link".to_string()),
                ..CommitOptions::new("Link".to_string())
            },
        );
        expect_refused(&mut differing, "commit --link", committed.await);

        let (refused, tempdir) = refused_staged_instance(Dropped::Tree).await;

        let token = token_of(&refused);
        test_file_write(tempdir.path().join("added.txt").as_path(), b"added");
        let staged = stage::stage(
            refused.clone(),
            &token,
            path_arguments(tempdir.path(), &["added.txt"]),
            StageOptions::default(),
        );
        expect_refused(&mut differing, "stage", staged.await);

        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}

/// `lore file info` finds a path's node in the tree of the current revision. Where the instance
/// does not hold the tree, it reads it from the remote, whose refusal is the command's failure
/// rather than a path the revision lacks.
#[tokio::test]
async fn a_refused_read_of_the_current_tree_fails_file_info() {
    Box::pin(with_execution(async {
        let (refused, _linked, _tempdir) = refused_committed_instance().await;
        let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(&refused)
            .await
            .expect("loading the current anchor");
        let tree = state_of(&refused, revision).await.tree_hash();
        drop_fragment(&refused, tree).await;

        let error = info::info(
            refused,
            vec![relative_path("seed.txt")],
            InfoOptions {
                revision: None,
                local: false,
                filtered: false,
            },
        )
        .await
        .expect_err("the remote refuses the tree");
        assert!(error.is_not_authorized(), "{error:?}");
    }))
    .await;
}

/// A committed instance holding more files in `dir` than a node block holds, opened again over a
/// remote that refuses the connect, after the store drops the second node block of its revision,
/// so a read of a node there goes to the remote. The instance's directory is removed with the
/// [`TempDir`].
async fn refused_node_block_instance() -> (Arc<RepositoryContext>, TempDir) {
    let files: Vec<String> = (0..BLOCK_NODE_COUNT)
        .map(|index| format!("dir/{index}"))
        .collect();
    let (repository, tempdir) = committed_repository("", &files).await;
    let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(&repository)
        .await
        .expect("loading the current anchor");
    let (blocks, _file_metadata) = state_of(&repository, revision)
        .await
        .block_address_lists(repository.clone())
        .await
        .expect("the block address lists are held locally");
    drop_fragment(&repository, blocks.as_type_slice::<Hash>()[1]).await;
    (reopened(&repository, &tempdir, Err(refusal())), tempdir)
}

/// A reset of a directory and a filtered `file info` of it walk the nodes below it. A node block
/// the instance does not hold is read from the remote, whose refusal is the command's failure.
#[tokio::test]
async fn a_refused_read_of_a_node_block_below_a_directory_fails_the_command() {
    Box::pin(with_execution(async {
        let mut differing = Vec::new();

        let (refused, tempdir) = refused_node_block_instance().await;
        let token = token_of(&refused);
        let reset = reset::reset(
            refused.clone(),
            &token,
            path_arguments(tempdir.path(), &["dir"]),
            LoreString::from(""),
            ResetOptions::default(),
        );
        expect_refused(&mut differing, "file reset", reset.await);

        let (refused, _tempdir) = refused_node_block_instance().await;
        let described = info::info(
            refused,
            vec![relative_path("dir")],
            InfoOptions {
                revision: None,
                local: false,
                filtered: true,
            },
        );
        expect_refused(&mut differing, "file info --filtered", described.await);

        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}

/// What a reset to where `file.txt` was last merged finds the instance lacks, so it reads that
/// from the remote.
#[derive(Debug, Clone, Copy)]
enum LastMergedGap {
    /// The state of the revision the file changed in before its latest change.
    EarlierState,
    /// The tree of that revision, whose state the instance holds.
    EarlierTree,
    /// The merged-in parent of the revision the reset starts from, which is a merge.
    MergedInParent,
    /// The metadata of that merged-in parent, here the branch point, whose state the instance
    /// holds.
    MergedInParentMetadata,
    /// The state of the last revision holding the file, which the current revision removed.
    StateBeforeRemoval,
    /// The tree of that revision, whose state the instance holds.
    TreeBeforeRemoval,
    /// The tree of the current revision.
    CurrentTree,
}

/// A committed instance on the branch `feature`, created from the default branch, which changed
/// `file.txt` twice and then, where `removed` says, removed it. Answers the instance, its
/// directory, and the revisions of those commits in order.
async fn last_merged_instance(removed: bool) -> (Arc<RepositoryContext>, TempDir, Vec<Hash>) {
    let (repository, tempdir) = committed_repository("", &["file.txt".to_string()]).await;
    create_feature_branch(repository.clone())
        .await
        .expect("creating the feature branch");
    let feature = branch::load_name_to_id(repository.clone(), "feature")
        .await
        .expect("the feature branch is held locally");
    lore_revision::instance::store_current_anchor_branch(&repository, feature)
        .await
        .expect("moving the instance to the feature branch");
    let file = tempdir.path().join("file.txt");
    let mut commits = Vec::new();
    for content in ["second", "third"] {
        test_file_write(file.as_path(), content.as_bytes());
        commits.push(commit_file(&repository, &tempdir).await);
    }
    if removed {
        #[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
        std::fs::remove_file(&file).expect("removing the file");
        commits.push(commit_file(&repository, &tempdir).await);
    }
    (repository, tempdir, commits)
}

/// Stages and commits `file.txt` of `repository`, rooted at `tempdir`, answering the revision.
async fn commit_file(repository: &Arc<RepositoryContext>, tempdir: &TempDir) -> Hash {
    stage_paths(repository, tempdir.path(), &["file.txt"], true, false).await;
    commit::commit_boxed(
        repository.clone(),
        &token_of(repository),
        CommitOptions::new("Change".to_string()),
    )
    .await
    .expect("committing the change")
}

/// The state of `revision`, which `repository` holds.
async fn state_of(repository: &Arc<RepositoryContext>, revision: Hash) -> Arc<State> {
    State::deserialize(repository.clone(), revision)
        .await
        .expect("the state is held locally")
}

/// Makes `repository`'s current revision a merge of `merged_in` into the state of `revision`.
async fn start_from_merge(repository: &Arc<RepositoryContext>, revision: Hash, merged_in: Hash) {
    let state = state_of(repository, revision).await;
    state.set_parent_other(merged_in);
    state.mark_dirty();
    let merge = state
        .serialize(repository.clone(), &token_of(repository))
        .await
        .expect("storing the merge");
    lore_revision::instance::store_current_anchor(repository, merge)
        .await
        .expect("moving the instance to the merge");
}

/// Drops the fragment at `hash` from `repository`'s store, so a read of it goes to the remote.
async fn drop_fragment(repository: &RepositoryContext, hash: Hash) {
    repository
        .immutable_store()
        .obliterate(
            repository.id,
            Address::zero_context_hash(hash),
            Arc::default(),
        )
        .await
        .expect("dropping the fragment");
}

/// A reset to where a file was last merged from a branch reads the revisions the file was in,
/// back to the branch point, and the merged-in parent of each merge on the way. A read of one the
/// instance does not hold goes to the remote, whose refusal is the command's failure, and the
/// working tree keeps the file as it was.
#[tokio::test]
async fn a_refused_read_of_a_revision_a_last_merged_reset_walks_fails_the_command() {
    Box::pin(with_execution(async {
        let mut differing = Vec::new();
        for gap in [
            LastMergedGap::EarlierState,
            LastMergedGap::EarlierTree,
            LastMergedGap::MergedInParent,
            LastMergedGap::MergedInParentMetadata,
            LastMergedGap::StateBeforeRemoval,
            LastMergedGap::TreeBeforeRemoval,
            LastMergedGap::CurrentTree,
        ] {
            let removed = matches!(
                gap,
                LastMergedGap::StateBeforeRemoval | LastMergedGap::TreeBeforeRemoval
            );
            let (repository, tempdir, commits) = last_merged_instance(removed).await;
            match gap {
                LastMergedGap::EarlierState => drop_fragment(&repository, commits[0]).await,
                LastMergedGap::EarlierTree => {
                    let tree = state_of(&repository, commits[0]).await.tree_hash();
                    drop_fragment(&repository, tree).await;
                }
                LastMergedGap::MergedInParent => {
                    start_from_merge(&repository, commits[1], Hash::from([7; 32])).await;
                }
                LastMergedGap::MergedInParentMetadata => {
                    let branch_point = state_of(&repository, commits[0]).await.parent_self();
                    let metadata = state_of(&repository, branch_point).await.metadata_hash();
                    start_from_merge(&repository, commits[1], branch_point).await;
                    drop_fragment(&repository, metadata).await;
                }
                LastMergedGap::StateBeforeRemoval => drop_fragment(&repository, commits[1]).await,
                LastMergedGap::TreeBeforeRemoval | LastMergedGap::CurrentTree => {
                    let tree = state_of(&repository, commits[1]).await.tree_hash();
                    drop_fragment(&repository, tree).await;
                }
            }
            let file = tempdir.path().join("file.txt");
            let before = std::fs::read(&file).ok();

            let reset = reset::reset_to_last_merged(
                reopened(&repository, &tempdir, Err(refusal())),
                path_arguments(tempdir.path(), &["file.txt"]),
                LoreString::from(branch::DEFAULT_DEFAULT_NAME),
                ResetMergeSide::default(),
                ResetOptions::default(),
            )
            .await;

            expect_refused(&mut differing, &format!("{gap:?}"), reset);
            if std::fs::read(&file).ok() != before {
                differing.push(format!("{gap:?}: the reset changed the file"));
            }
        }
        assert!(differing.is_empty(), "{differing:#?}");
    }))
    .await;
}
