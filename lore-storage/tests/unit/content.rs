// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_base::test_util::TempDir;
use lore_base::types::Hash;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::Partition;
use lore_storage::content::ContentHandle;
use lore_storage::content::ContentSource;
use lore_storage::content::WindowRead;
use lore_storage::error::StorageError;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::hash;
use lore_storage::immutable_store::ImmutableStore;
use lore_storage::local::immutable_store::ImmutableStoreSettings;
use lore_storage::local::immutable_store::LocalImmutableStore;
use lore_storage::options::ReadOptions;

async fn make_store() -> (TempDir, Arc<dyn ImmutableStore>) {
    let dir = TempDir::new("lore-storage-content-test-");
    let store = LocalImmutableStore::new(
        Some(PathBuf::from(dir.as_ref())),
        ImmutableStoreSettings::default(),
    )
    .await
    .expect("create test store");
    (dir, store)
}

const PARTITION: [u8; 16] = [3; 16];

fn partition() -> Partition {
    Partition::from(PARTITION)
}

/// The bytes the tests store: distinct per position, so a read from the wrong offset differs.
fn content(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

/// Stores `payload` uncompressed as one fragment, returning its address.
async fn put_whole(store: &Arc<dyn ImmutableStore>, payload: &[u8]) -> Address {
    let address = Address {
        hash: hash::hash_slice(payload),
        context: Context::from([9; 16]),
    };
    store
        .clone()
        .put(
            partition(),
            address,
            Fragment {
                flags: FragmentFlags::PayloadStoredLocal.bits(),
                size_payload: payload.len() as u32,
                size_content: payload.len() as u64,
            },
            Some(Bytes::copy_from_slice(payload)),
            false,
        )
        .await
        .expect("put content");
    address
}

fn stored(store: &Arc<dyn ImmutableStore>, address: Address, size: u64) -> ContentSource<'static> {
    ContentSource::stored(
        store.clone(),
        partition(),
        address,
        size,
        ReadOptions::default(),
        None,
    )
}

async fn window(handle: &ContentHandle, start: usize, want: usize, offset: u64) -> Vec<u8> {
    let buffer = BytesMut::zeroed(start + want);
    let filled = handle
        .read_window(WindowRead::new(buffer, start, want), offset)
        .await
        .expect("a window inside the content");
    filled[start..start + want].to_vec()
}

#[tokio::test]
async fn memory_content_reports_its_size_and_reads_whole() {
    let bytes = content(1000);
    let source = ContentSource::memory(Bytes::from(bytes.clone()));

    let (handle, size) = source.open_once().await.unwrap();

    assert_eq!(size, 1000);
    assert_eq!(handle.read_all(1000).await.unwrap().as_ref(), &bytes[..]);
    assert_eq!(source.read_all().await.unwrap().as_ref(), &bytes[..]);
}

#[tokio::test]
async fn memory_content_reads_a_window_at_an_offset_into_the_buffer() {
    let bytes = content(1000);
    let (handle, _) = ContentSource::memory(Bytes::from(bytes.clone()))
        .open_once()
        .await
        .unwrap();

    // Three bytes of headroom in front of the window, which the read leaves alone.
    assert_eq!(window(&handle, 3, 100, 250).await, &bytes[250..350]);
    assert_eq!(window(&handle, 0, 1, 999).await, &bytes[999..1000]);
}

