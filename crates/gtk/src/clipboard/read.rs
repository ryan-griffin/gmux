//! Permission and asynchronous desktop reads for one frontend window.
use gmux_client::remote::{RemoteSession, SessionHandle};
use gmux_protocol::{
    clipboard as protocol,
    clipboard::{
        Destination, ReadDescriptor, ReadEvent, ReadRequest, ReadResult, ReadStatus, Representation,
    },
    transport::StreamCommand,
};
use gtk::gio;
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::Instant,
};

/// One operation per desktop window. Clipboard bytes are never prefetched.
pub(crate) struct Desktop {
    clipboards: [gdk::Clipboard; 2],
    epochs: [Cell<u64>; 2],
    serial: Cell<u64>,
    pastes: RefCell<HashMap<u64, LocalPaste>>,
    sessions: RefCell<Vec<Weak<RefCell<RemoteSession>>>>,
    active: RefCell<Option<Operation>>,
    handlers: RefCell<Vec<(gdk::Clipboard, glib::SignalHandlerId)>>,
}
struct LocalPaste {
    serial: u64,
    destination: Destination,
    epoch: u64,
    expires: Instant,
    session: Weak<RefCell<RemoteSession>>,
}
fn consume_paste(
    pastes: &mut HashMap<u64, LocalPaste>,
    pane: u64,
    destination: Destination,
    epoch: u64,
    serial: Option<u64>,
    session: &Weak<RefCell<RemoteSession>>,
    now: Instant,
) -> bool {
    let valid = pastes.get(&pane).is_some_and(|p| {
        p.destination == destination
            && p.session.ptr_eq(session)
            && p.epoch == epoch
            && p.expires > now
            && serial.is_none_or(|serial| serial == p.serial)
    });
    if valid {
        pastes.remove(&pane);
    }
    valid
}
struct Operation {
    pane: u64,
    id: u64,
    destination: Destination,
    session: Weak<RefCell<RemoteSession>>,
    controls: super::Banner,
    cancel: gio::Cancellable,
    started: bool,
    paste: bool,
}
impl Drop for Desktop {
    fn drop(&mut self) {
        if let Some(op) = self.active.get_mut().take() {
            op.cancel.cancel();
        }
        for (clipboard, handler) in self.handlers.get_mut().drain(..) {
            clipboard.disconnect(handler);
        }
    }
}
impl Desktop {
    pub(crate) fn new(standard: &gdk::Clipboard, primary: &gdk::Clipboard) -> Rc<Self> {
        let desktop = Rc::new(Self {
            clipboards: [standard.clone(), primary.clone()],
            epochs: [Cell::new(1), Cell::new(1)],
            serial: Cell::new(0),
            pastes: RefCell::default(),
            sessions: RefCell::default(),
            active: RefCell::default(),
            handlers: RefCell::default(),
        });
        for destination in [Destination::Clipboard, Destination::Primary] {
            let weak = Rc::downgrade(&desktop);
            let clipboard = desktop.clipboard(destination);
            let handler = clipboard.connect_changed(move |_| {
                let Some(desktop) = weak.upgrade() else {
                    return;
                };
                let epoch = &desktop.epochs[destination as usize];
                desktop
                    .pastes
                    .borrow_mut()
                    .retain(|_, p| p.destination != destination);
                epoch.set(
                    epoch
                        .get()
                        .checked_add(1)
                        .expect("Clipboard epoch exhausted"),
                );
                if desktop
                    .active
                    .borrow()
                    .as_ref()
                    .is_some_and(|op| op.started && op.destination == destination)
                {
                    desktop.finish(ReadStatus::IoError, true);
                }
                desktop.sessions.borrow_mut().retain(|weak| {
                    let Some(session) = weak.upgrade() else {
                        return false;
                    };
                    let _ = session.borrow().clipboard_changed(destination, epoch.get());
                    true
                });
            });
            desktop.handlers.borrow_mut().push((clipboard, handler));
        }
        desktop
    }
    pub(crate) fn clipboard(&self, destination: Destination) -> gdk::Clipboard {
        self.clipboards[destination as usize].clone()
    }
    pub(crate) fn track(&self, session: &SessionHandle) {
        self.sessions
            .borrow_mut()
            .retain(|weak| weak.strong_count() > 0);
        self.sessions.borrow_mut().push(Rc::downgrade(session));
        for destination in [Destination::Clipboard, Destination::Primary] {
            let _ = session
                .borrow()
                .clipboard_changed(destination, self.epochs[destination as usize].get());
        }
    }
    fn targets(&self, destination: Destination) -> Vec<String> {
        let formats = self
            .clipboard(destination)
            .formats()
            .union_serialize_mime_types();
        let mut result = Vec::new();
        let mut bytes = 0;
        for mime in formats.mime_types() {
            let mime = mime.as_str();
            if !protocol::valid_mime(mime)
                || !mime.contains('/')
                || result.iter().any(|s| s == mime)
            {
                continue;
            }
            if result.len() == protocol::MAX_TARGETS || bytes + mime.len() + 1 > 4096 {
                break;
            }
            bytes += mime.len() + 1;
            result.push(mime.to_owned());
        }
        result
    }
    pub(crate) fn paste_intent(
        self: &Rc<Self>,
        terminal: &Rc<crate::terminal::Terminal>,
        destination: Destination,
    ) -> anyhow::Result<()> {
        self.pastes.borrow_mut().remove(&terminal.id);
        // A deliberate paste takes precedence over unanswered clipboard work
        // anywhere in this window, including a prompt in another split pane.
        self.finish(ReadStatus::Busy, true);
        let serial = self
            .serial
            .get()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Paste sequence exhausted"))?;
        self.serial.set(serial);
        terminal
            .session
            .borrow()
            .clipboard_command(StreamCommand::Scroll {
                delta: 0,
                bottom: true,
            })?;
        let now = Instant::now();
        let epoch = self.epochs[destination as usize].get();
        {
            let mut pastes = self.pastes.borrow_mut();
            pastes.retain(|_, p| p.expires > now);
            if pastes.len() >= 16
                && let Some(oldest) = pastes
                    .iter()
                    .min_by_key(|(_, p)| p.expires)
                    .map(|(&id, _)| id)
            {
                pastes.remove(&oldest);
            }
            pastes.insert(
                terminal.id,
                LocalPaste {
                    serial,
                    destination,
                    epoch,
                    expires: now + protocol::TRANSFER_TIMEOUT,
                    session: Rc::downgrade(&terminal.session),
                },
            );
        }
        let result = terminal
            .session
            .borrow()
            .clipboard_command(StreamCommand::UserPaste {
                serial,
                destination,
                epoch,
                mimes: self.targets(destination),
            });
        if result.is_err() {
            self.pastes.borrow_mut().remove(&terminal.id);
        }
        result
    }
    pub(crate) fn cancel_pane(&self, pane: u64) {
        self.pastes.borrow_mut().remove(&pane);
        if self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|op| op.pane == pane)
        {
            self.finish(ReadStatus::Busy, true);
        }
    }
    fn clear_controls(controls: &super::Banner) {
        controls.dismiss();
    }

    fn finish(&self, status: ReadStatus, send: bool) {
        let Some(op) = self.active.borrow_mut().take() else {
            return;
        };
        op.cancel.cancel();
        Self::clear_controls(&op.controls);
        if send
            && !op.paste
            && let Some(session) = op.session.upgrade()
        {
            let _ = session.borrow().clipboard_reply(
                ReadResult {
                    id: op.id,
                    status,
                    epoch: 0,
                    available: vec![],
                    contents: vec![],
                },
                vec![],
            );
        }
    }
    fn fail(session: &SessionHandle, id: u64, status: ReadStatus) {
        let _ = session.borrow().clipboard_reply(
            ReadResult {
                id,
                status,
                epoch: 0,
                available: vec![],
                contents: vec![],
            },
            vec![],
        );
    }
    pub(crate) fn handle(
        self: &Rc<Self>,
        terminal: &Rc<crate::terminal::Terminal>,
        event: ReadEvent,
        remote: Option<&str>,
        active_window: bool,
    ) {
        match event {
            ReadEvent::Cancel { id } => {
                if self
                    .active
                    .borrow()
                    .as_ref()
                    .is_some_and(|op| op.pane == terminal.id && op.id == id && !op.paste)
                {
                    self.finish(ReadStatus::Denied, false);
                }
            }
            ReadEvent::PasteText {
                serial,
                destination,
                epoch,
            } => {
                if epoch != self.epochs[destination as usize].get()
                    || self.active.borrow().is_some()
                    || !consume_paste(
                        &mut self.pastes.borrow_mut(),
                        terminal.id,
                        destination,
                        epoch,
                        Some(serial),
                        &Rc::downgrade(&terminal.session),
                        Instant::now(),
                    )
                {
                    return;
                }
                self.active.replace(Some(Operation {
                    pane: terminal.id,
                    id: serial,
                    destination,
                    session: Rc::downgrade(&terminal.session),
                    controls: terminal.clipboard_controls.clone(),
                    cancel: gio::Cancellable::new(),
                    started: false,
                    paste: true,
                }));
                self.read(
                    terminal,
                    ReadRequest {
                        id: serial,
                        destination,
                        mimes: vec!["text/plain;charset=utf-8".into(), "text/plain".into()],
                        list: false,
                        name: String::new(),
                        granted: true,
                        epoch: Some(epoch),
                    },
                    true,
                );
            }
            ReadEvent::Request { request } => {
                if request.validate().is_err() {
                    Self::fail(&terminal.session, request.id, ReadStatus::Unsupported);
                    return;
                }
                if request.mimes.is_empty() {
                    let available = if request.list {
                        self.targets(request.destination)
                    } else {
                        vec![]
                    };
                    let _ = terminal.session.borrow().clipboard_reply(
                        ReadResult {
                            id: request.id,
                            status: ReadStatus::Success,
                            epoch: self.epochs[request.destination as usize].get(),
                            available,
                            contents: vec![],
                        },
                        vec![],
                    );
                    return;
                }
                if self.active.borrow().is_some() {
                    Self::fail(&terminal.session, request.id, ReadStatus::Busy);
                    return;
                }
                if !active_window || !terminal.root.is_mapped() {
                    Self::fail(&terminal.session, request.id, ReadStatus::Denied);
                    return;
                }
                if request.granted
                    && (request.epoch != Some(self.epochs[request.destination as usize].get())
                        || !consume_paste(
                            &mut self.pastes.borrow_mut(),
                            terminal.id,
                            request.destination,
                            self.epochs[request.destination as usize].get(),
                            None,
                            &Rc::downgrade(&terminal.session),
                            Instant::now(),
                        ))
                {
                    Self::fail(&terminal.session, request.id, ReadStatus::Denied);
                    return;
                }
                self.active.replace(Some(Operation {
                    pane: terminal.id,
                    id: request.id,
                    destination: request.destination,
                    session: Rc::downgrade(&terminal.session),
                    controls: terminal.clipboard_controls.clone(),
                    cancel: gio::Cancellable::new(),
                    started: false,
                    paste: false,
                }));
                let weak = Rc::downgrade(self);
                let pane = terminal.id;
                let id = request.id;
                glib::timeout_add_local_once(protocol::READ_TIMEOUT, move || {
                    if let Some(desktop) = weak.upgrade()
                        && desktop
                            .active
                            .borrow()
                            .as_ref()
                            .is_some_and(|op| op.pane == pane && op.id == id && !op.paste)
                    {
                        desktop.finish(ReadStatus::Busy, true);
                    }
                });
                if request.granted {
                    self.read(terminal, request, false);
                    return;
                }
                let destination = if request.destination == Destination::Primary {
                    "primary selection"
                } else {
                    "clipboard"
                };
                let text = match remote {
                    Some(name) => format!("Share your {destination} with a program on {name}?"),
                    None => format!("Allow a program in this pane to read your {destination}?"),
                };
                let prompt = super::prompt::Prompt::new(&text);
                let deny = prompt.deny().clone();
                let allow = prompt.allow().clone();
                terminal.clipboard_controls.present(&prompt);
                let weak = Rc::downgrade(self);
                let pane = terminal.id;
                let id = request.id;
                deny.connect_clicked(move |_| {
                    if let Some(desktop) = weak.upgrade()
                        && desktop.active.borrow().as_ref().is_some_and(|op| {
                            op.pane == pane && op.id == id && !op.paste && !op.started
                        })
                    {
                        desktop.finish(ReadStatus::Denied, true);
                    }
                });
                let weak = Rc::downgrade(self);
                let pane = Rc::downgrade(terminal);
                allow.connect_clicked(move |_| {
                    if let (Some(desktop), Some(terminal)) = (weak.upgrade(), pane.upgrade()) {
                        desktop.read(&terminal, request.clone(), false);
                    }
                });
            }
        }
    }
    fn read(
        self: &Rc<Self>,
        terminal: &Rc<crate::terminal::Terminal>,
        request: ReadRequest,
        paste: bool,
    ) {
        let (cancel, epoch) = {
            let mut active = self.active.borrow_mut();
            let Some(op) = active.as_mut().filter(|op| {
                op.pane == terminal.id && op.id == request.id && op.paste == paste && !op.started
            }) else {
                return;
            };
            op.started = true;
            Self::clear_controls(&op.controls);
            (
                op.cancel.clone(),
                self.epochs[request.destination as usize].get(),
            )
        };
        let available = self.targets(request.destination);
        let clipboard = self.clipboard(request.destination);
        let wanted: Vec<_> =
            request
                .mimes
                .iter()
                .filter(|m| available.contains(m))
                .fold(Vec::new(), |mut v, m| {
                    if !v.contains(m) {
                        v.push(m.clone());
                    }
                    v
                });
        let weak = Rc::downgrade(self);
        let pane = Rc::downgrade(terminal);
        let timed = cancel.clone();
        glib::timeout_add_local_once(protocol::TRANSFER_TIMEOUT, move || timed.cancel());
        glib::spawn_future_local(async move {
            let operation = async {
                let mut contents = Vec::new();
                let mut total = 0;
                for mime in wanted {
                    let (stream, actual) = clipboard
                        .read_future(&[mime.as_str()], glib::Priority::DEFAULT)
                        .await?;
                    anyhow::ensure!(actual == mime, "Unexpected clipboard format");
                    let mut data = Vec::new();
                    loop {
                        let bytes = stream
                            .read_bytes_future(
                                protocol::READ_CHUNK.min(protocol::MAX_READ_BYTES - total + 1),
                                glib::Priority::DEFAULT,
                            )
                            .await?;
                        if bytes.is_empty() {
                            break;
                        }
                        anyhow::ensure!(
                            bytes.len() <= protocol::MAX_READ_BYTES - total,
                            "Clipboard is too large"
                        );
                        total += bytes.len();
                        data.extend_from_slice(&bytes);
                    }
                    let _ = stream.close_future(glib::Priority::DEFAULT).await;
                    if mime == "text/plain" || mime == "text/plain;charset=utf-8" {
                        anyhow::ensure!(
                            !data.contains(&0) && std::str::from_utf8(&data).is_ok(),
                            "Invalid clipboard text"
                        );
                    }
                    contents.push(Representation { mime, data });
                    if paste {
                        break;
                    }
                }
                Ok::<_, anyhow::Error>(contents)
            };
            let outcome = gio::CancellableFuture::new(operation, cancel).await;
            let (Some(desktop), Some(terminal)) = (weak.upgrade(), pane.upgrade()) else {
                return;
            };
            if !desktop.active.borrow().as_ref().is_some_and(|op| {
                op.pane == terminal.id && op.id == request.id && op.paste == paste
            }) {
                return;
            }
            if desktop.epochs[request.destination as usize].get() != epoch {
                desktop.finish(ReadStatus::IoError, true);
                return;
            }
            match outcome {
                Ok(Ok(contents)) => {
                    let op = desktop.active.borrow_mut().take().unwrap();
                    Self::clear_controls(&op.controls);
                    let result = if paste {
                        let text = contents
                            .into_iter()
                            .next()
                            .map(|c| String::from_utf8(c.data).unwrap())
                            .unwrap_or_default();
                        terminal
                            .session
                            .borrow()
                            .clipboard_command(StreamCommand::UserPasteText {
                                serial: request.id,
                                epoch,
                                text,
                            })
                    } else {
                        let result = ReadResult {
                            id: request.id,
                            status: ReadStatus::Success,
                            epoch,
                            available: if request.list { available } else { vec![] },
                            contents: contents
                                .iter()
                                .map(|c| ReadDescriptor {
                                    mime: c.mime.clone(),
                                    len: c.data.len(),
                                })
                                .collect(),
                        };
                        terminal.session.borrow().clipboard_reply(result, contents)
                    };
                    if let Err(error) = result {
                        eprintln!("Could not deliver clipboard read: {error}");
                    }
                }
                Ok(Err(error)) => {
                    if paste {
                        terminal.show_status(&format!("Could not read clipboard: {error}"));
                    }
                    desktop.finish(ReadStatus::IoError, true);
                }
                Err(_) => {
                    if paste {
                        terminal.show_status("Clipboard read timed out");
                    }
                    desktop.finish(ReadStatus::Busy, true);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_claims_require_a_local_one_use_paste_with_matching_scope() {
        let now = Instant::now();
        let mut pastes = HashMap::new();
        let session = Weak::new();
        assert!(!consume_paste(
            &mut pastes,
            7,
            Destination::Clipboard,
            3,
            None,
            &session,
            now
        ));
        pastes.insert(
            7,
            LocalPaste {
                serial: 42,
                destination: Destination::Clipboard,
                epoch: 3,
                expires: now + protocol::TRANSFER_TIMEOUT,
                session: session.clone(),
            },
        );
        assert!(!consume_paste(
            &mut pastes,
            8,
            Destination::Clipboard,
            3,
            None,
            &session,
            now
        ));
        assert!(!consume_paste(
            &mut pastes,
            7,
            Destination::Primary,
            3,
            None,
            &session,
            now
        ));
        assert!(!consume_paste(
            &mut pastes,
            7,
            Destination::Clipboard,
            4,
            None,
            &session,
            now
        ));
        assert!(!consume_paste(
            &mut pastes,
            7,
            Destination::Clipboard,
            3,
            Some(41),
            &session,
            now
        ));
        assert!(consume_paste(
            &mut pastes,
            7,
            Destination::Clipboard,
            3,
            Some(42),
            &session,
            now
        ));
        assert!(!consume_paste(
            &mut pastes,
            7,
            Destination::Clipboard,
            3,
            None,
            &session,
            now
        ));
        pastes.insert(
            7,
            LocalPaste {
                serial: 43,
                destination: Destination::Primary,
                epoch: 4,
                expires: now,
                session: session.clone(),
            },
        );
        assert!(!consume_paste(
            &mut pastes,
            7,
            Destination::Primary,
            4,
            Some(43),
            &session,
            now
        ));
    }
}
