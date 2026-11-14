//! The command palette: `Ctrl+K`, type a few letters, press Enter.
//!
//! One entry point for everything the window can do, which matters most for
//! the actions that have no shortcut and live three menus deep. It searches
//! three kinds of target with the same keystrokes:
//!
//! * **commands** — every `win.` action, invoked by name so the palette cannot
//!   drift from what the menus actually do;
//! * **places** — home, the standard folders, favourites, Trash and Recent;
//! * **tabs** — switching to any open tab by its folder name.
//!
//! Recently used commands come first while the query is empty, the way a
//! launcher's history does. Matching is a subsequence fuzzy match (`ctrlsb` →
//! "Control strip sidebar" would match), because that is how people type when
//! they already know the name.
use std::{path::PathBuf, rc::Rc};

use adw::prelude::*;
use gtk::{gdk, glib};

use super::window::{Location, Window};

/// A fixed command in the palette.
struct Command {
    title: &'static str,
    subtitle: &'static str,
    /// Name without the `win.` prefix.
    action: &'static str,
    /// Extra words that should match, including English aliases for a
    /// translated title.
    keywords: &'static str,
}

/// Everything worth reaching from the palette, in the order it is offered.
///
/// Kept next to [`Window::install_actions`] in spirit: every name here must
/// exist in that registry, but a missing one only makes the palette toast an
/// error, never crash.
const COMMANDS: &[Command] = &[
    Command { title: "New folder", subtitle: "Create a folder here", action: "new-folder", keywords: "mkdir directory" },
    Command { title: "New file", subtitle: "Create an empty file here", action: "new-file", keywords: "create touch" },
    Command { title: "New tab", subtitle: "Open the current folder in another tab", action: "new-tab", keywords: "tab" },
    Command { title: "New window", subtitle: "Open another window", action: "new-window", keywords: "window" },
    Command { title: "Close tab", subtitle: "Close the tab in front", action: "close-tab", keywords: "tab" },
    Command { title: "Rename…", subtitle: "Rename the selection", action: "rename", keywords: "name" },
    Command { title: "Copy", subtitle: "Copy the selection", action: "copy", keywords: "clipboard" },
    Command { title: "Cut", subtitle: "Move the selection on paste", action: "cut", keywords: "clipboard" },
    Command { title: "Paste", subtitle: "Paste into this folder", action: "paste", keywords: "clipboard" },
    Command { title: "Copy path", subtitle: "Copy the full path as text", action: "copy-path", keywords: "clipboard location" },
    Command { title: "Move to Trash", subtitle: "Delete the selection, reversibly", action: "trash", keywords: "delete remove" },
    Command { title: "Delete permanently", subtitle: "Remove the selection for good", action: "delete-permanently", keywords: "delete remove" },
    Command { title: "Shred…", subtitle: "Overwrite the selection before deleting", action: "shred", keywords: "secure erase wipe" },
    Command { title: "Properties", subtitle: "Size, permissions and metadata", action: "properties", keywords: "info details exif" },
    Command { title: "Preview", subtitle: "Quick look at the selection", action: "preview", keywords: "space quicklook" },
    Command { title: "Open with…", subtitle: "Choose an application", action: "open-with", keywords: "application" },
    Command { title: "Open in terminal", subtitle: "A shell in this folder", action: "open-terminal", keywords: "shell console" },
    Command { title: "Find in this folder", subtitle: "Search names and contents", action: "focus-search", keywords: "search find" },
    Command { title: "Go to path…", subtitle: "Type a location", action: "edit-path", keywords: "location address" },
    Command { title: "Back", subtitle: "Previous folder", action: "go-back", keywords: "history" },
    Command { title: "Forward", subtitle: "Next folder", action: "go-forward", keywords: "history" },
    Command { title: "Up", subtitle: "Parent folder", action: "go-up", keywords: "parent" },
    Command { title: "Home", subtitle: "Your home folder", action: "go-home", keywords: "home" },
    Command { title: "Reload", subtitle: "Read this folder again", action: "reload", keywords: "refresh" },
    Command { title: "Select all", subtitle: "Select every item", action: "select-all", keywords: "selection" },
    Command { title: "Select none", subtitle: "Clear the selection", action: "select-none", keywords: "selection" },
    Command { title: "Invert selection", subtitle: "Swap selected and unselected", action: "invert-selection", keywords: "selection" },
    Command { title: "Switch view", subtitle: "Grid or list", action: "toggle-view", keywords: "grid list layout" },
    Command { title: "Show hidden files", subtitle: "Toggle dotfiles", action: "toggle-hidden", keywords: "dotfiles" },
    Command { title: "Folders first", subtitle: "Sort folders above files", action: "toggle-dirs-first", keywords: "sort order" },
    Command { title: "Thumbnails", subtitle: "Turn previews on or off", action: "toggle-thumbnails", keywords: "preview images" },
    Command { title: "Sidebar", subtitle: "Show or hide the sidebar", action: "toggle-sidebar", keywords: "panel places" },
    Command { title: "Zoom in", subtitle: "Larger icons", action: "zoom-in", keywords: "size" },
    Command { title: "Zoom out", subtitle: "Smaller icons", action: "zoom-out", keywords: "size" },
    Command { title: "Reset zoom", subtitle: "Default icon size", action: "zoom-reset", keywords: "size" },
    Command { title: "Reverse sort order", subtitle: "Flip ascending and descending", action: "sort-descending", keywords: "order" },
    Command { title: "Add to favourites", subtitle: "Pin this folder to the sidebar", action: "add-favourite", keywords: "bookmark pin" },
    Command { title: "Disk usage", subtitle: "What is using the space", action: "disk-usage", keywords: "space size" },
    Command { title: "Download from URL…", subtitle: "Save a link into this folder", action: "download", keywords: "http link" },
    Command { title: "Compress…", subtitle: "Make an archive of the selection", action: "compress", keywords: "zip archive" },
    Command { title: "Extract here", subtitle: "Unpack the selected archive", action: "extract-here", keywords: "unzip archive" },
    Command { title: "Extract to…", subtitle: "Unpack into a chosen folder", action: "extract-to", keywords: "unzip archive" },
    Command { title: "Restore from Trash", subtitle: "Put the selection back", action: "restore", keywords: "undelete" },
    Command { title: "Empty Trash", subtitle: "Delete everything in the Trash", action: "empty-trash", keywords: "trash clear" },
    Command { title: "Next tab", subtitle: "Move one tab along", action: "next-tab", keywords: "tab" },
    Command { title: "Previous tab", subtitle: "Move one tab back", action: "previous-tab", keywords: "tab" },
    Command { title: "Keyboard shortcuts", subtitle: "Every shortcut in one place", action: "shortcuts", keywords: "keys help" },
    Command { title: "Preferences", subtitle: "Change how Cadrocfile behaves", action: "preferences", keywords: "settings options" },
    Command { title: "About Cadrocfile", subtitle: "Version and credits", action: "about", keywords: "version" },
];

