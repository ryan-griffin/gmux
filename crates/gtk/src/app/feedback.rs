//! Transient failures and the active tab's ongoing connection state.
use super::*;
use crate::client::connections::ConnectionStatus;
use std::time::Instant;

#[derive(Default)]
pub(super) struct Errors {
    active: RefCell<Option<adw::Toast>>,
    recent: RefCell<Option<RecentError>>,
}

struct RecentError {
    pane: u64,
    title: String,
    details: String,
    shown_at: Instant,
}

impl Errors {
    pub(super) fn show(
        self: &Rc<Self>,
        window: &adw::ApplicationWindow,
        overlay: &adw::ToastOverlay,
        pane: u64,
        title: &str,
        details: &str,
    ) {
        if self.recent.borrow().as_ref().is_some_and(|last| {
            last.pane == pane
                && last.title == title
                && last.details == details
                && (self.active.borrow().is_some()
                    || last.shown_at.elapsed() < Duration::from_secs(5))
        }) {
            return;
        }
        *self.recent.borrow_mut() = Some(RecentError {
            pane,
            title: title.into(),
            details: details.into(),
            shown_at: Instant::now(),
        });
        // Keep at most one pending error. A failing input stream must not fill
        // the overlay's queue or replay stale failures after it recovers.
        let previous = self.active.borrow_mut().take();
        if let Some(previous) = previous {
            previous.dismiss();
        }
        let toast = adw::Toast::builder()
            .title(title)
            .use_markup(false)
            .button_label("Details")
            .timeout(5)
            .build();
        let parent = window.downgrade();
        let title = title.to_owned();
        let details = details.to_owned();
        toast.connect_button_clicked(move |_| {
            let Some(parent) = parent.upgrade() else {
                return;
            };
            let dialog = adw::AlertDialog::builder()
                .heading(&title)
                .body(&details)
                .body_use_markup(false)
                .heading_use_markup(false)
                .build();
            dialog.add_response("close", "Close");
            dialog.set_close_response("close");
            dialog.set_default_response(Some("close"));
            dialog.present(Some(&parent));
        });
        let weak = Rc::downgrade(self);
        toast.connect_dismissed(move |toast| {
            if let Some(errors) = weak.upgrade() {
                let mut active = errors.active.borrow_mut();
                if active.as_ref() == Some(toast) {
                    active.take();
                }
            }
        });
        *self.active.borrow_mut() = Some(toast.clone());
        overlay.add_toast(toast);
    }
}

impl Ui {
    pub(super) fn pane_error(
        &self,
        pane: u64,
        source: crate::terminal::ErrorSource,
        summary: &str,
        details: &str,
    ) {
        let tabs = self.tabs.borrow();
        let Some(tab) = tabs.iter().find(|tab| tab.panes.contains_key(&pane)) else {
            return;
        };
        let sessions = self.sessions.borrow();
        // The banner describes unavailable connections. Ignore the resulting
        // per-pane errors, including keys pressed while reconnecting.
        if matches!(source, crate::terminal::ErrorSource::Session)
            && !sessions.connected(sessions.pane_ref(pane).daemon)
        {
            return;
        }
        let title = if tab.panes.len() > 1 {
            format!(
                "{} · {}: {summary}",
                tab.name,
                tab.panes[&pane].title.text()
            )
        } else {
            format!("{}: {summary}", tab.name)
        };
        self.errors
            .show(&self.window, &self.toast, pane, &title, details);
    }

    pub(super) fn refresh_connection_banner(&self) {
        let tabs = self.tabs.borrow();
        let Some(tab) = tabs.iter().find(|tab| tab.id == self.active.get()) else {
            self.connection_banner.set_revealed(false);
            return;
        };
        let sessions = self.sessions.borrow();
        let status = sessions.connection_status(sessions.tab_ref(tab.id).daemon);
        let state = match status {
            ConnectionStatus::Connected => {
                self.connection_banner.set_revealed(false);
                return;
            }
            ConnectionStatus::Connecting => "Connecting to",
            ConnectionStatus::Reconnecting { .. } => "Reconnecting to",
            ConnectionStatus::Disconnected { .. } => "Disconnected from",
        };
        let name = sessions.connection_name(tab.id);
        let name = if name.is_empty() {
            "This Computer"
        } else {
            &name
        };
        let title = format!("{state} {name}");
        if self.connection_banner.title() != title {
            self.connection_banner.set_title(&title);
        }
        self.connection_banner.set_revealed(true);
    }
}
