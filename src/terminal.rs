use crate::{
    engine::{Engine, Frame},
    ipc,
    model::{self, Settings},
    render,
    search::Search,
    theme::Theme,
    title::{ForegroundProcess, PaneTitle},
};
use gtk::{gdk, glib, pango, prelude::*};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    cell::{Cell as ValueCell, RefCell},
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

pub struct Terminal {
    pub id: u64,
    pub root: gtk::Box,
    pub area: gtk::DrawingArea,
    pub title: gtk::Label,
    title_bar: gtk::CenterBox,
    status: gtk::Label,
    size_label: gtk::Label,
    size_revealer: gtk::Revealer,
    pub directory: RefCell<PathBuf>,
    pub exited: ValueCell<bool>,
    engine: RefCell<Engine>,
    master: Box<dyn MasterPty + Send>,
    child: RefCell<Option<Box<dyn Child + Send + Sync>>>,
    writer: mpsc::SyncSender<Vec<u8>>,
    write_errors: mpsc::Receiver<String>,
    receiver: mpsc::Receiver<Vec<u8>>,
    selection: RefCell<Option<(usize, usize)>>,
    drag_origin: ValueCell<(f64, f64)>,
    search: RefCell<Search>,
    search_revision: ValueCell<u64>,
    metrics: ValueCell<(f64, f64)>,
    grid: ValueCell<(u16, u16)>,
    grid_initialized: ValueCell<bool>,
    pending_size: ValueCell<Option<(u16, u16)>>,
    size_changed: ValueCell<Option<Instant>>,
    zoom: ValueCell<f64>,
    settings: Rc<RefCell<Settings>>,
    glyphs: RefCell<HashMap<([u8; 64], u8), pango::Layout>>,
    render_font: RefCell<Option<(pango::FontDescription, pango::Context, u32)>>,
    pid: Option<u32>,
    pane_title: RefCell<PaneTitle>,
    title_polled: ValueCell<Instant>,
    im: gtk::IMMulticontext,
    mouse_position: ValueCell<(f64, f64)>,
    mouse_pressed: ValueCell<i32>,
    theme: ValueCell<Theme>,
    chrome_colors: ValueCell<Option<([u8; 3], [u8; 3])>>,
    chrome_css: gtk::CssProvider,
}
impl Terminal {
    pub fn new(
        id: u64,
        tab: u64,
        directory: PathBuf,
        settings: Rc<RefCell<Settings>>,
        on_focus: Rc<dyn Fn(u64)>,
        on_close: Rc<dyn Fn(u64)>,
        application: &gtk::Application,
    ) -> anyhow::Result<Rc<Self>> {
        let pair = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = CommandBuilder::new(&shell);
        command.arg("-l");
        command.cwd(&directory);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "gmux");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        command.env("GMUX_SOCKET", ipc::socket_path());
        command.env("GMUX_TAB", tab.to_string());
        command.env("GMUX_PANE", id.to_string());
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            command.env(
                "PATH",
                format!(
                    "{}:{}",
                    parent.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        }
        let child = pair.slave.spawn_command(command)?;
        let pid = child.process_id();
        drop(pair.slave);
        let mut writer = pair.master.take_writer()?;
        let (write_sender, write_receiver) = mpsc::sync_channel::<Vec<u8>>(64);
        let (error_sender, write_errors) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(bytes) = write_receiver.recv() {
                if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                    let _ = error_sender.send(error.to_string());
                    break;
                }
            }
        });
        let mut reader = pair.master.try_clone_reader()?;
        let (sender, receiver) = mpsc::sync_channel(64);
        std::thread::spawn(move || {
            let mut buffer = [0; 16384];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sender.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("card");
        root.add_css_class("terminal-pane");
        root.set_widget_name(&format!("terminal-pane-{id}"));
        root.set_hexpand(true);
        root.set_vexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("pane-content");
        content.add_css_class("pane-clip");
        content.set_overflow(gtk::Overflow::Hidden);
        content.set_hexpand(true);
        content.set_vexpand(true);
        root.append(&content);
        let toolbar = gtk::CenterBox::new();
        toolbar.add_css_class("pane-toolbar");
        toolbar.set_visible(settings.borrow().show_pane_titles);
        let title = gtk::Label::builder()
            .label(model::directory_name(&directory))
            .xalign(0.5)
            .ellipsize(pango::EllipsizeMode::End)
            .hexpand(true)
            .build();
        title.add_css_class("caption");
        toolbar.set_center_widget(Some(&title));
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close pane (Ctrl+Shift+W)")
            .build();
        close.add_css_class("circular");
        close.add_css_class("flat");
        close.add_css_class("pane-close");
        close.set_valign(gtk::Align::Center);
        toolbar.set_end_widget(Some(&close));
        content.append(&toolbar);
        let area = gtk::DrawingArea::builder()
            .focusable(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        area.add_css_class("terminal-canvas");
        area.set_cursor_from_name(Some("text"));
        area.set_tooltip_text(Some("Terminal"));
        area.update_property(&[gtk::accessible::Property::Label("Terminal")]);
        let canvas = gtk::Overlay::builder()
            .child(&area)
            .hexpand(true)
            .vexpand(true)
            .build();
        let size_label = gtk::Label::new(None);
        size_label.add_css_class("pane-size");
        let size_revealer = gtk::Revealer::builder()
            .child(&size_label)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .transition_duration(150)
            .build();
        canvas.add_overlay(&size_revealer);
        canvas.set_measure_overlay(&size_revealer, false);
        canvas.set_clip_overlay(&size_revealer, true);
        content.append(&canvas);
        let status = gtk::Label::builder().xalign(1.0).visible(false).build();
        status.add_css_class("caption");
        status.add_css_class("dim-label");
        status.add_css_class("pane-status");
        content.append(&status);
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(&area));
        let mut engine = Engine::new()?;
        engine.set_cursor_style(settings.borrow().cursor_style)?;
        let terminal = Rc::new(Self {
            id,
            root,
            area,
            title,
            title_bar: toolbar,
            status,
            size_label,
            size_revealer,
            directory: RefCell::new(directory),
            engine: RefCell::new(engine),
            master: pair.master,
            child: RefCell::new(Some(child)),
            writer: write_sender,
            write_errors,
            receiver,
            selection: RefCell::new(None),
            drag_origin: ValueCell::new((0.0, 0.0)),
            search: RefCell::new(Search::default()),
            search_revision: ValueCell::new(0),
            metrics: ValueCell::new((9.6, 22.0)),
            grid: ValueCell::new((80, 24)),
            grid_initialized: ValueCell::new(false),
            pending_size: ValueCell::new(None),
            size_changed: ValueCell::new(None),
            settings,
            zoom: ValueCell::new(0.0),
            glyphs: RefCell::new(HashMap::new()),
            render_font: RefCell::new(None),
            pid,
            pane_title: RefCell::new(PaneTitle::default()),
            title_polled: ValueCell::new(Instant::now()),
            exited: ValueCell::new(false),
            im,
            mouse_position: ValueCell::new((0.0, 0.0)),
            mouse_pressed: ValueCell::new(0),
            theme: ValueCell::new(Theme::adwaita(adw::StyleManager::default().is_dark())),
            chrome_colors: ValueCell::new(None),
            chrome_css: gtk::CssProvider::new(),
        });
        gtk::style_context_add_provider_for_display(
            &terminal.root.display(),
            &terminal.chrome_css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        terminal.refresh_theme();
        let weak = Rc::downgrade(&terminal);
        terminal.area.set_draw_func(move |_, cr, w, h| {
            if let Some(t) = weak.upgrade() {
                t.draw(cr, w, h);
            }
        });
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(&terminal);
        focus.connect_enter(move |_| {
            if let Some(t) = weak.upgrade() {
                t.im.focus_in();
                if t.engine.borrow().mode(1004) {
                    let _ = t.send(b"\x1b[I");
                }
                on_focus(t.id);
            }
        });
        let weak = Rc::downgrade(&terminal);
        focus.connect_leave(move |_| {
            if let Some(t) = weak.upgrade() {
                t.im.focus_out();
                if t.engine.borrow().mode(1004) {
                    let _ = t.send(b"\x1b[O");
                }
                t.area.queue_draw();
            }
        });
        terminal.area.add_controller(focus);
        close.connect_clicked(move |_| on_close(id));
        let key = gtk::EventControllerKey::new();
        // Use the registered accelerators and GDK's keyboard-layout matching
        // for both press and release, instead of a separate modifier whitelist.
        let shortcuts = Rc::new(
            application
                .list_action_descriptions()
                .iter()
                .flat_map(|action| application.accels_for_action(action))
                .filter_map(gtk::accelerator_parse)
                .collect::<Vec<_>>(),
        );
        let press_shortcuts = shortcuts.clone();
        let pending_key = Rc::new(RefCell::new((String::new(), 0u32, 0u16)));
        let pending = pending_key.clone();
        let weak = Rc::downgrade(&terminal);
        terminal.im.connect_commit(move |_, text| {
            if let Some(t) = weak.upgrade() {
                let pending = pending.borrow();
                let bytes = t
                    .engine
                    .borrow_mut()
                    .key(&pending.0, pending.1, pending.2, false, text);
                if !bytes.is_empty() {
                    t.input(&bytes);
                }
            }
        });
        let weak = Rc::downgrade(&terminal);
        key.connect_key_pressed(move |controller, key, _, modifiers| {
            let Some(t) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if application_shortcut(controller, &press_shortcuts) {
                return glib::Propagation::Proceed;
            }
            if let Some(previous) = scroll_shortcut(key, modifiers) {
                let delta = if previous {
                    -(t.grid.get().1 as isize)
                } else {
                    t.grid.get().1 as isize
                };
                t.scroll(delta);
                return glib::Propagation::Stop;
            }
            let text = key
                .to_unicode()
                .filter(|c| !c.is_control())
                .map(|c| c.to_string())
                .unwrap_or_default();
            let cp = key.to_unicode().map(|c| c as u32).unwrap_or(0);
            *pending_key.borrow_mut() = (
                key.name().map(|n| n.to_string()).unwrap_or_default(),
                cp,
                mods(modifiers),
            );
            if !modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) && let Some(event) = controller.current_event()
                && t.im.filter_keypress(&event)
            {
                return glib::Propagation::Stop;
            }
            let bytes = t.engine.borrow_mut().key(
                key.name().as_deref().unwrap_or(""),
                cp,
                mods(modifiers),
                false,
                &text,
            );
            if !bytes.is_empty() {
                t.input(&bytes);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        let weak = Rc::downgrade(&terminal);
        key.connect_key_released(move |controller, key, _, modifiers| {
            if application_shortcut(controller, &shortcuts)
                || scroll_shortcut(key, modifiers).is_some()
            {
                return;
            }
            if let Some(t) = weak.upgrade() {
                let bytes = t.engine.borrow_mut().key(
                    key.name().as_deref().unwrap_or(""),
                    key.to_unicode().map(|c| c as u32).unwrap_or(0),
                    mods(modifiers),
                    true,
                    "",
                );
                if !bytes.is_empty() {
                    let _ = t.send(&bytes);
                }
            }
        });
        terminal.area.add_controller(key);
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&terminal);
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(t) = weak.upgrade() {
                let state = gesture.current_event_state();
                if !state.contains(gdk::ModifierType::SHIFT_MASK) && t.engine.borrow().mode(0) {
                    let button = match gesture.current_button() {
                        1 => 1,
                        2 => 3,
                        3 => 2,
                        _ => 0,
                    };
                    t.mouse_pressed.set(button);
                    t.mouse_position.set((x, y));
                    t.area.grab_focus();
                    t.mouse(0, button, state);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
        });
        let weak = Rc::downgrade(&terminal);
        click.connect_released(move |gesture, _, x, y| {
            if let Some(t) = weak.upgrade()
                && t.mouse_pressed.get() != 0
            {
                t.mouse_position.set((x, y));
                t.mouse(1, t.mouse_pressed.get(), gesture.current_event_state());
                t.mouse_pressed.set(0);
            }
        });
        terminal.area.add_controller(click);
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&terminal);
        motion.connect_motion(move |controller, x, y| {
            if let Some(t) = weak.upgrade() {
                t.mouse_position.set((x, y));
                let state = controller.current_event_state();
                if !state.contains(gdk::ModifierType::SHIFT_MASK) && t.engine.borrow().mode(0) {
                    t.mouse(2, t.mouse_pressed.get(), state);
                }
            }
        });
        terminal.area.add_controller(motion);
        let drag = gtk::GestureDrag::new();
        drag.set_button(1);
        let weak = Rc::downgrade(&terminal);
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                t.area.grab_focus();
                t.drag_origin.set((x, y));
                t.selection.borrow_mut().take();
                t.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(&terminal);
        drag.connect_drag_update(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                let (ox, oy) = t.drag_origin.get();
                // GestureDrag begins on press. Wait for intentional movement,
                // using GTK's configured threshold rather than selecting a click.
                if t.selection.borrow().is_none()
                    && !t.area.drag_check_threshold(
                        ox.round() as i32,
                        oy.round() as i32,
                        (ox + x).round() as i32,
                        (oy + y).round() as i32,
                    )
                {
                    return;
                }
                let start = t.cell_at(ox, oy);
                let end = t.cell_at(ox + x, oy + y);
                *t.selection.borrow_mut() = Some((start.min(end), start.max(end)));
                t.area.queue_draw();
            }
        });
        terminal.area.add_controller(drag);
        let middle = gtk::GestureClick::new();
        middle.set_button(2);
        let weak = Rc::downgrade(&terminal);
        middle.connect_pressed(move |_, _, _, _| {
            if let Some(t) = weak.upgrade() {
                t.paste_from(&t.area.primary_clipboard());
            }
        });
        terminal.area.add_controller(middle);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(&terminal);
        scroll.connect_scroll(move |controller, _, dy| {
            if let Some(t) = weak.upgrade() {
                let state = controller.current_event_state();
                if t.engine.borrow().mode(0) && !state.contains(gdk::ModifierType::SHIFT_MASK) {
                    // Mouse buttons four and five represent the wheel.
                    for _ in 0..(dy.abs().ceil() as usize).clamp(1, 8) {
                        t.mouse(0, if dy < 0.0 { 4 } else { 5 }, state);
                    }
                } else {
                    t.scroll((dy * 3.0).round() as isize);
                }
            }
            glib::Propagation::Stop
        });
        terminal.area.add_controller(scroll);
        Ok(terminal)
    }
    pub fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(!self.exited.get(), "This terminal's process has exited");
        self.writer
            .try_send(bytes.to_vec())
            .map_err(|error| anyhow::anyhow!("Could not send terminal input: {error}"))?;
        Ok(())
    }
    fn input(&self, bytes: &[u8]) {
        self.selection.borrow_mut().take();
        self.engine.borrow_mut().scroll(0, true);
        if let Err(error) = self.send(bytes) {
            self.show_status(&error.to_string());
        }
        self.area.queue_draw();
    }
    fn mouse(&self, action: i32, button: i32, modifiers: gdk::ModifierType) {
        let (x, y) = self.mouse_position.get();
        let (cw, ch) = self.metrics.get();
        let bytes = self.engine.borrow_mut().mouse(
            (x.max(0.0) as f32, y.max(0.0) as f32),
            (cw as u32, ch as u32),
            action,
            button,
            mods(modifiers),
            self.mouse_pressed.get() != 0,
        );
        if !bytes.is_empty() {
            let _ = self.send(&bytes);
        }
    }
    pub fn send_text(&self, text: &str, enter: bool) -> anyhow::Result<()> {
        let mut bytes = self.engine.borrow_mut().paste(text, false)?;
        if enter {
            bytes.push(b'\r');
        }
        self.send(&bytes)
    }
    pub fn tick(&self) -> Vec<String> {
        let now = Instant::now();
        if let Some((cols, rows)) = self.pending_size.take() {
            self.size_label.set_text(&format!("{cols} × {rows}"));
            self.size_revealer.set_reveal_child(true);
            self.size_changed.set(Some(now));
        }
        if self
            .size_changed
            .get()
            .is_some_and(|changed| now.duration_since(changed) >= Duration::from_millis(800))
        {
            self.size_revealer.set_reveal_child(false);
            self.size_changed.set(None);
        }
        if let Ok(error) = self.write_errors.try_recv() {
            self.show_status(&error);
        }
        let mut engine = self.engine.borrow_mut();
        let mut changed = engine.expire_render_hold();
        // Bound work per frame so a noisy process cannot block GTK.
        for _ in 0..32 {
            match self.receiver.try_recv() {
                Ok(bytes) => {
                    engine.write(&bytes);
                    changed = true;
                }
                Err(_) => break,
            }
        }
        let (replies, notifications) = engine.take_events();
        let title_update = engine.take_title_update();
        if !replies.is_empty() {
            let _ = self.send(&replies);
        }
        let mut colors = None;
        if changed {
            self.area.queue_draw();
            if engine.snapshot().is_ok() {
                colors = Some(engine.frame);
                self.refresh_search_frame(&engine);
            }
        }
        drop(engine);
        self.refresh_title(title_update);
        if let Some(frame) = colors {
            self.sync_chrome(frame);
        }
        if !self.exited.get()
            && let Some(Ok(Some(_))) = self.child.borrow_mut().as_mut().map(|c| c.try_wait())
        {
            self.exited.set(true);
        }
        notifications
    }
    fn show_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(true);
    }
    fn refresh_title(&self, title_update: Option<String>) {
        let mut title = self.pane_title.borrow_mut();
        if !self.exited.get() {
            let pid = self
                .master
                .process_group_leader()
                .filter(|pid| *pid > 0)
                .map(|pid| pid as u32)
                .or(self.pid);
            let now = Instant::now();
            if pid != title.foreground_pid()
                || title_update.is_some()
                || now.duration_since(self.title_polled.get()) >= Duration::from_millis(250)
            {
                title.update(pid.and_then(ForegroundProcess::read), title_update);
                self.title_polled.set(now);
            }
        }
        let text = title.display(&self.directory.borrow(), self.pid);
        if self.title.text() != text {
            self.title.set_text(&text);
        }
    }
    pub fn refresh_settings(&self) {
        self.title_bar
            .set_visible(self.settings.borrow().show_pane_titles);
        if let Err(error) = self
            .engine
            .borrow_mut()
            .set_cursor_style(self.settings.borrow().cursor_style)
        {
            eprintln!("{error}");
        }
        self.area.queue_draw();
    }
    pub fn refresh_theme(&self) {
        let theme = Theme::adwaita(adw::StyleManager::default().is_dark());
        let frame = {
            let mut engine = self.engine.borrow_mut();
            if let Err(error) = engine.set_theme(&theme).and_then(|_| engine.snapshot()) {
                eprintln!("{error}");
                return;
            }
            engine.frame
        };
        self.theme.set(theme);
        self.sync_chrome(frame);
        self.area.queue_draw();
    }
    fn sync_chrome(&self, frame: Frame) {
        let colors = (frame.background, frame.foreground);
        if self.chrome_colors.get() == Some(colors) {
            return;
        }
        self.chrome_colors.set(Some(colors));
        let [br, bg, bb] = frame.background;
        let [fr, fg, fb] = frame.foreground;
        self.chrome_css.load_from_string(&format!(
            "#terminal-pane-{} {{ --terminal-bg: rgb({br}, {bg}, {bb}); --terminal-fg: rgb({fr}, {fg}, {fb}); }}",
            self.id
        ));
    }
    pub fn refresh_directory(&self) {
        let leader = self
            .master
            .process_group_leader()
            .map(|p| p as u32)
            .or(self.pid);
        let actual_directory =
            leader.and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok());
        if let Some(directory) = actual_directory {
            *self.directory.borrow_mut() = directory;
        } else {
            // OSC metadata persists until another report arrives. Prefer the
            // live local process directory so an old report cannot undo cd.
            let pwd = self.engine.borrow().metadata(true);
            if let Ok(path) = glib::filename_from_uri(&pwd)
                && path.0.is_dir()
            {
                *self.directory.borrow_mut() = path.0;
            }
        }
        self.title
            .set_tooltip_text(Some(&self.directory.borrow().display().to_string()));
        self.refresh_title(None);
    }
    pub fn busy(&self) -> bool {
        !self.exited.get()
            && self
                .master
                .process_group_leader()
                .zip(self.pid)
                .is_some_and(|(fg, pid)| fg as u32 != pid)
    }
    pub fn set_active(&self, active: bool) {
        if active {
            self.root.add_css_class("active-pane");
        } else {
            self.root.remove_css_class("active-pane");
        }
        self.area.queue_draw();
    }
    pub fn text(&self) -> anyhow::Result<String> {
        self.engine.borrow_mut().text()
    }
    pub fn copy(&self) -> bool {
        let Some((start, end)) = *self.selection.borrow() else {
            return false;
        };
        let mut e = self.engine.borrow_mut();
        if e.snapshot().is_err() {
            return false;
        }
        let cols = e.frame.cols as usize;
        let mut text = String::new();
        for index in start..=end.min(e.cells.len().saturating_sub(1)) {
            if index > start && index % cols == 0 {
                while text.ends_with(' ') {
                    text.pop();
                }
                text.push('\n');
            }
            if e.cells[index].flags & 64 != 0 {
                continue;
            }
            let glyph = e.cells[index].text();
            text.push_str(if glyph.is_empty() { " " } else { glyph });
        }
        self.area.clipboard().set_text(text.trim_end());
        true
    }
    pub fn paste(self: &Rc<Self>) {
        self.paste_from(&self.area.clipboard());
    }
    fn paste_from(self: &Rc<Self>, clipboard: &gdk::Clipboard) {
        let weak = Rc::downgrade(self);
        clipboard.read_text_async(None::<&gtk::gio::Cancellable>, move |result| {
            if let (Some(t), Ok(Some(text))) = (weak.upgrade(), result) {
                let bytes = t.engine.borrow_mut().paste(&text, true);
                match bytes {
                    Ok(bytes) => t.input(&bytes),
                    Err(error) => t.show_status(&error.to_string()),
                }
            }
        });
    }
    pub fn scroll(&self, delta: isize) {
        self.selection.borrow_mut().take();
        self.engine.borrow_mut().scroll(delta, false);
        self.area.queue_draw();
    }
    pub fn search(&self, query: &str) {
        let mut search = self.search.borrow_mut();
        search.set_query(query);
        let mut engine = self.engine.borrow_mut();
        if engine.snapshot().is_ok() {
            search.refresh(&engine.cells, engine.frame.cols as usize);
            self.search_revision.set(engine.revision());
        }
        self.area.queue_draw();
    }
    pub fn search_advance(&self, previous: bool) {
        self.search.borrow_mut().advance(previous);
        self.area.queue_draw();
    }
    pub fn search_status(&self) -> (usize, usize) {
        self.search.borrow().status()
    }
    fn refresh_search_frame(&self, engine: &Engine) {
        // Another consumer, such as Read or Copy, may already have refreshed
        // the cell buffer. Track its revision rather than the last call's dirty flag.
        if self.search_revision.replace(engine.revision()) != engine.revision() {
            self.search
                .borrow_mut()
                .refresh(&engine.cells, engine.frame.cols as usize);
        }
    }
    pub fn zoom(&self, delta: f64) {
        self.zoom.set(if delta == 0.0 {
            0.0
        } else {
            (self.zoom.get() + delta).clamp(-6.0, 20.0)
        });
        self.area.queue_draw();
    }
    fn cell_at(&self, x: f64, y: f64) -> usize {
        let (cw, ch) = self.metrics.get();
        let (cols, rows) = self.grid.get();
        let x = (x / cw).floor().max(0.0) as usize;
        let y = (y / ch).floor().max(0.0) as usize;
        y.min(rows as usize - 1) * cols as usize + x.min(cols as usize - 1)
    }
    fn draw(&self, cr: &gtk::cairo::Context, width: i32, height: i32) {
        let settings = self.settings.borrow();
        let font = crate::font::description(&settings, self.zoom.get());
        drop(settings);
        let context = self.area.pango_context();
        let font_key = (font.clone(), context.clone(), context.serial());
        if self.render_font.borrow().as_ref() != Some(&font_key) {
            let layout = pango::Layout::new(&context);
            layout.set_font_description(Some(&font));
            layout.set_text("M");
            let (width, height) = layout.pixel_size();
            self.metrics
                .set((width.max(1) as f64, (height + 2).max(1) as f64));
            *self.render_font.borrow_mut() = Some(font_key);
            self.glyphs.borrow_mut().clear();
        }
        let (cw, ch) = self.metrics.get();
        // GTK supplies the small canvas margins. Use its full allocation so
        // the grid, selections, and pointer coordinates share the same origin.
        let cols = ((width as f64 / cw).floor() as i32).clamp(1, 1000) as u16;
        let rows = ((height as f64 / ch).floor() as i32).clamp(1, 1000) as u16;
        let mut e = self.engine.borrow_mut();
        let initialized = self.grid_initialized.get();
        if self.grid.get() != (cols, rows) {
            if let Err(error) = e.resize(cols, rows, cw as u32, ch as u32) {
                eprintln!("{error}");
                return;
            }
            self.grid.set((cols, rows));
            self.selection.borrow_mut().take();
            let _ = self.master.resize(PtySize {
                cols,
                rows,
                pixel_width: width.max(0) as u16,
                pixel_height: height.max(0) as u16,
            });
            if initialized && !self.exited.get() {
                // The next tick updates the overlay outside GTK's draw callback.
                self.pending_size.set(Some((cols, rows)));
            }
        }
        self.grid_initialized.set(true);
        let Ok(_) = e.snapshot() else {
            return;
        };
        let frame = e.frame;
        let cursor_visible = frame.cursor_visible != 0 && !self.exited.get();
        let focused = self.area.has_focus();
        let block_cursor = cursor_visible && focused && frame.cursor_style == 1;
        let cursor_index = frame.cursor_y as usize * cols as usize + frame.cursor_x as usize;
        let cursor_rect = (
            frame.cursor_x as f64 * cw,
            frame.cursor_y as f64 * ch,
            cw,
            ch,
        );
        let theme = self.theme.get();
        color(cr, frame.background, 1.0);
        let _ = cr.paint();
        let selection = *self.selection.borrow();
        self.refresh_search_frame(&e);
        let mut search = self.search.borrow_mut();
        let matches = search.highlights(e.cells.len());
        let mut glyphs = self.glyphs.borrow_mut();
        if glyphs.len() > 4096 {
            glyphs.clear();
        }
        // Paint all backgrounds before glyphs. A wide glyph can cover its
        // trailing cell, whose selection/background must not erase it.
        let _ = cr.save();
        cr.set_antialias(gtk::cairo::Antialias::None);
        for (index, cell) in e.cells.iter().enumerate() {
            let x = (index % cols as usize) as f64 * cw;
            let y = (index / cols as usize) as f64 * ch;
            let selected = selection.is_some_and(|(start, end)| index >= start && index <= end);
            if selected || matches[index] != 0 || cell.bg != frame.background {
                if selected || matches[index] == 2 {
                    color(cr, theme.selection_background, 1.0);
                } else if matches[index] == 1 {
                    color(cr, cell.bg, 1.0);
                    cr.rectangle(x, y, cw, ch);
                    let _ = cr.fill();
                    color(cr, theme.palette[11], 0.3);
                } else {
                    color(cr, cell.bg, 1.0);
                }
                cr.rectangle(x, y, cw, ch);
                let _ = cr.fill();
            }
        }
        let _ = cr.restore();
        // Paint a solid block cursor behind the character, then draw that
        // character in the cell's background color so it remains readable.
        if block_cursor {
            color(cr, frame.cursor, 1.0);
            render::draw_cursor(cr, frame.cursor_style, focused, cursor_rect);
        }
        for (index, cell) in e.cells.iter().enumerate() {
            let x = (index % cols as usize) as f64 * cw;
            let y = (index / cols as usize) as f64 * ch;
            let text = cell.text();
            let has_glyph = !text.is_empty() && text != " ";
            if cell.flags & 32 != 0 || !has_glyph && cell.flags & (8 | 16) == 0 {
                continue;
            }
            let selected = selection.is_some_and(|(start, end)| index >= start && index <= end);
            let under_cursor = block_cursor && index == cursor_index;
            color(
                cr,
                if under_cursor {
                    cell.bg
                } else if selected || matches[index] == 2 {
                    theme.selection_foreground
                } else {
                    cell.fg
                },
                if cell.flags & 4 != 0 && !under_cursor {
                    0.5
                } else {
                    1.0
                },
            );
            // Blank cells and wide-character tails can still have decorations.
            if has_glyph && !render::draw_graphic(cr, text, (x, y, cw, ch)) {
                let key = (cell.text, cell.flags & 3);
                let glyph = glyphs.entry(key).or_insert_with(|| {
                    let layout = pango::Layout::new(&context);
                    let mut font = font.clone();
                    if cell.flags & 1 != 0 {
                        font.set_weight(pango::Weight::Bold);
                    }
                    if cell.flags & 2 != 0 {
                        font.set_style(pango::Style::Italic);
                    }
                    layout.set_font_description(Some(&font));
                    layout.set_text(text);
                    layout
                });
                cr.move_to(x, y + 1.0);
                pangocairo::functions::show_layout(cr, glyph);
            }
            if cell.flags & 8 != 0 {
                cr.rectangle(x, y + ch - 2.0, cw, 1.0);
                let _ = cr.fill();
            }
            if cell.flags & 16 != 0 {
                cr.rectangle(x, y + ch / 2.0, cw, 1.0);
                let _ = cr.fill();
            }
        }
        if cursor_visible {
            let (x, y, _, _) = cursor_rect;
            self.im.set_cursor_location(&gdk::Rectangle::new(
                x as i32, y as i32, cw as i32, ch as i32,
            ));
            if !block_cursor {
                color(cr, frame.cursor, if focused { 1.0 } else { 0.35 });
                render::draw_cursor(cr, frame.cursor_style, focused, cursor_rect);
            }
        }
    }
}
fn scroll_shortcut(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<bool> {
    if !modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
        return None;
    }
    match key {
        gdk::Key::Page_Up => Some(true),
        gdk::Key::Page_Down => Some(false),
        _ => None,
    }
}
fn application_shortcut(
    controller: &gtk::EventControllerKey,
    shortcuts: &[(gdk::Key, gdk::ModifierType)],
) -> bool {
    controller
        .current_event()
        .and_then(|event| event.downcast::<gdk::KeyEvent>().ok())
        .is_some_and(|event| {
            shortcuts
                .iter()
                .any(|&(key, modifiers)| event.matches(key, modifiers) != gdk::KeyMatch::None)
        })
}
fn color(cr: &gtk::cairo::Context, rgb: [u8; 3], alpha: f64) {
    cr.set_source_rgba(
        rgb[0] as f64 / 255.0,
        rgb[1] as f64 / 255.0,
        rgb[2] as f64 / 255.0,
        alpha,
    );
}
fn mods(m: gdk::ModifierType) -> u16 {
    (m.contains(gdk::ModifierType::SHIFT_MASK) as u16)
        | ((m.contains(gdk::ModifierType::CONTROL_MASK) as u16) << 1)
        | ((m.contains(gdk::ModifierType::ALT_MASK) as u16) << 2)
        | ((m.contains(gdk::ModifierType::SUPER_MASK) as u16) << 3)
}
impl Drop for Terminal {
    fn drop(&mut self) {
        gtk::style_context_remove_provider_for_display(&self.root.display(), &self.chrome_css);
        if !self.exited.get() {
            if let Some(fg) = self.master.process_group_leader().filter(|pid| *pid > 1) {
                unsafe {
                    libc::kill(-fg, libc::SIGHUP);
                }
            }
            if let Some(mut child) = self.child.get_mut().take() {
                let _ = child.kill();
                // Reap asynchronously, keeping shutdown responsive.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
}
