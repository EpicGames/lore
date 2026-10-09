// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use rbe_lore::digest;
use rbe_proto::reapi::compressor;
use rbe_server::cas::*;

const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn parses_plain_and_compressed_resources() {
    let r = parse_resource(&format!("blobs/{H}/42")).unwrap();
    assert_eq!((r.hash.as_str(), r.size, r.codec), (H, 42, Codec::Identity));

    let r = parse_resource(&format!("inst/uploads/u-1/blobs/sha256/{H}/7/meta")).unwrap();
    assert_eq!((r.size, r.codec), (7, Codec::Identity));

    let r = parse_resource(&format!("compressed-blobs/zstd/{H}/1000")).unwrap();
    assert_eq!((r.hash.as_str(), r.size, r.codec), (H, 1000, Codec::Zstd));

    let r = parse_resource(&format!("inst/uploads/u-2/compressed-blobs/zstd/{H}/5/x")).unwrap();
    assert_eq!((r.size, r.codec), (5, Codec::Zstd));
}

#[test]
fn rejects_compressors_it_does_not_offer() {
    let err = parse_resource(&format!("compressed-blobs/deflate/{H}/5")).unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unimplemented);
    let err = parse_resource("compressed-blobs").unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(Codec::from_proto(compressor::Value::Deflate as i32).is_err());
}

#[test]
fn zstd_round_trips_and_is_capped_at_the_declared_size() {
    let blob = b"cc -Iexternal/a -Iexternal/b -c foo.cc -o foo.o ".repeat(200);
    let wire = Codec::Zstd.encode(blob.clone()).unwrap();
    assert!(wire.len() < blob.len() / 10);
    assert_eq!(
        Codec::Zstd.decode(&wire, blob.len() as i64).unwrap(),
        &blob[..]
    );
    // A frame that expands past what the digest declares is refused, not allocated.
    assert!(Codec::Zstd.decode(&wire, blob.len() as i64 - 1).is_err());
    assert!(Codec::Zstd.decode(b"not zstd", 100).is_err());
    assert_eq!(Codec::Identity.decode(b"abc", 3).unwrap(), &b"abc"[..]);
}

#[tokio::test]
async fn codec_gives_the_same_answer_on_and_off_the_runtime() {
    // One blob under the blocking-pool threshold and one over it.
    for blob in [
        b"small blob ".repeat(10),
        b"large blob ".repeat(OFF_RUNTIME_BYTES),
    ] {
        let size = blob.len() as i64;
        let wire = Codec::Zstd.encode_async(blob.clone()).await.unwrap();
        let (back, hash) = Codec::Zstd.decode_and_hash(wire, size).await.unwrap();
        assert_eq!(
            (back.as_slice(), hash.as_str()),
            (&blob[..], digest::sha256_hex(&blob).as_str())
        );
        let (same, _) = Codec::Identity
            .decode_and_hash(blob.clone(), size)
            .await
            .unwrap();
        assert_eq!(same, blob);
        let err = Codec::Zstd
            .decode_and_hash(b"not zstd".to_vec(), size)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
