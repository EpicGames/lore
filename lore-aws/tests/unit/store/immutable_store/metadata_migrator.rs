// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use lore_aws::aws::error::DynamoErrorCode;
use lore_aws::aws::item::AttributeValue;
use lore_aws::aws::item::Item;
use lore_aws::dynamodb::ScanConfig;
use lore_aws::store::immutable_store::FragmentState;
use lore_aws::store::immutable_store::metadata_migrator::*;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::store::fake::FRAGMENT_METADATA_TABLE_NAME;
use crate::store::fake::Fake;
use crate::store::fake::store;
use crate::store::fake::store_with_separate_metadata_table;

/// A migrator over `fake` with no retries, writing unless `is_dry_run`.
async fn make_migrator(fake: &Fake, is_dry_run: bool) -> Arc<MetadataMigrator> {
    Arc::new(MetadataMigrator::new(MetadataMigratorConfig {
        dynamodb: lore_aws::dynamodb::MockDynamoDb::default(),
        store: store_with_separate_metadata_table(fake).await,
        metadata_table_name: FRAGMENT_METADATA_TABLE_NAME.into(),
        api_call_max_retries: 0,
        api_retry_base_delay: Duration::ZERO,
        scan_config: ScanConfig::default(),
        is_dry_run,
    }))
}

fn make_zstd_payload(content: &[u8]) -> (Fragment, Bytes, Hash) {
    let hash = lore_storage::hash_slice(content);
    let raw = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    let (frag, payload) = lore_storage::compress(raw, content, lore_storage::CompressionMode::Zstd)
        .expect("zstd compression should succeed on compressible data");
    (frag, payload, hash)
}

fn make_lz4_payload(content: &[u8]) -> (Fragment, Bytes, Hash) {
    let hash = lore_storage::hash_slice(content);
    let raw = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };
    let (frag, payload) = lore_storage::compress(raw, content, lore_storage::CompressionMode::Lz4)
        .expect("lz4 compression should succeed");
    (frag, payload, hash)
}

mod try_codec_probes_tests {
    use super::*;

    #[test]
    fn uncompressed_payload_matches_hash() {
        let content = b"hello from lore migration tests";
        let payload = Bytes::copy_from_slice(content);
        let hash = lore_storage::hash_slice(content);
        let (frag, bytes) = try_codec_probes(&payload, 0, content.len() as u64, hash)
            .expect("should match own hash");
        assert_eq!(frag.flags & FragmentFlags::PayloadCompressed, 0);
        assert_eq!(bytes.as_ref(), content.as_slice());
        assert_eq!(frag.size_payload as usize, bytes.len());
        assert_eq!(frag.size_content, content.len() as u64);
    }

    #[test]
    fn zstd_compressed_payload_matches_hash() {
        let content = vec![0xABu8; 300];
        let (in_frag, compressed, hash) = make_zstd_payload(&content);
        let (out_frag, decompressed) = try_codec_probes(&compressed, 0, in_frag.size_content, hash)
            .expect("zstd payload should be identified");
        assert_eq!(lore_storage::hash_slice(&decompressed), hash);
        assert_eq!(out_frag.size_payload as usize, decompressed.len());
        assert_eq!(out_frag.size_content, content.len() as u64);
    }

    #[test]
    fn lz4_compressed_payload_matches_hash() {
        let content = vec![0x77u8; 300];
        let (in_frag, compressed, hash) = make_lz4_payload(&content);
        let (out_frag, decompressed) = try_codec_probes(&compressed, 0, in_frag.size_content, hash)
            .expect("lz4 payload should be identified");
        assert_eq!(lore_storage::hash_slice(&decompressed), hash);
        assert_eq!(out_frag.size_payload as usize, decompressed.len());
        assert_eq!(out_frag.size_content, content.len() as u64);
    }

    #[test]
    fn garbage_bytes_return_none() {
        let garbage = Bytes::from(vec![0xFFu8; 64]);
        let hash = lore_storage::hash_slice(b"something else entirely");
        assert!(try_codec_probes(&garbage, 0, 64, hash).is_none());
    }
}

mod decompress_hash_tests {
    use super::*;

    #[test]
    fn accurate_uncompressed() {
        let content = vec![0x11u8; 50];
        let hash = lore_storage::hash_slice(&content);
        let frag = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        match decompress_hash(frag, &Bytes::from(content.clone()), hash) {
            DecompressOutcome::PayloadAccurate(_, b) => {
                assert_eq!(b.as_ref(), content.as_slice());
            }
            other => panic!("expected PayloadAccurate, got {other:?}"),
        }
    }

