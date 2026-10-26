//! Native pane progress bars with mapped-only activity animation.
use gmux_client::activity::{Activity, Bar};
use gtk::{glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Default)]
struct Animation {
    current: Cell<Bar>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

impl Animation {
    fn stop(&self, bar: &gtk::ProgressBar) {
        if let Some(tick) = self.tick.borrow_mut().take() {
            tick.remove();
            // Leaving GTK activity mode also removes its internal frame callback.
            bar.set_fraction(0.0);
            bar.reset_property(gtk::AccessibleProperty::ValueNow);
        }
    }

    fn sync(&self, bar: &gtk::ProgressBar) {
        if self.current.get() != Bar::Working(None) || !bar.is_mapped() {
            self.stop(bar);
        } else if self.tick.borrow().is_none() {
            bar.pulse();
            let last = Cell::new(bar.frame_clock().map_or(0, |clock| clock.frame_time()));
            *self.tick.borrow_mut() = Some(bar.add_tick_callback(move |bar, clock| {
                if clock.frame_time() - last.get() >= 100_000 {
                    bar.pulse();
                    last.set(clock.frame_time());
                }
                glib::ControlFlow::Continue
            }));
        }
    }
}

pub(crate) struct PaneProgress {
    pub(crate) bar: gtk::ProgressBar,
    animation: Rc<Animation>,
}

impl PaneProgress {
    pub(crate) fn new() -> Self {
        // Adwaita's thin OSD bar overlays the canvas below its optional header.
        // It never participates in terminal measurement or intercepts input.
        let bar = gtk::ProgressBar::builder()
            .valign(gtk::Align::Start)
            .halign(gtk::Align::Fill)
            .can_target(false)
            .visible(false)
            .build();
        bar.add_css_class("osd");
        bar.add_css_class("pane-progress");
        bar.update_property(&[
            gtk::accessible::Property::Label("Progress"),
            gtk::accessible::Property::ReadOnly(true),
        ]);
        let animation = Rc::new(Animation::default());
        let mapped = animation.clone();
        bar.connect_map(move |bar| mapped.sync(bar));
        let unmapped = animation.clone();
        bar.connect_unmap(move |bar| unmapped.sync(bar));
        Self { bar, animation }
    }

    pub(crate) fn update(&self, activity: Option<Activity<'_>>) {
        let progress = activity.map_or(Bar::Hidden, Activity::bar);
        if self.animation.current.replace(progress) == progress {
            return;
        }
        self.animation.stop(&self.bar);
        self.bar
            .set_fraction(f64::from(progress.percentage().unwrap_or(0)) / 100.0);
        for (class, enabled) in [
            ("error", matches!(progress, Bar::Error(_))),
            ("warning", matches!(progress, Bar::Paused(_))),
            ("unknown", progress.percentage().is_none()),
        ] {
            if enabled {
                self.bar.add_css_class(class);
            } else {
                self.bar.remove_css_class(class);
            }
        }
        let text = progress.description();
        self.bar.update_property(&[
            gtk::accessible::Property::ValueMin(0.0),
            gtk::accessible::Property::ValueMax(100.0),
            gtk::accessible::Property::ValueText(&text),
        ]);
        if let Some(p) = progress.percentage() {
            self.bar
                .update_property(&[gtk::accessible::Property::ValueNow(p.into())]);
        } else {
            self.bar.reset_property(gtk::AccessibleProperty::ValueNow);
        }
        self.bar.set_visible(progress != Bar::Hidden);
        self.animation.sync(&self.bar);
    }
}
