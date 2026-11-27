// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub fn normalize_remote_url(remote_url: &str) -> &str {
    let url = remote_url.trim_end_matches('/');
    // Strip protocol specifier (e.g. "lore://", "urc://", "grpc://") so that
    // URLs differing only in scheme still match the same shared store entry.
    if let Some(rest) = url.split_once("://") {
        rest.1
    } else {
        url
    }
}

/// Whether two remote URLs name the same server: the same host, compared without case, and the
/// same port where both name one.
///
/// The scheme and path are ignored, and so is a port named on one side only, which the scheme's
/// default fills in. Hosts are not resolved, so `localhost` and `127.0.0.1` differ. Unlike
/// [`normalize_remote_url`], which keys shared stores by the URL as written, this matches any
/// spelling of a server.
#[lore_macro::test_pub]
pub(crate) fn same_server(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (
        lore_transport::parse_remote_url(a),
        lore_transport::parse_remote_url(b),
    ) else {
        return false;
    };
    let (Some(host_a), Some(host_b)) = (a.host_str(), b.host_str()) else {
        return false;
    };
    host_a.eq_ignore_ascii_case(host_b)
        && match (a.port(), b.port()) {
            (Some(port_a), Some(port_b)) => port_a == port_b,
            _ => true,
        }
}