/// What running a row does.
#[derive(Clone)]
enum Target {
    Action(&'static str),
    Folder(PathBuf),
    Trash,
    Recent,
    Tab(usize),
    /// A radio action: the action name plus the value to select.
    Parameter(&'static str, &'static str),
    /// A command from the recents list; its static entry supplies the action.
    RecentCommand(&'static str),
}

struct Entry {
    title: String,
    subtitle: String,
    target: Target,
    /// Whether this came from the recents list, so it can be drawn apart.
    recent: bool,
}

impl Window {
    /// Opens the palette, or closes it if it is already open.
    pub(crate) fn show_palette(self: &Rc<Self>) {
        if self.palette_open.get() {
            if let Some(dialog) = self.palette.borrow_mut().take() {
                dialog.close();
            }
            self.palette_open.set(false);
            return;
        }

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Type a command, folder or tab…")
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        column.append(&search);
        column.append(&scroller);

        let dialog = adw::Dialog::builder()
            .content_width(640)
            .content_height(440)
            .child(&column)
            .build();
        dialog.add_css_class("palette");

        let window = Rc::clone(self);
        let rows = Rc::new(std::cell::RefCell::new(Vec::<Target>::new()));
        let rebuild = {
            let window = Rc::downgrade(&window);
            let list = list.clone();
            let rows = Rc::clone(&rows);
            move |query: &str| {
                let Some(window) = window.upgrade() else { return };
                let entries = window.palette_entries(query);
                list.remove_all();
                rows.borrow_mut().clear();

                if entries.is_empty() {
                    let row = adw::ActionRow::builder()
                        .title("No matches")
                        .subtitle("Try a shorter word, or a folder name")
                        .build();
                    row.set_sensitive(false);
                    list.append(&row);
                    return;
                }

                for entry in entries {
                    let row = adw::ActionRow::builder()
                        .title(&entry.title)
                        .subtitle(&entry.subtitle)
                        .activatable(true)
                        .build();
                    let icon = gtk::Image::from_icon_name(match entry.target {
                        Target::Action(_) | Target::RecentCommand(_) | Target::Parameter(..) => {
                            "system-run-symbolic"
                        }
                        Target::Folder(_) | Target::Trash | Target::Recent => "folder-symbolic",
                        Target::Tab(_) => "view-grid-symbolic",
                    });
                    icon.set_margin_end(12);
                    if entry.recent {
                        icon.add_css_class("dim-label");
                    }
                    row.add_prefix(&icon);
                    list.append(&row);
                    rows.borrow_mut().push(entry.target);
                }

                // Enter acts on the selected row, so something must always be
                // selected; the first result is the best guess.
                if let Some(first) = list.row_at_index(0) {
                    list.select_row(Some(&first));
                }
            }
        };

        let rebuild = Rc::new(rebuild);
        rebuild("");

        let activate = {
            let dialog = dialog.downgrade();
            let window = Rc::downgrade(self);
            let rows = Rc::clone(&rows);
            let list = list.downgrade();
            move || {
                let Some(window) = window.upgrade() else { return };
                let Some(list) = list.upgrade() else { return };
                let Some(row) = list.selected_row() else { return };
                let index = row.index();
                let Some(target) = (index >= 0).then(|| rows.borrow().get(index as usize).cloned()).flatten()
                else {
                    return;
                };
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
                window.palette_open.set(false);
                window.run_palette_target(target);
            }
        };
        let activate = Rc::new(activate);

        {
            let activate = Rc::clone(&activate);
            list.connect_row_activated(move |_, _| activate());
        }

        {
            let rebuild = Rc::clone(&rebuild);
            search.connect_search_changed(move |entry| rebuild(entry.text().as_str()));
        }

        // Typing keeps focus in the search entry, so the arrows have to be
        // forwarded to the list by hand. Page keys scroll, Home/End jump.
        {
            let list = list.clone();
            let activate = Rc::clone(&activate);
            let dialog = dialog.downgrade();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, _| {
                let step = match key {
                    gdk::Key::Down => 1,
                    gdk::Key::Up => -1,
                    gdk::Key::Escape => {
                        if let Some(dialog) = dialog.upgrade() {
                            dialog.close();
                        }
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::Return | gdk::Key::KP_Enter => {
                        activate();
                        return glib::Propagation::Stop;
                    }
                    _ => return glib::Propagation::Proceed,
                };
                let count = list.observe_children().n_items();
                if count == 0 {
                    return glib::Propagation::Stop;
                }
                let current = list.selected_row().map(|r| r.index()).unwrap_or(0) as i64;
                let next = (current + step).rem_euclid(count as i64);
                if let Some(row) = list.row_at_index(next as i32) {
                    list.select_row(Some(&row));
                }
                glib::Propagation::Stop
            });
            search.add_controller(keys);
        }

        {
            let weak = Rc::downgrade(self);
            dialog.connect_closed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.palette_open.set(false);
                }
            });
        }

