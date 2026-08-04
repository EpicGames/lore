// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod test_util;

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::str::FromStr;

    use lore::file::LoreFileStageArgs;
    use lore::file::LoreFileWriteArgs;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::revision::LoreRevisionCommitArgs;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use rand::Rng;
    use rand::distr::Alphanumeric;

    use super::test_util::TempDir;

    async fn setup_committed_file(
        globals: &LoreGlobalArgs,
        repository_path: &std::path::Path,
        payload: &[u8],
    ) -> std::path::PathBuf {
        let name: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();
        let mut url = String::from_str("lore://localhost/").unwrap_or_default();
        url.push_str(name.as_str());
        let args = LoreRepositoryCreateArgs {
            repository_url: url.into(),
            id: LoreString::default(),
            description: LoreString::default(),
            default_branch_name: LoreString::default(),
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        };
        let result = lore::repository::create(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to create repository");

        let file_path = repository_path.join("payload.bin");
        {
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(file_path.as_path())
                .expect("Failed to create payload file");
            file.write_all(payload)
                .expect("Failed to write payload file");
        }

        let lore_path = LoreString::from(file_path.as_path());
        let args = LoreFileStageArgs {
            paths: LoreArray::from_vec(vec![lore_path.clone()]),
            case_change: 0,
            scan: 0,
        };
        let result = lore::file::stage(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to stage payload");

        let args = LoreRevisionCommitArgs {
            message: LoreString::from("Commit payload"),
            ..Default::default()
        };
        let result = lore::revision::commit(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to commit payload");

        file_path
    }

    /// Output destination is outside the repository — exercises the new
    /// `repository_call_read` dispatch added when the write target lies
    /// outside the repo.
    #[tokio::test]
    async fn write_to_outside_repo_destination() {
        let repo_dir = TempDir::new("lore-file-write-outside-repo-");
        let out_dir = TempDir::new("lore-file-write-outside-out-");
        let repository_path = repo_dir.path().to_path_buf();

        let globals = LoreGlobalArgs {
            repository_path: repository_path.as_path().into(),
            offline: 1,
            identity: "test-user".into(),
            ..Default::default()
        };

        let mut payload = [0u8; 1024];
        rand::rng().fill(&mut payload[..]);

        let file_path = setup_committed_file(&globals, &repository_path, &payload).await;

        let output_path = out_dir.path().join("payload.out");
        let args = LoreFileWriteArgs {
            address: LoreString::default(),
            path: file_path.as_path().into(),
            revision: LoreString::default(),
            output: output_path.as_path().into(),
        };
        let result = lore::file::write(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to write to outside-repo destination");

        let written = std::fs::read(&output_path).expect("Unable to read outside-repo output file");
        assert_eq!(
            written.as_slice(),
            &payload,
            "Outside-repo write content mismatch"
        );
    }

    /// Output destination is inside the repository — exercises the preserved
    /// `repository_call_write` dispatch.
    #[tokio::test]
    async fn write_to_inside_repo_destination() {
        let repo_dir = TempDir::new("lore-file-write-inside-repo-");
        let repository_path = repo_dir.path().to_path_buf();

        let globals = LoreGlobalArgs {
            repository_path: repository_path.as_path().into(),
            offline: 1,
            identity: "test-user".into(),
            ..Default::default()
        };

        let mut payload = [0u8; 1024];
        rand::rng().fill(&mut payload[..]);

        let file_path = setup_committed_file(&globals, &repository_path, &payload).await;

        let output_path = repository_path.join("payload.out");
        let args = LoreFileWriteArgs {
            address: LoreString::default(),
            path: file_path.as_path().into(),
            revision: LoreString::default(),
            output: output_path.as_path().into(),
        };
        let result = lore::file::write(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to write to inside-repo destination");

        let written = std::fs::read(&output_path).expect("Unable to read inside-repo output file");
        assert_eq!(
            written.as_slice(),
            &payload,
            "Inside-repo write content mismatch"
        );
    }
}
