//! Link actions read the last painted viewport, including its geometry.
use super::*;
use gmux_protocol::hyperlinks::Hyperlinks;

#[derive(Default)]
pub(super) struct DisplayedLinks {
    pub links: Hyperlinks,
    pub revision: Option<u64>,
    pub frame: Frame,
    pub metrics: (f64, f64),
    pub bidi: Option<Rc<bidi::Grid>>,
}
impl DisplayedLinks {
    pub fn logical(&self, index: usize) -> usize {
        self.bidi.as_ref().map_or(index, |bidi| bidi.logical(index))
    }

    pub fn cursor_column(&self) -> usize {
        let x = self.frame.cursor_x as usize;
        self.bidi
            .as_ref()
            .and_then(|grid| grid.row(self.frame.cursor_y as usize))
            .map_or(x, |row| row.visual(x))
    }

    pub fn target_at(&self, x: f64, y: f64) -> Option<&str> {
        let index =
            Hyperlinks::cell_at_point((self.frame.cols, self.frame.rows), self.metrics, (x, y))?;
        self.links.target_at(self.logical(index))
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
        bidi: Option<&bidi::Grid>,
    ) -> Option<(usize, usize)> {
        if !self.pointer_inside.get() || self.context_menu.is_visible() {
            return None;
        }
        let visual = Hyperlinks::cell_at_point(
            (frame.cols, frame.rows),
            metrics,
            self.mouse_position.get(),
        )?;
        links
            .span_at(bidi.map_or(visual, |bidi| bidi.logical(visual)))
            .filter(|span| links.targets.get(span[2] as usize).is_some())
            .map(|[start, end, _]| (start as usize, end as usize))
    }

    pub(super) fn hovered_link(
        &self,
        links: &Hyperlinks,
        frame: Frame,
        metrics: (f64, f64),
        bidi: Option<&bidi::Grid>,
    ) -> Option<(usize, usize)> {
        self.link_modifiers
            .get()
            .then(|| self.link_span(links, frame, metrics, bidi))
            .flatten()
    }

    pub(crate) fn links(&self) -> serde_json::Value {
        let displayed = self.displayed_links.borrow();
        serde_json::json!({"frame":displayed.frame,"hyperlinks":displayed.links})
    }

    pub(super) fn refresh_link_hover(&self) {
        let displayed = self.displayed_links.borrow();
        let span = self.link_span(
            &displayed.links,
            displayed.frame,
            displayed.metrics,
            displayed.bidi.as_deref(),
        );
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
    fn rtl_link_and_cursor_follow_the_visual_cell() {
        let cells = ["م", "ن", "X"].map(|glyph| {
            let mut cell = gmux_protocol::Cell::default();
            cell.set_text(glyph).unwrap();
            cell
        });
        let displayed = DisplayedLinks {
            metrics: (10.0, 20.0),
            bidi: bidi::Grid::update(3, &cells, None),
            frame: Frame {
                cols: 3,
                rows: 1,
                cursor_x: 0,
                ..Frame::default()
            },
            links: Hyperlinks {
                targets: vec!["test:rtl".into()],
                spans: vec![[0, 1, 0]],
            },
            ..DisplayedLinks::default()
        };
        assert_eq!(displayed.cursor_column(), 1);
        assert_eq!(displayed.target_at(1.0, 1.0), None);
        assert_eq!(displayed.target_at(11.0, 1.0), Some("test:rtl"));
    }

    #[test]
    fn displayed_link_geometry_rejects_margins_and_maps_fractional_cells() {
        let displayed = DisplayedLinks {
            metrics: (7.5, 13.25),
            revision: Some(1),
            bidi: None,
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
