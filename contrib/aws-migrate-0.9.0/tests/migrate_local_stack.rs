// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![cfg(feature = "integration_tests")]

//! End-to-end migration of a store written the old way, against a local `MinIO` and `DynamoDB`
//! Local.
//!
//! The store is seeded the way one that predates the fragment moving onto the S3 object looks:
//! bare objects with no metadata of their own, a row per fragment in the legacy metadata table,
//! and the associations that make those fragments readable. Among them is a piece of content too
//! large for one fragment, held as the pieces it was cut into and a list naming them. The tool is
//! then run as an operator runs it, interrupted, run again, and the result checked from both ends
//! — the state rows and object headers the migration was supposed to write, and a read through the
//! store with the legacy table dropped, which is the state the deployment is left in.
//!
//! Requires the services in `lore-integration-tests/compose.yaml`; see the README.

use std::collections::HashMap;
use std::future::Future;
use std::mem::size_of;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::AttributeDefinition;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::types::KeySchemaElement;
use aws_sdk_dynamodb::types::KeyType;
use aws_sdk_dynamodb::types::ProvisionedThroughput;
use aws_sdk_dynamodb::types::ScalarAttributeType;
use bytes::Bytes;
use lore_aws::clients::AwsClientBuilder;
use lore_aws::clients::HttpClientSettings;
use lore_aws::dynamodb::DynamoDb;
use lore_aws::dynamodb::ScanConfig;
use lore_aws::s3::S3;
use lore_aws::store::immutable_store::AwsImmutableStore;
use lore_aws::store::immutable_store::AwsImmutableStoreSettings;
use lore_aws::store::immutable_store::DynamoDbImmutableStoreSettings;
use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE;
use lore_aws::store::immutable_store::FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE;
use lore_aws::store::immutable_store::S3StoreSettings;
use lore_aws::store::object_metadata::from_object_metadata;
use lore_storage::Address;
use lore_storage::CompressionMode;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentFlags;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::ImmutableStore;
use lore_storage::Partition;
use lore_storage::StoreGetData;
use lore_storage::TypedBytes;
use tokio::task::JoinSet;
use zerocopy::IntoBytes;

/// The endpoints and credentials `lore-integration-tests/compose.yaml` brings up.
const S3_ENDPOINT: &str = "http://127.0.0.1:9000";
const DYNAMODB_ENDPOINT: &str = "http://127.0.0.1:9090";
const REGION: &str = "us-east-1";
const ACCESS_KEY: &str = "lorelocal";
const SECRET_KEY: &str = "lorelocal";

/// The payload bucket, created on first use. Shared between runs: objects are keyed by content
/// hash and every run seeds its own, so nothing a run writes is another run's.
const BUCKET: &str = "lore-aws-migrate-local";

/// The attribute a state row carries and a legacy metadata row does not, which is how the two
/// shapes are told apart.
const STATE_ATTRIBUTE: &str = "state";

/// The value [`STATE_ATTRIBUTE`] holds for a payload that is stored and readable. An obliteration
/// sets a flag bit instead, so this says the row is not a tombstone as well as that it exists.
const STORED_STATE: &str = "0";

/// The attribute naming the hash, in every table this test writes.
const HASH_ATTRIBUTE: &str = "hash";

/// How many standalone fragments the store is seeded with, on top of the fragmented piece.
///
/// A population rather than a handful: a migration is a scan, and a scan over a few rows exercises
/// neither its pagination nor the run being long enough to interrupt with work left over.
const STANDALONE_COUNT: usize = 1024;

/// How many pieces the large content is cut into.
const CHUNK_COUNT: usize = 16;

/// How large each of those pieces is. One fragment's worth, well inside the size threshold.
const CHUNK_SIZE: usize = 64 * 1024;

/// Every fragment the migration has to convert: the standalone ones, the pieces of the large
/// content, and the list naming those pieces, which is a fragment of its own.
const FRAGMENT_TOTAL: usize = STANDALONE_COUNT + CHUNK_COUNT + 1;

/// Rows the scan reads: every fragment above, and the obliterated one it has to leave alone.
const METADATA_ROW_TOTAL: usize = FRAGMENT_TOTAL + 1;

/// How many of this test's own requests are in flight at once.
///
/// The store takes thousands of round trips to seed and as many again to check. Issuing them one
/// at a time is most of the wall clock at this size, and none of them depend on each other.
const CONCURRENCY: usize = 32;

/// How often a poll checks on the running tool.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How many times it checks before giving up, at [`POLL_INTERVAL`] each. Generous: a run opens its
/// clients and scans before it converts anything, and a resumed run re-reads every row it already
/// converted before it reaches new work.
const POLL_ATTEMPTS: usize = 12_000;