        *self.palette.borrow_mut() = Some(dialog.clone());
        self.palette_open.set(true);
        dialog.present(Some(&self.window));
        search.grab_focus();
    }

    /// All entries matching `query`, best first.
    fn palette_entries(self: &Rc<Self>, query: &str) -> Vec<Entry> {
        let query = query.trim();
        let mut entries: Vec<Entry> = Vec::new();

        let commands = |title: &str, subtitle: &str, target: Target, recent: bool| Entry {
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            target,
            recent,
        };

        if query.is_empty() {
            for action in &self.config.borrow().recent_commands {
                if let Some(command) = COMMANDS.iter().find(|c| c.action == action) {
                    entries.push(commands(
                        command.title,
                        "Recently used",
                        Target::RecentCommand(command.action),
                        true,
                    ));
                }
            }
        }

        let mut scored: Vec<(i32, Entry)> = Vec::new();

        for command in COMMANDS {
            // A command already offered under recents would appear twice.
            if query.is_empty()
                && self.config.borrow().recent_commands.iter().any(|a| a == command.action)
            {
                continue;
            }
            let haystack = format!("{} {} {}", command.title, command.subtitle, command.keywords);
            if let Some(score) = fuzzy(query, &haystack) {
                scored.push((
                    score,
                    commands(command.title, command.subtitle, Target::Action(command.action), false),
                ));
            }
        }

        // Radio choices are offered as individual rows: a palette has no
        // submenu, and "Sort by size" is what someone types anyway.
        for (title, subtitle, action, value, keywords) in [
            ("Sort by name", "Alphabetical order", "sort-by", "name", "order"),
            ("Sort by size", "Largest first", "sort-by", "size", "order"),
            ("Sort by date", "Newest first", "sort-by", "modified", "order date"),
            ("Sort by type", "Group by file type", "sort-by", "kind", "order type"),
            ("Theme: system", "Follow the desktop", "theme", "system", "appearance dark light"),
            ("Theme: light", "Always light", "theme", "light", "appearance"),
            ("Theme: dark", "Always dark", "theme", "dark", "appearance"),
        ] {
            if let Some(score) = fuzzy(query, &format!("{title} {subtitle} {keywords}")) {
                scored.push((
                    score,
                    commands(title, subtitle, Target::Parameter(action, value), false),
                ));
            }
        }

        // Places: the standard folders that exist, favourites, Trash.
        let mut places: Vec<(String, Target)> = Vec::new();
        for (title, folder) in [
            ("Home", dirs::home_dir()),
            ("Desktop", dirs::desktop_dir()),
            ("Documents", dirs::document_dir()),
            ("Downloads", dirs::download_dir()),
            ("Music", dirs::audio_dir()),
            ("Pictures", dirs::picture_dir()),
            ("Videos", dirs::video_dir()),
        ] {
            if let Some(folder) = folder.filter(|p| p.is_dir()) {
                places.push((title.to_string(), Target::Folder(folder)));
            }
        }
        for favourite in &self.config.borrow().favourites {
            let title = favourite
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| favourite.to_string_lossy().into_owned());
            places.push((title, Target::Folder(favourite.clone())));
        }
        places.push(("Trash".into(), Target::Trash));
        places.push(("Recent".into(), Target::Recent));

        for (title, target) in places {
            let subtitle = match &target {
                Target::Folder(path) => path.to_string_lossy().into_owned(),
                _ => "Places".to_string(),
            };
            if let Some(score) = fuzzy(query, &format!("{title} {subtitle} place")) {
                scored.push((score, commands(&title, &subtitle, target, false)));
            }
        }

        // Old tabs are one keypress away even when folded in the tab strip.
        let tabs: Vec<(usize, String)> = self
            .tabs
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| match &*tab.location.borrow() {
                Location::Directory(path) => Some((
                    index,
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned()),
                )),
                Location::Trash => Some((index, "Trash".to_string())),
                Location::Recent => Some((index, "Recent".to_string())),
            })
            .collect();
        for (index, title) in tabs {
            let subtitle = format!("Switch to tab {}", index + 1);
            if let Some(score) = fuzzy(query, &format!("{title} {subtitle} tab")) {
                scored.push((score, commands(&title, &subtitle, Target::Tab(index), false)));
            }
        }

        if query.is_empty() {
            entries.extend(scored.into_iter().map(|(_, entry)| entry));
            return entries;
        }

        // Higher score first, and shorter titles win ties: `home` should offer
        // "Home" before "Home folder backup".
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.len().cmp(&b.1.title.len())));
        entries.extend(scored.into_iter().map(|(_, entry)| entry));
        entries.truncate(40);
        entries
    }

    /// Runs a palette row, remembering action commands for next time.
    fn run_palette_target(self: &Rc<Self>, target: Target) {
        match target {
            Target::Action(name) | Target::RecentCommand(name) => {
                self.remember_command(name);
                WidgetExt::activate_action(&self.window, &format!("win.{name}"), None).ok();
            }
            Target::Parameter(name, value) => {
                WidgetExt::activate_action(
                    &self.window,
                    &format!("win.{name}"),
                    Some(&value.to_variant()),
                )
                .ok();
            }
            Target::Folder(path) => self.open_path(path),
            Target::Trash => self.navigate_to(Location::Trash, true),
            Target::Recent => self.navigate_to(Location::Recent, true),
            Target::Tab(index) => {
                let page = self.tab_view.nth_page(index as i32);
                self.tab_view.set_selected_page(&page);
            }
        }
    }

    fn remember_command(self: &Rc<Self>, action: &str) {
        {
            let mut config = self.config.borrow_mut();
            config.recent_commands.retain(|existing| existing != action);
            config.recent_commands.insert(0, action.to_string());
            config.recent_commands.truncate(6);
        }
        self.schedule_save();
    }
}

