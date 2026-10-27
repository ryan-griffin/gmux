//! Transient, ordered clipboard offers to one frontend.
use serde::{Deserialize, Serialize};

pub const MAX_HTML: usize = 8 * 1024 * 1024;

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
    /// Leave room for the dedicated clipboard packet header and control reply.
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
                || json_bytes(&self.text) + json_bytes(html) > crate::transport::MAX_PACKET - 4096
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

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Write {
    pub destination: Destination,
    /// None clears the destination; Some with empty text offers an empty value.
    pub content: Option<Content>,
}

impl Write {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(content) = &self.content {
            anyhow::ensure!(
                content.text.len() <= crate::transport::MAX_PASTE,
                "Clipboard text is too large"
            );
            // GDK string providers use NUL-terminated UTF-8 strings.
            anyhow::ensure!(!content.text.contains('\0'), "Clipboard text contains NUL");
            if let Some(html) = &content.html {
                anyhow::ensure!(html.len() <= MAX_HTML, "Clipboard HTML is too large");
                anyhow::ensure!(!html.contains('\0'), "Clipboard HTML contains NUL");
            }
        }
        Ok(())
    }
}
