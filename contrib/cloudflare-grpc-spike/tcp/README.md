# Raw TCP / Emscripten gRPC transport spike

This unauthenticated local diagnostic stores no data and is not a Lore server.
It accepts cleartext HTTP/2 on loopback port 7000, implements a minimal discovery
reply and returns `NOT_FOUND` for short `Get` requests, then sends actual HTTP/2
`grpc-status` trailers. It is intended to exercise transport only; framing,
flow control, error handling, security and other RPCs are intentionally incomplete.

The listener and isolated connection pattern are adapted from Cloudflare's
Apache-2.0-licensed [incoming TCP example](https://github.com/cloudflare/workers-rs/tree/606f27fd93efc698f575ab7e84c6be66e5535468/examples/emscripten-incoming-tcp).
The license text is available in this repository at
[`../../../vendor/quinn-proto/LICENSE-APACHE`](../../../vendor/quinn-proto/LICENSE-APACHE).

Install Wrangler 4, Node, Rust beta with `wasm32-unknown-emscripten`, and an
Emscripten-capable `worker-build` from the workers-rs revision in `Cargo.toml`.
This revision's build tool downloads and configures Emscripten automatically.
For example, install it from a checkout of that exact workers-rs revision with
`cargo install --locked --path worker-build`. From this directory, run:

```sh
node probe.mjs
```

The script starts local Wrangler and runs the parent directory's tonic probe;
it needs Cargo and the parent's locked dependencies. For the unmodified client,
while `wrangler dev --config wrangler.toml` is running, use:

```sh
../../../target/debug/lore --debug repository list grpc://127.0.0.1:7000
```

That command reaches discovery and fails at the intentionally unimplemented
`RepositoryList` RPC. Cleartext loopback is only a diagnostic, not a production
configuration. On 2026-11-04 both tests succeeded in reaching their expected
outcomes under local Wrangler 4.147.0 with the experimental Emscripten toolchain.
The build printed errors about template manifests while scanning the pinned git
dependency, but completed successfully; this needs investigation before relying
on the toolchain for more than the experiment.

For public connections, Cloudflare's
[announcement](https://blog.cloudflare.com/grpc-workers/) describes a private
beta Spectrum application targeting a Worker's `connect` handler. Availability,
provisioning, TLS handling, streaming, trailers and the actual Lore client all
need testing on the target account. The local `[[connect]]` listener does not
by itself provision public ingress.

## Public ingress investigation (2026-11-04)

The diagnostic was deployed separately as `lore-tcp-grpc-spike`, version
`b70a31e1-40fe-4ddc-adb3-4db8ef58e92e`, with `workers_dev = false` and no
public target. The Workers API reports the `connect` handler and script tag
`e9a30e2d10f64cd988788e6d67f5c871`. The existing Fetch diagnostic was not changed.

The [Spectrum application API](https://developers.cloudflare.com/api/resources/spectrum/subresources/apps/methods/create/)
documents `traffic_type: "worker"` and `origin_worker_id` for TCP Worker
origins. With this origin, TLS can be `off` or `flexible`; the latter terminates
client TLS at the edge. The relationship between Spectrum's socket address and
this diagnostic's port 7000, TLS ALPN, and the actual public transport remain
unverified.

The Wrangler OAuth login can read the `gasparina.work` zone but requests to list
Spectrum applications and DNS records returned HTTP 403, code 10000. The API
documents Zone Settings Read or Write for listing Spectrum applications, Zone
Settings Write for creation, and DNS Read or Write for listing DNS records. An
appropriately scoped credential is needed to inspect existing resources and
provision a separate hostname. No Spectrum application has been created by this
investigation.

On this machine the deploy requires the Emscripten-capable `worker-build` and
the rustup shims ahead of Homebrew's standalone Rust in `PATH`. A dry run and
deployment completed despite the template-manifest diagnostics noted above.
