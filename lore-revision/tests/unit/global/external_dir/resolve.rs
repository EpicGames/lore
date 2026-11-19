// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures plant roots and files directly; what these test is how a resolution reads them.
#![allow(clippy::disallowed_methods)]

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use lore_revision::global::external_dir::marker::Marker;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::resolve::test_util::Reads;
use lore_revision::global::external_dir::resolve::test_util::reads_in;
use lore_revision::global::external_dir::resolve::*;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::instance::InstanceId;
use lore_revision::repository::DOT_LORE;

use super::redirect;

/// A global data directory, and a directory beside it for configured roots.
struct Fixture {
    global: TempDir,
    volume: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            global: TempDir::new("lore-external-resolve-global-"),
            volume: TempDir::new("lore-external-resolve-volume-"),
        }
    }

    fn default_location(&self) -> PathBuf {
        self.global.path().join("external")
    }

    /// A root directory named `name` beside the global data directory, not yet created.
    fn root(&self, name: &str) -> PathBuf {
        self.volume.path().join(name)
    }

    fn resolve(&self) -> RootState {
        resolve_in(self.global.path().to_path_buf())
    }

    fn availability(&self) -> Availability {
        self.resolve().availability
    }
}

fn generation(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("non-zero")
}

/// Stores a setting that names `path`, with `id` and `generation`, in `global`.
async fn store_setting(global: &Path, path: &str, id: RootId, generation: NonZeroU64) {
    Setting {
        path: path.to_owned(),
        id,
        generation,
    }
    .save(global)
    .await
    .expect("save the setting");
}

/// Creates `root` with mode `0o700`, as Lore creates a root, holding a marker of `id` and
/// `generation`.
async fn marked_root(root: &Path, id: RootId, generation: NonZeroU64) {
    lore_io::create_dir_all_private(root).expect("create the root");
    Marker { id, generation }
        .save(root)
        .await
        .expect("save the marker");
}

/// A configured root, its setting and its marker, which agree.
async fn configured(fixture: &Fixture) -> (PathBuf, RootId) {
    let root = fixture.root("root");
    let id = RootId::generate();
    marked_root(&root, id, generation(1)).await;
    store_setting(
        fixture.global.path(),
        root.to_str().expect("UTF-8"),
        id,
        generation(1),
    )
    .await;
    (root, id)
}

/// With no setting and nothing at the default location, the root there is available and empty.
#[test]
fn no_setting_and_no_default_location_is_an_available_empty_root() {
    let fixture = Fixture::new();

    let state = fixture.resolve();

    assert_eq!(state.default, DefaultLocation::Missing);
    assert_eq!(
        state.availability,
        Availability::Available(fixture.default_location())
    );
    assert!(!fixture.default_location().exists(), "nothing is created");
}

/// With no setting, a directory at the default location is the available root, and needs no
/// marker.
#[test]
fn no_setting_and_a_directory_at_the_default_location_is_available() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.default_location()).expect("create the default root");

    let state = fixture.resolve();

    assert_eq!(state.default, DefaultLocation::Directory);
    assert_eq!(
        state.availability,
        Availability::Available(fixture.default_location())
    );
}

/// A redirect at the default location without a setting means the setting was lost, and reads
/// as unknown.
#[test]
fn a_redirect_without_a_setting_is_unavailable() {
    let fixture = Fixture::new();
    let target = fixture.root("root");
    std::fs::create_dir(&target).expect("create the target");
    redirect(&fixture.default_location(), &target);

    let state = fixture.resolve();

    assert_eq!(state.default, DefaultLocation::Redirect);
    assert_eq!(
        state.availability,
        Availability::Unavailable(Unavailable::RedirectWithoutSetting)
    );
}

/// A configured root that holds the setting's marker is available, and the resolution reads each
/// file once.
#[tokio::test]
async fn a_configured_root_with_its_marker_is_available_after_one_read_of_each_file() {
    let fixture = Fixture::new();
    let (root, _) = configured(&fixture).await;

    let availability = fixture.availability();

    assert_eq!(availability, Availability::Available(root));
    assert_eq!(
        reads_in(fixture.global.path()),
        Reads {
            setting: 1,
            marker: 1,
            record: 1,
        }
    );
}