/// How a fragment was stored before the migration, and therefore which recovery path it exercises.
#[derive(Clone, Copy)]
enum Legacy {
    /// Stored uncompressed and described accurately.
    Uncompressed,
    /// Zstd, described accurately.
    Zstd,
    /// LZ4, described accurately.
    Lz4,
    /// Zstd bytes described as LZ4, so the codec has to be deduced by probing.
    MislabelledCodec,
    /// Zstd bytes behind the fragment struct the oldest objects were written with, which has to be
    /// stripped before any codec matches.
    PrefixedPayload,
}

/// The shapes the seeded store is built from, one per fragment in turn.
const SHAPES: [Legacy; 5] = [
    Legacy::Uncompressed,
    Legacy::Zstd,
    Legacy::Lz4,
    Legacy::MislabelledCodec,
    Legacy::PrefixedPayload,
];

/// One fragment as the old layout held it.
#[derive(Clone)]
struct Seeded {
    address: Address,
    /// The row the legacy metadata table holds, describing the object exactly as it was written.
    row: Fragment,
    /// The object's bytes, carrying no metadata of their own.
    object: Bytes,
}

/// A piece of content too large for one fragment: the pieces it was cut into, the list naming
/// them, and the bytes the two together stand for.
struct Fragmented {
    /// The fragment whose payload is the reference list. Its hash is over that list, not over the
    /// content the list stands for, which is what makes it a fragment the migration can check.
    list: Seeded,
    chunks: Vec<Seeded>,
    content: Bytes,
}

/// The three tables one run of this test owns.
///
/// Named per run: a leftover state row would read as a fragment already migrated, which is the one
/// thing that would make an interrupted run indistinguishable from a finished one.
struct Tables {
    fragments: Arc<str>,
    state: Arc<str>,
    metadata: Arc<str>,
}

impl Tables {
    fn new() -> Self {
        let run: u64 = rand::random();
        Self {
            fragments: Arc::from(format!("lore-migrate-test-fragments-{run:016x}")),
            state: Arc::from(format!("lore-migrate-test-state-{run:016x}")),
            metadata: Arc::from(format!("lore-migrate-test-metadata-{run:016x}")),
        }
    }
}

/// How an interrupted run ended, and how much of the store it had converted by then.
struct Interrupted {
    status: ExitStatus,
    migrated: usize,
}

/// The counts a run ends on, by the names it logs them under.
struct Totals(HashMap<String, u64>);

impl Totals {
    /// The count logged under `name`. Every outcome is reported on the final line, so a missing one
    /// is a total the tool stopped keeping rather than an outcome that did not occur.
    fn get(&self, name: &str) -> u64 {
        *self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("the final totals should report {name}, got {:?}", self.0))
    }
}

/// A store written the old way is migrated in full across an interrupted run and a resumed one,
/// then reads back through the new layout with the legacy table gone — the large content included,
/// which only reassembles if every piece of it and the list naming them all came through.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_migration_finishes_on_the_next_run_and_reads_back_without_the_legacy_table()
{
    let tables = Arc::new(Tables::new());
    let dynamodb = dynamodb_client().await;
    let s3 = Arc::new(s3_client().await);
    create_bucket(&s3).await;
    create_tables(&dynamodb, &tables).await;

    let partition: Partition = rand::random();
    let fragmented = seed_fragmented();
    let seeded = everything_seeded(&fragmented);
    let obliterated = seed_obliterated();
    assert_eq!(seeded.len(), FRAGMENT_TOTAL);

    for_each_fragment(&seeded, |fragment| {
        let (s3, dynamodb, tables) = (s3.clone(), dynamodb.clone(), tables.clone());
        async move { write_legacy_fragment(&s3, &dynamodb, &tables, partition, &fragment).await }
    })
    .await;
    write_object_and_row(&s3, &dynamodb, &tables, &obliterated).await;

    let interrupted = run_until_interrupted(&tables, &dynamodb).await;
    assert!(
        !interrupted.status.success(),
        "an interrupted run must report that it did not finish, got {}",
        interrupted.status
    );
    assert!(
        (1..FRAGMENT_TOTAL).contains(&interrupted.migrated),
        "the interrupt must leave work behind, but {} of {FRAGMENT_TOTAL} were migrated",
        interrupted.migrated
    );

    let resumed = run_to_completion(&tables).await;
    assert!(
        resumed.success(),
        "resuming must finish the migration, got {resumed}"
    );

    assert_verification_pass_is_clean(&run_dry_run(&tables).await);

    for_each_fragment(&seeded, |fragment| {
        let (s3, dynamodb, tables) = (s3.clone(), dynamodb.clone(), tables.clone());
        async move { assert_migrated(&s3, &dynamodb, &tables, &fragment).await }
    })
    .await;

    drop_table(&dynamodb, &tables.metadata).await;

    let store = migrated_store(s3_client().await, dynamodb.clone(), &tables);
    for_each_fragment(&seeded, |fragment| {
        let store = store.clone();
        async move { assert_reads_back(&store, partition, fragment.address).await }
    })
    .await;
    assert_reconstructs(&store, partition, &fragmented).await;
    assert_left_obliterated(&dynamodb, &tables, &store, partition, &obliterated).await;

    let mut written = seeded;
    written.push(obliterated);
    clean_up(&s3, &dynamodb, &tables, &written).await;
}

