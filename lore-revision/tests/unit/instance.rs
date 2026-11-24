// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write external `.lore` configs directly; what these test is how staleness reads them.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::error::Disconnected;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_revision::event::LoreEvent;
use lore_revision::global::external_dir::external_dir_for_instance;
use lore_revision::global::external_dir::marker::Marker;
use lore_revision::global::external_dir::record::MoveKind;
use lore_revision::global::external_dir::record::MovePhase;
use lore_revision::global::external_dir::record::MoveRecord;
use lore_revision::global::external_dir::resolve::Lookup;
use lore_revision::global::external_dir::resolve::Unknown;
use lore_revision::global::external_dir::resolve::resolve;
use lore_revision::global::external_dir::resolve::test_util::reads_in;
use lore_revision::global::external_dir::setting::RootId;
use lore_revision::global::external_dir::setting::Setting;
use lore_revision::instance::*;
use lore_revision::interface::LoreString;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::CONFIG;
use lore_revision::repository::DOT_LORE;
use lore_revision::repository::INSTANCE;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::SALT_LORE;
use lore_revision::util::path::clean;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::hash_function_arg;
use lore_storage::store_types::KeyType;
use lore_storage::test_util::HookedStore;
use lore_storage::test_util::StoreCall;
use zerocopy::IntoBytes;

use crate::global::external_dir::redirect;
use crate::helper::RepositoryContextCreationArgsExt;
use crate::helper::default_repository_creation_args;
use crate::helper::execution_in;
use crate::helper::in_memory;
use crate::helper::test_store_create;
use crate::repository::test_helpers::built_context;
use crate::repository::test_helpers::counted_endpoint;

#[test]
fn from_hex_roundtrips_text_encoding() {
    let id = InstanceId::generate();
    let hex = id.text_encoding();
    assert_eq!(InstanceId::from_hex(&hex), Some(id));
    assert_eq!(InstanceId::from_hex("not hex"), None);
    assert_eq!(
        InstanceId::from_hex("ab"),
        None,
        "too short to be an InstanceId"
    );
}

#[test]
fn instance_key_is_deterministic() {
    let id = InstanceId::generate();
    let (key1, typ1) = instance_key(SALT_LORE, id);
    let (key2, typ2) = instance_key(SALT_LORE, id);
    assert_eq!(key1, key2);
    assert_eq!(typ1, typ2);
    assert_eq!(typ1, KeyType::Instance);
}

#[test]
fn instance_key_differs_for_different_ids() {
    let a = InstanceId::generate();
    let b = InstanceId::generate();
    let (key_a, _) = instance_key(SALT_LORE, a);
    let (key_b, _) = instance_key(SALT_LORE, b);
    assert_ne!(key_a, key_b);
}

#[test]
fn anchor_keys_differ_for_current_vs_staged() {
    let id = InstanceId::generate();
    let (current, typ_c) = anchor_key(SALT_LORE, ANCHOR_CURRENT, id);
    let (staged, typ_s) = anchor_key(SALT_LORE, ANCHOR_STAGED, id);
    assert_ne!(current, staged);
    assert_eq!(typ_c, KeyType::Untyped);
    assert_eq!(typ_s, KeyType::Untyped);
}

/// The lower-case hex of instance IDs whose digits cover `0` to `f`.
const INSTANCE_ID_HEX: [&str; 4] = [
    "00000000000000000000000000000000",
    "00112233445566778899aabbccddeeff",
    "fedcba98765432100123456789abcdef",
    "ffffffffffffffffffffffffffffffff",
];

/// An anchor key hashes the anchor's function name and the lower-case hex of the instance ID, the
/// spelling every anchor key in a store derives from.
#[test]
fn anchor_key_hashes_the_lower_case_hex_of_the_id() {
    for text in INSTANCE_ID_HEX {
        let id = InstanceId::from_hex(text).expect("an instance ID");
        for function in [ANCHOR_CURRENT, ANCHOR_CURRENT_BRANCH, ANCHOR_STAGED] {
            assert_eq!(
                anchor_key(SALT_LORE, function, id).0,
                hash_function_arg(SALT_LORE, function, text),
                "{function} of {text}"
            );
        }
    }
}

/// An instance key hashes the lower-case hex of the instance ID, the spelling every instance key in
/// a store derives from.
#[test]
fn instance_key_hashes_the_lower_case_hex_of_the_id() {
    for text in INSTANCE_ID_HEX {
        let id = InstanceId::from_hex(text).expect("an instance ID");
        assert_eq!(
            instance_key(SALT_LORE, id).0,
            hash_function_arg(SALT_LORE, INSTANCE_METADATA, text),
            "{text}"
        );
    }
}

#[test]
fn generate_produces_nonzero_unique_values() {
    let a = InstanceId::generate();
    let b = InstanceId::generate();
    assert!(!a.is_zero());
    assert!(!b.is_zero());
    assert_ne!(a, b);
}

#[test]
fn default_is_zero() {
    let id = InstanceId::default();
    assert!(id.is_zero());
}

#[test]
fn roundtrip_bytes() {
    let id = InstanceId::generate();
    let bytes = id.as_bytes().to_vec();
    assert_eq!(bytes.len(), 16);
    let mut restored = InstanceId::default();
    restored.as_mut_bytes().copy_from_slice(&bytes);
    assert_eq!(id, restored);
}

/// The minimal config of an SWFS-backed instance.
const SWFS_CONFIG: &[u8] = b"[vfs]\nvfs_type = \"Swfs\"\n";

/// The minimal config of a FUSE-backed instance.
const FUSE_CONFIG: &[u8] = b"[vfs]\nvfs_type = \"Fuse\"\n";

