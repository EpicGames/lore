// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A remote revision service that answers revision lists with fixed pages.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::NotFound;
use lore_base::types::BranchId;
use lore_base::types::BranchPoint;
use lore_base::types::Hash;
use lore_revision::repository::RemoteState;
use lore_revision::repository::RepositoryContext;
use lore_transport::BranchListResponse;
use lore_transport::BranchPushResponse;
use lore_transport::BranchQueryResponse;
use lore_transport::Connection;
use lore_transport::MetadataSetResult;
use lore_transport::ProtocolError;
use lore_transport::Revision;
use lore_transport::RevisionItem;
use lore_transport::RevisionListResponse;
use lore_transport::RevisionListStart;

/// What a page answers: a revision number on the remote's branch, or a revision signature.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Anchor {
    Number(u64),
    Signature(Hash),
}

struct Page {
    anchor: Anchor,
    items: Vec<(u64, Hash)>,
    older: Hash,
}

/// A remote holding one branch at `latest`, which answers a revision list with the page
/// registered for its anchor and counts the revision lists it is asked for. An anchor with no
/// page is not found.
pub(crate) struct PagedRemote {
    branch: BranchId,
    latest: Hash,
    pages: Vec<Page>,
    lists: AtomicUsize,
}

impl PagedRemote {
    pub(crate) fn new(branch: BranchId, latest: Hash) -> Self {
        Self {
            branch,
            latest,
            pages: Vec::new(),
            lists: AtomicUsize::new(0),
        }
    }

    /// Answer `anchor` with `items`, newest first, and `older` as the cursor to the page
    /// below.
    pub(crate) fn page(mut self, anchor: Anchor, items: Vec<(u64, Hash)>, older: Hash) -> Self {
        self.pages.push(Page {
            anchor,
            items,
            older,
        });
        self
    }

    pub(crate) fn lists(&self) -> usize {
        self.lists.load(Ordering::Relaxed)
    }
}

/// A client context on in-memory stores whose remote is `remote`, and a context over the same
/// stores that can write the revisions the client holds locally.
pub(crate) async fn client_and_writer(
    remote: Arc<PagedRemote>,
) -> (Arc<RepositoryContext>, Arc<RepositoryContext>) {
    let (immutable, mutable) = lore_revision::repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable");
    let repository_id = lore_revision::lore::RepositoryId::default();
    let writer = Arc::new(RepositoryContext::new_null_context(
        immutable.clone(),
        mutable.clone(),
    ));
    let client = Arc::new(RepositoryContext::new_with_state(
        None,
        immutable,
        mutable,
        repository_id,
        lore_revision::instance::InstanceId::default(),
        RemoteState::Connected(Connection::detached_revision(repository_id, remote)),
        Arc::default(),
        None,
    ));
    (client, writer)
}

/// A local revision on a branch created from revision 3 of a line only the remote holds.
pub(crate) struct BranchedFromRemoteLine {
    /// The remote's line, revision 1 first.
    pub remote_line: Vec<Hash>,
    pub local: Hash,
    pub client: Arc<RepositoryContext>,
}

/// The page items for revisions `numbers` of `remote_line`, in the order given.
pub(crate) fn numbered(remote_line: &[Hash], numbers: &[u64]) -> Vec<(u64, Hash)> {
    numbers
        .iter()
        .map(|&number| (number, remote_line[number as usize - 1]))
        .collect()
}

/// [`BranchedFromRemoteLine`] over a remote line of six revisions, which the remote serves
/// from a cached segment holding all six when asked for revision 3.
pub(crate) async fn branched_from_cached_segment() -> BranchedFromRemoteLine {
    branched_from_remote_line(|remote, remote_line| {
        remote.page(
            Anchor::Signature(remote_line[2]),
            numbered(remote_line, &[6, 5, 4, 3, 2, 1]),
            Hash::default(),
        )
    })
    .await
}