/// Every fragment the store is seeded with, in the order they are written.
///
/// The pieces of the large content and the list naming them are fragments like any other, so the
/// migration has to find them by the same scan and the checks cover them by the same loops.
fn everything_seeded(fragmented: &Fragmented) -> Vec<Seeded> {
    let mut seeded: Vec<Seeded> = (0..STANDALONE_COUNT).map(seed).collect();
    seeded.extend(fragmented.chunks.iter().cloned());
    seeded.push(fragmented.list.clone());
    seeded
}

/// The content a seeded fragment is the hash of.
///
/// Repetitive so every codec beats the compression threshold, and tagged with the index so no two
/// fragments share a hash.
fn seeded_content(label: &str, index: usize, size: usize) -> Vec<u8> {
    let mut content = format!("lore-aws-migrate {label} {index}\n").into_bytes();
    content.resize(
        size,
        u8::try_from(index % 251).expect("the index modulus is a byte"),
    );
    content
}

/// One standalone fragment, in the shape its index calls for and under a context of its own.
fn seed(index: usize) -> Seeded {
    seeded_fragment(
        seeded_content("fragment", index, 1024),
        SHAPES[index % SHAPES.len()],
        rand::random(),
    )
}

/// One fragment in the given legacy shape: the object bytes, and the metadata row that describes
/// them.
fn seeded_fragment(content: Vec<u8>, shape: Legacy, context: Context) -> Seeded {
    let hash = lore_storage::hash_slice(&content);
    let raw = Fragment {
        flags: 0,
        size_payload: content.len() as u32,
        size_content: content.len() as u64,
    };

    let (row, object) = match shape {
        Legacy::Uncompressed => (raw, Bytes::from(content)),
        Legacy::Zstd => compressed(raw, &content, CompressionMode::Zstd),
        Legacy::Lz4 => compressed(raw, &content, CompressionMode::Lz4),
        Legacy::MislabelledCodec => {
            let (fragment, object) = compressed(raw, &content, CompressionMode::Zstd);
            (
                mislabelled(fragment, FragmentFlags::PayloadCompressedLZ4),
                object,
            )
        }
        Legacy::PrefixedPayload => {
            let (fragment, payload) = compressed(raw, &content, CompressionMode::Zstd);
            let object = prefixed(&payload);
            (
                Fragment {
                    size_payload: object.len() as u32,
                    ..fragment
                },
                object,
            )
        }
    };

    Seeded {
        address: Address { hash, context },
        row,
        object,
    }
}

/// A piece of content too large for one fragment, cut into [`CHUNK_COUNT`] pieces with a list
/// naming them — the shape the store holds anything past the fragment size threshold in.
///
/// Every fragment of it shares one context, as a fragmented write gives them. The pieces take the
/// same legacy shapes as the standalone fragments, so each has to be recovered on its own terms
/// and the content only reassembles if all of them were.
fn seed_fragmented() -> Fragmented {
    let context: Context = rand::random();
    let mut content = Vec::with_capacity(CHUNK_COUNT * CHUNK_SIZE);
    let mut references = Vec::with_capacity(CHUNK_COUNT);
    let mut chunks = Vec::with_capacity(CHUNK_COUNT);

    for index in 0..CHUNK_COUNT {
        let piece = seeded_content("chunk", index, CHUNK_SIZE);
        let offset_content = content.len() as u64;
        content.extend_from_slice(&piece);

        let chunk = seeded_fragment(piece, SHAPES[index % SHAPES.len()], context);
        references.push(FragmentReference {
            hash: chunk.address.hash,
            offset_content,
        });
        chunks.push(chunk);
    }

    Fragmented {
        list: fragment_list(&references, content.len(), context),
        chunks,
        content: Bytes::from(content),
    }
}

