// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fs::File;
use std::io::Write;
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Waker;
use std::time::Duration;

use lore_io::iocp::*;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_NONE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
use windows_sys::Win32::System::Pipes::PIPE_TYPE_BYTE;
use windows_sys::Win32::System::Pipes::PIPE_WAIT;

/// A pipe pair whose reading end is overlapped, for the one operation a file cannot be made to
/// perform on demand.
///
/// Whether a *file* read defers is a property of the host — its filesystem, its cache state and
/// whatever filters sit in front of them — so a test that reads one asserts nothing it can rely
/// on. A pipe with no data waiting defers by definition. The handle underneath is an ordinary
/// overlapped handle bound to the same port, so the submission, the packet, the reaper and the
/// wake are the file path's exactly.
fn overlapped_pipe_pair() -> (File, File) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name: Vec<u16> = format!(
        "\\\\.\\pipe\\lore-io-iocp-{}-{}\0",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
    .encode_utf16()
    .collect();

    // SAFETY: Calling OS functions. One instance of a byte-mode pipe, readable by this process
    // and overlapped, named by the null-terminated string above.
    let server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        server,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );

    // SAFETY: Calling OS functions. Opens the instance created above, which is listening.
    let client = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        client,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );

    // SAFETY: both handles were just created and are owned by nothing else.
    unsafe { (File::from_raw_handle(server), File::from_raw_handle(client)) }
}

/// The path this backend exists for: an operation the kernel does not finish during the
/// issuing call is completed by the reaper, through the port, and hands its buffer back.
///
/// The assertion that the first poll is pending is half the test. Without it a run where the
/// kernel happened to complete inline would still pass, and inline completion is the path this
/// case is not about.
#[test]
fn a_pending_operation_completes_through_the_port() {
    let driver = IocpDriver::new().expect("a completion port");
    let (server, mut client) = overlapped_pipe_pair();
    driver.register(&server).expect("binding the pipe");

    futures::executor::block_on(async {
        let mut read = Box::pin(driver.read_at(Arc::new(server), 8, 0));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            read.as_mut().poll(&mut context).is_pending(),
            "a read of an empty pipe completed during the issuing call"
        );

        client.write_all(b"deferred").expect("writing to the pipe");
        let bytes = read.await.expect("the deferred read");
        assert_eq!(&bytes[..], b"deferred");
    });

    let stats = driver.stats();
    assert_eq!(stats.reaped, 1, "the completion did not come from the port");
    assert_eq!(stats.inline, 0, "{stats:?}");
}

/// Abandoning a pending operation must not free what the kernel still owns. The buffer lives
/// in the entry, so the completion arriving after the future is gone has somewhere to land —
/// and the reaper accounting for it is what says the entry was still there to complete.
#[test]
fn an_abandoned_pending_operation_is_completed_and_released() {
    let driver = IocpDriver::new().expect("a completion port");
    let (server, mut client) = overlapped_pipe_pair();
    driver.register(&server).expect("binding the pipe");

    let mut read = Box::pin(driver.read_at(Arc::new(server), 8, 0));
    let mut context = Context::from_waker(Waker::noop());
    assert!(read.as_mut().poll(&mut context).is_pending());
    drop(read);

    client.write_all(b"deferred").expect("writing to the pipe");
    for _ in 0..2000 {
        if driver.stats().reaped == 1 {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "an abandoned operation was never completed: {:?}",
        driver.stats()
    );
}

/// Every operation is completed once, by exactly one of the two paths.
///
/// Which path is a property of the host, so the test does not pin it. What it pins is that the
/// two cannot both claim an operation, which is the failure
/// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` exists to make impossible; the benchmark reports the
/// share over a real workload.
#[test]
fn every_file_operation_is_completed_by_exactly_one_path() {
    let dir = lore_base::test_util::TempDir::new("lore-io-iocp-count-");
    let path = dir.child("counted");
    std::fs::write(&path, vec![7u8; 64 * 1024]).expect("seeding the file");

    let driver = IocpDriver::new().expect("a completion port");
    futures::executor::block_on(async {
        let file = Arc::new(
            driver
                .open(
                    lore_io::file::OpenOptions::new().read(true).to_std(),
                    path.clone(),
                )
                .await
                .expect("opening the file"),
        );
        for offset in [0, 16 * 1024, 48 * 1024] {
            let bytes = driver
                .read_at(Arc::clone(&file), 16 * 1024, offset)
                .await
                .expect("reading the file");
            assert_eq!(bytes.len(), 16 * 1024);
            assert!(bytes.iter().all(|byte| *byte == 7));
        }
    });

    let stats = driver.stats();
    assert_eq!(stats.submits, 3, "{stats:?}");
    assert_eq!(stats.submits, stats.inline + stats.reaped, "{stats:?}");
    let _ = std::fs::remove_file(&path);
}

/// The completion-port backend writes unbuffered: a file it opens unbuffered takes aligned
/// overlapped writes cut to its length, and its unbuffered whole-file write holds exactly the
/// content, written unbuffered.
#[tokio::test]
async fn the_completion_port_backend_writes_unbuffered() {
    use lore_io::AlignedBuf;
    use lore_io::BackendKind;
    use lore_io::IoDriver;
    use lore_io::OpenOptions;
    use lore_io::UNBUFFERED_ALIGNMENT;

    let driver = IoDriver::new(BackendKind::Iocp).expect("make the completion-port driver");
    let dir = lore_base::test_util::TempDir::new("lore-io-iocp-unbuffered-");
    let content: Vec<u8> = (0..2 * UNBUFFERED_ALIGNMENT + 123)
        .map(|index| (index % 251) as u8)
        .collect();

    let unbuffered_file = dir.child("unbuffered_file");
    let file = driver
        .open(
            &unbuffered_file,
            &OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .unbuffered(true),
        )
        .await
        .expect("open unbuffered");
    let mut buffer = AlignedBuf::zeroed(content.len());
    buffer.content_mut().copy_from_slice(&content);
    file.write_all_at(buffer, 0)
        .await
        .expect("write unbuffered");
    file.set_len(content.len() as u64, ())
        .await
        .expect("set the end of the file");
    drop(file);
    assert_eq!(std::fs::read(&unbuffered_file).expect("read back"), content);

    let whole = dir.child("whole");
    let (metadata, unbuffered) = driver
        .write_file_bytes_unbuffered(&whole, bytes::Bytes::from(content.clone()), false)
        .await
        .expect("write the whole file");
    assert!(unbuffered);
    assert_eq!(metadata.len(), content.len() as u64);
    assert_eq!(std::fs::read(&whole).expect("read back"), content);
}
