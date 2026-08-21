// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::pin::Pin;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_proto::lore::thin_client::v1::ContentDiffChunkResponse;
use lore_proto::lore::thin_client::v1::ContentDiffHeader;
use lore_proto::lore::thin_client::v1::ContentDiffRequest;
use lore_proto::lore::thin_client::v1::ContentDiffResponse;
use lore_proto::lore::thin_client::v1::content_diff_response::Payload;
use lore_revision::file::diff::DEFAULT_CONTEXT_LINES;
use lore_revision::file::diff::DiffOptions;
use lore_revision::file::diff::build_unified_patch;
use lore_revision::infer::infer_is_diffable_by_slice;
use lore_revision::util::encoding::decode_text_for_display;
use lore_storage::ReadOptions;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::util::setup_execution;

type ContentDiffStream =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<ContentDiffResponse, Status>> + Send + 'static>>;

/// Characters of diff text streamed per `ContentDiffResponse`.
const CHUNK_SIZE: usize = 64 * 1024;

/// Per-side input cap: content larger than this is not reassembled for
/// diffing; the header reports `truncated = true` instead.
const MAX_INPUT_BYTES: u64 = 16 * 1024 * 1024;

/// Bytes read for text/binary sniffing when the content exceeds
/// [`MAX_INPUT_BYTES`].
const SNIFF_BYTES: usize = 8192;

/// `lore.thin_client.v1.ThinClientService.ContentDiff` handler.
///
/// Two-way unified diff between two CAS addresses (either side may be
/// empty bytes = "no content", for adds/deletes). Content is reassembled
/// and decompressed server-side and diffed with the same pipeline the
/// client diff path uses (`lore_revision::file::diff::build_unified_patch`),
/// so output is byte-identical to a client-side diff. The first stream
/// message carries the header (stats / binary / truncated flags); text
/// chunks follow unless the header short-circuits.
///
/// Three-way mode (`address_base` set) is not implemented yet and is
/// rejected with `Unimplemented`.
#[tracing::instrument(name = "ContentDiff::v1::handle", skip_all)]
pub async fn handler(
    request: Request<ContentDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
) -> Result<Response<ContentDiffStream>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    if req.address_base.as_ref().is_some_and(|b| !b.is_empty()) {
        return Err(Status::unimplemented(
            "lore.thin_client.v1.ThinClientService.ContentDiff 3-way mode not yet implemented",
        ));
    }
    if req.address_from.is_empty() && req.address_to.is_empty() {
        return Err(Status::invalid_argument(
            "at least one of address_from / address_to must be set",
        ));
    }

    let options = DiffOptions {
        context_lines: req.context_lines.unwrap_or(DEFAULT_CONTEXT_LINES),
        ignore_whitespace_eol: req.ignore_whitespace_eol,
        ignore_whitespace_inline: req.ignore_whitespace_inline,
    };
    let max_diff_size = req.max_diff_size;

    let execution = setup_execution(module_path!(), correlation_id, user_id);

    LORE_CONTEXT
        .scope(execution, async move {
            // Read both sides up front so failures surface as a unary Status
            // before the stream opens.
            let from = read_side(&immutable_store, repository_id, &req.address_from).await?;
            let to = read_side(&immutable_store, repository_id, &req.address_to).await?;

            let header;
            let mut text = String::new();
            {
                // Sniff on whatever bytes each side has: full content for
                // in-cap reads, a leading prefix for oversized ones. Empty
                // sides (add/delete) are trivially diffable; the binary
                // sniffer treats an empty slice as non-diffable. Prefix
                // sniffing is a heuristic (a binary file with a clean-text
                // first 8 KiB reads as text), matching how the mime probes
                // operate on leading bytes.
                let binary = |bytes: &[u8]| !bytes.is_empty() && !infer_is_diffable_by_slice(bytes);
                let any_oversized =
                    matches!(from, Side::Oversized { .. }) || matches!(to, Side::Oversized { .. });
                if binary(from.sniff_bytes()) || binary(to.sniff_bytes()) {
                    // Binary content: stats and text are not meaningful;
                    // the header's `binary` flag is the whole answer.
                    header = ContentDiffHeader {
                        binary: true,
                        ..Default::default()
                    };
                } else if any_oversized {
                    // Text (as far as the prefix shows) but too large to
                    // reassemble and diff.
                    header = ContentDiffHeader {
                        truncated: true,
                        ..Default::default()
                    };
                } else {
                    let old = decode_text_for_display(from.sniff_bytes());
                    let new = decode_text_for_display(to.sniff_bytes());
                    let patch =
                        build_unified_patch(&old, &new, "from", "to", options).unwrap_or_default();
                    let (added, deleted) = count_changes(&patch);
                    let truncated = max_diff_size.is_some_and(|max| patch.len() as u64 > max);
                    header = ContentDiffHeader {
                        lines_added: added,
                        lines_deleted: deleted,
                        truncated,
                        ..Default::default()
                    };
                    if !truncated {
                        text = patch;
                    }
                }
            }

            let (tx, rx) = mpsc::channel(4);
            tokio::spawn(async move {
                if tx
                    .send(Ok(ContentDiffResponse {
                        payload: Some(Payload::Header(header)),
                    }))
                    .await
                    .is_err()
                {
                    return; // receiver dropped
                }
                let mut offset = 0;
                while offset < text.len() {
                    // Chunk on a char boundary so every chunk is valid UTF-8.
                    let mut end = (offset + CHUNK_SIZE).min(text.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    let response = ContentDiffResponse {
                        payload: Some(Payload::Chunk(ContentDiffChunkResponse {
                            diff: text[offset..end].to_string(),
                        })),
                    };
                    if tx.send(Ok(response)).await.is_err() {
                        return;
                    }
                    offset = end;
                }
            });

            Ok(Response::new(
                Box::pin(ReceiverStream::new(rx)) as ContentDiffStream
            ))
        })
        .await
}

