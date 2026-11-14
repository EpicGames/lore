// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_storage::Fragment;
use lore_storage::FragmentFlags;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::immutable_store::*;
use zerocopy::IntoBytes;

fn make_fragment(size_payload: u32) -> Fragment {
    Fragment {
        flags: 0,
        size_payload,
        size_content: size_payload as u64,
    }
}

#[test]
fn validate_size_accepts_zero() {
    assert!(validate_fragment_size(&make_fragment(0)).is_ok());
}

#[test]
fn validate_size_accepts_exact_threshold() {
    let fragment = make_fragment(lore_storage::FRAGMENT_SIZE_THRESHOLD as u32);
    assert!(validate_fragment_size(&fragment).is_ok());
}

#[test]
fn validate_size_rejects_over_threshold() {
    let fragment = make_fragment(lore_storage::FRAGMENT_SIZE_THRESHOLD as u32 + 1);
    let err = validate_fragment_size(&fragment).expect_err("should reject oversize");
    assert!(matches!(err, StoreError::Oversized(_)));
}

#[test]
fn validate_size_rejects_oversized_unfragmented_content() {
    // size_payload within bounds but size_content over threshold: must be caught to
    // prevent downstream callers (e.g. decompress) from pre-allocating a huge buffer.
    let fragment = Fragment {
        flags: 0,
        size_payload: 128,
        size_content: lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 + 1,
    };
    let err = validate_fragment_size(&fragment).expect_err("should reject oversize content");
    assert!(matches!(err, StoreError::Oversized(_)));
}

#[test]
fn validate_payload_accepts_matching() {
    let fragment = make_fragment(128);
    assert!(validate_fragment_payload(&fragment, 128).is_ok());
}

#[test]
fn validate_payload_rejects_length_mismatch() {
    let fragment = make_fragment(128);
    let err = validate_fragment_payload(&fragment, 127).expect_err("should reject mismatch");
    assert!(matches!(err, StoreError::Internal(_)));
}

#[test]
fn validate_payload_rejects_oversize_before_mismatch() {
    // Oversize must be reported even when the buffer length also doesn't match,
    // because the oversize check happens first.
    let fragment = make_fragment(lore_storage::FRAGMENT_SIZE_THRESHOLD as u32 + 1);
    let err = validate_fragment_payload(&fragment, 0).expect_err("should reject oversize");
    assert!(matches!(err, StoreError::Oversized(_)));
}

mod validate_metadata {
    use super::*;

