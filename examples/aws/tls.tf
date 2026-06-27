# =============================================================================
# TLS — CA + server certificate for QUIC and gRPC between nodes
#
# The public QUIC endpoint generates an ephemeral cert if none is configured,
# but the internal replication endpoint (quic_internal) requires an explicit
# certificate. We generate a CA + server cert here so both primary and edge
# can establish trusted QUIC connections.
# =============================================================================

resource "tls_private_key" "ca" {
  algorithm   = "ECDSA"
  ecdsa_curve = "P384"
}

resource "tls_self_signed_cert" "ca" {
  private_key_pem = tls_private_key.ca.private_key_pem

  subject {
    common_name  = "${local.name}-ca"
    organization = "Lore Example"
  }

  validity_period_hours = 8760
  is_ca_certificate     = true
  allowed_uses          = ["cert_signing", "crl_signing"]
}

resource "tls_private_key" "server" {
  algorithm   = "ECDSA"
  ecdsa_curve = "P384"
}

resource "tls_cert_request" "server" {
  private_key_pem = tls_private_key.server.private_key_pem

  subject {
    common_name  = "lore-server"
    organization = "Lore Example"
  }

  # Cloud Map DNS names used by clients and inter-node communication
  dns_names = ["primary.${local.name}.internal", "edge.${local.name}.internal", "localhost"]
}

resource "tls_locally_signed_cert" "server" {
  cert_request_pem   = tls_cert_request.server.cert_request_pem
  ca_private_key_pem = tls_private_key.ca.private_key_pem
  ca_cert_pem        = tls_self_signed_cert.ca.cert_pem

  validity_period_hours = 8760
  allowed_uses          = ["digital_signature", "key_encipherment", "server_auth"]
}

# Fullchain = server cert + CA cert
locals {
  fullchain_pem = "${tls_locally_signed_cert.server.cert_pem}${tls_self_signed_cert.ca.cert_pem}"
  server_key    = tls_private_key.server.private_key_pem
  ca_pem        = tls_self_signed_cert.ca.cert_pem
}

resource "aws_secretsmanager_secret" "tls" {
  name_prefix = "${local.name}-tls-"
  tags        = local.tags
}

resource "aws_secretsmanager_secret_version" "tls" {
  secret_id = aws_secretsmanager_secret.tls.id
  secret_string = jsonencode({
    fullchain = local.fullchain_pem
    key       = local.server_key
    ca        = local.ca_pem
  })
}
