//! Transient, ordered clipboard offers to one frontend.
use serde::{Deserialize, Serialize};

pub const MAX_HTML: usize = 8 * 1024 * 1024;
/// Total decoded application data, including expanded Kitty aliases.
pub const MAX_APPLICATION_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REPRESENTATIONS: usize = 4;
/// Explicit user copy can contain 4 MiB of text and 8 MiB of HTML.
pub const MAX_OFFER_BYTES: usize = crate::transport::MAX_PASTE + MAX_HTML;
/// JSON metadata only, excluding the packet's two length prefixes.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Formats {
    #[default]
    Text,
    TextHtml,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Clipboard,
    Primary,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    pub text: String,
    /// Optional, self-contained HTML describing the same text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
}

impl Content {
    /// Leave room for the control API's JSON reply metadata.
    /// Count JSON escaping without allocating another encoded copy. Plain-only
    /// consumers never traverse their text for this optional representation.
    pub fn bound_html(&mut self) {
        if let Some(html) = &self.html {
            let json_bytes = |text: &str| {
                text.bytes()
                    .map(|byte| match byte {
                        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
                        0..=31 => 6,
                        _ => 1,
                    })
                    .sum::<usize>()
            };
            if html.len() > MAX_HTML
                || json_bytes(&self.text) + json_bytes(html) > crate::network::MAX_REPLY - 4096
            {
                self.html = None;
            }
        }
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Self { text, html: None }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Representation {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Offer(pub Vec<Representation>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidOffer {
    Unsupported,
    InvalidData,
}

impl std::fmt::Display for InvalidOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "Unsupported clipboard MIME type or representation count",
            Self::InvalidData => "Invalid or oversized clipboard data",
        })
    }
}
impl std::error::Error for InvalidOffer {}

/// Validate the complete borrowed request before copying any of its data.
pub fn validate_representations<'a>(
    contents: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    limit: usize,
) -> Result<usize, InvalidOffer> {
    let mut names = [""; MAX_REPRESENTATIONS];
    let mut total = 0usize;
    let mut plain = None;
    let mut count = 0;
    for (mime, data) in contents {
        validate_metadata(mime, data.len())?;
        if count == names.len() {
            return Err(InvalidOffer::Unsupported);
        }
        if names[..count].contains(&mime) {
            return Err(InvalidOffer::InvalidData);
        }
        names[count] = mime;
        count += 1;
        total = total
            .checked_add(data.len())
            .ok_or(InvalidOffer::InvalidData)?;
        if total > limit {
            return Err(InvalidOffer::InvalidData);
        }
        if is_plain(mime) {
            if std::str::from_utf8(data).is_err() || data.contains(&0) {
                return Err(InvalidOffer::InvalidData);
            }
            // The String fallback must describe both supplied plain aliases.
            if plain.is_some_and(|previous| previous != data) {
                return Err(InvalidOffer::InvalidData);
            }
            plain = Some(data);
        }
    }
    if count == 0 {
        return Err(InvalidOffer::InvalidData);
    }
    Ok(total)
}

pub fn is_plain(mime: &str) -> bool {
    matches!(mime, "text/plain" | "text/plain;charset=utf-8")
}

fn validate_metadata(mime: &str, len: usize) -> Result<(), InvalidOffer> {
    let limit = match mime {
        "text/plain" | "text/plain;charset=utf-8" => crate::transport::MAX_PASTE,
        "text/html" | "image/png" => MAX_HTML,
        _ => return Err(InvalidOffer::Unsupported),
    };
    if len > limit {
        return Err(InvalidOffer::InvalidData);
    }
    Ok(())
}

impl Offer {
    pub fn validate(&self) -> Result<(), InvalidOffer> {
        validate_representations(
            self.0.iter().map(|r| (r.mime.as_str(), r.data.as_slice())),
            MAX_OFFER_BYTES,
        )
        .map(|_| ())
    }

    pub fn get(&self, mime: &str) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|r| r.mime == mime)
            .map(|r| r.data.as_slice())
    }

    pub fn plain_text(&self) -> Option<&str> {
        self.0
            .iter()
            .find(|r| is_plain(&r.mime))
            .and_then(|r| std::str::from_utf8(&r.data).ok())
    }

    pub fn byte_len(&self) -> usize {
        self.0.iter().map(|r| r.data.len()).sum()
    }

    pub fn memory_size(&self) -> usize {
        self.0.capacity() * std::mem::size_of::<Representation>()
            + self
                .0
                .iter()
                .map(|r| r.mime.capacity() + r.data.capacity())
                .sum::<usize>()
    }
}

impl From<Content> for Offer {
    fn from(content: Content) -> Self {
        let mut contents = Vec::with_capacity(if content.html.is_some() { 2 } else { 1 });
        contents.push(Representation {
            mime: "text/plain".into(),
            data: content.text.into_bytes(),
        });
        if let Some(html) = content.html {
            contents.push(Representation {
                mime: "text/html".into(),
                data: html.into_bytes(),
            });
        }
        Self(contents)
    }
}

impl From<String> for Offer {
    fn from(text: String) -> Self {
        Content::from(text).into()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Write {
    pub destination: Destination,
    /// None clears; Some offers MIME values, including zero-byte representations.
    pub content: Option<Offer>,
}

impl Write {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(content) = &self.content {
            content.validate()?;
        }
        Ok(())
    }
}

pub(crate) mod wire;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_copy_preserves_plain_text_when_control_json_expands_too_far() {
        let text = "\"".repeat(crate::transport::MAX_PASTE);
        let mut content = Content {
            text,
            html: Some("\\".repeat(MAX_HTML)),
        };
        content.bound_html();
        assert!(content.html.is_none());
        assert_eq!(content.text.len(), crate::transport::MAX_PASTE);
    }
}
