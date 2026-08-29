# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Tests that the grpc_internal server enforces mTLS.

When `certificate.cert_chain` is set (the CA cert used to verify client
certificates), the internal gRPC server must:

  - Reject clients that present no client certificate at the TLS handshake.
  - Accept clients that present a valid CA-signed client certificate and
    return a gRPC-level response (UNIMPLEMENTED — AdminService is not
    registered on the internal server).

The PKI used here is generated fresh for every test run using the
`cryptography` library so the suite needs no pre-generated cert files.
"""

import datetime
import ipaddress
import logging
import os

import grpc
import pytest

from lore_server import (
    _kill_server_by_pid,
    _wait_for_grpc_port,
    allocate_free_port,
    generate_server_config,
    launch_lore_server,
)

logger = logging.getLogger(__name__)

# AdminService is not registered on the grpc_internal server.  A successful
# TLS handshake followed by a call to this method returns UNIMPLEMENTED
# rather than a transport error, which distinguishes "reached gRPC" from
# "rejected at TLS".
_ADMIN_SERVER_INFO = "/urc.rpc.AdminService/ServerInfo"


# ---------------------------------------------------------------------------
# PKI generation
# ---------------------------------------------------------------------------


def _generate_pki(pki_dir):
    """Generate an ephemeral PKI: CA, server cert+key, client cert+key.

    Returns a dict of PEM bytes and Path objects that the caller can pass
    directly to grpc.ssl_channel_credentials and to local.toml.
    """
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Lore Test CA")])
    ca_cert = (
        x509.CertificateBuilder()
        .subject_name(ca_name)
        .issuer_name(ca_name)
        .public_key(ca_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(datetime.datetime.now(datetime.timezone.utc))
        .not_valid_after(datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .add_extension(
            x509.KeyUsage(
                key_cert_sign=True,
                crl_sign=True,
                digital_signature=False,
                content_commitment=False,
                key_encipherment=False,
                data_encipherment=False,
                key_agreement=False,
                encipher_only=False,
                decipher_only=False,
            ),
            critical=True,
        )
        .sign(ca_key, hashes.SHA256())
    )

    def _leaf(common_name, san=None):
        key = ec.generate_private_key(ec.SECP256R1())
        builder = (
            x509.CertificateBuilder()
            .subject_name(
                x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])
            )
            .issuer_name(ca_cert.subject)
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(datetime.datetime.now(datetime.timezone.utc))
            .not_valid_after(datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(days=1))
        )
        if san:
            builder = builder.add_extension(
                x509.SubjectAlternativeName(san), critical=False
            )
        cert = builder.sign(ca_key, hashes.SHA256())
        return cert, key

    server_cert, server_key = _leaf(
        "localhost",
        [
            x509.DNSName("localhost"),
            x509.IPAddress(ipaddress.ip_address("127.0.0.1")),
        ],
    )
    client_cert, client_key = _leaf("lore-test-client")

    def _pem_cert(c):
        return c.public_bytes(serialization.Encoding.PEM)

    def _pem_key(k):
        return k.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )

    ca_cert_pem = _pem_cert(ca_cert)
    ca_cert_path = pki_dir / "ca.pem"
    server_cert_path = pki_dir / "server_cert.pem"
    server_key_path = pki_dir / "server_key.pem"

    ca_cert_path.write_bytes(ca_cert_pem)
    server_cert_path.write_bytes(_pem_cert(server_cert))
    server_key_path.write_bytes(_pem_key(server_key))

    return {
        "ca_cert_path": ca_cert_path,
        "ca_cert_pem": ca_cert_pem,
        "server_cert_path": server_cert_path,
        "server_key_path": server_key_path,
        "client_cert_pem": _pem_cert(client_cert),
        "client_key_pem": _pem_key(client_key),
    }


# ---------------------------------------------------------------------------
# gRPC helper
# ---------------------------------------------------------------------------


def _grpc_status_code(target, credentials, timeout=5.0):
    """Call AdminService/ServerInfo and return the gRPC status code.

    A TLS handshake failure surfaces as UNAVAILABLE before any gRPC message
    is exchanged.  UNIMPLEMENTED means TLS succeeded and the call reached the
    server's gRPC dispatcher.
    """
    with grpc.secure_channel(target, credentials) as channel:
        stub = channel.unary_unary(
            _ADMIN_SERVER_INFO,
            request_serializer=lambda _: b"",
            response_deserializer=lambda b: b,
        )
        try:
            stub(None, timeout=timeout)
            return grpc.StatusCode.OK
        except grpc.RpcError as exc:
            return exc.code()


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


@pytest.mark.smoke
@pytest.mark.xdist_group("grpc_internal_mtls")
class TestGrpcInternalMtls:
    """The grpc_internal server must enforce mTLS when cert_chain is configured.

    A client that presents no client certificate must be rejected at the TLS
    handshake.  A client that presents a valid CA-signed client certificate
    must get past TLS and receive a gRPC-level response.
    """

    @pytest.fixture(scope="class")
    def pki(self, tmp_path_factory):
        return _generate_pki(tmp_path_factory.mktemp("pki"))

    @pytest.fixture(scope="class")
    def mtls_server_config(self, request, tmp_path_factory, pki):
        shared_port = allocate_free_port()
        ports = {
            "quic": shared_port,
            "grpc": shared_port,
            "http": allocate_free_port(),
            "internal": allocate_free_port(),
        }
        server_root, server_env = generate_server_config(
            request, tmp_path_factory, ports
        )

        # Enable the internal gRPC server and point it at the generated PKI.
        # Writing to local.toml mirrors the pattern in test_forwarded_requests.py.
        server_env["LORE__SERVER__GRPC_INTERNAL__ENABLED"] = "true"
        with open(
            os.path.join(server_root, "lore-server", "config", "local.toml"),
            "a",
            encoding="utf-8",
        ) as f:
            f.write("[server.grpc_internal.certificate]\n")
            f.write(f'cert_file = "{pki["server_cert_path"].as_posix()}"\n')
            f.write(f'pkey_file = "{pki["server_key_path"].as_posix()}"\n')
            f.write(f'cert_chain = "{pki["ca_cert_path"].as_posix()}"\n')

        return server_root, server_env, ports

    @pytest.fixture(scope="class")
    def mtls_server(self, mtls_server_config, lore_server_executable_path):
        server_root, server_env, ports = mtls_server_config
        server_proc, log_path, log_fd = launch_lore_server(
            server_root, server_env, lore_server_executable_path
        )
        # launch_lore_server waits on the public gRPC port; also wait on the
        # internal port before the tests probe it.
        _wait_for_grpc_port("127.0.0.1", ports["internal"])
        yield server_proc
        _kill_server_by_pid(
            server_proc.pid, log_path, label="mtls grpc_internal server"
        )
        log_fd.close()

    def test_client_without_cert_is_rejected(
        self, mtls_server, mtls_server_config, pki
    ):
        """Connecting without a client certificate must fail at the TLS handshake."""
        _, _, ports = mtls_server_config
        target = f"127.0.0.1:{ports['internal']}"

        # Trust the server's CA but send no client certificate.
        creds = grpc.ssl_channel_credentials(root_certificates=pki["ca_cert_pem"])
        code = _grpc_status_code(target, creds)

        assert code == grpc.StatusCode.UNAVAILABLE, (
            f"Expected UNAVAILABLE (TLS handshake rejection) when no client cert is "
            f"presented to an mTLS-only endpoint, got {code}"
        )

    def test_client_with_valid_cert_reaches_grpc(
        self, mtls_server, mtls_server_config, pki
    ):
        """A client presenting a valid CA-signed cert gets past TLS and receives a gRPC response."""
        _, _, ports = mtls_server_config
        target = f"127.0.0.1:{ports['internal']}"

        creds = grpc.ssl_channel_credentials(
            root_certificates=pki["ca_cert_pem"],
            private_key=pki["client_key_pem"],
            certificate_chain=pki["client_cert_pem"],
        )
        code = _grpc_status_code(target, creds)

        # AdminService is not registered on the internal server, so the call
        # returns UNIMPLEMENTED — proof that TLS succeeded and gRPC dispatched it.
        assert code == grpc.StatusCode.UNIMPLEMENTED, (
            f"Expected UNIMPLEMENTED (gRPC reached after mTLS handshake) with a "
            f"valid client cert, got {code}"
        )