/// [`BranchedFromRemoteLine`] over a remote line of six revisions, which the remote walks two
/// at a time from its latest revision. `parent_pages` adds what the remote answers when asked
/// for revision 3, the parent of the local revision.
pub(crate) async fn branched_from_remote_line(
    parent_pages: impl FnOnce(PagedRemote, &[Hash]) -> PagedRemote,
) -> BranchedFromRemoteLine {
    let remote_line: Vec<Hash> = (1..=6).map(|number| Hash::from([number; 32])).collect();
    let remote = PagedRemote::new(BranchId::from([7; 16]), remote_line[5])
        .page(
            Anchor::Signature(remote_line[5]),
            numbered(&remote_line, &[6, 5]),
            remote_line[3],
        )
        .page(
            Anchor::Signature(remote_line[4]),
            numbered(&remote_line, &[5, 4, 3, 2, 1]),
            Hash::default(),
        )
        .page(
            Anchor::Signature(remote_line[3]),
            numbered(&remote_line, &[4, 3, 2, 1]),
            Hash::default(),
        );
    let remote = Arc::new(parent_pages(remote, &remote_line));
    let (client, writer) = client_and_writer(remote).await;

    let state = lore_revision::state::State::new();
    state.set_parent_self(remote_line[2]);
    state.set_revision_number(4);
    let token = writer
        .try_write_token()
        .expect("a null context carries a write token");
    let local = state
        .serialize(writer.clone(), token)
        .await
        .expect("serializing the local revision");

    BranchedFromRemoteLine {
        remote_line,
        local,
        client,
    }
}

#[async_trait]
impl Revision for PagedRemote {
    async fn branch_create(
        &self,
        _: BranchId,
        _: &str,
        _: &str,
        _: &str,
        _: &[BranchPoint],
    ) -> Result<Hash, ProtocolError> {
        unreachable!("no test of a paged remote creates a branch")
    }

    async fn branch_delete(&self, _: BranchId) -> Result<(), ProtocolError> {
        unreachable!("no test of a paged remote deletes a branch")
    }

    async fn branch_query(
        &self,
        branch: Option<BranchId>,
        _: Option<&str>,
    ) -> Result<BranchQueryResponse, ProtocolError> {
        assert_eq!(branch, Some(self.branch), "the remote's branch is queried");
        Ok(BranchQueryResponse {
            id: self.branch,
            latest: self.latest,
            metadata: Hash::default(),
            deleted: false,
        })
    }

    async fn branch_push(
        &self,
        _: BranchId,
        _: Hash,
        _: bool,
        _: bool,
    ) -> Result<BranchPushResponse, ProtocolError> {
        unreachable!("no test of a paged remote pushes a branch")
    }

    async fn branch_list(&self) -> Result<BranchListResponse, ProtocolError> {
        unreachable!("no test of a paged remote lists branches")
    }

    async fn revision_list(
        &self,
        start: RevisionListStart,
    ) -> Result<RevisionListResponse, ProtocolError> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        let anchor = match start {
            RevisionListStart::Identifier(identifier) => {
                assert_eq!(
                    identifier.branch, self.branch,
                    "the remote's branch is listed"
                );
                Anchor::Number(identifier.number)
            }
            RevisionListStart::Signature(signature) => Anchor::Signature(signature),
        };
        let Some(page) = self.pages.iter().find(|page| page.anchor == anchor) else {
            return Err(ProtocolError::from(NotFound));
        };
        Ok(RevisionListResponse {
            items: page
                .items
                .iter()
                .map(|&(number, signature)| RevisionItem {
                    number,
                    signature,
                    metadata: Hash::default(),
                    state: Bytes::new(),
                })
                .collect(),
            next_revision: page.older,
            previous_revision: Hash::default(),
        })
    }

    async fn branch_metadata_get(&self, _: BranchId) -> Result<Hash, ProtocolError> {
        unreachable!("no test of a paged remote reads branch metadata")
    }

    async fn branch_metadata_set(
        &self,
        _: BranchId,
        _: Hash,
        _: Hash,
    ) -> Result<MetadataSetResult, ProtocolError> {
        unreachable!("no test of a paged remote writes branch metadata")
    }
}