enum Side {
    Empty,
    Content(bytes::Bytes),
    /// Content larger than [`MAX_INPUT_BYTES`]; carries only a leading
    /// prefix for sniffing.
    Oversized {
        prefix: bytes::Bytes,
    },
}

impl Side {
    /// The bytes available for text/binary sniffing (and, for in-cap
    /// content, the full body).
    fn sniff_bytes(&self) -> &[u8] {
        match self {
            Side::Content(bytes) => bytes,
            Side::Oversized { prefix } => prefix,
            Side::Empty => &[],
        }
    }
}

/// Read one side's content by its address bytes. Empty bytes = no content.
///
/// `DiffChange.content_from` / `content_to` carry the CAS hash without the
/// context half, so the address is built hash-only and resolution relies on
/// the store's `MatchHash` fallback. The server enables `LOCAL_ISOLATION`
/// globally, which restricts default reads to full `(hash, context)`
/// matches; `no_isolation()` re-enables the fallback for these reads. This
/// does not widen access: the lookup is still scoped to the caller's
/// authorized partition, and the content hash itself proves the bytes.
/// (An alternative would be carrying full `Address`es in `DiffChange` —
/// a wire change, deliberately not made here.)
async fn read_side(
    immutable_store: &Arc<dyn lore_storage::ImmutableStore>,
    repository_id: lore_base::types::Partition,
    hash_bytes: &[u8],
) -> Result<Side, Status> {
    if hash_bytes.is_empty() {
        return Ok(Side::Empty);
    }
    let address = Address {
        hash: Hash::from(hash_bytes),
        context: Default::default(),
    };
    if address.hash.is_zero() {
        return Ok(Side::Empty);
    }
    let options = ReadOptions::default()
        .no_isolation()
        .with_max_content_size(MAX_INPUT_BYTES);
    match lore_storage::read(
        immutable_store.clone(),
        repository_id,
        address,
        None,
        options,
        None, /* server has the data locally; no remote session */
    )
    .await
    {
        Ok(bytes) => Ok(Side::Content(bytes)),
        Err(lore_storage::StorageError::Oversized(_)) => {
            // Too large to reassemble in full — fetch just a leading
            // prefix (ranged read, no size cap) so text/binary sniffing
            // still works.
            let prefix = lore_storage::read(
                immutable_store.clone(),
                repository_id,
                address,
                Some(0..SNIFF_BYTES),
                ReadOptions::default().no_isolation(),
                None,
            )
            .await
            .unwrap_or_default();
            Ok(Side::Oversized { prefix })
        }
        Err(lore_storage::StorageError::AddressNotFound(_)) => {
            Err(Status::not_found("content not found"))
        }
        Err(other) => Err(Status::internal(format!("read failed: {other}"))),
    }
}

/// Count added/deleted lines in a unified patch, skipping the `---`/`+++`
/// file headers.
fn count_changes(patch: &str) -> (u64, u64) {
    let mut added = 0;
    let mut deleted = 0;
    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            deleted += 1;
        }
    }
    (added, deleted)
}

#[cfg(test)]
mod tests {
    use lore_base::types::Context;
    use lore_storage::WriteOptions;
    use tokio_stream::StreamExt;

    use super::*;
    use crate::store::test_store_create;

    async fn write_blob(
        store: &Arc<dyn lore_storage::ImmutableStore>,
        partition: lore_base::types::Partition,
        content: &[u8],
    ) -> bytes::Bytes {
        // Non-default context: the handler only receives the hash half, so
        // this forces the MatchHash fallback path (a full-address match
        // misses), mirroring production DiffChange addresses.
        let (address, _fragment) = lore_storage::write_content(
            store.clone(),
            partition,
            Context::from([7u8; 16].as_ref()),
            bytes::Bytes::copy_from_slice(content),
            WriteOptions::default().no_remote_write(),
            None,
            None,
        )
        .await
        .expect("write blob");
        bytes::Bytes::copy_from_slice(address.hash.as_ref())
    }

