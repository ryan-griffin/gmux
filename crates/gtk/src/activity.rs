//! One sidebar slot for activity and unread alerts. No animation or polling.
use gmux_protocol::activity::{Activity, State};
use gtk::prelude::*;

pub(crate) fn icon_name(state: State) -> &'static str {
    match state {
        State::Blocked => "dialog-question-symbolic",
        State::Error => "dialog-error-symbolic",
        State::Done => "emblem-ok-symbolic",
        State::Working => "content-loading-symbolic",
        State::Idle | State::Paused => "media-playback-pause-symbolic",
    }
}

pub(crate) struct Indicator {
    pub(crate) widget: gtk::Overlay,
    icon: gtk::Image,
    unread: gtk::Box,
}
impl Indicator {
    pub(crate) fn new() -> Self {
        let icon = gtk::Image::builder().pixel_size(16).build();
        // A constant 16px slot keeps adding an unread badge from shifting text.
        let widget = gtk::Overlay::builder()
            .child(&icon)
            .width_request(16)
            .height_request(16)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let unread = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        unread.add_css_class("tab-unread");
        unread.set_can_target(false);
        unread.set_accessible_role(gtk::AccessibleRole::Presentation);
        widget.add_overlay(&unread);
        Self {
            widget,
            icon,
            unread,
        }
    }
    pub(crate) fn update(&self, activity: Option<Activity<'_>>, unread: bool) {
        self.widget.set_visible(activity.is_some() || unread);
        self.icon
            .set_icon_name(activity.map(|a| icon_name(a.state())));
        self.unread.set_visible(unread);
        let badge = activity.is_some();
        self.unread
            .set_size_request(if badge { 6 } else { 10 }, if badge { 6 } else { 10 });
        self.unread.set_halign(if badge {
            gtk::Align::End
        } else {
            gtk::Align::Center
        });
        self.unread.set_valign(if badge {
            gtk::Align::Start
        } else {
            gtk::Align::Center
        });
        if badge {
            self.unread.add_css_class("badge");
        } else {
            self.unread.remove_css_class("badge");
        }
        let mut text = activity.map_or_else(String::new, |a| a.description());
        if unread {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str("Unread terminal alerts");
        }
        self.widget
            .set_tooltip_text((!text.is_empty()).then_some(text.as_str()));
        self.widget
            .update_property(&[gtk::accessible::Property::Label(&text)]);
        self.icon
            .update_property(&[gtk::accessible::Property::Label(&text)]);
    }
}
