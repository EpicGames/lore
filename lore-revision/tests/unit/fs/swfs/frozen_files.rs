// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::swfs::api_interface::swfs_api::SWFSFile;
use lore_revision::fs::swfs::frozen_files::FROZEN_READ_ALIGNMENT;
use lore_revision::fs::swfs::frozen_files::FROZEN_READ_RETAINED;
use lore_revision::fs::swfs::frozen_files::FrozenFile;
use lore_revision::fs::swfs::frozen_files::FrozenListCollector;
use lore_revision::fs::swfs::frozen_files::FrozenReadBuffer;
use lore_revision::fs::swfs::frozen_files::frozen_read_range;

const NEW_NODE: u32 = 1;
const CHANGED_WHILE_FROZEN: u32 = 1 << 15;
const DIRECTORY_ATTRIBUTE: u64 = 0x10;

struct Entry {
    path: &'static str,
    mod_flags: u32,
    file_index: u32,
    size: u64,
    attrs: u64,
}

/// Lays the entries out the way SWFS does: each an `SWFSFile` followed by its NUL-terminated path,
/// the next entry right after (so unaligned for `SWFSFile`), `next` and `path` absolute pointers
/// into the buffer.
fn fill_packed(buffer: *mut u8, entries: &[Entry]) -> usize {
    let mut offset = 0;
    for (i, entry) in entries.iter().enumerate() {
        let path_at = offset + size_of::<SWFSFile>();
        let next_at = path_at + entry.path.len() + 1;
        unsafe {
            std::ptr::copy_nonoverlapping(
                entry.path.as_ptr(),
                buffer.add(path_at),
                entry.path.len(),
            );
            *buffer.add(path_at + entry.path.len()) = 0;
            let mut file: SWFSFile = std::mem::zeroed();
            file.path = buffer.add(path_at).cast();
            file.mod_flags = entry.mod_flags;
            file.file_index = entry.file_index;
            file.finfo.size = entry.size;
            file.finfo.attrs = entry.attrs;
            file.finfo.time_write = 7;
            file.finfo.time_create = 5;
            file.next = if i + 1 < entries.len() {
                buffer.add(next_at).cast()
            } else {
                std::ptr::null_mut()
            };
            std::ptr::write_unaligned(buffer.add(offset).cast::<SWFSFile>(), file);
        }
        offset = next_at;
    }
    offset
}

fn packed_size(entries: &[Entry]) -> usize {
    entries
        .iter()
        .map(|entry| size_of::<SWFSFile>() + entry.path.len() + 1)
        .sum()
}

#[test]
fn the_list_collector_reads_the_packed_unaligned_entries() {
    let entries = [
        Entry {
            path: r"\b.txt",
            mod_flags: CHANGED_WHILE_FROZEN,
            file_index: 7,
            size: 13,
            attrs: 0x20,
        },
        Entry {
            path: r"\sub",
            mod_flags: CHANGED_WHILE_FROZEN | NEW_NODE,
            file_index: 14,
            size: 0,
            attrs: DIRECTORY_ATTRIBUTE,
        },
        Entry {
            path: r"\sub\n.txt",
            mod_flags: CHANGED_WHILE_FROZEN,
            file_index: 15,
            size: 10,
            attrs: 0x20,
        },
    ];
    let total = packed_size(&entries);
    let mut collector = FrozenListCollector::default();
    let mut allocated = 0usize;

    let buffer = unsafe { collector.on_callback(std::ptr::null_mut(), total, &mut allocated) };
    assert!(!buffer.is_null());
    assert!(allocated >= total);
    let second_entry_at = packed_size(&entries[..1]);
    assert_ne!(
        (buffer as usize + second_entry_at) % align_of::<SWFSFile>(),
        0,
        "the second entry must be unaligned for this test to mean anything"
    );
    fill_packed(buffer, &entries);
    // SWFS passes no size pointer on the second call.
    let next = unsafe { collector.on_callback(buffer, total, std::ptr::null_mut()) };

    assert!(next.is_null());
    assert_eq!(
        collector.into_files(),
        vec![
            FrozenFile {
                path: r"\b.txt".to_string(),
                mod_flags: CHANGED_WHILE_FROZEN,
                file_index: 7,
                size: 13,
                time_create: 5,
                time_write: 7,
                is_directory: false,
            },
            FrozenFile {
                path: r"\sub".to_string(),
                mod_flags: CHANGED_WHILE_FROZEN | NEW_NODE,
                file_index: 14,
                size: 0,
                time_create: 5,
                time_write: 7,
                is_directory: true,
            },
            FrozenFile {
                path: r"\sub\n.txt".to_string(),
                mod_flags: CHANGED_WHILE_FROZEN,
                file_index: 15,
                size: 10,
                time_create: 5,
                time_write: 7,
                is_directory: false,
            },
        ]
    );
}

