// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The scope a branch create gives the new branch, through the `lore.revision.v1` and `urc.rpc`
//! `BranchCreate` handlers: the scope the request names, or the one it inherits from its stack,
//! recorded in the branch's metadata where it isn't the default scope, and carried by every
//! `lore.revision.v1` branch record.
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_proto::lore::model::v1 as model_v1;
use lore_proto::lore::revision::v1 as revision_v1;
use lore_revision::branch;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::scope;
use lore_revision::scope::ScopeId;
use lore_revision::state::allow_all_repositories;
use lore_server::grpc::handlers;
use lore_server::grpc::revision;
use lore_server::hooks::HookDispatcher;
use lore_telemetry::InstrumentProvider;
use lore_transport::grpc::REPOSITORY_ID_KEY;
use rand::random;
use tokio_stream::StreamExt;
use tonic::Code;
use tonic::Request;
use tonic::Status;

use crate::notification::testing::MockNotificationSender;
use crate::store::test_support::test_store_create;

#[derive(Clone)]
struct TestInstrumentProvider;

impl InstrumentProvider for TestInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "test"
    }
}

/// The `BranchCreate` surface a create goes through.
#[derive(Clone, Copy, Debug)]
enum Surface {
    V1,
    Legacy,
}

const SURFACES: [Surface; 2] = [Surface::V1, Surface::Legacy];

/// The stores of a test and a repository in them.
struct Fixture {
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    repository: RepositoryId,
}

impl Fixture {
    fn context(&self) -> Arc<RepositoryContext> {
        Arc::new(RepositoryContext::new_server_context(
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            self.repository,
            allow_all_repositories(),
        ))
    }

