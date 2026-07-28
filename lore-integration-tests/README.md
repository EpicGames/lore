# Lore Integration Tests

This module currently contains the following tests:

## AWS Store Tests

These tests exercise the AWS store against "real" AWS resources (where "real" in this case means
local approximations of S3 and DynamoDB). In this case we're using [MinIO](https://min.io/) as an
approximation of S3
and [DynamoDB-Local](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/DynamoDBLocal.html)
for DynamoDB.

### Running Locally

Due to their dependence on an external resource, the AWS store integration tests are gated behind a
Cargo feature to ensure that they are not run as part a normal `cargo test` invocation. Before
running the integration tests you must first ensure there are resources available to test against.
The easiest way to do this is use Docker Compose and the
provided [compose.yaml](./compose.yaml) to start up the necessary services:

From the root of the repo just run:

```shell
$ docker compose --file lore-integration-tests/compose.yaml up
```

This will start up both MinIO and DynamoDB local. MinIO will run on port 9000 with
its UI on port 9001 (accessible via http://localhost:9001/, the login info is found
in `MINIO_ROOT_USER` and `MINIO_ROOT_PASSWORD` in the docker-comppase.yaml file).

Once the services are up, you can run the tests via the following:

```shell
$ cargo test --package lore-integration-tests --features integration_tests
```

This will run *only* the integration tests.

#### A Note on Storage

DynamoDB is configured to store everything in memory, so its contents will be wiped whenever the
container is restarted. Unfortunately MinIO does not have a comparable setting, so you'll probably
want to be sure to clear out the bucket contents over time if you're running tests frequently. This
can be done via the UI, or via
the [MinIO client](https://min.io/docs/minio/linux/reference/minio-mc.html) (`mc`).

```shell
$ MC_HOST_local=http://lorelocal:lorelocal@localhost:9000 mc rb --force --insecure local/lore-immutable-store-test
```

## gRPC Integration Tests

There is a suite of integration tests for gRPC Services. These are gated behind a separate feature:
`grpc_integration_tests`, and are mutually exclusive from the other suites of integration tests. This is due to the fact
that a gRPC server must be up and running for the tests to run. To run these tests, first run the server locally,
e.g.

```shell
$ AWS_ACCESS_KEY_ID=lorelocal AWS_SECRET_ACCESS_KEY=lorelocal AWS_REGION=us-east-1 RUST_LOG=info ./target/debug/loreserver 2>&1 | tee /tmp/lore.log
```

By default the tests for the replication service use TLS when talking to the server. For this to work you must generate
a self signed certificate and configure both the server and the test to use them. First run the following script:

```shell
./scripts/server/make-certs.sh
```

This will prompt you for a passphrase, use whatever you like. The certificates will be generated in the `./certs`
directory (relative to the repository root). Note: this script generates two sets of certs, one suffixed with `-bad` to
facilitate verifying the behavior when the client and server certs do not match.

Next, configure your local server to use the certs for the replication gRPC server by adding the following block to
`local.toml`

```toml
[server.grpc_internal.certificate]
cert_file = "./certs/server.crt"
pkey_file = "./certs/server.key"
cert_chain = "./certs/ca.crt"
```

Finally, confirm that [./src/replication_service_test.rs](./src/replication_service_test.rs) is pointing to the same
location for client TLS configuration and that the server url is using the `https://` scheme (it should be by default).
It's also worth nothing that the `domain_name` in the `ClientTlsConfig` must match the value for `$SERVER_CN` from the
`make-certs.sh` script.

Note: If you're not interested in using TLS for the tests, you can skip all of the above and just change the test file
to use an `http://` scheme instead.

Once the server is up and running, you can run the gRPC integration test suite by running:

```shell
$ cargo test --package lore-integration-tests --features grpc_integration_tests
```
