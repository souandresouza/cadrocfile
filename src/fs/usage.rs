//! What is taking up the space: the sizes of a folder's children, measured in
//! parallel and reported as each one finishes.
//!
//! Measured the way `du -x` measures, for the same reasons:
//!
//! - **Space on disk, not apparent size.** A 20 GB sparse VM image that holds
//!   2 GB of data takes 2 GB, and that is the number that answers "why is my
//!   disk full". File length would send the user hunting the wrong file.
//! - **One filesystem.** Crossing mount points counts someone else's disk as
//!   this folder's — and a cloud drive mounted under your home would be walked
//!   file by file over the network.
//! - **Hard links once.** Two names for one file occupy its space once.

use std::{
    collections::HashSet,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone)]
pub struct Child {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug)]
pub enum UsageEvent {
    /// The folder's children, before any has been measured.
    Listed(Vec<Child>),
    /// One child's total, in bytes on disk, and how many files it holds.
    Measured { path: PathBuf, bytes: u64, files: u64 },
    /// Everything is measured. `unreadable` counts folders that could not be
    /// opened, so the totals are known to be low.
    Finished { unreadable: u64 },
}

pub struct UsageHandle {
    pub events: async_channel::Receiver<UsageEvent>,
    cancel: Arc<AtomicBool>,
}

impl Drop for UsageHandle {
    fn drop(&mut self) {
        // Drilling into a subfolder or closing the window abandons the walk;
        // a walk of a whole disk carrying on unseen would be pure waste.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Starts measuring every child of `root`.
pub fn start(root: PathBuf, include_hidden: bool) -> UsageHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("cadrocfile-usage".into())
        .spawn(move || {
            let children = list(&root, include_hidden);
            if tx.send_blocking(UsageEvent::Listed(children.clone())).is_err() {
                return;
            }

            // Shared across children so a file hard-linked from two of them
            // still counts once overall.
            let seen = Mutex::new(HashSet::new());
            let unreadable = std::sync::atomic::AtomicU64::new(0);
            let device = std::fs::metadata(&root).map(|m| m.dev()).ok();

            // Largest-first is unknowable in advance, but folders are where the
            // time goes; starting them first keeps one big folder from being
            // the last thing still running on a lone thread.
            let mut ordered = children;
            ordered.sort_by_key(|c| !c.is_dir);

            super::parallel::for_each(&ordered, |child| {
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let (bytes, files) = measure(&child.path, device, &seen, &unreadable, &worker_cancel);
                if !worker_cancel.load(Ordering::Relaxed) {
                    let _ = tx.send_blocking(UsageEvent::Measured { path: child.path.clone(), bytes, files });
                }
            });

            if !worker_cancel.load(Ordering::Relaxed) {
                let unreadable = unreadable.load(Ordering::Relaxed);
                let _ = tx.send_blocking(UsageEvent::Finished { unreadable });
            }
        })
        .expect("spawn usage thread");

    UsageHandle { events: rx, cancel }
}

fn list(root: &Path, include_hidden: bool) -> Vec<Child> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !include_hidden && name.starts_with('.') {
                return None;
            }
            // `file_type` does not follow symlinks, so a link to a directory is
            // listed as the small link it is rather than its target's size.
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            Some(Child { path: entry.path(), name, is_dir })
        })
        .collect()
}