    /// The inlined check passes exactly the fragments every check passes, over every combination
    /// of the ten low flag bits and sizes around each bound, and each other bit alone.
    #[test]
    fn the_quick_check_agrees_with_every_check() {
        let threshold = lore_storage::FRAGMENT_SIZE_THRESHOLD as u64;
        let reference = size_of::<FragmentReference>() as u64;
        let payloads = [
            0,
            1,
            reference,
            100,
            threshold - 1,
            threshold,
            threshold + 1,
        ];
        let contents = [0, 1, 99, 100, 101, threshold, threshold + 1, 2 * threshold];
        let flags = (0u32..1 << 10).chain((10..32).map(|bit| 1u32 << bit));
        for flags in flags {
            for &size_payload in &payloads {
                for &size_content in &contents {
                    let fragment = Fragment {
                        flags,
                        size_payload: size_payload as u32,
                        size_content,
                    };
                    assert_eq!(
                        fragment_metadata_is_valid(&fragment),
                        refuse_fragment_metadata(&fragment).is_ok(),
                        "{fragment:?}"
                    );
                    let lengths = [
                        None,
                        Some(size_payload as usize),
                        Some(size_payload as usize + 1),
                        Some((size_payload as usize).saturating_sub(1)),
                    ];
                    for payload_len in lengths {
                        assert_eq!(
                            put_is_valid(&fragment, payload_len),
                            refuse_put(&fragment, payload_len, true).is_ok(),
                            "{fragment:?} with a payload of {payload_len:?} bytes"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn accepts_uncompressed_unfragmented() {
        assert!(validate_fragment_metadata(&make_fragment(128)).is_ok());
    }

    #[test]
    fn rejects_zero_size_payload() {
        let err = validate_fragment_metadata(&make_fragment(0)).expect_err("size_payload=0");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_oversize_payload() {
        let fragment = make_fragment(lore_storage::FRAGMENT_SIZE_THRESHOLD as u32 + 1);
        let err = validate_fragment_metadata(&fragment).expect_err("oversize");
        assert!(matches!(err, StoreError::Oversized(_)));
    }

    #[test]
    fn rejects_size_payload_greater_than_size_content() {
        let fragment = Fragment {
            flags: 0,
            size_payload: 100,
            size_content: 50,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("payload > content");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_unknown_flag_bits() {
        let fragment = Fragment {
            flags: 1 << 31,
            size_payload: 128,
            size_content: 128,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("unknown flags");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_multiple_compression_flags() {
        let fragment = Fragment {
            flags: (FragmentFlags::PayloadCompressedLZ4 | FragmentFlags::PayloadCompressedZstd)
                .into(),
            size_payload: 100,
            size_content: 200,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("multi compression");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_reserved_compression_bit() {
        // bit 4 is inside PayloadCompressed mask but not a defined compressor
        let fragment = Fragment {
            flags: 1 << 4,
            size_payload: 100,
            size_content: 200,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("reserved compression bit");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_obliterated_flag_on_ingress() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadObliterated.into(),
            size_payload: 128,
            size_content: 128,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("obliterated");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_do_not_replicate_flag_on_ingress() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadDoNotReplicate.into(),
            size_payload: 128,
            size_content: 128,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("do_not_replicate");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn accepts_local_cache_priority_on_ingress() {
        // PayloadLocalCachePriority is a client-set write hint that must
        // persist through the storage system; it must not be rejected at
        // validation.
        let fragment = Fragment {
            flags: FragmentFlags::PayloadLocalCachePriority.into(),
            size_payload: 128,
            size_content: 128,
        };
        assert!(validate_fragment_metadata(&fragment).is_ok());
    }

    #[test]
    fn accepts_payload_stored_flags() {
        // PayloadStored* is set by peers during replication and cleared by the
        // Put handler; it must not be rejected at validation.
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredDurable.into(),
            size_payload: 128,
            size_content: 128,
        };
        assert!(validate_fragment_metadata(&fragment).is_ok());
    }

    #[test]
    fn rejects_compressed_and_fragmented_combo() {
        let fragment = Fragment {
            flags: (FragmentFlags::PayloadCompressedLZ4 | FragmentFlags::PayloadFragmented).into(),
            size_payload: 80,
            size_content: 200,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("compressed+fragmented");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_uncompressed_unfragmented_size_mismatch() {
        let fragment = Fragment {
            flags: 0,
            size_payload: 100,
            size_content: 200,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("size mismatch");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn accepts_compressed_with_shrinking_content() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadCompressedLZ4.into(),
            size_payload: 100,
            size_content: 200,
        };
        assert!(validate_fragment_metadata(&fragment).is_ok());
    }

    #[test]
    fn rejects_compressed_with_oversize_content() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadCompressedLZ4.into(),
            size_payload: 100,
            size_content: lore_storage::FRAGMENT_SIZE_THRESHOLD as u64 + 1,
        };
        let err = validate_fragment_metadata(&fragment).expect_err("oversize content");
        assert!(matches!(err, StoreError::Oversized(_)));
    }

    #[test]
    fn accepts_fragmented_with_large_content() {
        // Fragmented fragments address total file content that can far exceed
        // FRAGMENT_SIZE_THRESHOLD; size_content is only bounded for non-fragmented
        // fragments (where it drives the decompress allocation).
        let fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.into(),
            size_payload: 80,
            size_content: 10 * 1024 * 1024 * 1024, // 10 GiB
        };
        assert!(validate_fragment_metadata(&fragment).is_ok());
    }
}

mod sanitise_behavior_flags {
    use super::*;

    mod do_not_replicate {
        use super::*;

        #[test]
        fn strips_and_returns_true() {
            let mut fragment = make_fragment(128);
            fragment.flags |= FragmentFlags::PayloadDoNotReplicate;

            let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

            assert!(behaviour.do_not_replicate);
            assert_eq!(fragment.flags & FragmentFlags::PayloadDoNotReplicate, 0);
        }

        #[test]
        fn returns_false_when_flag_absent() {
            let mut fragment = make_fragment(128);

            let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

            assert!(!behaviour.do_not_replicate);
            assert_eq!(fragment.flags, 0);
        }
    }

    #[test]
    fn preserves_other_flags() {
        let mut fragment = make_fragment(128);
        fragment.flags |=
            FragmentFlags::PayloadStoredDurable | FragmentFlags::PayloadDoNotReplicate;

        let behaviour = sanitise_fragment_behavior_flags(&mut fragment);

        assert!(behaviour.do_not_replicate);
        assert_ne!(fragment.flags & FragmentFlags::PayloadStoredDurable, 0);
        assert_eq!(fragment.flags & FragmentFlags::PayloadDoNotReplicate, 0);
    }
}

mod validate_list {
    use super::*;

    fn make_refs_payload(refs: &[FragmentReference]) -> Bytes {
        Bytes::copy_from_slice(refs.as_bytes())
    }

    fn make_fragmented(refs_len: usize, size_content: u64) -> Fragment {
        Fragment {
            flags: FragmentFlags::PayloadFragmented.into(),
            size_payload: (refs_len * std::mem::size_of::<FragmentReference>()) as u32,
            size_content,
        }
    }

    #[test]
    fn accepts_well_formed_list() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 1000,
            },
        ];
        let fragment = make_fragmented(refs.len(), 2000);
        let payload = make_refs_payload(&refs);
        assert!(validate_fragment_list(&fragment, &payload).is_ok());
    }

    #[test]
    fn rejects_non_multiple_of_ref_size() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.into(),
            size_payload: 41,
            size_content: 1000,
        };
        let payload = Bytes::from(vec![0u8; 41]);
        let err = validate_fragment_list(&fragment, &payload).expect_err("non-multiple");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_payload_length_mismatch() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
        ];
        let fragment = make_fragmented(refs.len(), 1000);
        // Report payload bytes but declare more payload via size_payload
        let short_payload = Bytes::from(vec![0u8; fragment.size_payload as usize - 40]);
        let err =
            validate_fragment_list(&fragment, &short_payload).expect_err("payload len mismatch");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_fewer_than_two_refs() {
        let refs = [FragmentReference {
            hash: Hash::from_u64(1),
            offset_content: 0,
        }];
        let fragment = make_fragmented(refs.len(), 1000);
        let payload = make_refs_payload(&refs);
        let err = validate_fragment_list(&fragment, &payload).expect_err("<2 refs");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_non_increasing_offsets() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 1000,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
        ];
        let fragment = make_fragmented(refs.len(), 2000);
        let payload = make_refs_payload(&refs);
        let err = validate_fragment_list(&fragment, &payload).expect_err("non-increasing");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_equal_offsets() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
        ];
        let fragment = make_fragmented(refs.len(), 2000);
        let payload = make_refs_payload(&refs);
        let err = validate_fragment_list(&fragment, &payload).expect_err("equal offsets");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_last_offset_at_content_end() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 0,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 2000,
            },
        ];
        let fragment = make_fragmented(refs.len(), 2000);
        let payload = make_refs_payload(&refs);
        let err = validate_fragment_list(&fragment, &payload).expect_err("last at end");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_last_offset_beyond_content_end() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 500,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 3000,
            },
        ];
        let fragment = make_fragmented(refs.len(), 2000);
        let payload = make_refs_payload(&refs);
        let err = validate_fragment_list(&fragment, &payload).expect_err("last beyond");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn rejects_content_end_overflow() {
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: u64::MAX - 10,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: u64::MAX,
            },
        ];
        let fragment = make_fragmented(refs.len(), 100);
        let payload = make_refs_payload(&refs);
        let err = validate_fragment_list(&fragment, &payload).expect_err("overflow");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    #[test]
    fn accepts_non_zero_first_offset() {
        // Interior/child-list case
        let refs = [
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 10_000_000,
            },
            FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: 10_500_000,
            },
        ];
        let fragment = make_fragmented(refs.len(), 1_000_000);
        let payload = make_refs_payload(&refs);
        assert!(validate_fragment_list(&fragment, &payload).is_ok());
    }