    /// `message` as a request naming the fixture's repository.
    fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(self.repository.data()),
        );
        request
    }

    /// Creates the scope `scope_id(byte)` named `name`.
    async fn scope(&self, byte: u8, name: &str) -> ScopeId {
        let id = scope_id(byte);
        scope::create(self.context(), id, name, "creator", 1)
            .await
            .expect("the scope is created")
            .expect("the scope is new");
        id
    }

    /// Creates `branch` named `name` with `stack` through the `lore.revision.v1` handler, naming
    /// `scope`, and answers the record the response carries.
    async fn create_v1(
        &self,
        branch: BranchId,
        name: &str,
        stack: &[BranchPoint],
        scope: Option<&[u8]>,
    ) -> Result<model_v1::Branch, Status> {
        let request = self.request(revision_v1::BranchCreateRequest {
            id: branch.into(),
            name: name.into(),
            creator: Some("alice".into()),
            category: String::new(),
            stack: stack.iter().map(model_v1::BranchPoint::from).collect(),
            scope: scope.map(bytes::Bytes::copy_from_slice),
        });
        revision::v1::branch_create::handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            notifications(),
            &None,
            &Arc::new(HookDispatcher::empty()),
            &TestInstrumentProvider,
        )
        .await
        .map(|response| {
            response
                .into_inner()
                .branch
                .expect("the response carries the branch")
        })
    }

    /// The record of `branch` the `lore.revision.v1` `BranchGet` handler answers.
    async fn get_v1(&self, branch: BranchId) -> model_v1::Branch {
        let request = self.request(revision_v1::BranchGetRequest {
            query: Some(revision_v1::branch_get_request::Query::Id(branch.into())),
        });
        revision::v1::branch_get::handler(
            request,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            &None,
        )
        .await
        .expect("the get succeeds")
        .into_inner()
        .branch
        .expect("the response carries the branch")
    }

    /// The records the `lore.revision.v1` `BranchList` handler answers, archived branches included.
    async fn list_v1(&self) -> Vec<model_v1::Branch> {
        let request = self.request(revision_v1::BranchListRequest {
            creator: None,
            include_deleted: true,
        });
        revision::v1::branch_list::handler(
            request,
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            &None,
            revision::v1::branch_list::DEFAULT_BRANCH_LIST_CONCURRENCY,
            &crate::grpc::revision::v1::branch_list::unshared_reads(),
        )
        .await
        .expect("the list succeeds")
        .into_inner()
        .map(|item| {
            item.expect("the list streams the branch")
                .branch
                .expect("the response carries the branch")
        })
        .collect()
        .await
    }

    /// Archives `branch` through the `lore.revision.v1` `BranchDelete` handler, and answers the
    /// record the response carries.
    async fn delete_v1(&self, branch: BranchId) -> model_v1::Branch {
        let request = self.request(revision_v1::BranchDeleteRequest { id: branch.into() });
        revision::v1::branch_delete::handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            notifications(),
            &None,
            &Arc::new(HookDispatcher::empty()),
            &TestInstrumentProvider,
        )
        .await
        .expect("the delete succeeds")
        .into_inner()
        .branch
        .expect("the response carries the branch")
    }

    /// Creates `branch` named `name` with `stack` through the `urc.rpc` handler.
    async fn create_legacy(
        &self,
        branch: BranchId,
        name: &str,
        stack: &[BranchPoint],
    ) -> Result<(), Status> {
        let request = self.request(lore_proto::BranchCreateRequest {
            branch: branch.into(),
            name: name.into(),
            creator: "alice".into(),
            created: 1,
            category: String::new(),
            stack: stack.iter().map(lore_proto::BranchPoint::from).collect(),
            revision_deprecated: None,
            parent_deprecated: None,
        });
        handlers::branch_create::handler(
            request,
            &crate::util::test_support::allow_all_authorizer(),
            self.immutable_store.clone(),
            self.mutable_store.clone(),
            notifications(),
            &Arc::new(HookDispatcher::empty()),
            &TestInstrumentProvider,
        )
        .await
        .map(|_| ())
    }

    /// Creates `branch` named `name` with `stack` through `surface`, naming no scope.
    async fn create(
        &self,
        surface: Surface,
        branch: BranchId,
        name: &str,
        stack: &[BranchPoint],
    ) -> Result<(), Status> {
        match surface {
            Surface::V1 => self.create_v1(branch, name, stack, None).await.map(|_| ()),
            Surface::Legacy => self.create_legacy(branch, name, stack).await,
        }
    }

    /// The metadata of `branch`, `None` where it has none.
    async fn metadata(&self, branch: BranchId) -> Option<lore_revision::metadata::Metadata> {
        branch::metadata(self.context(), branch).await.ok()
    }

    /// The value the metadata of `branch` holds under `scope`, `None` where it holds none.
    async fn stored_scope(&self, branch: BranchId) -> Option<Vec<u8>> {
        let metadata = self
            .metadata(branch)
            .await
            .expect("the branch has metadata");
        metadata.get_binary(branch::SCOPE).ok().map(<[u8]>::to_vec)
    }

    /// The scope of `branch`, as `branch::scope_of` reads it.
    async fn scope_of(&self, branch: BranchId) -> ScopeId {
        branch::scope_of(self.context(), branch)
            .await
            .expect("the scope reads")
            .expect("the branch has metadata")
    }

    /// Archives `branch`.
    async fn archive(&self, branch: BranchId) {
        branch::delete(self.context(), branch)
            .await
            .expect("the branch is archived");
    }

    /// Archives scope `id`.
    async fn archive_scope(&self, id: ScopeId) {
        scope::archive(self.context(), id)
            .await
            .expect("the archive succeeds")
            .expect("the scope is archived");
    }

    /// Creates a root branch named `main`, which an archived branch can stack on.
    async fn main(&self) -> BranchId {
        let main = new_branch();
        self.create_v1(main, "main", &[], None)
            .await
            .expect("main is created");
        main
    }

    /// Whether `name` maps to a branch.
    async fn name_is_claimed(&self, name: &str) -> bool {
        branch::load_name_to_id(self.context(), name).await.is_ok()
    }
}

