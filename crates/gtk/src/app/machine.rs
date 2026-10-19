//! GTK presentation of toolkit-independent machine metadata and connection state.
use super::*;
use crate::client::connections::ConnectionStatus;

pub(super) fn icon_name(kind: model::MachineKind) -> &'static str {
    use model::MachineKind::*;
    match kind {
        Unknown | Desktop | Watch => "computer-symbolic",
        Laptop | Convertible => "gmux-laptop-symbolic",
        Server | VirtualMachine => "gmux-server-symbolic",
        Tablet => "tablet-symbolic",
        Phone => "phone-symbolic",
        Embedded => "gmux-chip-symbolic",
        Container => "gmux-container-symbolic",
    }
}

fn label(kind: model::MachineKind) -> &'static str {
    use model::MachineKind::*;
    match kind {
        Unknown => "Computer",
        Desktop => "Desktop",
        Laptop => "Laptop",
        Convertible => "Convertible",
        Server => "Server",
        Tablet => "Tablet",
        Phone => "Phone",
        Watch => "Watch",
        Embedded => "Embedded Computer",
        VirtualMachine => "Virtual Machine",
        Container => "Container",
    }
}

pub(super) fn icon(kind: model::MachineKind) -> gtk::Image {
    let image = gtk::Image::new();
    image.set_pixel_size(16);
    update_icon(&image, kind);
    image
}

pub(super) fn update_icon(image: &gtk::Image, kind: model::MachineKind) {
    set_icon(image, icon_name(kind), label(kind));
}

fn set_icon(image: &gtk::Image, name: &str, label: &str) {
    image.set_icon_name(Some(name));
    image.set_tooltip_text(Some(label));
    image.update_property(&[gtk::accessible::Property::Label(label)]);
}

pub(super) fn update_tab(tab: &mut Tab, sessions: &connections::Connections) {
    let status = sessions.connection_status(sessions.tab_ref(tab.id).daemon);
    let disconnected = matches!(
        status,
        ConnectionStatus::Disconnected { .. } | ConnectionStatus::Reconnecting { .. }
    );
    let icon_kind = if disconnected {
        None
    } else {
        Some(sessions.machine_kind(tab.id))
    };
    if tab.machine_icon_kind != icon_kind {
        tab.machine_icon_kind = icon_kind;
        if let Some(kind) = icon_kind {
            update_icon(&tab.machine_icon, kind);
            tab.machine_box.remove_css_class("warning");
            tab.machine_box.add_css_class("dimmed");
        } else {
            set_icon(
                &tab.machine_icon,
                "network-offline-symbolic",
                "Disconnected",
            );
            tab.machine_box.remove_css_class("dimmed");
            tab.machine_box.add_css_class("warning");
        }
    }
    let mut caption = sessions.connection_name(tab.id);
    if !caption.is_empty() && matches!(status, ConnectionStatus::Connecting) {
        caption.push_str(" · Connecting…");
    }
    if !caption.is_empty() && matches!(status, ConnectionStatus::Reconnecting { .. }) {
        caption.push_str(" · Reconnecting…");
    }
    tab.machine_label.set_text(&caption);
    if !caption.is_empty() && matches!(status, ConnectionStatus::Disconnected { .. }) {
        caption.push_str(" · Disconnected");
    }
    tab.machine_box.set_tooltip_text(Some(&caption));
}
