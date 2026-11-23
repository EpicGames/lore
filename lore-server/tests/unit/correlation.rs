// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::correlation::CorrelationId;

#[test]
fn test_default_correlation_id() {
    let correlation_id = CorrelationId::default();

    uuid::Uuid::try_parse(&correlation_id.0).expect("Inner correlation id should be a valid UUID");
}

mod make_correlation_id_span {
    use lore_server::correlation::span::MakeCorrelationIdSpan;
    use tower_http::trace::MakeSpan;

    use crate::log_capture::CapturedLogs;

    /// The span records a request's path and never its query, which can carry a bearer token.
    #[test]
    fn records_the_path_without_the_query() {
        let (logs, _guard) = CapturedLogs::start();
        let request = http::Request::get("/v1/presigned/repository/address?token=secret-token")
            .body(())
            .unwrap();

        drop(MakeCorrelationIdSpan.make_span(&request));

        let created = logs
            .lines()
            .into_iter()
            .find(|line| line["span"]["name"] == "urc")
            .expect("the span was created");
        assert_eq!(created["span"]["uri"], "/v1/presigned/repository/address");
        assert!(!logs.text().contains("secret-token"), "{}", logs.text());
    }
}
