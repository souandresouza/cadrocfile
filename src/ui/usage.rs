//! The disk usage window: which folders are taking the space.
//!
//! Opens on a folder, lists its children largest first with a bar for each
//! one's share, and fills the numbers in as each child is measured — the big
//! folders are what you are looking for and they finish last, so the list
//! re-sorts itself as it goes rather than making you wait for all of it.
//! Click a folder to look inside it; the Up button goes back.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{glib, pango};

use crate::fs::usage::{self, Child, UsageEvent, UsageHandle};

type Callback<T> = RefCell<Option<Rc<dyn Fn(T)>>>;

/// The widgets of one row that change as numbers arrive.
struct RowParts {
    path: PathBuf,
    is_dir: bool,
    bar: gtk::LevelBar,
    size: gtk::Label,
    share: gtk::Label,
}

pub struct UsageWindow {
    window: adw::Window,
    title: adw::WindowTitle,
    up: gtk::Button,
    list: gtk::ListBox,
    footer: gtk::Label,
    include_hidden: bool,
    root: RefCell<PathBuf>,
    /// Indexed by the number stored as each row's widget name, so the sort
    /// function can find a row's size without a search per comparison.
    rows: Rc<RefCell<Vec<RowParts>>>,
    sizes: Rc<RefCell<HashMap<PathBuf, (u64, u64)>>>,
    handle: RefCell<Option<UsageHandle>>,
    generation: Cell<u64>,
    on_open: Callback<PathBuf>,
}

impl UsageWindow {
    pub fn new(parent: &impl IsA<gtk::Window>, include_hidden: bool) -> Rc<Self> {
        let title = adw::WindowTitle::new("", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let up = gtk::Button::from_icon_name("go-up-symbolic");
        up.set_tooltip_text(Some("Up to the enclosing folder"));
        header.pack_start(&up);

        let open = gtk::Button::builder()
            .label("Open Folder")
            .tooltip_text("Show this folder in the main window")
            .build();
        header.pack_end(&open);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .valign(gtk::Align::Start)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let footer = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .margin_start(14)
            .margin_end(14)
            .margin_bottom(10)
            .css_classes(["caption", "dim-label"])
            .build();

        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        content.append(&scroller);
        content.append(&footer);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let window = adw::Window::builder()
            .default_width(720)
            .default_height(640)
            .transient_for(parent)
            .hide_on_close(true)
            .content(&toolbar)
            .build();

        let this = Rc::new(Self {
            window,
            title,
            up,
            list,
            footer,
            include_hidden,
            root: RefCell::new(PathBuf::new()),
            rows: Rc::new(RefCell::new(Vec::new())),
            sizes: Rc::new(RefCell::new(HashMap::new())),
            handle: RefCell::new(None),
            generation: Cell::new(0),
            on_open: RefCell::new(None),
        });

        // Largest first; anything not measured yet sinks to the bottom, then
        // by name so the order is stable while numbers are still arriving.
        let (rows, sizes) = (Rc::clone(&this.rows), Rc::clone(&this.sizes));
        this.list.set_sort_func(move |a, b| {
            let key = |row: &gtk::ListBoxRow| {
                let rows = rows.borrow();
                let part = row.widget_name().parse::<usize>().ok().and_then(|i| rows.get(i).map(|p| p.path.clone()));
                let size = part.as_ref().and_then(|p| sizes.borrow().get(p).map(|s| s.0));
                (size, part)
            };
            let ((size_a, path_a), (size_b, path_b)) = (key(a), key(b));
            size_b.cmp(&size_a).then_with(|| path_a.cmp(&path_b)).into()
        });

        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let target = row
                .widget_name()
                .parse::<usize>()
                .ok()
                .and_then(|i| this.rows.borrow().get(i).map(|p| (p.path.clone(), p.is_dir)));
            match target {
                Some((path, true)) => this.show(&path),
                // A file has nothing inside it to look at; show it where it is.
                Some((path, false)) => {
                    let callback = this.on_open.borrow().clone();
                    if let Some(callback) = callback {
                        callback(path);
                    }
                }
                None => {}
            }
        });