/// What an instance's external `.lore` directory holds.
#[derive(Clone, Copy, Debug)]
enum External {
    /// There is no external directory.
    Absent,
    /// The directory, with no config in it.
    NoConfig,
    /// The directory, with a config of these bytes.
    Config(&'static [u8]),
    /// The directory, with a directory where its config would be, so the config cannot be read.
    Unreadable,
}

/// The staleness of the registration of a fresh instance whose external `.lore` directory holds
/// `external`, at a path that is gone where `path_gone`, else an empty directory: a checkout with
/// no instance file.
async fn staleness_with(external: External, path_gone: bool) -> InstanceStaleness {
    let global = TempDir::new("lore-instance-staleness-global-");
    let parent = TempDir::new("lore-instance-staleness-path-");
    let path = parent.path().join("instance");
    if !path_gone {
        std::fs::create_dir(&path).expect("make the instance path");
    }
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let instance_id = InstanceId::generate();
            let dot_lore = external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                instance_id,
            )
            .expect("global data directory")
            .join(lore_revision::repository::DOT_LORE);
            match external {
                External::Absent => {}
                External::NoConfig => {
                    std::fs::create_dir_all(&dot_lore).expect("make the external directory");
                }
                External::Config(config) => {
                    std::fs::create_dir_all(&dot_lore).expect("make the external directory");
                    std::fs::write(dot_lore.join(CONFIG), config).expect("write the config");
                }
                External::Unreadable => {
                    std::fs::create_dir_all(dot_lore.join(CONFIG)).expect("occupy the config path");
                }
            }
            let state = resolve().expect("resolve the external root");
            instance_staleness(&state, &path.display().to_string(), instance_id).await
        })
        .await
}

/// How an instance's external directory reads as unknown.
#[derive(Clone, Copy, Debug)]
enum UnknownRoot {
    /// A redirect at the default location, with no setting.
    RedirectWithoutSetting,
    /// A setting that names a missing root.
    MissingRoot,
    /// A move record.
    Interrupted,
    /// A stat that fails other than with not-found.
    #[cfg(target_family = "unix")]
    StatFailed,
    /// The instance absent from a configured root, and present at the default location.
    AtDefaultLocation,
}

/// Every way an external directory reads as unknown.
const UNKNOWN_ROOTS: &[UnknownRoot] = &[
    UnknownRoot::RedirectWithoutSetting,
    UnknownRoot::MissingRoot,
    UnknownRoot::Interrupted,
    #[cfg(target_family = "unix")]
    UnknownRoot::StatFailed,
    UnknownRoot::AtDefaultLocation,
];

/// Makes the external directory of `instance_id` read as `how` in the global data directory of
/// the call in scope, whose configured roots go in `volume`.
async fn make_unknown(how: UnknownRoot, volume: &Path, instance_id: InstanceId) {
    let data = lore_revision::global::get_global_data_dir().expect("the global data directory");
    let default = data.join("external");
    let setting = |path: &Path, id: RootId| Setting {
        path: path.to_str().expect("UTF-8").to_owned(),
        id,
        generation: std::num::NonZeroU64::MIN,
    };
    match how {
        UnknownRoot::RedirectWithoutSetting => {
            let target = volume.join("target");
            std::fs::create_dir_all(&target).expect("make the target");
            redirect(&default, &target);
        }
        UnknownRoot::MissingRoot => setting(&volume.join("missing"), RootId::generate())
            .save(&data)
            .await
            .expect("save the setting"),
        UnknownRoot::Interrupted => MoveRecord {
            from: String::new(),
            to: volume.join("root").display().to_string(),
            kind: MoveKind::Rename,
            phase: MovePhase::Place,
        }
        .save(&data)
        .await
        .expect("save the record"),
        #[cfg(target_family = "unix")]
        UnknownRoot::StatFailed => {
            let looping = default.join(instance_id.text_encoding());
            std::fs::create_dir_all(&default).expect("make the default location");
            std::os::unix::fs::symlink(&looping, &looping).expect("make a link to itself");
        }
        UnknownRoot::AtDefaultLocation => {
            let root = volume.join("root");
            let id = RootId::generate();
            lore_io::create_dir_all_private(&root).expect("make the root");
            Marker {
                id,
                generation: std::num::NonZeroU64::MIN,
            }
            .save(&root)
            .await
            .expect("save the marker");
            setting(&root, id)
                .save(&data)
                .await
                .expect("save the setting");
            std::fs::create_dir_all(default.join(instance_id.text_encoding()))
                .expect("leave the instance at the default location");
        }
    }
}

/// An external directory whose lookup is unknown keeps the registration active, whether the path
/// or only the checkout is gone.
#[tokio::test]
async fn an_unknown_external_directory_keeps_the_registration_active() {
    for &how in UNKNOWN_ROOTS {
        for path_gone in [true, false] {
            let global = TempDir::new("lore-instance-unknown-global-");
            let volume = TempDir::new("lore-instance-unknown-volume-");
            let parent = TempDir::new("lore-instance-unknown-path-");
            let path = parent.path().join("instance");
            if !path_gone {
                std::fs::create_dir(&path).expect("make the instance path");
            }
            let staleness = LORE_CONTEXT
                .scope(execution_in(global.path()), async {
                    let instance_id = InstanceId::generate();
                    make_unknown(how, volume.path(), instance_id).await;
                    let state = resolve().expect("resolve the external root");
                    assert!(
                        matches!(state.lookup(instance_id), Lookup::Unknown(_)),
                        "{how:?}"
                    );
                    instance_staleness(&state, &path.display().to_string(), instance_id).await
                })
                .await;
            assert_eq!(
                staleness,
                InstanceStaleness::Active,
                "{how:?}, path gone {path_gone}"
            );
        }
    }
}

