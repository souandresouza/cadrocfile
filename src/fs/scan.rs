//! Directory enumeration.
//!
//! Listing runs as a GLib future on the main context rather than on a worker
//! thread: gio's async enumerator already does its I/O off-thread, and staying
//! on the main context means the resulting `FileEntry` values need no
//! cross-thread synchronisation before they reach the list model.
use crate::tr;

use std::path::{Path, PathBuf};

use gio::prelude::*;

use super::entry::{FileEntry, QUERY_ATTRS};

/// How many entries to pull per `next_files` round-trip.
///
/// Large enough that a 100k-entry directory doesn't cost 100k round-trips,
/// small enough that the first batch paints almost immediately.
const BATCH: i32 = 256;

/// Enumerates `path`, reporting each batch through `on_batch` as it arrives.
///
/// Partial results are kept on error: a directory that becomes unreadable
/// halfway through should still show what was listed, with the error surfaced
/// separately.
pub async fn scan_dir<F>(
    path: &Path,
    cancellable: &gio::Cancellable,
    mut on_batch: F,
) -> Result<(), glib::Error>
where
    F: FnMut(Vec<FileEntry>),
{
    let dir = gio::File::for_path(path);
    let enumerator = dir
        .enumerate_children_future(
            QUERY_ATTRS,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
        )
        .await?;

    loop {
        if cancellable.is_cancelled() {
            return Ok(());
        }

        let infos = enumerator
            .next_files_future(BATCH, glib::Priority::DEFAULT)
            .await?;

        if infos.is_empty() {
            break;
        }

        let batch: Vec<FileEntry> = infos
            .iter()
            .map(|info| FileEntry::from_info(path, info))
            .collect();
        on_batch(batch);
    }

    // Closing releases the underlying fd immediately instead of waiting for the
    // enumerator to be dropped by the GC-less refcount at some later point.
    let _ = enumerator.close_future(glib::Priority::DEFAULT).await;
    Ok(())
}

/// Free and total bytes on the filesystem holding `path`, for the status bar.
pub async fn filesystem_usage(path: &Path) -> Option<(u64, u64)> {
    let file = gio::File::for_path(path);
    let info = file
        .query_filesystem_info_future("filesystem::free,filesystem::size", glib::Priority::DEFAULT)
        .await
        .ok()?;
    let free = info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE);
    let size = info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_SIZE);
    (size > 0).then_some((free, size))
}

/// Recursively totals the bytes and item count under `root`.
///
/// Blocking — call from a worker thread. `should_stop` is polled between
/// entries so a properties dialog closing can abandon a huge walk.
pub fn dir_stats(root: &Path, should_stop: &dyn Fn() -> bool) -> (u64, u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut dirs = 0u64;

    // `follow_links(false)` is what keeps a symlink loop from turning this into
    // an infinite walk, and stops a link to / from being counted as content.
    // `same_file_system` keeps it off other disks: a drive or a cloud account
    // mounted somewhere under home would otherwise be walked in full — over
    // the network, for a cloud drive — just to fill in a Properties dialog.
    let walk = walkdir::WalkDir::new(root).follow_links(false).same_file_system(true);
    for entry in walk.into_iter().filter_map(|e| e.ok()) {
        if should_stop() {
            break;
        }
        if entry.depth() == 0 {
            continue;
        }
        match entry.metadata() {
            Ok(md) if md.is_dir() => dirs += 1,
            Ok(md) => {
                files += 1;
                bytes += md.len();
            }
            Err(_) => {}
        }
    }

    (bytes, files, dirs)
}

/// The standard user folders that belong in the sidebar, in the order Nautilus
/// presents them.
///
/// `dirs::*` reads `user-dirs.dirs`, which many systems simply do not have; on
/// those, every lookup returns `None` and the sidebar would show nothing but
/// Home. So each entry falls back to the conventional `$HOME/<Name>` path, and
/// the whole set is filtered by what actually exists on disk.
///
/// Folder names are translated based on the system locale (e.g. "Documentos"
/// in pt-BR) by reading the `XDG_DESKTOP_DIR` etc. keys from `user-dirs.dirs`
/// and translating the directory name.
pub fn xdg_places() -> Vec<(String, PathBuf, &'static str)> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };

    let mut places: Vec<(String, PathBuf, &'static str)> =
        vec![(tr!("Home").to_string(), home.clone(), "user-home-symbolic")];

    let candidates: [(Option<PathBuf>, &str, &str); 6] = [
        (dirs::desktop_dir(), "Desktop", "user-desktop-symbolic"),
        (dirs::document_dir(), "Documents", "folder-documents-symbolic"),
        (dirs::download_dir(), "Downloads", "folder-download-symbolic"),
        (dirs::audio_dir(), "Music", "folder-music-symbolic"),
        (dirs::picture_dir(), "Pictures", "folder-pictures-symbolic"),
        (dirs::video_dir(), "Videos", "folder-videos-symbolic"),
    ];

    for (configured, label, icon) in candidates {
        let path = configured.unwrap_or_else(|| home.join(label));
        // XDG can point several of these at $HOME when the user has no such
        // folder; a "Documents" entry that opens Home would be a lie.
        if path.is_dir() && path != home {
            // Translate the folder name based on the system locale
            let translated = translate_folder_name(label, &path, &home);
            places.push((translated, path, icon));
        }
    }

    places
}

/// Translates a folder name based on the system locale.
///
/// Reads `~/.config/user-dirs.dirs` to get the configured path, extracts the
/// folder name from the path, and translates it using gettext.
///
/// If the folder name on disk is already localized (e.g. "Documentos" in pt-BR),
/// it is used as-is. Otherwise, the English label is translated.
fn translate_folder_name(label: &str, path: &Path, home: &Path) -> String {
    // Try to get the folder name from the path (relative to home)
    let folder_name = path
        .strip_prefix(home)
        .ok()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| label.to_string());

    // If the folder name on disk differs from the English label, it is already
    // localized by the system (e.g. "Documentos" instead of "Documents").
    // Use it as-is.
    if folder_name != label {
        return folder_name;
    }

    // Otherwise, translate the English label using gettext
    tr!(label).to_string()
}