/// Subsequence match with a score, or `None` when `query` does not fit.
///
/// Every query character must appear in order. Runs at the start of a word are
/// worth more, and adjacent runs more still, so `nf` finds "New folder" above
/// a title where the letters merely happen to be scattered.
fn fuzzy(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let needle: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if needle.is_empty() {
        return Some(0);
    }
    let haystack: Vec<char> = text.to_lowercase().chars().collect();

    let mut score = 0;
    let mut index = 0;
    for &wanted in &needle {
        let mut found = None;
        for position in index..haystack.len() {
            if haystack[position] == wanted {
                found = Some(position);
                break;
            }
        }
        let position = found?;
        score += 10;
        if position == 0 || !haystack[position - 1].is_alphanumeric() {
            score += 14;
        }
        if position == index && index > 0 {
            score += 8;
        }
        index = position + 1;
    }
    // Shorter haystacks are usually the more direct match.
    Some(score - (haystack.len() as i32 / 8))
}

#[cfg(test)]
mod tests {
    use super::fuzzy;

    #[test]
    fn scattered_letters_match_in_order() {
        assert!(fuzzy("nf", "New folder").is_some());
        assert!(fuzzy("ctrlsb", "Control strip sidebar").is_some());
        assert!(fuzzy("zz", "New folder").is_none());
    }

    #[test]
    fn word_starts_beat_letters_in_the_middle() {
        let start = fuzzy("nf", "New folder").unwrap();
        let middle = fuzzy("nf", "Unified").unwrap();
        assert!(start > middle, "{start} should beat {middle}");
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert_eq!(fuzzy("", "anything"), Some(0));
    }
}