/// Space used by `path` and everything beneath it, on its own filesystem.
fn measure(
    path: &Path,
    device: Option<u64>,
    seen: &Mutex<HashSet<(u64, u64)>>,
    unreadable: &std::sync::atomic::AtomicU64,
    cancel: &AtomicBool,
) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    // `follow_links(false)` alone does not cover the root: walkdir follows a
    // symlink it is *started* on regardless, so a link to a 400 GB folder
    // measured as 400 GB until this was set too.
    let walk = walkdir::WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .same_file_system(true);

    for entry in walk {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if error.io_error().is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied) {
                    unreadable.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
        };
        let Ok(meta) = entry.metadata() else { continue };
        // `same_file_system` stops descent below a mount point, but a child of
        // the root can itself be one (a drive mounted at ~/Backup): it gets
        // listed, and must count as the empty mount point it is from here.
        if entry.depth() == 0 && device.is_some_and(|d| d != meta.dev()) {
            return (0, 0);
        }
        if meta.nlink() > 1 && !meta.is_dir() {
            let key = (meta.dev(), meta.ino());
            if !seen.lock().is_ok_and(|mut seen| seen.insert(key)) {
                continue;
            }
        }
        // `st_blocks` is always in 512-byte units, whatever the filesystem's
        // block size — this is the space actually taken.
        bytes += meta.blocks() * 512;
        if !meta.is_dir() {
            files += 1;
        }
    }
    (bytes, files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn drain(handle: &UsageHandle) -> (Vec<Child>, Vec<(String, u64, u64)>, u64) {
        let (mut listed, mut measured, mut unreadable) = (Vec::new(), Vec::new(), 0);
        while let Ok(event) = handle.events.recv_blocking() {
            match event {
                UsageEvent::Listed(children) => listed = children,
                UsageEvent::Measured { path, bytes, files } => {
                    measured.push((path.file_name().unwrap().to_string_lossy().into_owned(), bytes, files));
                }
                UsageEvent::Finished { unreadable: u } => {
                    unreadable = u;
                    break;
                }
            }
        }
        measured.sort();
        (listed, measured, unreadable)
    }

    #[test]
    fn every_child_is_listed_and_then_measured() {
        let root = TempDir::new("usage");
        std::fs::create_dir_all(root.join("big/deeper")).unwrap();
        std::fs::write(root.join("big/deeper/a.bin"), vec![1u8; 200_000]).unwrap();
        std::fs::write(root.join("big/b.bin"), vec![1u8; 100_000]).unwrap();
        std::fs::write(root.join("small.txt"), b"hi").unwrap();

        let (listed, measured, _) = drain(&start(root.path().to_path_buf(), false));
        assert_eq!(listed.len(), 2);
        assert_eq!(measured.len(), 2, "{measured:?}");

        let big = measured.iter().find(|m| m.0 == "big").unwrap();
        assert!(big.1 >= 300_000, "big holds 300 KB of data, measured {}", big.1);
        assert_eq!(big.2, 2, "two files inside big");
        let small = measured.iter().find(|m| m.0 == "small.txt").unwrap();
        assert!(small.1 < big.1);
    }

    /// Space on disk, not length: a sparse file claims far more than it uses.
    #[test]
    fn sparse_files_count_the_space_they_actually_use() {
        let root = TempDir::new("usage-sparse");
        let file = std::fs::File::create(root.join("disk.img")).unwrap();
        file.set_len(1024 * 1024 * 1024).unwrap(); // 1 GB long, nothing written
        drop(file);

        let (_, measured, _) = drain(&start(root.path().to_path_buf(), false));
        assert!(measured[0].1 < 1024 * 1024, "a sparse 1 GB file measured {} bytes", measured[0].1);
    }

    #[test]
    fn hard_links_are_counted_once() {
        let root = TempDir::new("usage-links");
        std::fs::create_dir_all(root.join("one")).unwrap();
        std::fs::create_dir_all(root.join("two")).unwrap();
        std::fs::write(root.join("one/data"), vec![7u8; 500_000]).unwrap();
        std::fs::hard_link(root.join("one/data"), root.join("two/data")).unwrap();

        let (_, measured, _) = drain(&start(root.path().to_path_buf(), false));
        let total: u64 = measured.iter().map(|m| m.1).sum();
        assert!(total < 900_000, "500 KB linked twice measured {total} — counted twice");
    }

    #[test]
    fn hidden_children_follow_the_hidden_file_setting() {
        let root = TempDir::new("usage-hidden");
        std::fs::write(root.join(".cache"), b"x").unwrap();
        std::fs::write(root.join("seen"), b"x").unwrap();
        assert_eq!(drain(&start(root.path().to_path_buf(), false)).0.len(), 1);
        assert_eq!(drain(&start(root.path().to_path_buf(), true)).0.len(), 2);
    }

    /// A link to a huge folder is a few bytes, not the folder.
    #[test]
    fn symlinks_are_not_followed() {
        let root = TempDir::new("usage-symlink");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/big"), vec![1u8; 400_000]).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        let (listed, measured, _) = drain(&start(root.path().to_path_buf(), false));
        assert!(!listed.iter().find(|c| c.name == "link").unwrap().is_dir);
        let link = measured.iter().find(|m| m.0 == "link").unwrap();
        assert!(link.1 < 100_000, "the link measured {} bytes", link.1);
    }

    #[test]
    fn a_dropped_handle_stops_the_walk() {
        let root = TempDir::new("usage-cancel");
        for i in 0..50 {
            std::fs::create_dir_all(root.join(format!("d{i}/a/b"))).unwrap();
        }
        let handle = start(root.path().to_path_buf(), false);
        let events = handle.events.clone();
        drop(handle);
        let started = std::time::Instant::now();
        while events.recv_blocking().is_ok() {}
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