/// Inside a FUSE mount, the Lore directory is the instance's external `.lore` where it is
/// present, and finding it fails, as no invalid path, where its lookup is unknown. A mount table
/// that can't be read reads as no mount.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_lore_directory_inside_a_mount_fails_on_an_unknown_external_directory() {
    use lore_revision::fs::fuse::discover::test_util::fuse_lore_entry;
    use lore_revision::fs::fuse::discover::test_util::plant_table;
    use lore_revision::fs::fuse::discover::test_util::plant_unreadable_table;
    use lore_revision::repository::get_dot_lore_path;

    let global = TempDir::new("lore-dot-lore-mount-global-");
    let volume = TempDir::new("lore-dot-lore-mount-volume-");
    let mount = TempDir::new("lore-dot-lore-mount-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let instance_id = InstanceId::generate();
            let inside = mount.path().join("sub");
            let planted = plant_table(mount.path(), fuse_lore_entry(mount.path(), instance_id));
            let dot_lore = external_dir_for_instance(
                &lore_revision::global::external_dir::resolve::resolve()
                    .expect("resolve the external root"),
                instance_id,
            )
            .expect("the external directory")
            .join(lore_revision::repository::DOT_LORE);
            std::fs::create_dir_all(&dot_lore).expect("make the external directory");
            assert_eq!(
                get_dot_lore_path(&inside).expect("the external directory"),
                dot_lore
            );

            make_unknown(UnknownRoot::MissingRoot, volume.path(), instance_id).await;
            let failed = get_dot_lore_path(&inside).expect_err("an unknown external directory");
            assert!(!failed.is_invalid_path(), "{failed}");

            drop(planted);
            let _unreadable = plant_unreadable_table(mount.path());
            assert_eq!(
                get_dot_lore_path(&inside).expect("no mount"),
                inside.join(DOT_LORE)
            );
        })
        .await;
}

/// The external directory's lookup decides whether an instance acted on is a virtual instance: a
/// present one is, an absent one isn't, and an unknown one refuses while a mount is at the path,
/// and reads as an ordinary instance with none.
#[test]
fn an_unknown_external_directory_refuses_an_instance_on_a_mount() {
    let instance_id = InstanceId::generate();
    let unknown = Lookup::Unknown(Unknown::Interrupted);

    assert!(
        external_presence(
            &Lookup::Present(PATH.into()),
            true,
            instance_id,
            PATH,
            "delete"
        )
        .expect("present")
    );
    assert!(
        !external_presence(&Lookup::Absent, true, instance_id, PATH, "delete").expect("absent")
    );
    assert!(!external_presence(&unknown, false, instance_id, PATH, "delete").expect("no mount"));
    external_presence(&unknown, true, instance_id, PATH, "delete")
        .expect_err("an unknown external directory under a mount");
}

/// Only positive evidence makes a registration stale. An external `.lore` naming SWFS keeps one
/// whose path or checkout is gone, as an SWFS instance cannot be removed by hand. One naming FUSE
/// keeps one whose checkout is gone, as an unmounted mount point is empty, but not one whose
/// mount point is gone.
#[tokio::test]
async fn an_external_config_naming_swfs_or_fuse_keeps_what_its_path_does_not() {
    for (config, path_gone, checkout_gone) in [
        (
            SWFS_CONFIG,
            InstanceStaleness::Active,
            InstanceStaleness::Active,
        ),
        (
            FUSE_CONFIG,
            InstanceStaleness::PathMissing,
            InstanceStaleness::Active,
        ),
    ] {
        let label = String::from_utf8_lossy(config);
        assert_eq!(
            staleness_with(External::Config(config), true).await,
            path_gone,
            "path gone, {label}"
        );
        assert_eq!(
            staleness_with(External::Config(config), false).await,
            checkout_gone,
            "checkout gone, {label}"
        );
    }
}

/// An external `.lore` whose config is there but cannot be read or parsed is no evidence the
/// instance is gone, whichever VFS it was, so it keeps the registration.
#[tokio::test]
async fn an_external_config_that_cannot_be_read_keeps_the_registration() {
    for external in [
        External::Config(b"this is not toml = = ="),
        External::Unreadable,
    ] {
        for path_gone in [true, false] {
            assert_eq!(
                staleness_with(external, path_gone).await,
                InstanceStaleness::Active,
                "{external:?}, path gone {path_gone}"
            );
        }
    }
}

/// An external `.lore` that names no VFS, holds no config or is not there is no evidence either
/// way, and leaves the registration as its path says.
#[tokio::test]
async fn an_external_directory_naming_no_vfs_leaves_the_registration_as_its_path_says() {
    for external in [
        External::Config(b"[vfs]\nvfs_type = \"None\"\n"),
        External::Config(b"remote_url = \"lore://host\"\n"),
        External::NoConfig,
        External::Absent,
    ] {
        assert_eq!(
            staleness_with(external, true).await,
            InstanceStaleness::PathMissing,
            "{external:?}"
        );
        assert_eq!(
            staleness_with(external, false).await,
            InstanceStaleness::CheckoutMissing,
            "{external:?}"
        );
    }
}

#[test]
fn display_is_hex() {
    let id = InstanceId::generate();
    let s = id.to_string();
    assert_eq!(s.len(), 32); // 16 bytes = 32 hex chars
    assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
}

const PATH: &str = "/checkout";

#[test]
fn an_swfs_instance_on_its_own_mount_can_be_acted_on() {
    let id = InstanceId::generate();
    check_mount(id, true, true, Some(id), PATH, "delete").expect("its own mount");
}

#[test]
fn a_regular_instance_on_no_mount_can_be_acted_on() {
    let id = InstanceId::generate();
    check_mount(id, false, true, None, PATH, "delete").expect("in the service");
    check_mount(id, false, false, None, PATH, "delete").expect("outside it");
}

#[test]
fn an_instance_file_naming_another_instance_than_the_mount_is_refused() {
    let named = InstanceId::generate();
    let mounted = InstanceId::generate();
    for is_swfs in [true, false] {
        let error = check_mount(named, is_swfs, true, Some(mounted), PATH, "move")
            .expect_err("another instance's mount")
            .to_string();
        assert!(error.contains(&mounted.to_string()), "{error}");
        assert!(error.contains(&named.to_string()), "{error}");
    }
}

#[test]
fn an_swfs_instance_that_is_not_mounted_at_its_path_is_refused() {
    let id = InstanceId::generate();
    let error = check_mount(id, true, true, None, PATH, "delete")
        .expect_err("not mounted")
        .to_string();
    assert!(error.contains("not mounted"), "{error}");
}

