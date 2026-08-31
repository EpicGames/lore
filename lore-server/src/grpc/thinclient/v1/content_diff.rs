// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use lore_base::lore_spawn;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_proto::lore::thin_client::v1 as thin_client_v1;
use lore_proto::lore::thin_client::v1::ContentDiffRequest;
use lore_proto::lore::thin_client::v1::ContentDiffResponse;
use lore_proto::lore::thin_client::v1::content_diff_response::Payload;
use lore_revision::file::diff::DEFAULT_CONTEXT_LINES;
use lore_revision::immutable;
use lore_revision::immutable::read_options_from_repository;
use lore_revision::infer::infer_is_diffable_by_slice;
use lore_revision::merge::merge3_text;
use lore_revision::repository::RepositoryContext;
use lore_revision::util::encoding::decode_text_for_display;
use lore_revision::util::encoding::is_utf16_bom;
use lore_telemetry::tracing::fields::REPOSITORY_ID;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::debug;
use tracing::warn;

use crate::grpc::extract_correlation_id;
use crate::grpc::get_repository;
use crate::grpc::get_user_id;
use crate::grpc::warn_error_to_status;
use crate::util::setup_execution;

type ContentDiffStream =
    Pin<Box<dyn Stream<Item = Result<ContentDiffResponse, Status>> + Send + 'static>>;

const CONTENT_DIFF_CHUNK_SIZE: usize = 64 * 1024;

/// `lore.thin_client.v1.ThinClientService.ContentDiff` handler.
///
/// Server-streams a `ContentDiffHeader` first, then zero or more text
/// chunks whose concatenation is the unified diff. The request is
/// address-only: empty bytes mean an absent side, 32 bytes mean a CAS hash
/// in the current repository context, and 48 bytes mean a full
/// `Address { hash, context }`.
///
/// Content is loaded up-front so storage failures surface as gRPC
/// `Status` before the stream opens. The stream itself only emits the
/// already-rendered header and diff chunks.
#[tracing::instrument(name = "ContentDiff::v1::handle", skip_all)]
pub async fn handler(
    request: Request<ContentDiffRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    mutable_store: Arc<dyn lore_storage::MutableStore>,
) -> Result<Response<ContentDiffStream>, Status> {
    let repository_id = get_repository(request.metadata())?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let req = request.into_inner();

    let execution = setup_execution(module_path!(), correlation_id, user_id);
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable_store,
        mutable_store,
        repository_id,
    ));

    LORE_CONTEXT
        .scope(execution, async move {
            let result = build_content_diff(repository, req).await?;
            let (tx, rx) = mpsc::channel(8);

            lore_spawn!(
                async move {
                    stream_content_diff(result, tx).await;
                }
                .in_current_span()
            );

            let stream: ContentDiffStream = Box::pin(ReceiverStream::from(rx));
            Ok(Response::new(stream))
        })
        .await
}

async fn build_content_diff(
    repository: Arc<RepositoryContext>,
    req: ContentDiffRequest,
) -> Result<ContentDiffResult, Status> {
    let from = read_content_side(repository.clone(), &req.address_from, "from").await?;
    let to = read_content_side(repository.clone(), &req.address_to, "to").await?;
    let base = match req.address_base.as_ref().filter(|bytes| !bytes.is_empty()) {
        Some(bytes) => Some(read_content_side(repository, bytes, "base").await?),
        None => None,
    };

    let any_binary =
        from.is_binary() || to.is_binary() || base.as_ref().is_some_and(|b| b.is_binary());
    if any_binary {
        return Ok(ContentDiffResult {
            header: thin_client_v1::ContentDiffHeader {
                binary: true,
                ..Default::default()
            },
            diff: None,
        });
    }

    let options = DiffRenderOptions {
        context_lines: req.context_lines.unwrap_or(DEFAULT_CONTEXT_LINES),
        ignore_whitespace_eol: req.ignore_whitespace_eol,
        ignore_whitespace_inline: req.ignore_whitespace_inline,
    };

    let (diff, has_conflicts, conflict_count) = match base {
        Some(base) => build_three_way_diff(base.text(), from.text(), to.text(), options),
        None => (
            build_unified_patch(from.text(), to.text(), "from", "to", options),
            false,
            0,
        ),
    };
    let (lines_added, lines_deleted) = diff.as_deref().map_or((0, 0), count_patch_stats);
    let truncated = diff
        .as_ref()
        .is_some_and(|diff| req.max_diff_size.is_some_and(|max| diff.len() as u64 > max));
    let diff = if truncated { None } else { diff };

    Ok(ContentDiffResult {
        header: thin_client_v1::ContentDiffHeader {
            lines_added,
            lines_deleted,
            binary: false,
            truncated,
            has_conflicts,
            conflict_count,
        },
        diff,
    })
}

