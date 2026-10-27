//! Desktop clipboard publication, independent of terminal rendering.
use gmux_client::clipboard::Content;
use gtk::{gdk, glib, prelude::*};

pub(crate) fn publish(clipboard: &gdk::Clipboard, content: Option<Content>) {
    let provider = content.map(|content| {
        let text = gdk::ContentProvider::for_value(&content.text.to_value());
        if let Some(html) = content.html {
            // Bytes owns the moved String. GDK retains both child providers and
            // the union until the offer is replaced, including asynchronous reads.
            let bytes = glib::Bytes::from_owned(html.into_bytes());
            let html = gdk::ContentProvider::for_bytes("text/html", &bytes);
            gdk::ContentProvider::new_union(&[text, html])
        } else {
            text
        }
    });
    let _ = clipboard.set_content(provider.as_ref());
}
