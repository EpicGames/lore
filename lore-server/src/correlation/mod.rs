// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod layer;
pub mod service;
pub mod span;

use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;
use std::ops::Deref;

/// Represents an Epic Correlation ID, which is currently a UUID v4
#[derive(Clone)]
pub struct CorrelationId(pub String);

impl CorrelationId {
    pub fn new(correlation_id: impl Into<String>) -> Self {
        Self(correlation_id.into())
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new(uuid::Uuid::new_v4())
    }
}

impl Deref for CorrelationId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl Display for CorrelationId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Debug for CorrelationId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

#[cfg(test)]
mod tests {
    use crate::correlation::CorrelationId;

    #[test]
    fn test_default_correlation_id() {
        let correlation_id = CorrelationId::default();

        uuid::Uuid::try_parse(&correlation_id.0)
            .expect("Inner correlation id should be a valid UUID");
    }
}
