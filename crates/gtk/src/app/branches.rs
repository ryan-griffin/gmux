//! Present pane metadata collected on each owning daemon.
use super::*;

impl Ui {
    pub(super) fn refresh_branches(self: &Rc<Self>) {
        let panes = self.tabs.borrow().iter().map(|tab| tab.active).collect();
        if let Err(error) = self.sessions.borrow_mut().set_git_panes(panes) {
            eprintln!("Could not subscribe to Git metadata: {error}");
        }
        self.update_branches();
    }

    pub(super) fn update_branches(self: &Rc<Self>) {
        let mut changed = false;
        for tab in self.tabs.borrow().iter() {
            let name = tab
                .panes
                .get(&tab.active)
                .map_or_else(String::new, |pane| pane.branch());
            if tab.branch_label.text() != name {
                tab.branch_label.set_label(&name);
                tab.branch_icon.set_visible(!name.is_empty());
                changed = true;
            }
        }
        if changed {
            self.refresh_sidebar();
        }
    }
}