    fn data(offset_content: u64) -> FragmentReference {
        FragmentReference {
            hash: Hash::from_u64(offset_content + 1),
            offset_content,
        }
    }

    fn hole(offset_content: u64) -> FragmentReference {
        FragmentReference {
            hash: Hash::default(),
            offset_content,
        }
    }

    fn check(refs: &[FragmentReference], size_content: u64) -> Result<(), StoreError> {
        validate_fragment_list(
            &make_fragmented(refs.len(), size_content),
            &make_refs_payload(refs),
        )
    }

    /// A hole may span far more than a fragment, between two leaves or at either end.
    #[test]
    fn accepts_holes_between_and_around_leaves() {
        let gib = 1u64 << 30;
        check(&[data(0), hole(4096), data(gib)], gib + 4096).expect("a hole between leaves");
        check(&[hole(0), data(gib), hole(gib + 4096)], 2 * gib).expect("holes at either end");
    }

    /// Content that is one hole is a list of that one hole, while one leaf alone is no list.
    #[test]
    fn accepts_one_hole_and_refuses_one_leaf() {
        check(&[hole(0)], 1 << 40).expect("a list of one hole");
        let err = check(&[data(0)], 4096).expect_err("a list of one leaf");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    /// Two holes in a row are one hole, so a list naming two is not the one way to name it.
    #[test]
    fn rejects_a_hole_following_a_hole() {
        let err = check(&[data(0), hole(4096), hole(8192)], 1 << 20).expect_err("hole after hole");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    /// A list holding a hole is a list of leaves, so an entry spanning more than a fragment
    /// beside one names a list where a leaf belongs.
    #[test]
    fn rejects_an_entry_larger_than_a_fragment_beside_a_hole() {
        let fragment_size = lore_storage::FRAGMENT_SIZE_THRESHOLD as u64;
        check(&[data(0), hole(fragment_size)], 2 * fragment_size).expect("a leaf-sized entry");
        let err = check(&[data(0), hole(fragment_size + 1)], 2 * fragment_size)
            .expect_err("an entry larger than a fragment");
        assert!(matches!(err, StoreError::Internal(_)));
    }

    /// The span of every entry beside a hole is checked, before the first hole and after the last
    /// alike, while a list without holes may name entries of any size.
    #[test]
    fn checks_the_span_of_every_entry_in_a_list_with_a_hole() {
        let fragment_size = lore_storage::FRAGMENT_SIZE_THRESHOLD as u64;
        let big = fragment_size + 1;
        check(&[data(0), data(big)], 2 * big).expect("a list of lists without holes");
        for (refs, size) in [
            (vec![data(0), data(big), hole(2 * big)], 3 * big),
            (vec![hole(0), data(4096)], 4096 + big),
            (
                vec![data(0), hole(4096), data(8192), data(8192 + big)],
                8192 + 2 * big,
            ),
        ] {
            let err = check(&refs, size).expect_err("an entry larger than a fragment");
            assert!(matches!(err, StoreError::Internal(_)), "{refs:?}");
        }
    }
}
