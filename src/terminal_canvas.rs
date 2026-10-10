use gmux::Scrollback;
use gtk::{glib, prelude::*, subclass::prelude::*};
use std::{
    cell::{Cell, RefCell},
    sync::OnceLock,
};

type SnapshotCallback = Box<dyn Fn(&gtk::Snapshot, i32, i32)>;
type ScrollCallback = Box<dyn Fn(u64)>;

mod imp {
    use super::*;

    pub struct TerminalCanvas {
        pub text: RefCell<crate::accessible_terminal::Text>,
        pub spare_text: RefCell<Option<crate::accessible_terminal::Text>>,
        pub text_buffer: RefCell<Option<gtk::TextBuffer>>,
        pub text_revision: Cell<Option<(u64, u16, u16, u64)>>,
        pub metrics: Cell<(f32, f32)>,
        pub callback: RefCell<Option<SnapshotCallback>>,
        pub scroll_callback: RefCell<Option<ScrollCallback>>,
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        pub hscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub adjustment_handler: RefCell<Option<glib::SignalHandlerId>>,
        pub scrollback: Cell<Scrollback>,
        pub scroll_pending: Cell<bool>,
        pub syncing: Cell<bool>,
        pub scroll_row: Cell<u64>,
        pub row_height: Cell<f64>,
    }

