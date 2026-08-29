# lore-aws-migrate

Migrates every fragment in an AWS immutable store onto the fragment state table, the layout Lore has read since v0.8.7.

A fragment used to be described by its own DynamoDB row: flags and sizes in the metadata table, payload in S3. It now travels on the S3 object carrying the payload, and DynamoDB keeps only a state row recording that the hash exists. A store holding objects written before v0.8.7 needs each one rewritten with its metadata attached and a state row published.

A standalone crate, outside the Lore workspace: it reaches `lore-aws` by path, restates the workspace's `quinn-proto` patch, and pins its dependencies in a committed `Cargo.lock`.

## Migrating a production store

1. [Build](#build) it, release, with the `oodle` feature if the store holds Oodle payloads.
2. [Rehearse with `--dry-run`](#run). It writes nothing, so it runs against the live store and sizes the maintenance window.
3. [Take the store out of service](#the-store-must-not-be-serving-traffic) for that window. Not optional.
4. [Run](#run) it.
5. [Verify](#verify). A zero exit status is not on its own a migrated store.
6. [Clean up](#clean-up): reopen traffic, retire the metadata table setting, delete the table, revoke the permissions.

## The store must not be serving traffic

**Put every node that reaches this store into maintenance mode until the run finishes.** After the rehearsal, not before — the rehearsal is what sizes the window.

This is not about load. A conversion reads a fragment's state, then spends a GET, a decompress, a recompress and a PUT before writing. If an obliteration completes on that hash inside that window the payload is uploaded anyway and the tombstone revived to `Stored`: the obliteration is undone, payload back in the bucket, hash readable and deduplicated against again. No total counts it — the store logs `Payload revives a tombstoned hash` at info level, which is what [Verify](#verify) has you grep for.

`LORE_SERVER_MAINTENANCE=1` puts a node in maintenance mode. It then serves `/health_check` over HTTP, answering `200 OK` so a load balancer keeps the node registered rather than cycling the task, and the gRPC environment service, answering `UNAVAILABLE` on every port the node normally exposes so a probe sees a reduced node rather than a refused connection. No storage, replication, or admin service is registered.

On the `contrib/aws` topology, set it on both task definitions. The edge reaches the store only through the primary — durable store `replicated` over QUIC, mutable store `remote` — so quiescing the primary is what stops writes reaching S3 and DynamoDB; quiesce the edge too, or it goes on taking client traffic into its own local store. Check no task is still on the previous revision, with the names the default `name = "lore"` prefix produces:

```sh
aws ecs describe-services --cluster lore-cluster --services lore lore-edge \
  --query 'services[].deployments[].{taskDefinition:taskDefinition,running:runningCount}' \
  --region us-west-2
```

## Prerequisites

- Rust 1.85 or later, for edition 2024. Nothing in the tree uses a nightly feature and no toolchain is pinned.
- A checkout of this repository: the crate depends on `../../lore-aws` and `../../vendor/quinn-proto` by path.
- Network access for the first build, or a warm cargo cache and `--offline`.
- AWS credentials from the standard chain.

## Build

```sh
cd contrib/aws-migrate-0.9.0
cargo build --release
```

The binary lands at `target/release/lore-aws-migrate`. Build it release: a migration reads, decompresses and rewrites every payload in the store.

Oodle payloads need the Oodle libraries, which are not in this repository. Without them every Oodle fragment is reported `unreadable` and left behind:

```sh
OODLE_LIB_DIR=/path/to/oodle cargo build --release --features oodle
```

## Run

A full migration is every segment of the metadata table, which is the default:

```sh
./target/release/lore-aws-migrate \
  --s3-bucket lore-fragments-abc123 \
  --fragments-table lore-fragments \
  --fragment-state-table lore-fragment-state \
  --fragment-metadata-table lore-metadata \
  --region us-west-2
```

Where a deployment never had a separate table — state rows and legacy metadata rows sharing one, told apart by shape — pass that name to both `--fragment-state-table` and `--fragment-metadata-table`.

The migration reads the S3 object, the metadata table behind it, and the state table; it writes the object and a state row. Associations are neither read nor written, so `--fragments-table` only names the store completely and is checked for existence.

The four store arguments can come from the environment instead: `LORE_MIGRATE_S3_BUCKET`, `LORE_MIGRATE_FRAGMENTS_TABLE`, `LORE_MIGRATE_FRAGMENT_STATE_TABLE`, `LORE_MIGRATE_FRAGMENT_METADATA_TABLE`, and `AWS_REGION` for `--region`.

Rehearse first. `--dry-run` analyses every fragment and reports what each outcome would be, writing nothing, so unlike a real run it is safe against a live store:

```sh
./target/release/lore-aws-migrate ... --dry-run
```

Read what it costs as a floor: on an unmigrated store it does everything a real run does bar the upload and the state write. Run it at the `--total-segments` and `--consumers` the real run will use, or the number says nothing about it.

`--total-segments` is how many parallel scans the table is divided into, `--consumers` how many fragments a segment converts at once. To spread one migration over several machines, give each the same `--total-segments` and one `--segment`:

```sh
# machine 0 of 4
./target/release/lore-aws-migrate ... --total-segments 4 --segment 0
```

Exit status is 0 only when every segment this invocation covered completed. Progress totals are logged every 30 seconds (`--progress-interval-secs 0` to silence them), and `RUST_LOG` sets the log level.

Four more knobs matter on a large or unhappy store; `--help` lists the rest:

| Argument | Default | What it is for |
|----------|---------|----------------|
| `--timeout-millis` | 30000 | One S3 or DynamoDB operation. A payload rewrite is one operation, so this bounds the largest fragment the migration can move |
| `--max-retries` | 5 | Retries after the first attempt, at a scan page, at a fragment conversion, and at the payload write inside it. The last two nest, so a failing write is tried (1 + max)² times |
| `--retry-base-delay-ms` | 200 | Multiplied by the attempt number, capped at five seconds. Raise it when DynamoDB is throttling |
| `--scan-limit` | unset | Items per scan page, to hold discovery back from outrunning the conversions |

### Interrupting and resuming

Repeating the same command resumes a run: a fragment with a state row **and** an object describing itself is skipped. `Ctrl-C` sets a flag read at the top of each fragment, so whatever is in flight finishes. A resumed run needs the servers held out of service exactly as the first did — the hazard above is per write, not per run.

Resuming is not free. Discovery keeps no checkpoint, so a resumed run re-reads every row and spends a state lookup and a `HeadObject` on each fragment it already migrated before reaching new work.

A fragment whose read or write keeps failing past its retries stops the whole run, every segment. A scan page that keeps failing is narrower: it ends its own segment's discovery and the run reports incomplete, while other segments finish their scans. A payload no codec can recover stops nothing — it is counted `unreadable` and passed over. Under `--dry-run` nothing stops the run at all, so one pass reports everything wrong with the store.

## What the totals mean

| Total | Meaning |
|-------|---------|
| `scanned` / `metadata_rows` | Rows read from the metadata table, and those that yielded a hash |
| `maintained` | Codec declared correctly and not Oodle: payload re-uploaded unchanged to attach its metadata |
| `recompressed_oodle` | Oodle payload recompressed to Zstd |
| `recompressed_mismatch` | Declared codec disagreed with the stored bytes; recompressed to Zstd |
| `stored_uncompressed` | Recompression did not pay for itself, so the payload is stored uncompressed |
| `payloads_deduced` | Codec had to be found by probing rather than trusted |
| `already_migrated` | State row present **and** the object already describes itself, so nothing to do |
| `state_with_no_head` | A state row was found but the object does not describe itself, so the fragment is converted anyway. Where the state and metadata tables are one this is every fragment, because the legacy row is what the state read finds |
| `obliterated` | The row says the payload was obliterated, so it is passed over and gets no state row. **Expected — see below** |
| `oversized` | The object's length, or the size its row claims, is past the threshold, so it is refused. An object whose length gives it away is never read: the check is on the `Content-Length` arriving ahead of the body |
| `unreadable` | No codec reproduced the hash, so the payload cannot be recovered and gets no state row |
| `errored` | Conversion failed after its retries. **A row whose object is gone lands here**, which is what an obliteration that also deleted the payload leaves behind |

### `obliterated` is an outcome, not a failure

An obliterated fragment gets no state row on purpose: its payload is destroyed and its associations are gone, so nothing can retrieve it and a state row would only put the hash back into the layout the store now reads. A nonzero count on a store that has obliterated anything is expected.

Only rows whose payload is still in the bucket reach this count — an obliteration interrupted before it deleted the object. One that got that far leaves a row the migration cannot load, counted under `errored`.

### Success is per segment, not per fragment

Exit status 0 means every segment reached the end of its scan, not that every fragment is migrated. Two outcomes leave a fragment unmigrated and still exit 0:

- `unreadable` — no codec reproduced the hash. Built without the `oodle` feature against a store holding Oodle payloads, every one of them lands here.
- `oversized` — a size past the threshold, so the fragment is refused rather than converted.

A migration is complete when both are zero. Until they are, the deployment still needs the metadata table configured (`dynamodb_fragment_metadata_table` under `[plugins.aws.immutable_store]`, or `fragment_metadata_table` in `contrib/aws`), because the fragments left behind are readable only through it.

## Verify

Verify while the nodes are still out of service.

### 1. Read the totals

From the `final` line the run logs:

- `unreadable`, `oversized` and `errored` must be zero. Each is a fragment left with no state row, readable only through the metadata table.
- `scanned` and `metadata_rows` should agree. A gap is rows that yielded no hash, which the run logs one by one.
- `obliterated` may be anything. Those fragments are meant to have no state row.

Then grep the log for `Payload revives a tombstoned hash`. No total counts it, and it is worse than an incomplete migration: the run uploaded a payload for a hash marked obliterated and set that hash back to `Stored`. It means an obliteration completed between a conversion reading the state and publishing it — the race quiescing the store prevents — so it should not appear at all. If it does, work out which hashes before reopening traffic.

### 2. Run it again as a dry run

The verify pass. It re-reads every row and reports what each fragment is now, writing nothing, so it is safe against a store already serving traffic again:

```sh
./target/release/lore-aws-migrate ... --dry-run
```

`already_migrated` and `obliterated` should add up to `metadata_rows`, with zero for everything else. `already_migrated` means a state row *and* an object carrying its own metadata — the pair the new layout reads through, which is why this pass establishes the migration where counting the state table would not. Anything under `maintained`, `recompressed_*` or `stored_uncompressed` is a fragment the run did not convert; `unreadable` or `oversized` is one it cannot.

It re-scans the table, heads every object, and downloads the payload of anything not migrated, so size a window for it as for a real run.

### 3. Spot-check an object

A migrated object carries its whole fragment in one metadata header:

```sh
aws s3api head-object --bucket lore-fragments-abc123 \
  --key <the 64 hex characters of the hash> --region us-west-2 --query Metadata
```

```json
{ "lore-fragment": "8:4096:16384" }
```

Flags in hex, then the payload and content sizes in decimal. An object with no `lore-fragment` key was not migrated, whatever the state table says about its hash.

## Clean up

Only once [Verify](#verify) reports a fully migrated store.

**1. Take the nodes back out of maintenance mode.** Leave the metadata table configured for now: a migrated store never falls back to it, and it keeps the store readable if the verification missed something. Not quite free — while the table is named a batch query also reads the state table, which it skips once the setting is gone.

**2. Stop naming the metadata table.** Remove `dynamodb_fragment_metadata_table` from `[plugins.aws.immutable_store]` and roll it out as an ordinary deployment. On the `contrib/aws` topology that is the `fragment_metadata_table` variable, which sets `LORE__PLUGINS__AWS__IMMUTABLE_STORE__DYNAMODB_FRAGMENT_METADATA_TABLE` on both task definitions; a deployment predating the rename may spell it `dynamodb_metadata_table`, still accepted as an alias. Unsetting it declares that no object without its own metadata exists, so such an object is reported as damaged rather than described from a row. This is the step to roll back if reads start failing; nothing is destroyed yet.

**3. Delete the metadata table, where it is a table of its own.** Once step 2 has run long enough to trust:

```sh
aws dynamodb delete-table --table-name lore-metadata --region us-west-2
```

Where `--fragment-state-table` and `--fragment-metadata-table` named the same table, **there is nothing to delete and deleting it would destroy the store**. Both row shapes share it, and a fragment migrated there kept the row it already had — the state write finds one present and leaves it — so the legacy-shaped rows *are* the state table. Step 2 is the whole of the cleanup there.

**4. Give back the permissions.** Those below are broader than the server's own role; `dynamodb:Scan` in particular is granted to nothing else.

**5. Remove the build.** The tool installs nothing and leaves no state; deleting `contrib/aws-migrate-0.9.0/target` reclaims several gigabytes.

## Permissions

The credentials the tool runs under need more than the server's own task role:

- `dynamodb:Scan` on the metadata table — the server never scans, so its policy does not grant this
- `dynamodb:GetItem` on the metadata and state tables, `dynamodb:PutItem` on the state table
- `dynamodb:DescribeTable` on all three tables and `s3:ListBucket` on the bucket, which is what `DescribeTable` and `HeadBucket` are authorized under. Both are issued at startup, so a wrong name fails the run before it touches anything
- `s3:GetObject` on the fragment bucket, covering the `HeadObject` the skip check spends, and `s3:PutObject` for the rewrite

Point `--s3-endpoint-url` and `--dynamodb-endpoint-url` at a local stack to rehearse against one, adding `--s3-force-path-style` where the endpoint needs it.

## Appendix: testing against a local stack

Not part of migrating a store. This is how a change to the tool, or to the migrator in `lore-aws` behind it, gets checked.

`tests/migrate_local_stack.rs` seeds just over a thousand fragments the way the old layout held them, runs this binary, interrupts it with `SIGINT` partway, runs it again, and checks the state rows, the object headers, the verification pass totals, and a read back through the store with the metadata table dropped. Every fragment takes one of five legacy shapes in turn, covering each recovery path. Among them are a megabyte of content held as sixteen pieces and a list naming them, which the test reassembles afterwards, and one obliterated fragment, which must come through with no state row and read as absent.

From the root of the repository:

```sh
docker compose --file lore-integration-tests/compose.yaml up --detach minio dynamodb
cd contrib/aws-migrate-0.9.0
cargo test --features integration_tests
```

The feature is off by default, so a bare `cargo test` runs only the argument tests and needs no services. The test creates the bucket on first use and tables named after the run, and deletes what it created when it passes; a failing run leaves its tables behind, named `lore-migrate-test-*`.