#[test]
fn an_swfs_instance_outside_the_service_is_refused() {
    let id = InstanceId::generate();
    let error = check_mount(id, true, false, None, PATH, "move")
        .expect_err("no service")
        .to_string();
    assert!(error.contains("only the service"), "{error}");
}

#[test]
fn a_mount_of_an_instance_with_no_external_directory_is_refused() {
    let id = InstanceId::generate();
    let error = check_mount(id, false, true, Some(id), PATH, "delete")
        .expect_err("a mount it cannot unmount")
        .to_string();
    assert!(error.contains("no external directory"), "{error}");
}

/// Make `path` a checkout whose `.lore/instance` names `instance_id`.
fn write_checkout(path: &Path, instance_id: InstanceId) {
    let dot_lore = path.join(DOT_LORE);
    std::fs::create_dir_all(&dot_lore).expect("make the .lore directory");
    std::fs::write(dot_lore.join(INSTANCE), instance_id.as_bytes())
        .expect("write the instance file");
}

/// Register the instance of `repository` at `path`, on `branch` at `revision`.
async fn register_on(
    repository: &Arc<RepositoryContext>,
    path: &Path,
    branch: BranchId,
    revision: Hash,
) {
    register_instance(
        repository,
        repository.instance_id,
        &path.display().to_string(),
    )
    .await
    .expect("register the instance");
    store_current_anchor_branch(repository, branch)
        .await
        .expect("store the current branch");
    store_current_anchor(repository, revision)
        .await
        .expect("store the current revision");
}

/// The record a lookup reports for the instance of `repository` registered at `path`.
fn record_of(
    repository: &RepositoryContext,
    path: &Path,
    revision: Hash,
    staleness: InstanceStaleness,
) -> InstanceRecord {
    InstanceRecord {
        instance_id: repository.instance_id,
        path: LoreString::from(clean(path.display().to_string())),
        revision,
        staleness,
    }
}

/// The key of the anchor of `instance_id` that `function` names, in `repository`.
fn anchor_of(repository: &RepositoryContext, function: &str, instance_id: InstanceId) -> Hash {
    anchor_key(repository.salt(), function, instance_id).0
}

/// The loads a [`failing_loads`] store sees.
#[derive(Default)]
struct Loads {
    /// The key whose loads fail, once set
    failing: OnceLock<Hash>,
    /// The key of each load, in order
    keys: Mutex<Vec<Hash>>,
}

/// `mutable`, recording the key of each load in `loads` and failing each load of the key
/// `loads.failing` holds, once set, with `Disconnected`.
fn failing_loads(mutable: Arc<dyn MutableStore>, loads: Arc<Loads>) -> Arc<HookedStore> {
    HookedStore::wrap(
        mutable,
        move |call, key, _key_type| {
            if call != StoreCall::Load {
                return Ok(());
            }
            loads.keys.lock().expect("the loaded keys").push(key);
            if loads.failing.get() == Some(&key) {
                return Err(StoreError::from(Disconnected));
            }
            Ok(())
        },
        None,
    )
}

/// `records` in the order of their instance ids. A lookup reports them in the order its list
/// answers the registration keys, which are hashes of the ids.
fn sorted(mut records: Vec<InstanceRecord>) -> Vec<InstanceRecord> {
    records.sort_by_key(|record| record.instance_id);
    records
}

/// A lookup from a context that holds a write token and is none of the instances reports a
/// registration whose path is gone and one whose path holds another instance's checkout as stale,
/// leaves both registered, and reports none of the instances on other branches.
#[tokio::test]
async fn records_on_branch_reports_stale_instances_without_retiring_them() {
    let global = TempDir::new("lore-records-on-branch-stale-global-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let parent = TempDir::new("lore-records-on-branch-stale-");
            let branch = BranchId::from([1; 16]);

            let removed =
                in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
            let removed_path = parent.path().join("removed");
            write_checkout(&removed_path, removed.instance_id);
            register_on(&removed, &removed_path, branch, Hash::from([2; 32])).await;

            let superseded =
                in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
            let superseded_path = parent.path().join("superseded");
            write_checkout(&superseded_path, InstanceId::generate());
            register_on(&superseded, &superseded_path, branch, Hash::from([5; 32])).await;

            let elsewhere =
                in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
            let elsewhere_path = parent.path().join("elsewhere");
            write_checkout(&elsewhere_path, elsewhere.instance_id);
            register_on(
                &elsewhere,
                &elsewhere_path,
                BranchId::from([3; 16]),
                Hash::from([4; 32]),
            )
            .await;

            std::fs::remove_dir_all(&removed_path).expect("remove the checkout");

            let reader = in_memory::repository_in(immutable, mutable.clone(), repository_id);
            let records = records_on_branch(&reader, branch)
                .await
                .expect("look up the branch");
            assert_eq!(
                sorted(records),
                sorted(vec![
                    record_of(
                        &removed,
                        &removed_path,
                        Hash::from([2; 32]),
                        InstanceStaleness::PathMissing
                    ),
                    record_of(
                        &superseded,
                        &superseded_path,
                        Hash::from([5; 32]),
                        InstanceStaleness::Superseded
                    ),
                ])
            );

            for instance in [&removed, &superseded, &elsewhere] {
                let (key, key_type) = instance_key(reader.salt(), instance.instance_id);
                let registration = mutable
                    .clone()
                    .load(repository_id, key, key_type)
                    .await
                    .expect("the registration loads");
                assert!(
                    !registration.is_zero(),
                    "the lookup leaves the registration of {} in place",
                    instance.instance_id
                );
            }
        })
        .await;
}

