// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_telemetry::user_agent_filter::NormalizeOutput;
use lore_telemetry::user_agent_filter::USER_AGENT_UNKNOWN;
use lore_telemetry::user_agent_filter::UserAgentFilter;

use crate::protocol::attribute_map::AttributeMap;
use crate::protocol::storage::messages::MessageParseError;

pub struct UserAgentValue(pub Arc<str>);

/// Parsed representation of a `ClientIdentify` protocol message.
///
/// `user_agent` is `None` when the raw bytes were empty, exceeded the `USER_AGENT_MAX_BYTES`-byte
/// limit, or contained anything outside printable ASCII — all silently ignored so that
/// a malformed header never breaks the connection.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientIdentify {
    pub user_agent: Option<String>,
}

const USER_AGENT_MAX_BYTES: usize = 256;

/// Returns `true` when every byte is printable ASCII (`0x20`–`0x7E`), so control characters,
/// `DEL` and any byte with the high bit set are rejected. Keeps the value safe to emit as a
/// telemetry attribute and log field.
fn is_printable_ascii(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ')
}

impl ClientIdentify {
    /// Parses raw bytes from the wire into a `ClientIdentify`.
    ///
    /// Validation mirrors `Correlate::parse`: invalid input is silently
    /// discarded rather than surfaced as a protocol error.
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        let user_agent = if bytes.is_empty()
            || bytes.len() > USER_AGENT_MAX_BYTES
            || !is_printable_ascii(&bytes)
        {
            None
        } else {
            // SAFETY: is_printable_ascii guarantees every byte is in 0x20–0x7E,
            // which is valid UTF-8.
            Some(unsafe { String::from_utf8_unchecked(bytes.to_vec()) })
        };

        Ok(Self { user_agent })
    }

    pub fn apply(&self, context: &Arc<AttributeMap>, filter: &UserAgentFilter) {
        let user_agent = match self.user_agent.as_ref() {
            Some(v) => v,
            None => return,
        };

        let normalized: Arc<str> = match filter.normalize(user_agent) {
            NormalizeOutput::KnownAgent(label) => label,
            NormalizeOutput::Unknown => {
                filter.sample_unknown_agent(user_agent);
                Arc::from(USER_AGENT_UNKNOWN)
            }
        };

        context.insert(UserAgentValue(normalized));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use lore_telemetry::user_agent_filter::UserAgentFilter;

    use super::*;
    use crate::protocol::attribute_map::AttributeMap;

    mod parse {
        use super::*;

        #[test]
        fn valid_ascii_returns_some() {
            let ua = "my-client/1.0";
            let result = ClientIdentify::parse(Bytes::from(ua)).unwrap();
            assert_eq!(
                result,
                ClientIdentify {
                    user_agent: Some(ua.to_string())
                }
            );
        }

        #[test]
        fn empty_bytes_returns_none() {
            let result = ClientIdentify::parse(Bytes::new()).unwrap();
            assert_eq!(result, ClientIdentify { user_agent: None });
        }

        #[test]
        fn exactly_256_bytes_returns_some() {
            let ua = "a".repeat(USER_AGENT_MAX_BYTES);
            let result = ClientIdentify::parse(Bytes::from(ua.clone())).unwrap();
            assert_eq!(
                result,
                ClientIdentify {
                    user_agent: Some(ua)
                }
            );
        }

        #[test]
        fn over_256_bytes_returns_none() {
            let ua = "a".repeat(USER_AGENT_MAX_BYTES + 1);
            let result = ClientIdentify::parse(Bytes::from(ua)).unwrap();
            assert_eq!(result, ClientIdentify { user_agent: None });
        }

        #[test]
        fn non_ascii_returns_none() {
            // Multi-byte UTF-8; not ASCII.
            let result = ClientIdentify::parse(Bytes::from("lore/1.0-\u{1F4E6}")).unwrap();
            assert_eq!(result, ClientIdentify { user_agent: None });
        }

        #[test]
        fn every_printable_ascii_byte_returns_some() {
            let ua: String = (0x20u8..=0x7E).map(char::from).collect();
            let result = ClientIdentify::parse(Bytes::from(ua.clone())).unwrap();
            assert_eq!(
                result,
                ClientIdentify {
                    user_agent: Some(ua)
                }
            );
        }

        #[test]
        fn control_characters_return_none() {
            for byte in (0x00u8..=0x1F).chain(std::iter::once(0x7F)) {
                let raw = Bytes::from(vec![b'a', byte, b'b']);
                let result = ClientIdentify::parse(raw).unwrap();
                assert_eq!(
                    result,
                    ClientIdentify { user_agent: None },
                    "byte {byte:#04x} must be rejected"
                );
            }
        }

        #[test]
        fn high_bit_bytes_return_none() {
            let result = ClientIdentify::parse(Bytes::from(vec![b'a', 0x80, b'b'])).unwrap();
            assert_eq!(result, ClientIdentify { user_agent: None });
        }
    }

    mod apply {
        use super::*;

        #[test]
        fn none_is_noop() {
            let ci = ClientIdentify { user_agent: None };
            let context = Arc::new(AttributeMap::default());
            let filter = UserAgentFilter::default();

            // Must not panic and must not insert anything.
            ci.apply(&context, &filter);
            assert!(context.get::<UserAgentValue>().is_none());
        }

        #[test]
        fn known_agent_stores_value_in_context() {
            let ci = ClientIdentify {
                user_agent: Some("my-client/1.0".to_string()),
            };
            let context = Arc::new(AttributeMap::default());
            // Default filter (no patterns) treats everything as known.
            let filter = UserAgentFilter::default();

            ci.apply(&context, &filter);

            let stored = context
                .get::<UserAgentValue>()
                .expect("UserAgentValue should be stored");
            assert_eq!(&*stored.0, "my-client/1.0");
        }

        #[test]
        fn unknown_agent_stores_unknown_sentinel() {
            let ci = ClientIdentify {
                user_agent: Some("rogue-client/9.9".to_string()),
            };
            let context = Arc::new(AttributeMap::default());
            // Allow-list that will not match "rogue-client".
            let filter = UserAgentFilter::new(&["my-client/.*"]).unwrap();

            ci.apply(&context, &filter);

            let stored = context
                .get::<UserAgentValue>()
                .expect("UserAgentValue should be stored");
            assert_eq!(&*stored.0, USER_AGENT_UNKNOWN);
        }
    }
}
