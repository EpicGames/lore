# Experimental Lore Worker

This is an incomplete, **unauthenticated** experiment. Anyone who can reach the
endpoint can read and modify its data; do not use it for real or sensitive
repositories. It is not a production server.

The isolated deployment is `lore-worker-mvp-spike` at
`grpcs://lore-mvp.gasparina.work`, using the R2 bucket
`lore-worker-mvp-spike-20261104` and SQLite-backed Durable Objects. Cloudflare's
edge translates native gRPC to gRPC-web for the Connect Fetch adapter. The
existing diagnostics have separate Worker names and hostnames.
The Wasm-backed deployment tested below is version
`4cc4120c-0a1e-406e-a3ba-dfe61db05e40`.

On 2026-11-04 the locally built unmodified Lore `0.12.1-nightly` CLI created and
listed a remote repository, staged and pushed two commits, then cloned it and
verified the file contents. That test used a loopback HTTP/2 bridge which
forwards native gRPC over TLS; Node verifies the upstream certificate. It does
not test Lore's own TLS stack. Direct `grpcs://` discovery was previously
verified by the user against the separate diagnostic, not against this MVP.

For an isolated test on a network where Lore can verify the TLS certificate,
use a unique repository name and a disposable working directory:

```sh
export LORE_ACCESS_TOKEN="$(node -e 'const b=x=>Buffer.from(JSON.stringify(x)).toString("base64url"); console.log(b({alg:"none"})+"."+b({sub:"tester",iss:"local",aud:"local",exp:4102444800})+".x")'"
lore --identity tester repository create grpcs://lore-mvp.gasparina.work/YOUR-UNIQUE-NAME
lore --identity tester repository list grpcs://lore-mvp.gasparina.work
# In the new repository's directory:
printf 'Hello from Lore on Workers.\n' > example.txt
lore --identity tester stage example.txt
lore --identity tester commit "initial commit"
lore --identity tester push
lore --identity tester clone grpcs://lore-mvp.gasparina.work/YOUR-UNIQUE-NAME /tmp/lore-clone
```

The example token only satisfies the CLI's local identity handling. The Worker
does not validate it or enforce permissions. The exact installed client version
and its TLS environment may behave differently.

## Reproduce the probes

From `connect/`, after installing Node (the immutable probe uses Node's Zstd
support), Wrangler 4, `protoc`, Rust with the `wasm32-unknown-unknown` target,
and building `target/debug/lore`. Wrangler's custom build uses `connect/` as
its invocation directory and compiles the portable Rust core automatically:

```sh
npm ci
npm run generate
node mvp/mutable-probe.mjs
node mvp/core-probe.mjs
node mvp/immutable-probe.mjs
node mvp/cli-probe.mjs
node mvp/cli-probe.mjs https://lore-mvp.gasparina.work
```

Run local probes sequentially because Wrangler may choose the same internal
ports. They use temporary local state and remove it on exit. The remote probe
creates a uniquely named repository that remains in remote storage; deletion is
not implemented. Its bridge listens only on loopback.

The Rust compiler selected by Cargo must have the Wasm target installed. If a
different `rustc` appears earlier in `PATH`, set `RUSTC` to the target-enabled
compiler (for example, `export RUSTC="$(rustup which rustc)"`).

## Scope and limitations

Fragment ingress checks, content hashing and metadata buffer encoding and
validation use the same portable Rust code as native Lore through Wasm. The
Fetch/Connect, R2 and Durable Object adapters, Zstd decoding, revision graph
traversal and branch operations remain JavaScript; this is not a complete
port of the Lore server or its revision semantics.

Implemented: discovery; repository create/get/list and metadata get; default
branch get/list/metadata get; limited single-parent fast-forward push; mutable
load/store/CAS; immutable put/get/get-metadata/query, resolved mappings and
same-repository copy. The revision checker verifies a limited current-format,
unfragmented subset and referenced content, but is not a complete structural
or security validation of arbitrary Lore revision graphs. The probes cover a
missing reference, concurrent competing pushes and restart persistence, not all
malformed graphs or crash recovery scenarios.

The server advertises no compression; it accepts ordinary bounded Zstd frames
from clients and stores their hash-verified decompressed content. LZ4, Oodle,
Zstd dictionaries and checksums, fragmented revisions, merges, forced pushes,
cross-repository copy, locks, deletion, metadata CAS and most revision APIs are
unsupported. Larger files and other ordinary CLI operations may fail.

Fragments are stored in R2 before their association is published in the
repository Durable Object. Retries can repair interrupted association
publication; interrupted writes can leave unreachable R2 objects. Repository
names remain reserved after interrupted initialization until the same request
retries. Garbage collection, lifecycle management and safe deletion are not
implemented.

`wrangler.jsonc` is local-only. `wrangler.remote.jsonc` configures the isolated
remote deployment; deploy with `CLOUDFLARE_ACCOUNT_ID` set to the intended
account and `wrangler deploy --config mvp/wrangler.remote.jsonc`.