    impl Default for TerminalCanvas {
        fn default() -> Self {
            Self {
                text: RefCell::default(),
                spare_text: RefCell::default(),
                text_buffer: RefCell::default(),
                text_revision: Cell::new(None),
                metrics: Cell::new((9.6, 22.0)),
                callback: RefCell::default(),
                scroll_callback: RefCell::default(),
                hadjustment: RefCell::default(),
                vadjustment: RefCell::default(),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                adjustment_handler: RefCell::default(),
                scrollback: Cell::default(),
                scroll_pending: Cell::default(),
                syncing: Cell::default(),
                scroll_row: Cell::default(),
                row_height: Cell::new(22.0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TerminalCanvas {
        const NAME: &'static str = "GmuxTerminalCanvas";
        type Type = super::TerminalCanvas;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable, gtk::AccessibleText);

        fn class_init(klass: &mut Self::Class) {
            klass.set_accessible_role(gtk::AccessibleRole::Terminal);
        }
    }

    impl ObjectImpl for TerminalCanvas {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPERTIES.get_or_init(|| {
                [
                    "hadjustment",
                    "vadjustment",
                    "hscroll-policy",
                    "vscroll-policy",
                ]
                .map(glib::ParamSpecOverride::for_interface::<gtk::Scrollable>)
                .into()
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "hadjustment" => {
                    let adjustment: Option<gtk::Adjustment> = value.get().unwrap();
                    if let Some(adjustment) = &adjustment {
                        adjustment.configure(0.0, 0.0, 0.0, 1.0, 1.0, 0.0);
                    }
                    self.hadjustment.replace(adjustment);
                }
                "vadjustment" => self.set_vadjustment(value.get().unwrap()),
                "hscroll-policy" => self.hscroll_policy.set(value.get().unwrap()),
                "vscroll-policy" => self.vscroll_policy.set(value.get().unwrap()),
                name => unreachable!("Unknown property {name}"),
            }
            self.obj().queue_allocate();
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "hadjustment" => self.hadjustment.borrow().to_value(),
                "vadjustment" => self.vadjustment.borrow().to_value(),
                "hscroll-policy" => self.hscroll_policy.get().to_value(),
                "vscroll-policy" => self.vscroll_policy.get().to_value(),
                name => unreachable!("Unknown property {name}"),
            }
        }

        fn dispose(&self) {
            self.set_vadjustment(None);
            self.hadjustment.borrow_mut().take();
            self.callback.borrow_mut().take();
            self.scroll_callback.borrow_mut().take();
        }
    }

    impl TerminalCanvas {
        fn set_vadjustment(&self, adjustment: Option<gtk::Adjustment>) {
            if let Some(previous) = self.vadjustment.borrow_mut().take()
                && let Some(handler) = self.adjustment_handler.borrow_mut().take()
            {
                previous.disconnect(handler);
            }
            if let Some(adjustment) = &adjustment {
                let weak = self.obj().downgrade();
                let handler = adjustment.connect_value_changed(move |adjustment| {
                    let Some(canvas) = weak.upgrade() else {
                        return;
                    };
                    let imp = canvas.imp();
                    if imp.syncing.get() {
                        return;
                    }
                    let row = (adjustment.value() / imp.row_height.get()).round() as u64;
                    if imp.scroll_row.replace(row) != row
                        && let Some(callback) = &*imp.scroll_callback.borrow()
                    {
                        imp.scroll_pending.set(true);
                        callback(row);
                    }
                });
                self.adjustment_handler.replace(Some(handler));
            }
            self.vadjustment.replace(adjustment);
            self.obj().sync_adjustment();
        }
    }

    impl ScrollableImpl for TerminalCanvas {}

    impl AccessibleTextImpl for TerminalCanvas {
        fn caret_position(&self) -> u32 {
            self.text.borrow().caret
        }
        fn contents(&self, start: u32, end: u32) -> Option<glib::Bytes> {
            Some(glib::Bytes::from_owned(
                self.text.borrow().bytes(start, end),
            ))
        }
        fn contents_at(
            &self,
            offset: u32,
            granularity: gtk::AccessibleTextGranularity,
        ) -> Option<(u32, u32, glib::Bytes)> {
            let text = self.text.borrow();
            if offset >= text.char_count() {
                return None;
            }
            let buffer = self
                .text_buffer
                .borrow_mut()
                .get_or_insert_with(|| {
                    let buffer = gtk::TextBuffer::new(None::<&gtk::TextTagTable>);
                    buffer.set_text(&text.value);
                    buffer
                })
                .clone();
            let mut start = buffer.iter_at_offset(offset as i32);
            let mut end = start;
            match granularity {
                gtk::AccessibleTextGranularity::Character => {
                    if !start.is_cursor_position() {
                        start.backward_cursor_position();
                    }
                    end.forward_cursor_position();
                }
                gtk::AccessibleTextGranularity::Word
                    if start.inside_word() || start.starts_word() =>
                {
                    if !start.starts_word() {
                        start.backward_word_start();
                    }
                    end.forward_word_end();
                }
                gtk::AccessibleTextGranularity::Sentence => {
                    if !start.starts_sentence() {
                        start.backward_sentence_start();
                    }
                    end.forward_sentence_end();
                }
                gtk::AccessibleTextGranularity::Word => {
                    end.forward_cursor_position();
                }
                _ => {
                    start.set_line_offset(0);
                    if !end.ends_line() {
                        end.forward_to_line_end();
                    }
                    end.forward_char();
                }
            }
            let (start, end) = (start.offset() as u32, end.offset() as u32);
            Some((start, end, glib::Bytes::from_owned(text.bytes(start, end))))
        }
        fn attributes(
            &self,
            _: u32,
        ) -> Vec<(gtk::AccessibleTextRange, glib::GString, glib::GString)> {
            Vec::new()
        }
        fn default_attributes(&self) -> Vec<(glib::GString, glib::GString)> {
            vec![("editable".into(), "false".into())]
        }
        fn selection(&self) -> Vec<gtk::AccessibleTextRange> {
            self.text
                .borrow()
                .selection
                .map(|(start, end)| {
                    vec![gtk::AccessibleTextRange::new(
                        start as usize,
                        (end - start) as usize,
                    )]
                })
                .unwrap_or_default()
        }
        fn extents(&self, start: u32, end: u32) -> Option<gtk::graphene::Rect> {
            let text = self.text.borrow();
            let (cw, ch) = self.metrics.get();
            text.extents(start, end, cw, ch)
                .map(|(x, y, width, height)| gtk::graphene::Rect::new(x, y, width, height))
        }
        fn offset(&self, point: &gtk::graphene::Point) -> Option<u32> {
            let text = self.text.borrow();
            let (cw, ch) = self.metrics.get();
            text.offset_at(point.x(), point.y(), cw, ch)
        }
        fn set_caret_position(&self, _: u32) -> bool {
            false
        }
        fn set_selection(&self, _: usize, _: gtk::AccessibleTextRange) -> bool {
            false
        }
    }

    impl WidgetImpl for TerminalCanvas {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().sync_adjustment();
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
                    popover.present();
                }
            }
        }
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(callback) = &*self.callback.borrow() {
                let widget = self.obj();
                callback(snapshot, widget.width(), widget.height());
            }
        }
    }
}

