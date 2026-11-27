// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Coverage for the repository a rename addresses ([`rename_target`]), and for the name check
//! [`rename`] makes before it reaches the remote.
use std::str::FromStr;

use lore_base::error::Disconnected;
use lore_base::error::RepositoryRenameIncomplete;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::CONFIG;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::ID;
use lore_revision::repository::reference::RepositoryReference;
use lore_revision::repository::rename::RenameError;
use lore_revision::repository::rename::rename;
use lore_revision::repository::rename::rename_error;
use lore_revision::repository::rename::rename_target;
use lore_transport::RepositoryRenameError;
use zerocopy::IntoBytes;

use crate::helper::setup_test_execution;

const INSTANCE_REMOTE: &str = "lore://instance-remote:41338";
const ID_TEXT: &str = "019e3be9efcd761194d2fbcbb44d249f";

fn id() -> RepositoryId {
    RepositoryId::from_str(ID_TEXT).expect("the ID parses")
}

/// An instance of the repository [`id`], whose config names `remote_url` when given, in a
/// directory named for the test with `prefix`.
#[allow(clippy::disallowed_methods)]
fn instance(prefix: &str, remote_url: Option<&str>) -> TempDir {
    let dir = TempDir::new(prefix);
    let dot_lore = dir.child(DOT_LORE);
    std::fs::create_dir(&dot_lore).expect("making the Lore directory");
    std::fs::write(dot_lore.join(ID), id().as_bytes()).expect("writing the ID");
    let config = remote_url.map_or_else(String::new, |url| format!("remote_url = \"{url}\"\n"));
    std::fs::write(dot_lore.join(CONFIG), config).expect("writing the config");
    dir
}

fn path(dir: &TempDir) -> &str {
    dir.path().to_str().expect("a UTF-8 path")
}

/// A URL names its own remote, whatever the instance's is, and a name or ID on it.
#[test]
fn a_url_names_its_remote_and_a_name_or_an_id() {
    let dir = instance(
        "a-url-names-its-remote-and-a-name-or-an-id-",
        Some(INSTANCE_REMOTE),
    );

    let by_name = rename_target(Some("lore://other:1234/org/project"), path(&dir))
        .expect("a URL with a name resolves");
    let by_id = rename_target(Some(&format!("lore://other:1234/{ID_TEXT}")), path(&dir))
        .expect("a URL with an ID resolves");

    assert_eq!(
        by_name,
        (
            "lore://other:1234".to_string(),
            RepositoryReference::Name("org/project".to_string())
        )
    );
    assert_eq!(
        by_id,
        (
            "lore://other:1234".to_string(),
            RepositoryReference::Id(id())
        )
    );
}

/// A bare name or ID, without a scheme, is a repository on the instance's remote. An ID is kept
/// as an ID, so the rename sends it without a lookup by name.
#[test]
fn a_bare_name_or_id_is_on_the_remote_of_the_instance() {
    let dir = instance(
        "a-bare-name-or-id-is-on-the-remote-of-the-instance-",
        Some(INSTANCE_REMOTE),
    );

    let by_name = rename_target(Some("org/project"), path(&dir)).expect("a bare name resolves");
    let by_id = rename_target(Some(ID_TEXT), path(&dir)).expect("a bare ID resolves");

    assert_eq!(
        by_name,
        (
            INSTANCE_REMOTE.to_string(),
            RepositoryReference::Name("org/project".to_string())
        )
    );
    assert_eq!(
        by_id,
        (INSTANCE_REMOTE.to_string(), RepositoryReference::Id(id()))
    );
}

/// Without a URL, the rename addresses the repository of the instance by the ID its Lore
/// directory records, on the instance's remote.
#[test]
fn no_url_is_the_repository_of_the_instance() {
    let dir = instance(
        "no-url-is-the-repository-of-the-instance-",
        Some(INSTANCE_REMOTE),
    );

    let target = rename_target(None, path(&dir)).expect("the instance's repository resolves");

    assert_eq!(
        target,
        (INSTANCE_REMOTE.to_string(), RepositoryReference::Id(id()))
    );
}

/// A bare name, or no URL at all, needs a remote to resolve against. A bare name in an instance
/// without a remote, or at a path that holds no instance, and no URL in an instance without a
/// remote, are `NoRemote` and not a malformed URL.
#[test]
fn a_bare_name_or_no_url_without_a_remote_is_no_remote() {
    let without_remote = instance("a-bare-name-or-no-url-without-a-remote-", None);
    let no_instance = TempDir::new("a-bare-name-or-no-url-without-a-remote-no-instance-");

    let bare_name = rename_target(Some("project"), path(&without_remote))
        .expect_err("a bare name needs a remote");
    let no_url = rename_target(None, path(&without_remote)).expect_err("no URL needs a remote");
    let bare_name_outside = rename_target(Some("project"), path(&no_instance))
        .expect_err("a bare name outside an instance needs a remote");

    assert!(
        bare_name.is_no_remote() && no_url.is_no_remote() && bare_name_outside.is_no_remote(),
        "{bare_name:?}; {no_url:?}; {bare_name_outside:?}"
    );
}

/// Without a URL, a path that holds no instance has no repository to rename, which is
/// `InvalidPath`.
#[test]
fn no_url_outside_an_instance_is_invalid_path() {
    let no_instance = TempDir::new("no-url-outside-an-instance-");

    let error = rename_target(None, path(&no_instance)).expect_err("there is no instance");

    assert!(error.is_invalid_path(), "{error:?}");
}

/// A new name the remote would refuse is refused before the rename reaches the remote.
#[tokio::test]
async fn an_invalid_name_is_invalid_arguments() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            for name in ["", "repo name", "org//repo", ".hidden"] {
                let error = rename(Some("lore://127.0.0.1:1/project"), name, "")
                    .await
                    .expect_err("the name is invalid");

                assert!(error.is_invalid_arguments(), "{name:?}: {error:?}");
            }
        })
        .await;
}

/// An incomplete rename names the repository by the remote's URL and its ID, which a repeat
/// resolves to it whatever name the remote holds. Another failure keeps its variant.
#[test]
fn an_incomplete_rename_names_the_remote_and_the_id() {
    let incomplete = rename_error(
        RepositoryRenameError::from(RepositoryRenameIncomplete {
            repository: ID_TEXT.to_string(),
        }),
        INSTANCE_REMOTE,
        id(),
    );
    let disconnected = rename_error(
        RepositoryRenameError::from(Disconnected),
        INSTANCE_REMOTE,
        id(),
    );

    assert!(
        matches!(
            &incomplete,
            RenameError::RepositoryRenameIncomplete(error)
                if error.repository == format!("{INSTANCE_REMOTE}/{ID_TEXT}")
        ),
        "{incomplete:?}"
    );
    assert!(disconnected.is_disconnected(), "{disconnected:?}");
}
