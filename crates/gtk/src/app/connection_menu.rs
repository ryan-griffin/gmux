//! Connection choices shared by the header, empty state, and main menu.
use super::*;

pub(super) struct ConnectionMenu {
    model: gio::Menu,
    choices: RefCell<Vec<Rc<Choice>>>,
    views: RefCell<Vec<Rc<View>>>,
}

struct View {
    list: gtk::ListBox,
    targets: RefCell<HashMap<gtk::ListBoxRow, Rc<Choice>>>,
}

struct Choice {
    machine: model::Machine,
    connected: Cell<bool>,
}

impl ConnectionMenu {
    pub(super) fn new() -> Self {
        let model = gio::Menu::new();
        let item = gio::MenuItem::new(None, None);
        item.set_attribute_value("custom", Some(&"connections".to_variant()));
        model.append_item(&item);
        model.freeze();
        Self {
            model,
            choices: RefCell::new(Vec::new()),
            views: RefCell::new(Vec::new()),
        }
    }
    pub(super) fn model(&self) -> &gio::Menu {
        &self.model
    }
    pub(super) fn dropdown(&self) -> gtk::PopoverMenu {
        let menu = gio::Menu::new();
        menu.append_section(Some("New Tab via Connection"), &self.model);
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        self.attach(&popover);
        popover
    }
    pub(super) fn attach(&self, popover: &gtk::PopoverMenu) {
        let view = Rc::new(View {
            list: gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .accessible_role(gtk::AccessibleRole::Group)
                .build(),
            targets: RefCell::new(HashMap::new()),
        });
        let weak = Rc::downgrade(&view);
        view.list.connect_row_activated(glib::clone!(
            #[weak]
            popover,
            move |list, row| {
                let Some(view) = weak.upgrade() else { return };
                let choice = view.targets.borrow().get(row).cloned();
                if row.is_sensitive()
                    && let Some(choice) = choice.filter(|choice| choice.connected.get())
                {
                    let target = choice.machine.daemon.to_string().to_variant();
                    if let Err(error) = list.activate_action("win.new-tab-machine", Some(&target)) {
                        eprintln!("Could not activate New Tab: {error}");
                    }
                    popover.popdown();
                }
            }
        ));
        view.populate(&self.choices.borrow());
        assert!(popover.add_child(&view.list, "connections"));
        self.views.borrow_mut().push(view);
    }
    pub(super) fn refresh(&self, sessions: &connections::Connections) {
        let machines = sessions.connections();
        if !self
            .choices
            .borrow()
            .iter()
            .map(|choice| &choice.machine)
            .eq(machines.iter())
        {
            let choices: Vec<_> = machines
                .into_iter()
                .map(|machine| {
                    Rc::new(Choice {
                        connected: Cell::new(sessions.connected(machine.daemon)),
                        machine,
                    })
                })
                .collect();
            for view in self.views.borrow().iter() {
                view.populate(&choices);
            }
            *self.choices.borrow_mut() = choices;
        }
        self.refresh_availability(sessions);
    }
    pub(super) fn refresh_availability(&self, sessions: &connections::Connections) {
        for choice in self.choices.borrow().iter() {
            choice
                .connected
                .set(sessions.connected(choice.machine.daemon));
        }
        for view in self.views.borrow().iter() {
            for (row, choice) in view.targets.borrow().iter() {
                row.set_sensitive(choice.connected.get());
            }
        }
    }
}

impl View {
    fn populate(&self, choices: &[Rc<Choice>]) {
        self.targets.borrow_mut().clear();
        self.list.remove_all();
        let mut mnemonics = HashSet::new();
        for choice in choices {
            let machine = &choice.machine;
            let label = crate::menu::mnemonic_label(&machine.name, &mut mnemonics);
            // Model buttons hide icons alongside text. Adwaita styles list rows
            // in menu popovers, so choices retain the native menu appearance.
            let row = gtk::ListBoxRow::builder()
                .accessible_role(gtk::AccessibleRole::MenuItem)
                .sensitive(choice.connected.get())
                .build();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let icon = gtk::Image::builder()
                .icon_name(super::machine::icon_name(machine.machine_kind))
                .pixel_size(16)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build();
            icon.add_css_class("connection-menu-icon");
            let name = gtk::Label::with_mnemonic(&label);
            name.set_xalign(0.0);
            name.set_mnemonic_widget(Some(&row));
            content.append(&icon);
            content.append(&name);
            row.set_child(Some(&content));
            row.update_property(&[gtk::accessible::Property::Label(&machine.name)]);
            self.targets
                .borrow_mut()
                .insert(row.clone(), Rc::clone(choice));
            self.list.append(&row);
        }
    }
}
