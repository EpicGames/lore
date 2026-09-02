// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod tests {
    use std::path::Path;

    use lore_revision::shared_store::registry::SharedStoreRegistry;
    use lore_revision::util::config::SaveableConfig;

    include!("helper.rs");

    #[tokio::test]
    async fn test_shared_store_registry() {
        let temp_dir = TempDir::new("shared_store_registry");
        let shared_store_path = temp_dir.path().join("registry.toml");

        let (mut registry, lock) = SharedStoreRegistry::load_locked_from_path(&shared_store_path)
            .await
            .expect("Should load default registry");

        assert!(registry.entries().is_empty());

        const PATH1: &str = "/tmp/path1";
        const REMOTE_URL1: &str = "http://localhost:8080/";
        const REMOTE_URL1B: &str = "http://localhost:8080/b";
        const PATH2: &str = "/tmp/path2";
        const REMOTE_URL2: &str = "http://localhost:9090/";
        let expected_paths = vec![(REMOTE_URL1, PATH1), (REMOTE_URL2, PATH2)];
        registry
            .register(REMOTE_URL1.into(), Path::new(PATH1))
            .unwrap();
        registry
            .register(REMOTE_URL1B.into(), Path::new(PATH1))
            .unwrap();
        registry
            .register(REMOTE_URL2.into(), Path::new(PATH2))
            .unwrap();

        assert_eq!(
            registry
                .entries()
                .iter()
                .map(|entry| (entry.remote_url(), entry.path()))
                .collect::<Vec<_>>(),
            expected_paths
        );

        registry
            .save_at_path(lock, &shared_store_path)
            .await
            .expect("Should save registry lock");

        assert_eq!(
            SharedStoreRegistry::load_from_path(&shared_store_path)
                .await
                .expect("Should load registry")
                .entries()
                .iter()
                .map(|entry| (entry.remote_url(), entry.path()))
                .collect::<Vec<_>>(),
            expected_paths
        );
    }
}
