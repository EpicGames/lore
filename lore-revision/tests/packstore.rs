// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_storage::packstore::*;

    include!("helper.rs");

    #[tokio::test]
    async fn memory_store_load_size() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let store = PackStore::new(None, 4, None);

                let arr = [0, 1, 2, 3];
                let packref = store
                    .store(bytes::Bytes::copy_from_slice(&arr))
                    .await
                    .expect("Failed store to packfile");
                assert_eq!(
                    packref.offset, 0,
                    "First store did not complete at offset 0 as expected",
                );

                let buffer = store
                    .load(packref.id, packref.offset, 4)
                    .await
                    .expect("Failed load from packfile");
                assert_eq!(
                    buffer.len(),
                    arr.len(),
                    "First store did not complete at offset 0 as expected",
                );

                assert_eq!(
                    buffer.as_ref(),
                    arr,
                    "Load did not yield same data as was stored"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn file_store_load_size() {
        let execution = setup_test_execution();
        LORE_CONTEXT
            .scope(execution, async move {
                let tempdir = generate_tempdir();
                let dir = tempdir.to_path_buf();
                let store = PackStore::new(Some(dir), 4, None);

                let arr = [0, 1, 2, 3];
                let packref = store
                    .store(bytes::Bytes::copy_from_slice(&arr))
                    .await
                    .expect("Failed store to packfile");
                assert_eq!(
                    packref.offset, 0,
                    "First store did not complete at offset 0 as expected",
                );

                let buffer = store
                    .load(packref.id, packref.offset, 4)
                    .await
                    .expect("Failed load from packfile");
                assert_eq!(
                    buffer.len(),
                    arr.len(),
                    "First store did not complete at offset 0 as expected",
                );

                assert_eq!(
                    buffer.as_ref(),
                    arr,
                    "Load did not yield same data as was stored"
                );
            })
            .await;
    }
}
