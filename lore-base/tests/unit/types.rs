// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::str::FromStr;

use lore_base::types::*;
use serde::Deserialize;
use serde::Serialize;
use zerocopy::IntoBytes;

#[test]
fn hash_hex_roundtrip() {
    let hash = Hash::from([
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
        0xcd, 0xef,
    ]);
    let s = hash.to_string();
    assert_eq!(s.len(), HASH_STRING_LENGTH);
    let parsed = Hash::from_str(&s).unwrap();
    assert_eq!(hash, parsed);
}

#[test]
fn context_hex_roundtrip() {
    let ctx = Context::from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    let s = ctx.to_string();
    let parsed = Context::from_str(&s).unwrap();
    assert_eq!(ctx, parsed);
}

#[test]
fn partition_hex_roundtrip() {
    let p = Partition::from([0xaa; 16]);
    let s = p.to_string();
    let parsed = Partition::from_str(&s).unwrap();
    assert_eq!(p, parsed);
}

#[test]
fn address_display_fromstr_roundtrip() {
    let addr = Address {
        hash: Hash::from([0x42; 32]),
        context: Context::from([0x13; 16]),
    };
    let s = addr.to_string();
    let parsed = Address::from_str(&s).unwrap();
    assert_eq!(addr, parsed);
}

#[test]
fn hash_serde_json_roundtrip() {
    let hash = Hash::from([0xab; 32]);
    let json = serde_json::to_string(&hash).unwrap();
    let parsed: Hash = serde_json::from_str(&json).unwrap();
    assert_eq!(hash, parsed);
}

#[test]
fn context_serde_json_roundtrip() {
    let ctx = Context::from([0xcd; 16]);
    let json = serde_json::to_string(&ctx).unwrap();
    let parsed: Context = serde_json::from_str(&json).unwrap();
    assert_eq!(ctx, parsed);
}

#[test]
fn partition_serde_json_roundtrip() {
    let p = Partition::from([0xef; 16]);
    let json = serde_json::to_string(&p).unwrap();
    let parsed: Partition = serde_json::from_str(&json).unwrap();
    assert_eq!(p, parsed);
}

#[test]
fn address_serde_json_roundtrip() {
    let addr = Address {
        hash: Hash::from([0x11; 32]),
        context: Context::from([0x22; 16]),
    };
    let json = serde_json::to_string(&addr).unwrap();
    let parsed: Address = serde_json::from_str(&json).unwrap();
    assert_eq!(addr, parsed);
}

/// JSON is where these identifiers are read by people and by other tools, so
/// the hex text is a compatibility guarantee, not an implementation detail.
/// A round-trip alone would not catch the encoding flipping to bytes, since
/// both directions would flip together.
#[test]
fn identifiers_serialize_as_hex_text_in_json() {
    assert_eq!(
        serde_json::to_string(&Hash::from([0xab; 32])).unwrap(),
        format!("\"{}\"", "ab".repeat(32))
    );
    assert_eq!(
        serde_json::to_string(&Context::from([0xcd; 16])).unwrap(),
        format!("\"{}\"", "cd".repeat(16))
    );
    assert_eq!(
        serde_json::to_string(&Partition::from([0xef; 16])).unwrap(),
        format!("\"{}\"", "ef".repeat(16))
    );
    let address = Address {
        hash: Hash::from([0x11; 32]),
        context: Context::from([0x22; 16]),
    };
    assert_eq!(
        serde_json::to_string(&address).unwrap(),
        format!("\"{}-{}\"", "11".repeat(32), "22".repeat(16))
    );
}

/// A non-self-describing format cannot be asked what a value is, so reading
/// one of these back used to fail outright: the deserializers called
/// `deserialize_any`, which bitcode rejects. Every command and record
/// carrying an identifier depends on this.
#[test]
fn identifiers_roundtrip_through_bitcode() {
    let hash = Hash::from([0xab; 32]);
    assert_eq!(
        bitcode::deserialize::<Hash>(&bitcode::serialize(&hash).unwrap()).unwrap(),
        hash
    );

    let context = Context::from([0xcd; 16]);
    assert_eq!(
        bitcode::deserialize::<Context>(&bitcode::serialize(&context).unwrap()).unwrap(),
        context
    );

    let partition = Partition::from([0xef; 16]);
    assert_eq!(
        bitcode::deserialize::<Partition>(&bitcode::serialize(&partition).unwrap()).unwrap(),
        partition
    );

    let address = Address {
        hash: Hash::from([0x11; 32]),
        context: Context::from([0x22; 16]),
    };
    assert_eq!(
        bitcode::deserialize::<Address>(&bitcode::serialize(&address).unwrap()).unwrap(),
        address
    );
}