/// With no setting, a resolution reads no marker.
#[test]
fn a_resolution_without_a_setting_reads_no_marker() {
    let fixture = Fixture::new();

    fixture.resolve();

    assert_eq!(
        reads_in(fixture.global.path()),
        Reads {
            setting: 1,
            marker: 0,
            record: 1,
        }
    );
}

/// A root whose marker holds another root ID belongs to another global data directory.
#[tokio::test]
async fn a_marker_of_another_root_id_is_unavailable() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    marked_root(&root, RootId::generate(), generation(1)).await;
    store_setting(
        fixture.global.path(),
        root.to_str().expect("UTF-8"),
        RootId::generate(),
        generation(1),
    )
    .await;

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::ForeignMarker)
    );
}

/// A root whose marker is from an earlier generation is one that was abandoned.
#[tokio::test]
async fn a_marker_of_an_earlier_generation_is_unavailable() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    let id = RootId::generate();
    marked_root(&root, id, generation(1)).await;
    store_setting(
        fixture.global.path(),
        root.to_str().expect("UTF-8"),
        id,
        generation(2),
    )
    .await;

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::OtherGeneration)
    );
}

/// A configured root that is missing, as on a volume that isn't mounted, is unavailable, and
/// nothing is created there.
#[tokio::test]
async fn a_missing_configured_root_is_unavailable() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    store_setting(
        fixture.global.path(),
        root.to_str().expect("UTF-8"),
        RootId::generate(),
        generation(1),
    )
    .await;

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::Missing)
    );
    assert!(!root.exists());
}

/// A directory at the configured path without a marker isn't a root the command moved to.
#[tokio::test]
async fn a_configured_directory_without_a_marker_is_unavailable() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    lore_io::create_dir_all_private(&root).expect("create the directory");
    store_setting(
        fixture.global.path(),
        root.to_str().expect("UTF-8"),
        RootId::generate(),
        generation(1),
    )
    .await;

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::NoMarker)
    );
}

/// A setting that names a relative path names no root.
#[tokio::test]
async fn a_relative_setting_is_unavailable() {
    let fixture = Fixture::new();
    store_setting(
        fixture.global.path(),
        "relative/root",
        RootId::generate(),
        generation(1),
    )
    .await;

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::RelativeSetting)
    );
}

/// A setting that names the default location reads the marker from a real directory there, and
/// reads a redirect or nothing there as unavailable.
#[tokio::test]
async fn a_setting_of_the_default_location_reads_only_a_real_directory() {
    let fixture = Fixture::new();
    let id = RootId::generate();
    store_setting(fixture.global.path(), "", id, generation(1)).await;
    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::Missing)
    );

    let elsewhere = fixture.root("root");
    marked_root(&elsewhere, id, generation(1)).await;
    redirect(&fixture.default_location(), &elsewhere);
    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::DefaultIsRedirect)
    );

    lore_revision::util::fs::unlink(fixture.default_location())
        .await
        .expect("remove the redirect");
    marked_root(&fixture.default_location(), id, generation(1)).await;
    assert_eq!(
        fixture.availability(),
        Availability::Available(fixture.default_location())
    );
}

/// A move record makes the root interrupted, whatever the setting says, and the resolution reads
/// no setting or marker.
#[tokio::test]
async fn a_move_record_is_interrupted() {
    let fixture = Fixture::new();
    configured(&fixture).await;
    MoveRecord {
        from: String::new(),
        to: "/target".to_owned(),
        kind: MoveKind::Rename,
        phase: MovePhase::Place,
    }
    .save(fixture.global.path())
    .await
    .expect("save the record");

    let before = reads_in(fixture.global.path());
    assert_eq!(fixture.availability(), Availability::Interrupted);
    let after = reads_in(fixture.global.path());

    assert_eq!(
        (after.setting, after.marker, after.record),
        (before.setting, before.marker, before.record + 1)
    );
}

