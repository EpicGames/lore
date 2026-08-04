// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod test_util;

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::str::FromStr;
    use std::sync::Arc;

    use lore::file::LoreFileMetadataGetArgs;
    use lore::file::LoreFileMetadataListArgs;
    use lore::file::LoreFileMetadataSetArgs;
    use lore::file::LoreFileStageArgs;
    use lore::file::LoreFileWriteArgs;
    use lore::interface::LoreMetadataType;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::revision::LoreRevisionCommitArgs;
    use lore::revision::LoreRevisionMetadataGetArgs;
    use lore::revision::LoreRevisionMetadataListArgs;
    use lore::revision::LoreRevisionMetadataSetArgs;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreMetadata;
    use lore_revision::interface::LoreString;
    use parking_lot::Mutex;
    use rand::Rng;
    use rand::distr::Alphanumeric;

    use super::test_util::TempDir;

    #[tokio::test]
    async fn metadata_revision() {
        let tempdir = TempDir::new("lore-stage-test-");
        let repository_path = tempdir.path().to_path_buf();

        let globals = LoreGlobalArgs {
            repository_path: repository_path.as_path().into(),
            offline: 1,
            identity: "test-user".into(),
            ..Default::default()
        };

        // repository create
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

        // metadata set
        let keys = vec![LoreString::from("foo"), LoreString::from("jack")];
        let values = vec![LoreString::from("bar"), LoreString::from("pot")];
        let formats = vec![LoreMetadataType::String, LoreMetadataType::String];
        assert_eq!(keys.len(), values.len());

        let args = LoreRevisionMetadataSetArgs {
            keys: LoreArray::from_vec(keys.clone()),
            values: LoreArray::from_vec(values.clone()),
            formats: LoreArray::from_vec(formats),
        };

        let result = lore::revision::metadata_set(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to set metadata");

        // revision commit
        let args = LoreRevisionCommitArgs {
            message: LoreString::from("A commit that contains revision metadata"),
            ..Default::default()
        };

        let result = lore::revision::commit(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to commit");

        // metadata list
        let keys_received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let values_received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let keys_received_ = Arc::clone(&keys_received);
        let values_received_ = Arc::clone(&values_received);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                if let LoreMetadata::String(value) = &data.value {
                    keys_received_.lock().push(data.key.as_str().to_string());
                    values_received_.lock().push(value.as_str().to_string());
                }
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreRevisionMetadataListArgs {
            revision: LoreString::default(),
        };

        let result = lore::revision::metadata_list(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Failed to list metadata");

        assert!(keys_received.lock().len() >= keys.len());
        assert!(values_received.lock().len() >= values.len());

        #[allow(clippy::needless_range_loop)]
        for i in 0..keys.len() {
            assert_eq!(
                keys[i].as_str(),
                keys_received.lock()[i].as_str(),
                "Metadata key mismatch"
            );

            assert_eq!(
                values[i].as_str(),
                values_received.lock()[i].as_str(),
                "Metadata value mismatch"
            );
        }

        // metadata get
        let key_received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let value_received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        let key_received_ = Arc::clone(&key_received);
        let value_received_ = Arc::clone(&value_received);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                if let LoreMetadata::String(value) = &data.value {
                    *key_received_.lock() = data.key.to_string();
                    *value_received_.lock() = value.to_string();
                }
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreRevisionMetadataGetArgs {
            key: LoreString::from("foo"),
            revision: LoreString::default(),
        };

        let result = lore::revision::metadata_get(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Failed to get metadata");

        let key1 = key_received.lock();
        let key2 = "foo";
        assert_eq!(key1.as_str(), key2, "Metadata key mismatch");

        let value1 = value_received.lock();
        let value2 = "bar";
        assert_eq!(value1.as_str(), value2, "Metadata value mismatch");
    }

    #[tokio::test]
    async fn metadata_file() {
        let tempdir = TempDir::new("lore-stage-test-");
        let repository_path = tempdir.path().to_path_buf();

        let globals = LoreGlobalArgs {
            repository_path: repository_path.as_path().into(),
            offline: 1,
            identity: "test-user".into(),
            ..Default::default()
        };

        // repository initialize
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

        // create file
        let file_path = repository_path.as_path().join("test.file");
        {
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(file_path.as_path())
                .expect("Failed to create test file");
            file.write_all(&[0, 1, 2, 3, 4])
                .expect("Failed to write test file");
        }

        // create thumbnail
        let thumbnail_path = repository_path.as_path().join("thumbnail.file");
        {
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(thumbnail_path.as_path())
                .expect("Failed to create thumbnail file");

            let mut payload = [0u8; 4096];
            rand::rng().fill(&mut payload[..]);

            file.write_all(&payload)
                .expect("Failed to write thumbnail file");
        }

        // file stage
        let file_path = LoreString::from(file_path);
        let paths = LoreArray::from_vec(vec![file_path.clone()]);
        let args = LoreFileStageArgs {
            paths,
            case_change: 0,
            scan: 1,
        };
        let result = lore::file::stage(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to stage file");

        // metadata set strings
        let paths = vec![file_path.clone()];
        let keys = vec![
            LoreString::from("asset_name"),
            LoreString::from("asset_type"),
        ];
        let values = vec![LoreString::from("Large Desk"), LoreString::from("Actor")];
        let formats = vec![LoreMetadataType::String, LoreMetadataType::String];
        let entries = vec![2];
        assert_eq!(keys.len(), values.len());

        let args = LoreFileMetadataSetArgs {
            paths: LoreArray::from_vec(paths),
            keys: LoreArray::from_vec(keys.clone()),
            values: LoreArray::from_vec(values.clone()),
            formats: LoreArray::from_vec(formats),
            entries: LoreArray::from_vec(entries),
        };

        let result = lore::file::metadata_set(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to set string metadata");

        // metadata list
        let keys_received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let values_received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let keys_received_ = Arc::clone(&keys_received);
        let values_received_ = Arc::clone(&values_received);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                if let LoreMetadata::String(value) = &data.value {
                    keys_received_.lock().push(data.key.as_str().to_string());
                    values_received_.lock().push(value.as_str().to_string());
                }
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreFileMetadataListArgs {
            path: file_path.clone(),
            revision: LoreString::default(),
        };

        let result = lore::file::metadata_list(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Failed to list metadata");

        assert_eq!(keys.len(), keys_received.lock().len());
        assert_eq!(values.len(), values_received.lock().len());

        #[allow(clippy::needless_range_loop)]
        for i in 0..keys.len() {
            assert_eq!(
                keys[i].as_str(),
                keys_received.lock()[i].as_str(),
                "Metadata key mismatch"
            );

            assert_eq!(
                values[i].as_str(),
                values_received.lock()[i].as_str(),
                "Metadata value mismatch"
            );
        }

        // metadata get strings
        let key_received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let value_received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        let key_received_ = Arc::clone(&key_received);
        let value_received_ = Arc::clone(&value_received);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                if let LoreMetadata::String(value) = &data.value {
                    *key_received_.lock() = data.key.as_str().to_string();
                    *value_received_.lock() = value.as_str().to_string();
                }
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreFileMetadataGetArgs {
            path: file_path.clone(),
            key: LoreString::from("asset_name"),
            revision: LoreString::default(),
        };

        let result = lore::file::metadata_get(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Failed to get metadata");

        let key1 = key_received.lock().clone();
        let key2 = "asset_name";
        assert_eq!(key1.as_str(), key2, "Metadata key mismatch");

        let value1 = value_received.lock().clone();
        let value2 = "Large Desk";
        assert_eq!(value1.as_str(), value2, "Metadata value mismatch");

        // metadata set binary
        let paths = vec![file_path.clone()];
        let keys = vec![LoreString::from("thumbnail")];
        let values = vec![thumbnail_path.as_path().into()];
        let formats = vec![LoreMetadataType::Binary];
        let entries = vec![1];
        assert_eq!(keys.len(), values.len());

        let args = LoreFileMetadataSetArgs {
            paths: LoreArray::from_vec(paths),
            keys: LoreArray::from_vec(keys),
            values: LoreArray::from_vec(values),
            formats: LoreArray::from_vec(formats),
            entries: LoreArray::from_vec(entries),
        };

        let result = lore::file::metadata_set(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to set binary metadata");

        // revision commit
        let args = LoreRevisionCommitArgs {
            message: LoreString::from("A commit that contains file metadata"),
            ..Default::default()
        };

        let result = lore::revision::commit(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to commit");

        // metadata get binary
        let key_received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let value_received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        let key_received_ = Arc::clone(&key_received);
        let value_received_ = Arc::clone(&value_received);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::Metadata(data) => {
                if let LoreMetadata::Address(value) = data.value {
                    *key_received_.lock() = data.key.as_str().to_string();
                    *value_received_.lock() = value.to_string();
                }
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreFileMetadataGetArgs {
            path: file_path.clone(),
            key: LoreString::from("thumbnail"),
            revision: LoreString::default(),
        };

        let result = lore::file::metadata_get(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Failed to get metadata");

        let key1 = key_received.lock().clone();
        let key2 = "thumbnail";
        assert_eq!(key1.as_str(), key2, "Metadata key mismatch");

        let value1 = value_received.lock().clone();
        assert_eq!(value1.len(), 97, "Metadata value length mismatch");

        // metadata file write
        let output_path = repository_path.join("thumbnail.output");

        let args = LoreFileWriteArgs {
            address: value1.into(),
            path: LoreString::default(),
            revision: LoreString::default(),
            output: output_path.as_path().into(),
        };

        let result = lore::file::write(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to write thumbnail");

        let thumbnail_input =
            std::fs::read(&thumbnail_path).expect("Unable to read input thumbnail");
        let thumbnail_output =
            std::fs::read(&output_path).expect("Unable to read output thumbnail");
        assert_eq!(
            thumbnail_input,
            thumbnail_output,
            "Failed to download original thumbnail data, output file {:?}",
            std::fs::metadata(&output_path)
        );
    }
}