    #[test]
    fn accurate_zstd() {
        let content = vec![0xAAu8; 400];
        let (frag, compressed, hash) = make_zstd_payload(&content);
        match decompress_hash(frag, &compressed, hash) {
            DecompressOutcome::PayloadAccurate(_, b) => {
                assert_eq!(lore_storage::hash_slice(&b), hash);
            }
            other => panic!("expected PayloadAccurate, got {other:?}"),
        }
    }

    #[test]
    fn deduced_wrong_codec_flag() {
        let content = vec![0xBBu8; 400];
        let (mut frag, compressed, hash) = make_zstd_payload(&content);
        // Declare LZ4 but data is actually Zstd
        frag.flags = (frag.flags & !FragmentFlags::PayloadCompressed)
            | FragmentFlags::PayloadCompressedLZ4.bits();
        match decompress_hash(frag, &compressed, hash) {
            DecompressOutcome::PayloadDeduced(_, b) => {
                assert_eq!(lore_storage::hash_slice(&b), hash);
            }
            other => panic!("expected PayloadDeduced, got {other:?}"),
        }
    }

    #[test]
    fn deduced_declared_uncompressed_actually_zstd() {
        let content = vec![0xCCu8; 400];
        let hash = lore_storage::hash_slice(&content);
        let raw = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (comp_frag, compressed) =
            lore_storage::compress(raw, &content, lore_storage::CompressionMode::Zstd).unwrap();
        // Claim uncompressed
        let lying = Fragment {
            flags: 0,
            size_payload: comp_frag.size_payload,
            size_content: content.len() as u64,
        };
        match decompress_hash(lying, &compressed, hash) {
            DecompressOutcome::PayloadDeduced(_, b) => {
                assert_eq!(lore_storage::hash_slice(&b), hash);
            }
            other => panic!("expected PayloadDeduced, got {other:?}"),
        }
    }

    #[test]
    fn deduced_legacy_metadata_prefix_stripped() {
        let content = vec![0xDDu8; 400];
        let hash = lore_storage::hash_slice(&content);
        let raw = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        let (comp_frag, compressed) =
            lore_storage::compress(raw, &content, lore_storage::CompressionMode::Zstd).unwrap();
        let prefix = vec![0u8; size_of::<Fragment>()];
        let prefixed: Bytes = [prefix.as_slice(), compressed.as_ref()].concat().into();
        let legacy = Fragment {
            flags: comp_frag.flags,
            size_payload: prefixed.len() as u32,
            size_content: content.len() as u64,
        };
        match decompress_hash(legacy, &prefixed, hash) {
            DecompressOutcome::PayloadDeduced(out_frag, b) => {
                assert_eq!(lore_storage::hash_slice(&b), hash);
                // The returned fragment must describe the decompressed content,
                // not the prefixed blob — both sizes reflect the stripped payload.
                assert_eq!(out_frag.size_payload as usize, b.len());
                assert_eq!(out_frag.size_content, content.len() as u64);
            }
            other => panic!("expected PayloadDeduced for legacy prefix, got {other:?}"),
        }
    }

    #[test]
    fn could_not_deduce_garbage() {
        let garbage = vec![0xFFu8; 64];
        let hash = lore_storage::hash_slice(b"definitely not this");
        let frag = Fragment {
            flags: FragmentFlags::PayloadCompressedZstd.bits(),
            size_payload: 64,
            size_content: 200,
        };
        let garbage = Bytes::from(garbage);
        match decompress_hash(frag, &garbage, hash) {
            DecompressOutcome::CouldNotDeduce => {}
            other => panic!("expected CouldNotDeduce, got {other:?}"),
        }
    }
}

mod recompress_tests {
    use super::*;

    fn decompressed_fragment(content: &[u8]) -> (Fragment, Bytes) {
        // Simulate what decompress_hash returns: a fragment with no compression flags,
        // size_payload == size_content == content length.
        let frag = Fragment {
            flags: 0,
            size_payload: content.len() as u32,
            size_content: content.len() as u64,
        };
        (frag, Bytes::copy_from_slice(content))
    }

    mod recompress_to_zstd_tests {
        use super::*;

        #[test]
        fn compressible_data_recompresses_to_zstd_and_round_trips() {
            let content = vec![0xAAu8; 500];
            let (frag, decompressed) = decompressed_fragment(&content);

            let (out_frag, out_payload, outcome) =
                recompress_to_zstd(frag, decompressed, ConvertOutcome::RecompressedOodle).unwrap();
            assert_eq!(outcome, ConvertOutcome::RecompressedOodle);
            assert_ne!(out_frag.flags & FragmentFlags::PayloadCompressedZstd, 0);
            assert_eq!(out_frag.size_content, content.len() as u64);
            assert_eq!(out_frag.size_payload as usize, out_payload.len());

            let (_, roundtripped) = lore_storage::decompress(out_frag, &out_payload).unwrap();
            assert_eq!(roundtripped.as_ref(), content.as_slice());
        }