/// On Unix, a root that others can write is unavailable, and a root the user alone can write is
/// available whatever the mode of its marker.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_root_others_can_write_is_unavailable_whatever_its_marker() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let (root, _) = configured(&fixture).await;
    let marker = root.join(lore_revision::global::external_dir::marker::MARKER);
    let set_mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };

    set_mode(&root, 0o770);
    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::NotPrivate)
    );
    set_mode(&root, 0o707);
    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::NotPrivate)
    );

    set_mode(&root, 0o755);
    set_mode(&marker, 0o664);
    assert_eq!(fixture.availability(), Availability::Available(root));
}

/// On macOS, a root whose extended ACL lets everyone write it is unavailable, though its mode
/// lets no one else write it.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_root_whose_extended_acl_lets_others_write_is_unavailable() {
    let fixture = Fixture::new();
    let (root, _) = configured(&fixture).await;
    let granted = std::process::Command::new("chmod")
        .args(["+a", "everyone allow add_file,delete_child"])
        .arg(&root)
        .status()
        .expect("run chmod");
    assert!(granted.success());

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::NotPrivate)
    );
}

/// The owner rule takes an object as the user's alone only when the user owns it, neither group
/// nor others can write it, and no extended ACL lets anyone else write it. On Linux, the group
/// bits of an object with an access ACL are its mask, so a mask that grants write is refused.
#[cfg(target_family = "unix")]
#[test]
fn the_owner_rule_admits_only_an_object_the_user_alone_can_write() {
    let user = 1000;

    assert!(is_private(user, 0o700, false, user));
    assert!(is_private(user, 0o755, false, user));
    assert!(is_private(user, 0o600, false, user));
    assert!(!is_private(1001, 0o700, false, user), "another owner");
    assert!(!is_private(0, 0o700, false, user), "root as the owner");
    assert!(!is_private(user, 0o720, false, user), "a group write bit");
    assert!(!is_private(user, 0o702, false, user), "an others write bit");
    assert!(
        !is_private(user, 0o770, false, user),
        "a mask that grants write"
    );
    assert!(
        !is_private(user, 0o700, true, user),
        "an extended ACL that lets another write"
    );
}

/// A lookup with no setting finds an instance's external `.lore` at the default location, and
/// answers absent where it isn't there or nothing is at the default location.
#[test]
fn a_lookup_without_a_setting_answers_present_or_absent() {
    let fixture = Fixture::new();
    let instance_id = InstanceId::generate();
    assert_eq!(fixture.resolve().lookup(instance_id), Lookup::Absent);

    let dot_lore = fixture
        .default_location()
        .join(instance_id.text_encoding())
        .join(DOT_LORE);
    std::fs::create_dir_all(&dot_lore).expect("make the external directory");

    assert_eq!(
        fixture.resolve().lookup(instance_id),
        Lookup::Present(dot_lore)
    );
    assert_eq!(
        fixture.resolve().lookup(InstanceId::generate()),
        Lookup::Absent
    );
}

/// A lookup in an interrupted or unavailable root is unknown, naming why.
#[tokio::test]
async fn a_lookup_in_a_root_that_cannot_be_used_is_unknown() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    store_setting(
        fixture.global.path(),
        root.to_str().expect("UTF-8"),
        RootId::generate(),
        generation(1),
    )
    .await;
    assert_eq!(
        fixture.resolve().lookup(InstanceId::generate()),
        Lookup::Unknown(Unknown::Unavailable(Unavailable::Missing))
    );

    MoveRecord {
        from: String::new(),
        to: root.to_str().expect("UTF-8").to_owned(),
        kind: MoveKind::Rename,
        phase: MovePhase::Place,
    }
    .save(fixture.global.path())
    .await
    .expect("save the record");
    assert_eq!(
        fixture.resolve().lookup(InstanceId::generate()),
        Lookup::Unknown(Unknown::Interrupted)
    );
}

/// An instance absent from a configured root but present in a directory at the default location
/// is unknown; absent from both, or with a redirect at the default location, it is absent.
#[tokio::test]
async fn an_instance_left_at_the_default_location_is_unknown() {
    let fixture = Fixture::new();
    let (root, _) = configured(&fixture).await;
    let instance_id = InstanceId::generate();
    std::fs::create_dir(fixture.default_location()).expect("make the default location");
    assert_eq!(fixture.resolve().lookup(instance_id), Lookup::Absent);

    std::fs::create_dir(fixture.default_location().join(instance_id.text_encoding()))
        .expect("leave the instance at the default location");
    assert_eq!(
        fixture.resolve().lookup(instance_id),
        Lookup::Unknown(Unknown::AtDefaultLocation)
    );

    std::fs::remove_dir_all(fixture.default_location()).expect("clear the default location");
    redirect(&fixture.default_location(), &root);
    assert_eq!(fixture.resolve().lookup(instance_id), Lookup::Absent);
}

