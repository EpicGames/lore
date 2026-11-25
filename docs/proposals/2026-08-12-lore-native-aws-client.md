---
lep: 2026-08-12-lore-native-aws-client
title: A lore-native AWS client for DynamoDB and S3
authors:
  - mjansson
status: Draft
created: 2026-08-12
updated: 2026-11-25
discussion: CROWD CR 536
---

# A lore-native AWS client for DynamoDB and S3

## Summary

Lore's AWS stores reach DynamoDB and S3 through the official AWS SDK for Rust. The SDK decodes every
DynamoDB row into hash maps with a separate heap allocation for each attribute, so reading one batch
of rows costs on the order of a thousand small allocations, and it brings 22 `aws-*` crates, 48
packages in all, into the build for the 15 AWS calls Lore makes. This proposal replaces the SDK with
a client of Lore's own, built on the HTTP and TLS libraries Lore already uses. The client implements
only those 15 calls, decodes a row without an allocation per attribute, and decides for itself how
long a call may take and which failures to send again. The stores built on it behave as before
except where this proposal changes them: which failures they treat as the service being busy, how
often they try again, how many locks one request may name, how obliteration lists a payload's
versions, and the configuration and credential changes Compatibility lists. An operator can also cap how many requests each client has in flight at
once, for a storage service that cannot keep up with the server's concurrency.

## Motivation

### The SDK's allocation profile is hostile to a thread-caching allocator under our concurrency

Lore runs on rpmalloc: `lore-base/src/allocator/mod.rs` installs it as the process allocator for
every build unless `LORE_ALLOCATOR=system` opts out. rpmalloc is a thread-caching allocator — each
thread holds its own spans and free lists per size class, and freed memory returns to a thread or
global cache rather than to the OS. That design buys allocation speed in exchange for two
sensitivities: the cached footprint is set by the *peak* number of live small blocks, not the mean,
and a block freed by a thread other than the one that allocated it costs more than a thread-local
push.

The SDK's item model exercises both. On main, reading rows arrives at a typed entry through **two
materializations of every item**:

1. `aws-smithy-json` parses the response body into a `HashMap<String, AttributeValue>`, where each
   attribute name is a heap `String` and each binary attribute a heap `Vec<u8>`.
2. `serde_dynamo` rebuilds that map as its own item type, a second hash map that takes over the
   names and blobs (`serde_dynamo` 4.3.0, `src/macros/aws_sdk.rs`), before deserializing into the
   entry (`lore-aws/src/store/immutable_store.rs`).

Some call sites in each store and in the metadata migrator also copy every name and blob first,
reading the item through a borrowing accessor where `serde_dynamo` needs an owned map
(`lore-aws/src/store/` on main).

Count it for one request. A `BatchGetItem` of 100 fragment-state keys — two attributes each, a
32-byte hash and a small number — costs on the order of a thousand allocations, nearly all of them
in the 16-to-48-byte size classes, plus 200 hash tables whose capacity rounding puts them in another
class again. The store sends those batches concurrently (`lore-aws/src/dynamodb.rs`), the response
buffers are allocated on a net runtime thread, where the connection lives, and the items are
consumed and freed on a core thread, and the pattern repeats for every store operation on every
connection the server is serving.

That is the shape a thread-caching allocator handles worst: a very high rate of tiny, short-lived
allocations spread across many size classes, some freed on a different thread than allocated them,
at a concurrency whose peak sets the resident footprint. The consequence is not slow allocation — it
is allocated-and-cached memory that a throughput burst raises and nothing lowers, spread over
per-thread span caches that no single measurement attributes to its cause.

None of this is inherent to the protocol. A DynamoDB response is one contiguous JSON document, Lore's
rows carry two to seven attributes with names that are compile-time constants, and the payloads are
already reference-counted buffers by the time a store touches them.

### The dependency tree is disproportionate

`Cargo.lock` on main holds 22 `aws-*` crates for these 15 calls: `aws-config`,
`aws-credential-types`, `aws-runtime`, `aws-sdk-dynamodb`, `aws-sdk-s3`, `aws-sdk-sts`, `aws-sigv4`,
`aws-types`, and fourteen `aws-smithy-*`. `aws-sdk-s3` 1.155.0 alone is about 240,000 lines modelling
112 operations, of which Lore calls six. `aws-smithy-eventstream` serves streaming event bodies,
which no Lore call reaches, and `base64-simd`, `outref`, `vsimd` and `regex-lite` enter the workspace
through these crates and nothing else. Beyond the build, each crate is a supply-chain surface, a
`cargo deny` entry and a notices entry.

### Nothing bounds the requests in flight

