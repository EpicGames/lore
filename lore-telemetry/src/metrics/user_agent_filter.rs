// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use rand::random;
use regex::RegexSet;
use tracing::info;

/// Recorded when a user agent was supplied but matched none of the configured patterns.
///
/// Shared by metric labels and tracing span fields so the two can be correlated; a caller that
/// substitutes its own string breaks that join silently.
pub const USER_AGENT_UNKNOWN: &str = "<unknown>";

/// Recorded when no user agent was supplied at all, in place of calling
/// [`normalize`](UserAgentFilter::normalize).
pub const USER_AGENT_NONE: &str = "<none>";

pub enum NormalizeOutput {
    KnownAgent(Arc<str>),
    Unknown,
}

/// Classifies HTTP `User-Agent` header values for use as metric labels.
///
/// When no patterns are configured (the default), every value is used as-is.
/// When patterns are configured, values that match at least one pattern are
/// used as-is; values that match none are replaced with `"<unknown>"`.
///
/// An optional [`unknown_sample_rate`](UserAgentFilter::with_unknown_sample_rate)
/// (default `0.0`) controls what fraction of unrecognised agents are recorded
/// with their actual value instead of `"<unknown>"`, allowing operators to
/// identify unexpected clients without unbounding metric cardinality.
///
/// Absent headers are recorded as `"<none>"` by the caller before calling
/// [`normalize`](UserAgentFilter::normalize).
pub struct UserAgentFilter {
    patterns: RegexSet,
    unknown_sample_rate: f64,
}

impl UserAgentFilter {
    /// Compiles `patterns` into a filter with a zero sample rate.
    ///
    /// Returns an error if any pattern is not a valid regular expression.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self, regex::Error> {
        Ok(Self {
            patterns: RegexSet::new(patterns)?,
            unknown_sample_rate: 0.0,
        })
    }

    /// Sets the fraction of unrecognized user-agents that are sampled into
    /// the log at `info` level.
    pub fn with_unknown_sample_rate(mut self, rate: f64) -> Self {
        self.unknown_sample_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// Returns the appropriate metric label for `value`.
    ///
    /// - If no patterns are configured, `value` is returned as-is.
    /// - If patterns are configured and `value` matches any of them, it is
    ///   returned as-is.
    pub fn normalize(&self, value: &str) -> NormalizeOutput {
        if self.patterns.is_empty() || self.patterns.is_match(value) {
            return NormalizeOutput::KnownAgent(Arc::from(value));
        }

        NormalizeOutput::Unknown
    }

    /// Given the user agent that is unknown, log it
    /// according to our sample rate
    pub fn sample_unknown_agent(&self, value: &str) {
        if self.unknown_sample_rate > 0.0 && random::<f64>() < self.unknown_sample_rate {
            info!(user_agent = value, "unknown user agent sampled");
        }
    }
}

impl Default for UserAgentFilter {
    fn default() -> Self {
        Self {
            patterns: RegexSet::empty(),
            unknown_sample_rate: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_known(output: NormalizeOutput, expected: &str) {
        match output {
            NormalizeOutput::KnownAgent(label) => assert_eq!(&*label, expected),
            NormalizeOutput::Unknown => panic!("expected KnownAgent, got Unknown"),
        }
    }

    fn assert_unknown(output: NormalizeOutput) {
        assert!(matches!(output, NormalizeOutput::Unknown));
    }

    #[test]
    fn no_patterns_allows_all() {
        let filter = UserAgentFilter::new::<String>(&[]).unwrap();
        assert_known(filter.normalize("my-client/1.0"), "my-client/1.0");
    }

    #[test]
    fn default_filter_allows_all() {
        let filter = UserAgentFilter::default();
        assert_known(filter.normalize("anything"), "anything");
    }

    #[test]
    fn matching_pattern_passes_through() {
        let filter = UserAgentFilter::new(&["my-client/.*"]).unwrap();
        assert_known(filter.normalize("my-client/1.0"), "my-client/1.0");
    }

    #[test]
    fn non_matching_maps_to_unknown() {
        let filter = UserAgentFilter::new(&["my-client/.*"]).unwrap();
        assert_unknown(filter.normalize("other-client/1.0"));
    }

    #[test]
    fn multiple_patterns_any_match_passes_through() {
        let filter = UserAgentFilter::new(&["my-client/.*", "other-client/.*"]).unwrap();
        assert_known(filter.normalize("other-client/1.0"), "other-client/1.0");
    }

    #[test]
    fn invalid_regex_returns_error() {
        assert!(UserAgentFilter::new(&["[invalid"]).is_err());
    }

    #[test]
    fn partial_pattern_match_passes_through() {
        let filter = UserAgentFilter::new(&["my-client"]).unwrap();
        assert_known(filter.normalize("my-client/1.0"), "my-client/1.0");
    }

    #[test]
    fn zero_sample_rate_always_unknown() {
        let filter = UserAgentFilter::new(&["my-client/.*"])
            .unwrap()
            .with_unknown_sample_rate(0.0);
        for _ in 0..100 {
            assert_unknown(filter.normalize("other-client/1.0"));
        }
    }

    #[test]
    fn full_sample_rate_still_returns_unknown_label() {
        // Sampling logs the value but the metric label is always <unknown>.
        let filter = UserAgentFilter::new(&["my-client/.*"])
            .unwrap()
            .with_unknown_sample_rate(1.0);
        assert_unknown(filter.normalize("other-client/1.0"));
    }

    #[test]
    fn sample_rate_clamped_above_one() {
        let filter = UserAgentFilter::new(&["my-client/.*"])
            .unwrap()
            .with_unknown_sample_rate(2.0);
        assert_unknown(filter.normalize("other-client/1.0"));
    }

    #[test]
    fn sample_rate_clamped_below_zero() {
        let filter = UserAgentFilter::new(&["my-client/.*"])
            .unwrap()
            .with_unknown_sample_rate(-1.0);
        assert_unknown(filter.normalize("other-client/1.0"));
    }
}