#[tokio::test]
async fn memory_content_refuses_a_window_past_its_end() {
    let (handle, _) = ContentSource::memory(Bytes::from(content(10)))
        .open_once()
        .await
        .unwrap();

    let buffer = BytesMut::zeroed(8);

    assert!(
        handle
            .read_window(WindowRead::new(buffer, 0, 8), 5)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn stored_content_reports_its_size_and_reads_whole() {
    let (_dir, store) = make_store().await;
    let bytes = content(5000);
    let address = put_whole(&store, &bytes).await;
    let source = stored(&store, address, 5000);

    let (handle, size) = source.open_once().await.unwrap();

    assert_eq!(size, 5000);
    assert_eq!(handle.read_all(5000).await.unwrap().as_ref(), &bytes[..]);
    assert_eq!(source.read_all().await.unwrap().as_ref(), &bytes[..]);
}

#[tokio::test]
async fn stored_content_reads_a_window_at_an_offset() {
    let (_dir, store) = make_store().await;
    let bytes = content(5000);
    let address = put_whole(&store, &bytes).await;
    let (handle, _) = stored(&store, address, 5000).open_once().await.unwrap();

    assert_eq!(window(&handle, 4, 700, 1234).await, &bytes[1234..1934]);
    assert_eq!(window(&handle, 0, 100, 4900).await, &bytes[4900..5000]);
}

#[tokio::test]
async fn stored_content_that_was_compressed_reads_back_expanded() {
    let (_dir, store) = make_store().await;
    let bytes: Vec<u8> = (0..20_000usize).map(|index| (index / 64) as u8).collect();
    let (fragment, payload) = lore_storage::compress::compress(
        Fragment {
            flags: 0,
            size_payload: bytes.len() as u32,
            size_content: bytes.len() as u64,
        },
        &bytes,
        lore_storage::compress::CompressionMode::Lz4,
    )
    .expect("compress test content");
    assert!((payload.len() as u64) < fragment.size_content);
    let address = Address {
        hash: hash::hash_slice(&bytes),
        context: Context::from([9; 16]),
    };
    store
        .clone()
        .put(partition(), address, fragment, Some(payload), false)
        .await
        .expect("put compressed content");

    let source = stored(&store, address, bytes.len() as u64);
    let (handle, size) = source.open_once().await.unwrap();

    assert_eq!(size, bytes.len() as u64);
    assert_eq!(
        handle.read_all(bytes.len()).await.unwrap().as_ref(),
        &bytes[..]
    );
    assert_eq!(window(&handle, 0, 500, 9000).await, &bytes[9000..9500]);
}

#[tokio::test]
async fn stored_content_the_store_does_not_hold_is_an_error() {
    let (_dir, store) = make_store().await;
    let missing = Address {
        hash: hash::hash_slice(b"nothing stored under this"),
        context: Context::from([9; 16]),
    };
    let source = stored(&store, missing, 25);

    assert!(source.read_all().await.is_err());
    let (handle, _) = source.open_once().await.unwrap();
    let buffer = BytesMut::zeroed(25);
    assert!(
        handle
            .read_window(WindowRead::new(buffer, 0, 25), 0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn every_kind_of_content_reads_the_bytes_a_file_with_them_would() {
    let (_dir, store) = make_store().await;
    let bytes = content(3000);
    let address = put_whole(&store, &bytes).await;
    let directory = TempDir::new("lore-storage-content-file-");
    let path = PathBuf::from(directory.as_ref()).join("same.bin");
    std::fs::write(&path, &bytes).unwrap();

    let from_file = ContentSource::file(&path).read_all().await.unwrap();
    let from_store = stored(&store, address, 3000).read_all().await.unwrap();
    let from_memory = ContentSource::memory(Bytes::from(bytes))
        .read_all()
        .await
        .unwrap();

    assert_eq!(from_store, from_file);
    assert_eq!(from_memory, from_file);
}

#[tokio::test]
async fn content_without_a_host_path_names_none() {
    let (_dir, store) = make_store().await;
    let address = put_whole(&store, &content(10)).await;

    assert!(ContentSource::memory(Bytes::new()).file_path().is_none());
    assert!(stored(&store, address, 10).file_path().is_none());
}

fn test_address(seed: u64) -> Address {
    Address {
        hash: Hash::from_u64(seed),
        ..Default::default()
    }
}

/// Virtual content names no host path.
#[test]
fn virtual_immutable_has_no_file_path() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    assert!(source.file_path().is_none());
}

/// This crate does not open virtual content.
#[tokio::test]
async fn virtual_immutable_open_once_is_unsupported() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    match source.open_once().await {
        Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::Unsupported),
        Ok(_) => panic!("virtual content cannot be opened directly"),
    }
}

/// Virtual immutable content displays as the address it names.
#[test]
fn virtual_immutable_display_names_address() {
    let address = test_address(7);
    let source = ContentSource::virtual_immutable(address, 42);
    assert_eq!(format!("{source}"), format!("immutable:{address}"));
}

/// Virtual immutable content answers its declared size, which settles a comparison by size
/// without reading it.
#[tokio::test]
async fn virtual_immutable_size_is_its_declared_size() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    assert_eq!(source.size().await.expect("a declared size"), 42);
}

/// A file's size is its length, and a path that holds nothing has no size.
#[tokio::test]
async fn a_file_size_is_its_length() {
    let file = lore_base::test_util::TempFile::with_contents("content-size-", b"twelve bytes");
    let missing = lore_base::test_util::TempDir::new("content-size-missing-");

    assert_eq!(
        ContentSource::file(file.path())
            .size()
            .await
            .expect("a file's size"),
        12
    );
    assert!(
        ContentSource::file(&missing.child("absent"))
            .size()
            .await
            .is_err()
    );
}

/// Opening or reading virtual content fails at once as an argument error, with no back-off.
#[tokio::test]
async fn virtual_immutable_open_and_read_all_are_invalid_arguments() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    let started = std::time::Instant::now();
    match source.open().await {
        Err(err) => assert!(matches!(err, StorageError::InvalidArguments(_)), "{err:?}"),
        Ok(_) => panic!("virtual content cannot be opened by this crate"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "the open waited on a back-off"
    );
    let err = source
        .read_all()
        .await
        .expect_err("virtual content cannot be read by this crate");
    assert!(matches!(err, StorageError::InvalidArguments(_)), "{err:?}");
}