/// A stat that fails other than with not-found is unknown, also with no setting.
#[cfg(target_family = "unix")]
#[test]
fn a_failed_stat_is_unknown() {
    let fixture = Fixture::new();
    let instance_id = InstanceId::generate();
    std::fs::create_dir(fixture.default_location()).expect("make the default location");
    let looping = fixture.default_location().join(instance_id.text_encoding());
    std::os::unix::fs::symlink(&looping, &looping).expect("make a link to itself");

    assert_eq!(
        fixture.resolve().lookup(instance_id),
        Lookup::Unknown(Unknown::StatFailed)
    );
}

/// A file where an instance's external directory would be is no external directory.
#[test]
fn a_file_in_place_of_an_external_directory_is_absent() {
    let fixture = Fixture::new();
    let instance_id = InstanceId::generate();
    std::fs::create_dir(fixture.default_location()).expect("make the default location");
    std::fs::write(
        fixture.default_location().join(instance_id.text_encoding()),
        b"file",
    )
    .expect("write a file there");

    assert_eq!(fixture.resolve().lookup(instance_id), Lookup::Absent);
}

/// A setting that names a file names no root.
#[tokio::test]
async fn a_setting_that_names_a_file_is_unavailable() {
    let fixture = Fixture::new();
    let file = fixture.root("file");
    std::fs::write(&file, b"file").expect("write a file");
    store_setting(
        fixture.global.path(),
        file.to_str().expect("UTF-8"),
        RootId::generate(),
        generation(1),
    )
    .await;

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::NotADirectory)
    );
}

/// A setting or a marker that can't be parsed leaves the root unavailable, rather than read as no
/// setting or no marker.
#[tokio::test]
async fn a_setting_or_marker_that_cannot_be_parsed_is_unavailable() {
    let fixture = Fixture::new();
    let (root, _) = configured(&fixture).await;
    let marker = root.join(lore_revision::global::external_dir::marker::MARKER);
    std::fs::write(&marker, b"generation = 1\n").expect("write a marker with no root ID");
    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::Unreadable)
    );

    std::fs::write(
        fixture
            .global
            .path()
            .join(lore_revision::global::external_dir::setting::EXTERNAL_ROOT),
        b"this is not toml = = =",
    )
    .expect("write a setting that isn't TOML");
    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::Unreadable)
    );
}

/// A setting that spells the default location as an absolute path reads only a real directory
/// there, as the empty path does, and never reaches a root through the redirect.
#[tokio::test]
async fn a_setting_spelling_the_default_location_never_follows_the_redirect() {
    let fixture = Fixture::new();
    let id = RootId::generate();
    let elsewhere = fixture.root("root");
    marked_root(&elsewhere, id, generation(1)).await;
    redirect(&fixture.default_location(), &elsewhere);
    store_setting(
        fixture.global.path(),
        fixture.default_location().to_str().expect("UTF-8"),
        id,
        generation(1),
    )
    .await;

    assert_eq!(
        fixture.availability(),
        Availability::Unavailable(Unavailable::DefaultIsRedirect)
    );
}

/// An instance absent from a configured root is unknown where what the default location holds
/// can't be examined, since it may hold the instance.
#[test]
fn an_absent_instance_is_unknown_where_the_default_location_cannot_be_examined() {
    let fixture = Fixture::new();
    let root = fixture.root("root");
    std::fs::create_dir(&root).expect("make the root");
    let state = RootState {
        global: fixture.global.path().to_path_buf(),
        default: DefaultLocation::Unreadable,
        availability: Availability::Available(root),
    };

    assert_eq!(
        state.lookup(InstanceId::generate()),
        Lookup::Unknown(Unknown::StatFailed)
    );
}
