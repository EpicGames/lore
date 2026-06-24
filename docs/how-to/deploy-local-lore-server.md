# Deploy a local Lore Server

`loreserver` is the server component of the Lore version control system. It's the centralized source of truth for your repositories. The endpoint your team pushes to, clones from, and resolves conflicts through.

In this guide, you'll deploy local Lore Servers — with durable storage and a config you can tailor to your needs — using multiple deployment methods, so your team has a reliable place to land their work.

## Prerequisites

- The `lore` CLI on your PATH. See [Install the Lore CLI](install-lore-cli.md).
- Ports `41337` (both TCP and UDP) and `41339` (TCP) free on the host.
- OpenSSL, to generate the self-signed certificate.

## Choose a path

The binary and Docker paths are mutually exclusive, and each is complete on its own — follow one top to bottom.

- **[Run from the binary](#run-from-the-binary):** Fewer moving parts and native performance. Pick this to run `loreserver` directly on the host.
- **[Run with Docker](#run-with-docker):** An isolated container. Pick this if you'd rather not put a binary on the host — but note the `linux/amd64` emulation caveat on Apple Silicon in the build step.

## Run from the binary

1. **Get the binary.**

    Download the release build for your platform:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    curl -fsSL https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.sh | bash -s -- --server
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    $env:LORE_SERVER=1; irm https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.ps1 | iex
    ```

    <!-- tabs:end -->

2. **Run it with default settings.**

    Start the server:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    ~/.local/bin/loreserver
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    & "$env:USERPROFILE\bin\loreserver.exe"
    ```

    <!-- tabs:end -->

    > [!NOTE]
    > These are the default install locations. If you passed `--install-dir` / `-InstallDir` (or `LORE_INSTALL_DIR`) when running the install script, use that path instead.

    Executed like this and with no config: The server runs in the foreground. It listens on port `41337` for QUIC and gRPC (QUIC on UDP, gRPC on TCP) and on port `41339` for HTTP. It runs using a self-signed certificate regenerated on each restart, and stores the mutable and immutable stores in a temporary directory that the OS will clear on reboot. Great for a proof of concept or a demo, but not somewhere you want to store anything important.

    When done, stop the server (`Ctrl + C`) and proceed to Step 3.

3. **Make it persistent.**

    At a minimum, a persistent Lore server needs its mutable and immutable stores written to a real location and a QUIC certificate that survives restarts. You can define both configurables in a `local.toml` config file that the server layers over its default configs.

    Create the server directory layout:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    mkdir -p /opt/loreserver/config /opt/loreserver/certs
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    New-Item -ItemType Directory -Force C:\loreserver\config, C:\loreserver\certs
    ```

    <!-- tabs:end -->

    Generate a self-signed certificate valid for `localhost`:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    openssl req -x509 -newkey rsa:2048 -nodes \
      -keyout /opt/loreserver/certs/key.pem \
      -out /opt/loreserver/certs/cert.pem \
      -days 365 -subj "/CN=localhost" -addext "subjectAltName=IP:127.0.0.1,DNS:localhost"
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    openssl req -x509 -newkey rsa:2048 -nodes `
      -keyout C:\loreserver\certs\key.pem `
      -out C:\loreserver\certs\cert.pem `
      -days 365 -subj "/CN=localhost" -addext "subjectAltName=IP:127.0.0.1,DNS:localhost"
    ```

    <!-- tabs:end -->

    Create the config file:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    Create `/opt/loreserver/config/local.toml`:

    ```toml
    [server.quic.certificate]
    cert_file = "/opt/loreserver/certs/cert.pem"
    pkey_file = "/opt/loreserver/certs/key.pem"

    [immutable_store.local]
    path = "/opt/loreserver/store"
    flush_delay_seconds = 10

    [mutable_store.local]
    path = "/opt/loreserver/store"
    flush_delay_seconds = 10
    ```

    <!-- tab -->
    **Windows**

    Create `C:\loreserver\config\local.toml`:

    ```toml
    [server.quic.certificate]
    cert_file = "C:\\loreserver\\certs\\cert.pem"
    pkey_file = "C:\\loreserver\\certs\\key.pem"

    [immutable_store.local]
    path = "C:\\loreserver\\store"
    flush_delay_seconds = 10

    [mutable_store.local]
    path = "C:\\loreserver\\store"
    flush_delay_seconds = 10
    ```

    <!-- tabs:end -->

    This is the minimal recommended config for a persistent Lore server: it targets a persistent path for both stores and replaces the ephemeral QUIC certificate with your own.

    With the config now defined, start the server and pass in the location of the config directory:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    ~/.local/bin/loreserver --config /opt/loreserver/config
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    & "$env:USERPROFILE\bin\loreserver.exe" --config C:\loreserver\config
    ```

    <!-- tabs:end -->

    > [!NOTE]
    > For the full set of loreserver configurables and the `local.toml` field reference, see the [Lore Server config reference](../reference/lore-server-config.md).

## Run with Docker

1. **Build the image.**

    This needs Docker (and WSL2 on Windows) and the Lore repository cloned locally. Building the image compiles the server, so it needs several GB of free RAM. From the repository root:

    ```bash
    docker build --platform linux/amd64 -f lore-server/Dockerfile -t lore-server .
    ```

    > [!NOTE]
    > On Apple Silicon or Windows (both arm64 and amd64), build and run with `--platform linux/amd64` as shown. The `linux/arm64` server image targets AWS Graviton3 (SVE), an instruction set those CPUs lack.

2. **Run it with default settings.**

    Map both the TCP and UDP sides of port `41337` and the HTTP port `41339`:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    docker run -d --name lore-server \
      -p 41337:41337/tcp \
      -p 41337:41337/udp \
      -p 41339:41339 \
      lore-server
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    docker run -d --name lore-server `
      -p 41337:41337/tcp `
      -p 41337:41337/udp `
      -p 41339:41339 `
      lore-server
    ```

    <!-- tabs:end -->

    The image stores data under `/data` inside the container and generates an ephemeral certificate, so it starts with no config of its own and runs with auth disabled. Like the binary's defaults, this is meant to be a throwaway instance: the certificate is ephemeral, and the container data is lost when the container is removed.

3. **Make it persistent.**

    At a minimum, a persistent Lore server needs its mutable and immutable stores written to a real location and a QUIC certificate that survives restarts. The built container image is already configured to store loreserver data under `/data` and load additional loreserver config files from `/etc/lore/config`. To make it persistent: create a Docker volume for `/data` so store data survives container removal, generate a QUIC certificate on the host, and create a `local.toml` that points loreserver at it — then bind-mount both into the container.

    Generate a self-signed certificate valid for `localhost`:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem -days 365 \
      -subj "/CN=localhost" -addext "subjectAltName=IP:127.0.0.1,DNS:localhost"
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem -days 365 `
      -subj "/CN=localhost" -addext "subjectAltName=IP:127.0.0.1,DNS:localhost"
    ```

    <!-- tabs:end -->

    Create a `local.toml` that points the QUIC endpoint at it — the image already stores data under `/data`, so the certificate is the only loreserver config override you need:

    ```toml
    [server.quic.certificate]
    cert_file = "/etc/lore/cert.pem"
    pkey_file = "/etc/lore/key.pem"
    ```

    Stop the throwaway container, then run again with the certificate, the overlay, and a host directory mounted for the data:

    <!-- tabs:start -->

    <!-- tab -->
    **macOS / Linux**

    ```bash
    docker stop lore-server && docker rm lore-server
    docker run -d --name lore-server \
      -p 41337:41337/tcp \
      -p 41337:41337/udp \
      -p 41339:41339 \
      -v "$PWD/cert.pem:/etc/lore/cert.pem:ro" \
      -v "$PWD/key.pem:/etc/lore/key.pem:ro" \
      -v "$PWD/local.toml:/etc/lore/config/local.toml:ro" \
      -v ~/lore-data:/data \
      lore-server
    ```

    <!-- tab -->
    **Windows**

    ```powershell
    docker stop lore-server; docker rm lore-server
    docker run -d --name lore-server `
      -p 41337:41337/tcp `
      -p 41337:41337/udp `
      -p 41339:41339 `
      -v "${PWD}/cert.pem:/etc/lore/cert.pem:ro" `
      -v "${PWD}/key.pem:/etc/lore/key.pem:ro" `
      -v "${PWD}/local.toml:/etc/lore/config/local.toml:ro" `
      -v "$env:USERPROFILE\lore-data:/data" `
      lore-server
    ```

    <!-- tabs:end -->

    Your data now persists in the mounted host directory and the certificate is stable across restarts. For everything else you can put in `local.toml`, see the [Lore Server config reference](../reference/lore-server-config.md).

## Check the server is healthy

The same probe works for both paths. In a second terminal:

```bash
curl -i http://127.0.0.1:41339/health_check
```

A healthy server returns `HTTP/1.1 200 OK` with an empty body.

## See also

- [Install the Lore CLI](install-lore-cli.md) — get `lore` on your PATH.
- [Quickstart](../tutorials/quickstart.md) — the full core loop using `lore` cli with a local server.
- [Lore Server config reference](../reference/lore-server-config.md) — every config field plus the multi-host, replication, auth, and plugin-backend surfaces this guide omits: the path to production.
