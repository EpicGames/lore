// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Compile-time probe: vectored I/O futures must be Send so callers can
//! spawn them on multi-threaded runtimes.

fn assert_send<T: Send>(value: T) -> T {
    value
}

#[tokio::test]
async fn vectored_futures_are_send() {
    let dir = std::env::temp_dir().join(format!("lore-io-sendprobe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let driver = lore_io::IoDriver::new(lore_io::BackendKind::Auto).unwrap();
    let file = driver
        .open(
            dir.join("data"),
            &lore_io::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true),
        )
        .await
        .unwrap();
    let written = assert_send(file.write_all_vectored_at(vec![vec![7u8; 64]], 0))
        .await
        .unwrap();
    let _read = assert_send(file.read_exact_vectored_at(written, 0))
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
