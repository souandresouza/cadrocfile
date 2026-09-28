//! Recently used files.
//!
//! Reads the freedesktop recent-files list (`~/.local/share/recently-used.xbel`)
//! through `GtkRecentManager`, which is the same list GTK's file chooser and
//! every other GTK application writes to — so "Recent" here means the same
//! thing it means everywhere else on the desktop.

use std::path::PathBuf;

use gtk::prelude::*;

/// How many entries to show. The list on disk grows to hundreds and is ordered
/// by recency, so anything past the first screenful or two is noise.
const LIMIT: usize = 200;

pub struct RecentItem {
    pub path: PathBuf,
    /// Unix seconds when the file was last opened, for sorting and display.
    pub visited: i64,
}

/// The most recently used local files, newest first.
///
/// Remote URIs are dropped: the rest of the app works in paths, and a
/// `smb://` entry that cannot be opened is worse than no entry. Files that have
/// since been deleted are dropped too — the recent list is not pruned when
/// something is removed, so without this the view fills with dead names.
///
/// Must be called on the main thread; `GtkRecentManager` is not thread-safe.
pub fn list() -> Vec<RecentItem> {
    let manager = gtk::RecentManager::default();
    let mut items: Vec<RecentItem> = manager
        .items()
        .into_iter()
        .filter_map(|info| {
            let uri = info.uri();
            let path = gio::File::for_uri(&uri).path()?;
            if !path.is_file() {
                return None;
            }
            Some(RecentItem {
                path,
                visited: info.visited().to_unix(),
            })
        })
        .collect();

    items.sort_by_key(|item| std::cmp::Reverse(item.visited));
    items.truncate(LIMIT);
    items
}

/// Forgets every entry, which is what a person means by "clear recent".
pub fn clear() {
    let _ = gtk::RecentManager::default().purge_items();
}