async fn read_content_side(
    repository: Arc<RepositoryContext>,
    bytes: &Bytes,
    label: &'static str,
) -> Result<DiffContent, Status> {
    let Some(address) = parse_content_address(bytes, label)? else {
        return Ok(DiffContent::empty());
    };

    if address.is_zero() {
        return Ok(DiffContent::empty());
    }

    let content = immutable::read(
        repository.clone(),
        address,
        None,
        read_options_from_repository(&repository)
            .with_decompress()
            .with_verify()
            .no_remote(),
    )
    .await
    .map_err(|err| {
        if err.is_address_not_found() || err.is_payload_not_found() || err.is_not_found() {
            Status::not_found(format!("{label} content not found"))
        } else {
            warn!(
                {REPOSITORY_ID} = %repository.id,
                side = label,
                ?address,
                ?err,
                "Failed to read content for thin-client diff",
            );
            warn_error_to_status(&err, |e| Status::internal(e.to_string()))
        }
    })?;

    Ok(make_diff_content(&content))
}

fn parse_content_address(bytes: &Bytes, label: &'static str) -> Result<Option<Address>, Status> {
    match bytes.len() {
        0 => Ok(None),
        len if len == size_of::<Hash>() => Ok(Some(Address::zero_context_hash(Hash::from(bytes)))),
        len if len == size_of::<Address>() => {
            let hash = Hash::from(&bytes[..size_of::<Hash>()]);
            let context = Context::from(&bytes[size_of::<Hash>()..]);
            Ok(Some(Address { hash, context }))
        }
        len => Err(Status::invalid_argument(format!(
            "{label} address must be empty, {} hash bytes, or {} address bytes; got {len}",
            size_of::<Hash>(),
            size_of::<Address>(),
        ))),
    }
}

fn build_three_way_diff(
    base: &str,
    from: &str,
    to: &str,
    options: DiffRenderOptions,
) -> (Option<String>, bool, u32) {
    match merge3_text(base, from, to, Some("base"), Some("from"), Some("to")) {
        Ok(merged) => (
            build_unified_patch(from, &merged, "from", "merged", options),
            false,
            0,
        ),
        Err(conflicted) => {
            let conflict_count = count_conflicts(&conflicted);
            (
                build_unified_patch(from, &conflicted, "from", "merged", options),
                true,
                conflict_count,
            )
        }
    }
}

async fn stream_content_diff(
    result: ContentDiffResult,
    tx: mpsc::Sender<Result<ContentDiffResponse, Status>>,
) {
    if tx
        .send(Ok(ContentDiffResponse {
            payload: Some(Payload::Header(result.header)),
        }))
        .await
        .is_err()
    {
        debug!("ContentDiff receiver dropped before header");
        return;
    }

    let Some(diff) = result.diff else {
        debug!("ContentDiff complete: header only");
        return;
    };

    let mut emitted = 0;
    for chunk in utf8_chunks(&diff, CONTENT_DIFF_CHUNK_SIZE) {
        if tx
            .send(Ok(ContentDiffResponse {
                payload: Some(Payload::Chunk(thin_client_v1::ContentDiffChunkResponse {
                    diff: chunk.to_string(),
                })),
            }))
            .await
            .is_err()
        {
            debug!(emitted, "ContentDiff receiver dropped mid-stream");
            return;
        }
        emitted += 1;
    }

    debug!(emitted, "ContentDiff complete");
}

struct ContentDiffResult {
    header: thin_client_v1::ContentDiffHeader,
    diff: Option<String>,
}

#[derive(Clone, Copy)]
struct DiffRenderOptions {
    context_lines: u32,
    ignore_whitespace_eol: bool,
    ignore_whitespace_inline: bool,
}

fn build_unified_patch(
    old: &str,
    new: &str,
    from_label: &str,
    to_label: &str,
    options: DiffRenderOptions,
) -> Option<String> {
    let patch = if options.ignore_whitespace_eol || options.ignore_whitespace_inline {
        format_patch_preserving_originals(
            old,
            new,
            options.context_lines,
            options.ignore_whitespace_eol,
            options.ignore_whitespace_inline,
        )?
    } else {
        // diffy's `Display`/`to_string()` defaults to `suppress_blank_empty: true`,
        // which drops the leading space on blank context lines (bare `\n`). Standard
        // unified-diff parsers require every hunk-body line to start with a sentinel
        // (' ', '+', '-', '\'), so format explicitly with suppression disabled.
        let patch = diffy::DiffOptions::new()
            .set_context_len(options.context_lines as usize)
            .create_patch(old, new);
        let s = diffy::PatchFormatter::new()
            .suppress_blank_empty(false)
            .fmt_patch(&patch)
            .to_string();
        if s.ends_with("+++ modified\n") {
            return None;
        }
        s
    };
    let patch = patch.replace("--- original", &format!("--- {from_label}"));
    let patch = patch.replace("+++ modified", &format!("+++ {to_label}"));
    Some(patch)
}