/// A notification sender that takes any number of creates and deletes.
fn notifications() -> Arc<MockNotificationSender> {
    let mut sender = MockNotificationSender::new();
    sender.expect_branch_created().returning(|_, _| ());
    sender.expect_branch_deleted().returning(|_, _| ());
    Arc::new(sender)
}

fn scope_id(byte: u8) -> ScopeId {
    ScopeId::from([byte; 16])
}

fn new_branch() -> BranchId {
    BranchId::from(uuid::Uuid::now_v7())
}

/// A stack entry of `branch` at a revision the create doesn't check.
fn point(branch: BranchId) -> BranchPoint {
    BranchPoint {
        branch,
        revision: Hash::from([7u8; 32]),
    }
}

async fn with_fixture<F, Fut>(test: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let (immutable_store, mutable_store, execution) =
        test_store_create().await.expect("Failed to create stores");
    let fixture = Fixture {
        immutable_store,
        mutable_store,
        repository: random(),
    };
    Box::pin(LORE_CONTEXT.scope(execution, test(fixture))).await;
}

fn code<T: std::fmt::Debug>(result: Result<T, Status>) -> Code {
    result.expect_err("the create is refused").code()
}

/// A root branch, through either surface, is in the default scope and stores no `scope`.
#[tokio::test]
async fn a_root_branch_is_in_the_default_scope() {
    with_fixture(|fixture| async move {
        for surface in SURFACES {
            let root = new_branch();
            fixture
                .create(surface, root, &format!("root-{surface:?}"), &[])
                .await
                .expect("the root is created");
            assert_eq!(fixture.stored_scope(root).await, None, "{surface:?}");
            assert_eq!(
                fixture.scope_of(root).await,
                scope::default_scope_id(fixture.repository),
                "{surface:?}"
            );
        }
    })
    .await;
}

/// A named live scope is the new branch's, stored in its metadata.
#[tokio::test]
async fn a_named_live_scope_is_assigned() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let branch = new_branch();
        fixture
            .create_v1(branch, "feature", &[], Some(named.data().as_slice()))
            .await
            .expect("the branch is created");
        assert_eq!(
            fixture.stored_scope(branch).await.as_deref(),
            Some(named.data().as_slice())
        );
    })
    .await;
}

/// Every `lore.revision.v1` response that carries a branch record carries its scope: the create,
/// get, list and delete records of a branch in a named scope carry the scope's id, and those of a
/// branch in the default scope carry none.
#[tokio::test]
async fn every_v1_branch_record_carries_its_scope() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let stack = [point(fixture.main().await)];
        let in_named = new_branch();
        let in_default = new_branch();
        let created_named = fixture
            .create_v1(
                in_named,
                "release-line",
                &stack,
                Some(named.data().as_slice()),
            )
            .await
            .expect("the branch is created");
        let created_default = fixture
            .create_v1(in_default, "feature", &stack, None)
            .await
            .expect("the branch is created");
        let listed = fixture.list_v1().await;
        for (branch, created, scope) in [
            (in_named, created_named, Some(named.data().as_slice())),
            (in_default, created_default, None),
        ] {
            let listed = listed
                .iter()
                .find(|record| record.id.as_ref() == branch.data().as_slice())
                .expect("the list carries the branch")
                .clone();
            let got = fixture.get_v1(branch).await;
            let deleted = fixture.delete_v1(branch).await;
            for (response, record) in [
                ("create", created),
                ("get", got),
                ("list", listed),
                ("delete", deleted),
            ] {
                assert_eq!(record.scope.as_deref(), scope, "{response} of {branch}");
            }
        }
    })
    .await;
}

/// The default scope's id names a live scope: the branch is in the default scope and stores no
/// `scope`.
#[tokio::test]
async fn the_default_scope_named_by_id_stores_no_key() {
    with_fixture(|fixture| async move {
        let default = scope::default_scope_id(fixture.repository);
        let branch = new_branch();
        fixture
            .create_v1(branch, "feature", &[], Some(default.data().as_slice()))
            .await
            .expect("the branch is created");
        assert_eq!(fixture.stored_scope(branch).await, None);
    })
    .await;
}

