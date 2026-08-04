// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Ingress filter that transcodes incoming Oodle-compressed fragments to Zstd.
//!
//! Unless disabled (via the `LORE_DISABLE_CONVERT_OODLE_ON_PUT` environment variable), every fragment a
//! client puts is inspected: if it is Oodle-compressed it is decompressed and re-compressed as
//! Zstd (or stored uncompressed if Zstd is not worthwhile) before being written. Because a
//! fragment's identity is the hash of its *uncompressed* content, the address is unchanged; only
//! `flags`, `size_payload`, and the stored blob change.

use std::sync::LazyLock;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_storage::CompressionMode;
use tracing::warn;

const CONVERT_DISABLED_ENV: &str = "LORE_DISABLE_CONVERT_OODLE_ON_PUT";

static CONVERT_DISABLED: LazyLock<bool> = LazyLock::new(
    || matches!(std::env::var(CONVERT_DISABLED_ENV), Ok(value) if value.eq_ignore_ascii_case("true") || value == "1"),
);

/// Apply the ingress conversion if it is enabled; otherwise the
/// inputs are returned unchanged.
pub(crate) async fn convert_oodle_on_ingress(
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> (Fragment, Option<Bytes>) {
    if *CONVERT_DISABLED {
        return (fragment, payload);
    }

    transcode_oodle_to_zstd(address, fragment, payload).await
}

/// Transcode an unfragmented Oodle payload to Zstd (or uncompressed, if Zstd is not worthwhile),
/// returning the rewritten fragment and blob. Non-Oodle, fragmented, and payload-less inputs are
/// returned unchanged, as is any input that fails to decompress, so a put is never broken by the
/// filter.
async fn transcode_oodle_to_zstd(
    address: Address,
    fragment: Fragment,
    payload: Option<Bytes>,
) -> (Fragment, Option<Bytes>) {
    let Some(payload) = payload else {
        return (fragment, None);
    };

    if fragment.flags & FragmentFlags::PayloadCompressedOodle2 == 0
        // compression + fragmented is never sent by legit clients
        || fragment.flags & FragmentFlags::PayloadFragmented != 0
    {
        return (fragment, Some(payload));
    }

    let (decompressed_fragment, decompressed) =
        match lore_storage::decompress(fragment, payload.as_ref()) {
            Ok(result) => result,
            Err(error) => {
                warn!(
                    address = %address,
                    ?error,
                    "Oodle->Zstd put conversion skipped: failed to decompress"
                );
                return (fragment, Some(payload));
            }
        };
    let decompressed = decompressed.freeze();

    match lore_storage::compress(
        decompressed_fragment,
        decompressed.as_ref(),
        CompressionMode::Zstd,
    ) {
        Ok((zstd_fragment, zstd_payload)) => (zstd_fragment, Some(zstd_payload)),
        // Zstd was not worthwhile/successful; store the content uncompressed. Either way it is no longer Oodle.
        Err(_) => (decompressed_fragment, Some(decompressed)),
    }
}

#[cfg(all(test, feature = "oodle"))]
mod tests {
    use lore_storage::hash_slice;

    use super::*;

    /// A highly compressible payload (a unique prefix plus a long constant run) that both Oodle
    /// and Zstd will happily compress.
    fn compressible_payload() -> Vec<u8> {
        let mut payload = Vec::with_capacity(1024);
        payload.extend_from_slice(b"oodle-ingress-test");
        payload.resize(1024, 0xab);
        payload
    }

    fn address_of(content: &[u8]) -> Address {
        Address {
            hash: hash_slice(content),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn transcodes_oodle_to_non_oodle_preserving_content() {
        let content = compressible_payload();
        let address = address_of(&content);
        let base = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (oodle_fragment, oodle_payload) =
            lore_storage::compress(base, content.as_slice(), CompressionMode::Oodle)
                .expect("payload should Oodle-compress");
        assert_ne!(
            oodle_fragment.flags & FragmentFlags::PayloadCompressedOodle2,
            0,
            "precondition: the input must be Oodle compressed"
        );

        let (converted, payload) =
            transcode_oodle_to_zstd(address, oodle_fragment, Some(oodle_payload)).await;
        let payload = payload.expect("conversion must keep the payload");

        // No longer Oodle, sizes consistent, and the content still decodes to the original.
        assert_eq!(converted.flags & FragmentFlags::PayloadCompressedOodle2, 0);
        assert_eq!(converted.size_content, base.size_content);
        assert_eq!(payload.len(), converted.size_payload as usize);
        let recovered_hash = lore_storage::hash_fragment(converted, payload.as_ref())
            .expect("converted fragment should hash");
        assert_eq!(recovered_hash, address.hash);
    }

    #[tokio::test]
    async fn leaves_non_oodle_fragment_unchanged() {
        let content = compressible_payload();
        let address = address_of(&content);
        let base = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (zstd_fragment, zstd_payload) =
            lore_storage::compress(base, content.as_slice(), CompressionMode::Zstd)
                .expect("payload should Zstd-compress");

        let (fragment, payload) =
            transcode_oodle_to_zstd(address, zstd_fragment, Some(zstd_payload.clone())).await;

        assert_eq!(fragment, zstd_fragment);
        assert_eq!(payload, Some(zstd_payload));
    }

    #[tokio::test]
    async fn leaves_metadata_only_put_unchanged() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadCompressedOodle2.bits(),
            size_payload: 64,
            size_content: 128,
        };

        let (out_fragment, out_payload) =
            transcode_oodle_to_zstd(Address::default(), fragment, None).await;

        assert_eq!(out_fragment, fragment);
        assert!(out_payload.is_none());
    }
}