#[test]
fn an_empty_list_asks_for_no_buffer() {
    let mut collector = FrozenListCollector::default();
    let mut allocated = 1usize;

    let buffer = unsafe { collector.on_callback(std::ptr::null_mut(), 0, &mut allocated) };

    assert!(buffer.is_null());
    assert_eq!(allocated, 0);
    assert!(collector.into_files().is_empty());
}

#[test]
fn the_read_buffer_is_aligned_and_rounded_up_to_a_whole_block() {
    for (requested, expected) in [
        (1usize, FROZEN_READ_ALIGNMENT),
        (13, FROZEN_READ_ALIGNMENT),
        (FROZEN_READ_ALIGNMENT, FROZEN_READ_ALIGNMENT),
        (FROZEN_READ_ALIGNMENT + 1, 2 * FROZEN_READ_ALIGNMENT),
    ] {
        let mut read = FrozenReadBuffer::default();
        let mut allocated = 0usize;

        let buffer = unsafe { read.on_callback(std::ptr::null_mut(), requested, &mut allocated) };

        assert!(!buffer.is_null());
        assert_eq!(buffer as usize % FROZEN_READ_ALIGNMENT, 0, "{requested}");
        assert_eq!(allocated, expected, "{requested}");
    }
}

#[test]
fn an_empty_file_still_gets_a_valid_buffer() {
    let mut read = FrozenReadBuffer::default();
    let mut allocated = 0usize;

    let buffer = unsafe { read.on_callback(std::ptr::null_mut(), 0, &mut allocated) };

    assert!(!buffer.is_null());
    assert_eq!(buffer as usize % FROZEN_READ_ALIGNMENT, 0);
    assert_eq!(allocated, FROZEN_READ_ALIGNMENT);
}

#[test]
fn the_filled_buffer_yields_the_bytes_the_read_returned() {
    let content = b"BBBB-modified";
    let mut read = FrozenReadBuffer::default();
    let mut allocated = 0usize;
    let buffer = unsafe { read.on_callback(std::ptr::null_mut(), content.len(), &mut allocated) };
    unsafe { std::ptr::copy_nonoverlapping(content.as_ptr(), buffer, content.len()) };

    // SWFS passes no size pointer on the second call.
    let next = unsafe { read.on_callback(buffer, content.len(), std::ptr::null_mut()) };

    assert!(next.is_null());
    let mut into = vec![0; content.len() - 5];
    assert_eq!(read.copy_into(content.len(), 5, &mut into), Some(true));
    assert_eq!(into, b"modified");
}

#[test]
fn a_read_never_filled_has_no_content() {
    let mut read = FrozenReadBuffer::default();
    let mut allocated = 0usize;
    unsafe { read.on_callback(std::ptr::null_mut(), 13, &mut allocated) };

    assert_eq!(read.copy_into(13, 0, &mut [0; 13]), None);
}