/// An unknown or archived named scope is refused with `FailedPrecondition`, and a value that isn't
/// 16 non-zero bytes with `InvalidArgument`, before anything is written.
#[tokio::test]
async fn a_named_scope_that_is_not_live_or_not_an_id_is_refused_with_nothing_written() {
    with_fixture(|fixture| async move {
        let archived = fixture.scope(2, "retired").await;
        fixture.archive_scope(archived).await;
        let unknown = scope_id(3);
        let refusals: [(&[u8], Code); 4] = [
            (unknown.data(), Code::FailedPrecondition),
            (archived.data(), Code::FailedPrecondition),
            (&[0u8; 16], Code::InvalidArgument),
            (&[1u8; 15], Code::InvalidArgument),
        ];
        for (named, expected) in refusals {
            let branch = new_branch();
            assert_eq!(
                code(fixture.create_v1(branch, "feature", &[], Some(named)).await),
                expected,
                "{named:?}"
            );
            assert!(fixture.metadata(branch).await.is_none(), "{named:?}");
            assert!(
                !fixture.name_is_claimed("feature").await,
                "{named:?}: the name stays free"
            );
        }
    })
    .await;
}

/// A branch off a parent in another scope inherits it through either surface, as a branch created
/// offline does on its first push, whose create carries its stack and names no scope.
#[tokio::test]
async fn a_branch_inherits_its_parents_scope_through_both_surfaces() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let parent = new_branch();
        fixture
            .create_v1(parent, "release-line", &[], Some(named.data().as_slice()))
            .await
            .expect("the parent is created");
        for surface in SURFACES {
            let child = new_branch();
            fixture
                .create(
                    surface,
                    child,
                    &format!("child-{surface:?}"),
                    &[point(parent)],
                )
                .await
                .expect("the child is created");
            assert_eq!(
                fixture.stored_scope(child).await.as_deref(),
                Some(named.data().as_slice()),
                "{surface:?}"
            );
        }
    })
    .await;
}

/// A branch off a parent in an archived scope inherits it.
#[tokio::test]
async fn an_archived_scope_is_inherited() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let parent = new_branch();
        fixture
            .create_v1(parent, "release-line", &[], Some(named.data().as_slice()))
            .await
            .expect("the parent is created");
        fixture.archive_scope(named).await;
        let child = new_branch();
        fixture
            .create_v1(child, "child", &[point(parent)], None)
            .await
            .expect("the child is created");
        assert_eq!(
            fixture.stored_scope(child).await.as_deref(),
            Some(named.data().as_slice())
        );
    })
    .await;
}

/// A branch pushed before its parent inherits from the first branch of its stack the server
/// holds, and the parent, pushed after it, takes the same scope.
#[tokio::test]
async fn a_branch_pushed_before_its_parent_inherits_from_the_nearest_known_ancestor() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let ancestor = new_branch();
        fixture
            .create_v1(ancestor, "release-line", &[], Some(named.data().as_slice()))
            .await
            .expect("the ancestor is created");
        let parent = new_branch();
        let child = new_branch();
        fixture
            .create_v1(child, "child", &[point(parent), point(ancestor)], None)
            .await
            .expect("the child is created");
        fixture
            .create_v1(parent, "parent", &[point(ancestor)], None)
            .await
            .expect("the parent is created");
        for branch in [child, parent] {
            assert_eq!(
                fixture.stored_scope(branch).await.as_deref(),
                Some(named.data().as_slice())
            );
        }
    })
    .await;
}

