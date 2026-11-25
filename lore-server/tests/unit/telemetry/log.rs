// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_server::telemetry::log::*;
use opentelemetry_sdk::logs::LogBatch;
use opentelemetry_sdk::logs::LogExporter as _;

use crate::telemetry::assert_connects_while_the_core_blocking_pool_is_held;

mod log_exporter {
    use super::*;

    /// The log exporter reaches a collector named by hostname while every core blocking thread is
    /// held.
    #[test]
    fn connects_by_hostname_while_the_core_blocking_pool_is_held() {
        assert_connects_while_the_core_blocking_pool_is_held(|config| {
            let exporter = log_exporter(config).expect("building the exporter");
            Box::new(move || {
                let _ = futures::executor::block_on(exporter.export(LogBatch::new(&[])));
            })
        });
    }
}
