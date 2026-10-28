//! GDK layout and lock state for Wayland keyboard events.
use super::*;
use std::{cell::OnceCell, rc::Weak};

pub(super) const SHIFT: u16 = 1 << 0;
pub(super) const CONTROL: u16 = 1 << 1;
pub(super) const ALT: u16 = 1 << 2;
pub(super) const SUPER: u16 = 1 << 3;
pub(super) const CAPS_LOCK: u16 = 1 << 4;
pub(super) const NUM_LOCK: u16 = 1 << 5;

pub(super) fn mods(m: gdk::ModifierType) -> u16 {
    [
        (gdk::ModifierType::SHIFT_MASK, SHIFT),
        (gdk::ModifierType::CONTROL_MASK, CONTROL),
        (gdk::ModifierType::ALT_MASK, ALT),
        (gdk::ModifierType::SUPER_MASK, SUPER),
        (gdk::ModifierType::LOCK_MASK, CAPS_LOCK),
    ]
    .into_iter()
    .fold(0, |encoded, (mask, flag)| {
        encoded | if m.contains(mask) { flag } else { 0 }
    })
}

pub(super) struct NativeKeyboard {
    display: gdk::Display,
    lock_device: Option<gdk::Device>,
    lock_state: ValueCell<u16>,
    notifications: OnceCell<[glib::SignalHandlerId; 2]>,
}

thread_local! {
    // Share device lock notifications across windows.
    static NATIVE: RefCell<Weak<NativeKeyboard>> = const { RefCell::new(Weak::new()) };
}

impl NativeKeyboard {
    pub(super) fn new(display: gdk::Display) -> Rc<Self> {
        NATIVE.with_borrow_mut(|cached| {
            if let Some(native) = cached.upgrade()
                && native.display == display
            {
                return native;
            }
            let device = display.default_seat().and_then(|seat| seat.keyboard());
            let lock_state = ValueCell::new(device.as_ref().map_or(0, read_locks));
            let native = Rc::new(Self {
                display,
                lock_device: device.clone(),
                lock_state,
                notifications: OnceCell::new(),
            });
            if let Some(device) = device {
                let weak = Rc::downgrade(&native);
                let changed = move |device: &gdk::Device| {
                    if let Some(native) = weak.upgrade() {
                        native.refresh_locks(device);
                    }
                };
                let num = device.connect_num_lock_state_notify(changed.clone());
                let caps = device.connect_caps_lock_state_notify(changed);
                native.notifications.set([num, caps]).unwrap();
            }
            *cached = Rc::downgrade(&native);
            native
        })
    }

    /// Refresh on focus entry; notifications maintain the snapshot between keys.
    pub(super) fn refresh_locks(&self, device: &gdk::Device) -> u16 {
        let locks = read_locks(device);
        if self.lock_device.as_ref() == Some(device) {
            self.lock_state.set(locks);
        }
        locks
    }

    pub(super) fn locks(&self, device: &gdk::Device) -> u16 {
        if self.lock_device.as_ref() == Some(device) {
            self.lock_state.get()
        } else {
            // A non-default seat/device still gets actual platform state.
            read_locks(device)
        }
    }

    pub(super) fn layout(&self, event: &gdk::KeyEvent) -> KeyLayout {
        self.lookup(
            event.keycode(),
            event.layout(),
            mods(event.consumed_modifiers()),
        )
    }

    pub(super) fn lookup(&self, code: u32, layout: u32, consumed_mods: u16) -> KeyLayout {
        let physical = if self.display.backend().is_wayland() {
            // wl_keyboard xkb_v1 uses evdev + 8, as does GDK's Wayland backend.
            code.checked_sub(8)
                .and_then(|code| u16::try_from(code).ok())
                .unwrap_or(0)
        } else {
            // Do not interpret another platform's keycodes as evdev codes.
            0
        };
        let unshifted = self
            .display
            .translate_key(code, gdk::ModifierType::empty(), layout as i32)
            .and_then(|(key, _, _, _)| key.to_unicode())
            .map_or(0, |cp| cp as u32);
        KeyLayout {
            physical,
            unshifted,
            consumed_mods,
        }
    }
}

impl Drop for NativeKeyboard {
    fn drop(&mut self) {
        if let Some(device) = &self.lock_device
            && let Some(handlers) = self.notifications.take()
        {
            for handler in handlers {
                device.disconnect(handler);
            }
        }
    }
}

fn read_locks(device: &gdk::Device) -> u16 {
    (if device.is_caps_locked() {
        CAPS_LOCK
    } else {
        0
    }) | if device.is_num_locked() { NUM_LOCK } else { 0 }
}