/// A branch none of whose stack the server holds is in the default scope and stores no `scope`.
#[tokio::test]
async fn a_branch_with_no_known_ancestor_is_in_the_default_scope() {
    with_fixture(|fixture| async move {
        let branch = new_branch();
        fixture
            .create_v1(
                branch,
                "orphan",
                &[point(new_branch()), point(new_branch())],
                None,
            )
            .await
            .expect("the branch is created");
        assert_eq!(fixture.stored_scope(branch).await, None);
    })
    .await;
}

/// A default-scope branch off a default-scope parent stores no `scope`.
#[tokio::test]
async fn a_default_scope_branch_stores_no_key() {
    with_fixture(|fixture| async move {
        let parent = new_branch();
        fixture
            .create_v1(parent, "main", &[], None)
            .await
            .expect("the parent is created");
        for surface in SURFACES {
            let child = new_branch();
            fixture
                .create(
                    surface,
                    child,
                    &format!("child-{surface:?}"),
                    &[point(parent)],
                )
                .await
                .expect("the child is created");
            assert_eq!(fixture.stored_scope(child).await, None, "{surface:?}");
        }
    })
    .await;
}

/// A create that restores an archived branch keeps the scope the branch holds: naming none or the
/// same restores it, the same even once that scope is archived, and naming another is refused with
/// `FailedPrecondition` and leaves it archived.
#[tokio::test]
async fn a_restore_keeps_the_branch_scope_and_refuses_another() {
    with_fixture(|fixture| async move {
        let held = fixture.scope(1, "release").await;
        let other = fixture.scope(2, "sandbox").await;
        let stack = [point(fixture.main().await)];
        let branch = new_branch();
        fixture
            .create_v1(branch, "feature", &stack, Some(held.data().as_slice()))
            .await
            .expect("the branch is created");
        fixture.archive(branch).await;

        assert_eq!(
            code(
                fixture
                    .create_v1(branch, "feature", &stack, Some(other.data().as_slice()))
                    .await
            ),
            Code::FailedPrecondition
        );
        assert!(
            !fixture.name_is_claimed("feature").await,
            "the branch stays archived"
        );

        fixture
            .create_v1(branch, "feature", &stack, Some(held.data().as_slice()))
            .await
            .expect("the branch is restored");
        assert_eq!(fixture.scope_of(branch).await, held);
        fixture.archive(branch).await;

        for surface in SURFACES {
            fixture
                .create(surface, branch, "feature", &stack)
                .await
                .expect("the branch is restored");
            assert_eq!(fixture.scope_of(branch).await, held, "{surface:?}");
            fixture.archive(branch).await;
        }

        fixture.archive_scope(held).await;
        fixture
            .create_v1(branch, "feature", &stack, Some(held.data().as_slice()))
            .await
            .expect("the branch is restored in its archived scope");
        assert_eq!(fixture.scope_of(branch).await, held);
    })
    .await;
}

/// A create of the id of a live branch that names another scope is refused with
/// `FailedPrecondition`, and one that names the branch's scope with `AlreadyExists`.
#[tokio::test]
async fn a_create_of_a_live_branch_naming_another_scope_is_refused() {
    with_fixture(|fixture| async move {
        let held = fixture.scope(1, "release").await;
        let other = fixture.scope(2, "sandbox").await;
        let branch = new_branch();
        fixture
            .create_v1(branch, "feature", &[], Some(held.data().as_slice()))
            .await
            .expect("the branch is created");
        let named = [
            (other, Code::FailedPrecondition),
            (held, Code::AlreadyExists),
        ];
        for (scope, expected) in named {
            assert_eq!(
                code(
                    fixture
                        .create_v1(branch, "feature", &[], Some(scope.data().as_slice()))
                        .await
                ),
                expected,
                "{scope}"
            );
            assert_eq!(fixture.scope_of(branch).await, held, "{scope}");
        }
    })
    .await;
}

