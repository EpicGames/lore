// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::store::replica_factory::*;

fn parse(initial_cwnd: &str) -> Result<ReplicaFactorySettings, toml::de::Error> {
    toml::from_str(&format!("client_message_buffer = 5000\n{initial_cwnd}"))
}

mod replica_factory_settings {
    use super::*;

    #[test]
    fn omitted_initial_cwnd_is_one_mebibyte() {
        let settings = parse("").expect("valid settings");

        assert_eq!(settings.initial_cwnd_bytes, 1024 * 1024);
    }

    #[test]
    fn minimum_initial_cwnd_is_accepted() {
        let settings = parse(&format!(
            "initial_cwnd_bytes = {MIN_REPLICA_INITIAL_CWND_BYTES}"
        ))
        .expect("valid settings");

        assert_eq!(settings.initial_cwnd_bytes, MIN_REPLICA_INITIAL_CWND_BYTES);
    }

    #[test]
    fn initial_cwnd_below_minimum_is_rejected() {
        parse(&format!(
            "initial_cwnd_bytes = {}",
            MIN_REPLICA_INITIAL_CWND_BYTES - 1
        ))
        .expect_err("below the minimum");
    }
}
