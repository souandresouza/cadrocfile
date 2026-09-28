//! The context menu.
//!
//! Deliberately *not* a `GtkPopoverMenu`. That widget wraps its contents in an
//! internal scroller and caps its own height well below the space available —
//! measured at 412px allocated against a 464px natural request in a 926px
//! window — which silently clipped the last entries off the bottom. Building
//! the menu out of plain buttons in a `GtkPopover` removes the cap and gives us
//! full control over padding, corner radius and the accelerator column.

use std::rc::Rc;

use gtk::{gdk, prelude::*};

pub struct ContextMenu {
    popover: gtk::Popover,
    list: gtk::Box,
    /// Whether the next `item` call should be preceded by a separator.
    pending_separator: std::cell::Cell<bool>,
}

impl ContextMenu {
    /// Creates the menu and anchors it to `parent`, which must stay alive for
    /// the menu's lifetime — normally the window.
    pub fn new(parent: &impl IsA<gtk::Widget>) -> Rc<Self> {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["menu-list"])
            .build();

        let popover = gtk::Popover::builder()
            .has_arrow(false)
            .autohide(true)
            .position(gtk::PositionType::Bottom)
            .halign(gtk::Align::Start)
            .css_classes(["cadrocfile-menu"])
            .child(&list)
            .build();
        popover.set_parent(parent);

        Rc::new(Self { popover, list, pending_separator: std::cell::Cell::new(false) })
    }

    /// Empties the menu, ready to be rebuilt for the current selection.
    pub fn begin(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.pending_separator.set(false);
    }

    /// Marks a group boundary. The separator is only drawn once an item
    /// actually follows, so a group that turns out to be empty leaves no gap.
    pub fn section(&self) {
        if self.list.first_child().is_some() {
            self.pending_separator.set(true);
        }
    }

    /// Adds an item that activates `action` on the window.
    pub fn item(self: &Rc<Self>, label: &str, action: &'static str, accel: Option<&str>) {
        if self.pending_separator.replace(false) {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.add_css_class("menu-separator");
            self.list.append(&separator);
        }

        let text = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build();

        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(24).build();
        row.append(&text);

        if let Some(accel) = accel {
            row.append(
                &gtk::Label::builder()
                    .label(accel)
                    .xalign(1.0)
                    .css_classes(["menu-accel"])
                    .build(),
            );
        }

        let button = gtk::Button::builder().child(&row).css_classes(["flat", "menu-item"]).build();

        let popover = self.popover.clone();
        button.connect_clicked(move |button| {
            // Close first, then dispatch. The popover stays parented either
            // way, so the `win.` lookup still resolves — unlike a menu that
            // unparents itself on close, which is how the previous
            // implementation ended up doing nothing at all when clicked.
            popover.popdown();
            let _ = WidgetExt::activate_action(button, action, None);
        });

        self.list.append(&button);
    }

    /// Shows the menu at `(x, y)`, in the coordinate space of the widget the
    /// popover is parented to.
    ///
    /// The anchor is clamped so the menu stays inside the window. Right-clicking
    /// an item near the right or bottom edge would otherwise put half the menu
    /// outside the app, which on a tiling compositor means over another window.
    pub fn show_at(&self, x: f64, y: f64) {
        // Measure the content rather than the popover: an unmapped popover has
        // no useful size yet, but its child box measures fine.
        let (_, width, _, _) = self.list.measure(gtk::Orientation::Horizontal, -1);
        let (_, height, _, _) = self.list.measure(gtk::Orientation::Vertical, -1);

        // Allow for the popover's own frame and shadow padding.
        const FRAME: i32 = 18;
        const EDGE: i32 = 8;

        let (mut x, mut y) = (x as i32, y as i32);
        if let Some(parent) = self.popover.parent() {
            let (pw, ph) = (parent.width(), parent.height());
            if pw > 0 {
                x = x.min((pw - width - FRAME - EDGE).max(EDGE));
            }
            if ph > 0 {
                y = y.min((ph - height - FRAME - EDGE).max(EDGE));
            }
        }

        self.popover.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        self.popover.popup();
    }

    /// Natural and allocated height, for tracing.
    pub fn measured(&self) -> (i32, i32) {
        let (_, natural, _, _) = self.popover.measure(gtk::Orientation::Vertical, -1);
        (natural, self.popover.height())
    }

    pub fn is_empty(&self) -> bool {
        self.list.first_child().is_none()
    }
}
