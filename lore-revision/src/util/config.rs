// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! TOML configuration files that survive an interrupted write.
//!
//! Lore keeps four of these: the global config, the repository config, the layer set, and the
//! shared-store config. None of them is stored in a revision, pushed, or cloned, so each is the
//! only record of what it holds and a lost one cannot be recovered by syncing. They share one
//! implementation rather than one per owner so that the two rules that keep them recoverable —
//! a write that cannot truncate the target, and absent being the only thing that means defaults
//! — are decided here once instead of drifting apart per call site.

use std::path::Path;
use std::path::PathBuf;

use lore_base::fs::lock::FSLock;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

#[error_set]
pub enum LoadError {}

#[error_set]
pub enum SaveError {}

/// Path [`save`] writes before renaming it over `path`.
///
/// A failed save leaves this file behind, so a partially written configuration stays recoverable.
/// The name is derived from the target rather than randomized so that a later load can find it
/// and name it — see [`recovery_hint`]. That is only safe while writers of a given config are
/// serialized: the repository flock covers the repository and layer configs, and `<path>.lock`
/// covers the global config. The shared-store config is written without one, so two processes
/// creating or migrating the same shared store can still race on its temporary.
fn temp_path(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    PathBuf::from(temp)
}

/// Suffix naming the temporary file when one is present, for appending to a load error.
///
/// Probed synchronously rather than through the I/O engine: this runs only while building an
/// error that is about to abort the command, so the one stat is not worth a dispatch, and
/// [`load_blocking`] needs a synchronous probe regardless.
fn recovery_hint(path: &Path) -> String {
    let temp_path = temp_path(path);
    if temp_path.is_file() {
        format!(
            ", an interrupted save left {} which may hold a recoverable copy",
            temp_path.display()
        )
    } else {
        String::new()
    }
}

fn read_error(path: &Path, err: &std::io::Error) -> LoadError {
    LoadError::internal(format!(
        "failed to read config {}: {err}{}",
        path.display(),
        recovery_hint(path)
    ))
}

/// Decodes and parses a config body, naming the file and any recoverable temporary on failure.
fn parse<ConfigType: Default + Serialize + for<'a> Deserialize<'a>>(
    bytes: &[u8],
    path: &Path,
) -> Result<ConfigType, LoadError> {
    let Ok(config) = std::str::from_utf8(bytes) else {
        return Err(LoadError::internal(format!(
            "config {} is not valid UTF-8{}",
            path.display(),
            recovery_hint(path)
        )));
    };
    toml::from_str(config).map_err(|err| {
        LoadError::internal(format!(
            "failed to parse config {}: {err}{}",
            path.display(),
            recovery_hint(path)
        ))
    })
}

/// Reads a TOML configuration, defaulting when the file is absent.
///
/// Absent is the only case that means defaults. A file that is present but unreadable, or
/// unreadable as text or TOML, is an error, since an empty string parses as empty TOML and would
/// otherwise hand back a default configuration indistinguishable from a deliberate one — which
/// the next [`save`] would then write back, making the loss permanent.
pub async fn load<ConfigType: Default + Serialize + for<'a> Deserialize<'a>>(
    path: impl AsRef<Path>,
) -> Result<ConfigType, LoadError> {
    let path = path.as_ref();
    match lore_io::IoDriver::global().read_file_bytes(path).await {
        Ok(bytes) => parse(bytes.as_ref(), path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ConfigType::default()),
        Err(err) => Err(read_error(path, &err)),
    }
}

/// Synchronous twin of [`load`], for callers that read a config before the runtime is doing
/// anything else.
///
/// The repository config is read on the startup path of every command, where a tiny file is
/// cheaper to read inline than to hand to the I/O engine: the dispatch costs a thread hop and
/// can queue behind store flush tasks still in flight from the previous command. It defaults and
/// errors on exactly the same cases as [`load`], so the two cannot drift.
pub fn load_blocking<ConfigType: Default + Serialize + for<'a> Deserialize<'a>>(
    path: impl AsRef<Path>,
) -> Result<ConfigType, LoadError> {
    let path = path.as_ref();
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes, path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ConfigType::default()),
        Err(err) => Err(read_error(path, &err)),
    }
}

/// [`load`] holding an exclusive lock on `path`, for a read the caller intends to write back.
///
/// The lock is a `<path>.lock` sibling, so it is unaffected by [`save`] replacing the config's
/// directory entry.
pub async fn load_with_lock<ConfigType: Default + Serialize + for<'a> Deserialize<'a>>(
    path: impl AsRef<Path>,
) -> Result<(ConfigType, FSLock), LoadError> {
    let path = path.as_ref();
    let lock = FSLock::acquire_file_lock(path).await.map_err(|err| {
        LoadError::internal(format!("failed to lock config {}: {err}", path.display()))
    })?;
    let config = load(path).await?;
    Ok((config, lock))
}

