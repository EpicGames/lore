// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Just enough XML to read the four documents this client receives.
//!
//! S3 error bodies, `ListObjectVersions` results, and STS credential responses — no attributes are
//! read, no namespaces are resolved, and nothing is validated beyond what reading those documents
//! requires. A pull reader rather than a tree, so a response is walked once with no allocation
//! except for text that carries an entity or arrives in more than one piece.
//!
//! Every malformed input is an error. Nothing here panics, indexes past the end, or recurses,
//! because the input is a network response.
use std::borrow::Cow;

use crate::aws::error::service_text;
use crate::aws::item::ByteStr;

/// Why a document could not be read.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum XmlError {
    #[error("unterminated tag")]
    UnterminatedTag,
    #[error("empty tag name")]
    EmptyTagName,
    #[error("unterminated comment, declaration or CDATA section")]
    UnterminatedDirective,
    #[error("expected text inside <{0}>")]
    ExpectedText(String),
    #[error("expected <{expected}>, found <{found}>")]
    UnexpectedElement { expected: String, found: String },
    #[error("document has no root element")]
    NoRoot,
    #[error("more than {limit} <{element}> elements")]
    TooMany { element: String, limit: usize },
}

/// One step through a document.
#[derive(Debug, PartialEq)]
pub enum XmlEvent<'a> {
    /// An opening tag, with its name. A self-closing tag reports `Open` then `Close`.
    Open(&'a str),
    /// A closing tag, with its name.
    Close(&'a str),
    /// Character data between tags with its entities resolved, or a CDATA section's content as
    /// written.
    Text(Cow<'a, str>),
}

/// A pull reader over a document.
pub struct XmlReader<'a> {
    rest: &'a str,
    /// Set when a self-closing tag has reported its `Open` and still owes its `Close`.
    pending_close: Option<&'a str>,
}

impl<'a> XmlReader<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            rest: input,
            pending_close: None,
        }
    }

    /// The next event, or `None` at the end of the document.
    pub fn next_event(&mut self) -> Result<Option<XmlEvent<'a>>, XmlError> {
        if let Some(name) = self.pending_close.take() {
            return Ok(Some(XmlEvent::Close(name)));
        }

        loop {
            if self.rest.is_empty() {
                return Ok(None);
            }

            if !self.rest.starts_with('<') {
                let end = self.rest.find('<').unwrap_or(self.rest.len());
                let (text, rest) = self.rest.split_at(end);
                self.rest = rest;

                // Whitespace between elements is layout, not content.
                if text.trim().is_empty() {
                    continue;
                }

                return Ok(Some(XmlEvent::Text(unescape(text))));
            }

            if let Some(section) = self.rest.strip_prefix(CDATA_OPEN) {
                let end = section
                    .find(CDATA_CLOSE)
                    .ok_or(XmlError::UnterminatedDirective)?;
                let (text, rest) = section.split_at(end);
                self.rest = &rest[CDATA_CLOSE.len()..];

                if text.is_empty() {
                    continue;
                }

                return Ok(Some(XmlEvent::Text(Cow::Borrowed(text))));
            }

            if let Some(terminator) = skipped_markup_end(self.rest) {
                let end = self
                    .rest
                    .find(terminator)
                    .ok_or(XmlError::UnterminatedDirective)?;
                self.rest = &self.rest[end + terminator.len()..];
                continue;
            }

            let end = self.rest.find('>').ok_or(XmlError::UnterminatedTag)?;
            let tag = &self.rest[1..end];
            self.rest = &self.rest[end + 1..];

            let (closing, tag) = match tag.strip_prefix('/') {
                Some(name) => (true, name),
                None => (false, tag),
            };
            let self_closing = tag.ends_with('/');
            let tag = tag.trim_end_matches('/');

            // Attributes are not read, so the name is everything up to the first space.
            let name = tag.split_whitespace().next().unwrap_or("");
            if name.is_empty() {
                return Err(XmlError::EmptyTagName);
            }

            if closing {
                return Ok(Some(XmlEvent::Close(name)));
            }

            if self_closing {
                self.pending_close = Some(name);
            }

            return Ok(Some(XmlEvent::Open(name)));
        }
    }

    /// The text of the element just opened, consuming its closing tag.
    ///
    /// An element with no content — `<Key></Key>` or `<Key/>` — reads as the empty string, which is
    /// what S3 sends for an absent marker. Text split by a comment, a CDATA section or a nested
    /// element is joined, and stays borrowed when it arrives in one piece.
    pub fn text(&mut self, name: &str) -> Result<Cow<'a, str>, XmlError> {
        let mut text = Cow::Borrowed("");

        loop {
            match self.next_event()? {
                Some(XmlEvent::Text(piece)) if text.is_empty() => text = piece,
                Some(XmlEvent::Text(piece)) => text.to_mut().push_str(&piece),
                Some(XmlEvent::Close(_)) => return Ok(text),
                // A nested element inside what should be a leaf: skip it whole rather than mistake
                // its text for this element's.
                Some(XmlEvent::Open(nested)) => {
                    self.skip_element(nested)?;
                }
                None => return Err(XmlError::ExpectedText(name.to_owned())),
            }
        }
    }

    /// Discard everything up to and including the close of the element just opened.
    pub fn skip_element(&mut self, name: &str) -> Result<(), XmlError> {
        let mut depth = 1usize;

        while depth > 0 {
            match self.next_event()? {
                Some(XmlEvent::Open(_)) => depth += 1,
                Some(XmlEvent::Close(_)) => depth -= 1,
                Some(XmlEvent::Text(_)) => {}
                None => return Err(XmlError::ExpectedText(name.to_owned())),
            }
        }

        Ok(())
    }
}

