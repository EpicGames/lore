// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod fan_out;
mod immutable_store;
pub(crate) mod mutable_store;
mod store_lock;

/// Run the flush `flush` starts, whose fan-out commit renames a `.new` file over the bucket file at
/// `blocked`, with a non-empty directory there, so that the commit fails at that rename once its
/// `level.pending` is on disk, and then remove the directory.
async fn fail_commit_at_rename<E, F: Future<Output = Result<(), E>>>(
    blocked: &std::path::Path,
    flush: impl FnOnce() -> F,
) {
    std::fs::create_dir_all(blocked).expect("block the bucket's rename");
    std::fs::write(blocked.join("blocker"), b"x").expect("block the bucket's rename");
    assert!(flush().await.is_err(), "the commit's rename fails");
    assert!(
        blocked
            .with_file_name(lore_storage::local::fan_out::LEVEL_PENDING_FILENAME)
            .exists(),
        "the commit failed before its level.pending"
    );
    std::fs::remove_dir_all(blocked).expect("unblock the bucket");
}

/// The bucket files under the index directory `root`, at any depth, by path, with the identity and
/// length of each, see [`lore_base::test_util::file_identity`]. Fails on a temporary file left
/// behind.
fn bucket_files(
    root: &std::path::Path,
) -> std::collections::HashMap<std::path::PathBuf, ((u64, u64), u64)> {
    let mut files = std::collections::HashMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("index directory").flatten() {
            let path = entry.path();
            let metadata = entry.metadata().expect("index entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.ends_with(lore_storage::local::fan_out::TEMPORARY_SUFFIX),
                "{} is left behind",
                path.display()
            );
            if metadata.is_dir() {
                dirs.push(path);
            } else if name.starts_with(lore_storage::local::fan_out::BUCKET_FILENAME_PREFIX) {
                let identity = lore_base::test_util::file_identity(&path);
                files.insert(path, (identity, metadata.len()));
            }
        }
    }
    files
}

/// Require each bucket file of `before` to be under `root` still, and those a flush since has
/// grown to have been written in place, keeping their identities, when `in_place`, and replaced by
/// a renamed file otherwise. At least one has to have grown.
fn assert_rewritten(
    root: &std::path::Path,
    before: &std::collections::HashMap<std::path::PathBuf, ((u64, u64), u64)>,
    in_place: bool,
) {
    let after = bucket_files(root);
    let mut grown = 0;
    for (path, (identity, len)) in before {
        let (after_identity, after_len) = after
            .get(path)
            .unwrap_or_else(|| panic!("{} is gone", path.display()));
        if after_len == len {
            continue;
        }
        grown += 1;
        assert_eq!(
            after_identity == identity,
            in_place,
            "{} was {} where in place is {in_place}",
            path.display(),
            if after_identity == identity {
                "written in place"
            } else {
                "replaced"
            }
        );
    }
    assert!(grown > 0, "the flush grew no bucket file");
}