/// One lookup lists the registrations once and groups the instances by their current branch,
/// several on one branch included, leaving out an instance with no current branch. A lookup on
/// that branch reports each of its instances.
#[tokio::test]
async fn records_by_branch_groups_instances_and_lists_once() {
    let global = TempDir::new("lore-records-by-branch-global-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
            let lists = Arc::new(AtomicUsize::new(0));
            let listed = lists.clone();
            let mutable = HookedStore::wrap(
                mutable,
                move |call, _key, _key_type| {
                    if call == StoreCall::List {
                        listed.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(())
                },
                None,
            );
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let parent = TempDir::new("lore-records-by-branch-");
            let shared = BranchId::from([1; 16]);

            let mut expected: HashMap<BranchId, Vec<InstanceRecord>> = HashMap::new();
            for (name, branch, revision) in [
                ("first", shared, Hash::from([2; 32])),
                ("beside-first", shared, Hash::from([5; 32])),
                ("second", BranchId::from([3; 16]), Hash::from([4; 32])),
            ] {
                let repository =
                    in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
                let path = parent.path().join(name);
                write_checkout(&path, repository.instance_id);
                register_on(&repository, &path, branch, revision).await;
                expected.entry(branch).or_default().push(record_of(
                    &repository,
                    &path,
                    revision,
                    InstanceStaleness::Active,
                ));
            }
            let unplaced =
                in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
            let unplaced_path = parent.path().join("unplaced");
            write_checkout(&unplaced_path, unplaced.instance_id);
            register_instance(
                &unplaced,
                unplaced.instance_id,
                &unplaced_path.display().to_string(),
            )
            .await
            .expect("register an instance with no current branch");

            let reader = in_memory::repository_in(immutable, mutable, repository_id);
            let before = lists.load(Ordering::SeqCst);
            let mut records = records_by_branch(&reader)
                .await
                .expect("look up every branch");
            assert_eq!(lists.load(Ordering::SeqCst) - before, 1);
            for group in records.values_mut().chain(expected.values_mut()) {
                group.sort_by_key(|record| record.instance_id);
            }
            assert_eq!(records, expected);

            let on_shared = records_on_branch(&reader, shared)
                .await
                .expect("look up the shared branch");
            assert_eq!(sorted(on_shared), expected[&shared]);
        })
        .await;
}

/// A registration whose blob the store doesn't hold is a miss: the lookup leaves it out and
/// reports the instances it can read.
#[tokio::test]
async fn records_on_branch_skips_a_registration_the_store_does_not_hold() {
    let global = TempDir::new("lore-records-on-branch-missing-global-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let parent = TempDir::new("lore-records-on-branch-missing-");
            let branch = BranchId::from([1; 16]);
            let revision = Hash::from([2; 32]);
            let repository = in_memory::repository_in(immutable, mutable.clone(), repository_id);
            let path = parent.path().join("live");
            write_checkout(&path, repository.instance_id);
            register_on(&repository, &path, branch, revision).await;
            register_missing_blob(&mutable, repository_id).await;

            let records = records_on_branch(&repository, branch)
                .await
                .expect("a registration the store lacks is skipped");
            assert_eq!(
                records,
                [record_of(
                    &repository,
                    &path,
                    revision,
                    InstanceStaleness::Active
                )]
            );
        })
        .await;
}

/// Register an instance of repository `repository_id` in `mutable` with a blob no store holds.
async fn register_missing_blob(mutable: &Arc<dyn MutableStore>, repository_id: RepositoryId) {
    let (key, key_type) = instance_key(SALT_LORE, InstanceId::generate());
    mutable
        .clone()
        .store(repository_id, key, Hash::from([9; 32]), key_type)
        .await
        .expect("register an instance whose blob the store lacks");
}

/// A registration whose blob names no usable instance id, here the zero id, names no anchors. Both
/// lookups leave it out, though anchors stored under the zero id name the branch and its path holds
/// a checkout, and report the instance beside it.
#[tokio::test]
async fn records_on_branch_skips_a_registration_without_a_usable_instance_id() {
    let global = TempDir::new("lore-records-on-branch-no-id-global-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let parent = TempDir::new("lore-records-on-branch-no-id-");
            let branch = BranchId::from([1; 16]);
            let revision = Hash::from([2; 32]);
            let repository =
                in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
            let path = parent.path().join("live");
            write_checkout(&path, repository.instance_id);
            register_on(&repository, &path, branch, revision).await;

            let unnamed_path = parent.path().join("unnamed");
            write_checkout(&unnamed_path, InstanceId::generate());
            let zero_id = Arc::new(
                RepositoryContext::new(
                    default_repository_creation_args(immutable, mutable)
                        .with_id(repository_id)
                        .with_instance_id(InstanceId::default()),
                )
                .with_write_token(in_memory::token()),
            );
            register_on(&zero_id, &unnamed_path, branch, Hash::from([3; 32])).await;

            let expected = vec![record_of(
                &repository,
                &path,
                revision,
                InstanceStaleness::Active,
            )];
            let records = records_on_branch(&repository, branch)
                .await
                .expect("look up the branch");
            assert_eq!(records, expected);
            let records = records_by_branch(&repository)
                .await
                .expect("look up every branch");
            assert_eq!(records, HashMap::from([(branch, expected)]));
        })
        .await;
}

/// A lookup reads the registrations from the local store alone. On a context whose remote is in
/// reach it asks the remote for nothing, not even for a registration blob the local store lacks,
/// and on a context that reads from the remote alone it reads them from the local store.
#[tokio::test]
async fn records_on_branch_reads_registrations_from_the_local_store_alone() {
    let global = TempDir::new("lore-records-on-branch-local-global-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let (endpoint, connection) = counted_endpoint(RepositoryId::default()).await;
            let reader = built_context(|context| {
                context
                    .with_remote_endpoint(endpoint)
                    .with_write_token(in_memory::token())
            })
            .await;
            assert!(!reader.is_offline());
            let immutable = reader.immutable_store();
            let mutable = reader
                .try_mutable_store_arc()
                .expect("the reader holds a write token");
            let parent = TempDir::new("lore-records-on-branch-local-");
            let branch = BranchId::from([1; 16]);
            let revision = Hash::from([2; 32]);
            let live = in_memory::repository_in(immutable.clone(), mutable.clone(), reader.id);
            let path = parent.path().join("live");
            write_checkout(&path, live.instance_id);
            register_on(&live, &path, branch, revision).await;
            register_missing_blob(&mutable, reader.id).await;
            let expected = [record_of(&live, &path, revision, InstanceStaleness::Active)];

            let records = records_on_branch(&reader, branch).await;

            assert_eq!(connection.storage_session_starts(), 0);
            assert_eq!(
                records.expect("a registration the store lacks is skipped"),
                expected
            );

            let remote_only = Arc::new(
                RepositoryContext::new(
                    default_repository_creation_args(immutable, mutable).with_id(reader.id),
                )
                .reading_remote_only(),
            );
            let records = records_on_branch(&remote_only, branch)
                .await
                .expect("look up the branch on a context that reads from the remote alone");
            assert_eq!(records, expected);
        })
        .await;
}