/// An identifier nested inside a struct is the shape that actually crosses
/// the service wire, and it exercises the field attributes rather than the
/// bare type.
#[test]
fn a_struct_of_identifiers_roundtrips_through_bitcode() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        address: Address,
        partition: Partition,
        hashes: Vec<Hash>,
    }

    let record = Record {
        address: Address {
            hash: Hash::from([0x37; 32]),
            context: Context::from([0x73; 16]),
        },
        partition: Partition::from([0x01; 16]),
        hashes: vec![Hash::from([0u8; 32]), Hash::from([0xff; 32])],
    };
    let encoded = bitcode::serialize(&record).unwrap();
    assert_eq!(bitcode::deserialize::<Record>(&encoded).unwrap(), record);
}

/// A short buffer converts to the zero address, which is a meaningful value
/// elsewhere, so a truncated one must be refused rather than silently become
/// it.
#[test]
fn a_truncated_address_is_refused_rather_than_zeroed() {
    let address = Address {
        hash: Hash::from([0x11; 32]),
        context: Context::from([0x22; 16]),
    };
    let encoded = bitcode::serialize(&address.as_bytes()[..40].to_vec()).unwrap();
    let parsed = bitcode::deserialize::<Address>(&encoded);
    assert!(
        parsed.is_err(),
        "a 40-byte address must not deserialize, got {parsed:?}"
    );
}

#[test]
fn fragment_serde_json_roundtrip() {
    let frag = Fragment {
        flags: 0x1234,
        size_payload: 5678,
        size_content: 9012,
    };
    let json = serde_json::to_string(&frag).unwrap();
    let parsed: Fragment = serde_json::from_str(&json).unwrap();
    assert_eq!(frag, parsed);
}

#[test]
fn hash_is_zero() {
    assert!(Hash::default().is_zero());
    assert!(!Hash::from([1; 32]).is_zero());
}

#[test]
fn context_is_zero() {
    assert!(Context::default().is_zero());
    assert!(!Context::from([1; 16]).is_zero());
}

#[test]
fn partition_is_zero() {
    assert!(Partition::default().is_zero());
    assert!(!Partition::from([1; 16]).is_zero());
}

#[test]
fn address_is_zero() {
    assert!(Address::default().is_zero());
}

#[test]
fn hash_from_u64_roundtrip() {
    let val: u64 = 0xdeadbeef_cafebabe;
    let hash = Hash::from_u64(val);
    assert_eq!(hash.to_u64(), val);
}

#[test]
fn hash_context_roundtrip() {
    let ctx = Context::from([0x42; 16]);
    let hash = Hash::from_context(ctx);
    assert_eq!(hash.to_context(), ctx);
}

#[test]
fn partition_context_conversion() {
    let ctx = Context::from([0x55; 16]);
    let p: Partition = ctx.into();
    assert_eq!(p.data(), ctx.data());
    let ctx2: Context = p.into();
    assert_eq!(ctx, ctx2);
}

#[test]
fn context_uuid_roundtrip() {
    let ctx = Context::from([0x77; 16]);
    let uuid: uuid::Uuid = ctx.into();
    let ctx2: Context = (&uuid).into();
    assert_eq!(ctx, ctx2);
}

#[test]
fn fragment_reference_layout() {
    assert_eq!(
        std::mem::size_of::<FragmentReference>(),
        std::mem::size_of::<Hash>() + std::mem::size_of::<u64>(),
    );
}

#[test]
fn typed_bytes_count_and_slice() {
    use bytes::Bytes;
    let data: Vec<u32> = vec![1, 2, 3, 4];
    let vb = VecBytes(data);
    let bytes = Bytes::copy_from_slice(vb.as_ref());
    assert_eq!(bytes.count::<u32>(), 4);
    let slice = bytes.as_type_slice::<u32>();
    assert_eq!(slice, &[1, 2, 3, 4]);
}