/// The fragment naming the pieces. Its payload is the reference list itself, so its hash is over
/// that list, and `size_content` is the content the list stands for rather than the payload's own
/// length — the one shape where those two legitimately differ.
fn fragment_list(
    references: &[FragmentReference],
    size_content: usize,
    context: Context,
) -> Seeded {
    let object = Bytes::copy_from_slice(references.as_bytes());
    let row = Fragment {
        flags: FragmentFlags::PayloadFragmented.bits(),
        size_payload: object.len() as u32,
        size_content: size_content as u64,
    };

    Seeded {
        address: Address {
            hash: lore_storage::hash_slice(&object),
            context,
        },
        row,
        object,
    }
}

/// A fragment the store obliterated: the row carries the obliteration flag and no association is
/// left pointing at it.
///
/// Its payload is still in the bucket, which is the shape an obliteration interrupted before it
/// deleted the object leaves. One that got that far leaves a row whose object the migration cannot
/// load at all, and reports it as an error rather than as obliterated.
fn seed_obliterated() -> Seeded {
    let mut fragment = seeded_fragment(
        seeded_content("obliterated", 0, 1024),
        Legacy::Uncompressed,
        rand::random(),
    );
    fragment.row.flags |= FragmentFlags::PayloadObliterated.bits();
    fragment
}

/// A payload compressed the way a store written before the migration held it, with the fragment
/// that describes it.
fn compressed(raw: Fragment, content: &[u8], mode: CompressionMode) -> (Fragment, Bytes) {
    lore_storage::compress(raw, content, mode).expect("the seeded content compresses")
}

/// The same fragment declaring a different codec, as a row written by a version that got the codec
/// wrong carries.
fn mislabelled(fragment: Fragment, codec: FragmentFlags) -> Fragment {
    Fragment {
        flags: (fragment.flags & !FragmentFlags::PayloadCompressed) | codec.bits(),
        ..fragment
    }
}

/// The oldest objects carried the fragment struct ahead of the payload. Zeroes stand in for it:
/// the migration recovers the payload by stripping that many bytes, never by reading them.
fn prefixed(payload: &Bytes) -> Bytes {
    let mut object = vec![0u8; size_of::<Fragment>()];
    object.extend_from_slice(payload);
    Bytes::from(object)
}

/// Runs `work` over every fragment, keeping [`CONCURRENCY`] of them in flight, and waits for all
/// of them.
///
/// This test carries no execution context to propagate, as the tool it drives does not.
#[allow(clippy::disallowed_methods)]
async fn for_each_fragment<F>(fragments: &[Seeded], work: impl Fn(Seeded) -> F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut running = JoinSet::new();

    for fragment in fragments {
        running.spawn(work(fragment.clone()));
        if running.len() >= CONCURRENCY {
            join_one(&mut running).await;
        }
    }

    while !running.is_empty() {
        join_one(&mut running).await;
    }
}

/// Waits for one task, re-raising a panic where it was raised so a failed assertion inside a task
/// fails the test with its own message rather than with a join error.
async fn join_one(running: &mut JoinSet<()>) {
    let Some(joined) = running.join_next().await else {
        return;
    };

    if let Err(error) = joined {
        std::panic::resume_unwind(error.into_panic());
    }
}

/// Writes one fragment into the store the way the old layout held it: bare object, metadata row,
/// and the association that makes it readable. The migration rewrites the first two and leaves the
/// association alone.
async fn write_legacy_fragment(
    s3: &S3,
    dynamodb: &DynamoDb,
    tables: &Tables,
    partition: Partition,
    fragment: &Seeded,
) {
    write_object_and_row(s3, dynamodb, tables, fragment).await;

    dynamodb
        .put_item(&tables.fragments, association(partition, fragment.address))
        .await
        .expect("writing the association should succeed");
}

/// The bare object and the metadata row describing it, which is everything an obliterated fragment
/// has left. A fragment still in use gets an association on top.
async fn write_object_and_row(s3: &S3, dynamodb: &DynamoDb, tables: &Tables, fragment: &Seeded) {
    s3.put_object(
        BUCKET,
        &fragment.address.hash.to_string(),
        fragment.object.clone(),
        None,
    )
    .await
    .expect("writing the pre-migration object should succeed");

    dynamodb
        .put_item(&tables.metadata, legacy_row(fragment))
        .await
        .expect("writing the pre-migration metadata row should succeed");
}

/// The metadata row that era wrote: the whole fragment flattened alongside the hash, and no state
/// attribute.
fn legacy_row(fragment: &Seeded) -> HashMap<String, AttributeValue> {
    HashMap::from([
        (HASH_ATTRIBUTE.to_owned(), hash_value(fragment.address.hash)),
        (
            "flags".to_owned(),
            AttributeValue::N(fragment.row.flags.to_string()),
        ),
        (
            "size_payload".to_owned(),
            AttributeValue::N(fragment.row.size_payload.to_string()),
        ),
        (
            "size_content".to_owned(),
            AttributeValue::N(fragment.row.size_content.to_string()),
        ),
    ])
}

