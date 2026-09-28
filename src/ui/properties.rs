//! The properties dialog.
//!
//! For a folder the interesting number — total size on disk — requires walking
//! the tree, so it is computed on a worker thread and streamed in while the
//! dialog is already on screen.
use crate::tr;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::fs::FileEntry;

/// Builds and presents the dialog for the given selection.
pub fn present(parent: &impl IsA<gtk::Widget>, entries: &[FileEntry]) {
    if entries.is_empty() {
        return;
    }

    let page = adw::PreferencesPage::new();
    let stop = Arc::new(AtomicBool::new(false));

    if entries.len() == 1 {
        build_single(&page, &entries[0], &stop);
    } else {
        build_multiple(&page, entries, &stop);
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));

    let dialog = adw::Dialog::builder()
        .title(tr!("Properties"))
        .content_width(460)
        .content_height(560)
        .child(&toolbar)
        .build();

    // Abandon the directory walk as soon as the dialog goes away, so closing it
    // doesn't leave a thread churning through a huge tree.
    let stop_on_close = Arc::clone(&stop);
    dialog.connect_closed(move |_| stop_on_close.store(true, Ordering::Relaxed));

    dialog.present(Some(parent));
}

fn build_single(page: &adw::PreferencesPage, entry: &FileEntry, stop: &Arc<AtomicBool>) {
    let header = adw::PreferencesGroup::new();

    let icon = gtk::Image::builder()
        .gicon(&crate::ui::thumbs::icon_for(&entry.content_type, entry.is_dir, entry.is_symlink))
        .pixel_size(72)
        .margin_top(8)
        .build();

    let name = gtk::Label::builder()
        .label(&entry.display_name)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .justify(gtk::Justification::Center)
        // Selectable so the name can be copied, but kept out of the focus
        // chain: a selectable label is focusable, and being the first such
        // widget in the dialog it took focus on open and showed the whole name
        // highlighted, as though something were already selected. Dragging
        // across it still selects.
        .selectable(true)
        .can_focus(false)
        .css_classes(["title-2"])
        .build();

    let kind = gtk::Label::builder()
        .label(entry.kind_label())
        .css_classes(["dim-label"])
        .margin_bottom(8)
        .build();

    let hero = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .build();
    hero.append(&icon);
    hero.append(&name);
    hero.append(&kind);
    header.add(&hero);
    page.add(&header);

    let general = adw::PreferencesGroup::builder().title("General").build();

    let size_row = adw::ActionRow::builder().title(tr!("Size")).build();
    if entry.is_dir {
        size_row.set_subtitle("Calculating…");
        spawn_size_walk(&size_row, &entry.path, Arc::clone(stop));
    } else {
        size_row.set_subtitle(&format!(
            "{} ({} bytes)",
            humansize::format_size(entry.size, humansize::DECIMAL),
            entry.size
        ));
    }
    general.add(&size_row);

    general.add(&info_row("Location", &parent_display(&entry.path)));
    general.add(&info_row(tr!("Modified"), &entry.modified_label()));

    if entry.is_symlink {
        let target = entry.symlink_target.clone().unwrap_or_else(|| "unknown".into());
        general.add(&info_row("Symbolic link to", &target));
    }

    page.add(&general);

    let access = adw::PreferencesGroup::builder().title("Permissions").build();
    access.add(&info_row("Mode", &format!("{} ({:o})", entry.permissions_label(), entry.mode & 0o7777)));
    access.add(&info_row(
        "You can",
        &capabilities_label(entry),
    ));
    page.add(&access);

    let advanced = adw::PreferencesGroup::builder().title("Details").build();
    advanced.add(&info_row("Full path", &entry.path.to_string_lossy()));
    advanced.add(&info_row("Media type", &precise_content_type(entry)));
    page.add(&advanced);
}