/// What opens a CDATA section, whose content is text taken as written.
const CDATA_OPEN: &str = "<![CDATA[";
/// What closes a CDATA section. A `>` alone does not.
const CDATA_CLOSE: &str = "]]>";

/// Where the markup at the start of `rest` ends, when it is markup that is skipped whole: a
/// comment, a processing instruction such as the XML declaration, or a declaration such as
/// `<!DOCTYPE …>`.
fn skipped_markup_end(rest: &str) -> Option<&'static str> {
    if rest.starts_with("<!--") {
        Some("-->")
    } else if rest.starts_with("<?") {
        Some("?>")
    } else if rest.starts_with("<!") {
        Some(">")
    } else {
        None
    }
}

/// Resolve the five XML entities. Borrows unless the text actually carries one.
#[lore_macro::test_pub]
fn unescape(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];

        let (entity, length) = if let Some(end) = tail.find(';').filter(|end| *end <= 10) {
            (&tail[..=end], end + 1)
        } else {
            // A bare ampersand. Not valid XML, but a response holding one should not become an
            // error when the character itself is unambiguous.
            out.push('&');
            rest = &tail[1..];
            continue;
        };

        match entity {
            "&amp;" => out.push('&'),
            "&lt;" => out.push('<'),
            "&gt;" => out.push('>'),
            "&quot;" => out.push('"'),
            "&apos;" => out.push('\''),
            numeric if numeric.starts_with("&#") => {
                let digits = &numeric[2..numeric.len() - 1];
                let code = if let Some(hex) = digits.strip_prefix('x').or(digits.strip_prefix('X'))
                {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    digits.parse().ok()
                };

                match code.and_then(char::from_u32) {
                    Some(character) => out.push(character),
                    // Unresolvable: keep the source text rather than dropping content.
                    None => out.push_str(numeric),
                }
            }
            unknown => out.push_str(unknown),
        }

        rest = &tail[length..];
    }

    out.push_str(rest);
    Cow::Owned(out)
}

/// `<Error><Code>…</Code><Message>…</Message><RequestId>…</RequestId></Error>`, as S3 and STS both
/// send it — STS wraps it in `<ErrorResponse>`, which this reads through.
#[derive(Debug, Default, PartialEq)]
pub struct XmlErrorBody {
    pub code: Option<ByteStr>,
    pub message: Option<ByteStr>,
    pub request_id: Option<ByteStr>,
}

/// Read an error body, taking whatever of it is present.
///
/// A body that is not XML at all — an HTML error page from a proxy, an empty `HEAD` response —
/// yields no code, and the caller falls back to the status.
///
/// Every field is bounded: all three are text the service chose, and all three reach a log line.
/// The generous bound rather than the tight one, because the code still carries its decorations
/// here.
pub fn parse_error_body(body: &str) -> XmlErrorBody {
    let mut reader = XmlReader::new(body);
    let mut parsed = XmlErrorBody::default();

    while let Ok(Some(event)) = reader.next_event() {
        if let XmlEvent::Open(name) = event {
            let field = match name {
                "Code" => &mut parsed.code,
                "Message" => &mut parsed.message,
                "RequestId" => &mut parsed.request_id,
                _ => continue,
            };

            if let Ok(text) = reader.text(name) {
                *field = Some(service_text(&text));
            }
        }
    }

    parsed
}
