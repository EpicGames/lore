// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The Action Cache: action digest -> `ActionResult`.
//!
//! This is the namespace that makes a warm build fast. Bazel computes an action's digest from
//! its command line, environment and the digests of every input, asks here, and on a hit skips
//! the action entirely -- no upload, no execution, just the output digests. Everything the
//! result points at lives in the CAS half of the same Lore store.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rbe_lore::LoreBlobStore;
use rbe_lore::Ns;
use rbe_lore::digest::key_of;
use rbe_proto::reapi::ActionResult;
use rbe_proto::reapi::Digest;
use rbe_proto::reapi::GetActionResultRequest;
use rbe_proto::reapi::UpdateActionResultRequest;
use rbe_proto::reapi::action_cache_server::ActionCache;
use tonic::Request;
use tonic::Response;
use tonic::Status;

pub struct ActionCacheService {
    store: Arc<LoreBlobStore>,
    verify: bool,
}

impl ActionCacheService {
    pub fn new(store: Arc<LoreBlobStore>, verify: bool) -> Self {
        Self { store, verify }
    }
}

/// Look up an action result, or `None` when there is no usable one.
///
/// Every failure is a miss. A read that errored, an entry that will not decode, and an entry
/// whose outputs have been evicted are all recoverable by executing the action, so none of them
/// may fail the build — a poisoned entry would otherwise pin that failure for every client until
/// the action's inputs changed. `Stats::ac_degraded` counts the cases that are not plain absence.
///
/// `verify` additionally checks that the blobs the result names are still in the CAS. Lore's GC
/// can evict one while the entry survives, and serving that entry fails the build with a missing
/// output instead of re-running the action. It costs one batched existence check per hit.
pub async fn lookup(
    store: &LoreBlobStore,
    action_digest: &Digest,
    verify: bool,
) -> Option<ActionResult> {
    let bytes = match store
        .get(Ns::Ac, &action_digest.hash, action_digest.size_bytes)
        .await
    {
        Ok(bytes) => bytes?,
        Err(error) => {
            store.stats.ac_degraded.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                action = %rbe_lore::digest::fmt(action_digest),
                %error,
                "action cache read failed; treating as a miss"
            );
            return None;
        }
    };

    let result = match <ActionResult as prost::Message>::decode(bytes.as_slice()) {
        Ok(result) => result,
        Err(error) => {
            store.stats.ac_degraded.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                action = %rbe_lore::digest::fmt(action_digest),
                %error,
                "cached ActionResult will not decode; treating as a miss"
            );
            return None;
        }
    };

    if verify && !outputs_present(store, &result).await {
        store.stats.ac_degraded.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            action = %rbe_lore::digest::fmt(action_digest),
            "action cache entry is incomplete; treating as a miss"
        );
        return None;
    }

    Some(result)
}

/// Whether every blob `result` names is still in the CAS.
async fn outputs_present(store: &LoreBlobStore, result: &ActionResult) -> bool {
    let referenced: Vec<Digest> = result
        .output_files
        .iter()
        .filter_map(|f| f.digest.clone())
        .chain(
            result
                .output_directories
                .iter()
                .filter_map(|d| d.tree_digest.clone()),
        )
        .chain(result.stdout_digest.clone())
        .chain(result.stderr_digest.clone())
        .filter(|d| !rbe_lore::digest::is_empty_digest(d))
        .collect();
    if referenced.is_empty() {
        return true;
    }

    let keys: Vec<_> = referenced.iter().map(key_of).collect();
    match store.exists_many(Ns::Cas, &keys).await {
        Ok(present) => present.iter().all(|present| *present),
        // exists_many already reports a failed probe as absent, so reaching here means the call
        // itself failed; the entry cannot be shown complete.
        Err(_) => false,
    }
}

pub async fn store_result(
    store: &LoreBlobStore,
    action_digest: &Digest,
    result: &ActionResult,
) -> Result<(), Status> {
    let bytes = prost::Message::encode_to_vec(result);
    store
        .put(
            Ns::Ac,
            &action_digest.hash,
            action_digest.size_bytes,
            &bytes,
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))
}

#[tonic::async_trait]
impl ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;

        match lookup(&self.store, &action_digest, self.verify).await {
            Some(mut result) => {
                // `inline_stdout`/`inline_stderr` ask for the streams in the response body
                // rather than as digests the client has to fetch separately.
                if req.inline_stdout
                    && let Some(d) = result.stdout_digest.clone()
                {
                    result.stdout_raw = crate::cas::read_blob(&self.store, &d).await?;
                }
                if req.inline_stderr
                    && let Some(d) = result.stderr_digest.clone()
                {
                    result.stderr_raw = crate::cas::read_blob(&self.store, &d).await?;
                }
                Ok(Response::new(result))
            }
            None => Err(Status::not_found("no cached result for this action")),
        }
    }

    async fn update_action_result(
        &self,
        request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;
        let result = req
            .action_result
            .ok_or_else(|| Status::invalid_argument("missing action_result"))?;

        store_result(&self.store, &action_digest, &result).await?;
        Ok(Response::new(result))
    }
}
