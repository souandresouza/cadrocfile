//! Freedesktop trash support.
//!
//! Moving to trash goes through gio, which already implements the spec's
//! awkward parts (per-volume `.Trash-$uid` directories, `trashinfo` writing,
//! name collision handling). Listing and restoring read the spec directories
//! directly, because gio's `trash://` backend exposes no restore operation.

use std::{
    fs,
    path::{Path, PathBuf},
};

use gio::prelude::FileExt;

#[derive(Debug, Clone)]
pub struct TrashItem {
    /// Name inside the trash `files/` directory.
    pub name: String,
    /// Where the item lived before deletion.
    pub original_path: PathBuf,
    /// Local time string as recorded in the trashinfo file.
    pub deleted_at: String,
    pub file_path: PathBuf,
    pub info_path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

/// Moves `path` to the appropriate trash directory for its filesystem.
pub fn trash_path(path: &Path) -> Result<(), String> {
    gio::File::for_path(path)
        .trash(gio::Cancellable::NONE)
        .map_err(|e| e.message().to_string())
}

/// The user's home trash directory (`$XDG_DATA_HOME/Trash`).
pub fn home_trash_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("Trash"))
}

/// Every trash directory worth listing: the home trash plus a `.Trash-$uid` on
/// each mounted volume, which is where files deleted from a USB drive land.
fn trash_dirs() -> Vec<PathBuf> {
    let mut dirs_out = Vec::new();
    if let Some(home) = home_trash_dir() {
        dirs_out.push(home);
    }

    let uid = unsafe { libc::getuid() };
    for root in mount_roots() {
        let candidate = root.join(format!(".Trash-{uid}"));
        if candidate.is_dir() {
            dirs_out.push(candidate);
        }
    }
    dirs_out
}

/// Mount points from /proc/mounts that a user could plausibly delete files on.
fn mount_roots() -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let _src = parts.next()?;
            let target = parts.next()?;
            // /proc/mounts escapes spaces as \040; decode so paths match.
            let target = target.replace("\\040", " ");
            let path = PathBuf::from(target);
            let keep = path.starts_with("/media")
                || path.starts_with("/run/media")
                || path.starts_with("/mnt");
            keep.then_some(path)
        })
        .collect()
}

/// Reads every trash directory and returns the items, newest first.
pub fn list_trash() -> Vec<TrashItem> {
    let mut items = Vec::new();

    for trash_dir in trash_dirs() {
        let info_dir = trash_dir.join("info");
        let files_dir = trash_dir.join("files");
        let Ok(entries) = fs::read_dir(&info_dir) else {
            continue;
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let info_path = entry.path();
            if info_path.extension().is_none_or(|e| e != "trashinfo") {
                continue;
            }
            let Some(item) = parse_trashinfo(&info_path, &files_dir) else {
                continue;
            };
            items.push(item);
        }
    }

    items.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    items
}

fn parse_trashinfo(info_path: &Path, files_dir: &Path) -> Option<TrashItem> {
    let text = fs::read_to_string(info_path).ok()?;
    let mut original = None;
    let mut deleted_at = String::new();

    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Path=") {
            // The spec stores the path percent-encoded.
            original = Some(PathBuf::from(
                urlencoding::decode(v).unwrap_or(std::borrow::Cow::Borrowed(v)).into_owned(),
            ));
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            deleted_at = v.to_string();
        }
    }

    // The trashinfo name minus `.trashinfo` is the name inside `files/`.
    let stem = info_path.file_stem()?.to_string_lossy().into_owned();
    let file_path = files_dir.join(&stem);
    let md = fs::symlink_metadata(&file_path).ok()?;

    Some(TrashItem {
        name: stem,
        original_path: original?,
        deleted_at,
        is_dir: md.is_dir(),
        size: if md.is_dir() { 0 } else { md.len() },
        file_path,
        info_path: info_path.to_path_buf(),
    })
}

/// Moves an item back where it came from, recreating missing parents.
///
/// Refuses rather than overwrites if something now occupies the original path.
pub fn restore(item: &TrashItem) -> Result<PathBuf, String> {
    if item.original_path.symlink_metadata().is_ok() {
        return Err(format!(
            "“{}” already exists at the original location",
            item.original_path.display()
        ));
    }
    if let Some(parent) = item.original_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    fs::rename(&item.file_path, &item.original_path).map_err(|e| {
        format!("Could not restore “{}”: {e}", item.name)
    })?;
    // Only drop the metadata once the content is safely back.
    let _ = fs::remove_file(&item.info_path);
    Ok(item.original_path.clone())
}

/// Permanently removes a single trashed item.
pub fn purge(item: &TrashItem) -> Result<(), String> {
    let result = if item.is_dir {
        fs::remove_dir_all(&item.file_path)
    } else {
        fs::remove_file(&item.file_path)
    };
    result.map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&item.info_path);
    Ok(())
}

/// Empties every trash directory, returning how many items were removed and
/// any that could not be.
pub fn empty_trash() -> (usize, Vec<String>) {
    let mut removed = 0usize;
    let mut errors = Vec::new();
    for item in list_trash() {
        match purge(&item) {
            Ok(()) => removed += 1,
            Err(e) => errors.push(format!("{}: {e}", item.name)),
        }
    }
    (removed, errors)
}

/// Number of items currently in the trash, for the sidebar badge.
pub fn trash_count() -> usize {
    list_trash().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    /// Builds a throwaway trash directory laid out per the freedesktop spec.
    fn fake_trash() -> TempDir {
        let root = TempDir::new("trash");
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        root
    }

    #[test]
    fn trashinfo_paths_are_percent_decoded() {
        let trash = fake_trash();
        fs::write(trash.join("files/my report.pdf"), b"pdf").unwrap();
        fs::write(
            trash.join("info/my report.pdf.trashinfo"),
            "[Trash Info]\nPath=/home/someone/Docs/my%20report.pdf\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();

        let item = parse_trashinfo(
            &trash.join("info/my report.pdf.trashinfo"),
            &trash.join("files"),
        )
        .expect("trashinfo should parse");

        assert_eq!(item.original_path, PathBuf::from("/home/someone/Docs/my report.pdf"));
        assert_eq!(item.deleted_at, "2026-01-02T03:04:05");
        assert_eq!(item.size, 3);
        assert!(!item.is_dir);
    }

    #[test]
    fn trashinfo_without_its_file_is_skipped() {
        let trash = fake_trash();
        // Metadata with no corresponding entry in files/ is a half-deleted
        // leftover; listing it would show an item that cannot be restored.
        fs::write(
            trash.join("info/ghost.txt.trashinfo"),
            "[Trash Info]\nPath=/home/someone/ghost.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();

        assert!(
            parse_trashinfo(&trash.join("info/ghost.txt.trashinfo"), &trash.join("files")).is_none()
        );
    }

    #[test]
    fn restore_refuses_to_overwrite_an_existing_file() {
        let trash = fake_trash();
        let home = trash.join("home");
        fs::create_dir_all(&home).unwrap();

        let original = home.join("notes.txt");
        fs::write(&original, b"the file that came back").unwrap();
        fs::write(trash.join("files/notes.txt"), b"trashed").unwrap();

        let item = TrashItem {
            name: "notes.txt".into(),
            original_path: original.clone(),
            deleted_at: String::new(),
            file_path: trash.join("files/notes.txt"),
            info_path: trash.join("info/notes.txt.trashinfo"),
            is_dir: false,
            size: 7,
        };

        assert!(restore(&item).is_err());
        assert_eq!(fs::read(&original).unwrap(), b"the file that came back");
    }
}
