// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_hashicorp::telemetry::NomadResourceDetector;
use opentelemetry::Key;
use opentelemetry::Value;
use opentelemetry_sdk::resource::ResourceDetector;

#[test]
fn test_nomad_resource_detector_with_env_vars() {
    temp_env::with_vars(
        [
            ("NOMAD_ALLOC_ID", Some("test-alloc-id")),
            ("NOMAD_JOB_ID", Some("test-job-id")),
        ],
        || {
            let resource = NomadResourceDetector.detect();
            assert_eq!(resource.len(), 2);

            assert_eq!(
                resource.get(&Key::from_static_str("nomad.alloc.id")),
                Some(Value::from("test-alloc-id"))
            );
            assert_eq!(
                resource.get(&Key::from_static_str("nomad.job.id")),
                Some(Value::from("test-job-id"))
            );
        },
    );
}

#[test]
fn test_nomad_resource_detector_with_missing_env_vars() {
    // Unsets every variable the detector reads, which a run inside a Nomad job has set.
    temp_env::with_vars_unset(["NOMAD_ALLOC_ID", "NOMAD_JOB_ID"], || {
        let resource = NomadResourceDetector.detect();

        assert_eq!(resource.len(), 0);
    });
}