/// A row in the fragments table, whose sort key is the partition holding the reference followed by
/// the address's context.
fn association(partition: Partition, address: Address) -> HashMap<String, AttributeValue> {
    let mut partition_context = Vec::with_capacity(partition.data().len() * 2);
    partition_context.extend_from_slice(partition.data());
    partition_context.extend_from_slice(address.context.data());

    HashMap::from([
        (
            FRAGMENTS_DYNAMO_PARTITION_KEY_ATTRIBUTE.to_owned(),
            hash_value(address.hash),
        ),
        (
            FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE.to_owned(),
            AttributeValue::B(Blob::new(partition_context)),
        ),
    ])
}

/// A hash as the binary attribute every one of these tables keys on.
fn hash_value(hash: Hash) -> AttributeValue {
    AttributeValue::B(Blob::new(hash.data().to_vec()))
}

/// The migrate tool, pointed at the local stack and at the tables this run owns.
///
/// Modest consumer count on purpose: the interrupt is sent as soon as the first fragment is
/// converted, and the fewer are in flight behind it the more of the store is left for the resumed
/// run to find.
fn migrate_command(tables: &Tables) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lore-aws-migrate"));
    command
        .args(["--s3-bucket", BUCKET])
        .args(["--fragments-table", &tables.fragments])
        .args(["--fragment-state-table", &tables.state])
        .args(["--fragment-metadata-table", &tables.metadata])
        .args(["--s3-endpoint-url", S3_ENDPOINT])
        .args(["--dynamodb-endpoint-url", DYNAMODB_ENDPOINT])
        .args(["--region", REGION])
        .arg("--s3-force-path-style")
        .args(["--consumers", "8"])
        .args(["--progress-interval-secs", "0"])
        .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
        .env("AWS_SECRET_ACCESS_KEY", SECRET_KEY);
    command
}

/// Starts the tool, interrupts it once it has converted a fragment, and reports how the run ended
/// and how much of the store it had converted.
async fn run_until_interrupted(tables: &Tables, dynamodb: &DynamoDb) -> Interrupted {
    let mut child = migrate_command(tables)
        .spawn()
        .expect("the migrate tool should start");

    await_first_migration(dynamodb, &tables.state, &mut child).await;
    interrupt(&child);
    let status = await_exit(&mut child).await;

    Interrupted {
        status,
        migrated: scan(dynamodb, &tables.state).await.len(),
    }
}

/// Runs the tool through to its own end, and reports how it exited.
async fn run_to_completion(tables: &Tables) -> ExitStatus {
    let mut child = migrate_command(tables)
        .spawn()
        .expect("the migrate tool should start");

    await_exit(&mut child).await
}

/// Runs the tool as the verification pass the README describes, and reports the totals it ended
/// on. Its output is captured rather than inherited, because those totals are the answer this pass
/// gives.
///
/// Waiting for the run fills its pipes as it goes, which polling for the exit would not, and this
/// test has no execution context to propagate onto the blocking pool.
#[allow(clippy::disallowed_methods)]
async fn run_dry_run(tables: &Tables) -> Totals {
    let mut command = migrate_command(tables);
    command.arg("--dry-run");

    let output =
        tokio::task::spawn_blocking(move || command.output().expect("the migrate tool should run"))
            .await
            .expect("the verification pass should not panic");

    assert!(
        output.status.success(),
        "the verification pass should complete, got {}",
        output.status
    );

    let log = [output.stdout, output.stderr].concat();
    totals(&String::from_utf8_lossy(&log))
}

/// A verification pass over a fully migrated store finds every fragment already migrated, the
/// obliterated one still obliterated, and nothing else, which is what the README has an operator
/// read before retiring the legacy table.
fn assert_verification_pass_is_clean(totals: &Totals) {
    assert_eq!(
        totals.get("metadata_rows"),
        METADATA_ROW_TOTAL as u64,
        "the pass should read every row the store was seeded with"
    );
    assert_eq!(
        totals.get("already_migrated"),
        FRAGMENT_TOTAL as u64,
        "every fragment should be found migrated"
    );
    assert_eq!(
        totals.get("obliterated"),
        1,
        "the obliterated fragment should be passed over on every pass, not converted"
    );

    for outcome in [
        "maintained",
        "recompressed_oodle",
        "recompressed_mismatch",
        "stored_uncompressed",
        "unreadable",
        "oversized",
        "errored",
    ] {
        assert_eq!(
            totals.get(outcome),
            0,
            "a migrated store should report no {outcome}"
        );
    }
}

