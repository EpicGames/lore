// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroUsize;
use std::sync::Arc;

use lore_proto::lore::revision::v1::BranchListRequest;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::grpc::forwarded_requests::CallerContext;
use crate::grpc::revision::v1::branch_list::BranchListStream;
use crate::grpc::revision::v1::branch_list::branch_list_implementation;

/// Handler that takes a `BranchList` request forwarded on from peer's `RevisionService`
/// and executes it, streaming the response back to the other server for forwarding on
/// to its client.
#[tracing::instrument(name = "ForwardedRevision::v1::BranchList::Handler", skip_all)]
pub async fn handler(
    request: Request<BranchListRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
    concurrency: NonZeroUsize,
) -> Result<Response<BranchListStream>, Status> {
    let caller_context = CallerContext::from_forwarded_request(&request)?;
    let req = request.into_inner();

    branch_list_implementation(
        req,
        caller_context,
        immutable_store,
        mutable_store,
        concurrency,
    )
    .await
}