        let weak = Rc::downgrade(&this);
        this.up.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let parent = this.root.borrow().parent().map(Path::to_path_buf);
            if let Some(parent) = parent {
                this.show(&parent);
            }
        });

        let weak = Rc::downgrade(&this);
        open.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            let root = this.root.borrow().clone();
            let callback = this.on_open.borrow().clone();
            if let Some(callback) = callback {
                callback(root);
            }
        });

        let weak = Rc::downgrade(&this);
        this.window.connect_hide(move |_| {
            if let Some(this) = weak.upgrade() {
                // Stops the walk. Measuring a disk nobody is looking at is waste.
                this.handle.borrow_mut().take();
                this.generation.set(this.generation.get() + 1);
            }
        });

        this
    }

    /// Asked to show a path in the main window.
    pub fn connect_open(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_open.borrow_mut() = Some(Rc::new(f));
    }

    /// Measures `folder` and presents the window.
    pub fn show(self: &Rc<Self>, folder: &Path) {
        *self.root.borrow_mut() = folder.to_path_buf();
        self.up.set_sensitive(folder.parent().is_some());
        self.title.set_title(&display_name(folder));
        self.title.set_subtitle("Measuring…");
        self.footer.set_label("");

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();
        self.sizes.borrow_mut().clear();

        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let handle = usage::start(folder.to_path_buf(), self.include_hidden);
        let events = handle.events.clone();
        // Replacing the handle drops the previous one, which cancels its walk.
        *self.handle.borrow_mut() = Some(handle);

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(first) = events.recv().await {
                let Some(this) = weak.upgrade() else { return };
                if this.generation.get() != generation {
                    return;
                }
                // Whatever else has arrived meanwhile is applied in the same
                // pass, so a folder of thousands of small children redraws a
                // handful of times rather than once per child.
                let mut finished = None;
                for event in std::iter::once(first).chain(std::iter::from_fn(|| events.try_recv().ok())) {
                    match event {
                        UsageEvent::Listed(children) => this.add_rows(children),
                        UsageEvent::Measured { path, bytes, files } => {
                            this.sizes.borrow_mut().insert(path, (bytes, files));
                        }
                        UsageEvent::Finished { unreadable } => finished = Some(unreadable),
                    }
                }
                this.update_numbers(finished.is_some());
                if let Some(unreadable) = finished {
                    this.finish(unreadable).await;
                    return;
                }
            }
        });

        self.window.present();
    }

    fn add_rows(&self, children: Vec<Child>) {
        if children.is_empty() {
            self.footer.set_label("This folder is empty.");
        }
        let mut rows = self.rows.borrow_mut();
        for child in children {
            let index = rows.len();
            let icon = gtk::Image::from_icon_name(if child.is_dir { "folder-symbolic" } else { "text-x-generic-symbolic" });

            let name = gtk::Label::builder()
                .label(&child.name)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build();
            let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).hexpand(true).build();
            // The default offsets turn a full bar orange and then red, which
            // reads as a warning. A share of a folder is not a warning.
            for offset in ["low", "high", "full"] {
                bar.remove_offset_value(Some(offset));
            }
            let middle = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).build();
            middle.append(&name);
            middle.append(&bar);

            let size = gtk::Label::builder()
                .label("…")
                .width_chars(9)
                .xalign(1.0)
                .css_classes(["numeric"])
                .build();
            let share = gtk::Label::builder()
                .width_chars(5)
                .xalign(1.0)
                .css_classes(["numeric", "dim-label", "caption"])
                .build();

            let line = gtk::Box::builder()
                .spacing(12)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(10)
                .margin_end(10)
                .build();
            line.append(&icon);
            line.append(&middle);
            line.append(&size);
            line.append(&share);
            if child.is_dir {
                line.append(&gtk::Image::from_icon_name("go-next-symbolic"));
            }

            let row = gtk::ListBoxRow::builder().child(&line).activatable(true).build();
            row.set_widget_name(&index.to_string());
            self.list.append(&row);
            rows.push(RowParts { path: child.path, is_dir: child.is_dir, bar, size, share });
        }
    }

    fn update_numbers(&self, complete: bool) {
        let sizes = self.sizes.borrow();
        let total: u64 = sizes.values().map(|s| s.0).sum();
        let files: u64 = sizes.values().map(|s| s.1).sum();
        for part in self.rows.borrow().iter() {
            let Some((bytes, _)) = sizes.get(&part.path) else { continue };
            part.size.set_label(&humansize::format_size(*bytes, humansize::DECIMAL));
            let fraction = if total == 0 { 0.0 } else { *bytes as f64 / total as f64 };
            part.bar.set_value(fraction);
            part.share.set_label(&format!("{:.0}%", fraction * 100.0));
        }
        let measured = sizes.len();
        let count = self.rows.borrow().len();
        drop(sizes);

        let size = humansize::format_size(total, humansize::DECIMAL);
        self.title.set_subtitle(&if complete {
            format!("{size} in {files} file{}", if files == 1 { "" } else { "s" })
        } else {
            format!("{size} so far · measured {measured} of {count}")
        });
        self.list.invalidate_sort();
    }

    async fn finish(&self, unreadable: u64) {
        let root = self.root.borrow().clone();
        let mut notes = Vec::new();
        if let Some((free, size)) = crate::fs::scan::filesystem_usage(&root).await {
            notes.push(format!(
                "This disk: {} free of {}",
                humansize::format_size(free, humansize::DECIMAL),
                humansize::format_size(size, humansize::DECIMAL)
            ));
        }
        if unreadable > 0 {
            notes.push(format!(
                "{unreadable} folder{} could not be read, so the totals are a little low",
                if unreadable == 1 { "" } else { "s" }
            ));
        }
        notes.push("Sizes are space used on this disk; other drives mounted inside are not counted".into());
        self.footer.set_label(&notes.join(" · "));
    }
}

fn display_name(path: &Path) -> String {
    if dirs::home_dir().as_deref() == Some(path) {
        return "Home".to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}
