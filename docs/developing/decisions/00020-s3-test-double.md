<!--
SPDX-FileCopyrightText: 2026 Epic Games, Inc.
SPDX-License-Identifier: MIT
-->

---
status: accepted
date: 2026-09-17
---

# ADR-00020: RustFS as the local S3 stand-in, replacing MinIO

## Context and Problem Statement

The AWS store suite in `lore-integration-tests` and the dev compose stack in
`lore-server/config/dev-local.toml` both talk to a local S3 implementation rather than to AWS. That
implementation was MinIO, and MinIO stopped being something a public repository can depend on:
Docker Hub no longer serves `minio/minio`, which broke the integration job on every PR until
`quay.io/minio/minio` was substituted as a stopgap. That was a redirect to a second registry for an
edition that no longer ships features, not a fix — the community edition went to maintenance mode
and its repository has since been archived.

Lore is public. Anyone who clones it must be able to bring the backing services up with
`docker compose up` and no registry credentials, so the stand-in has to be an image that a stranger
can pull. We need a replacement, not another registry.

## Decision Drivers

- The image must be pullable unauthenticated by outside contributors and by CI. This is the driver
  that ended MinIO; a replacement that could go the same way is no replacement.
- It must serve every S3 operation `lore-aws/src/s3.rs` issues. `ListObjectVersions` and
  `DeleteObject` with a `versionId` are the sharp end: `AwsImmutableStore::delete_payload` lists an
  object's versions and deletes each one, and the conformance battery run by
  `aws_immutable_store_satisfies_the_conformance_contract` calls `obliterate` against the live
  endpoint. A stand-in without version listing does not merely leave a gap — it fails that test.
- One container, credentials from environment variables, no cluster bootstrap step. The compose file
  is a fixture, and `.github/workflows/pr-validate.yml` waits on an HTTP readiness probe — anything
  needing a provisioning sidecar has to be reproduced in both places and in the migration tool's
  instructions under `contrib/`.
- The store should be wiped on restart, the way DynamoDB Local's `-inMemory` wipes the tables.
- The license should not reintroduce the concern being solved. Weak on its own — the stand-in is a
  separate process Lore neither links nor redistributes — but a change that leaves the licensing
  question exactly where it was is not worth making.

## Considered Options

- Keep MinIO, pulled from Quay
- Garage
- RustFS
- SeaweedFS

## Decision Outcome

Chosen option: **RustFS**, pinned to `rustfs/rustfs:1.0.0` by digest, because it meets the version-listing
driver — which eliminates Garage outright — and is the closest thing to a drop-in among the two
that do: one container, an access key and secret from the environment, and a `/health` endpoint for
the CI wait loop, so the compose service, the readiness probe and the `contrib/` instructions all
keep their existing shape.

Its S3 behavior was verified against the operations the store actually issues before this was
adopted, not taken from its compatibility table. The result that decided it: on an *unversioned*
bucket — which is what the test harness creates — `ListObjectVersions` returns the current object
with `VersionId: "null"` and `IsLatest: true`, and `DeleteObject` accepts `--version-id null`, which
is what real S3 does and what `delete_payload` is written against. Enabling versioning on the bucket
then yields distinct version ids, and deleting one leaves the other intact.

The compose service mounts `/data` and `/logs` as `tmpfs` with a widened mode. That covers two
things at once: the store becomes ephemeral, and the container's non-root uid (10001) gets somewhere
writable without a bind mount that has to be chowned on every developer's machine.

### Consequences

- Good, because the integration job no longer depends on an edition in maintenance mode or on a
  second registry standing in for a repository that has gone away.
- Good, because the conformance battery keeps its full coverage, obliterate included. No check had
  to be relaxed or capability turned off to make the stand-in work, so nothing about the AWS store's
  contract is now less tested than it was under MinIO.
- Good, because the bucket is empty on every restart, which removes the manual cleanup step the
  integration README used to ask for.
- Neutral, because the 1.0.0 tag is newer than the code beneath it; see the option analysis below
  for what that does and does not imply.
- Neutral, because the console moves from `http://localhost:9001/` to
  `http://localhost:9001/rustfs/console/`, and the readiness probe from
  `/minio/health/ready` to `/health`. RustFS also answers the MinIO probe path, but the native one is
  used so nothing reads as a leftover.
- Neutral, because production is unaffected either way: real deployments talk to S3 itself, and this
  decision only governs what runs in compose and in CI.

## Pros and Cons of the Options

### Keep MinIO, pulled from Quay

- Good, because it needs no change and its S3 fidelity is the benchmark the others are measured
  against.
- Bad, because it does not address the reason this came up. Docker Hub already dropped the image
  once; the community edition is archived, so the next break has no third registry to move to.
