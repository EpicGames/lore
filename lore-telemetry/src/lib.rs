// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Includes URC-specific extensions: InstrumentProvider, LabelArray, DropRecord, DropTimeMs.
//
// This crate provides lightweight telemetry utilities without tracing dependencies.
// For server-side initialization (TelemetryInitializer, resource detectors, etc.), use urc-server.

mod config;
mod error;
mod metrics;
pub mod observe;
pub mod timer;

pub mod drop_record;
pub mod drop_time;
pub mod execution_state;
pub mod tracing;

use std::borrow::Cow;

pub use config::*;
pub use error::*;
pub use metrics::*;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Gauge;
use opentelemetry::metrics::Histogram;
use opentelemetry::metrics::Meter;
use smallvec::SmallVec;
pub use timer::*;

// URC-specific constants
pub const METRICS_OPERATION_LATENCY_METRIC_NAME: &str = "operation_duration";
pub const METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME: &str = "context";

pub const METRICS_SUCCESS_ATTRIBUTE_NAME: &str = "success";

pub fn create_operation_context_attribute(context: impl Into<Cow<'static, str>>) -> KeyValue {
    KeyValue::new(METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME, context.into())
}

pub type LabelArray = SmallVec<[KeyValue; 8]>;

/// Maps a given numeric value to its associated bucket
pub fn bucket(value: usize, bucket_size: usize) -> Result<usize, TelemetryError> {
    if bucket_size == 0 {
        Err(TelemetryError::internal(format!(
            "Failed to map value to bucket: {value}, {bucket_size}"
        )))
    } else if value.is_multiple_of(bucket_size) {
        Ok(value)
    } else {
        Ok(value + (bucket_size - value % bucket_size))
    }
}

/// Trait for providing telemetry instruments with a consistent namespace
pub trait InstrumentProvider {
    fn namespace(&self) -> &'static str;

    fn meter(&self) -> Meter {
        crate::meter(self.namespace())
    }

    fn labels(&self) -> &[KeyValue] {
        &[]
    }

    fn scope_name(&self, name: impl Into<Cow<'static, str>>) -> impl Into<Cow<'static, str>> {
        format!("{}.{}", self.namespace(), name.into())
    }

    fn latency_histogram_ms(&self, name: impl Into<Cow<'static, str>>) -> Histogram<f64> {
        self.meter()
            .f64_histogram(self.scope_name(name))
            .with_unit("milliseconds")
            .with_boundaries(vec![
                10., 30., 50., 100., 200., 300., 400., 500., 1200., 2000., 5000., 10000., 15000.,
                20000., 30000., 60000.,
            ])
            .build()
    }

    fn size_histogram(&self, name: impl Into<Cow<'static, str>>) -> Histogram<u64> {
        self.meter()
            .u64_histogram(self.scope_name(name))
            .with_unit("bytes")
            .with_boundaries(vec![
                64.,      // 64 B
                256.,     // 256 B
                512.,     // 512 B
                1_024.,   // 1 KiB
                2_048.,   // 2 KiB
                4_096.,   // 4 KiB
                8_192.,   // 8 KiB
                16_384.,  // 16 KiB
                32_768.,  // 32 KiB
                65_536.,  // 64 KiB
                131_072., // 128 KiB
                262_144., // 256 KiB - Fragment Size
            ])
            .build()
    }

    fn length_histogram(
        &self,
        name: impl Into<Cow<'static, str>>,
        boundaries: Vec<f64>,
    ) -> Histogram<u64> {
        self.meter()
            .u64_histogram(self.scope_name(name))
            .with_boundaries(boundaries)
            .build()
    }

    fn counter(&self, name: impl Into<Cow<'static, str>>) -> Counter<u64> {
        self.meter().u64_counter(self.scope_name(name)).build()
    }

    fn gauge(&self, name: impl Into<Cow<'static, str>>) -> Gauge<u64> {
        self.meter().u64_gauge(self.scope_name(name)).build()
    }

    fn get_labels_for_operation_context(
        &self,
        context: impl Into<Cow<'static, str>>,
    ) -> LabelArray {
        let mut labels = SmallVec::new();
        labels.extend(self.labels().iter().cloned());
        labels.push(create_operation_context_attribute(context));
        labels
    }

    fn labels_from_operation_context(
        &self,
        context: impl Into<Cow<'static, str>>,
        labels: &mut [KeyValue],
    ) -> Result<usize, usize> {
        let self_labels = self.labels();
        let base = self_labels.len();
        let required = base + 1;
        if labels.len() < required {
            return Err(required);
        }

        let (head, tail) = labels.split_at_mut(base);
        head.clone_from_slice(self.labels());
        tail[0] = create_operation_context_attribute(context);

        Ok(required)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use opentelemetry::KeyValue;

    use super::*;

    // Test for InstrumentProvider trait
    static TEST_PROVIDER_ATTRIBUTES: LazyLock<[KeyValue; 2]> = LazyLock::new(|| {
        [
            KeyValue::new("key1".to_string(), "value1".to_string()),
            KeyValue::new("key2".to_string(), "value2".to_string()),
        ]
    });

    struct TestProvider {}
    impl InstrumentProvider for TestProvider {
        fn namespace(&self) -> &'static str {
            "test-namespace"
        }

        fn labels(&self) -> &[KeyValue] {
            TEST_PROVIDER_ATTRIBUTES.as_slice()
        }
    }

    #[test]
    fn can_concatenate_context_label() {
        let test_provider = TestProvider {};

        let all_labels = test_provider.get_labels_for_operation_context("my-test-context");
        assert_eq!(all_labels.len(), 3);
        assert_eq!(all_labels[0].key.as_str(), "key1");
        assert_eq!(all_labels[0].value.as_str(), "value1");
        assert_eq!(all_labels[1].key.as_str(), "key2");
        assert_eq!(all_labels[1].value.as_str(), "value2");

        assert_eq!(
            all_labels[2].key.as_str(),
            METRICS_OPERATION_CONTEXT_ATTRIBUTE_NAME
        );
        assert_eq!(all_labels[2].value.as_str(), "my-test-context");
    }
}
