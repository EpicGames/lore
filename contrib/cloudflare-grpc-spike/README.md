# Lore gRPC Workers transport spike

This is a feasibility experiment, **not a Lore server**. It exposes unauthenticated
`EnvironmentGet` and a `Get` stream that answers every address with an in-band
`NOT_FOUND`. It stores no data. Do not point real repositories at it.

The protobuf messages are generated from this checkout's Lore definitions. The
probe checks whether responses arrive while the client is still sending requests,
then requires a successful terminal gRPC trailer. The Workers Response API does
not document a way to set such a trailer; this experiment deliberately does not
pretend that an initial status header is a terminal trailer. A failure is a
feasibility result, not a passing server implementation.

Requirements: `protoc`, Rust with `wasm32-unknown-unknown`, `worker-build` 0.8,
Wrangler 4, and Node.js.

```sh
cargo install worker-build --version 0.8.7 --locked
rustup target add wasm32-unknown-unknown
node probe.mjs http://127.0.0.1:8788 --local
```

Local Wrangler accepts HTTP/1 here, so this tests Workerd's streaming behavior,
not Cloudflare's public HTTP/2 ingress or gRPC client compatibility. For the
public gate, deploy to an isolated Worker and run:

```sh
wrangler deploy
node probe.mjs https://YOUR-WORKER.example.com
```

The remote probe uses HTTP/2. Test with the actual current Lore client too;
neither the local probe nor the Node probe is a substitute for that test.
`--octet-stream` changes the request content type solely to diagnose HTTP/2
streaming when gRPC requests are blocked; it is not a compatibility test.

The standalone tonic probe uses the same tonic version and TLS settings as the
current checkout, and prints nested connection errors:

```sh
cargo run --locked --example tonic_probe -- https://YOUR-WORKER.example.com
```

Where a local TLS inspection certificate is incompatible with rustls, the
following diagnostic uses a loopback-only TCP-to-TLS bridge with Node's normal
certificate verification. The bridge passes HTTP/2 bytes through unchanged. It
does not test the Lore client's TLS connection and is not a production proxy:

```sh
node tonic-bridge.mjs https://YOUR-WORKER.example.com
```

## Observations (2026-11-04)

The original raw gRPC Fetch diagnostic below was followed by a separate
[Connect/gRPC-web diagnostic](connect/README.md), based on a supplied example.
On a custom domain with gRPC enabled, edge conversion produced valid native
gRPC trailers and supported the bidirectional `Get` probe. A direct test by the
user's installed Lore 0.12.0 client connected over TLS and completed discovery;
the agent's environment still encounters local TLS interception.

With Wrangler 4.147.0 local Workerd returned two item responses before the
request ended. The spike has no implementation of terminal response trailers,
and the probe failed because the terminal `grpc-status` was absent.

On a `workers.dev` deployment, an HTTP/2 request with `application/grpc`
received HTTP 403; the same unary request with `application/octet-stream` reached
the Worker. With the diagnostic content type, the streaming response did not
arrive until the request ended. These results do not establish the behavior of a
custom domain in a zone with gRPC enabled. The current Lore client also reported
a connection-level transport error against that `workers.dev` deployment, before
environment discovery; its precise underlying cause was not established.

On the same date, a custom domain returned HTTP 403 for `application/grpc`.
An `application/octet-stream` unary request reached the Worker after the custom
domain propagated, but a diagnostic stream timed out waiting for its first
response before the request closed. The zone's gRPC setting needs to be enabled
before this can test genuine gRPC behavior on the custom domain.

After enabling the zone's gRPC setting, genuine `application/grpc` requests to
the custom domain reached the Worker and two responses arrived before the request
stream was closed. The probe still failed because the terminal trailer was absent.
The tonic 0.14.6 diagnostic, through the loopback TLS bridge, successfully called
`EnvironmentGet` and received both `Get` item responses, then reported `Unknown`
with `protocol error: missing grpc-status trailer` on the stream's end.

The actual Lore client still failed to connect directly on this machine. The
tonic diagnostic exposed the cause: the network presented a certificate issued
by Cloudflare Corporate Zero Trust, whose P-521 issuer signature was unsupported
by rustls/ring. Node accepted that chain; this is a local TLS-interception issue,
not evidence that the Worker's public certificate is invalid. The bridge does not
establish direct unmodified-client compatibility.

## TCP sockets and trailers

The Workers [`cloudflare:sockets` API](https://developers.cloudflare.com/workers/runtime-apis/tcp-sockets/)
opens outbound TCP connections. The [connect handler](https://developers.cloudflare.com/workers/runtime-apis/handlers/connect/)
also accepts sockets forwarded by another Worker over a service binding. The
general documentation says direct public ingress is unsupported, but the
[gRPC Workers announcement](https://blog.cloudflare.com/grpc-workers/) describes
a private beta for Spectrum TCP ingress targeting a Worker's `connect` handler.
The [Emscripten announcement](https://blog.cloudflare.com/rust-workers-emscripten-target/)
demonstrates serving inbound TCP in Rust and describes the toolchain as experimental.
Public ingress requires separately provisioned Spectrum mapping; ordinary Worker
custom domains invoke `fetch`, not `connect`.

The [Workers `node:http` documentation](https://developers.cloudflare.com/workers/runtime-apis/nodejs/http/#serverresponse)
also explicitly says `ServerResponse` does not support trailer headers.

An experimental raw TCP / Emscripten Worker is in [`tcp/`](tcp/README.md). In
local Wrangler, tonic successfully performed unary discovery, received two
bidirectional `Get` responses, and accepted the terminal gRPC trailer. The
unmodified Lore client also completed discovery locally and reached the expected
unimplemented repository method. Public ingress, TLS, and production readiness
remain unverified.
