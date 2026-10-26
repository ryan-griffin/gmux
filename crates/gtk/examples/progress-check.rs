//! Native size/role checks for the production progress widget at interface fonts.
#[path = "../src/progress.rs"]
mod progress;
use gmux_client::progress::{Progress, ProgressState};
use gmux_client::{
    activity::Activity,
    program_status::{Record, State, Summary},
};
use gtk::{gio, prelude::*};
use std::time::{Duration, Instant};

fn settle() {
    let until = Instant::now() + Duration::from_millis(60);
    while Instant::now() < until {
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn widget_count(widget: &gtk::Widget) -> usize {
    let mut count = 1;
    let mut child = widget.first_child();
    while let Some(widget) = child {
        count += widget_count(&widget);
        child = widget.next_sibling();
    }
    count
}

fn check_pane(direction: gtk::TextDirection) -> serde_json::Value {
    let progress = progress::PaneProgress::new();
    progress.bar.set_direction(direction);
    let area = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();
    let overlay = gtk::Overlay::builder().child(&area).vexpand(true).build();
    overlay.add_overlay(&progress.bar);
    overlay.set_measure_overlay(&progress.bar, false);
    let header = gtk::Label::new(Some("Pane title"));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&overlay);
    let window = gtk::Window::builder()
        .child(&content)
        .default_width(360)
        .default_height(160)
        .build();
    window.present();
    settle();
    let fill = progress.bar.first_child().unwrap().first_child().unwrap();
    assert_eq!(
        progress.bar.accessible_role(),
        gtk::AccessibleRole::ProgressBar
    );
    assert!(!progress.bar.can_target());
    assert!(!progress.bar.shows_text());
    let mut bar_height = 0;
    for titles in [true, false] {
        header.set_visible(titles);
        settle();
        let size = (area.width(), area.height());
        let mut value = Progress::default();
        for state in [
            ProgressState::Determinate,
            ProgressState::Error,
            ProgressState::Paused,
            ProgressState::Indeterminate,
            ProgressState::Inactive,
        ] {
            for percentage in [0, 9, 42, 99, 100] {
                value.apply(state, Some(percentage));
                progress.update(gmux_client::activity::Activity::select(None, value));
                settle();
                assert_eq!((area.width(), area.height()), size, "{titles}, {state:?}");
                assert_eq!(progress.bar.is_mapped(), value.active());
                assert_eq!(
                    fill.has_css_class("pulse"),
                    state == ProgressState::Indeterminate
                );
                if value.active() {
                    assert!(progress.bar.height() > 0);
                    bar_height = progress.bar.height();
                    assert_eq!(progress.bar.width(), overlay.width());
                    assert_eq!(progress.bar.compute_bounds(&overlay).unwrap().y(), 0.0);
                    assert_eq!(
                        gtk::test_accessible_has_property(
                            &progress.bar,
                            gtk::AccessibleProperty::ValueNow
                        ),
                        state != ProgressState::Indeterminate
                    );
                    assert!(gtk::test_accessible_has_property(
                        &progress.bar,
                        gtk::AccessibleProperty::ValueText
                    ));
                    if state != ProgressState::Indeterminate {
                        assert_eq!(progress.bar.fraction(), f64::from(percentage) / 100.0);
                    }
                }
            }
        }
        value.apply(ProgressState::Indeterminate, None);
        progress.update(gmux_client::activity::Activity::select(None, value));
        window.set_visible(false);
        settle();
        assert!(
            !fill.has_css_class("pulse"),
            "Unmapping must leave native activity mode"
        );
        assert!(!progress.bar.is_mapped());
        assert!(!gtk::test_accessible_has_property(
            &progress.bar,
            gtk::AccessibleProperty::ValueNow
        ));
        window.present();
        settle();
        assert!(
            fill.has_css_class("pulse"),
            "Mapping must resume unknown progress"
        );
        value.apply(ProgressState::Inactive, None);
        progress.update(gmux_client::activity::Activity::select(None, value));
        settle();
        assert!(!fill.has_css_class("pulse"));
        assert!(!progress.bar.is_mapped());

        // Rich reports select the same production bar, replacing a conflicting
        // legacy value. Missing estimates never become numeric zero or 100%.
        value.apply(ProgressState::Determinate, Some(91));
        for (state, percentage) in [
            (State::Working, Some(38)),
            (State::Working, None),
            (State::Blocked, Some(25)),
            (State::Blocked, None),
            (State::Error, None),
            (State::Done, None),
            (State::Idle, None),
        ] {
            let status = Summary {
                count: 1,
                record: Record {
                    id: String::new(),
                    state,
                    kind: None,
                    progress: percentage,
                    app: None,
                    title: String::new(),
                    message: String::new(),
                },
            };
            progress.update(Activity::select(Some(&status), value));
            settle();
            assert_eq!((area.width(), area.height()), size);
            assert_eq!(
                progress.bar.is_mapped(),
                matches!(state, State::Working | State::Blocked | State::Error)
            );
            assert_eq!(
                fill.has_css_class("pulse"),
                state == State::Working && percentage.is_none()
            );
            assert_eq!(
                gtk::test_accessible_has_property(&progress.bar, gtk::AccessibleProperty::ValueNow),
                percentage.is_some()
            );
            assert_eq!(
                progress.bar.has_css_class("warning"),
                state == State::Blocked
            );
            assert_eq!(progress.bar.has_css_class("error"), state == State::Error);
            if let Some(p) = percentage {
                assert_eq!(progress.bar.fraction(), f64::from(p) / 100.0);
            }
        }
        progress.update(Activity::select(None, value));
        settle();
        assert_eq!(progress.bar.fraction(), 0.91);
        value.apply(ProgressState::Inactive, None);
        progress.update(Activity::select(None, value));
    }
    let result = serde_json::json!({"widget_count":widget_count(progress.bar.upcast_ref()),
        "style":"Adwaita osd", "height":bar_height});
    window.close();
    result
}
fn main() {
    adw::init().unwrap();
    gio::resources_register_include!("gmux.gresource").unwrap();
    let css = gtk::CssProvider::new();
    css.load_from_resource("/dev/gmux/Gmux/style.css");
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let mut results = Vec::new();
    for font in ["Sans 11", "Sans 22"] {
        gtk::Settings::default()
            .unwrap()
            .set_property("gtk-font-name", font);
        for direction in [gtk::TextDirection::Ltr, gtk::TextDirection::Rtl] {
            results.push(serde_json::json!({
                "interface_font": font,
                "direction": format!("{direction:?}"),
                "pane_bar": check_pane(direction),
            }));
        }
    }
    println!("{}", serde_json::to_string(&results).unwrap());
}