/// Normalise `old` and `new` per-line for comparison, run diffy, then re-emit
/// the unified diff with original (un-normalised) line content. Returns
/// `None` when no hunks remain after normalisation (i.e. the files are equal
/// under the selected whitespace rules).
///
/// The line count of each normalised side equals the line count of the
/// original side, so diffy's 1-based hunk line numbers index back into the
/// original line arrays correctly.
fn format_patch_preserving_originals(
    old: &str,
    new: &str,
    context_lines: u32,
    ignore_eol: bool,
    ignore_inline: bool,
) -> Option<String> {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    let old_norm: String = old_lines
        .iter()
        .map(|l| normalise_line(l, ignore_eol, ignore_inline))
        .collect();
    let new_norm: String = new_lines
        .iter()
        .map(|l| normalise_line(l, ignore_eol, ignore_inline))
        .collect();

    let patch = diffy::DiffOptions::new()
        .set_context_len(context_lines as usize)
        .create_patch(&old_norm, &new_norm);

    if patch.hunks().is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("--- original\n");
    out.push_str("+++ modified\n");

    for hunk in patch.hunks() {
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            hunk.old_range(),
            hunk.new_range()
        ));
        let mut old_idx = hunk.old_range().start();
        let mut new_idx = hunk.new_range().start();

        for line in hunk.lines() {
            match line {
                diffy::Line::Context(_) => {
                    let orig = old_lines
                        .get(old_idx.saturating_sub(1))
                        .copied()
                        .unwrap_or("");
                    write_patch_line(&mut out, ' ', orig);
                    old_idx += 1;
                    new_idx += 1;
                }
                diffy::Line::Delete(_) => {
                    let orig = old_lines
                        .get(old_idx.saturating_sub(1))
                        .copied()
                        .unwrap_or("");
                    write_patch_line(&mut out, '-', orig);
                    old_idx += 1;
                }
                diffy::Line::Insert(_) => {
                    let orig = new_lines
                        .get(new_idx.saturating_sub(1))
                        .copied()
                        .unwrap_or("");
                    write_patch_line(&mut out, '+', orig);
                    new_idx += 1;
                }
            }
        }
    }

    Some(out)
}

/// Per-line normalisation. Keeps the trailing `\n` (if present) so the line
/// count is preserved between original and normalised content. `\r` is
/// treated as whitespace so the EOL/inline rules apply uniformly to LF and
/// CRLF inputs.
fn normalise_line(line: &str, ignore_eol: bool, ignore_inline: bool) -> String {
    let (content, terminator) = match line.strip_suffix('\n') {
        Some(rest) => (rest, "\n"),
        None => (line, ""),
    };

    let mut work = if ignore_inline {
        collapse_inline_whitespace(content)
    } else {
        content.to_string()
    };

    if ignore_eol {
        let trimmed_len = work.trim_end_matches([' ', '\t', '\r']).len();
        work.truncate(trimmed_len);
    }

    work.push_str(terminator);
    work
}

/// Collapses runs of ASCII space/tab/CR to a single space. Does not invent
/// whitespace where there was none, and does not touch newline characters
/// (callers strip the terminator before invoking). Folding `\r` in keeps
/// LF and CRLF line endings on equal footing for inline comparison.
fn collapse_inline_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' || c == '\r' {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// Writes one unified-diff line. Every hunk-body line begins with its sentinel
/// (' ', '+', '-') — including blank context lines, which are emitted as `" \n"`
/// so standard unified-diff parsers count them as rows. Lines without a trailing
/// `\n` get a `\ No newline at end of file` marker.
fn write_patch_line(out: &mut String, sign: char, line: &str) {
    out.push(sign);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push('\n');
        out.push_str("\\ No newline at end of file\n");
    }
}

/// One side of a diff: either decoded display text, or a marker that the raw
/// bytes were detected as binary (non-text) content. Binary content carries no
/// text — it is never rendered through the diff/merge pipeline.
enum DiffContent {
    Text(String),
    Binary,
}

impl DiffContent {
    /// An absent side (file missing on this revision / `/dev/null`). Treated as
    /// empty text, never binary.
    fn empty() -> Self {
        DiffContent::Text(String::new())
    }

    fn is_binary(&self) -> bool {
        matches!(self, DiffContent::Binary)
    }