glib::wrapper! {
    pub struct TerminalCanvas(ObjectSubclass<imp::TerminalCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::AccessibleText, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl TerminalCanvas {
    pub fn update_text(
        &self,
        screen: &gmux::session::ScreenSnapshot<'_>,
        selection: Option<(usize, usize)>,
    ) {
        let imp = self.imp();
        let key = (
            screen.text_revision,
            screen.frame.cols,
            screen.frame.rows,
            screen.frame.scrollback.offset,
        );
        let next = if imp.text_revision.get() != Some(key) {
            // Keep the old text readable until GTK's removal notification has
            // completed, then reuse its allocations for the next rebuild.
            if let Some(mut text) = imp.spare_text.borrow_mut().take() {
                text.rebuild(screen.frame, screen.cells, selection);
                text
            } else {
                crate::accessible_terminal::Text::from_frame(screen.frame, screen.cells, selection)
            }
        } else {
            let mut old = imp.text.borrow_mut();
            let (caret, selected) = (old.caret, old.selection);
            old.update_positions(screen.frame, selection);
            let caret_changed = old.caret != caret;
            let selection_changed = old.selection != selected;
            drop(old);
            if caret_changed {
                self.update_caret_position();
            }
            if selection_changed {
                self.update_selection_bound();
            }
            return;
        };
        let (content_changed, caret_changed, selection_changed, old_len) = {
            let old = imp.text.borrow();
            (
                old.value != next.value,
                old.caret != next.caret,
                old.selection != next.selection,
                old.char_count(),
            )
        };
        let new_len = next.char_count();
        if content_changed {
            self.update_contents(gtk::AccessibleTextContentChange::Remove, 0, old_len);
            imp.text_buffer.borrow_mut().take();
        }
        let used_bytes = next.used_bytes();
        let old = imp.text.replace(next);
        let spare = (!old.has_excess_capacity(used_bytes)).then_some(old);
        imp.spare_text.replace(spare);
        imp.text_revision.set(Some(key));
        if content_changed {
            self.update_contents(gtk::AccessibleTextContentChange::Insert, 0, new_len);
        }
        if content_changed || caret_changed {
            self.update_caret_position();
        }
        if content_changed || selection_changed {
            self.update_selection_bound();
        }
    }
    pub fn set_text_metrics(&self, cw: f32, ch: f32) {
        self.imp().metrics.set((cw, ch));
    }
    pub fn new() -> Self {
        let canvas: Self = glib::Object::new();
        canvas.set_focusable(true);
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        canvas.set_overflow(gtk::Overflow::Hidden);
        canvas
    }

    pub fn set_snapshot_func(&self, callback: impl Fn(&gtk::Snapshot, i32, i32) + 'static) {
        *self.imp().callback.borrow_mut() = Some(Box::new(callback));
        self.queue_draw();
    }

    pub fn set_scroll_func(&self, callback: impl Fn(u64) + 'static) {
        *self.imp().scroll_callback.borrow_mut() = Some(Box::new(callback));
    }

    pub fn set_scrollback(&self, scrollback: Scrollback, pending: bool) {
        self.imp().scrollback.set(scrollback);
        self.imp().scroll_pending.set(pending);
        self.sync_adjustment();
    }

    pub fn set_row_height(&self, height: f64) {
        let height = height.max(1.0);
        let previous = self.imp().row_height.replace(height);
        if previous != height {
            self.configure_adjustment(previous);
        }
    }

    fn sync_adjustment(&self) {
        self.configure_adjustment(self.imp().row_height.get());
    }

    fn configure_adjustment(&self, previous_row_height: f64) {
        let imp = self.imp();
        let Some(adjustment) = imp.vadjustment.borrow().clone() else {
            return;
        };
        let scrollback = imp.scrollback.get();
        let height = imp.row_height.get();
        // Preserve fractional rows through acknowledgements and font changes.
        // GTK handles touchpad deltas and momentum in logical pixels.
        let value = if imp.scroll_pending.get() || scrollback.offset == imp.scroll_row.get() {
            adjustment.value() / previous_row_height * height
        } else {
            scrollback.offset as f64 * height
        };
        imp.syncing.set(true);
        adjustment.configure(
            value,
            0.0,
            scrollback.total as f64 * height,
            height,
            scrollback.len.saturating_sub(1).max(1) as f64 * height,
            scrollback.len as f64 * height,
        );
        imp.scroll_row
            .set((adjustment.value() / height).round() as u64);
        imp.syncing.set(false);
    }
}
