// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use http::Method;
use http::StatusCode;
use lore_aws::aws::http::HttpClient;
use lore_aws::aws::http::HttpRequest;
use lore_aws::clients::HttpClientSettings;

use crate::test_server::StubResponse;
use crate::test_server::StubServer;

/// The store's traffic goes to the service directly, whatever proxy the environment names for other
/// egress.
#[tokio::test]
#[serial_test::serial]
async fn the_service_client_ignores_the_environments_proxy() {
    let server = StubServer::start(vec![StubResponse::ok("{}")]).await;

    temp_env::async_with_vars(
        [
            ("HTTP_PROXY", Some("http://127.0.0.1:1")),
            ("http_proxy", Some("http://127.0.0.1:1")),
            ("ALL_PROXY", None),
            ("all_proxy", None),
            ("NO_PROXY", None),
            ("no_proxy", None),
        ],
        async {
            let client = HttpClient::new(&HttpClientSettings {
                pool_idle_timeout_seconds: 5,
                nodelay: true,
                max_connections: None,
                connection_acquire_timeout_millis: None,
            })
            .expect("a client builds");

            let response = client
                .send(
                    HttpRequest::new(Method::GET, server.endpoint()),
                    Duration::from_secs(5),
                )
                .await
                .expect("the request reaches the service rather than the proxy");

            assert_eq!(response.status, StatusCode::OK);
        },
    )
    .await;
}