    /// The decoded text for a text side; `""` for binary. Callers short-circuit
    /// on `is_binary()` before reaching this, so the binary case is never read
    /// in practice.
    fn text(&self) -> &str {
        match self {
            DiffContent::Text(s) => s,
            DiffContent::Binary => "",
        }
    }
}

/// Build display content from raw bytes.
///
/// An empty buffer (an absent side) is text, not binary. UTF-16 BOM input is
/// exempt from the binary check: `decode_text_for_display` renders it as
/// readable text, and the diff path intentionally shows UTF-16 as text — unlike
/// the merge path, where `infer_is_diffable_by_slice` treats UTF-16 as binary
/// to preserve bytes. Everything else (null bytes, non-text MIME, Unreal
/// packages, invalid UTF-8) is classified as binary, and its bytes are never
/// decoded.
fn make_diff_content(bytes: &[u8]) -> DiffContent {
    if !bytes.is_empty() && !is_utf16_bom(bytes) && !infer_is_diffable_by_slice(bytes) {
        DiffContent::Binary
    } else {
        DiffContent::Text(decode_text_for_display(bytes))
    }
}

fn count_patch_stats(patch: &str) -> (u64, u64) {
    let mut lines_added = 0;
    let mut lines_deleted = 0;

    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            lines_added += 1;
        } else if line.starts_with('-') {
            lines_deleted += 1;
        }
    }

    (lines_added, lines_deleted)
}

fn count_conflicts(text: &str) -> u32 {
    text.lines()
        .filter(|line| line.starts_with("<<<<<<< "))
        .count() as u32
}

fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    let max_bytes = max_bytes.max(1);
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start
                + text[start..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or_default();
        }
        chunks.push(&text[start..end]);
        start = end;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> DiffRenderOptions {
        DiffRenderOptions {
            context_lines: DEFAULT_CONTEXT_LINES,
            ignore_whitespace_eol: false,
            ignore_whitespace_inline: false,
        }
    }

    #[test]
    fn parse_content_address_accepts_empty() {
        assert!(
            parse_content_address(&Bytes::new(), "from")
                .expect("parse")
                .is_none()
        );
    }

    #[test]
    fn parse_content_address_accepts_hash_only() {
        let bytes = Bytes::from(vec![0x11; size_of::<Hash>()]);
        let address = parse_content_address(&bytes, "from")
            .expect("parse")
            .expect("address");

        assert_eq!(address.hash, Hash::from(&bytes));
        assert_eq!(address.context, Context::default());
    }

    #[test]
    fn parse_content_address_accepts_full_address() {
        let mut bytes = vec![0x11; size_of::<Hash>()];
        bytes.extend([0x22; size_of::<Context>()]);
        let bytes = Bytes::from(bytes);
        let address = parse_content_address(&bytes, "to")
            .expect("parse")
            .expect("address");

        assert_eq!(address.hash, Hash::from(&bytes[..size_of::<Hash>()]));
        assert_eq!(address.context, Context::from(&bytes[size_of::<Hash>()..]));
    }

    #[test]
    fn parse_content_address_rejects_bad_length() {
        let err =
            parse_content_address(&Bytes::from_static(b"short"), "base").expect_err("bad length");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn two_way_diff_reports_stats() {
        let patch = build_unified_patch("old\n", "new\n", "from", "to", options()).expect("patch");
        let (added, deleted) = count_patch_stats(&patch);

        assert!(patch.contains("--- from\n"));
        assert!(patch.contains("+++ to\n"));
        assert_eq!((added, deleted), (1, 1));
    }

    #[test]
    fn unchanged_diff_has_no_patch() {
        assert!(build_unified_patch("same\n", "same\n", "from", "to", options()).is_none());
    }

    #[test]
    fn ignored_whitespace_can_remove_patch() {
        let mut options = options();
        options.ignore_whitespace_eol = true;
        assert!(build_unified_patch("same   \n", "same\n", "from", "to", options).is_none());
    }

    #[test]
    fn three_way_conflict_sets_conflict_count() {
        let (patch, has_conflicts, conflict_count) =
            build_three_way_diff("base\n", "from\n", "to\n", options());

        assert!(patch.expect("patch").contains("<<<<<<< from"));
        assert!(has_conflicts);
        assert_eq!(conflict_count, 1);
    }

    #[test]
    fn binary_content_is_detected() {
        assert!(make_diff_content(&[0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00]).is_binary());
    }

    #[test]
    fn utf8_chunks_split_on_character_boundaries() {
        let text = "aa🙂bb";
        let chunks = utf8_chunks(text, 3);

        assert_eq!(chunks.concat(), text);
        assert!(
            chunks
                .iter()
                .all(|chunk| std::str::from_utf8(chunk.as_bytes()).is_ok())
        );
    }
}