/// The entry's media type, asking gio to look inside the file if the name alone
/// did not settle it.
///
/// Listing a folder deliberately avoids this: gio classifies an extensionless
/// file by opening and reading it, which is far too expensive to do for every
/// file in a directory (see the Performance notes in the README). For the one
/// file whose properties are open it costs nothing, and it is exactly where the
/// precise answer is worth having.
fn precise_content_type(entry: &FileEntry) -> String {
    if entry.is_dir || entry.content_type != "application/octet-stream" {
        return entry.content_type.clone();
    }
    gio::File::for_path(&entry.path)
        .query_info(
            gio::FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE,
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok()
        .and_then(|info| info.content_type())
        .map(|t| t.to_string())
        .unwrap_or_else(|| entry.content_type.clone())
}

fn build_multiple(page: &adw::PreferencesPage, entries: &[FileEntry], stop: &Arc<AtomicBool>) {
    let folders = entries.iter().filter(|e| e.is_dir).count();
    let files = entries.len() - folders;
    let immediate_bytes: u64 = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();

    let header = adw::PreferencesGroup::new();
    let summary = gtk::Label::builder()
        .label(format!("{} items selected", entries.len()))
        .css_classes(["title-2"])
        .margin_top(8)
        .margin_bottom(8)
        .build();
    header.add(&summary);
    page.add(&header);

    let general = adw::PreferencesGroup::builder().title("General").build();
    general.add(&info_row(
        "Contents",
        &format!(
            "{files} file{}, {folders} folder{}",
            if files == 1 { "" } else { "s" },
            if folders == 1 { "" } else { "s" }
        ),
    ));

    let size_row = adw::ActionRow::builder().title("Total size").build();
    if folders > 0 {
        size_row.set_subtitle("Calculating…");
        let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        spawn_multi_size_walk(&size_row, paths, Arc::clone(stop));
    } else {
        size_row.set_subtitle(&humansize::format_size(immediate_bytes, humansize::DECIMAL));
    }
    general.add(&size_row);

    if let Some(first) = entries.first() {
        general.add(&info_row("Location", &parent_display(&first.path)));
    }
    page.add(&general);
}

fn capabilities_label(entry: &FileEntry) -> String {
    let mut parts = Vec::new();
    if entry.can_read {
        parts.push(if entry.is_dir { "list" } else { "read" });
    }
    if entry.can_write {
        parts.push(if entry.is_dir { "add and remove items" } else { "write" });
    }
    if entry.can_execute {
        parts.push(if entry.is_dir { "enter" } else { "execute" });
    }
    if parts.is_empty() {
        return "nothing".to_string();
    }
    parts.join(", ")
}

fn parent_display(path: &Path) -> String {
    path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".to_string())
}

fn info_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        // Long paths are the reason this dialog exists half the time; let them
        // be selected and copied rather than truncated.
        .subtitle_selectable(true)
        .build();
    row.add_css_class("property");
    row
}

/// Walks one directory on a worker thread, updating the row when done.
fn spawn_size_walk(row: &adw::ActionRow, path: &Path, stop: Arc<AtomicBool>) {
    let (tx, rx) = async_channel::bounded(1);
    let path = path.to_path_buf();
    let stop_for_thread = Arc::clone(&stop);

    std::thread::Builder::new()
        .name("cadrocfile-dirsize".into())
        .spawn(move || {
            let stats =
                crate::fs::scan::dir_stats(&path, &|| stop_for_thread.load(Ordering::Relaxed));
            let _ = tx.send_blocking(stats);
        })
        .ok();

    let row = row.clone();
    glib::spawn_future_local(async move {
        let Ok((bytes, files, dirs)) = rx.recv().await else { return };
        if stop.load(Ordering::Relaxed) {
            return;
        }
        row.set_subtitle(&format!(
            "{} · {files} file{}, {dirs} folder{}",
            humansize::format_size(bytes, humansize::DECIMAL),
            if files == 1 { "" } else { "s" },
            if dirs == 1 { "" } else { "s" }
        ));
    });
}

fn spawn_multi_size_walk(row: &adw::ActionRow, paths: Vec<PathBuf>, stop: Arc<AtomicBool>) {
    let (tx, rx) = async_channel::bounded(1);
    let stop_for_thread = Arc::clone(&stop);

    std::thread::Builder::new()
        .name("cadrocfile-dirsize".into())
        .spawn(move || {
            let should_stop = || stop_for_thread.load(Ordering::Relaxed);
            let mut total = 0u64;
            let mut files = 0u64;

            for path in paths {
                if should_stop() {
                    break;
                }
                match std::fs::symlink_metadata(&path) {
                    Ok(md) if md.is_dir() => {
                        let (bytes, count, _) = crate::fs::scan::dir_stats(&path, &should_stop);
                        total += bytes;
                        files += count;
                    }
                    Ok(md) => {
                        total += md.len();
                        files += 1;
                    }
                    Err(_) => {}
                }
            }
            let _ = tx.send_blocking((total, files));
        })
        .ok();

    let row = row.clone();
    glib::spawn_future_local(async move {
        let Ok((bytes, files)) = rx.recv().await else { return };
        if stop.load(Ordering::Relaxed) {
            return;
        }
        row.set_subtitle(&format!(
            "{} in {files} file{}",
            humansize::format_size(bytes, humansize::DECIMAL),
            if files == 1 { "" } else { "s" }
        ));
    });
}