#[test]
fn typed_bytes_mut_zeroed_count() {
    use bytes::BytesMut;
    let buf = BytesMut::zeroed_count::<u64>(3);
    assert_eq!(buf.len(), 3 * std::mem::size_of::<u64>());
    assert!(buf.iter().all(|&b| b == 0));
}

#[test]
fn vec_bytes_as_ref() {
    let v = VecBytes(vec![1u32, 2, 3]);
    let bytes: &[u8] = v.as_ref();
    assert_eq!(bytes.len(), 3 * std::mem::size_of::<u32>());
}

#[test]
fn hash_from_slice_and_as_bytes() {
    let test_hash = "0123456789abcdefabcdef09876543210123456789abcdefabcdef0987654321";
    let test_bytes: Vec<u8> = (0..test_hash.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&test_hash[i..i + 2], 16).unwrap())
        .collect();

    let h = Hash::from_str(test_hash).expect("Hash creation failed");
    assert_eq!(format!("{h}"), test_hash);
    let h2 = Hash::from(&test_bytes[..]);
    let h3 = Hash::from(h.as_bytes());
    assert_eq!(h, h2);
    assert_eq!(h, h3);
    assert_eq!(test_bytes, h.as_bytes());
}

#[test]
fn context_from_slice_and_as_bytes() {
    let test_context = "0123456789abcdefabcdef0987654321";
    let test_bytes: Vec<u8> = (0..test_context.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&test_context[i..i + 2], 16).unwrap())
        .collect();

    let h = Context::from_str(test_context).expect("Context creation failed");
    assert_eq!(format!("{h}"), test_context);
    let h2 = Context::from(&test_bytes[..]);
    let h3 = Context::from(h.as_bytes());
    assert_eq!(h, h2);
    assert_eq!(h, h3);
    assert_eq!(test_bytes, h.as_bytes());
}

#[test]
fn address_from_slice_and_as_bytes() {
    let test_context = "0123456789abccccabcdef0987654321";
    let test_hash = "0123456789abcdefddddef09876543210123456789abcdefabcdef0987654321";
    let test_addr = format!("{test_hash}-{test_context}");
    let test_str_bytes = format!("{test_hash}{test_context}");
    let test_bytes: Vec<u8> = (0..test_str_bytes.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&test_str_bytes[i..i + 2], 16).unwrap())
        .collect();

    let h = Address::from_str(&test_addr).expect("Address creation failed");
    assert_eq!(format!("{h}"), test_addr);
    let h2 = Address::from(&test_bytes[..]);
    assert_eq!(format!("{h2}"), test_addr);
    let h3 = Address::from(h.as_bytes());
    assert_eq!(h, h2);
    assert_eq!(h, h3);
    assert_eq!(test_bytes, h.as_bytes());
}

#[test]
fn hash_buffer_deterministic() {
    let hash = Hash::hash_buffer(b"test hash");
    assert_eq!(
        "622eeba4ec46cec1ba0fb55b988f48b88856a1cc3b3d0064074f798af0b88597",
        format!("{hash}")
    );
}

#[test]
fn formatting_debug_eq_display() {
    let hash = Hash::hash_buffer(b"test hash");
    let context =
        Context::from_str("0123456789abccccabcdef0987654321").expect("Context creation failed");
    let address = Address { hash, context };

    assert_eq!(format!("{hash}"), format!("{hash:?}"));
    assert_eq!(format!("{context}"), format!("{context:?}"));
    assert_eq!(format!("{address}"), format!("{address:?}"));
}

#[test]
fn hash_from_str_invalid_hex() {
    assert!(Hash::from_str("not_valid_hex").is_err());
}

#[test]
fn hash_from_str_wrong_length() {
    assert!(Hash::from_str("aabb").is_err());
}

#[test]
fn hash_from_str_empty() {
    assert!(Hash::from_str("").is_err());
}

#[test]
fn context_from_str_wrong_length() {
    assert!(Context::from_str("aabb").is_err());
}