/// A default-scope branch, live or archived, refuses a create that names another scope with
/// `FailedPrecondition` and stays in the default scope.
#[tokio::test]
async fn a_default_scope_branch_refuses_another_scope() {
    with_fixture(|fixture| async move {
        let other = fixture.scope(1, "release").await;
        let default = scope::default_scope_id(fixture.repository);
        let stack = [point(fixture.main().await)];
        let branch = new_branch();
        fixture
            .create_v1(branch, "feature", &stack, None)
            .await
            .expect("the branch is created");
        for archived in [false, true] {
            if archived {
                fixture.archive(branch).await;
            }
            assert_eq!(
                code(
                    fixture
                        .create_v1(branch, "feature", &stack, Some(other.data().as_slice()))
                        .await
                ),
                Code::FailedPrecondition,
                "archived: {archived}"
            );
            assert_eq!(
                fixture.scope_of(branch).await,
                default,
                "archived: {archived}"
            );
        }
        assert!(
            !fixture.name_is_claimed("feature").await,
            "the branch stays archived"
        );
    })
    .await;
}

/// A named scope overrides the scope of the parent: a branch off a parent in one scope that names
/// another is in the named one, the default scope included.
#[tokio::test]
async fn a_named_scope_overrides_the_parents_scope() {
    with_fixture(|fixture| async move {
        let parents = fixture.scope(1, "release").await;
        let named = fixture.scope(2, "sandbox").await;
        let default = scope::default_scope_id(fixture.repository);
        let parent = new_branch();
        fixture
            .create_v1(parent, "release-line", &[], Some(parents.data().as_slice()))
            .await
            .expect("the parent is created");
        for (name, scope) in [("child-named", named), ("child-default", default)] {
            let child = new_branch();
            fixture
                .create_v1(child, name, &[point(parent)], Some(scope.data().as_slice()))
                .await
                .expect("the child is created");
            assert_eq!(fixture.scope_of(child).await, scope, "{name}");
        }
    })
    .await;
}

/// A branch off an archived parent inherits the parent's scope.
#[tokio::test]
async fn an_archived_parent_passes_its_scope_on() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let parent = new_branch();
        fixture
            .create_v1(
                parent,
                "release-line",
                &[point(fixture.main().await)],
                Some(named.data().as_slice()),
            )
            .await
            .expect("the parent is created");
        fixture.archive(parent).await;
        let child = new_branch();
        fixture
            .create_v1(child, "child", &[point(parent)], None)
            .await
            .expect("the child is created");
        assert_eq!(fixture.scope_of(child).await, named);
    })
    .await;
}

/// A parent whose scope can't be read fails the create through either surface, which writes
/// nothing, rather than moving on to the next branch of the stack.
#[tokio::test]
async fn a_parent_whose_scope_fails_to_read_fails_the_create() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let ancestor = new_branch();
        fixture
            .create_v1(ancestor, "release-line", &[], Some(named.data().as_slice()))
            .await
            .expect("the ancestor is created");
        let parent = new_branch();
        fixture
            .create_v1(parent, "parent", &[], None)
            .await
            .expect("the parent is created");
        let mut metadata = fixture
            .metadata(parent)
            .await
            .expect("the parent has metadata");
        metadata
            .set_binary(branch::SCOPE, &[1u8; 15])
            .expect("set scope");
        let hash = metadata
            .serialize(fixture.context())
            .await
            .expect("serialize");
        branch::mutable_store_metadata(fixture.context(), parent, hash)
            .await
            .expect("store the metadata");

        for surface in SURFACES {
            let child = new_branch();
            let name = format!("child-{surface:?}");
            assert_eq!(
                code(
                    fixture
                        .create(surface, child, &name, &[point(parent), point(ancestor)])
                        .await
                ),
                Code::Internal,
                "{surface:?}"
            );
            assert!(fixture.metadata(child).await.is_none(), "{surface:?}");
            assert!(!fixture.name_is_claimed(&name).await, "{surface:?}");
        }
    })
    .await;
}

