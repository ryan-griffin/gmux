use gtk::{glib, prelude::*, subclass::prelude::*};
use std::cell::OnceCell;

// Match AdwBanner's content layout, with its single action generalized to two.
const HORIZONTAL_GAP: i32 = 6;
const CENTER_CLEARANCE: i32 = 36;
const VERTICAL_GAP: i32 = 9;
const INSET: i32 = 6;
const INLINE_BUTTON_MIN: i32 = 84;
const WRAPPED_BUTTON_MIN: i32 = 160;

#[derive(Clone, Copy, Default)]
struct ButtonSize {
    visible: bool,
    width: i32,
    minimum_height: i32,
    natural_height: i32,
}
struct Layout {
    title_width: i32,
    title_height: i32,
    minimum_height: i32,
    natural_height: i32,
    buttons: [ButtonSize; 2],
    actions_width: i32,
    actions_height: i32,
    wrapped: bool,
    stacked: bool,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Prompt {
        pub title: OnceCell<gtk::Label>,
        pub buttons: OnceCell<[gtk::Button; 2]>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for Prompt {
        const NAME: &'static str = "GmuxClipboardPrompt";
        type Type = super::Prompt;
        type ParentType = gtk::Widget;
    }
    impl ObjectImpl for Prompt {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }
    impl Prompt {
        fn buttons(&self, minimum: i32, limit: i32) -> [ButtonSize; 2] {
            self.buttons.get().unwrap().each_ref().map(|button| {
                if !button.is_visible() {
                    return ButtonSize::default();
                }
                let (_, natural, _, _) = button.measure(gtk::Orientation::Horizontal, -1);
                let width = natural.max(minimum).min(limit.max(0));
                let (minimum_height, natural_height, _, _) =
                    button.measure(gtk::Orientation::Vertical, width);
                ButtonSize {
                    visible: true,
                    width,
                    minimum_height,
                    natural_height,
                }
            })
        }
        fn actions_width(buttons: &[ButtonSize; 2]) -> i32 {
            let count = buttons.iter().filter(|b| b.visible).count() as i32;
            buttons.iter().map(|b| b.width).sum::<i32>() + (count - 1).max(0) * HORIZONTAL_GAP
        }
        fn layout(&self, width: i32) -> Layout {
            let width = if width < 0 { i32::MAX / 4 } else { width };
            let title = self.title.get().unwrap();
            let (_, title_natural, _, _) = title.measure(gtk::Orientation::Horizontal, -1);
            let inline = self.buttons(INLINE_BUTTON_MIN, i32::MAX);
            let wrapped =
                INSET + title_natural + HORIZONTAL_GAP + Self::actions_width(&inline) > width;
            let buttons = if wrapped {
                self.buttons(WRAPPED_BUTTON_MIN, width)
            } else {
                inline
            };
            let stacked = wrapped && Self::actions_width(&buttons) > width;
            let actions_width = if stacked {
                buttons.iter().map(|b| b.width).max().unwrap_or(0)
            } else {
                Self::actions_width(&buttons)
            };
            let count = buttons.iter().filter(|b| b.visible).count() as i32;
            let height = |natural: bool| {
                let heights = buttons.iter().filter(|b| b.visible).map(|b| {
                    if natural {
                        b.natural_height
                    } else {
                        b.minimum_height
                    }
                });
                if stacked {
                    heights.sum::<i32>() + (count - 1).max(0) * VERTICAL_GAP
                } else {
                    heights.max().unwrap_or(0)
                }
            };
            let title_width =
                title_natural.min((width - if wrapped { 2 * INSET } else { 0 }).max(0));
            let (title_minimum, title_height, _, _) =
                title.measure(gtk::Orientation::Vertical, title_width);
            let minimum_height = if wrapped {
                2 * INSET + title_minimum + VERTICAL_GAP + height(false)
            } else {
                title_minimum.max(height(false))
            };
            let natural_height = if wrapped {
                2 * INSET + title_height + VERTICAL_GAP + height(true)
            } else {
                title_height.max(height(true))
            };
            Layout {
                title_width,
                title_height,
                minimum_height,
                natural_height,
                buttons,
                actions_width,
                actions_height: height(false),
                wrapped,
                stacked,
            }
        }
    }
    impl WidgetImpl for Prompt {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let (_, title, _, _) = self.title.get().unwrap().measure(orientation, -1);
                let minimum = 2 * INSET + WRAPPED_BUTTON_MIN;
                let natural = (INSET
                    + title
                    + HORIZONTAL_GAP
                    + Self::actions_width(&self.buttons(INLINE_BUTTON_MIN, i32::MAX)))
                .max(minimum);
                (minimum, natural, -1, -1)
            } else {
                let layout = self.layout(for_size);
                (layout.minimum_height, layout.natural_height, -1, -1)
            }
        }
        fn size_allocate(&self, width: i32, height: i32, _: i32) {
            let layout = self.layout(width);
            let rtl = self.obj().direction() == gtk::TextDirection::Rtl;
            let mut title_x = if layout.wrapped {
                (width - layout.title_width) / 2
            } else {
                width / 2 - layout.title_width / 2
            };
            if !layout.wrapped
                && layout.title_width + 2 * (layout.actions_width + CENTER_CLEARANCE) > width
            {
                title_x = if rtl {
                    width - layout.title_width - INSET
                } else {
                    INSET
                };
            }
            let title_y = if layout.wrapped {
                INSET
            } else {
                height / 2 - layout.title_height / 2
            };
            allocate(
                self.title.get().unwrap(),
                title_x,
                title_y,
                layout.title_width,
                layout.title_height,
            );
            let actions_x = if layout.wrapped {
                width / 2 - layout.actions_width / 2
            } else if rtl {
                0
            } else {
                width - layout.actions_width
            };
            let actions_y = if layout.wrapped {
                height - layout.actions_height - INSET
            } else {
                height / 2 - layout.actions_height / 2
            };
            let mut offset = 0;
            for (button, size) in self.buttons.get().unwrap().iter().zip(layout.buttons) {
                if !size.visible {
                    continue;
                }
                let (x, y) = if layout.stacked {
                    (width / 2 - size.width / 2, actions_y + offset)
                } else {
                    (
                        actions_x
                            + if rtl {
                                layout.actions_width - offset - size.width
                            } else {
                                offset
                            },
                        actions_y + layout.actions_height / 2 - size.minimum_height / 2,
                    )
                };
                allocate(button, x, y, size.width, size.minimum_height);
                offset += if layout.stacked {
                    size.minimum_height + VERTICAL_GAP
                } else {
                    size.width + HORIZONTAL_GAP
                };
            }
        }
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            for child in std::iter::once(self.title.get().unwrap().upcast_ref::<gtk::Widget>())
                .chain(
                    self.buttons
                        .get()
                        .unwrap()
                        .iter()
                        .map(|b| b.upcast_ref::<gtk::Widget>()),
                )
            {
                if child.is_visible() {
                    widget.snapshot_child(child, snapshot);
                }
            }
        }
    }
    fn allocate(widget: &impl IsA<gtk::Widget>, x: i32, y: i32, width: i32, height: i32) {
        let transform =
            gtk::gsk::Transform::new().translate(&gtk::graphene::Point::new(x as f32, y as f32));
        widget.allocate(width.max(0), height.max(0), -1, Some(transform));
    }
}

glib::wrapper! {
    pub struct Prompt(ObjectSubclass<imp::Prompt>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}
impl Prompt {
    pub fn new(text: &str) -> Self {
        let prompt: Self = glib::Object::new();
        prompt.set_hexpand(true);
        let title = gtk::Label::builder()
            .label(text)
            .hexpand(true)
            .justify(gtk::Justification::Center)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .lines(3)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        title.add_css_class("heading");
        title.set_parent(&prompt);
        prompt.imp().title.set(title).unwrap();
        let buttons = ["Deny", "Allow Once"].map(|label| {
            let button = gtk::Button::builder()
                .label(label)
                .focus_on_click(false)
                .use_underline(true)
                .can_shrink(true)
                .build();
            button.set_parent(&prompt);
            button
        });
        prompt.imp().buttons.set(buttons).unwrap();
        prompt
    }
    pub fn deny(&self) -> &gtk::Button {
        &self.imp().buttons.get().unwrap()[0]
    }
    pub fn allow(&self) -> &gtk::Button {
        &self.imp().buttons.get().unwrap()[1]
    }
}