        #[test]
        fn on_success_outcome_is_forwarded() {
            let content = vec![0xBBu8; 500];
            let (frag, decompressed) = decompressed_fragment(&content);

            let (_, _, outcome) =
                recompress_to_zstd(frag, decompressed, ConvertOutcome::RecompressedMismatch)
                    .unwrap();
            assert_eq!(outcome, ConvertOutcome::RecompressedMismatch);
        }

        #[test]
        fn incompressible_data_falls_back_to_uncompressed() {
            let content: Vec<u8> = (0u8..=32).collect(); // 33 bytes, no repetition
            let (frag, decompressed) = decompressed_fragment(&content);

            let (out_frag, out_payload, outcome) = recompress_to_zstd(
                frag,
                decompressed.clone(),
                ConvertOutcome::RecompressedOodle,
            )
            .unwrap();
            assert_eq!(outcome, ConvertOutcome::ConvertedCompressedToUncompressed);
            assert_eq!(out_frag.flags & FragmentFlags::PayloadCompressed, 0);
            assert_eq!(out_payload, decompressed);
            assert_eq!(out_frag.size_content, content.len() as u64);
            assert_eq!(out_frag.size_payload as usize, out_payload.len());
        }
    }
}

mod parse_metadata_entry_tests {
    use lore_aws::aws::item::AttributeValue;
    use lore_aws::aws::item::ByteStr;

    use super::*;

    #[test]
    fn valid_hash_returns_some() {
        let hash: Hash = rand::random();
        let mut item = Item::new();
        item.insert(
            ByteStr::from_static("hash"),
            AttributeValue::blob(hash.data()),
        );
        assert_eq!(parse_metadata_entry(&item), Some(hash));
    }

    #[test]
    fn empty_item_returns_none() {
        assert!(parse_metadata_entry(&Item::new()).is_none());
    }

    #[test]
    fn wrong_type_for_hash_returns_none() {
        let mut item = Item::new();
        item.insert(
            ByteStr::from_static("hash"),
            AttributeValue::string("not-a-blob"),
        );
        assert!(parse_metadata_entry(&item).is_none());
    }
}

mod discover_legacy_fragments_tests {
    use lore_aws::dynamodb::MockDynamoDb;
    use lore_aws::dynamodb::ScanPage;

    use super::*;

    fn item_for_hash(hash: Hash) -> Item {
        let mut item = Item::new();
        item.insert(
            lore_aws::aws::item::ByteStr::from_static("hash"),
            lore_aws::aws::item::AttributeValue::blob(hash.data()),
        );
        item
    }

    async fn make_discover_migrator(dynamodb: MockDynamoDb) -> Arc<MetadataMigrator> {
        let fake = Fake::default();
        Arc::new(MetadataMigrator::new(MetadataMigratorConfig {
            dynamodb,
            store: store(&fake).await,
            metadata_table_name: "test-metadata".into(),
            api_call_max_retries: 0,
            api_retry_base_delay: Duration::ZERO,
            scan_config: ScanConfig::default(),
            is_dry_run: false,
        }))
    }