/// A lookup on a branch loads the current revision only of the instances on it, and a failed load
/// of an instance's current branch fails both lookups rather than leaving the instance out.
#[tokio::test]
async fn records_on_branch_fails_on_a_failed_current_branch_load() {
    let global = TempDir::new("lore-records-current-branch-load-global-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
            let loads = Arc::new(Loads::default());
            let mutable = failing_loads(mutable, loads.clone());
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let parent = TempDir::new("lore-records-current-branch-load-");
            let branch = BranchId::from([1; 16]);
            let revision = Hash::from([2; 32]);

            let repository =
                in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
            let path = parent.path().join("on-branch");
            write_checkout(&path, repository.instance_id);
            register_on(&repository, &path, branch, revision).await;
            let elsewhere = in_memory::repository_in(immutable, mutable, repository_id);
            let elsewhere_path = parent.path().join("elsewhere");
            write_checkout(&elsewhere_path, elsewhere.instance_id);
            register_on(
                &elsewhere,
                &elsewhere_path,
                BranchId::from([3; 16]),
                Hash::from([4; 32]),
            )
            .await;

            loads.keys.lock().expect("the loaded keys").clear();
            let records = records_on_branch(&repository, branch)
                .await
                .expect("look up the branch");
            assert_eq!(
                records,
                [record_of(
                    &repository,
                    &path,
                    revision,
                    InstanceStaleness::Active
                )]
            );
            let keys = std::mem::take(&mut *loads.keys.lock().expect("the loaded keys"));
            let current_branch =
                anchor_of(&repository, ANCHOR_CURRENT_BRANCH, elsewhere.instance_id);
            let current = anchor_of(&repository, ANCHOR_CURRENT, elsewhere.instance_id);
            assert!(
                keys.contains(&current_branch),
                "a lookup on the branch loads the current branch of an instance on another branch"
            );
            assert!(
                !keys.contains(&current),
                "a lookup on the branch loads no current revision of an instance on another branch"
            );

            loads
                .failing
                .set(anchor_of(
                    &repository,
                    ANCHOR_CURRENT_BRANCH,
                    repository.instance_id,
                ))
                .expect("arm the failing load");
            let error = records_on_branch(&repository, branch)
                .await
                .expect_err("a failed load fails the lookup");
            assert!(error.is_disconnected(), "{error}");
            let error = records_by_branch(&repository)
                .await
                .expect_err("a failed load fails the lookup");
            assert!(error.is_disconnected(), "{error}");
        })
        .await;
}

/// A failed load of an instance's current revision fails both lookups rather than reporting the
/// instance with no revision.
#[tokio::test]
async fn records_on_branch_fails_on_a_failed_current_revision_load() {
    let global = TempDir::new("lore-records-current-revision-load-global-");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
            let loads = Arc::new(Loads::default());
            let repository =
                in_memory::repository(immutable, failing_loads(mutable, loads.clone()));
            let parent = TempDir::new("lore-records-current-revision-load-");
            let branch = BranchId::from([1; 16]);
            let path = parent.path().join("on-branch");
            write_checkout(&path, repository.instance_id);
            register_on(&repository, &path, branch, Hash::from([2; 32])).await;

            loads
                .failing
                .set(anchor_of(
                    &repository,
                    ANCHOR_CURRENT,
                    repository.instance_id,
                ))
                .expect("arm the failing load");
            let error = records_on_branch(&repository, branch)
                .await
                .expect_err("a failed load fails the lookup");
            assert!(error.is_disconnected(), "{error}");
            let error = records_by_branch(&repository)
                .await
                .expect_err("a failed load fails the lookup");
            assert!(error.is_disconnected(), "{error}");
        })
        .await;
}

fn volume() -> lore_io::clone::Volume {
    lore_io::clone::Volume {
        id: 1,
        filesystem: "apfs".to_string(),
        mount_point: "/".into(),
    }
}

fn copies(size: u64, available: u64) -> lore_revision::fs::filesystem_provider::CloneAnswer {
    lore_revision::fs::filesystem_provider::CloneAnswer::CopiesBytes {
        reason: "the volume does not clone files".to_string(),
        size: lore_revision::fs::walk::TreeSize {
            size,
            file_count: 1,
            entry_count: 1,
        },
        volume: volume(),
        available,
    }
}

fn refusal(
    answer: &lore_revision::fs::filesystem_provider::CloneAnswer,
    allow_copy: bool,
) -> Result<(), InstanceError> {
    refuse_clone(
        answer,
        allow_copy,
        "/source",
        lore_revision::repository::VfsType::None,
        "/destination",
    )
}

#[test]
fn a_clone_that_shares_blocks_is_made() {
    let answer = lore_revision::fs::filesystem_provider::CloneAnswer::SharesBlocks {
        backend: lore_io::clone::Backend::Apfs,
        volume: volume(),
        entry_by_entry: Default::default(),
    };

    assert!(refusal(&answer, false).is_ok());
}

#[test]
fn a_clone_that_would_copy_is_refused_unless_a_copy_is_allowed() {
    assert!(refusal(&copies(100, 1000), false).is_err_and(|err| err.is_not_supported()));
    assert!(refusal(&copies(100, 1000), true).is_ok());
}