Nothing in the stores bounds how many requests a service client has in flight, and so how many
connections it has open. The SDK's connector, as `lore-aws/src/clients.rs` on main builds it, sets
`TCP_NODELAY` as `nodelay` says, and no bound; the idle timeout it is configured with does not reach the connector it
builds (`aws-smithy-http-client` 1.5.1, `src/client.rs`). `aws-smithy-http-client` 1.5.1 offers an opt-in
per-host bound (`src/client/pool/builder.rs` in that crate), which counts connecting, idle and active
connections alike, and `reqwest` bounds only idle connections
(<https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html#method.pool_max_idle_per_host>).
A storage service that cannot keep up with the concurrency the server hands it, as the single-node
service in Appendix A could not, needs a ceiling an operator can set.

### A lock request is atomic only in parts

The AWS lock store takes and releases locks through `TransactWriteItems`, which applies at most 100
items, all or none, and refuses a transaction that names one item twice
(<https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_TransactWriteItems.html>). Today
the lock store splits a longer request into transactions of 100 and sends them concurrently
(`lore-aws/src/dynamodb.rs`), so a lock request over 100 resources is atomic per transaction rather
than per request: a request the service partly sheds leaves some resources locked and others not,
and its caller is told only that the whole of it failed. Lore's own clients already send at most 100
resources at a time (`lore-revision/src/lock/util.rs`), but nothing tells them the limit, and nothing
holds any other caller of the lock service to it.

### Supporting problems

**The SDK's error model is opaque where the stores need it sharp.** Every store maps AWS failures
onto back-off-and-retry or a failure to surface, and getting that mapping right has meant matching
on strings the SDK gives no typed access to: `lore-aws/src/store/lock_store.rs` on main matches the
`ThrottlingException` code on an unmodelled error, because the SDK has no variant for a throttled
transaction. The SDK also leaves the unprocessed keys of a successful `BatchGetItem` to its caller,
so Lore already resends them itself (`lore-aws/src/dynamodb.rs`). Lore already writes the error
model; it writes it on top of someone else's.

**The SDK's runtime behaviour needs a shim on main.** `lore-aws/src/net_http_client.rs` on main
exists because the SDK's default connection pool spawns each connection's driver onto whatever
runtime is current when a request is first polled — for Lore a core runtime thread, where a pooled
connection driver must not live — and its doc comment explains why wrapping the connector is not
enough. Alternatives Considered weighs the SDK's newer pool, which would make the shim unnecessary.

## Goals / Non-Goals

### Goals

- Make reading and writing rows cost no allocation per attribute: no hash map and no second
  materialization per item, no heap string per attribute name, and no heap buffer per binary value
  Lore stores.
- Depend on no `aws-*` crate anywhere in the workspace, including test-only dependencies.
- Serve every AWS call Lore makes today, byte-compatibly with the data already in production tables
  and buckets.
- Decide in this repository how long an operation may take, which failures are sent again and how
  they back off, configured from the stores' existing settings, and give an operator a cap on the
  requests each client has in flight.
- Make every lock request all or nothing: refuse one the lock store cannot apply as a single atomic
  operation, and tell clients how many resources a request may name.
- Keep the stores' behaviour, except where The operations, Retry, Errors and Lock batches below, and
  Compatibility, say it changes.

### Non-Goals

- A general-purpose AWS SDK. This client implements 15 operations and offers no others.
- Credential sources Lore does not deploy on: SSO, `credential_process`, `source_profile` role
  chaining, `mfa_serial`, and SigV4a (multi-region signing). A developer who needs a profile's full
  chain resolves it outside the process, as Credentials describes.
- S3 multipart upload, server-side encryption headers, and presigned URLs. Lore's largest object is
  one fragment payload, at most 256 KiB
  ([ADR 00010](../developing/decisions/00010-fragment-max-size.md)), so a single `PutObject` always
  suffices.

## Proposed Design

Where each Goal is answered:

| Goal | Answered by |
| --- | --- |
| No allocation per attribute | The item model; Signing |
| No `aws-*` crate | Proposed Design as a whole, which replaces everything the SDK did for Lore; Verification for `serde_dynamo`, kept for tests without the feature that brings the SDK |
| Every call, byte-compatibly with production data | The operations; The item model; Verification |
| Time budget, resends and backoff decided here, and a cap on requests in flight | Transport, pooling and timeouts; Retry; Errors |
| Every lock request all or nothing | Lock batches |
| Keep the stores' behaviour | The operations; Retry, Errors and Lock batches for where it changes |

### The operations, and what each one needs from the wire

This is the whole surface.

| Service | Operation | What Lore uses it for | What the response must yield |
| --- | --- | --- | --- |
| DynamoDB | `DescribeTable` | checking at startup that a table exists | success, or `ResourceNotFoundException` |
| DynamoDB | `GetItem` | reading one fragment's state or metadata, and mutable store reads | the item, or none |
| DynamoDB | `BatchGetItem` | fragment associations and states in bulk, and lock status | `Responses`, `UnprocessedKeys` |
| DynamoDB | `PutItem` | fragment associations, and mutable store writes | nothing |
| DynamoDB | `PutItem` with a condition | publishing fragment state and marks, and the mutable store's compare-and-swap | on failure, `ConditionalCheckFailedException` with the losing item |
| DynamoDB | `DeleteItem` | clearing fragment state and associations | nothing |
| DynamoDB | `DeleteItem` with a condition | the mutable store's compare-and-swap to zero | on failure, `ConditionalCheckFailedException` |
| DynamoDB | `Query` | listing fragments, locks and mutable store entries | `Items`, `Count`, `ScannedCount`, `LastEvaluatedKey` |
| DynamoDB | `Scan` | the fragment metadata migration | `Items`, `LastEvaluatedKey` |
| DynamoDB | `TransactWriteItems` | taking and releasing locks | on failure, `TransactionCanceledException` with each item's reason |
| S3 | `HeadBucket` | checking at startup that a bucket exists | success, or 404 |
| S3 | `HeadObject` | reading a payload's metadata | `x-amz-meta-*`, or `NotFound` |
| S3 | `GetObject` | reading a fragment payload | `x-amz-meta-*`, `Content-Length`, the body as a stream, or `NoSuchKey` |
| S3 | `PutObject` | writing a fragment payload | nothing |
| S3 | `DeleteObject` | obliterating a payload version | nothing |
| S3 | `ListObjectVersions` | finding a payload's versions to obliterate | `Version`, `DeleteMarker`, `IsTruncated`, the next markers |
| STS | `AssumeRoleWithWebIdentity` | web-identity credentials | `AccessKeyId`, `SecretAccessKey`, `SessionToken`, `Expiration` |

Obliteration reads only the versions of a key, never its delete markers, so a key that once took a
delete without a version keeps its marker. This proposal keeps that behaviour: the client reports
both, and the store ignores the markers as it does today. Whether obliteration should delete them is
an Unresolved Question.

Serving `ListObjectVersions` correctly means following its pages, which the code it replaces does
not: it reads one page, so obliteration removes only part of a key with more than a thousand
versions and reports the key removed. The client follows the listing to its end, and fails rather
than returning a short list when the service reports a truncated page without advancing its markers.

Below a configured endpoint's own path, every S3 key Lore uses is a 64-character hex string
(`lore-aws/src/store/immutable_store.rs`) and every DynamoDB request path is `/`, so URI-path
canonicalization, the classic SigV4 failure, has non-trivial input only in an endpoint path an
operator configures.

### Transport, pooling and timeouts

The client sends over `reqwest` with `rustls`, which the workspace already depends on and which
`lore-server` already builds clients from (`lore-server/src/auth/jwk.rs`). Each service client has a
connection pool of its own, configured from the store's existing `http` table: `nodelay` keeps its
meaning, and `pool_idle_timeout_seconds` takes effect, which on main it does not.

The client speaks HTTP/1.1 only. The SDK's connector also offers HTTP/2 when it negotiates TLS
(`aws-smithy-http-client` 1.5.1, `src/client/tls/rustls_provider.rs`), so it uses HTTP/2 with an
endpoint that picks it. Moving requests onto multiplexed HTTP/2 connections changes throughput and
does not belong in a migration; Unresolved Questions asks whether to measure it.

A connection's I/O stays on the net runtime for the connection's life, the property
`net_http_client.rs` restores on main. This client gets it from where it sends requests, not by
wrapping a dependency's types, so the shim goes.

Timeouts keep today's shape. `timeout_millis` is the budget of a whole operation, retries included,
as the SDK's operation timeout is today (`lore-server/src/plugins/aws.rs`), and there is no separate
connect timeout. Each attempt may take what remains of the budget, up to a share of the whole, so an
attempt that stalls leaves room for another. A `GetObject` body is read under the operation's
deadline rather than the attempt's: once the headers arrive, a payload arriving slowly but steadily
has what is left of the whole budget. The SDK stopped only a body that stalled, one sending nothing
for five seconds, and left a slow but steady one unbounded (`aws-smithy-runtime` 1.16.1,
`src/client/defaults.rs`); the budget bounds both.

A response read into memory is refused past a size larger than any answer these operations can
produce, and the size is enforced on the bytes as they arrive rather than on `Content-Length`, which
a chunked reply does not carry.

#### The connection limit

Each store's `http` table gains an optional `max_connections`, unset by default. Set, it is the most
requests each of the store's clients has in flight at once: the immutable store's S3 and DynamoDB
clients each take that many, and so do the clients of the mutable and lock stores. Requests to the
credential endpoints (STS, IMDS and the container agent) are not counted.

- **What counts:** a request counts from before it is sent until its body has been read. A
  `GetObject` counts until its caller has read the body or given up on it, at its deadline at the
  latest. The limit counts neither idle connections nor a connection the HTTP library finishes
  opening in the background after another came free (`hyper-util` 0.1.21, `src/client/legacy/client.rs`), so a
  busy client can hold more sockets than the setting. Under HTTP/1.1 the count is the connections in
  use. The limit is per process, so several servers in front of one storage service each allow that
  many.
- **The wait:** a request past the limit waits for a connection until its operation's deadline,
  past its attempt's share of the budget. It is waiting behind other requests of the same client,
  not stalled against a service that does not answer, so cutting the wait short would only put it at
  the back of the queue again. Requests get connections in the order they asked.
- **A wait that runs out:** a request that finds no connection released by its deadline is neither
  sent nor resent. The stores back off from it as from a service that shed it, and the operation
  metrics label it `aws_code="ConnectionLimit"`, so an operator can tell it from AWS throttling.
- **A wait that leaves too little:** a request whose wait cut its attempt's time short — the whole
  of it fit before the deadline when the wait began — is sent with what remains. When it is not
  answered in that time, the operation metrics label it `aws_code="ConnectionLimitTimeout"` rather
  than `Timeout`, and the stores back off from it. It may have been sent, so it may have been
  applied, and it is not resent, since the budget is spent. A request whose time the deadline had
  already cut, as a late resend's is, or that found a connection free, times out as the service's.
- **How long requests wait:** `urc.aws.http.connection_wait` records each request's wait, labelled
  with the client's `service` (`s3` or `dynamodb`) and, in the server, its `store` (`immutable`,
  `mutable` or `lock`), so a wait points at the store whose limit to raise.