    #[tokio::test]
    async fn single_page_enqueues_all_hashes() {
        let hash1: Hash = rand::random();
        let hash2: Hash = rand::random();
        let h1 = hash1;
        let h2 = hash2;
        let mut dynamodb = MockDynamoDb::default();
        dynamodb
            .expect_scan_page()
            .returning(move |_, start_key, _| {
                if start_key.is_none() {
                    Ok(ScanPage {
                        items: vec![item_for_hash(h1), item_for_hash(h2)],
                        last_evaluated_key: None,
                    })
                } else {
                    panic!("unexpected second page call")
                }
            });
        let migrator = make_discover_migrator(dynamodb).await;
        let (tx, mut rx) = mpsc::channel(10);
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        migrator
            .discover_legacy_fragments(tx, stats.clone(), aborted)
            .await
            .unwrap();
        let mut received = vec![];
        while let Some(h) = rx.recv().await {
            received.push(h);
        }
        assert_eq!(received.len(), 2);
        assert!(received.contains(&hash1));
        assert!(received.contains(&hash2));
        assert_eq!(stats.scanned.load(Ordering::Relaxed), 2);
        assert_eq!(stats.valid_metadata_entries.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn paginated_scan_follows_last_evaluated_key() {
        let hash1: Hash = rand::random();
        let hash2: Hash = rand::random();
        let (h1, h2) = (hash1, hash2);
        let call_count = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let cc = call_count.clone();
        let mut dynamodb = MockDynamoDb::default();
        dynamodb
            .expect_scan_page()
            .returning(move |_, start_key, _| {
                let n = cc.fetch_add(1, Ordering::SeqCst);
                match n {
                    0 => {
                        assert!(start_key.is_none());
                        Ok(ScanPage {
                            items: vec![item_for_hash(h1)],
                            last_evaluated_key: Some({
                                let mut key = Item::new();
                                key.insert(
                                    lore_aws::aws::item::ByteStr::from_static("pk"),
                                    lore_aws::aws::item::AttributeValue::string("page1"),
                                );
                                key
                            }),
                        })
                    }
                    1 => {
                        assert!(start_key.is_some());
                        Ok(ScanPage {
                            items: vec![item_for_hash(h2)],
                            last_evaluated_key: None,
                        })
                    }
                    _ => panic!("unexpected extra scan call"),
                }
            });
        let migrator = make_discover_migrator(dynamodb).await;
        let (tx, mut rx) = mpsc::channel(10);
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        migrator
            .discover_legacy_fragments(tx, stats, aborted)
            .await
            .unwrap();
        let mut received = vec![];
        while let Some(h) = rx.recv().await {
            received.push(h);
        }
        assert_eq!(received.len(), 2);
        assert!(received.contains(&hash1));
        assert!(received.contains(&hash2));
    }

    /// Discovery that gives up on a scan reports why: a shed scan as a back-off, any other failure
    /// as internal.
    #[tokio::test]
    async fn a_failed_scan_reports_its_classification() {
        LORE_CONTEXT
            .scope(crate::store::setup_execution("test".to_string()), async {
                for (failure, shed) in [
                    (crate::store::fake::throughput_exceeded(), true),
                    (
                        crate::store::fake::dynamo_error(
                            DynamoErrorCode::ValidationException,
                            http::StatusCode::BAD_REQUEST,
                        ),
                        false,
                    ),
                ] {
                    let mut dynamodb = MockDynamoDb::default();
                    dynamodb
                        .expect_scan_page()
                        .times(1)
                        .return_once(move |_, _, _| Err(failure));
                    let migrator = make_discover_migrator(dynamodb).await;
                    let (tx, _rx) = mpsc::channel(10);

                    let error = migrator
                        .discover_legacy_fragments(
                            tx,
                            Arc::new(RewriteStats::default()),
                            Arc::new(AtomicBool::new(false)),
                        )
                        .await
                        .expect_err("a scan that failed is not a completed discovery");

                    if shed {
                        assert!(error.is_slow_down(), "expected a back-off, got {error:?}");
                    } else {
                        assert!(
                            error.is_internal(),
                            "expected an internal failure, got {error:?}"
                        );
                    }
                }
            })
            .await;
    }

    #[tokio::test]
    async fn aborted_flag_stops_before_scanning() {
        let mut dynamodb = MockDynamoDb::default();
        dynamodb.expect_scan_page().never();
        let migrator = make_discover_migrator(dynamodb).await;
        let (tx, _rx) = mpsc::channel(10);
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(true));
        migrator
            .discover_legacy_fragments(tx, stats.clone(), aborted)
            .await
            .unwrap();
        assert_eq!(stats.scanned.load(Ordering::Relaxed), 0);
    }
}

mod process_fragment_tests {
    use super::*;

