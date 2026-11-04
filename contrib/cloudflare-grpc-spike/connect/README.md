# Connect / gRPC-web transport spike

This unauthenticated diagnostic stores no data and is not a Lore server. It
implements discovery and a `Get` stream that returns an in-band `NOT_FOUND` for
each address. A one-byte hash of `fe` deliberately terminates the stream with
`RESOURCE_EXHAUSTED` to test error conversion.

The Fetch adapter follows the supplied [Connect example](https://gitlab.cfdata.org/mar/grpc-workers/-/blob/master/server/src/worker-handler.ts)
at revision `942ec23a113f5e154e01fef7e0400989064e1c7d`. It generates JavaScript
descriptors directly from this checkout's Lore protobuf definitions. A native
gRPC client relies on Cloudflare's edge conversion to and from gRPC-web.

Requirements: Node, npm, `protoc` and Wrangler 4. Install dependencies with
`npm ci`; Wrangler generates descriptors as part of its build. From this directory:

```sh
node local-probe.mjs
node ../probe.mjs https://YOUR-WORKER.example.com
node edge-errors.mjs https://YOUR-WORKER.example.com
node ../tonic-bridge.mjs https://YOUR-WORKER.example.com
node lore-bridge.mjs https://YOUR-WORKER.example.com
```

The local probe verifies gRPC-web messages and trailers, not native gRPC ingress.
The remote Node probes use native gRPC over HTTP/2. `tonic-bridge.mjs` passes
HTTP/2 through a loopback TLS bridge, and `lore-bridge.mjs` additionally rewrites
the HTTP/2 authority for the unmodified Lore client. Both verify the upstream
certificate using Node, not Lore's TLS implementation. The Lore client reaches
the intentionally unimplemented `RepositoryList` and exits with an error.

## Observations (2026-11-04)

Deployed as `lore-connect-grpc-spike` at `lore-connect.gasparina.work`, version
`2d7aaa70-5a93-432e-b4a9-e4f35adf6d67`, on a zone with gRPC enabled. Native gRPC
discovery returned a successful HTTP/2 trailer; two `Get` replies arrived before
the request stream closed, followed by a successful terminal trailer. The tonic
probe passed the same checks through the loopback TLS bridge. A stream error was
converted to `RESOURCE_EXHAUSTED` with its message; a stream could be cancelled
after a reply and a new HTTP/2 connection completed discovery. This does not
prove server-side resource cleanup or automatic client reconnection.

The unmodified Lore client completed discovery through the authority-rewriting
bridge and reached `RepositoryList`. The user also ran the installed, unmodified
Lore `0.12.0+1967` client directly over `grpcs://` on 2026-11-04: TLS connected,
discovery succeeded, and `RepositoryList` returned the expected 404 because the
diagnostic does not implement it. This is a direct TLS and discovery success for
that client; it is not a direct storage-stream or repository-operation test.
Direct `grpcs://` still fails in the agent's environment because its TLS
interceptor presents a P-521 issuer signature unsupported by the client's
rustls/ring configuration. The spike is JavaScript and does not yet prove the
planned Rust backend can run inside this handler.

## Rust/Wasm boundary (2026-11-04)

The unmodified `lore-revision` crate does not currently build for
`wasm32-unknown-unknown`: `cargo check --locked -p lore-revision --lib --target
wasm32-unknown-unknown` fails in `mio`, which does not support this target.
`cargo tree -p lore-revision --target wasm32-unknown-unknown -i mio --edges
normal,build` shows native networking from Tokio, tonic, QUIC and telemetry;
`lore-storage` also depends on transport and telemetry. The workspace enables
Tokio's filesystem and network features.
Avoiding just one `mio` dependency is therefore not sufficient evidence of a
portable server. Isolating shared types and algorithms from runtime, transport,
filesystem, telemetry, native compression and native allocation, then adapting
async/storage behavior, is necessary to assess the larger port. The precise
scope of that refactoring and remaining Wasm errors are not yet established.

`lore-base` now has a default-on `native` feature. With default features off,
its real domain types, errors and text validation build for Workers Wasm
without Tokio, the OS-backed allocator or `lore-io`. This does not provide OS
randomness, filesystem, async runtime or native allocator APIs. Cargo features
are additive, so every dependency must avoid enabling `native`; currently
`lore-storage` and `lore-revision` still request it and other native libraries.
The usual native build remains the default. From the repository root:

```sh
cargo check --locked -p lore-base --no-default-features --target wasm32-unknown-unknown
```

The local-only [`wasm/`](wasm/) probe now uses the actual `lore-base` crate
with native features disabled. Its scalar ABI exercises compression flags,
BLAKE3 hashing, address parsing and fragment response validation from a Connect
`Get` stream. It also checks that an unfragmented 4 GiB content size is rejected
without truncating it to a 32-bit `usize`. To reproduce, from this directory,
after `npm ci` and installing the Rust Wasm target and Wrangler:

```sh
npm run generate
cargo build --locked --release --target wasm32-unknown-unknown --manifest-path wasm/Cargo.toml
```

Run `node probe.mjs` with `wasm/` as the working directory. Local Workerd
returned all six expected results before the request closed, with a successful
gRPC-web trailer. This does not exercise real storage or show that the
revision/server code runs in Workers. No deployed Worker was changed by this
experiment. Further separation of storage, protocol-generated transport and
revision code is needed before implementing Workers-backed storage. In
particular, `lore-storage` contains other `u64`-to-`usize` conversions in its
ingress, decompression and defragmentation paths; these need explicit bounds
checks before they can be used safely on 32-bit Wasm.

## Supplied-token compatibility (source analysis, 2026-11-04)

The following describes this checkout, not necessarily the user's installed
0.12.0 binary; no authenticated end-to-end request has been tested yet.

* CLI global options `--identity-token` and `--access-token` (or environment
  variables `LORE_IDENTITY_TOKEN` and `LORE_ACCESS_TOKEN`) accept supplied
  credentials. `LoreGlobalArgs::validate` derives the identity from the
  identity token if present, otherwise the access token. The token must decode
  as a JWT with the required claims and a nonempty identity, using `identity`,
  its namespaced alias, or `sub` in that order. This client-side decoding does
  **not** verify the signature or expiry and must not be used for server trust.
* A supplied access token bypasses the repository authorization exchange. But
  supplying it alone leaves the authentication-token slot empty: repository
  create, lookup and list use the authentication interceptor, so they send no
  bearer token in that case. Repository-scoped services use the authorization
  interceptor and can send the access token. Depending on the selected auth
  path, some repository-specific calls also use the authorization interceptor.
  Supplying both identity and access tokens populates the two slots; their
  acceptance and the necessary grants still require an end-to-end test.
* Server verification must independently verify signature/key, issuer,
  audience and expiry, and enforce repository/action grants. Existing server
  code resolves a JWK by `kid` and supports key refresh and configurable resource
  claims; a Worker implementation must match the chosen policy rather than
  trusting the client's decoded identity. Discovery/auth endpoint configuration,
  token issuance or provisioning, repository metadata headers and behavior of
  the actual installed client remain to be tested.

Relevant source: `lore-revision/src/interface.rs`,
`lore-credential/src/jwt.rs`, `lore-transport/src/auth/exchange.rs`,
`lore-transport/src/grpc/mod.rs`, `lore-server/src/auth/jwt.rs` and
`lore-server/src/authnz/resource_grants_authorizer.rs`.

The checkout's unit tests for supplied access-token exchange and for
access-token-only identity resolution passed, as did the test asserting an
access token alone leaves the authentication slot empty. The locally built
CLI also rejected a malformed `LORE_ACCESS_TOKEN` before connecting. These
checks do not verify JWT signatures or exercise an authenticated Worker.

## Backend groundwork (2026-11-04)

The separate [`mvp/`](mvp/) Worker now implements a limited storage, repository
and revision subset backed by R2 and SQLite Durable Objects. It is deployed
separately, and a remote create/push/clone CLI workflow passed through a
loopback TLS bridge. See its README for the endpoint, reproduction steps and
important authentication and feature limitations.
