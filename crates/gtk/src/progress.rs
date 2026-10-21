//! Native pane progress bars with mapped-only activity animation.
use gmux_client::progress::{Progress, ProgressState};
use gtk::{glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

fn description(progress: Progress) -> String {
    let percentage = progress.percentage();
    match progress.state() {
        ProgressState::Inactive => String::new(),
        ProgressState::Determinate => format!("In progress, {percentage}%"),
        ProgressState::Error => format!("Error, {percentage}%"),
        ProgressState::Paused => format!("Paused, {percentage}%"),
        ProgressState::Indeterminate => "In progress, percentage unknown".into(),
    }
}

#[derive(Default)]
struct Activity {
    current: Cell<Progress>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

impl Activity {
    fn stop(&self, bar: &gtk::ProgressBar) {
        if let Some(tick) = self.tick.borrow_mut().take() {
            tick.remove();
            // Leaving GTK activity mode also removes its internal frame callback.
            bar.set_fraction(0.0);
            Self::unknown_value(bar);
        }
    }

    fn sync(&self, bar: &gtk::ProgressBar) {
        if self.current.get().state() != ProgressState::Indeterminate || !bar.is_mapped() {
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

    fn unknown_value(bar: &gtk::ProgressBar) {
        bar.reset_property(gtk::AccessibleProperty::ValueNow);
        bar.update_property(&[
            gtk::accessible::Property::ValueMin(0.0),
            gtk::accessible::Property::ValueMax(100.0),
            gtk::accessible::Property::ValueText("In progress, percentage unknown"),
        ]);
    }
}

pub(crate) struct PaneProgress {
    pub(crate) bar: gtk::ProgressBar,
    activity: Rc<Activity>,
}

impl PaneProgress {
    pub(crate) fn new(pane: u64) -> Self {
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
            gtk::accessible::Property::Label(&format!("Pane {pane} progress")),
            gtk::accessible::Property::ReadOnly(true),
        ]);
        let activity = Rc::new(Activity::default());
        let mapped = activity.clone();
        bar.connect_map(move |bar| mapped.sync(bar));
        let unmapped = activity.clone();
        bar.connect_unmap(move |bar| unmapped.sync(bar));
        Self { bar, activity }
    }

    pub(crate) fn update(&self, progress: Progress) {
        if self.activity.current.replace(progress) == progress {
            return;
        }
        self.activity.stop(&self.bar);
        let indeterminate = progress.state() == ProgressState::Indeterminate;
        self.bar.set_fraction(if indeterminate {
            0.0
        } else {
            f64::from(progress.percentage()) / 100.0
        });
        for (class, enabled) in [
            ("error", progress.state() == ProgressState::Error),
            ("warning", progress.state() == ProgressState::Paused),
        ] {
            if enabled {
                self.bar.add_css_class(class);
            } else {
                self.bar.remove_css_class(class);
            }
        }
        let text = description(progress);
        self.bar.update_property(&[
            gtk::accessible::Property::ValueMin(0.0),
            gtk::accessible::Property::ValueMax(100.0),
            gtk::accessible::Property::ValueText(&text),
        ]);
        if indeterminate {
            Activity::unknown_value(&self.bar);
        } else {
            self.bar
                .update_property(&[gtk::accessible::Property::ValueNow(
                    progress.percentage().into(),
                )]);
        }
        self.bar.set_visible(progress.active());
        self.activity.sync(&self.bar);
    }
}