#[test]
fn a_read_longer_than_its_buffer_is_clamped_rather_than_overrun() {
    let mut read = FrozenReadBuffer::default();
    let mut allocated = 0usize;
    let buffer = unsafe { read.on_callback(std::ptr::null_mut(), 4, &mut allocated) };
    unsafe { read.on_callback(buffer, 4, std::ptr::null_mut()) };

    let mut into = vec![0; allocated + 1];
    assert_eq!(read.copy_into(allocated + 100, 0, &mut into), Some(false));
    let mut within = vec![0; allocated];
    assert_eq!(read.copy_into(allocated + 100, 0, &mut within), Some(true));
}

#[test]
fn a_read_too_short_for_the_range_fills_nothing() {
    let mut read = FrozenReadBuffer::default();
    let mut allocated = 0usize;
    let buffer = unsafe { read.on_callback(std::ptr::null_mut(), 8, &mut allocated) };
    unsafe { read.on_callback(buffer, 8, std::ptr::null_mut()) };

    assert_eq!(read.copy_into(8, 4, &mut [0; 5]), Some(false));
}

#[test]
fn a_range_is_widened_to_the_whole_blocks_it_touches() {
    let block = FROZEN_READ_ALIGNMENT;
    for (offset, length, expected) in [
        (0u64, 1usize, (0u64, block)),
        (0, block, (0, block)),
        (1, block, (0, 2 * block)),
        (block as u64, block, (block as u64, block)),
        (block as u64 + 7, 10, (block as u64, block)),
        (3 * block as u64 - 1, 2, (2 * block as u64, 2 * block)),
    ] {
        assert_eq!(
            frozen_read_range(offset, length),
            expected,
            "{offset} + {length}"
        );
    }
}

fn frozen(size: u64, time_write: u64, is_directory: bool) -> FrozenFile {
    FrozenFile {
        path: r"\f.txt".to_string(),
        mod_flags: CHANGED_WHILE_FROZEN,
        file_index: 1,
        size,
        time_create: 0,
        time_write,
        is_directory,
    }
}

/// 100ns ticks from 1601 to the Unix epoch, which is what separates SWFS's file times from the
/// milliseconds a `FileInfo` holds.
const UNIX_EPOCH_AS_FILE_TIME: u64 = 116_444_736_000_000_000;

#[test]
fn a_frozen_file_describes_itself_as_file_info() {
    let info = frozen(13, UNIX_EPOCH_AS_FILE_TIME + 12_345 * 10_000 + 9_999, false).file_info();

    assert_eq!(
        info,
        FileInfo::File {
            executable: None,
            size: 13,
            mtime: 12_345,
        }
    );
}

#[test]
fn a_frozen_directory_describes_itself_as_a_directory() {
    assert_eq!(frozen(0, 0, true).file_info(), FileInfo::Directory);
}

#[test]
fn a_time_before_the_unix_epoch_is_zero() {
    let info = frozen(1, 5, false).file_info();

    assert_eq!(info.mtime(), 0);
}

/// Asks `read` for a buffer of `size` bytes, as SWFS's first call does, fills it with `content`
/// and hands it back, as the second does. Answers the buffer it was given.
fn read_through(read: &mut FrozenReadBuffer, size: usize, content: &[u8]) -> *mut u8 {
    let mut allocated = 0usize;
    let buffer = unsafe { read.on_callback(std::ptr::null_mut(), size, &mut allocated) };
    assert!(!buffer.is_null());
    unsafe { std::ptr::copy_nonoverlapping(content.as_ptr(), buffer, content.len()) };
    unsafe { read.on_callback(buffer, size, std::ptr::null_mut()) };
    buffer
}

#[test]
fn a_read_buffer_is_reused_by_a_later_read_it_covers() {
    let mut read = FrozenReadBuffer::default();
    let first = read_through(&mut read, 2 * FROZEN_READ_ALIGNMENT, b"first");
    let second = read_through(&mut read, 13, b"second");

    assert_eq!(first, second, "a read the buffer covers allocated again");
    assert_eq!(read.capacity(), 2 * FROZEN_READ_ALIGNMENT);
    let mut into = [0; 6];
    assert_eq!(read.copy_into(6, 0, &mut into), Some(true));
    assert_eq!(&into, b"second");
}

