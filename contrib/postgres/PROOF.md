# PostgreSQL + R2 + Consul implementation proof

**Result: PASS**

Run date: 2026-08-16. Primary host: `enfis1`. Run root:
`/var/tmp/lore-pg-r2-consul-20260816T141340Z`.

This proof uses PostgreSQL as Lore's semantic fragment catalog, Cloudflare R2 as the durable
unversioned payload store, and Consul as active topology discovery for a composite Lore store.
All resources are uniquely named, test-owned, and bound to loopback where applicable. No resource
was pruned during or after the proof.

## Acceptance results

| Criterion | Result | Evidence |
| --- | --- | --- |
| DynamoDB is behind a semantic catalog seam without regressions | **PASS** | `cargo test -p lore-aws --lib`: 72 passed, 0 failed, 2 ignored. |
| PostgreSQL satisfies the catalog state-machine contract | **PASS** | Shared contract passed against an isolated live PostgreSQL schema. |
| Association and obliteration races serialize correctly | **PASS** | 32 association attempts were rejected after the marker; 32 simultaneous starts shared one deterministic lease. |
| R2 uses explicit unversioned exact-key deletion | **PASS** | Remote obliteration finished in 0.52 s; 9 terminal catalog hashes were all HTTP 404 in R2; no `ListObjectVersions`/HTTP 501 appeared. |
| Interrupted obliteration is resumable | **PASS** | Unit coverage verifies an existing `PayloadObliterating` marker resumes; the previous isolated R2 outage proof remains green. |
| Consul is active topology, not catalog storage | **PASS** | The corrected composite server made 328 recorded health-service queries; final server log has 0 error lines. |
| End-to-end data integrity | **PASS** | Create, stage, commit, push, and cold clone passed. The 1 MiB payload matched SHA-256 `360d9dee65c1cec3353f3309ee6bfb45eef65313b0e98c39f0f32e5416212028`. |
| PostgreSQL and R2 remain consistent | **PASS** | Before obliteration, all 19 PostgreSQL hashes had successful exact-key R2 HEADs and total payload bytes matched (`525598`). |
| Existing test data was untouched | **PASS** | The 9 newly obliterated hashes had zero overlap with all 10,349 hashes in the older isolated catalog. |
| Credentials are not logged | **PASS** | PostgreSQL config Debug output is redacted; final plugin/Consul unit tests verify secret redaction. |

## Live resources

- PostgreSQL container: `lore-pg-catalog-20260816t153000z`, loopback port `15439`.
- PostgreSQL schema: `lore_live_composite_20260816t141340z`.
- Consul container: `lore-consul-20260816t141340z`, loopback port `18500`.
- Server ports: `44237` and `44239`, loopback only.
- R2 bucket used: the earlier test-owned `lore-r2-proof-20260816t095204z` with short-lived,
  bucket-scoped credentials. Only newly generated, non-overlapping content hashes were removed.
- R2 bucket `lore-pg-r2-consul-20260816t141340z` was also created through `cf` but remained empty
  because the available short-lived S3 credential was scoped to the earlier test bucket.

The run root, database schemas, bucket, containers, server process, source tree, and evidence remain
in place to comply with the no-prune requirement.

## Reproducible tests

Run from the repository root:

```sh
cargo +nightly fmt --all -- --check
cargo test -p lore-aws --lib
cargo test -p lore-server plugins::hashicorp::tests --lib
git diff --check

cd contrib/postgres
cargo +nightly fmt -p lore-postgres -p lore-postgres-server -- --check
cargo test -p lore-postgres-server --lib
cargo clippy -p lore-postgres-server --lib -- -D warnings
LORE_POSTGRES_TEST_URL='host=127.0.0.1 port=15439 user=lore_test password=<test-password> dbname=lore_test sslmode=disable' \
  cargo test -p lore-postgres --features integration-tests --test catalog_contract -- --nocapture
```

The ARM64 build on `enfis1` used an isolated source tree and Cargo cache:

```sh
docker run \
  --name lore-build4-pg-r2-consul-20260816t141340z \
  -e CARGO_HOME=/cargo-home \
  -v /var/tmp/lore-pg-r2-consul-20260816T141340Z/src:/workspace \
  -v /var/tmp/lore-pg-r2-consul-20260816T141340Z/cargo-home:/cargo-home \
  -w /workspace/contrib/postgres \
  rust:1.94.1-bookworm \
  cargo build --release -p lore-postgres-server
```

Rust 1.90 was also tried and correctly rejected by Cargo because the resolved AWS SDK requires
Rust 1.94.1. All build-container logs are retained. The successful clean release build took
1 minute 56 seconds; the exact final incremental ARM64 build took 6.92 seconds wall-clock
(5.59 seconds reported by Cargo).

## Live checks and timings

The final composite configuration is equivalent to
[`config-r2-consul.toml`](config-r2-consul.toml), with isolated paths, schema, bucket, and ports.
It includes `immutable_store.composite.replica_factory`; without that section Consul still polls,
but Lore cannot materialize discovered peers. The proof deliberately caught this configuration
error, corrected it, and verified the replacement server log has zero errors.

Key timings:

- corrected composite server startup: approximately 0.55 s;
- direct repository create: 1.10 s;
- stage: 0.02 s;
- commit: 1.71 s;
- direct R2 push: 1.89 s;
- cold direct clone: 1.66 s;
- composite push: 0.67 s;
- composite cached clone: 0.08 s;
- remote recursive obliteration: 0.52 s, first attempt successful.

Catalog and object checks:

```text
pre-obliteration metadata=19 associations=19 payload_bytes=525598
pre-obliteration exact R2 HEAD success=19/19
post-obliteration terminal rows=9 remaining associations=10
post-obliteration terminal R2 objects absent=9/9
migration checksum=31cdf92ab669b9568bd81c600e676cc358e7a35110500e293c08418bbbc36c49
```

## Checksums and logs

```text
ARM64 loreserver-postgres:
ab0045965f0accb4bd4e69bb91c8ec35d5b5cd98132773630a9508b29303256b

Final live configuration:
00c3551f39614f257931ca0e774c01e0a8eed23cec50cd05c1c52d52112cc1b2

56-file exact-final evidence manifest:
d9554fe63b028b907d39162e67728e5b697208d9c32588d6f324cf21437be779
```

The exact-final binary remained running after a read-only repository-list check returned
`lore-pg-r2-consul-composite-proof`. Its runtime error evidence is empty. The first exact-final
restart intentionally remains in `exact-final-server-missing-env.log`: it omitted the retained R2
credential environment and exited before serving or performing a data operation. The corrected
restart used the same protected, bucket-scoped test credential file as the earlier proof.

All live logs, timings, catalog snapshots, hash lists, R2 check summaries, build metadata, and the
SHA-256 manifest are retained under:

```text
/var/tmp/lore-pg-r2-consul-20260816T141340Z/evidence
```
