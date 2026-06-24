# Guide: Running Lore unauthenticated (internal/VPN only)

This is the simplest way to run Lore for a small team behind a firewall or VPN. Relying on the network for security and user-provided strings for identity.

## 1. Server Configuration

In your server's `local.toml`, ensure the following is set:

```toml
[server.quic]
# Allow any client to connect without a certificate
verify_client_certs = false

[server.quic.certificate]
# The server's own certificate (required for TLS/QUIC)
cert_file = "/opt/loreserver/certs/cert.pem"
pkey_file = "/opt/loreserver/certs/key.pem"
```

## 2. Restart the service

After editing the config, restart the server:

```bash
sudo systemctl restart loreserver
```

## 3. Client usage

Since there is no "login," users connect by pointing their Lore client to your server IP/hostname.

Users should follow the [Setting Up Your Lore Identity](./setup-identity.md) guide to ensure their names show up in the history.