    #[tokio::test]
    async fn skips_already_migrated() {
        // A fully migrated fragment has both a state entry and a properly-headered S3 object.
        // Migrate a legacy fragment first so both are set up correctly, then verify the
        // second call skips cleanly without touching state_with_no_head.
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x11u8; 100];
        let hash = lore_storage::hash_slice(&content);
        fake.put_object_without_metadata(hash, &content);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            },
        );
        migrator
            .process_fragment(hash, &RewriteStats::default())
            .await
            .unwrap();

        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::SkippedMigrated
        );
        assert_eq!(stats.state_with_no_head.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn state_with_no_head_is_counted_and_fragment_is_reprocessed() {
        // State entry exists but head_fragment returns 404 — a race between deployments.
        // The stat is incremented and processing falls through to load the fragment from
        // the legacy path and re-migrate it. publish_state handles the pre-existing Stored
        // row via its RowAbsent conditional write, so the write succeeds without error.
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x42u8; 150];
        let hash = lore_storage::hash_slice(&content);
        fake.set_state(hash, FragmentState::Stored);
        // put_object_once: visible to get_object (load) but not head_object (head_fragment)
        fake.put_object_once(hash, &content);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            },
        );
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::Maintained
        );
        assert_eq!(stats.state_with_no_head.load(Ordering::Relaxed), 1);
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
    }

    #[tokio::test]
    async fn skips_obliterated() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x42u8; 100];
        let hash = lore_storage::hash_slice(&content);
        fake.put_object_without_metadata(hash, &content);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: FragmentFlags::PayloadObliterated.bits(),
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            },
        );
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::SkippedObliterated
        );
    }

    #[tokio::test]
    async fn maintains_uncompressed_writes_state() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x33u8; 150];
        let hash = lore_storage::hash_slice(&content);
        fake.put_object_without_metadata(hash, &content);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            },
        );
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::Maintained
        );
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
    }

    #[tokio::test]
    async fn maintains_zstd_writes_state() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x44u8; 500];
        let (frag, compressed, hash) = make_zstd_payload(&content);
        fake.put_object_without_metadata(hash, &compressed);
        fake.set_legacy_metadata_row(hash, frag);
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::Maintained
        );
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
    }

    #[tokio::test]
    async fn maintains_lz4_writes_state() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x55u8; 500];
        let (frag, compressed, hash) = make_lz4_payload(&content);
        fake.put_object_without_metadata(hash, &compressed);
        fake.set_legacy_metadata_row(hash, frag);
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::Maintained
        );
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
    }

    #[tokio::test]
    async fn could_not_deduce_irrecoverable_payload() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let garbage = vec![0xFFu8; 64];
        let hash: Hash = rand::random(); // hash doesn't match garbage
        fake.put_object_without_metadata(hash, &garbage);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: FragmentFlags::PayloadCompressedZstd.bits(),
                size_payload: garbage.len() as u32,
                size_content: 200,
            },
        );
        let stats = Arc::new(RewriteStats::default());
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::CouldNotDeducePayload
        );
    }

    #[tokio::test]
    async fn skips_malicious_when_size_content_exceeds_threshold() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x42u8; 64];
        let hash: Hash = lore_storage::hash_slice(&content);
        fake.put_object_without_metadata(hash, &content);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: FragmentFlags::PayloadCompressedZstd.bits(),
                size_payload: content.len() as u32,
                size_content: lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 + 1,
            },
        );
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::SkippedMaliciousFragment
        );
    }

    /// Do not decompress a fragment that is fragmented, as a malicious one
    /// could be magnitudes larger than what is enforced on ingress.
    /// This is needed as some fragments exist before stricter ingress was enforced
    #[tokio::test]
    async fn skips_malicious_when_compressed_and_fragmented() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x99u8; 500];
        let (_, compressed, hash) = make_zstd_payload(&content);
        fake.put_object_without_metadata(hash, &compressed);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: FragmentFlags::PayloadCompressedZstd.bits()
                    | FragmentFlags::PayloadFragmented.bits(),
                size_payload: compressed.len() as u32,
                size_content: lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 + 1,
            },
        );
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::SkippedMaliciousFragment
        );
        assert_eq!(fake.state_of(hash), None);
    }

    /// A fragment list is addressed by the hash of the references it holds, so an uncompressed
    /// fragmented payload verifies against its own bytes and migrates like any other.
    #[tokio::test]
    async fn fragmented_alone_is_migrated() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let references = vec![0x0Fu8; 256];
        let hash = lore_storage::hash_slice(&references);
        fake.put_object_without_metadata(hash, &references);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: FragmentFlags::PayloadFragmented.bits(),
                size_payload: references.len() as u32,
                size_content: lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 + 1,
            },
        );
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::Maintained
        );
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));

        let stored = fake
            .stored_fragment(hash)
            .expect("a maintained fragment is re-uploaded to carry its metadata headers");
        assert_ne!(stored.flags & FragmentFlags::PayloadFragmented, 0);
        assert_eq!(stored.flags & FragmentFlags::PayloadCompressed, 0);
        assert_eq!(stored.size_payload as usize, references.len());
        assert_eq!(
            stored.size_content,
            lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 + 1
        );
    }

    #[tokio::test]
    async fn deduced_codec_increments_payloads_deduced_stat() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x66u8; 500];
        let (mut frag, compressed, hash) = make_zstd_payload(&content);
        // Lie: claim LZ4 when actually Zstd
        frag.flags = (frag.flags & !FragmentFlags::PayloadCompressed)
            | FragmentFlags::PayloadCompressedLZ4.bits();
        fake.put_object_without_metadata(hash, &compressed);
        fake.set_legacy_metadata_row(hash, frag);
        let stats = RewriteStats::default();
        migrator.process_fragment(hash, &stats).await.unwrap();
        assert_eq!(stats.payloads_deduced.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn dry_run_skips_write_payload_and_state() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, true).await;
        let content = vec![0x33u8; 150];
        let hash = lore_storage::hash_slice(&content);
        fake.put_object_without_metadata(hash, &content);
        fake.set_legacy_metadata_row(
            hash,
            Fragment {
                flags: 0,
                size_payload: content.len() as u32,
                size_content: content.len() as u64,
            },
        );
        let stats = RewriteStats::default();
        // Outcome is still reported correctly — only the write is suppressed.
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::Maintained
        );
        assert_eq!(fake.state_of(hash), None, "dry_run must not write state");
    }

    #[tokio::test]
    async fn mismatch_recompresses_to_zstd_and_writes_state() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x77u8; 500];
        let (mut frag, compressed, hash) = make_zstd_payload(&content);
        // Declare LZ4 but store Zstd bytes — a codec mismatch.
        frag.flags = (frag.flags & !FragmentFlags::PayloadCompressed)
            | FragmentFlags::PayloadCompressedLZ4.bits();
        fake.put_object_without_metadata(hash, &compressed);
        fake.set_legacy_metadata_row(hash, frag);
        let stats = RewriteStats::default();
        assert_eq!(
            migrator.process_fragment(hash, &stats).await.unwrap(),
            ConvertOutcome::RecompressedMismatch
        );
        assert_eq!(stats.payloads_deduced.load(Ordering::Relaxed), 1);
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
    }
}

