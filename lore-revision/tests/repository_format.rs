// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use lore_revision::repository::DOT_LORE;
    use lore_revision::repository::DOT_LOREIGNORE;
    use lore_revision::repository::DOT_URC;
    use lore_revision::repository::DOT_URCIGNORE;
    use lore_revision::repository::RepositoryFormat;
    use lore_revision::repository::SALT_LORE;
    use lore_revision::repository::SALT_URC;

    #[test]
    fn format_salt_urc() {
        assert_eq!(RepositoryFormat::Urc.salt(), SALT_URC);
    }

    #[test]
    fn format_salt_lore() {
        assert_eq!(RepositoryFormat::Lore.salt(), SALT_LORE);
    }

    #[test]
    fn format_dot_dir() {
        assert_eq!(RepositoryFormat::Urc.dot_dir(), DOT_URC);
        assert_eq!(RepositoryFormat::Lore.dot_dir(), DOT_LORE);
    }

    #[test]
    fn format_ignore_file() {
        // Both formats use .loreignore as the primary ignore file; legacy
        // .urcignore is honored only as a load_filter fallback.
        assert_eq!(RepositoryFormat::Urc.ignore_file(), DOT_LOREIGNORE);
        assert_eq!(RepositoryFormat::Lore.ignore_file(), DOT_LOREIGNORE);
    }

    #[test]
    fn detect_urc_directory() {
        let dir = std::env::temp_dir().join("lore-test-detect-urc");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".urc")).expect("create .urc dir");

        let format = RepositoryFormat::detect(&dir);
        assert!(matches!(format, RepositoryFormat::Urc));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_lore_directory() {
        let dir = std::env::temp_dir().join("lore-test-detect-lore");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".spacesync")).expect("create .lore dir");

        let format = RepositoryFormat::detect(&dir);
        assert!(matches!(format, RepositoryFormat::Lore));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_neither_defaults_to_lore() {
        let dir = std::env::temp_dir().join("lore-test-detect-neither");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");

        let format = RepositoryFormat::detect(&dir);
        assert!(matches!(format, RepositoryFormat::Lore));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_both_prefers_urc() {
        let dir = std::env::temp_dir().join("lore-test-detect-both");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".urc")).expect("create .urc dir");
        std::fs::create_dir_all(dir.join(".spacesync")).expect("create .lore dir");

        let format = RepositoryFormat::detect(&dir);
        assert!(matches!(format, RepositoryFormat::Urc));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_salt_divergence_between_formats() {
        // Same function name, different salts -> different keys (R2, R11)
        let key_urc = lore_storage::hash::hash_function(b"urc", "test_func");
        let key_lore = lore_storage::hash::hash_function(b"lore", "test_func");
        assert_ne!(key_urc, key_lore);

        // Same salt -> same key (deterministic)
        let key_urc2 = lore_storage::hash::hash_function(b"urc", "test_func");
        assert_eq!(key_urc, key_urc2);
    }

    #[test]
    fn discovery_finds_lore_directory() {
        // Simulate directory walk: a parent with .lore/ should be found
        let base = std::env::temp_dir().join("lore-test-discovery-lore");
        let _ = std::fs::remove_dir_all(&base);
        let nested = base.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("create nested dirs");
        std::fs::create_dir_all(base.join(".spacesync")).expect("create .lore dir");

        // Walk up from nested, looking for .lore or .urc
        let mut current = nested.as_path();
        let found = loop {
            if current.join(".urc").is_dir() || current.join(".spacesync").is_dir() {
                break Some(current.to_path_buf());
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => break None,
            }
        };
        assert_eq!(found, Some(base.clone()));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn discovery_finds_urc_directory() {
        let base = std::env::temp_dir().join("lore-test-discovery-urc");
        let _ = std::fs::remove_dir_all(&base);
        let nested = base.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("create nested dirs");
        std::fs::create_dir_all(base.join(".urc")).expect("create .urc dir");

        let mut current = nested.as_path();
        let found = loop {
            if current.join(".urc").is_dir() || current.join(".spacesync").is_dir() {
                break Some(current.to_path_buf());
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => break None,
            }
        };
        assert_eq!(found, Some(base.clone()));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_filter_lore_format_falls_back_to_urcignore() {
        use lore_revision::repository::load_filter;

        let dir = std::env::temp_dir().join("lore-test-ignore-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".spacesync")).expect("create .lore dir");

        // Write a .urcignore with a pattern (no .loreignore present)
        std::fs::write(dir.join(".urcignore"), "secret.txt\n").expect("write .urcignore");

        let filter = load_filter(&dir).expect("filter should load");
        // The ignore filter should contain user-defined rules from .urcignore
        // plus auto-generated exclusions (.urc, .lore, conflict suffixes).
        // With one user rule ("secret.txt") and 6 auto-generated rules, we expect 7 lines.
        assert_eq!(filter.ignore.lines.len(), 7);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_filter_lore_format_prefers_loreignore() {
        use lore_revision::repository::load_filter;

        let dir = std::env::temp_dir().join("lore-test-ignore-prefer");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".spacesync")).expect("create .lore dir");

        // Both files present — .loreignore should win
        std::fs::write(dir.join(".loreignore"), "a.txt\nb.txt\n").expect("write .loreignore");
        std::fs::write(dir.join(".urcignore"), "secret.txt\n").expect("write .urcignore");

        let filter = load_filter(&dir).expect("filter should load");
        // 2 user rules from .loreignore + 6 auto-generated = 8
        assert_eq!(filter.ignore.lines.len(), 8);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_filter_urc_format_falls_back_to_urcignore() {
        use lore_revision::repository::load_filter;

        // The .urcignore fallback is universal: even a legacy .urc-format
        // repository (whose primary ignore file is now also .loreignore) loads
        // a lone .urcignore when no .loreignore is present.
        let dir = std::env::temp_dir().join("lore-test-ignore-fallback-urc");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(DOT_URC)).expect("create .urc dir");

        std::fs::write(dir.join(DOT_URCIGNORE), "secret.txt\n").expect("write .urcignore");

        let filter = load_filter(&dir).expect("filter should load");
        // One user rule ("secret.txt") + 6 auto-generated rules = 7 lines.
        assert_eq!(filter.ignore.lines.len(), 7);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
