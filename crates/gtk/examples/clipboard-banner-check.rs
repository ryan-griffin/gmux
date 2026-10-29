//! Check the custom two-action banner against the installed AdwBanner.
use adw::prelude::*;
use gtk::glib;
#[path = "../src/clipboard/banner.rs"]
mod banner;
#[path = "../src/clipboard/prompt.rs"]
mod prompt;

fn bounds(widget: &impl IsA<gtk::Widget>, parent: &impl IsA<gtk::Widget>) -> gtk::graphene::Rect {
    widget.compute_bounds(parent).unwrap()
}

fn pump(milliseconds: u64) {
    let end = std::time::Instant::now() + std::time::Duration::from_millis(milliseconds);
    while std::time::Instant::now() < end {
        while glib::MainContext::default().iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
fn main() {
    gtk::init().unwrap();
    adw::init().unwrap();
    let settings = gtk::Settings::default().unwrap();
    settings.set_gtk_enable_animations(false);
    let mut cases = 0;
    for scheme in [adw::ColorScheme::ForceLight, adw::ColorScheme::ForceDark] {
        adw::StyleManager::default().set_color_scheme(scheme);
        for font in ["Sans 11", "Sans 18"] {
            settings.set_gtk_font_name(Some(font));
            for direction in [gtk::TextDirection::Ltr, gtk::TextDirection::Rtl] {
                let text = "Allow a program in this pane to read your clipboard?";
                let native = adw::Banner::new(text);
                native.set_button_label(Some("Allow Once"));
                native.set_direction(direction);
                native.set_revealed(true);
                let content = native
                    .first_child()
                    .unwrap()
                    .downcast::<gtk::Revealer>()
                    .unwrap()
                    .child()
                    .unwrap();
                let native_title = content.first_child().unwrap();
                let native_button = content.last_child().unwrap();
                let custom = banner::Banner::new();
                custom.set_direction(direction);
                let prompt = prompt::Prompt::new(text);
                prompt.set_direction(direction);
                prompt.deny().set_visible(false);
                custom.present(&prompt);
                let title = prompt.first_child().unwrap();
                assert_eq!(
                    custom.measure(gtk::Orientation::Horizontal, -1),
                    native.measure(gtk::Orientation::Horizontal, -1)
                );
                // One visible action must reproduce native measurement and placement.
                for width in [184, 200, 359, 360, 719, 800, 1100, 1401] {
                    let measured = custom.measure(gtk::Orientation::Vertical, width);
                    let expected = native.measure(gtk::Orientation::Vertical, width);
                    assert_eq!(measured, expected, "{font}, {direction:?}, width {width}");
                    custom.allocate(width, measured.1, -1, None);
                    native.allocate(width, expected.1, -1, None);
                    for (got, expected) in [
                        (bounds(&title, &custom), bounds(&native_title, &native)),
                        (
                            bounds(prompt.allow(), &custom),
                            bounds(&native_button, &native),
                        ),
                    ] {
                        assert_eq!(got, expected, "{font}, {direction:?}, width {width}");
                    }
                    cases += 1;
                }
                prompt.deny().set_visible(true);
                // Both actions must stay inside the banner and clear the title.
                for width in [184, 200, 360, 719, 800, 1100, 1401] {
                    let (_, height, _, _) = custom.measure(gtk::Orientation::Vertical, width);
                    custom.allocate(width, height, -1, None);
                    let t = bounds(&title, &custom);
                    let d = bounds(prompt.deny(), &custom);
                    let a = bounds(prompt.allow(), &custom);
                    for b in [d, a] {
                        assert!(
                            b.x() >= 0.
                                && b.y() >= 0.
                                && b.x() + b.width() <= width as f32
                                && b.y() + b.height() <= height as f32
                        );
                        assert!(
                            !t.intersection(&b)
                                .is_some_and(|r| r.width() > 0. && r.height() > 0.)
                        );
                    }
                    assert!(
                        !d.intersection(&a)
                            .is_some_and(|r| r.width() > 0. && r.height() > 0.)
                    );
                    assert!(!prompt.allow().gets_focus_on_click());
                    cases += 1;
                }
                custom.dismiss();
                assert!(
                    custom
                        .first_child()
                        .unwrap()
                        .downcast::<gtk::Revealer>()
                        .unwrap()
                        .child()
                        .is_none()
                );
            }
        }
    }
    let window = gtk::Window::new();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let entry = gtk::Entry::new();
    root.append(&entry);
    let custom = banner::Banner::new();
    let prompt = prompt::Prompt::new("Clipboard permission");
    custom.present(&prompt);
    root.append(&custom);
    window.set_child(Some(&root));
    window.present();
    while glib::MainContext::default().iteration(false) {}
    entry.grab_focus();
    assert!(custom.child_focus(gtk::DirectionType::TabForward));
    assert_eq!(
        gtk::prelude::GtkWindowExt::focus(&window).as_ref(),
        Some(prompt.deny().upcast_ref())
    );
    assert!(custom.child_focus(gtk::DirectionType::TabForward));
    assert_eq!(
        gtk::prelude::GtkWindowExt::focus(&window).as_ref(),
        Some(prompt.allow().upcast_ref())
    );
    // A new request can arrive before the previous banner finishes hiding.
    settings.set_gtk_enable_animations(true);
    custom.dismiss();
    pump(50);
    let replacement = prompt::Prompt::new("Another clipboard request");
    custom.present(&replacement);
    pump(300);
    let revealer = custom
        .first_child()
        .unwrap()
        .downcast::<gtk::Revealer>()
        .unwrap();
    assert_eq!(revealer.child().as_ref(), Some(replacement.upcast_ref()));
    assert!(revealer.is_child_revealed());
    custom.dismiss();
    pump(300);
    assert!(revealer.child().is_none());
    window.close();
    println!(
        "PASS: {cases} native geometry/adaptation cases, both font sizes, directions, themes, hide cleanup and keyboard focus"
    );
}
