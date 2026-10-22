//! Transient, ordered text writes to one frontend's desktop clipboard.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Clipboard,
    Primary,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Write {
    pub destination: Destination,
    /// None clears the destination; Some("") offers an empty text value.
    pub text: Option<String>,
}

impl Write {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(text) = &self.text {
            anyhow::ensure!(
                text.len() <= crate::transport::MAX_PASTE,
                "Clipboard text is too large"
            );
            // GDK text offers use NUL-terminated UTF-8 strings.
            anyhow::ensure!(!text.contains('\0'), "Clipboard text contains NUL");
        }
        Ok(())
    }
}
