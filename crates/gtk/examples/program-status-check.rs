//! Check sidebar status tooltips, accessibility and stable title allocation.
#[path = "../src/activity.rs"]
mod activity;
use gmux_protocol::program_status::{Record, State, Summary};
use gtk::prelude::*;
use std::time::{Duration, Instant};

fn settle() {
    let until = Instant::now() + Duration::from_millis(40);
    while Instant::now() < until {
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn check_sidebar(direction: gtk::TextDirection) -> serde_json::Value {
    let indicator = activity::Indicator::new();
    let title = gtk::Label::new(Some("Build and deploy"));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_direction(direction);
    row.append(&indicator.widget);
    row.append(&title);
    let window = gtk::Window::builder()
        .child(&row)
        .default_width(300)
        .build();
    window.present();
    let mut legacy = gmux_protocol::progress::Progress::default();
    legacy.apply(
        gmux_protocol::progress::ProgressState::Determinate,
        Some(42),
    );
    let report = gmux_protocol::activity::Activity::select(None, legacy).unwrap();
    let icon = indicator
        .widget
        .child()
        .unwrap()
        .downcast::<gtk::Image>()
        .unwrap();
    let dot = indicator
        .widget
        .last_child()
        .unwrap()
        .downcast::<gtk::Box>()
        .unwrap();
    indicator.update(Some(report), false);
    settle();
    let title_x = title.compute_bounds(&row).unwrap().x();
    let width = indicator.widget.width();
    assert_eq!(width, 16);
    for unread in [true, false, true] {
        indicator.update(Some(report), unread);
        settle();
        assert_eq!(
            icon.icon_name().as_deref(),
            Some("content-loading-symbolic")
        );
        assert_eq!(dot.is_visible(), unread);
        assert_eq!(indicator.widget.width(), width);
        assert_eq!(title.compute_bounds(&row).unwrap().x(), title_x);
        let text = indicator.widget.tooltip_text().unwrap();
        assert!(text.contains("In progress, 42%"));
        assert_eq!(text.contains("Unread terminal alerts"), unread);
        assert!(gtk::test_accessible_has_property(
            &indicator.widget,
            gtk::AccessibleProperty::Label
        ));
    }
    for state in [
        State::Idle,
        State::Working,
        State::Done,
        State::Blocked,
        State::Error,
    ] {
        let status = Summary {
            count: 64,
            record: Record {
                id: "worker".into(),
                state,
                kind: None,
                progress: None,
                app: Some("cargo".into()),
                title: "x".repeat(192),
                message: format!("<b>Plain text</b>\u{202e}{}", "x".repeat(2000)),
            },
        };
        indicator.update(
            Some(gmux_protocol::activity::Activity::Program(&status)),
            true,
        );
        settle();
        assert_eq!(indicator.widget.width(), width);
        assert_eq!(title.compute_bounds(&row).unwrap().x(), title_x);
        let text = indicator.widget.tooltip_text().unwrap();
        assert!(text.contains("<b>Plain text</b>") && !text.contains('\u{202e}'));
        assert!(!text.starts_with("Pane "));
        assert_eq!(
            icon.icon_name().as_deref(),
            Some(activity::icon_name(
                gmux_protocol::activity::Activity::Program(&status).state()
            ))
        );
        assert!(gtk::test_accessible_has_property(
            &icon,
            gtk::AccessibleProperty::Label
        ));
    }
    indicator.update(None, true);
    settle();
    assert!(indicator.widget.is_mapped() && dot.is_mapped());
    assert!(!dot.has_css_class("badge"));
    assert_eq!(dot.halign(), gtk::Align::Center);
    assert_eq!(indicator.widget.width(), width);
    assert_eq!(title.compute_bounds(&row).unwrap().x(), title_x);
    indicator.update(None, false);
    settle();
    assert!(!indicator.widget.is_mapped());
    window.close();
    serde_json::json!({"slot_width":width,"unread_does_not_shift_title":true})
}
fn main() {
    adw::init().unwrap();
    let mut measurements = Vec::new();
    for font in ["Sans 11", "Sans 22"] {
        gtk::Settings::default()
            .unwrap()
            .set_gtk_font_name(Some(font));
        for direction in [gtk::TextDirection::Ltr, gtk::TextDirection::Rtl] {
            measurements.push(
                serde_json::json!({"font":font, "direction":format!("{direction:?}"),
                "sidebar":check_sidebar(direction)}),
            );
        }
    }
    println!("{}", serde_json::to_string(&measurements).unwrap());
}
