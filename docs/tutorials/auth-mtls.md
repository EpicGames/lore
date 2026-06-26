# Guide: High-security mTLS authentication

mTLS (Mutual TLS) requires every user to have a unique certificate signed by a Root Authority (CA) that the server trusts. This is the most secure method but has the highest management overhead.

## 1. Create a root CA

If you don't have one, create a CA to sign user certificates:

```bash
openssl genrsa -out ca-key.pem 2048
openssl req -new -x509 -nodes -days 3650 -key ca-key.pem -out ca.pem
```

## 2. Server Configuration

Point the server to the CA file so it knows which clients to trust:

```toml
[server.quic]
verify_client_certs = true

[server.quic.certificate]
cert_file = "/opt/loreserver/certs/cert.pem"
pkey_file = "/opt/loreserver/certs/key.pem"
cert_chain = "/opt/loreserver/certs/ca.pem" # The Trusted CA
```

## 3. Generate user certificates

For **each person** on the team, you must generate a keypair and sign it:

1. **User Key:** `openssl genrsa -out user-key.pem 2048`
2. **Request:** `openssl req -new -key user-key.pem -out user.csr`
3. **Sign:** `openssl x509 -req -in user.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out user-cert.pem -days 365`

## 4. Distribute to user

The user must place `user-cert.pem` and `user-key.pem` on their machine and configure their Lore client to use them (refer to Lore CLI advanced config).
