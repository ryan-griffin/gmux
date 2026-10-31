//! Link actions read the last painted viewport, including its geometry.
use super::*;
use gmux_protocol::hyperlinks::Hyperlinks;

#[derive(Default)]
pub(super) struct DisplayedLinks {
    pub links: Hyperlinks,
    pub revision: Option<u64>,
    pub frame: Frame,
    pub metrics: (f64, f64),
}
impl DisplayedLinks {
    pub fn target_at(&self, x: f64, y: f64) -> Option<&str> {
        self.links
            .target_at_point((self.frame.cols, self.frame.rows), self.metrics, (x, y))
    }
}

impl Terminal {
    pub(crate) fn link_modifiers_changed(&self, active: bool) {
        self.link_modifiers.set(active);
        self.refresh_link_hover();
    }

    fn link_span(
        &self,
        links: &Hyperlinks,
        frame: Frame,
        metrics: (f64, f64),
    ) -> Option<(usize, usize)> {
        (self.pointer_inside.get() && !self.context_menu.is_visible())
            .then(|| {
                links.span_at_point((frame.cols, frame.rows), metrics, self.mouse_position.get())
            })
            .flatten()
            .filter(|span| links.targets.get(span[2] as usize).is_some())
            .map(|[start, end, _]| (start as usize, end as usize))
    }

    pub(super) fn hovered_link(
        &self,
        links: &Hyperlinks,
        frame: Frame,
        metrics: (f64, f64),
    ) -> Option<(usize, usize)> {
        self.link_modifiers
            .get()
            .then(|| self.link_span(links, frame, metrics))
            .flatten()
    }

    pub(crate) fn links(&self) -> serde_json::Value {
        let displayed = self.displayed_links.borrow();
        serde_json::json!({"frame":displayed.frame,"hyperlinks":displayed.links})
    }

    pub(super) fn refresh_link_hover(&self) {
        let displayed = self.displayed_links.borrow();
        let span = self.link_span(&displayed.links, displayed.frame, displayed.metrics);
        self.cursor.set_link_hover(span.is_some());
        let span = span.filter(|_| self.link_modifiers.get());
        if self.link_hover.replace(span) != span {
            self.area.queue_draw();
        }
    }

    pub(super) fn open_link(self: &Rc<Self>, x: f64, y: f64) -> bool {
        let displayed = self.displayed_links.borrow();
        let Some(uri) = displayed.target_at(x, y) else {
            return false;
        };
        let launcher = gtk::UriLauncher::new(uri);
        let parent = self.area.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        launcher.launch(parent.as_ref(), gio::Cancellable::NONE, move |result| {
            if let Err(error) = result
                && !error.matches(gtk::DialogError::Cancelled)
                && !error.matches(gtk::DialogError::Dismissed)
                && !error.matches(gio::IOErrorEnum::Cancelled)
                && let Some(terminal) = weak.upgrade()
            {
                terminal.report_desktop_error("Could not open link", &error.to_string());
            }
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displayed_link_geometry_rejects_margins_and_maps_fractional_cells() {
        let displayed = DisplayedLinks {
            metrics: (7.5, 13.25),
            revision: Some(1),
            frame: Frame {
                cols: 4,
                rows: 2,
                ..Frame::default()
            },
            links: Hyperlinks {
                targets: vec!["test:actual".into()],
                spans: vec![[2, 6, 0]],
            },
        };
        for (x, y) in [(15.0, 0.0), (29.99, 13.24), (0.0, 13.25), (14.99, 26.49)] {
            assert_eq!(displayed.target_at(x, y), Some("test:actual"));
            assert_eq!(
                displayed
                    .links
                    .span_at_point((4, 2), displayed.metrics, (x, y)),
                Some([2, 6, 0])
            );
        }
        for (x, y) in [
            (-0.1, 0.0),
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
            (30.0, 0.0),
            (0.0, 26.5),
            (14.99, 0.0),
            (15.0, 13.25),
        ] {
            assert_eq!(displayed.target_at(x, y), None);
        }
        // URI interning does not imply that separate spans form one logical link.
        let mut links = displayed.links;
        links.spans = vec![[0, 2, 0], [4, 6, 0]];
        assert_eq!(
            links.span_at_point((4, 2), displayed.metrics, (0.0, 0.0)),
            Some([0, 2, 0])
        );
        assert_eq!(
            links.span_at_point((4, 2), displayed.metrics, (0.0, 13.25)),
            Some([4, 6, 0])
        );
        links.clone_from(&Hyperlinks::default());
        assert_eq!(
            links.span_at_point((4, 2), displayed.metrics, (0.0, 0.0)),
            None
        );
    }
}