/// Writes a TOML configuration so a reader sees either the previous contents or the new ones.
///
/// The body lands in a temporary file that is flushed to disk before being renamed over the
/// target, so an interrupted save cannot truncate the target. `std::fs::rename` replaces an
/// existing destination on every supported platform, Windows included, where it maps to
/// `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`.
///
/// Every failure names the file it was working on and says which of the two holds the good
/// copy, because that is the message a user gets when a save goes wrong — a load error, where
/// [`recovery_hint`] would say the same thing, no longer happens for an interrupted save now
/// that the target is never truncated.
pub async fn save<ConfigType: Serialize>(
    config: &ConfigType,
    path: impl AsRef<Path>,
) -> Result<(), SaveError> {
    let path = path.as_ref();
    let config_string = toml::to_string_pretty(&config).map_err(|err| {
        SaveError::internal(format!(
            "failed to format config {} as TOML: {err}",
            path.display()
        ))
    })?;
    let temp_path = temp_path(path);

    lore_io::IoDriver::global()
        .write_file_bytes(&temp_path, bytes::Bytes::from(config_string), true)
        .await
        .map_err(|err| {
            SaveError::internal(format!(
                "failed to write config {}: {err}, {} is unchanged",
                temp_path.display(),
                path.display()
            ))
        })?;

    lore_io::IoDriver::global()
        .rename(&temp_path, path)
        .await
        .map_err(|err| {
            SaveError::internal(format!(
                "failed to rename {} onto {}: {err}, the new configuration is in the temporary file",
                temp_path.display(),
                path.display()
            ))
        })?;

    Ok(())
}

#[cfg(test)]
// Fixtures write config files directly; what these test is how loading and saving read them.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[derive(Default, Serialize, Deserialize, PartialEq, Debug)]
    struct Settings {
        name: String,
    }

    #[tokio::test]
    async fn an_absent_config_is_the_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config: Settings = load(dir.path().join("absent.toml"))
            .await
            .expect("an absent config defaults");
        assert_eq!(config, Settings::default());
    }

    #[tokio::test]
    async fn a_config_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"name = \"configured\"\n").expect("write config");

        let config: Settings = load(&path).await.expect("a valid config loads");
        assert_eq!(config.name, "configured");
    }

    /// A present file that is not text is an error rather than a default: an empty string parses
    /// as empty TOML, so defaulting here would be indistinguishable from a deliberate default.
    #[tokio::test]
    async fn a_config_that_is_not_text_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x80]).expect("write config");

        assert!(load::<Settings>(&path).await.is_err());
    }

    #[tokio::test]
    async fn a_config_that_is_not_toml_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"this is not toml = = =").expect("write config");

        assert!(load::<Settings>(&path).await.is_err());
    }

    /// A present-but-unreadable config must not read as a default, or the next save would write
    /// that default back over a configuration that was merely inaccessible.
    #[tokio::test]
    async fn a_config_that_cannot_be_read_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::create_dir(&path).expect("occupy the config path");

        assert!(load::<Settings>(&path).await.is_err());
    }

    #[test]
    fn a_blocking_load_of_an_absent_config_is_the_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config: Settings =
            load_blocking(dir.path().join("absent.toml")).expect("an absent config defaults");
        assert_eq!(config, Settings::default());
    }

    /// The blocking loader is the one the repository config uses on the startup path, and it
    /// used to default on every read failure — including a `.lore` that could not be opened,
    /// which presented as a repository with no remote.
    #[test]
    fn a_blocking_load_of_a_config_that_cannot_be_read_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::create_dir(&path).expect("occupy the config path");

        assert!(load_blocking::<Settings>(&path).is_err());
    }

    #[tokio::test]
    async fn a_save_replaces_the_config_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"name = \"original\"\n").expect("write config");

        save(
            &Settings {
                name: "replacement".to_owned(),
            },
            &path,
        )
        .await
        .expect("a save replaces the config");

        let config: Settings = load(&path).await.expect("the saved config loads");
        assert_eq!(config.name, "replacement");
        assert!(!temp_path(&path).exists());
    }

    /// Occupying the temporary path with a directory fails the write that precedes the rename,
    /// standing in for a crash or a full disk at the same point.
    #[tokio::test]
    async fn a_save_that_fails_before_the_rename_keeps_the_previous_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"name = \"original\"\n").expect("write config");
        std::fs::create_dir(temp_path(&path)).expect("occupy the temporary path");

        assert!(
            save(
                &Settings {
                    name: "replacement".to_owned(),
                },
                &path,
            )
            .await
            .is_err()
        );

        let config: Settings = load(&path).await.expect("the previous config loads");
        assert_eq!(config.name, "original");
    }

    /// A save failure is the only message a user gets for an interrupted write, so it has to say
    /// which file it was writing and which one still holds a good copy.
    #[tokio::test]
    async fn a_failed_save_names_the_target_and_the_temporary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"name = \"original\"\n").expect("write config");
        std::fs::create_dir(temp_path(&path)).expect("occupy the temporary path");

        let error = save(
            &Settings {
                name: "replacement".to_owned(),
            },
            &path,
        )
        .await
        .expect_err("a save onto an occupied temporary path fails");

        let message = error.to_string();
        assert!(
            message.contains(&temp_path(&path).display().to_string()),
            "the error must name the temporary it failed to write: {message}"
        );
        assert!(
            message.contains(&path.display().to_string()),
            "the error must name the config it was saving: {message}"
        );
    }
}