mod fragment_stream_consumer_tests {
    use super::*;

    #[tokio::test]
    async fn stops_cleanly_when_channel_closed() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let (_, rx) = mpsc::channel::<Hash>(10);
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        migrator
            .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn converts_fragment_and_writes_state() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let content = vec![0x77u8; 600];
        let (frag, compressed, hash) = make_zstd_payload(&content);
        fake.put_object_without_metadata(hash, &compressed);
        fake.set_legacy_metadata_row(hash, frag);
        let (tx, rx) = mpsc::channel(10);
        tx.send(hash).await.unwrap();
        drop(tx);
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        migrator
            .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
            .await
            .unwrap();
        assert_eq!(stats.maintained.load(Ordering::Relaxed), 1);
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
    }

    #[tokio::test]
    async fn stats_accumulate_across_multiple_outcomes() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;

        // already migrated: run process_fragment once so both state and a
        // properly-headered S3 object exist, making head_fragment succeed on the second pass.
        let migrated_content = vec![0x10u8; 100];
        let migrated = lore_storage::hash_slice(&migrated_content);
        fake.put_object_without_metadata(migrated, &migrated_content);
        fake.set_legacy_metadata_row(
            migrated,
            Fragment {
                flags: 0,
                size_payload: migrated_content.len() as u32,
                size_content: migrated_content.len() as u64,
            },
        );
        migrator
            .process_fragment(migrated, &RewriteStats::default())
            .await
            .unwrap();

        // obliterated
        let obl_content = vec![0x20u8; 100];
        let obl_hash = lore_storage::hash_slice(&obl_content);
        fake.put_object_without_metadata(obl_hash, &obl_content);
        fake.set_legacy_metadata_row(
            obl_hash,
            Fragment {
                flags: FragmentFlags::PayloadObliterated.bits(),
                size_payload: obl_content.len() as u32,
                size_content: obl_content.len() as u64,
            },
        );

        // uncompressed conversion
        let unc = vec![0x30u8; 150];
        let unc_hash = lore_storage::hash_slice(&unc);
        fake.put_object_without_metadata(unc_hash, &unc);
        fake.set_legacy_metadata_row(
            unc_hash,
            Fragment {
                flags: 0,
                size_payload: unc.len() as u32,
                size_content: unc.len() as u64,
            },
        );

        // zstd — already correct codec, maintained in place
        let zstd_content = vec![0x40u8; 500];
        let (zstd_frag, zstd_compressed, zstd_hash) = make_zstd_payload(&zstd_content);
        fake.put_object_without_metadata(zstd_hash, &zstd_compressed);
        fake.set_legacy_metadata_row(zstd_hash, zstd_frag);

        // lz4 — accurate codec, maintained in place
        let lz4_content = vec![0x50u8; 500];
        let (lz4_frag, lz4_compressed, lz4_hash) = make_lz4_payload(&lz4_content);
        fake.put_object_without_metadata(lz4_hash, &lz4_compressed);
        fake.set_legacy_metadata_row(lz4_hash, lz4_frag);

        // compressed and fragmented at once — no valid fragment carries both
        let malicious_content = vec![0x60u8; 500];
        let (malicious_frag, malicious_compressed, malicious_hash) =
            make_zstd_payload(&malicious_content);
        fake.put_object_without_metadata(malicious_hash, &malicious_compressed);
        fake.set_legacy_metadata_row(
            malicious_hash,
            Fragment {
                flags: malicious_frag.flags | FragmentFlags::PayloadFragmented.bits(),
                ..malicious_frag
            },
        );

        let (tx, rx) = mpsc::channel(10);
        for h in [
            migrated,
            obl_hash,
            unc_hash,
            zstd_hash,
            lz4_hash,
            malicious_hash,
        ] {
            tx.send(h).await.unwrap();
        }
        drop(tx);

        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        migrator
            .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
            .await
            .unwrap();

        assert_eq!(stats.skipped_migrated.load(Ordering::Relaxed), 1);
        assert_eq!(stats.skipped_obliterated.load(Ordering::Relaxed), 1);
        assert_eq!(stats.skipped_malicious.load(Ordering::Relaxed), 1);
        // uncompressed, zstd, and lz4 fragments all have accurate codecs: maintained in place
        assert_eq!(stats.maintained.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn dry_run_error_continues_loop_without_stopping() {
        // One fragment has no S3 object → load() returns an error.
        // One fragment is valid and uncompressed.
        // In dry_run mode the consumer must process both rather than aborting on the error.
        let fake = Fake::default();
        let migrator = make_migrator(&fake, true).await;

        // Fragment that will error: metadata row exists but no S3 object.
        let error_content = vec![0x01u8; 100];
        let (error_frag, _compressed, error_hash) = make_zstd_payload(&error_content);
        fake.set_legacy_metadata_row(error_hash, error_frag);

        // Fragment that succeeds: uncompressed, valid.
        let ok_content = vec![0x02u8; 100];
        let ok_hash = lore_storage::hash_slice(&ok_content);
        fake.put_object_without_metadata(ok_hash, &ok_content);
        fake.set_legacy_metadata_row(
            ok_hash,
            Fragment {
                flags: 0,
                size_payload: ok_content.len() as u32,
                size_content: ok_content.len() as u64,
            },
        );

        let (tx, rx) = mpsc::channel(10);
        tx.send(error_hash).await.unwrap();
        tx.send(ok_hash).await.unwrap();
        drop(tx);

        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        // Should complete without returning an error despite the first fragment failing.
        migrator
            .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
            .await
            .unwrap();

        assert_eq!(stats.errored.load(Ordering::Relaxed), 1);
        assert_eq!(stats.maintained.load(Ordering::Relaxed), 1);
        // dry_run: neither fragment should have been written to state.
        assert_eq!(fake.state_of(ok_hash), None);
    }

    #[tokio::test]
    async fn aborted_flag_stops_consumer_without_processing() {
        let fake = Fake::default();
        let migrator = make_migrator(&fake, false).await;
        let (tx, rx) = mpsc::channel::<Hash>(10);
        for _ in 0..5 {
            tx.send(rand::random()).await.unwrap();
        }
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(true));
        migrator
            .fragment_stream_consumer(Arc::new(Mutex::new(rx)), stats.clone(), aborted)
            .await
            .unwrap();
        assert_eq!(stats.maintained.load(Ordering::Relaxed), 0);
        assert_eq!(stats.recompressed_mismatch.load(Ordering::Relaxed), 0);
    }
}