#[test]
fn partition_from_str_invalid_hex() {
    assert!(Partition::from_str("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
}

#[test]
fn address_from_str_too_many_parts() {
    assert!(Address::from_str("aa-bb-cc").is_err());
}

#[test]
fn address_from_str_invalid_hash_part() {
    assert!(Address::from_str("not_hex").is_err());
}

#[test]
fn hash_from_short_slice_returns_default() {
    let short: &[u8] = &[1, 2, 3];
    let hash = Hash::from(short);
    assert!(hash.is_zero());
}

#[test]
fn context_from_short_slice_returns_default() {
    let short: &[u8] = &[1, 2];
    let ctx = Context::from(short);
    assert!(ctx.is_zero());
}

#[test]
fn typed_bytes_to_aligned_already_aligned() {
    use bytes::Bytes;
    let data = vec![1u64, 2, 3];
    let bytes = Bytes::copy_from_slice(VecBytes(data).as_ref());
    let aligned = bytes.clone().to_aligned::<u64>();
    assert_eq!(aligned, bytes);
}

#[test]
fn typed_bytes_to_aligned_empty() {
    use bytes::Bytes;
    let bytes = Bytes::new();
    let aligned = bytes.clone().to_aligned::<u64>();
    assert_eq!(aligned, bytes);
}

#[test]
fn zero_heap_alloc_produces_zeroed_fragment() {
    let boxed = Fragment::new_from_heap_zeroed();
    assert_eq!(boxed.flags, 0);
    assert_eq!(boxed.size_payload, 0);
    assert_eq!(boxed.size_content, 0);
}

#[test]
fn clone_heap_alloc_preserves_data() {
    let frag = Fragment {
        flags: 42,
        size_payload: 100,
        size_content: 200,
    };
    let boxed = frag.clone_on_heap();
    assert_eq!(*boxed, frag);
}

#[test]
fn partition_bytes_roundtrip() {
    use bytes::Bytes;
    let p = Partition::from([0xab; 16]);
    let bytes: Bytes = p.into();
    assert_eq!(bytes.len(), 16);
    let recovered = Partition::from(&bytes);
    assert_eq!(p, recovered);
}

#[test]
fn hash_bytes_roundtrip() {
    use bytes::Bytes;
    let h = Hash::from([0xcd; 32]);
    let bytes: Bytes = h.into();
    assert_eq!(bytes.len(), 32);
    let recovered = Hash::from(&bytes);
    assert_eq!(h, recovered);
}

#[test]
fn context_bytes_roundtrip() {
    use bytes::Bytes;
    let ctx = Context::from([0xef; 16]);
    let bytes: Bytes = ctx.into();
    assert_eq!(bytes.len(), 16);
    let recovered = Context::from(&bytes);
    assert_eq!(ctx, recovered);
}

#[test]
fn address_bytes_roundtrip() {
    use bytes::Bytes;
    let addr = Address {
        hash: Hash::from([0x11; 32]),
        context: Context::from([0x22; 16]),
    };
    let bytes: Bytes = addr.into();
    assert_eq!(bytes.len(), std::mem::size_of::<Address>());
    let recovered = Address::from(&bytes);
    assert_eq!(addr, recovered);
}

#[test]
fn hash_from_array_preserves_data() {
    let arr = [0x42u8; 32];
    assert_eq!(Hash::from(arr).data(), &arr);
}

#[test]
fn partition_from_array_preserves_data() {
    let arr = [0x42u8; 16];
    assert_eq!(Partition::from(arr).data(), &arr);
}

#[test]
fn context_from_array_preserves_data() {
    let arr = [0x42u8; 16];
    assert_eq!(Context::from(arr).data(), &arr);
}

#[test]
fn clone_and_resize_zeroed_grow() {
    use bytes::Bytes;
    let original = Bytes::from_static(&[1u8, 2, 3, 4]);
    let resized = original.clone_and_resize_zeroed::<u8>(8);
    assert_eq!(resized.len(), 8);
    assert_eq!(&resized[..4], &[1, 2, 3, 4]);
    assert_eq!(&resized[4..], &[0, 0, 0, 0]);
}

#[test]
fn clone_and_resize_zeroed_same_size() {
    use bytes::Bytes;
    let original = Bytes::from_static(&[5u8, 6, 7, 8]);
    let resized = original.clone_and_resize_zeroed::<u8>(4);
    assert_eq!(resized.len(), 4);
    assert_eq!(&resized[..], &[5, 6, 7, 8]);
}

#[test]
fn typed_bytes_mut_with_count_capacity() {
    use bytes::BytesMut;
    let buf = BytesMut::with_count_capacity::<u32>(5);
    assert_eq!(buf.capacity(), 5 * std::mem::size_of::<u32>());
    assert_eq!(buf.len(), 0);
}

#[test]
fn typed_bytes_mut_set_count() {
    use bytes::BytesMut;
    let mut buf = BytesMut::zeroed_count::<u32>(4);
    assert_eq!(buf.count::<u32>(), 4);
    unsafe { buf.set_count::<u32>(2) };
    assert_eq!(buf.count::<u32>(), 2);
    assert_eq!(buf.len(), 2 * std::mem::size_of::<u32>());
}

#[test]
fn fragment_bytes_roundtrip() {
    use bytes::Bytes;
    let frag = Fragment {
        flags: 0xaa,
        size_payload: 1000,
        size_content: 2000,
    };
    let bytes: Bytes = frag.into();
    let recovered = Fragment::from(&bytes);
    assert_eq!(frag, recovered);
}

#[test]
fn fragment_as_ref_slice_roundtrip() {
    let frag = Fragment {
        flags: 7,
        size_payload: 64,
        size_content: 128,
    };
    let slice: &[u8] = frag.as_ref();
    let recovered = Fragment::from(slice);
    assert_eq!(frag, recovered);
}

#[test]
fn fragment_reference_bytes_roundtrip() {
    use bytes::Bytes;
    let fref = FragmentReference {
        hash: Hash::from([0xbb; 32]),
        offset_content: 12345,
    };
    let bytes: Bytes = fref.into();
    let recovered = FragmentReference::from(&bytes);
    assert_eq!(fref, recovered);
}

#[test]
fn fragment_reference_as_ref_roundtrip() {
    let fref = FragmentReference {
        hash: Hash::from([0xcc; 32]),
        offset_content: 99999,
    };
    let slice: &[u8] = fref.as_ref();
    let recovered = FragmentReference::from(slice);
    assert_eq!(fref, recovered);
}

#[test]
fn partition_uuid_roundtrip() {
    let p = Partition::from([0x88; 16]);
    let uuid: uuid::Uuid = p.into();
    let p2: Partition = uuid.into();
    assert_eq!(p, p2);
}

#[test]
fn address_zero_context_hash() {
    let hash = Hash::from([0xaa; 32]);
    let addr = Address::zero_context_hash(hash);
    assert_eq!(addr.hash, hash);
    assert!(addr.context.is_zero());
}

#[test]
fn address_from_str_hash_only() {
    let hash_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let addr = Address::from_str(hash_hex).unwrap();
    assert_eq!(format!("{}", addr.hash), hash_hex);
    assert!(addr.context.is_zero());
}

#[test]
fn hash_buffer_empty_input() {
    let hash = Hash::hash_buffer(b"");
    assert!(!hash.is_zero());
    let hash2 = Hash::hash_buffer(b"");
    assert_eq!(hash, hash2);
}

#[test]
fn partition_from_str_wrong_length() {
    assert!(Partition::from_str("aabb").is_err());
}

#[test]
fn typed_bytes_from_type_static() {
    use bytes::Bytes;
    static DATA: [u32; 3] = [10, 20, 30];
    let bytes = Bytes::from_type_static(&DATA);
    assert_eq!(bytes.len(), 3 * std::mem::size_of::<u32>());
    let slice = bytes.as_type_slice::<u32>();
    assert_eq!(slice, &[10, 20, 30]);
}

#[test]
fn typed_bytes_mut_as_type_slice_mut() {
    use bytes::BytesMut;
    let mut buf = BytesMut::zeroed_count::<u32>(4);
    let slice = buf.as_type_slice_mut::<u32>();
    slice[0] = 42;
    slice[1] = 99;
    let read_slice = buf.as_type_slice::<u32>();
    assert_eq!(read_slice[0], 42);
    assert_eq!(read_slice[1], 99);
}

/// A buffer with no capacity never allocated, so its pointer is dangling and aligned for
/// bytes rather than for the type. A zero-length slice still requires an aligned pointer,
/// so the count has to be answered without forming one.
#[test]
fn typed_bytes_mut_type_slices_are_empty_without_capacity() {
    use bytes::BytesMut;
    let mut buf = BytesMut::with_count_capacity::<FragmentReference>(0);
    assert!(buf.as_type_slice_mut::<FragmentReference>().is_empty());
    assert!(buf.as_type_slice::<FragmentReference>().is_empty());
}

/// A capacity too small to hold one element is the same case as none at all.
#[test]
fn typed_bytes_mut_type_slices_are_empty_below_one_element() {
    use bytes::BytesMut;
    let mut buf = BytesMut::with_capacity(std::mem::size_of::<FragmentReference>() - 1);
    assert!(buf.as_type_slice_mut::<FragmentReference>().is_empty());
    assert!(buf.as_type_slice::<FragmentReference>().is_empty());
}

mod validate_response {
    use super::*;

    #[test]
    fn accepts_uncompressed_unfragmented() {
        let fragment = Fragment {
            flags: 0,
            size_payload: 128,
            size_content: 128,
        };
        assert!(validate_fragment_response(&fragment).is_ok());
    }

    #[test]
    fn accepts_server_managed_flags_unlike_ingress_validator() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadStoredDurable.into(),
            size_payload: 128,
            size_content: 128,
        };
        assert!(validate_fragment_response(&fragment).is_ok());
    }

    #[test]
    fn accepts_compressed_within_threshold() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadCompressedLZ4.into(),
            size_payload: 100,
            size_content: 200,
        };
        assert!(validate_fragment_response(&fragment).is_ok());
    }

    #[test]
    fn accepts_fragmented_addressing_huge_content() {
        let fragment = Fragment {
            flags: FragmentFlags::PayloadFragmented.into(),
            size_payload: 80,
            size_content: 10 * 1024 * 1024 * 1024, // 10 GiB of referenced content
        };
        assert!(validate_fragment_response(&fragment).is_ok());
    }

    #[test]
    fn rejects_zero_size_payload() {
        let fragment = Fragment {
            flags: 0,
            size_payload: 0,
            size_content: 0,
        };
        assert!(validate_fragment_response(&fragment).is_err());
    }

    #[test]
    fn rejects_oversized_payload() {
        let fragment = Fragment {
            flags: 0,
            size_payload: FRAGMENT_SIZE_THRESHOLD as u32 + 1,
            size_content: FRAGMENT_SIZE_THRESHOLD as u64 + 1,
        };
        assert!(validate_fragment_response(&fragment).is_err());
    }

    #[test]
    fn rejects_payload_greater_than_content() {
        let fragment = Fragment {
            flags: 0,
            size_payload: 200,
            size_content: 100,
        };
        assert!(validate_fragment_response(&fragment).is_err());
    }

    #[test]
    fn rejects_non_fragmented_oversized_content() {
        // Compressed or uncompressed, a non-fragmented fragment must not
        // claim size_content larger than FRAGMENT_SIZE_THRESHOLD because
        // it materializes into a single buffer on read.
        let fragment = Fragment {
            flags: FragmentFlags::PayloadCompressedLZ4.into(),
            size_payload: 1000,
            size_content: FRAGMENT_SIZE_THRESHOLD as u64 + 1,
        };
        assert!(validate_fragment_response(&fragment).is_err());
    }

    #[test]
    fn rejects_fragmented_and_compressed_combo() {
        let fragment = Fragment {
            flags: (FragmentFlags::PayloadFragmented | FragmentFlags::PayloadCompressedLZ4).into(),
            size_payload: 80,
            size_content: 2000,
        };
        assert!(validate_fragment_response(&fragment).is_err());
    }
}

mod pass_through_hasher {
    use std::hash::BuildHasher;
    use std::hash::BuildHasherDefault;

    use super::*;

    /// A `Hash` writes its bytes last, so a set of hashes buckets on their leading eight bytes. A
    /// write after them would replace them as the hash.
    #[test]
    fn a_hash_hashes_to_its_leading_eight_bytes() {
        let hash = Hash::from(std::array::from_fn::<u8, 32, _>(|index| index as u8 + 1));
        let leading: [u8; 8] = hash.as_bytes()[..8].try_into().unwrap();
        assert_eq!(
            BuildHasherDefault::<PassThroughHasher>::default().hash_one(hash),
            u64::from_ne_bytes(leading)
        );
    }

    #[test]
    fn a_u64_key_hashes_to_itself() {
        let key = 0x0123_4567_89ab_cdef_u64;
        assert_eq!(
            BuildHasherDefault::<PassThroughHasher>::default().hash_one(key),
            key
        );
    }

    /// A write shorter than eight bytes, such as a flag's, would make a key hash to few values.
    #[test]
    #[should_panic(expected = "writes at least eight")]
    fn a_short_write_is_refused() {
        BuildHasherDefault::<PassThroughHasher>::default().hash_one((Hash::default(), true));
    }
}