/// The walk reads the first `INHERITED_SCOPE_PROBES` branches of a stack: a known ancestor last
/// among them passes its scope on, and one past them leaves the branch in the default scope.
#[tokio::test]
async fn the_walk_reads_a_bounded_number_of_stack_entries() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let ancestor = new_branch();
        fixture
            .create_v1(ancestor, "release-line", &[], Some(named.data().as_slice()))
            .await
            .expect("the ancestor is created");
        for (unknown, inherited) in [
            (branch::INHERITED_SCOPE_PROBES - 1, Some(named)),
            (branch::INHERITED_SCOPE_PROBES, None),
        ] {
            let stack: Vec<BranchPoint> = (0..unknown)
                .map(|_| point(new_branch()))
                .chain([point(ancestor)])
                .collect();
            let child = new_branch();
            fixture
                .create_v1(child, &format!("child-{unknown}"), &stack, None)
                .await
                .expect("the child is created");
            assert_eq!(
                fixture.stored_scope(child).await,
                inherited.map(|scope| scope.data().to_vec()),
                "{unknown} unknown entries"
            );
        }
    })
    .await;
}

/// The walk stops at the first branch of the stack the server holds, a default-scope parent
/// included, rather than moving on to an ancestor in another scope.
#[tokio::test]
async fn the_walk_stops_at_a_default_scope_parent() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let ancestor = new_branch();
        fixture
            .create_v1(ancestor, "release-line", &[], Some(named.data().as_slice()))
            .await
            .expect("the ancestor is created");
        let parent = new_branch();
        fixture
            .create_v1(parent, "parent", &[], None)
            .await
            .expect("the parent is created");
        for surface in SURFACES {
            let child = new_branch();
            fixture
                .create(
                    surface,
                    child,
                    &format!("child-{surface:?}"),
                    &[point(parent), point(ancestor)],
                )
                .await
                .expect("the child is created");
            assert_eq!(fixture.stored_scope(child).await, None, "{surface:?}");
        }
    })
    .await;
}

/// A create naming a scope for a branch whose own scope can't be read fails as internal, and
/// leaves the branch's metadata and its name as they were.
#[tokio::test]
async fn a_named_create_of_a_branch_whose_scope_fails_to_read_fails() {
    with_fixture(|fixture| async move {
        let named = fixture.scope(1, "release").await;
        let stack = [point(fixture.main().await)];
        let branch = new_branch();
        fixture
            .create_v1(branch, "feature", &stack, None)
            .await
            .expect("the branch is created");
        fixture.archive(branch).await;
        let mut metadata = fixture
            .metadata(branch)
            .await
            .expect("the branch has metadata");
        metadata
            .set_binary(branch::SCOPE, &[1u8; 15])
            .expect("set scope");
        let hash = metadata
            .serialize(fixture.context())
            .await
            .expect("serialize");
        branch::mutable_store_metadata(fixture.context(), branch, hash)
            .await
            .expect("store the metadata");

        assert_eq!(
            code(
                fixture
                    .create_v1(branch, "feature", &stack, Some(named.data().as_slice()))
                    .await
            ),
            Code::Internal
        );
        assert_eq!(
            branch::metadata_hash(fixture.context(), branch).await.ok(),
            Some(hash)
        );
        assert!(!fixture.name_is_claimed("feature").await);
    })
    .await;
}

/// A create with a name that isn't valid, or with a zero branch id, is refused with
/// `InvalidArgument` before its scope is resolved, whatever scope it names.
#[tokio::test]
async fn invalid_create_arguments_are_refused_before_the_scope() {
    with_fixture(|fixture| async move {
        let archived = fixture.scope(1, "retired").await;
        fixture.archive_scope(archived).await;
        for (branch, name) in [(new_branch(), ""), (BranchId::default(), "feature")] {
            assert_eq!(
                code(
                    fixture
                        .create_v1(branch, name, &[], Some(archived.data().as_slice()))
                        .await
                ),
                Code::InvalidArgument,
                "{branch} {name:?}"
            );
        }
    })
    .await;
}
