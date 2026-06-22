// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use lore_base::lore_spawn;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use tokio::time::MissedTickBehavior;
use tracing::debug;
use tracing::warn;
use x509_parser::prelude::*;

/// Information extracted from a TLS certificate for metrics reporting.
#[derive(Debug, Clone)]
pub struct CertificateInfo {
    /// Path to the certificate file.
    pub cert_path: PathBuf,
    /// Certificate subject (e.g., CN=example.com).
    pub subject: String,
    /// Certificate serial number in hex format.
    pub serial: String,
    /// Unix timestamp when the certificate expires.
    pub expiry_timestamp: i64,
}

struct CertificateMetricsInstrumentProvider;

impl InstrumentProvider for CertificateMetricsInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.certs"
    }
}

struct CertificateMetricsInstruments {
    expiry_seconds: Gauge<i64>,
}

impl CertificateMetricsInstruments {
    fn new() -> Self {
        let instrument_provider = CertificateMetricsInstrumentProvider;
        let meter = instrument_provider.meter();

        Self {
            expiry_seconds: meter
                .i64_gauge(instrument_provider.scope_name("expiry_seconds"))
                .with_unit("seconds")
                .with_description("Seconds until certificate expires (negative if expired)")
                .build(),
        }
    }

    pub fn instance() -> &'static Self {
        static INSTANCE: OnceLock<CertificateMetricsInstruments> = OnceLock::new();
        INSTANCE.get_or_init(Self::new)
    }
}

/// Parse a certificate file and extract metadata for metrics reporting.
///
/// When the certificate file contains a chain (multiple certificates), only the leaf certificate
/// (first in the file) is parsed and reported. This is the server's identity certificate and the
/// most relevant for operational monitoring.
pub fn parse_certificate_info(cert_path: &Path) -> Option<CertificateInfo> {
    let certs: Vec<CertificateDer<'static>> = match CertificateDer::pem_file_iter(cert_path) {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(e) => {
            warn!(
                cert_path = %cert_path.display(),
                error = %e,
                "Failed to read certificate file for metrics"
            );
            return None;
        }
    };

    // The first certificate should be the leaf cert, so that's what we care about.
    let cert_der = certs.first()?;

    let (_, cert) = match X509Certificate::from_der(cert_der.as_ref()) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(
                cert_path = %cert_path.display(),
                error = %e,
                "Failed to parse certificate"
            );
            return None;
        }
    };

    let subject = cert.subject().to_string();
    let serial = cert.serial.to_str_radix(16).to_lowercase();
    let expiry_timestamp = cert.validity().not_after.timestamp();

    Some(CertificateInfo {
        cert_path: cert_path.to_path_buf(),
        subject,
        serial,
        expiry_timestamp,
    })
}

fn record_certificate_expiry(expiry_timestamp: i64, labels: &[KeyValue]) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let seconds_until_expiry = expiry_timestamp - now;

    debug!(seconds_until_expiry, "Recording certificate expiry metric");

    CertificateMetricsInstruments::instance()
        .expiry_seconds
        .record(seconds_until_expiry, labels);
}

/// Start periodic certificate expiry metric reporting.
///
/// Spawns a background task that records the time remaining (in seconds) until the certificate
/// expires. The metric can go negative if the certificate has already expired.
pub fn start_certificate_metrics(cert_info: CertificateInfo, interval: Duration) {
    // Pre-compute labels once since certificate info never changes
    let labels = [
        KeyValue::new("cert_path", cert_info.cert_path.display().to_string()),
        KeyValue::new("subject", cert_info.subject),
        KeyValue::new("serial", cert_info.serial),
        KeyValue::new("expiry_timestamp", cert_info.expiry_timestamp),
    ];
    let expiry_timestamp = cert_info.expiry_timestamp;

    // Record immediately on startup
    record_certificate_expiry(expiry_timestamp, &labels);

    lore_spawn!(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            record_certificate_expiry(expiry_timestamp, &labels);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    // Self-signed test certificate (CN=localhost:8443, expires 2025-09-07)
    const TEST_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIDWTCCAkGgAwIBAgIUIjsbV4maoFQusbqM5oaXqwznp/kwDQYJKoZIhvcNAQEL
BQAwPDEXMBUGA1UEAwwObG9jYWxob3N0Ojg0NDMxFDASBgNVBAoMC1NlbGYgc2ln
bmVkMQswCQYDVQQGEwJDSDAeFw0yNDA5MDcxMTU2NDVaFw0yNTA5MDcxMTU2NDVa
MDwxFzAVBgNVBAMMDmxvY2FsaG9zdDo4NDQzMRQwEgYDVQQKDAtTZWxmIHNpZ25l
ZDELMAkGA1UEBhMCQ0gwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCS
qRG3I7I6lswb1uFc3vukOAJo0XK3wvf35/rr1n+yEI0gTtRmDe57MW9PZ5NdWD2P
04xMOjdvBT3Ih+QOQ3MViKc0bXtXDfxy+P0s/2qqw8wdk1Vjt23G/1ARO88NHib2
YdPg4dUsfsLOUxb7yZYdiTEBLbuUQWYs7C7sTs8ARYukbpBlWICCR1ujJT59CwcU
Pfz6Web//aLk9cfDp3mETU2fr9i0FecSm8lkrsSSJ0d6X49PKwKHBNM1puKPjh0Z
CIeuCWb/PF0YC/tylcRbWkRMdw4yhUZjj2QLa89uInxbQE2mym6pvkj/NwCwPNxI
yBNH5ovgdk7xlPK4RTBLAgMBAAGjUzBRMB0GA1UdDgQWBBQ8LECO5fTmnDZ6rx/W
+fXfHfdHOTAfBgNVHSMEGDAWgBQ8LECO5fTmnDZ6rx/W+fXfHfdHOTAPBgNVHRMB
Af8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQCLlFJfM6KXSg1lTk6GRjN5lV2Z
J4ckc89Z2UUUzaWl3w9UzRVJWZeR57OUiBBoiLAZhetIrbYO2nx5YwKJmmDomtfI
OXCWoqRrur4i2mSNot70H4rNWzkbT9dA1x96GRyYZXr8NiXqqcwnRmDi7PCCkweV
z1OZyZH2WV+gXsVSIyGc9OkeB54aXQVLcq1hvqrqPgcN+Lz0/t9kCO/GuFgVdSYd
qDaypyqy8YAKigKSgU5Xs2gfL28Nq0bTGOTy9/fqls8ueMblEm+e5i/4FowvsINa
eFIZ7GeXtWFz+ftM1FrUvXA5XESE0H9iNZklf0dnJXwheUXRpn06bXysEyk4
-----END CERTIFICATE-----
"#;

    #[test]
    fn test_parse_valid_certificate() {
        let temp_dir = std::env::temp_dir();
        let cert_path = temp_dir.join("test_cert_metrics.pem");

        let mut file = std::fs::File::create(&cert_path).unwrap();
        file.write_all(TEST_CERT_PEM.as_bytes()).unwrap();

        let result = parse_certificate_info(&cert_path);
        std::fs::remove_file(&cert_path).ok();

        let info = result.expect("should parse valid certificate");
        assert_eq!(info.cert_path, cert_path);
        assert!(info.subject.contains("localhost:8443"));
        assert!(!info.serial.is_empty());
        // Expires 2025-09-07 11:56:45 UTC
        assert_eq!(info.expiry_timestamp, 1757246205);
    }

    #[test]
    fn test_parse_missing_certificate() {
        let result = parse_certificate_info(Path::new("/nonexistent/path/cert.pem"));
        assert!(result.is_none());
    }
}