#[test]
fn a_copy_is_refused_as_oversized_when_the_destination_has_less_room() {
    assert!(refusal(&copies(100, 99), true).is_err_and(|err| err.is_oversized()));
    assert!(refusal(&copies(100, 100), true).is_ok());
}

/// A FUSE instance's copy is written to the external root, so its refusals name the external root.
#[test]
fn a_fuse_instance_s_refusal_names_the_external_root() {
    let fuse_refusal = |available, allow_copy| {
        refuse_clone(
            &copies(100, available),
            allow_copy,
            "/source",
            lore_revision::repository::VfsType::Fuse,
            "/destination",
        )
        .expect_err("refused")
        .to_string()
    };

    let not_supported = fuse_refusal(1000, false);
    assert!(
        not_supported.contains(
            "Put the external root, which holds the local changes of every FUSE instance,"
        ),
        "{not_supported}"
    );
    let oversized = fuse_refusal(0, true);
    assert!(
        oversized.contains("the external root's volume has"),
        "{oversized}"
    );
}

#[test]
fn an_unsupported_clone_is_refused_even_with_a_copy_allowed() {
    let answer = lore_revision::fs::filesystem_provider::CloneAnswer::Unsupported {
        reason: "this filesystem provider cannot clone instances".to_string(),
    };

    assert!(refusal(&answer, false).is_err_and(|err| err.is_not_supported()));
    assert!(refusal(&answer, true).is_err_and(|err| err.is_not_supported()));
}

/// Only a name made of the destination's, `.cloning-` and a `UUIDv7` instance ID is a clone's
/// in-progress directory.
#[test]
fn only_a_clone_s_name_is_taken_for_one_in_progress() {
    let named = |name: &str| names_clone_in_progress("proj".as_ref(), name.as_ref());
    let v7 = InstanceId::generate();
    let v4 = InstanceId::from_hex(&uuid::Uuid::new_v4().simple().to_string()).expect("an ID");

    assert!(named(&format!("proj.cloning-{v7}")));
    assert!(!named("proj.cloning-notes"));
    assert!(!named("proj.cloning-2"));
    assert!(!named(&format!("proj.cloning-{v4}")));
    assert!(!named(&format!("proj.cloning-{v7}.lock")));
    assert!(!named(&format!("other.cloning-{v7}")));
    assert!(!named(&format!("proj.cloning-{v7}x")));
}

/// A destination whose name leaves no room for the names cloning writes beside it is refused when
/// cloning an on-disk instance, and taken when cloning a virtual clone, which writes nothing beside
/// its destination.
#[tokio::test]
async fn a_long_destination_name_is_refused_only_where_the_clone_writes_beside_it() {
    let global = TempDir::new("lore-instance-long-name-global-");
    let parent = TempDir::new("lore-instance-long-name-");
    let target = parent.path().join("n".repeat(240));
    let target = target.to_str().expect("a UTF-8 path");
    LORE_CONTEXT
        .scope(execution_in(global.path()), async {
            assert!(matches!(
                resolve_clone_destination(target, true),
                Err(InstanceError::InvalidArguments(_))
            ));
            let resolved = resolve_clone_destination(target, false)
                .expect("taken when cloning a virtual clone");
            assert_eq!(resolved.file_name(), Path::new(target).file_name());
        })
        .await;
}

/// The instance events an execution sent, each as its instance and `stale` code.
type InstanceEvents = Arc<Mutex<Vec<(InstanceId, u8)>>>;

/// An execution whose global data directory is `global`, recording each instance event it sends
/// in [`InstanceEvents`]. The list is complete once its dispatcher is drained.
fn recording_execution_in(
    global: &Path,
) -> (
    InstanceEvents,
    Arc<lore_revision::interface::ExecutionContext>,
) {
    let events = InstanceEvents::default();
    let sink = events.clone();
    let callback: lore_revision::interface::LoreEventCallback =
        Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::RepositoryInstance(data) = event {
                sink.lock()
                    .expect("the instance events")
                    .push((data.instance_id, data.stale));
            }
        }));
    let environment = lore_base::env::CallEnvironment {
        values: [Some(global.display().to_string()), None, None],
    };
    let execution = Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            Default::default(),
            lore_revision::relay::EventDispatcher::new(callback),
            "test-user".to_string(),
        )
        .with_environment(environment),
    );
    (events, execution)
}

/// Prune keeps the registration and anchors of virtual instances whose external directory is
/// unknown, though their paths are gone; listing and the records report them active; and the
/// records resolve the external root once for all of them.
#[tokio::test]
async fn prune_listing_and_records_keep_unknown_virtual_instances() {
    let global = TempDir::new("lore-instance-unknown-prune-global-");
    let volume = TempDir::new("lore-instance-unknown-prune-volume-");
    let (events, execution) = recording_execution_in(global.path());
    LORE_CONTEXT
        .scope(execution.clone(), async {
            let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let parent = TempDir::new("lore-instance-unknown-prune-");
            let branch = BranchId::from([7; 16]);
            let revision = Hash::from([8; 32]);
            let mut instances = Vec::new();
            for index in 0..3 {
                let instance =
                    in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
                register_on(
                    &instance,
                    &parent.path().join(format!("gone-{index}")),
                    branch,
                    revision,
                )
                .await;
                instances.push(instance.instance_id);
            }
            make_unknown(UnknownRoot::MissingRoot, volume.path(), instances[0]).await;
            let pruner = in_memory::repository_in(immutable, mutable.clone(), repository_id);
            let data =
                lore_revision::global::get_global_data_dir().expect("the global data directory");

            let before = reads_in(&data).setting;
            let records = records_on_branch(&pruner, branch)
                .await
                .expect("look up the branch");
            assert_eq!(reads_in(&data).setting, before + 1, "one resolution");
            assert_eq!(records.len(), 3);
            assert!(
                records
                    .iter()
                    .all(|record| record.staleness == InstanceStaleness::Active)
            );

            instance_list(pruner.clone())
                .await
                .expect("list the instances");
            assert_eq!(
                instance_prune(pruner.clone()).await.expect("prune"),
                0,
                "prune retires none"
            );
            for &instance_id in &instances {
                let (key, key_type) = instance_key(pruner.salt(), instance_id);
                let registration = mutable
                    .clone()
                    .load(repository_id, key, key_type)
                    .await
                    .expect("the registration loads");
                assert!(!registration.is_zero(), "{instance_id} stays registered");
                let (key, key_type) = anchor_key(pruner.salt(), ANCHOR_CURRENT, instance_id);
                let anchor = mutable
                    .clone()
                    .load(repository_id, key, key_type)
                    .await
                    .expect("the anchor loads");
                assert_eq!(anchor, revision, "{instance_id} keeps its anchor");
            }
        })
        .await;
    execution.dispatcher.drain().await;
    let mut listed = events.lock().expect("the instance events").clone();
    listed.sort();
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert!(listed.iter().all(|&(_, stale)| stale == 0), "{listed:?}");
}

