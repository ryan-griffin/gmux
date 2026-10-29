//! Desktop clipboard publication, independent of terminal rendering.
use gmux_client::clipboard::Offer;
use gtk::{gdk, glib, prelude::*};

pub(crate) fn publish(clipboard: &gdk::Clipboard, content: Option<Offer>) -> anyhow::Result<()> {
    // Validate before constructing providers or mutating the desktop clipboard.
    if let Some(content) = &content {
        content.validate()?;
    }
    let provider = content.map(|content| {
        // Raw providers come first so a String serializer cannot replace supplied
        // MIME bytes. Only actual plain text supplies GTK's String fallback.
        let text = content
            .plain_text()
            .map(|text| gdk::ContentProvider::for_value(&text.to_value()));
        let mut providers = Vec::with_capacity(content.0.len() + usize::from(text.is_some()));
        for representation in content.0 {
            // Bytes takes the Vec without copying. The union retains every child
            // through asynchronous reads, after the transient packet has gone.
            let bytes = glib::Bytes::from_owned(representation.data);
            providers.push(gdk::ContentProvider::for_bytes(
                &representation.mime,
                &bytes,
            ));
        }
        if let Some(text) = text {
            providers.push(text);
        }
        gdk::ContentProvider::new_union(&providers)
    });
    clipboard.set_content(provider.as_ref())?;
    Ok(())
}

mod banner;
mod prompt;
pub(crate) use banner::Banner;
mod read;
pub(crate) use read::Desktop;