#[test]
fn a_read_buffer_grows_for_a_read_it_does_not_cover() {
    let mut read = FrozenReadBuffer::default();
    read_through(&mut read, 13, b"small");
    let mut allocated = 0usize;

    let buffer = unsafe {
        read.on_callback(
            std::ptr::null_mut(),
            3 * FROZEN_READ_ALIGNMENT,
            &mut allocated,
        )
    };

    assert!(!buffer.is_null());
    assert_eq!(allocated, 3 * FROZEN_READ_ALIGNMENT);
    assert_eq!(read.capacity(), 3 * FROZEN_READ_ALIGNMENT);
}

#[test]
fn a_read_buffer_reports_what_each_read_asked_for_not_what_it_holds() {
    let mut read = FrozenReadBuffer::default();
    read_through(&mut read, 3 * FROZEN_READ_ALIGNMENT, b"large");
    let mut allocated = 0usize;

    let buffer = unsafe { read.on_callback(std::ptr::null_mut(), 13, &mut allocated) };
    unsafe { read.on_callback(buffer, 13, std::ptr::null_mut()) };

    assert_eq!(allocated, FROZEN_READ_ALIGNMENT);
    // Bytes past this read's request are never copied out, whatever an earlier read left there.
    let mut into = vec![0; FROZEN_READ_ALIGNMENT + 1];
    assert_eq!(
        read.copy_into(3 * FROZEN_READ_ALIGNMENT, 0, &mut into),
        Some(false)
    );
}

#[test]
fn a_read_buffer_larger_than_what_is_retained_is_released() {
    let mut read = FrozenReadBuffer::default();
    read_through(&mut read, 4 * FROZEN_READ_ALIGNMENT, b"large");

    read.retain_at_most(2 * FROZEN_READ_ALIGNMENT);

    assert_eq!(read.capacity(), 0);
    read_through(&mut read, 13, b"again");
    read.retain_at_most(2 * FROZEN_READ_ALIGNMENT);
    assert_eq!(read.capacity(), FROZEN_READ_ALIGNMENT);
}

#[test]
fn a_read_that_fills_nothing_after_one_that_did_has_no_content() {
    let mut read = FrozenReadBuffer::default();
    read_through(&mut read, 13, b"stale content");
    let mut allocated = 0usize;

    // SWFS asks for the buffer and never hands it back filled.
    unsafe { read.on_callback(std::ptr::null_mut(), 13, &mut allocated) };

    assert_eq!(read.copy_into(13, 0, &mut [0; 13]), None);
}

#[test]
fn a_read_answered_without_any_callback_has_no_content() {
    let mut read = FrozenReadBuffer::default();
    read_through(&mut read, 13, b"stale content");

    // SWFS answers a file with no bytes of its own without calling back at all.
    let copied = read.read(|_| 0, 0, &mut [0]);

    assert_eq!(copied, None);
}

#[test]
fn a_read_releases_a_buffer_larger_than_what_is_retained_and_keeps_a_smaller_one() {
    let mut read = FrozenReadBuffer::default();
    let ask = |size: usize| {
        move |target: *mut FrozenReadBuffer| {
            let mut allocated = 0usize;
            unsafe { (*target).on_callback(std::ptr::null_mut(), size, &mut allocated) };
            0
        }
    };

    read.read(ask(FROZEN_READ_RETAINED + 1), 0, &mut [0]);
    assert_eq!(read.capacity(), 0, "a buffer over the cap was kept");

    read.read(ask(FROZEN_READ_ALIGNMENT), 0, &mut [0]);
    assert_eq!(read.capacity(), FROZEN_READ_ALIGNMENT);
}
