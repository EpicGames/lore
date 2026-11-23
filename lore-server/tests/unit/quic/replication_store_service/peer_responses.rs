// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;
use std::time::Instant;

use lore_server::quic::replication_store_service::Command;
use lore_server::quic::replication_store_service::ReplicationServiceErrorCode;
use lore_server::quic::replication_store_service::peer_responses::*;
use lore_transport::quic::QuicServiceError;
use lore_transport::quic::response_observer::ObservedResponse;
use opentelemetry::KeyValue;
use opentelemetry::Value;

const THRESHOLD: Duration = Duration::from_millis(20);

fn observer() -> PeerResponseObserver {
    PeerResponseObserver::new(
        [
            KeyValue::new("peer_id", "peer-1"),
            KeyValue::new("peer_locality", "SameRegion"),
            KeyValue::new("replica_type", "read"),
        ],
        Some(THRESHOLD),
    )
}

fn response(
    opcode: Command,
    error_status: Option<u32>,
    to_header: Duration,
    header_to_last_byte: Duration,
) -> ObservedResponse {
    queued_response(
        opcode,
        error_status,
        Duration::ZERO,
        to_header,
        header_to_last_byte,
    )
}

fn queued_response(
    opcode: Command,
    error_status: Option<u32>,
    queue_to_written: Duration,
    to_header: Duration,
    header_to_last_byte: Duration,
) -> ObservedResponse {
    let queued = Instant::now();
    let written = queued + queue_to_written;
    let header = written + to_header;
    ObservedResponse::new(
        opcode as u8,
        error_status,
        0,
        0,
        queued,
        written,
        header,
        header + header_to_last_byte,
    )
}

fn label(labels: &[KeyValue], key: &str) -> Value {
    labels
        .iter()
        .find(|label| label.key.as_str() == key)
        .unwrap_or_else(|| panic!("no {key} label in {labels:?}"))
        .value
        .clone()
}

mod response_labels {
    use super::*;

    #[test]
    fn carries_the_peer_opcode_and_status() {
        let labels = observer().response_labels(&response(
            Command::ImmutableLocalGet,
            None,
            Duration::ZERO,
            Duration::ZERO,
        ));

        assert_eq!(label(&labels, "peer_id"), Value::from("peer-1"));
        assert_eq!(label(&labels, "replica_type"), Value::from("read"));
        assert_eq!(
            label(&labels, OPCODE_LABEL_KEY),
            Value::from("immutable_local_get")
        );
        assert_eq!(label(&labels, RESPONSE_STATUS_LABEL_KEY), Value::from("ok"));
    }

    #[test]
    fn a_miss_is_labelled_by_its_error() {
        let labels = observer().response_labels(&response(
            Command::ImmutableLocalGet,
            Some(ReplicationServiceErrorCode::PayloadNotFound as u32),
            Duration::ZERO,
            Duration::ZERO,
        ));

        assert_eq!(
            label(&labels, RESPONSE_STATUS_LABEL_KEY),
            Value::from("payload_not_found")
        );
    }
}

mod response_status_label {
    use super::*;

    #[test]
    fn a_refusal_any_service_answers_is_labelled_by_its_error() {
        assert_eq!(
            response_status_label(Some(QuicServiceError::SlowDown as u32)),
            "slow_down"
        );
        assert_eq!(
            response_status_label(Some(QuicServiceError::NotFound as u32)),
            "not_found"
        );
    }

    #[test]
    fn a_status_no_service_defines_is_another_error() {
        assert_eq!(response_status_label(Some(299)), "other_error");
    }
}

mod is_slow {
    use super::*;

    #[test]
    fn a_slow_header_or_a_slow_body_is_slow() {
        let observer = observer();
        let late_header = response(
            Command::ImmutableLocalGet,
            None,
            THRESHOLD + Duration::from_millis(1),
            Duration::ZERO,
        );
        let slow_body = response(
            Command::ImmutableLocalGet,
            None,
            Duration::ZERO,
            THRESHOLD + Duration::from_millis(1),
        );

        assert!(observer.is_slow(&late_header));
        assert!(observer.is_slow(&slow_body));
    }

    #[test]
    fn a_request_slow_to_be_written_is_slow() {
        let late_write = queued_response(
            Command::ImmutableLocalGet,
            None,
            THRESHOLD + Duration::from_millis(1),
            Duration::ZERO,
            Duration::ZERO,
        );

        assert!(observer().is_slow(&late_write));
    }

    #[test]
    fn without_a_threshold_nothing_is_slow() {
        let observer = PeerResponseObserver::new(
            [
                KeyValue::new("peer_id", "peer-2"),
                KeyValue::new("peer_locality", "OtherRegion"),
                KeyValue::new("replica_type", "read"),
            ],
            None,
        );

        assert!(!observer.is_slow(&response(
            Command::ImmutableLocalGet,
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )));
    }

    #[test]
    fn a_response_at_the_threshold_is_not_slow() {
        let observer = observer();

        assert!(!observer.is_slow(&response(
            Command::ImmutableLocalGet,
            None,
            THRESHOLD,
            THRESHOLD,
        )));
    }
}