mod run_migrator_tests {
    use lore_aws::dynamodb::MockDynamoDb;
    use lore_aws::dynamodb::ScanPage;

    use super::*;

    async fn make_config(fake: &Fake, dynamodb: MockDynamoDb) -> MetadataMigratorConfig {
        MetadataMigratorConfig {
            dynamodb,
            store: store_with_separate_metadata_table(fake).await,
            metadata_table_name: FRAGMENT_METADATA_TABLE_NAME.into(),
            api_call_max_retries: 0,
            api_retry_base_delay: Duration::ZERO,
            scan_config: ScanConfig::default(),
            is_dry_run: false,
        }
    }

    #[tokio::test]
    async fn empty_scan_returns_true() {
        let fake = Fake::default();
        let mut dynamodb = MockDynamoDb::default();
        dynamodb.expect_scan_page().returning(|_, _, _| {
            Ok(ScanPage {
                items: vec![],
                last_evaluated_key: None,
            })
        });
        let config = make_config(&fake, dynamodb).await;
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        assert!(
            run_migrator(
                config,
                OrchestrationConfig { num_consumers: 2 },
                stats,
                aborted
            )
            .await
        );
    }

    #[tokio::test]
    async fn pre_aborted_returns_false() {
        let fake = Fake::default();
        let config = make_config(&fake, MockDynamoDb::default()).await;
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(true));
        assert!(
            !run_migrator(
                config,
                OrchestrationConfig { num_consumers: 2 },
                stats,
                aborted
            )
            .await
        );
    }

    #[tokio::test]
    async fn fragments_are_migrated_and_stats_updated() {
        let fake = Fake::default();

        let content = vec![0x42u8; 500];
        let (frag, compressed, hash) = make_zstd_payload(&content);
        fake.put_object_without_metadata(hash, &compressed);
        fake.set_legacy_metadata_row(hash, frag);

        let mut item = Item::new();
        item.insert("hash", AttributeValue::blob(hash.data()));
        let mut dynamodb = MockDynamoDb::default();
        dynamodb
            .expect_scan_page()
            .returning(move |_, start_key, _| {
                if start_key.is_none() {
                    Ok(ScanPage {
                        items: vec![item.clone()],
                        last_evaluated_key: None,
                    })
                } else {
                    Ok(ScanPage {
                        items: vec![],
                        last_evaluated_key: None,
                    })
                }
            });

        let config = make_config(&fake, dynamodb).await;
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));

        assert!(
            run_migrator(
                config,
                OrchestrationConfig { num_consumers: 2 },
                stats.clone(),
                aborted,
            )
            .await
        );

        assert_eq!(stats.maintained.load(Ordering::Relaxed), 1);
        assert_eq!(fake.state_of(hash), Some(FragmentState::Stored));
    }

    #[tokio::test]
    async fn consumer_error_returns_false() {
        let fake = Fake::default();

        let content = vec![0x42u8; 500];
        let (frag, _compressed, hash) = make_zstd_payload(&content);
        // Set up the metadata row so discovery returns the hash, but omit the S3 object.
        // load() will hit NoSuchKey → StoreError::AddressNotFound → consumer error.
        fake.set_legacy_metadata_row(hash, frag);

        let mut item = Item::new();
        item.insert("hash", AttributeValue::blob(hash.data()));
        let mut dynamodb = MockDynamoDb::default();
        dynamodb
            .expect_scan_page()
            .returning(move |_, start_key, _| {
                if start_key.is_none() {
                    Ok(ScanPage {
                        items: vec![item.clone()],
                        last_evaluated_key: None,
                    })
                } else {
                    Ok(ScanPage {
                        items: vec![],
                        last_evaluated_key: None,
                    })
                }
            });

        let config = make_config(&fake, dynamodb).await;
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));

        assert!(
            !run_migrator(
                config,
                OrchestrationConfig { num_consumers: 2 },
                stats.clone(),
                aborted,
            )
            .await
        );

        assert_eq!(stats.errored.load(Ordering::Relaxed), 1);
    }

    /// A scan that keeps failing leaves most of the table unread, so the segment did not
    /// complete — however cleanly the consumers that had nothing to do finished.
    #[tokio::test]
    async fn a_run_whose_scan_gave_up_reports_the_segment_incomplete() {
        let fake = Fake::default();
        let mut dynamodb = MockDynamoDb::default();
        dynamodb
            .expect_scan_page()
            .returning(|_, _, _| Err(crate::store::fake::throughput_exceeded()));

        let config = make_config(&fake, dynamodb).await;

        assert!(
            !run_migrator(
                config,
                OrchestrationConfig { num_consumers: 2 },
                Arc::new(RewriteStats::default()),
                Arc::new(AtomicBool::new(false)),
            )
            .await,
            "a segment whose scan gave up did not complete"
        );
    }

    /// Discovery hands hashes over a bounded channel, so a page wider than that channel leaves
    /// it waiting to send. Every consumer here fails on its first fragment and stops, which is
    /// also the shape an interrupted run ends in: the run has to notice they have gone rather
    /// than wait on them.
    #[tokio::test]
    async fn a_run_whose_consumers_have_all_stopped_ends_rather_than_waiting_on_them() {
        let fake = Fake::default();

        // Metadata rows with no S3 object behind them, so every fragment fails to load.
        let items: Vec<_> = std::iter::repeat_with(|| {
            let hash: Hash = rand::random();
            let mut item = Item::new();
            item.insert("hash", AttributeValue::blob(hash.data()));
            item
        })
        .take(64)
        .collect();

        let mut dynamodb = MockDynamoDb::default();
        dynamodb
            .expect_scan_page()
            .returning(move |_, start_key, _| {
                if start_key.is_none() {
                    Ok(ScanPage {
                        items: items.clone(),
                        last_evaluated_key: None,
                    })
                } else {
                    Ok(ScanPage {
                        items: vec![],
                        last_evaluated_key: None,
                    })
                }
            });

        let config = make_config(&fake, dynamodb).await;
        let run = run_migrator(
            config,
            OrchestrationConfig { num_consumers: 2 },
            Arc::new(RewriteStats::default()),
            Arc::new(AtomicBool::new(false)),
        );

        assert!(
            !tokio::time::timeout(Duration::from_secs(30), run)
                .await
                .expect("the run should end rather than wait on a channel nothing drains"),
            "a run every consumer stopped in did not complete"
        );
    }

    #[tokio::test]
    async fn multiple_consumers_empty_scan_returns_true() {
        let fake = Fake::default();
        let mut dynamodb = MockDynamoDb::default();
        dynamodb.expect_scan_page().returning(|_, _, _| {
            Ok(ScanPage {
                items: vec![],
                last_evaluated_key: None,
            })
        });
        let config = make_config(&fake, dynamodb).await;
        let stats = Arc::new(RewriteStats::default());
        let aborted = Arc::new(AtomicBool::new(false));
        assert!(
            run_migrator(
                config,
                OrchestrationConfig { num_consumers: 4 },
                stats,
                aborted
            )
            .await
        );
    }
}