/// What the external directory of a virtual instance registered at a path holds.
#[derive(Clone, Copy, Debug)]
enum RegisteredExternal {
    /// A config naming this VFS.
    Names(&'static [u8]),
    /// A config that can't be read.
    Unreadable,
    /// Nothing that can be told: the setting names a missing root.
    Unknown,
}

/// Registers another instance at `path` in `repository`'s store, whose external directory holds
/// `external`, in the global data directory of the call in scope, and answers its id.
async fn register_virtual_at(
    repository: &Arc<RepositoryContext>,
    path: &Path,
    external: RegisteredExternal,
    volume: &Path,
) -> InstanceId {
    let instance_id = InstanceId::generate();
    register_instance(repository, instance_id, &path.display().to_string())
        .await
        .expect("register the virtual instance");
    let state = resolve().expect("resolve the external root");
    let dot_lore = external_dir_for_instance(&state, instance_id)
        .expect("the external directory")
        .join(lore_revision::repository::DOT_LORE);
    match external {
        RegisteredExternal::Names(config) => {
            std::fs::create_dir_all(&dot_lore).expect("make the external directory");
            std::fs::write(dot_lore.join(CONFIG), config).expect("write the config");
        }
        RegisteredExternal::Unreadable => {
            std::fs::create_dir_all(dot_lore.join(CONFIG)).expect("occupy the config path");
        }
        RegisteredExternal::Unknown => {
            make_unknown(UnknownRoot::MissingRoot, volume, instance_id).await;
        }
    }
    instance_id
}

/// Whether `instance_id` is registered in `repository`'s store.
async fn is_registered(repository: &Arc<RepositoryContext>, instance_id: InstanceId) -> bool {
    let (key, key_type) = instance_key(repository.salt(), instance_id);
    repository
        .read_mutable_store()
        .load(repository.id, key, key_type)
        .await
        .is_ok_and(|registration| !registration.is_zero())
}

/// Registering an instance at the path of another registration whose external directory names a
/// virtual instance, can't be read or is unknown is refused with `InvalidArguments`, registers
/// nothing, and keeps the other registration, as update-path and instance clone register through
/// it.
#[tokio::test]
async fn registration_refuses_the_path_of_a_registered_virtual_instance() {
    for external in [
        RegisteredExternal::Names(FUSE_CONFIG),
        RegisteredExternal::Names(SWFS_CONFIG),
        RegisteredExternal::Unreadable,
        RegisteredExternal::Unknown,
    ] {
        let global = TempDir::new("lore-register-refusal-global-");
        let volume = TempDir::new("lore-register-refusal-volume-");
        let parent = TempDir::new("lore-register-refusal-path-");
        LORE_CONTEXT
            .scope(execution_in(global.path()), async {
                let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
                let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
                let path = parent.path().join("instance");
                let registrar =
                    in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
                let virtual_id =
                    register_virtual_at(&registrar, &path, external, volume.path()).await;
                let new = in_memory::repository_in(immutable, mutable, repository_id);

                let refused =
                    register_instance(&new, new.instance_id, &path.display().to_string()).await;

                let error = refused.expect_err("the registration is refused");
                assert!(error.is_invalid_arguments(), "{external:?}: {error}");
                assert!(is_registered(&new, virtual_id).await, "{external:?}");
                assert!(!is_registered(&new, new.instance_id).await, "{external:?}");
            })
            .await;
    }
}

/// An ordinary instance's registration at the path, with an external directory that names no
/// virtual instance or none at all, is still retired by a registration there.
#[tokio::test]
async fn registration_still_retires_an_ordinary_instance_at_its_path() {
    for external in [
        RegisteredExternal::Names(b"[vfs]\nvfs_type = \"None\"\n"),
        RegisteredExternal::Names(b"remote_url = \"lore://host\"\n"),
    ] {
        let global = TempDir::new("lore-register-retire-global-");
        let volume = TempDir::new("lore-register-retire-volume-");
        let parent = TempDir::new("lore-register-retire-path-");
        LORE_CONTEXT
            .scope(execution_in(global.path()), async {
                let (immutable, mutable, _) = test_store_create().await.expect("make the stores");
                let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
                let path = parent.path().join("instance");
                let registrar =
                    in_memory::repository_in(immutable.clone(), mutable.clone(), repository_id);
                let ordinary =
                    register_virtual_at(&registrar, &path, external, volume.path()).await;
                let without = InstanceId::generate();
                store_instance_registration(&registrar, without, &path.display().to_string())
                    .await
                    .expect("register an instance with no external directory");
                let new = in_memory::repository_in(immutable, mutable, repository_id);

                register_instance(&new, new.instance_id, &path.display().to_string())
                    .await
                    .expect("the registration goes ahead");

                assert!(!is_registered(&new, ordinary).await, "{external:?}");
                assert!(!is_registered(&new, without).await);
                assert!(is_registered(&new, new.instance_id).await);
            })
            .await;
    }
}