    async fn run_diff(
        store: Arc<dyn lore_storage::ImmutableStore>,
        partition: lore_base::types::Partition,
        from: bytes::Bytes,
        to: bytes::Bytes,
        max_diff_size: Option<u64>,
    ) -> (ContentDiffHeader, String) {
        let mut request = Request::new(ContentDiffRequest {
            address_from: from,
            address_to: to,
            address_base: None,
            context_lines: None,
            ignore_whitespace_eol: false,
            ignore_whitespace_inline: false,
            max_diff_size,
        });
        request.metadata_mut().insert_bin(
            "urc-repository-id-bin",
            tonic::metadata::BinaryMetadataValue::from_bytes(partition.as_ref()),
        );
        let mut stream = handler(request, store).await.expect("diff").into_inner();
        let mut header = None;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            match item.expect("stream item").payload {
                Some(Payload::Header(h)) => header = Some(h),
                Some(Payload::Chunk(chunk)) => text.push_str(&chunk.diff),
                None => panic!("payload unset"),
            }
        }
        (header.expect("header first"), text)
    }

    #[tokio::test]
    async fn diff_modify_add_delete_binary_and_truncation() {
        let (store, _mutable, execution) = test_store_create().await.expect("test stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let partition = lore_base::types::Partition::default();

            let old = write_blob(&store, partition, b"alpha\nbravo\ncharlie\n").await;
            let new = write_blob(&store, partition, b"alpha\nBRAVO\ncharlie\ndelta\n").await;

            // Modify: one line changed, one added.
            let (header, text) =
                run_diff(store.clone(), partition, old.clone(), new.clone(), None).await;
            assert_eq!(header.lines_added, 2);
            assert_eq!(header.lines_deleted, 1);
            assert!(!header.binary && !header.truncated);
            assert!(text.contains("-bravo\n"), "diff text: {text}");
            assert!(text.contains("+BRAVO\n"));
            assert!(text.contains("+delta\n"));

            // Add: empty from side.
            let (header, text) = run_diff(
                store.clone(),
                partition,
                bytes::Bytes::new(),
                new.clone(),
                None,
            )
            .await;
            assert_eq!(header.lines_added, 4);
            assert_eq!(header.lines_deleted, 0);
            assert!(text.contains("+alpha\n"));

            // Delete: empty to side.
            let (header, _text) = run_diff(
                store.clone(),
                partition,
                old.clone(),
                bytes::Bytes::new(),
                None,
            )
            .await;
            assert_eq!(header.lines_added, 0);
            assert_eq!(header.lines_deleted, 3);

            // Binary input short-circuits (PNG magic → non-diffable mime).
            let binary =
                write_blob(&store, partition, b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR").await;
            let (header, text) =
                run_diff(store.clone(), partition, old.clone(), binary, None).await;
            assert!(header.binary);
            assert!(text.is_empty());
            assert_eq!(header.lines_added, 0);

            // Truncation: stats survive, no chunks.
            let (header, text) = run_diff(store.clone(), partition, old, new, Some(4)).await;
            assert!(header.truncated);
            assert!(text.is_empty());
            assert_eq!(header.lines_added, 2);
        }))
        .await;
    }

    /// Deterministic pseudo-random bytes with a PNG magic prefix, so the
    /// content sniffs as binary (and fragments when large).
    fn binary_buffer(len: usize, seed: u64) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut state = seed | 1;
        while out.len() < len {
            // xorshift64
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[tokio::test]
    async fn oversized_binary_reports_binary_not_truncated() {
        let (store, _mutable, execution) = test_store_create().await.expect("test stores");
        Box::pin(LORE_CONTEXT.scope(execution, async move {
            let partition = lore_base::types::Partition::default();
            // Over MAX_INPUT_BYTES: full reassembly is refused, but the
            // prefix sniff still classifies the content as binary.
            const SIZE: usize = MAX_INPUT_BYTES as usize + 1024 * 1024;
            let v1 = binary_buffer(SIZE, 9);
            let mut v2 = v1.clone();
            for byte in &mut v2[1_000_000..1_050_000] {
                *byte ^= 0x5A;
            }
            let a1 = write_blob(&store, partition, &v1).await;
            let a2 = write_blob(&store, partition, &v2).await;
            let (header, text) = run_diff(store.clone(), partition, a1, a2, None).await;
            assert!(header.binary, "prefix sniff must classify as binary");
            assert!(!header.truncated);
            assert!(text.is_empty());
        }))
        .await;
    }
}