/// The totals off a finished run's log, which is read across both of its output streams so that
/// which one the log subscriber writes to is not part of what this asserts.
///
/// The tool reports only through its log, so this is the line an operator reads to decide a store
/// is migrated. Reading the same one is what ties the README's verification to what the tool does.
fn totals(log: &str) -> Totals {
    let line = log
        .lines()
        .map(without_escapes)
        .find(|line| line.contains(r#"phase="final""#))
        .expect("a finished run logs its final totals");

    Totals(
        line.split_whitespace()
            .filter_map(|field| {
                let (name, value) = field.split_once('=')?;
                Some((name.to_owned(), value.parse().ok()?))
            })
            .collect(),
    )
}

/// The line with its colouring removed, which the log carries whenever the tool is built with it.
///
/// Only the select graphic renditions a log is coloured with are stripped: an escape runs to the
/// `m` that ends it.
fn without_escapes(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut characters = line.chars();

    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            characters.by_ref().take_while(|c| *c != 'm').for_each(drop);
        } else {
            plain.push(character);
        }
    }

    plain
}

/// Waits for the run to publish its first state row, so the interrupt lands on a run that has
/// started converting rather than one still opening its clients.
async fn await_first_migration(dynamodb: &DynamoDb, table: &Arc<str>, child: &mut Child) {
    for _ in 0..POLL_ATTEMPTS {
        if !scan(dynamodb, table).await.is_empty() {
            return;
        }
        assert!(
            running(child),
            "the run ended before it had migrated anything"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("no fragment was migrated before the poll gave up");
}

/// Stops the run the way an operator does, with the interrupt its handler listens for. Anything
/// harsher would end it mid-fragment and say nothing about resuming.
fn interrupt(child: &Child) {
    let pid = libc::pid_t::try_from(child.id()).expect("a process id fits a pid_t");

    // SAFETY: `kill` reads no memory through the arguments, and the child is still owned here, so
    // the id names it rather than whatever the system may have reused.
    let sent = unsafe { libc::kill(pid, libc::SIGINT) };

    assert_eq!(
        sent, 0,
        "the run should still be going when the interrupt is sent"
    );
}

/// The tool's exit status, waited for without blocking the runtime.
async fn await_exit(child: &mut Child) -> ExitStatus {
    for _ in 0..POLL_ATTEMPTS {
        if let Some(status) = child
            .try_wait()
            .expect("the child's status should be readable")
        {
            return status;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let _ = child.kill();
    panic!("the migrate tool did not exit before the poll gave up");
}

/// Whether the run is still going.
fn running(child: &mut Child) -> bool {
    child
        .try_wait()
        .expect("the child's status should be readable")
        .is_none()
}

/// Every fragment carries a state row of the new shape and an object describing itself, which is
/// the whole of what the migration was there to write.
async fn assert_migrated(s3: &S3, dynamodb: &DynamoDb, tables: &Tables, fragment: &Seeded) {
    let hash = fragment.address.hash;
    let row = get_row(dynamodb, &tables.state, hash)
        .await
        .unwrap_or_else(|| panic!("{hash} should have a state row"));
    let state = row.get(STATE_ATTRIBUTE).and_then(|value| value.as_n().ok());
    assert_eq!(
        state.map(String::as_str),
        Some(STORED_STATE),
        "{hash} should have a state row saying its payload is stored, got {row:?}"
    );

    let head = s3
        .head_object(BUCKET, &hash.to_string())
        .await
        .unwrap_or_else(|error| panic!("{hash} should still have an object: {error:?}"));
    let stored = from_object_metadata(head.metadata())
        .unwrap_or_else(|error| panic!("{hash} should carry its fragment in its header: {error}"));

    assert_eq!(
        i64::from(stored.size_payload),
        head.content_length()
            .expect("S3 reports the length of an object it heads"),
        "the header on {hash} should describe the bytes it was written with"
    );
}

/// The payload reads back through the store as the content its hash is over, with only the state
/// table and the object's own header left to describe it.
async fn assert_reads_back(store: &Arc<AwsImmutableStore>, partition: Partition, address: Address) {
    let (fragment, payload) = load(store, partition, address).await;

    assert_eq!(
        lore_storage::hash_slice(&content_of(fragment, payload)),
        address.hash,
        "{address} should read back as the content it is the hash of"
    );
}

/// The large content comes back whole: its list still reads as a list, every piece it names reads
/// as its own content, and the pieces reassemble into the bytes the content was cut from.
///
/// This is the check a hash-per-fragment cannot make. Each piece proving it is its own hash says
/// nothing about whether the list still names them, in the right order, at the right offsets.
async fn assert_reconstructs(
    store: &Arc<AwsImmutableStore>,
    partition: Partition,
    fragmented: &Fragmented,
) {
    let address = fragmented.list.address;
    let (list, payload) = load(store, partition, address).await;

    assert_ne!(
        list.flags & FragmentFlags::PayloadFragmented,
        0,
        "the list at {address} should still read as one"
    );
    assert_eq!(
        list.size_content,
        fragmented.content.len() as u64,
        "the list should still stand for the whole content"
    );

    let aligned = payload.to_aligned::<FragmentReference>();
    let references = aligned.as_type_slice::<FragmentReference>();
    assert_eq!(
        references.len(),
        CHUNK_COUNT,
        "the list should still name every piece"
    );

    let mut reassembled = Vec::with_capacity(fragmented.content.len());
    for reference in references {
        assert_eq!(
            reference.offset_content,
            reassembled.len() as u64,
            "the pieces should still be named in the order they were cut"
        );

        let piece = Address {
            hash: reference.hash,
            context: address.context,
        };
        let (fragment, payload) = load(store, partition, piece).await;
        reassembled.extend_from_slice(&content_of(fragment, payload));
    }

    assert_eq!(
        reassembled.len(),
        fragmented.content.len(),
        "the pieces should reassemble to the length they were cut from"
    );
    assert_eq!(
        lore_storage::hash_slice(&reassembled),
        lore_storage::hash_slice(&fragmented.content),
        "the pieces should reassemble into the content they were cut from"
    );
}

/// The obliterated fragment was left as it was found: no state row, and nothing to read.
///
/// Both halves are the point. A state row would put the hash back into the layout the store now
/// reads, and the association the obliteration removed is what keeps the payload unreachable
/// without one.
async fn assert_left_obliterated(
    dynamodb: &DynamoDb,
    tables: &Tables,
    store: &Arc<AwsImmutableStore>,
    partition: Partition,
    fragment: &Seeded,
) {
    let hash = fragment.address.hash;
    assert!(
        get_row(dynamodb, &tables.state, hash).await.is_none(),
        "{hash} was obliterated and should have got no state row"
    );

    let error = store
        .clone()
        .get(partition, fragment.address)
        .await
        .err()
        .unwrap_or_else(|| panic!("{hash} was obliterated and should not read back"));
    assert!(
        error.is_address_not_found(),
        "{hash} should read as absent, got {error:?}"
    );
}

/// One representation, read through the store.
async fn load(
    store: &Arc<AwsImmutableStore>,
    partition: Partition,
    address: Address,
) -> (Fragment, Bytes) {
    store
        .clone()
        .get(partition, address)
        .await
        .and_then(StoreGetData::into_payload)
        .unwrap_or_else(|error| panic!("{address} should read back: {error:?}"))
}

/// The content a stored representation stands for, decompressed where the fragment says it is
/// compressed.
fn content_of(fragment: Fragment, payload: Bytes) -> Bytes {
    if fragment.flags & FragmentFlags::PayloadCompressed == 0 {
        return payload;
    }

    lore_storage::decompress(fragment, &payload)
        .expect("a migrated payload decompresses with the codec its header declares")
        .1
        .freeze()
}

/// A store on the migrated layout: no legacy metadata table, so an object the migration left
/// without a header reads as damaged rather than being described from a row.
fn migrated_store(s3: S3, dynamodb: DynamoDb, tables: &Tables) -> Arc<AwsImmutableStore> {
    let settings = AwsImmutableStoreSettings::new(
        S3StoreSettings::new(BUCKET.to_owned()),
        DynamoDbImmutableStoreSettings::new(tables.fragments.to_string(), tables.state.to_string()),
        false,
    );

    Arc::new(AwsImmutableStore::new(s3, dynamodb, &settings))
}

/// Removes the tables and the objects this run wrote, so a stack that outlives it is left as it
/// was found.
async fn clean_up(s3: &Arc<S3>, dynamodb: &DynamoDb, tables: &Arc<Tables>, written: &[Seeded]) {
    for_each_fragment(written, |fragment| {
        let s3 = s3.clone();
        async move {
            s3.delete_object(BUCKET, &fragment.address.hash.to_string(), None)
                .await
                .expect("deleting a test object should succeed");
        }
    })
    .await;

    drop_table(dynamodb, &tables.fragments).await;
    drop_table(dynamodb, &tables.state).await;
}

/// The clients the seeding and the checks go through, pointed at the same endpoints the tool is.
async fn s3_client() -> S3 {
    AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings::default())
        .with_credentials_provider(credentials())
        .region(REGION)
        .endpoint(S3_ENDPOINT)
        .build_config()
        .await
        .s3_with_path_style(true)
        .build()
        .await
        .expect("the S3 client should build")
}

async fn dynamodb_client() -> DynamoDb {
    AwsClientBuilder::builder()
        .with_http_settings(&HttpClientSettings::default())
        .with_credentials_provider(credentials())
        .region(REGION)
        .endpoint(DYNAMODB_ENDPOINT)
        .build_config()
        .await
        .dynamodb()
        .build()
        .await
        .expect("the DynamoDB client should build")
}

/// The credentials the compose file starts both services with.
fn credentials() -> aws_sdk_dynamodb::config::Credentials {
    aws_sdk_dynamodb::config::Credentials::new(ACCESS_KEY, SECRET_KEY, None, None, "test")
}

/// Creates the payload bucket unless it is already there, as it is on every run after the first.
async fn create_bucket(s3: &S3) {
    if s3
        .bucket_exists(BUCKET.to_owned())
        .await
        .expect("checking the bucket should succeed")
    {
        return;
    }

    s3.sdk_client()
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("creating the bucket should succeed");
}

/// Creates the three tables this run owns, in the shapes the store keys them on.
async fn create_tables(dynamodb: &DynamoDb, tables: &Tables) {
    create_table(dynamodb, &tables.fragments, true).await;
    create_table(dynamodb, &tables.state, false).await;
    create_table(dynamodb, &tables.metadata, false).await;
}

/// One table keyed on the hash, with the fragments table's association sort key where `associated`
/// asks for it.
async fn create_table(dynamodb: &DynamoDb, name: &Arc<str>, associated: bool) {
    let mut create = dynamodb
        .sdk_client()
        .create_table()
        .table_name(&**name)
        .attribute_definitions(binary_attribute(HASH_ATTRIBUTE))
        .key_schema(key(HASH_ATTRIBUTE, KeyType::Hash))
        .provisioned_throughput(
            ProvisionedThroughput::builder()
                .read_capacity_units(5)
                .write_capacity_units(5)
                .build()
                .expect("the throughput should build"),
        );

    if associated {
        create = create
            .attribute_definitions(binary_attribute(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE))
            .key_schema(key(FRAGMENTS_DYNAMO_SORT_KEY_ATTRIBUTE, KeyType::Range));
    }

    create
        .send()
        .await
        .unwrap_or_else(|error| panic!("creating {name} should succeed: {error:?}"));
}

fn binary_attribute(name: &str) -> AttributeDefinition {
    AttributeDefinition::builder()
        .attribute_name(name)
        .attribute_type(ScalarAttributeType::B)
        .build()
        .expect("the attribute definition should build")
}

fn key(name: &str, key_type: KeyType) -> KeySchemaElement {
    KeySchemaElement::builder()
        .attribute_name(name)
        .key_type(key_type)
        .build()
        .expect("the key schema should build")
}

/// Deletes a table and waits for it to be gone, so what follows reads a store that no longer has
/// it rather than one still taking it down.
async fn drop_table(dynamodb: &DynamoDb, name: &Arc<str>) {
    dynamodb
        .sdk_client()
        .delete_table()
        .table_name(&**name)
        .send()
        .await
        .unwrap_or_else(|error| panic!("deleting {name} should succeed: {error:?}"));

    for _ in 0..POLL_ATTEMPTS {
        if !dynamodb
            .table_exists(name)
            .await
            .expect("checking the table should succeed")
        {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("{name} was still there after the poll gave up");
}

/// One row, or `None` where the table holds none for that hash.
async fn get_row(
    dynamodb: &DynamoDb,
    table: &Arc<str>,
    hash: Hash,
) -> Option<HashMap<String, AttributeValue>> {
    dynamodb
        .get_item(
            table,
            HashMap::from([(HASH_ATTRIBUTE.to_owned(), hash_value(hash))]),
            true,
        )
        .await
        .expect("reading a row should succeed")
        .item
}

/// Every row in a table, following the scan to the end of it.
async fn scan(dynamodb: &DynamoDb, table: &Arc<str>) -> Vec<HashMap<String, AttributeValue>> {
    let mut items = Vec::new();
    let mut start_key = None;

    loop {
        let page = dynamodb
            .scan_page(table, start_key, &ScanConfig::default())
            .await
            .expect("scanning a table should succeed");
        items.extend(page.items);

        match page.last_evaluated_key {
            Some(key) => start_key = Some(key),
            None => return items,
        }
    }
}