- **Values:** the server refuses `0` when it reads the settings, so a store that sets it does not
  start.
- **Unset:** the client keeps no count, and a request never waits.

A request whose caller stops waiting for it is cancelled, with or without a limit. One still waiting
for a connection is never sent and gives up its place in the queue. One already sent is cut off with
its connection, so a write may or may not have been applied, which is the outcome a dropped
connection already gives and one the caller that gave up never learns either way.

### Signing

The client signs with SigV4 in headers, written over `ring`
(<https://docs.aws.amazon.com/general/latest/gr/signature-version-4.html>). Signing allocates at most
two header values per request: its signature and, for a request with a body, the body's SHA-256
digest.

The client signs `host`, every header whose name begins `x-amz-`, and `content-type` and `range`
when a request sets them. That is one rule over the headers a request carries rather than a list per
operation, and it covers only headers this client sets: a header an intermediary may add or rewrite
breaks a request when signed rather than protecting it. Both named exceptions change what the service
does — `content-type` is what DynamoDB dispatches its protocol on, and `range` decides which bytes a
`GetObject` returns.

An S3 request over TLS is signed with `UNSIGNED-PAYLOAD`, which S3 accepts: TLS already protects the
body in transit, and otherwise every write would digest up to 256 KiB of payload on the calling
worker. A `PutObject` carries the body's CRC32 instead, a far cheaper sum that S3 checks before it
stores the object. Everywhere else the client signs the body's SHA-256 digest: DynamoDB, whose bodies are small,
and any plain-HTTP endpoint, such as the MinIO and DynamoDB Local configurations the integration
tests use, where the signature is the only thing binding the body to the request.

SigV4 rejects a request whose time is more than five minutes from the service's clock. The client
learns the offset from the rejection's `Date` header, where the SDK measures it from every response,
applies it to later signatures and re-signs once, on `RequestTimeTooSkewed`, on `InvalidSignatureException` from DynamoDB
and on `SignatureDoesNotMatch` from S3. Without that, a host with a drifting clock fails every
request rather than the first. An S3 `HEAD` is the exception: S3 answers its skewed signature with a
bare 403 that names no code, so a host's `HeadObject` requests fail until another S3 request on the
same client teaches the offset.

### Credentials

A chain, resolved on first use and cached with refresh, one cache per region shared by the
process's clients:

1. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
2. Container credentials: `AWS_CONTAINER_CREDENTIALS_FULL_URI` or `_RELATIVE_URI`, with
   `AWS_CONTAINER_AUTHORIZATION_TOKEN(_FILE)`
   (<https://docs.aws.amazon.com/sdkref/latest/guide/feature-container-credentials.html>). This is
   ECS and EKS Pod Identity.
3. Web identity: `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_ARN`, with `AWS_ROLE_SESSION_NAME`,
   exchanged through `AssumeRoleWithWebIdentity`, which needs no credentials of its own
   (<https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html>). This
   is EKS IRSA.
4. IMDSv2: a session token, then the instance profile's role
   (<https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instance-metadata-v2-how-it-works.html>),
   at `AWS_EC2_METADATA_SERVICE_ENDPOINT` when set, and skipped under `AWS_EC2_METADATA_DISABLED`.
5. The shared credentials file, `~/.aws/credentials` or the file `AWS_SHARED_CREDENTIALS_FILE`
   names: static keys under `[default]` or `AWS_PROFILE`.

That order differs from the SDK's, which reads the shared files second and web identity before
container credentials (`aws-config` 1.13.0, `src/default_provider/credentials.rs`), as Compatibility
describes. Off EC2, reaching the shared credentials file costs one wait for the metadata service
first, once per process. The chain stops there deliberately. `source_profile` role chaining recurses through STS
`AssumeRole`, which is itself signed with the source profile's credentials, so credential resolution
would become mutually recursive with signing, and it brings AWS's two-file profile precedence and
`mfa_serial`, which prompts a person for a code. None of that belongs in a server process, and the
web-identity path covers role assumption for every deployment. A developer who needs a profile's
chain, SSO included, resolves it in the shell
(<https://docs.aws.amazon.com/cli/latest/reference/configure/export-credentials.html>):

```bash
eval "$(aws configure export-credentials --profile prod-readonly --format env)"
```

That sets the variables step 1 reads.

The cache refreshes credentials before they expire, one caller at a time, while every other caller
keeps signing with the current ones. A refresh that fails while the current credentials are still valid is
tried again on a later request; one that fails after they expire fails the request. Serializing
refreshes does not deduplicate them: a provider that hands back credentials already inside the
refresh margin — a short session, or a clock skewed enough to make fresh ones look stale — would leave
every caller still wanting a refresh, one endpoint call per request. So the cache also spaces refresh
attempts by a floor that a normal refresh never notices.

The region comes from the store's setting, then `AWS_REGION`, then `AWS_DEFAULT_REGION`, then IMDS's
`/latest/meta-data/placement/region`, unless `AWS_EC2_METADATA_DISABLED` is `true`. A store client that
finds none of them fails to build (see Compatibility). IMDS is asked until it answers: its answer,
or a token request that gets no answer at all, which is how a host off EC2 looks, serves every later
client and the telemetry resource detector, so a host off EC2 waits on the link-local address once.
Once the service has answered at all, a request it sheds or leaves unanswered is tried three times,
with a short back-off, and is not kept if every attempt fails, so a later client asks again. Any
other refusal is kept as no region.

An endpoint comes from the store's `*_endpoint_url` setting, then `AWS_ENDPOINT_URL_<SERVICE>` and
`AWS_ENDPOINT_URL`, as the AWS SDKs read them, with `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` honoured.
Otherwise it is the service's regional hostname, under `amazonaws.com.cn` in the China regions and
`amazonaws.com` everywhere else, with S3 addressed by virtual host unless `s3_force_path_style` is
set. A configured endpoint may carry a path, as one behind a gateway does, and every request's path
follows it.

### Retry

**This proposal gives congestion one authority, with one bounded exception, and it is not this
client.** A service that
answered is reporting load, and the storage layer already waits that out: it holds a ladder sized for
it, and it knows whether the caller still wants the work. A ladder in the client as well multiplies —
eight store attempts each wrapping several seconds of client attempts is tens of seconds spent on one
fragment — and a short first wait puts every shed request back on the wire while the service is still
shedding. So the split is:

- **Resent by the client:** a request that got no answer, whether or not it reached the service — a
  connect failure, a reset, a body that stops mid-payload. A `GetObject` body, read after the call
  returns, is the exception: the store's ladder sends that request again. Resending it costs one request, against
  the whole store operation the layer above would otherwise repeat.
- **Handed to the caller:** anything the service answered. HTTP 429 — including the
  `TooManyRequests` an S3-compatible storage service such as MinIO sheds with — HTTP 5xx, and the codes
  `ThrottlingException`, `ProvisionedThroughputExceededException`, `RequestLimitExceeded`,
  `InternalServerError` and `ServiceUnavailable`
  (<https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Programming.Errors.html>), and
  S3's `SlowDown` and `RequestTimeout`
  (<https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html>). So are the codes saying
  the same request succeeds when sent again: S3's `BadDigest`, `XAmzContentSHA256Mismatch`,
  `IncompleteBody` and `OperationAborted`, MinIO's `XAmzContentChecksumMismatch`, and DynamoDB's
  `TransactionInProgressException` and `TransactionConflictException`.
- **Handed to the caller, from this client:** a request the connection limit held back or kept
  queued, as The connection limit describes. A resend held back after an earlier attempt was sent is
  reported as `Timeout` rather than as never sent, since that attempt may have been applied.
- **Corrected by the client:** a skewed clock, as Signing describes, and credentials the cache still
  trusts but the service calls expired, which are dropped, fetched again and signed with once. Each
  fixes the request rather than waiting on the service, no other layer can do it, and each is taken
  at most once per operation.

A wait between attempts never runs past the operation's deadline, so a chain of attempts cannot
overrun the budget its caller was promised. Each ladder jitters every wait around its backoff, by
the same proportion at every rung, in this client and in the storage layer, so a shed herd
spreads out where it bunches, at the top of the ladder.

Making the storage layer the sole authority has consequences, and this proposal takes them on:

- **Its patience is re-sized.** A store attempt used to be worth several requests and up to a full
  operation budget; with a throttle handed straight up it is worth one request, so the waiting is all
  in the store's ladder. The server's store retry policy takes more attempts (`lore-storage/src/lib.rs`),
  restoring the patience the two ladders gave a throttled request between them. A store attempt that
  times out still spends a whole budget, so a call that times out on every attempt holds its request
  handler longer than before, by the budgets of the added attempts. With `max_connections` set and
  reached, each attempt can also queue for a whole budget, as Risks describes. The server answers a Lore client that holds a far longer ladder
  of its own, so it stays the least patient tier.
- **Paths with no ladder get one.** The storage layer's ladder covered fragment reads and writes
  only. The lock store and the mutable store take one of their own, at the same tier: one wait per
  store operation, not per request.
- **`BatchGetItem` resends its unprocessed keys** on a short ladder of its own, the existing
  `dynamodb_unprocessed_keys_retry`, inside one store attempt. Unprocessed keys arrive on a
  successful response, so no layer above sees them. A batch still short when that ladder runs out is
  reported as congestion, which the store's ladder then waits out, rather than as a partial result,
  because a missing association reads as content that is not there. This is the one place two
  ladders nest, and the inner one is bounded by its count of resends.

`TransactWriteItems` is idempotent only under a `ClientRequestToken`
(<https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_TransactWriteItems.html>). A
transaction resent without one can apply twice, and for a lock the second attempt comes back as a
failed condition on a lock the caller in fact holds, reported as "locked by someone else". A resent
transaction carries the token it was first sent with. A release keeps one token across every attempt
of its store ladder, so a release whose answer was lost is replayed rather than found gone, and each
round of an acquisition takes a fresh one, since its items change between rounds. A conditional write
resent after its answer was lost can fail its own condition, so the mutable store's compare-and-swap
reports a swap as applied when it finds the value it writes already in place, and clearing an
obliteration mark or reviving a fragment's state accepts its own earlier write the same way. Two
cannot. A swap to zero deletes its item, and a delete leaves nothing to recognise, so one resent
after its answer was lost reports the swap lost. Taking an obliteration mark cannot either, because
the row does not record who took it: a take resent after its answer was lost fails as a conflict and
leaves the row marked. Both behave as they did when the SDK resent them, and every other operation
is idempotent.

### Lock batches

The lock service holds every lock request to what the lock store applies as one atomic operation,
so the limit becomes part of the lock protocol:

- Each lock store names the most resources it applies as one atomic operation. The AWS lock store
  names 100, the transaction's limit, and a store that names none is held to 100 as well, which
  covers the local lock store.
- The lock service refuses a lock, unlock, admin lock or status request naming more resources than
  that with `OUT_OF_RANGE`, and one naming a resource twice with `INVALID_ARGUMENT`, checking the
  count first. The service keys a resource by its branch and hash. Its description is not part of the key,
  so two resources that differ only in description name one lock, and the service refuses the
  request rather than keeping one of the descriptions.
- The server advertises the most resources it takes in one lock, unlock or status request: the lock
  store's limit, or `[environment.config] max_lock_batch` when that is smaller. A Lore client splits
  its requests to the advertised size, and to 100 when the server advertises none, which is the size
  clients already use (`lore-revision/src/lock/util.rs`). Compatibility gives the field.

### Errors

A failed call is one of four things: an answer from the service, carrying its status, its error
code, its message and request id, and for a failed condition or a cancelled transaction the item or
the reasons; a transport failure, where no answer arrived; no credentials to sign with, so nothing
was sent; or a spent budget. A code this client knows reaches logs and the `aws_code` label under
one canonical name, with the spellings that mean the same thing folded into it, and any other code
as the service wrote it.

DynamoDB names an error in the response's `__type`, and S3 in the XML body's `Code`, except for
`HEAD`, whose answer is its status alone (<https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html>).
The AWS JSON protocol also allows the code in a `code` field or an `x-amzn-errortype` header
(<https://smithy.io/2.0/aws/protocols/aws-json-1_0-protocol.html>), so the client reads both after
`__type`, for a service or an intermediary that answers the protocol that way. Without them such a
throttle would read as an unrecognised code, and a store backs off from an unrecognised code only
when its status says the service shed it.

An expired session is refreshed only when the service says so, with `ExpiredToken`,
`ExpiredTokenException` or S3's `TokenRefreshRequired`. A bare 403 from an S3 `HEAD` carries no body, so an expired session looks
like a denied key, and refreshing on every such refusal would put a credential-provider call behind
each one. A `HEAD` signed with an expired token fails until the next named expiry or the refresh
margin replaces it.

The classification only pays off if the layer above honours it, and today it mostly does not. The
stores map most service failures to `SlowDown` — `AccessDenied`, a bucket that is not there, a
request the service rejected — every S3 failure among them (`lore-aws/src/store/immutable_store.rs`
on main), so the server's store ladder retries a misconfiguration, the Lore client's own ladder
retries it again when the server reports congestion, and whoever reads the log goes
looking for load that was never there. With this proposal a store backs off from a call only when
the client classifies it as shed: a spent budget, a failure to reach the service, a request the
connection limit held back or kept waiting, a status or code the service marks as congestion, a code
saying the same request succeeds when sent again, or a credential provider that could not be
reached. It reports every other failure as a failure. Neither
is reported as not-found: a failed read says nothing about whether the content exists, and a
not-found on a referenced hash is what the immutable store treats as lost data. The retry loop and
the stores read the same classification, so a code cannot mean congestion to one tier and something
else to the other.

### The item model

Reading a row costs no allocation per attribute:

- **One materialization.** An item is decoded once, from the response buffer, and the typed entry is
  read from it. No hash map is built.
- **No name allocation.** The attribute names in Lore's rows are its types' field names, so an item
  holds them without copying, and only a name read off the wire takes storage.
- **No allocation for a value Lore stores.** A string refers to the response buffer, and every binary
  value Lore stores — a hash or a context, 32 bytes at most — is held in the item. The fragment and
  mutable store rows read in bulk decode with no allocation of their own; a lock row, the widest and
  far rarer, takes one.
- **Deterministic bodies.** An item keeps its attributes in order, so a request's body is a function
  of its input, which is what lets a test compare a resent request with the first byte for byte.

Rows keep their exact encoding: every entry encodes as `serde_dynamo` encodes it today, and an
identifier keeps its binary encoding in every row, which the differential test under Verification
proves.

What this does not reach: an item still materializes once between the response buffer and a typed
entry, and a write still encodes each row into one allocation, with a copy of each binary value.

### Verification

Five layers, in increasing order of how much they prove:

1. **Signing vectors.** The published SigV4 test suite
   (<https://docs.aws.amazon.com/general/latest/gr/signature-v4-test-suite.html>), on the canonical
   request, the string to sign and the signature.
2. **A scripted local service** that records every request, so a test asserts what was signed as
   well as what came back. It produces what a real backend will not reliably: a success carrying
   `UnprocessedKeys`, a throttled transaction, a skewed clock, a silent service, an unresolvable
   credential, a chunked body past the size cap, a body that stops mid-payload, and a client whose
   connection limit is reached.
3. **Differential item encoding.** For every entry type the stores write, in both directions and in
   the legacy row shapes, the client's encoding equals `serde_dynamo`'s. `serde_dynamo` stays as a
   test-only dependency for this, without the feature that brings in the SDK.
4. **The client against real services,** the MinIO and DynamoDB Local containers the integration
   suite already runs. MinIO verifies SigV4, so against it the signature is shown to be accepted
   rather than well formed. DynamoDB Local verifies no signature, so it proves the wire format alone.
5. **The existing integration suite,** both stores and the lock store against MinIO and DynamoDB
   Local, including obliteration, pagination, index queries, transactions and conditional writes.

What no real service in the suite exercises is proven as specified rather than as AWS answers it:
`max_connections` against a storage service that sheds; a session token, which the suite never
sends; virtual-hosted S3 addressing, since the suite's endpoints are IP addresses; TLS, since every
endpoint in the suite is plain HTTP; endpoint and region resolution from the environment; the
credential providers and their refresh; clock-skew re-signing; obliteration from a versioned bucket
through the store; DynamoDB signature verification; `UnprocessedKeys` from a real service; and how
DynamoDB answers a transaction replayed under its token.

The allocation goal gets a measurement rather than an argument, because "improves the allocation
profile" is not something a review can settle by reading. Appendix A gives the method and the
results.

## Compatibility

- **Wire format** — N/A. Lore's fragment and revision encodings are untouched.
- **Client/server protocols** — The lock protocol changes, as Lock batches describes.
  `lore.environment.v1.Config` (`lore/environment/v1/environment.proto`) gains `max_lock_batch`
  (field 3); `0`
  or absent advertises no limit, and an older peer ignores the field. The legacy message in
  `model.proto` is left alone, since the clients still on it predate the field. The lock service
  refuses a lock, unlock, admin lock or status request naming more resources than the lock store takes
  at once with `OUT_OF_RANGE`, and one naming a resource twice with `INVALID_ARGUMENT`. Every Lore
  client batches to 100 or fewer, old and new alike, so it is unaffected unless it names a resource
  twice. A third-party gRPC caller that sends more than 100 resources to lock or unlock, or a resource
  twice, is refused where the store used to take the request, and one that branched on
  `INVALID_ARGUMENT` for an oversized status request receives `OUT_OF_RANGE`.
- **AWS requests** — The requests carry the same operations, attributes and object metadata keys as
  the SDK's. A `PutObject` carries the CRC32 of its body, as the SDK's does by default, and S3 checks
  it before storing the object (`aws-smithy-types` 1.9.0, `src/checksum_config.rs`, where request
  checksums default to `WhenSupported`, and `aws-sdk-s3` 1.155.0, `src/operation/put_object.rs`). A
  `GetObject` no longer asks S3 for the stored checksum to check the body against, as the SDK's does
  by default; Security Considerations gives what checks a read instead. DynamoDB requests go to the
  regional endpoint, `dynamodb.{region}.amazonaws.com`, where the SDK sent requests signed with
  credentials that carry an account ID — web identity, container credentials, or keys with
  `AWS_ACCOUNT_ID` or `aws_account_id` set — to the account-based endpoint,
  `{account}.ddb.{region}.amazonaws.com` (`aws-sdk-dynamodb` 1.134.0, `src/config/endpoint.rs`, with
  `AccountIdEndpointMode` defaulting to `Preferred` in `aws-types` 1.7.0, `src/endpoint_config.rs`).
  A network rule that names DynamoDB's hostname sees a different one. Every request names
  `lore/<version>` in `User-Agent`, where the SDK named itself there and in `x-amz-user-agent`, so a
  policy condition on `aws:UserAgent`, or a log filter that matched the SDK's, sees Lore's instead.
- **On-disk format** — N/A. DynamoDB rows and S3 objects keep their schema and encoding, including
  the `repository_context` attribute and the `lore-fragment` object metadata key; the differential
  test under Verification proves the rows' encoding.
- **CLI and public API** — No CLI change. `lore-aws` stops re-exporting SDK types, so code inside the
  tree names Lore's instead; `lore-aws` is not published (`lore-aws/Cargo.toml`), so no consumer
  outside the tree sees this.
- **Configuration** — The AWS store settings keep their names: the `http` table, `timeout_millis`,
  `*_slow_operation_threshold_millis`, `s3_force_path_style`, `*_endpoint_url`, `*_region` and
  `dynamodb_unprocessed_keys_retry`. They keep their meanings too, with two changes:
  `timeout_millis` now also bounds the reading of a `GetObject` body, where the SDK stopped only one
  that stalled, so a payload has to arrive within what is left of the operation's budget once its
  headers have; and `pool_idle_timeout_seconds` takes effect, so idle connections close after it, 30
  seconds by default, where on main they stayed open for the HTTP library's 90. Each store's `http`
  table, such as `[plugins.aws.immutable_store.http]`, gains `max_connections`, unset by default; a
  store that sets it to `0` does not start, and a server without the setting ignores the key. The
  mutable store's `force_write`, which nothing reads, leaves its settings, and a table that still
  sets it loads as before. `[environment.config]` gains `max_lock_batch`.
- **Credentials** — The chain narrows and reorders, as Credentials describes:
  - A profile in `~/.aws/config` or `~/.aws/credentials` that takes its credentials from `role_arn`
    (with `source_profile` or `credential_source`), `credential_process` or `sso_session`, and static
    keys in `~/.aws/config`, stop resolving; their holder exports credentials into the environment
    instead.
  - The shared credentials file is read last rather than second. A host that has both a credentials
    file and a container, web-identity or instance role now signs as the role, where the SDK signed
    with the file's keys.
  - Container credentials come before web identity. A pod that has both now signs with the
    container's.
  - Over plain HTTP, `AWS_CONTAINER_CREDENTIALS_FULL_URI` must name a loopback address, `localhost`
    as written, or the container agent's addresses, where the SDK accepted any host name that
    resolved to loopback.
- **Region** — Region resolution narrows, and this can stop a deployment starting. A store client
  takes its region from the setting, then `AWS_REGION`, then `AWS_DEFAULT_REGION`, then IMDS, and
  fails to build if none of them says. The SDK would also have consulted `~/.aws/config`, so a
  deployment that sets its region only there now fails at startup, with a message naming the places
  to set it. An EC2 deployment that configures no region keeps the instance's own, as it did with the
  SDK.
- **Environment variables** — Besides those Credentials and the endpoint resolution name, the client
  reads none of the SDK's configuration variables (`aws-config` 1.13.0, `src/default_provider/`):
  `AWS_CONFIG_FILE`, `AWS_USE_FIPS_ENDPOINT`, `AWS_USE_DUALSTACK_ENDPOINT`, `AWS_MAX_ATTEMPTS`,
  `AWS_RETRY_MODE`, `AWS_REQUEST_CHECKSUM_CALCULATION`, `AWS_RESPONSE_CHECKSUM_VALIDATION`,
  `AWS_ACCOUNT_ID_ENDPOINT_MODE` and `AWS_ACCOUNT_ID` have no effect. A deployment that sets `AWS_USE_FIPS_ENDPOINT` must
  name the FIPS endpoints in its `*_endpoint_url` settings, or its requests go to the standard
  endpoints. STS requests follow the environment's proxy settings, where the SDK sent them direct;
  requests to the stores' services and to the metadata endpoints do not.
- **Telemetry** — Every existing metric keeps its name. A service error code the SDK named labels
  `aws_code` as before, except that codes meaning the same thing fold into one name: S3's
  `TooManyRequests`, `RequestThrottled` and `ThrottlingException` into `SlowDown`,
  `AllAccessDisabled` into `AccessDenied`, `RequestTimeoutException` into `RequestTimeout` and
  `TokenRefreshRequired` into `ExpiredToken`; DynamoDB's `ThrottledException`, `Throttling` and
  `TooManyRequests` into `ThrottlingException`, `InternalFailure` into `InternalServerError`,
  `ServiceUnavailableException` into `ServiceUnavailable`, `AccessDenied` into
  `AccessDeniedException`, `SignatureDoesNotMatch` into `InvalidSignatureException`, `RequestExpired`
  into `RequestTimeTooSkewed` and `ExpiredToken` into `ExpiredTokenException`. An answer with no body
  is named from its status — S3's as `Forbidden`, `ServiceUnavailable`, `RequestTimeout`, `SlowDown`
  or `InternalError`, DynamoDB's 5xx as `InternalServerError` and 429 as `ThrottlingException`, and
  any other as its status number — where the SDK named only a 404 and labelled the rest
  `<unknown>`. Failures no
  service answered take the client's own labels in place of the SDK's (`lore-aws/src/lib.rs` on
  main): a spent budget is `Timeout` where the SDK said `SdkTimeoutError` or
  `DispatchFailure_Timeout`; a failed connection, read or response is `TransportIo`, `TransportBody`
  or `TransportProtocol` where it said `DispatchFailure_IO`, `DispatchFailure_Other` or
  `DispatchFailure`; a response the SDK could not parse, `<unknown>` on main, is `TransportProtocol`;
  and among the SDK's other `<unknown>` failures, the client labels a request that could not be built
  `TransportBuild` and a credential failure `Credentials` or `CredentialsTransient`. With `max_connections` set, the
  `urc.aws.http.connection_wait` histogram appears, labelled `service` and, in the server, `store`,
  and `aws_code` gains the values `ConnectionLimit` and `ConnectionLimitTimeout`.

## Non-Functional Considerations

- **Concurrency** — A batched read over 100 keys sends its chunks concurrently, as it does today, and
  a paginated query reads its pages in turn, because each page's request names where the previous
  one stopped. Every store operation shares its service client, with one pool and, when
  `max_connections` is set, one limit that waiting requests queue behind in arrival order. Credential
  refresh is single-flight, so a herd of expiring requests makes one endpoint call.
- **Memory** — The proposal's first goal. Per item, two hash maps and an allocation per attribute
  name and binary value become no table and no allocation per attribute; The item model gives the
  reasoning and Appendix A the measurement. Payloads stay reference-counted buffers end to end, and
  signing allocates at most two header values per request. Nothing buffers in proportion to
  repository size: a payload is at most one fragment, and a response read into memory is capped.
  With `max_connections` set, a request queued for a connection holds its body until it is sent or
  its deadline passes, so the queue holds no more than the operations in flight.
- **Statelessness** — The client holds process-level state: a connection pool, a credential cache, a
  cache of derived signing keys and a learned clock offset. The SDK holds a pool, a credential cache
  and a clock offset today, and derives a signing key for every request. With `max_connections` set
  the client also holds the count of requests in flight.
- **Determinism** — Improves. Ordered items make a request's body a function of its input, which the
  SDK's hash-map iteration order does not give. Retry waits keep their jitter, so timing stays
  non-deterministic by design.
- **Latency under the connection limit** — With `max_connections` set and reached, a request queues
  for up to its operation's budget, and the store's ladder repeats that for each of its attempts,
  where a service that sheds answers at once. Without the setting, nothing queues in the client.

## Migration Plan

Nothing in a repository needs migrating. What needs operator attention is the configuration,
credential and telemetry changes in Compatibility, one of which can stop a deployment starting, and
the lock requests the server now refuses.

**Who is affected.**

- A deployment that sets its region only in `~/.aws/config`.
- A person or CI runner authenticating through a `~/.aws/config` profile of the kinds Compatibility
  names.
- A host with both a shared credentials file and a role, which now signs as the role, and a pod with
  both container credentials and a web identity, which now signs with the container's.
- A deployment relying on `AWS_USE_FIPS_ENDPOINT`.
- A deployment whose network rules name the account-based DynamoDB hostname.
- A deployment that sets `HTTPS_PROXY` for other traffic, whose STS requests now go through it.
- A container whose `AWS_CONTAINER_CREDENTIALS_FULL_URI` names, over plain HTTP, a host that resolves
  to loopback.
- Every deployment whose alerts or dashboards match `aws_code` values Telemetry renames or folds.
- A third-party gRPC caller that locks more than 100 resources at once or names one twice.

A container or instance deployment using environment credentials or an instance role alone, the
production shape, is unaffected but for the telemetry change.

**Phase 1, pre-flight, before the change lands anywhere.** Each condition above is visible from
configuration alone: confirm each deployment off EC2 sets a region in its settings or its
environment, that no runner depends on such a profile, which identity each host and pod is meant to
sign as, which endpoints a FIPS deployment names, that network rules and proxies allow the regional
DynamoDB hostname and STS, and which alerts match the `aws_code` values Telemetry changes. Update
those alerts to match both the old values and the new ones before the change lands. Each fix —
setting the region, exporting credentials, removing a stale credentials file, naming the endpoints —
works with the SDK too, so it can be made before this lands, with no coordination.

**Phase 2, a rolling upgrade.** Servers on the SDK and servers on this client can share tables and
buckets while the fleet upgrades: both read and write the same rows and objects byte for byte, as
On-disk format states. Every Lore client works with either server, since it batches lock requests to
100 or fewer. A server on this client refuses the oversized and duplicate lock requests an SDK server
took.

**The failure mode is loud.** A missing region fails at client construction with a message naming the
places to set it, rather than starting and reading the wrong table. A change of signing
identity is not loud: a host that signs as a role it was not meant to sees access errors, or succeeds
with the role's access, which is why the pre-flight checks it.

**Rollback.** Reverting restores the SDK, and, because rows and objects keep their encoding, a
reverted server reads everything this client wrote, so a deployment recovers by revert with no state
to undo. The signal to watch is a rise in the operation metrics' failures by `aws_code` — service
codes compare directly except where Telemetry folds or names them, and transport failures under its
mapping — or in the stores'
`SlowDown` rate, against the pre-change baseline. With `max_connections` set, the `SlowDown` rate
also counts the operations the client's own limit held back or kept queued, labelled
`ConnectionLimit` and `ConnectionLimitTimeout`. A server rolled back ignores the setting, so the
limit goes with it.

## Security Considerations

This proposal moves request signing and credential acquisition from audited upstream code into
Lore's, which is the security-relevant part of it.

**Signing.** A network attacker could modify a header that is sent but left out of the signed set
undetected. The client signs by one rule over the headers each request carries — `host`, every
`x-amz-*` header, and `content-type` and `range` when set — so no header this client sets under
`x-amz-` is sent unsigned, and the signing vectors and the scripted service, which checks every
`x-amz-*` header it receives against the signed set, both assert it.

**Payload integrity.** An S3 body sent over TLS is signed as `UNSIGNED-PAYLOAD` and carries its
CRC32, which S3 checks before it stores the object, as it did with the SDK. A payload damaged between
the client's buffer and S3's storage — past a proxy or gateway that ends the TLS connection, say — is
refused with `BadDigest` while the server still holds it, and the store sends it again. Over plain
HTTP the client also signs the body's digest. A read is no longer checked against the checksum S3
stored: [ADR 00023](../developing/decisions/00023-data-verification-and-trust.md) has a server trust
its durable store, and a Lore client hashes every payload it fetches and refuses one that does not
match its address.

**Credentials.** Credentials reach nothing but the `Authorization` and `x-amz-security-token`
headers, and their debug output redacts the secret and the token, so neither can reach a log line
through a formatter. The client never builds a presigned URL. It reaches IMDS through IMDSv2 only, so
an operator can require IMDSv2 on the instance — which is what stops a server-side request forgery
from reading the metadata service with a plain GET — without breaking Lore.

The client accepts `AWS_CONTAINER_CREDENTIALS_FULL_URI` only for loopback and the container agent's
fixed addresses unless its scheme is HTTPS, the restriction the AWS SDKs apply. It is load bearing:
the provider sends `AWS_CONTAINER_AUTHORIZATION_TOKEN` to whatever that variable names, so without
it one environment variable could make the process hand the token, and with it the role, to an
arbitrary host in cleartext. The client compares a hostname as written and never resolves it,
because a name that resolves to loopback today can resolve elsewhere tomorrow.

**Endpoints.** A deployment that relied on `AWS_USE_FIPS_ENDPOINT` to reach FIPS endpoints reaches
the standard ones unless it names the FIPS endpoints in its settings, as Compatibility describes.

**Responses.** A misdirected endpoint is an error rather than an outage. The client refuses a response
read into memory past a size larger than any answer these operations produce, enforced on the bytes
as they arrive. A small body can still decode into far more than its size, so where a request fixes
how many entries its answer can hold — rows and unprocessed keys never outnumber the keys sent, a
cancelled transaction gives at most one reason per item, a version page holds at most S3's thousand
entries — the client refuses a longer answer as it decodes it. `GetObject`, the one unbounded body,
streams. The client truncates a service's error message before keeping it, because the service
chooses that string and it reaches a log line.

**TLS.** The client uses `rustls` with `ring`, as the SDK's connector did, and offers no way to
disable certificate verification. It trusts the platform's root certificates and the webpki roots,
as the workspace's other `reqwest` clients do (`Cargo.toml`), where the SDK's connector trusted the
platform's only (`aws-smithy-http-client` 1.5.1, `src/client/tls/rustls_provider.rs`), so the set of
roots it accepts grows.

The trust model does not otherwise change: Lore trusts AWS with the same credentials for the same
operations, and no new data crosses a trust boundary.

## Privacy Considerations

No change in what leaves the process. Requests carry what they carry today: hashes, partition and
context identifiers, branch identifiers, lock owner strings and fragment payloads. Nothing new is
logged — a failure carries the service's code, message and request id, which the stores already log,
and not the item bodies an SDK error's debug output prints. Deletion and redaction are unaffected:
obliteration removes S3 object versions and DynamoDB rows through the same operations.

## Risks and Assumptions

**Assumptions**

- **Assumption:** A ceiling on the requests each client has in flight keeps a storage service that
  cannot keep up with the server's concurrency, as the one in Appendix A could not, from shedding —
  *invalidated if:* a run with `max_connections` set sees that service shed at the same rate. No run
  has measured it.
- **Assumption:** Lore's AWS surface stays at these 15 operations — *invalidated if:* a feature needs
  multipart upload, S3 lifecycle or tagging, DynamoDB streams, or `UpdateItem`. Each would be one more
  request and response of its own, but the assumption is what keeps the client small.
- **Assumption:** Production credentials come from the container, web-identity or IMDS paths, and a
  developer who needs a profile chain exports it into the environment — *invalidated if:* an automated
  job outside a container relies on `source_profile`, SSO or `credential_process` and has no shell to
  export from.
- **Assumption:** S3, and the S3-compatible stores Lore runs against, accept a CRC32 as a
  `PutObject`'s checksum — *invalidated if:* one of them refuses `x-amz-checksum-crc32` or requires
  another algorithm.

**Risks**

- **Risk:** `max_connections` set too low turns local queueing into `SlowDown`, so store operations
  back off from a limit of their own process as from AWS throttling, and each store attempt can queue
  for a whole `timeout_millis` before it backs off, where a service that sheds answers at once. A
  resend held back after an earlier attempt was sent is labelled `Timeout`, as a slow service is —
  *mitigation:* the setting is unset by default, and `connection_wait`, labelled by store, shows how
  long each store's requests queue, so a limit that is reached shows against the store that sets it,
  and the `ConnectionLimit` and `ConnectionLimitTimeout` labels show before the limit fails requests
  any other way.
- **Risk:** A SigV4 edge case appears only in production — a header canonicalized differently, a
  query parameter sorted wrongly — *mitigation:* one signing rule over the headers each request
  carries, hex-only keys, the published vectors, and the operations run against MinIO, which verifies
  SigV4. DynamoDB Local does not, so a signing case only DynamoDB requests reach rests on the vectors.
- **Risk:** A resent `TransactWriteItems` applies twice and a caller sees "locked by someone else" for
  a lock it holds — *mitigation:* the resend carries the first attempt's request token, which a test
  asserts by comparing both request bodies byte for byte, and every token fits the 36 characters the
  service allows
  (<https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_TransactWriteItems.html>).
- **Risk:** A take of an obliteration mark resent after its answer was lost fails as a conflict and
  leaves the row marked, and a mutable-store swap to zero resent after its answer was lost reports
  the swap lost — *accepted:* neither row records who wrote it, so a resend cannot tell its own
  earlier write from another writer's, and the SDK's resends had the same exposure. Recording the
  writer would close both.
- **Risk:** The credential cache calls its endpoint once per request instead of once per refresh,
  under exactly the conditions where that endpoint is already struggling — *mitigation:* refresh
  attempts are spaced by a floor on top of the single-flight refresh, and a test drives concurrent
  callers at expiry and asserts one call.
- **Risk:** The client's encoder writes identifiers as hex because the identifier rule in `lore-base`
  does not recognise it — *mitigation:* the rule recognises the client's encoder, a test fails if it
  stops doing so, and the differential encoding test fails on exactly this.
- **Risk:** The retry classification is narrower than the SDK's, and a transient failure starts
  failing requests that used to succeed — *mitigation:* the classification is one table, and the
  `aws_code` label on every operation's metrics shows an unhandled code as a rise in a labelled
  failure.
- **Risk:** A service-side change Lore does not notice arrives as a production failure rather than as
  an SDK upgrade — *mitigation:* the `aws_code` label shows a new failure by its code, and a store
  backs off from a code it does not recognise only when its status says the service shed it, so the
  failure surfaces rather than looping.
- **Risk:** A credential provider this client does not implement is in use somewhere — *mitigation:*
  the chain logs at debug which provider satisfied each resolution and which declined, and a chain
  that resolves nothing fails the request before anything is sent.
- **Risk:** A slice of a response body outlives the decode and keeps the whole response alive,
  turning an allocation win into a retention leak — *mitigation:* binary values are held in the item
  rather than referring to the body, and typed entries take owned values before the response is
  dropped, so only the values passed straight into the next request (`LastEvaluatedKey`,
  `UnprocessedKeys`) refer to it.
- **Risk:** The allocation profile improves in the benchmark and not in production, because the real
  cost was elsewhere — *mitigation:* the measurement pairs the count with the kernel's own peak RSS
  under the shipping allocator, so a count that falls without the footprint following shows rather
  than being assumed.

## Drawbacks

- Lore owns SigV4 correctness, credential refresh and retry classification, which AWS maintains today.
- A new AWS operation costs a request and a response written by hand instead of a method call.
- Code that reads an attribute uses the client's string and binary types instead of the standard
  library's.

## Alternatives Considered

### Keep the SDK, narrow its features

Disable more Cargo features, drop `aws-config` in favour of hand-built credentials, and keep the
generated clients.

*Rejected because:* the crate count barely moves — `aws-sdk-s3` needs `aws-smithy-runtime`,
`aws-smithy-http`, `aws-smithy-types`, `aws-sigv4`, `aws-runtime` and `aws-types` whatever its
features — and the allocation profile and the error model stay as they are.

### Keep the SDK, fix the layers Lore owns

Keep the SDK and change what Lore does around it: decode the SDK's items directly instead of through
`serde_dynamo`, stop copying them at the call sites, turn off the SDK's own retries so the storage
layer is the one authority, and move onto the SDK's opt-in connection pool, which places connection
work on a runtime the caller names (`aws-smithy-http-client` 1.5.1, `src/client/pool/partition.rs`)
and offers a per-host bound, retiring the runtime shim.

*Rejected because:* it removes Lore's share of the allocations and keeps the SDK's. The SDK's own
decode still builds a hash map per item with a heap string per attribute name and a heap buffer per
binary value, which is one materialization of the two this proposal removes, and every one of the
22 `aws-*` crates stays. The error model stays opaque. It is the smaller change, and the place to
start if this proposal were rejected.

### Keep the SDK's transport, replace only the protocol layer

Use `aws-smithy-runtime` for signing, retries and connections, and hand-write the DynamoDB and S3
requests and responses.

*Rejected because:* it keeps an error and retry model the stores already override, and with
`aws-smithy-runtime` most of the dependency graph: that crate alone brings `aws-smithy-async`,
`aws-smithy-http`, `aws-smithy-observability`, `aws-smithy-runtime-api`, `aws-smithy-schema` and
`aws-smithy-types` (`Cargo.lock` on main). It writes the protocol layer, where the allocations are,
and keeps the crates, the worst split of the two.

### Use `aws-sigv4` for signing and write the rest

Hand-write transport, protocol and credentials, but take signing from the crate that specializes in
it.

*Rejected because:* `aws-sigv4` keeps at least seven of the crates this proposal removes —
`aws-sigv4`, `aws-credential-types`, `aws-smithy-async`, `aws-smithy-http`, `aws-smithy-runtime-api`,
`aws-smithy-runtime-api-macros` and `aws-smithy-types` (`Cargo.lock` on main) — in exchange for an
algorithm the published SigV4 test vectors verify exactly.

### Use Apache Arrow's `object_store`

`object_store` implements SigV4, the S3 calls and the AWS credential providers over `reqwest`.

*Rejected because:* it covers object stores only. Eight of Lore's fifteen calls are DynamoDB's, which
would still need a client of their own, signing and credentials included, so the dependency would
cover the smaller part of the problem and leave the part with the allocation profile untouched.

### Use `rusoto`

*Rejected because:* `rusoto` is unmaintained (<https://github.com/rusoto/rusoto>).

## Prior Art

A purpose-built S3 client is an established choice where an SDK is out of proportion to the calls
made. `minio-go` (<https://github.com/minio/minio-go>), which restic's S3 backend uses
(<https://github.com/restic/restic>), implements SigV4 and the S3 calls it needs itself rather than
wrapping an AWS SDK, and `s3fs-fuse` (<https://github.com/s3fs-fuse/s3fs-fuse>) signs its own
requests over libcurl. Apache Arrow's `object_store` (<https://github.com/apache/arrow-rs-object-store>)
does the same in Rust over `reqwest`. The lesson worth borrowing is that a small, explicit request per
operation is easier to keep correct than a general client.

Git LFS is the contrast rather than the precedent. Its client authenticates only to an LFS server,
over the LFS batch API, and the server holds the storage credentials and hands out transfer URLs.
Lore's server holds that role, which is why this client serves the server's stores and the `lore`
client never holds storage credentials.

## Unresolved Questions

- **Should obliteration delete S3 delete markers?** The store ignores them today and this proposal
  keeps that, so a key whose versions are all deleted but whose marker remains is still listed and
  still billed. The client reports the markers, so deleting them would change only the store. It is
  a decision for whoever owns the obliteration guarantee.
- **Does HTTP/2 help?** The client speaks HTTP/1.1 only. Whether multiplexing helps a workload of
  many small requests is a measurement, not a design decision.
- **Should the client hold an adaptive rate limiter of its own?** `max_connections` gives an
  operator a fixed ceiling. What stays open is a ceiling that follows the storage service. Without
  the setting the client has no ceiling on its own offered load: it sends whatever concurrency the
  store hands it, and backoff per request cannot break a herd, because each request decides alone and
  none knows the others were shed too. S3 scales its request rate per prefix and answers 503
  `SlowDown` past it (<https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html>);
  a single-node store has no such headroom, as Appendix A shows. The shape of an answer is known — a
  token bucket over the whole client, drained by throttles and refilled by successes, which AWS SDKs
  call adaptive retry mode (<https://docs.aws.amazon.com/sdkref/latest/guide/feature-retry-behavior.html>)
  — but whether Lore needs it, and whether it belongs in this client or in the storage layer that owns
  congestion, is not settled here.

## Appendix A — what the implementation measured

Everything above proposes; this records what building the proposal produced, so a reviewer can check
the case against evidence rather than argument.

**Method.** A harness drives a fixed workload that commits, pushes and clones — 79,150 files and
9.23 GiB, producing 176,150 S3 objects and 180,812 DynamoDB rows — against both clients on the same
local MinIO and DynamoDB Local stack, destroyed and verified empty before every run, and fails a run whose stored
object and row counts differ from the other side's. Both trees were built at `release-lto`. One
`LD_PRELOAD` interposer, used unchanged by both trees, counts allocations, and the kernel's `VmHWM`
gives peak RSS. The interposer sees only libc, so counting needs `LORE_ALLOCATOR=system`:
**allocation counts come from instrumented runs, one per tree, and every memory and timing figure
from uninstrumented runs on the shipping allocator, three per tree, compared repeat by repeat.**
Counts are a property of the code and survive the substitution; RSS is a property of the allocator
and does not. The SDK side is main with `aws-sdk-s3` 1.133.0, before main moved to 1.155.0, and these
figures have not been measured again since.

| metric | AWS SDK | this client | delta |
|---|---:|---:|---:|
| allocations, write path | 579,384,604 | 107,157,088 | −81.5% |
| allocations, read path | 164,197,311 | 34,202,603 | −79.2% |
| allocations, combined | 743,581,915 | 141,359,691 | **−81.0%** |
| bytes requested, write path | 96.84 GiB | 54.06 GiB | −44.2% |
| peak RSS, write path | 2.85 GiB | 2.14 GiB | **−25.1%** |
| peak RSS, read path | 1.46 GiB | 1.42 GiB | not a result |
| wall clock, measured phases | 115.8s | 102.5s | **−11.5%** |

Confidence differs per metric, and the weakest is stated as weak:

- Allocation counts reproduce to within 0.2%. The reduction is uniform across operations —
  committing −80.7%, pushing −82.8%, cloning −79.2% — and it is entirely in allocations of 256 B and below.
  Payload-sized buckets are unchanged, 64 KiB −0.0% and 128 KiB +0.0%, which shows both trees moved
  the same data through the same buffers.
- Write-path peak RSS is lower in 3 of 3 repeats, by 22–30%, with the baseline's own spread at ±0.3%.
- Wall clock is lower in 3 of 3 repeats: −8.5%, −17.5%, −8.0%.
- **Read-path peak RSS is not a result.** −3.1% against ±16.6% spread on both sides, lower in only 1
  of 3 repeats. It is recorded for completeness and is not a gain.

An 81% cut in allocation count buys 25% in resident memory, and the gap is expected: write-path RSS is
dominated by fragment payload buffers, which both builds allocate identically.

### Dependency and binary size

Against main with `aws-sdk-s3` 1.155.0, the version this change was last merged with, both built at
`release-lto` with the same toolchain:

| | AWS SDK | this client | delta |
|---|---:|---:|---:|
| `loreserver`, stripped | 46,896,680 B | 39,604,008 B | −7,292,672 B (**−15.6%**) |
| `loreserver`, unstripped | 707,693,880 B | 611,671,048 B | −96,022,832 B (−13.6%) |
| `Cargo.lock` packages | 523 | 469 | −54 |
| `aws-*` crates | 22 | 0 | — |

Of the 54 packages fewer, 48 are the SDK's; the rest left with dependencies this change also dropped.

### What building it showed

Four findings changed the design, and each is reflected above:

- **Item order was not deterministic** while items passed through `serde_dynamo`'s hash map, so two
  encodings of one row could differ. The client keeps attributes in order, which the determinism in
  The item model needs.
- **The client classified overload too broadly.** Counting everything it would resend included the
  clock-skew codes, so a signature the service kept rejecting reached a store as congestion.
- **An S3-compatible storage service sheds with a code AWS does not use.** MinIO answers a shed request with
  `429 TooManyRequests`, which an unrecognised-code rule would never resend, so the client classifies
  the status as well as the code.
- **The two retry ladders multiplied.** Against a single-node store, one throttled fragment could
  spend tens of seconds across roughly thirty requests, and the client's short first wait re-offered
  load to a service that had just asked for less. Before Retry's split, repeats swung by ±32% and no
  timing figure could be defended.

The measurement environment had a failure of its own: MinIO on ZFS parked thousands of threads in
uninterruptible waits under this load, so the harness bounds MinIO's processes and memory. Timings
from a single-node store measure the store's limits as much as the client's, which is why the
allocation count is the figure this proposal rests on.
