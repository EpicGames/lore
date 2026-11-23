// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::cluster::peer::Locality;
use lore_revision::store::composite::ReplicaType;
use lore_server::quic::initial_cwnd::DEFAULT_PEER_INITIAL_CWND_BYTES;
use lore_server::quic::initial_cwnd::InitialCwndBytes;
use lore_server::store::replica_factory::*;

fn parse(initial_cwnd: &str) -> Result<ReplicaFactorySettings, toml::de::Error> {
    toml::from_str(&format!("client_message_buffer = 5000\n{initial_cwnd}"))
}

mod replica_factory_settings {
    use super::*;

    #[test]
    fn omitted_initial_cwnd_is_the_peer_default() {
        let settings = parse("").expect("valid settings");

        assert_eq!(settings.initial_cwnd_bytes, DEFAULT_PEER_INITIAL_CWND_BYTES);
    }

    #[test]
    fn minimum_initial_cwnd_is_accepted() {
        let settings = parse(&format!("initial_cwnd_bytes = {}", InitialCwndBytes::MIN))
            .expect("valid settings");

        assert_eq!(settings.initial_cwnd_bytes.get(), InitialCwndBytes::MIN);
    }

    #[test]
    fn initial_cwnd_below_minimum_is_rejected() {
        let error = parse(&format!(
            "initial_cwnd_bytes = {}",
            InitialCwndBytes::MIN - 1
        ))
        .expect_err("below the minimum");

        assert!(
            error.to_string().contains(&format!(
                "initial_cwnd_bytes must be at least {}",
                InitialCwndBytes::MIN
            )),
            "unexpected error: {error}"
        );
    }
}

mod peer_user_agent_component {
    use super::*;

    #[test]
    fn a_peer_read_and_write_connection_identify_differently() {
        for locality in [Locality::SameRegion, Locality::OtherRegion] {
            assert_ne!(
                peer_user_agent_component(locality, ReplicaType::Read),
                peer_user_agent_component(locality, ReplicaType::Write),
            );
        }
    }

    #[test]
    fn names_the_locality_and_the_role() {
        assert_eq!(
            peer_user_agent_component(Locality::SameRegion, ReplicaType::Read),
            "replication-same-region-read"
        );
        assert_eq!(
            peer_user_agent_component(Locality::OtherRegion, ReplicaType::Write),
            "replication-other-region-write"
        );
    }
}

mod peer_slow_response_threshold {
    use std::time::Duration;

    use super::*;

    const THRESHOLD: Duration = Duration::from_millis(20);

    #[test]
    fn a_same_region_peer_is_logged_from_the_threshold() {
        assert_eq!(
            peer_slow_response_threshold(Locality::SameRegion, Some(THRESHOLD)),
            Some(THRESHOLD)
        );
    }

    #[test]
    fn a_same_region_peer_is_not_logged_without_a_threshold() {
        assert_eq!(
            peer_slow_response_threshold(Locality::SameRegion, None),
            None
        );
    }

    #[test]
    fn an_other_region_peer_is_never_logged() {
        assert_eq!(
            peer_slow_response_threshold(Locality::OtherRegion, Some(THRESHOLD)),
            None
        );
    }
}