- Bad, because pinning a public project to an archived dependency asks every future contributor to
  discover the situation themselves.

### Garage

- Good, because it is a mature, genuinely small single-binary store, and the project's
  self-hosting focus matches how the fixture is used.
- Bad, because it does not implement bucket versioning at all. `ListObjectVersions` and
  `PutBucketVersioning` are absent and `GetBucketVersioning` is documented as a stub that always
  answers "versioning not enabled", so `delete_payload` cannot run against it and the conformance
  battery fails on the obliterate check. Deleting that check to accommodate the stand-in would mean
  giving up integration coverage of a deletion path, which is not a trade worth making.
- Bad, because a client cannot talk to a fresh Garage node until a layout is assigned and a key is
  created or imported. That is an init container in compose plus the equivalent in the `contrib/`
  migration instructions, for a fixture whose whole job is to come up unattended.
- Neutral, because AGPLv3 is the same license class as what it would replace. Irrelevant to Lore
  mechanically, but it means the licensing question is not improved by the move.

### RustFS (chosen)

- Good, because versioning, `ListObjectVersions` and versioned deletes all work, verified directly
  against the operations `lore-aws` issues.
- Good, because it is a drop-in in the shape the fixture already has: ports 9000/9001, path-style
  addressing by default, a root access key and secret from the environment, no bootstrap.
- Good, because Apache-2.0 settles the licensing question rather than relocating it.
- Good, because it is not a one-person project: 169 contributors, 33k stars and 1.5k forks at time
  of writing. Attention proves nothing about S3 correctness, but a project this widely watched is
  unlikely to go unmaintained mid-release — which is the failure mode that ended MinIO's usefulness
  here.
- Neutral, because the 1.0.0 tag is newer than the code under it. GA landed on 2026-09-16, the day
  before this was adopted, but the repository dates to November 2023 and shipped 75 alphas, 15 betas
  (April to July 2026) and 7 release candidates (August to September 2026) on the way there. The
  fresh thing is the label, not the implementation — the risk to weigh is a late regression in a
  maturing project, not unexercised code.
- Neutral, because the project's own channels disagree about production readiness — the GA
  announcement declares it ready while the Docker Hub page still reads "RustFS is under rapid
  development. Do NOT use in production environments!". Stale copy on one of them, most likely.
  Neither claim is load-bearing here, since nothing in production depends on this container, but it
  does mean the vendor's own statements are not evidence either way.
- Neutral, because the post-GA cadence is quick — two 1.0.1 preview builds inside nineteen hours of
  1.0.0 shipping. Ordinary for a fresh GA, and the reason the fixture tracks a digest rather than a
  moving tag.
- Neutral, because its MinIO on-disk compatibility is a preview feature. It does not matter for this
  use — there is no MinIO data to migrate, buckets are created by the harness on demand.

### SeaweedFS

- Good, because it is Apache-2.0 with a far longer operational history — the repository dates to
  2014 against RustFS's 2023 — supports versioning and `ListObjectVersions`, and runs the ceph
  `s3-tests` suite in CI. Community size is a wash (35k stars to RustFS's 33k); the difference is
  years in service, not attention.
- Good, because several other projects have already made this exact swap away from MinIO.
- Bad, because its `ListObjectVersions` has needed repeated recent fixes, including version listings
  that include directory placeholder keys — precisely the operation this decision hinges on.
- Neutral, because the deployment shape is a little further from the current fixture (a `weed server
  -s3` invocation rather than an S3 server as such), which is a small cost, not a blocking one.

This is the closest runner-up, and the two are closer than the choice between them suggests. If
RustFS disappoints in practice — the suite going intermittently red against a pinned digest, or a
release that changes S3 semantics — SeaweedFS is where to go next, and this ADR should be superseded
rather than amended.

## When to Revisit

Digest bumps ride the normal review path, and the integration suite is what qualifies them: it is
the only thing standing between a RustFS behavior change and a false CI signal. Take final 1.0.x
releases, not the preview builds cut from `main`. Nothing here needs watching on a schedule — the
suite will say if something breaks.

## More Information

- `lore-integration-tests/compose.yaml` and `lore-integration-tests/README.md`.
- The readiness probe and service list in `.github/workflows/pr-validate.yml`.
- `lore-server/config/dev-local.toml` — the compose stack's S3 endpoint.
- `contrib/aws-migrate-0.9.0/README.md` names the compose service directly, so it moved with the
  rename from `minio` to `rustfs`.
- `AwsImmutableStore::delete_payload` in `lore-aws/src/store/immutable_store.rs` is the code that
  makes version listing a requirement rather than a nice-to-have, and
  `an_obliterated_address_matches_nothing` in `lore-storage/src/conformance.rs` is what drives it
  from the integration suite.
