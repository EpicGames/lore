// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_io::AlignedBuf;
use lore_io::IoDriver;
use lore_io::OpenOptions;
use lore_io::UNBUFFERED_ALIGNMENT;
use lore_io::unbuffered::test_util::Refusal;
use lore_io::unbuffered::test_util::refuse_unbuffered;
use lore_io::unbuffered_fallbacks;
use lore_io::unbuffered_len;

/// The lengths each unbuffered write is tried at: empty, inside the first sector, at its end and
/// past it, and at and past the most an unfragmented fragment holds.
const LENGTHS: [usize; 7] = [0, 1, 4095, 4096, 4097, 256 * 1024, 256 * 1024 + 1];

/// Content of `len` bytes that differs at every offset within a sector.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

#[test]
fn an_aligned_buffer_is_aligned_and_pads_its_content_with_zeros() {
    for len in LENGTHS {
        let mut buffer = AlignedBuf::zeroed(len);
        buffer.content_mut().fill(0xA5);

        assert_eq!(buffer.len(), len);
        assert_eq!(buffer.content().len(), len);
        assert_eq!(buffer.as_ref().len(), unbuffered_len(len));
        assert_eq!(buffer.as_ref().len() % UNBUFFERED_ALIGNMENT, 0);
        if len > 0 {
            assert_eq!(buffer.as_ref().as_ptr() as usize % UNBUFFERED_ALIGNMENT, 0);
        }
        assert!(
            buffer.as_ref()[len..].iter().all(|byte| *byte == 0),
            "{len}"
        );
    }
}

/// A buffer taken to a shorter length pads the new content with zeros, whatever it held there,
/// so a write of it never carries the bytes of an earlier one.
#[test]
fn an_aligned_buffer_taken_shorter_zeroes_what_it_held_past_the_content() {
    // SAFETY: every byte of the content is written before it is read.
    let mut buffer = unsafe { AlignedBuf::uninit(3 * UNBUFFERED_ALIGNMENT) };
    buffer.content_mut().fill(0xFF);

    // SAFETY: the new content lies within what was written above.
    unsafe { buffer.set_len(UNBUFFERED_ALIGNMENT + 10) };

    assert_eq!(buffer.as_ref().len(), 2 * UNBUFFERED_ALIGNMENT);
    assert!(buffer.content().iter().all(|byte| *byte == 0xFF));
    assert!(
        buffer.as_ref()[UNBUFFERED_ALIGNMENT + 10..]
            .iter()
            .all(|byte| *byte == 0)
    );
}

/// The unbuffered whole-file write holds exactly the content, at its exact length, durable or
/// not, and writes it unbuffered on Windows and through the file cache elsewhere.
#[tokio::test]
async fn an_unbuffered_whole_file_write_holds_exactly_its_content() {
    let dir = TempDir::new("lore-io-unbuffered-whole-");
    for len in LENGTHS {
        for durable in [false, true] {
            let path = dir.child(&format!("file-{len}-{durable}"));
            std::fs::write(&path, vec![0xEE; len + 8192]).expect("write the earlier content");
            let content = pattern(len);

            let (metadata, unbuffered) = IoDriver::global()
                .write_file_bytes_unbuffered(&path, Bytes::from(content.clone()), durable)
                .await
                .expect("write the file");

            assert_eq!(unbuffered, cfg!(target_family = "windows"), "{len}");
            assert_eq!(metadata.len(), len as u64, "{len}");
            assert_eq!(std::fs::read(&path).expect("read back"), content, "{len}");
        }
    }
}

/// A volume that refuses the unbuffered open, or the write after it, has the file written through
/// the file cache, whole, and counted.
#[tokio::test]
async fn a_refused_unbuffered_whole_file_write_goes_through_the_file_cache() {
    let dir = TempDir::new("lore-io-unbuffered-refused-");
    for (refusal, name) in [(Refusal::Open, "open"), (Refusal::Write, "write")] {
        let refused = dir.child(name);
        std::fs::create_dir(&refused).expect("make the refused directory");
        refuse_unbuffered(&refused, refusal);
        let path = refused.join("file");
        let content = pattern(5000);
        let before = unbuffered_fallbacks();

        let (metadata, unbuffered) = IoDriver::global()
            .write_file_bytes_unbuffered(&path, Bytes::from(content.clone()), false)
            .await
            .expect("write the file");

        assert!(!unbuffered, "{name}");
        assert_eq!(metadata.len(), content.len() as u64, "{name}");
        assert_eq!(std::fs::read(&path).expect("read back"), content, "{name}");
        if cfg!(target_family = "windows") {
            assert!(unbuffered_fallbacks() > before, "{name}");
        }
    }
}

/// A file opened unbuffered takes aligned writes of padded buffers, and its end set to the
/// content's length leaves exactly the content.
#[tokio::test]
async fn a_file_opened_unbuffered_holds_aligned_writes_cut_to_its_length() {
    let dir = TempDir::new("lore-io-unbuffered-file-");
    let path = dir.child("file");
    let len = 3 * UNBUFFERED_ALIGNMENT + 100;
    let content = pattern(len);
    let file = IoDriver::global()
        .open(
            &path,
            &OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .unbuffered(true),
        )
        .await
        .expect("open unbuffered");

    let mut first = AlignedBuf::zeroed(2 * UNBUFFERED_ALIGNMENT);
    first
        .content_mut()
        .copy_from_slice(&content[..2 * UNBUFFERED_ALIGNMENT]);
    let mut last = AlignedBuf::zeroed(len - 2 * UNBUFFERED_ALIGNMENT);
    last.content_mut()
        .copy_from_slice(&content[2 * UNBUFFERED_ALIGNMENT..]);
    file.write_all_at(first, 0)
        .await
        .expect("write the first part");
    file.write_all_at(last, 2 * UNBUFFERED_ALIGNMENT as u64)
        .await
        .expect("write the last part");
    file.set_len(len as u64, ())
        .await
        .expect("set the end of the file");
    drop(file);

    assert_eq!(std::fs::read(&path).expect("read back"), content);
}
