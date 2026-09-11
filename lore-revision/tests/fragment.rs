// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_storage::CompressionMode;
    use lore_storage::compress;
    #[cfg(feature = "oodle")]
    use lore_storage::compress::FRAGMENT_COMPRESS_SIZE_LIMIT;
    #[cfg(feature = "oodle")]
    use rand::Rng;

    include!("helper.rs");

    #[cfg(feature = "oodle")]
    #[tokio::test]
    async fn compress_too_small() {
        let mut payload = [0u8; FRAGMENT_COMPRESS_SIZE_LIMIT - 1];
        rand::rng().fill(&mut payload[..]);

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                };

                compress::compress_without_deprecation_checks(
                    fragment,
                    &payload,
                    CompressionMode::Oodle,
                )
                .expect_err("Undersized fragment compress did not fail as expected");
            })
            .await;
    }

    #[cfg(feature = "oodle")]
    #[tokio::test]
    async fn compress_already_compressed() {
        let mut payload = [0u8; FRAGMENT_COMPRESS_SIZE_LIMIT * 2];
        rand::rng().fill(&mut payload[..]);

        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: (FragmentFlags::PayloadStoredLocal
                        | FragmentFlags::PayloadCompressedOodle2)
                        .bits(),
                    size_payload: payload.len() as u32,
                    size_content: 4 * payload.len() as u64,
                };

                compress::compress_without_deprecation_checks(
                    fragment,
                    &payload,
                    CompressionMode::Oodle,
                )
                .expect_err("Compressed fragment compress did not fail as expected");
            })
            .await;
    }

    #[cfg(feature = "oodle")]
    #[tokio::test]
    async fn compress_decompress() {
        let mut data = [0u8; 10000];
        for (i, item) in data.iter_mut().enumerate() {
            *item = (i % 10) as u8;
        }

        let execution = setup_test_execution();

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: data.len() as u32,
                    size_content: data.len() as u64,
                };

                let (compressed_fragment, compressed_buffer) =
                    compress::compress_without_deprecation_checks(
                        fragment,
                        &data,
                        CompressionMode::Oodle,
                    )
                    .expect("Compression failed");

                let (decompressed_fragment, decompressed_data) =
                    compress::decompress(compressed_fragment, compressed_buffer.as_ref())
                        .expect("Decompression failed");

                assert_eq!(fragment.size_content, decompressed_fragment.size_content);
                assert_eq!(fragment.size_payload, decompressed_fragment.size_payload);
                assert_eq!(
                    decompressed_fragment.flags & FragmentFlags::PayloadCompressed,
                    0
                );

                assert_eq!(data, decompressed_data.as_ref());
            })
            .await;
    }

    #[tokio::test]
    async fn compress_decompress_lz4() {
        let mut data = [0u8; 10000];
        for (i, item) in data.iter_mut().enumerate() {
            *item = (i % 10) as u8;
        }

        let execution = setup_test_execution();

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: data.len() as u32,
                    size_content: data.len() as u64,
                };

                let (compressed_fragment, compressed_buffer) =
                    compress::compress(fragment, &data, CompressionMode::Lz4)
                        .expect("LZ4 compression failed");

                assert_ne!(
                    compressed_fragment.flags & FragmentFlags::PayloadCompressedLZ4,
                    0
                );

                let (decompressed_fragment, decompressed_data) =
                    compress::decompress(compressed_fragment, compressed_buffer.as_ref())
                        .expect("LZ4 decompression failed");

                assert_eq!(fragment.size_content, decompressed_fragment.size_content);
                assert_eq!(fragment.size_payload, decompressed_fragment.size_payload);
                assert_eq!(
                    decompressed_fragment.flags & FragmentFlags::PayloadCompressed,
                    0
                );

                assert_eq!(data, decompressed_data.as_ref());
            })
            .await;
    }

    #[tokio::test]
    async fn compress_decompress_zstd() {
        let mut data = [0u8; 10000];
        for (i, item) in data.iter_mut().enumerate() {
            *item = (i % 10) as u8;
        }

        let execution = setup_test_execution();

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: data.len() as u32,
                    size_content: data.len() as u64,
                };

                let (compressed_fragment, compressed_buffer) =
                    compress::compress(fragment, &data, CompressionMode::Zstd)
                        .expect("Zstd compression failed");

                assert_ne!(
                    compressed_fragment.flags & FragmentFlags::PayloadCompressedZstd,
                    0
                );

                let (decompressed_fragment, decompressed_data) =
                    compress::decompress(compressed_fragment, compressed_buffer.as_ref())
                        .expect("Zstd decompression failed");

                assert_eq!(fragment.size_content, decompressed_fragment.size_content);
                assert_eq!(fragment.size_payload, decompressed_fragment.size_payload);
                assert_eq!(
                    decompressed_fragment.flags & FragmentFlags::PayloadCompressed,
                    0
                );

                assert_eq!(data, decompressed_data.as_ref());
            })
            .await;
    }

    #[tokio::test]
    async fn compress_decompress_into_zstd() {
        let mut data = [0u8; 10000];
        for (i, item) in data.iter_mut().enumerate() {
            *item = (i % 10) as u8;
        }

        let execution = setup_test_execution();

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: data.len() as u32,
                    size_content: data.len() as u64,
                };

                let (compressed_fragment, compressed_buffer) =
                    compress::compress(fragment, &data, CompressionMode::Zstd)
                        .expect("Zstd compression failed");

                let mut decompressed_data = vec![0u8; data.len()];
                let decompressed_fragment = compress::decompress_into_slice(
                    compressed_fragment,
                    compressed_buffer.as_ref(),
                    &mut decompressed_data,
                )
                .expect("Zstd decompress_into failed");

                assert_eq!(fragment.size_content, decompressed_fragment.size_content);
                assert_eq!(fragment.size_payload, decompressed_fragment.size_payload);
                assert_eq!(
                    decompressed_fragment.flags & FragmentFlags::PayloadCompressed,
                    0
                );

                assert_eq!(data.as_slice(), decompressed_data.as_slice());
            })
            .await;
    }

    #[tokio::test]
    async fn compress_decompress_into_lz4() {
        let mut data = [0u8; 10000];
        for (i, item) in data.iter_mut().enumerate() {
            *item = (i % 10) as u8;
        }

        let execution = setup_test_execution();

        #[allow(clippy::large_futures)]
        LORE_CONTEXT
            .scope(execution, async move {
                let fragment = Fragment {
                    flags: FragmentFlags::PayloadStoredLocal.bits(),
                    size_payload: data.len() as u32,
                    size_content: data.len() as u64,
                };

                let (compressed_fragment, compressed_buffer) =
                    compress::compress(fragment, &data, CompressionMode::Lz4)
                        .expect("LZ4 compression failed");

                let mut decompressed_data = vec![0u8; data.len()];
                let decompressed_fragment = compress::decompress_into_slice(
                    compressed_fragment,
                    compressed_buffer.as_ref(),
                    &mut decompressed_data,
                )
                .expect("LZ4 decompress_into failed");

                assert_eq!(fragment.size_content, decompressed_fragment.size_content);
                assert_eq!(fragment.size_payload, decompressed_fragment.size_payload);
                assert_eq!(
                    decompressed_fragment.flags & FragmentFlags::PayloadCompressed,
                    0
                );

                assert_eq!(data.as_slice(), decompressed_data.as_slice());
            })
            .await;
    }
}
