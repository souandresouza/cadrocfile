//! Recursive search beneath a directory, by name or by contents.
//!
//! Runs on a worker thread and streams matches back in batches, so results
//! appear while the walk is still going rather than after it finishes.
//!
//! The walk is deliberately cheap: `readdir` already reports whether an entry
//! is a directory, so the common case costs no `stat` at all. Only entries whose
//! *name* matches are stat'd to build a full [`FileEntry`], which keeps a search
//! over a home directory bound by I/O rather than by syscalls.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gio::prelude::*;

use crate::fs::FileEntry;

/// Results are streamed in batches of this size.
const BATCH: usize = 48;

/// Hard cap on results. Past this the list is useless to a human anyway, and
/// holding more only costs memory.
const MAX_RESULTS: usize = 5_000;

/// Directories never worth descending into: kernel and runtime pseudo-
/// filesystems that contain no user files but plenty of infinite depth.
const SKIP_ABSOLUTE: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp/.X11-unix"];

pub struct SearchHandle {
    pub results: async_channel::Receiver<SearchEvent>,
    cancel: Arc<AtomicBool>,
}

impl SearchHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        // Dropping the handle must stop the walk; otherwise navigating away
        // leaves a thread churning through a home directory for nothing.
        self.cancel();
    }
}

#[derive(Debug)]
pub enum SearchEvent {
    Matches(Vec<FileEntry>),
    /// The walk ended. `truncated` means the result cap was reached.
    Finished { total: usize, truncated: bool },
}

/// Starts a case-insensitive substring search for `query` under `root`.
///
/// `skip_hidden` mirrors the view's own hidden-file setting, so a search does
/// not surface things the folder listing would hide.
pub fn start(root: PathBuf, query: String, skip_hidden: bool) -> SearchHandle {
    let (tx, rx) = async_channel::bounded(8);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let needle = query.to_lowercase();

    std::thread::Builder::new()
        .name("cadrocfile-search".into())
        .spawn(move || {
            let mut batch = Vec::with_capacity(BATCH);
            let mut total = 0usize;
            let mut truncated = false;

            let walker = walkdir::WalkDir::new(&root)
                // Never follow links: a single symlink back up the tree would
                // turn this into an unbounded walk.
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| should_descend(entry, skip_hidden));

            for entry in walker {
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(entry) = entry else { continue };
                // Depth 1 is the folder's own listing, which the view already
                // holds and filters locally — matching it here would duplicate
                // every direct child.
                if entry.depth() <= 1 {
                    continue;
                }

                let name = entry.file_name().to_string_lossy();
                if !name.to_lowercase().contains(&needle) {
                    continue;
                }

                // Only now is a `stat` worth paying for.
                let Some(file_entry) = describe(entry.path()) else { continue };
                batch.push(file_entry);
                total += 1;

                if total >= MAX_RESULTS {
                    truncated = true;
                    break;
                }
                if batch.len() >= BATCH {
                    // A full channel means the UI hasn't drained yet; blocking
                    // here throttles the walk to the speed results are consumed.
                    if tx.send_blocking(SearchEvent::Matches(std::mem::take(&mut batch))).is_err() {
                        return;
                    }
                    batch = Vec::with_capacity(BATCH);
                }
            }

            if !batch.is_empty() {
                let _ = tx.send_blocking(SearchEvent::Matches(batch));
            }
            let _ = tx.send_blocking(SearchEvent::Finished { total, truncated });
        })
        .expect("spawn search thread");

    SearchHandle { results: rx, cancel }
}

/// Files larger than this are only searched up to this point. Text worth
/// searching is almost never this big; logs that are get their first 100 MB.
const MAX_CONTENT_BYTES: u64 = 100 * 1024 * 1024;

/// Read size once a file is known to be text.
const CHUNK: usize = 1024 * 1024;

/// How much of a file is inspected before deciding it is binary. The same
/// test and window as `file` and git: text essentially never contains a NUL.
const SNIFF: usize = 8192;

/// Case-insensitive substring matching over raw file bytes.
///
/// ASCII queries — nearly all of them — lowercase the file's bytes in place
/// and hand them to `memchr`'s SIMD searcher, which never decodes UTF-8 at
/// all. Anything else needs Unicode case folding (`CAFÉ` must find `café`), so
/// it decodes and lowercases each window as text.
struct Matcher {
    ascii: Option<memchr::memmem::Finder<'static>>,
    unicode: String,
    /// Bytes carried from one chunk into the next, so a match straddling a
    /// chunk boundary is still found.
    overlap: usize,
}

impl Matcher {
    fn new(query: &str) -> Self {
        let lower = query.to_lowercase();
        let ascii = query
            .is_ascii()
            .then(|| memchr::memmem::Finder::new(lower.as_bytes()).into_owned());
        // A lowercased character can take more bytes than the original, so the
        // unicode path carries a generous margin.
        let overlap = lower.len() * 4 + 4;
        Self { ascii, unicode: lower, overlap }
    }

    fn find(&self, window: &[u8]) -> bool {
        match &self.ascii {
            Some(finder) => finder.find(window).is_some(),
            None => String::from_utf8_lossy(window).to_lowercase().contains(&self.unicode),
        }
    }

    /// Whether `path` contains the query. Binary files never match.
    fn matches_file(&self, path: &Path, chunk: usize, cancel: &AtomicBool) -> bool {
        use std::io::Read;
        let Ok(file) = std::fs::File::open(path) else { return false };
        let mut reader = file.take(MAX_CONTENT_BYTES);

        let mut window: Vec<u8> = Vec::with_capacity(chunk + self.overlap);
        let mut buffer = vec![0u8; chunk.max(SNIFF)];
        let mut sniffed = false;

        loop {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            // The first read is only as big as the sniff, so a binary file —
            // most of what a home directory holds by size — costs 8 KB, not a
            // megabyte.
            let want = if sniffed { chunk } else { SNIFF };
            let read = read_full(&mut reader, &mut buffer[..want]);
            if read == 0 {
                return false;
            }
            let fresh = &mut buffer[..read];
            if !sniffed {
                if memchr::memchr(0, fresh).is_some() {
                    return false;
                }
                sniffed = true;
            }
            if self.ascii.is_some() {
                fresh.make_ascii_lowercase();
            }

            window.extend_from_slice(fresh);
            if self.find(&window) {
                return true;
            }
            let keep = window.len().min(self.overlap);
            window.drain(..window.len() - keep);
        }
    }
}

/// Fills `buffer` as far as the file allows; short reads are normal on pipes
/// and network filesystems and must not be mistaken for the end.
fn read_full(reader: &mut impl std::io::Read, buffer: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    filled
}

/// Starts a search for files *containing* `query` under `root`.
///
/// Reading files is the whole cost, so the work is spread across the shared
/// thread budget: one thread walks the tree and feeds paths to workers, which
/// read and match in parallel. The walk runs ahead through a bounded queue,
/// so a slow disk throttles it rather than letting it buffer the whole tree.
///
/// Unlike the name search, the folder's own files are included — the view's
/// local filter matches names, so it cannot have found them already.
pub fn start_contents(root: PathBuf, query: String, skip_hidden: bool) -> SearchHandle {
    let (tx, rx) = async_channel::bounded(8);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("cadrocfile-grep".into())
        .spawn(move || {
            let matcher = Arc::new(Matcher::new(&query));
            let cancel = worker_cancel;

            let (path_tx, path_rx) = std::sync::mpsc::sync_channel::<PathBuf>(256);
            let path_rx = Arc::new(std::sync::Mutex::new(path_rx));
            let (hit_tx, hit_rx) = std::sync::mpsc::channel::<PathBuf>();

            let walker = {
                let cancel = Arc::clone(&cancel);
                std::thread::Builder::new().name("cadrocfile-grep-walk".into()).spawn(move || {
                    let walk = walkdir::WalkDir::new(&root)
                        .follow_links(false)
                        .into_iter()
                        .filter_entry(|entry| should_descend(entry, skip_hidden));
                    for entry in walk.flatten() {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        // Regular files only: opening a FIFO blocks until
                        // something writes to it, which could be never.
                        if entry.file_type().is_file()
                            && path_tx.send(entry.into_path()).is_err()
                        {
                            break;
                        }
                    }
                })
            };

            let workers: Vec<_> = (0..super::parallel::threads())
                .filter_map(|_| {
                    let (path_rx, hit_tx) = (Arc::clone(&path_rx), hit_tx.clone());
                    let (matcher, cancel) = (Arc::clone(&matcher), Arc::clone(&cancel));
                    std::thread::Builder::new()
                        .name("cadrocfile-grep-read".into())
                        .spawn(move || {
                            loop {
                                // Held only for the `recv`, never while reading.
                                let next = path_rx.lock().ok().and_then(|rx| rx.recv().ok());
                                let Some(path) = next else { break };
                                // After a cancel the queue is still drained, so
                                // a walker blocked on a full queue can see the
                                // flag and stop, instead of waiting forever.
                                if cancel.load(Ordering::Relaxed) {
                                    continue;
                                }
                                if matcher.matches_file(&path, CHUNK, &cancel)
                                    && hit_tx.send(path).is_err()
                                {
                                    break;
                                }
                            }
                        })
                        .ok()
                })
                .collect();
            // The workers hold the only senders now, so the channel closes
            // exactly when the last of them finishes.
            drop(hit_tx);

            let mut batch = Vec::with_capacity(BATCH);
            let mut total = 0usize;
            let mut truncated = false;
            loop {
                if cancel.load(Ordering::Relaxed) && !truncated {
                    break;
                }
                // A timeout, so a slow trickle of matches still reaches the
                // screen instead of waiting for a full batch.
                match hit_rx.recv_timeout(std::time::Duration::from_millis(150)) {
                    Ok(path) => {
                        if truncated {
                            continue;
                        }
                        if let Some(entry) = describe(&path) {
                            batch.push(entry);
                            total += 1;
                        }
                        if total >= MAX_RESULTS {
                            truncated = true;
                            cancel.store(true, Ordering::Relaxed);
                        }
                        if batch.len() >= BATCH
                            && tx.send_blocking(SearchEvent::Matches(std::mem::take(&mut batch))).is_err()
                        {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if !batch.is_empty()
                            && tx.send_blocking(SearchEvent::Matches(std::mem::take(&mut batch))).is_err()
                        {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            if let Ok(walker) = walker {
                let _ = walker.join();
            }
            for worker in workers {
                let _ = worker.join();
            }
            // A search the user abandoned reports nothing further.
            if cancel.load(Ordering::Relaxed) && !truncated {
                return;
            }
            if !batch.is_empty() {
                let _ = tx.send_blocking(SearchEvent::Matches(batch));
            }
            let _ = tx.send_blocking(SearchEvent::Finished { total, truncated });
        })
        .expect("spawn content search thread");

    SearchHandle { results: rx, cancel }
}

/// Whether to walk into a directory at all.
fn should_descend(entry: &walkdir::DirEntry, skip_hidden: bool) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();

    // Hidden directories are skipped wholesale rather than per-entry: descending
    // into .git or node_modules' dotfiles produces thousands of matches nobody
    // asked for.
    if skip_hidden && name.starts_with('.') {
        return false;
    }
    if entry.file_type().is_dir() && SKIP_ABSOLUTE.iter().any(|s| entry.path() == Path::new(s)) {
        return false;
    }
    true
}

/// Builds a display entry for a match.
fn describe(path: &Path) -> Option<FileEntry> {
    let file = gio::File::for_path(path);
    let info = file
        .query_info(
            crate::fs::entry::QUERY_ATTRS,
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let parent = path.parent()?;
    Some(FileEntry::from_info(parent, &info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;

    fn tree() -> TempDir {
        let root = TempDir::new("search");
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::create_dir_all(root.join(".hidden/deep")).unwrap();
        fs::write(root.join("top-cadrocfile.txt"), b"x").unwrap();          // depth 1
        fs::write(root.join("a/cadrocfile-notes.md"), b"x").unwrap();       // depth 2
        fs::write(root.join("a/b/c/CADROCFILE-deep.rs"), b"x").unwrap();    // depth 4
        fs::write(root.join("a/unrelated.txt"), b"x").unwrap();
        fs::write(root.join(".hidden/deep/cadrocfile-secret"), b"x").unwrap();
        root
    }

    /// Drains a search to completion and returns the matched file names.
    fn collect(handle: &SearchHandle) -> (Vec<String>, usize) {
        let mut names = Vec::new();
        let mut total = 0;
        loop {
            match handle.results.recv_blocking() {
                Ok(SearchEvent::Matches(batch)) => {
                    names.extend(batch.into_iter().map(|e| e.display_name));
                }
                Ok(SearchEvent::Finished { total: t, .. }) => {
                    total = t;
                    break;
                }
                Err(_) => break,
            }
        }
        names.sort();
        (names, total)
    }

    #[test]
    fn finds_matches_at_any_depth_below_the_folder() {
        let root = tree();
        let handle = start(root.path().to_path_buf(), "cadrocfile".into(), true);
        let (names, total) = collect(&handle);

        // Depth 1 is deliberately excluded: the view already lists and filters
        // the folder's own children.
        assert!(!names.contains(&"top-cadrocfile.txt".to_string()), "got {names:?}");
        assert!(names.contains(&"cadrocfile-notes.md".to_string()), "got {names:?}");
        assert!(names.contains(&"CADROCFILE-deep.rs".to_string()), "got {names:?}");
        assert!(!names.contains(&"unrelated.txt".to_string()));
        assert_eq!(total, names.len());
    }

    #[test]
    fn matching_is_case_insensitive() {
        let root = tree();
        let handle = start(root.path().to_path_buf(), "CaDrOcFiLe-DeEp".into(), true);
        let (names, _) = collect(&handle);
        assert_eq!(names, vec!["CADROCFILE-deep.rs".to_string()]);
    }

    #[test]
    fn hidden_directories_are_skipped_unless_asked_for() {
        let root = tree();

        let (hidden_off, _) = collect(&start(root.path().to_path_buf(), "secret".into(), true));
        assert!(hidden_off.is_empty(), "got {hidden_off:?}");

        let (hidden_on, _) = collect(&start(root.path().to_path_buf(), "secret".into(), false));
        assert_eq!(hidden_on, vec!["cadrocfile-secret".to_string()]);
    }

    #[test]
    fn dropping_the_handle_stops_the_walk() {
        let root = tree();
        let handle = start(root.path().to_path_buf(), "cadrocfile".into(), true);
        handle.cancel();
        // After cancelling, the channel closes without delivering `Finished`.
        // The walk must not keep the thread alive holding the directory open.
        while let Ok(event) = handle.results.recv_blocking() {
            if matches!(event, SearchEvent::Finished { .. }) {
                break;
            }
        }
    }

    fn contents_tree() -> TempDir {
        let root = TempDir::new("grep");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join(".cache")).unwrap();
        fs::write(root.join("notes.txt"), "remember the Quarterly Report").unwrap();     // depth 1
        fs::write(root.join("src/main.rs"), "fn main() { quarterly_report(); }").unwrap();
        fs::write(root.join("src/deep/other.rs"), "nothing to see").unwrap();
        fs::write(root.join(".cache/hit.txt"), "quarterly report").unwrap();
        // The phrase is present, but so is a NUL: binary, never a match.
        let mut blob = b"\x00\x01binary ".to_vec();
        blob.extend_from_slice(b"quarterly report");
        fs::write(root.join("image.bin"), blob).unwrap();
        root
    }

    #[test]
    fn contents_are_searched_at_every_depth_including_the_folder_itself() {
        let root = contents_tree();
        let handle = start_contents(root.path().to_path_buf(), "quarterly".into(), true);
        let (names, total) = collect(&handle);
        assert_eq!(names, ["main.rs", "notes.txt"], "got {names:?}");
        assert_eq!(total, 2);
    }

    #[test]
    fn binary_files_are_never_reported_even_when_they_contain_the_text() {
        let root = contents_tree();
        let handle = start_contents(root.path().to_path_buf(), "quarterly report".into(), true);
        let (names, _) = collect(&handle);
        assert!(!names.contains(&"image.bin".to_string()), "got {names:?}");
    }

    #[test]
    fn hidden_folders_are_searched_only_when_hidden_files_are_shown() {
        let root = contents_tree();
        let (hidden_off, _) = collect(&start_contents(root.path().to_path_buf(), "quarterly report".into(), true));
        assert!(!hidden_off.contains(&"hit.txt".to_string()));
        let (hidden_on, _) = collect(&start_contents(root.path().to_path_buf(), "quarterly report".into(), false));
        assert!(hidden_on.contains(&"hit.txt".to_string()), "got {hidden_on:?}");
    }

    /// Reading in chunks is what keeps memory flat on big files, and it is the
    /// classic place to lose a match: the text split across two reads.
    #[test]
    fn a_match_split_across_two_chunks_is_still_found() {
        let dir = TempDir::new("grep-chunk");
        let file = dir.join("big.txt");
        let chunk = SNIFF * 2;
        // Put the phrase so it starts a few bytes before the second read ends.
        let mut text = "a".repeat(SNIFF + chunk - 4);
        text.push_str("NEEDLE-in-the-haystack");
        text.push_str(&"b".repeat(100));
        fs::write(&file, &text).unwrap();

        let cancel = AtomicBool::new(false);
        for query in ["needle-in-the-haystack", "NEEDLE"] {
            let matcher = Matcher::new(query);
            assert!(matcher.matches_file(&file, chunk, &cancel), "{query:?} was missed at the seam");
        }
        assert!(!Matcher::new("not present").matches_file(&file, chunk, &cancel));
    }

    /// ASCII lowercasing would leave É alone and miss this.
    #[test]
    fn non_ascii_text_matches_case_insensitively() {
        let dir = TempDir::new("grep-unicode");
        let file = dir.join("menu.txt");
        fs::write(&file, "Le CAFÉ est ouvert").unwrap();
        let cancel = AtomicBool::new(false);
        assert!(Matcher::new("café").matches_file(&file, CHUNK, &cancel));
        assert!(Matcher::new("Ouvert").matches_file(&file, CHUNK, &cancel));
    }

    #[test]
    fn an_empty_file_matches_nothing() {
        let dir = TempDir::new("grep-empty");
        let file = dir.join("empty");
        fs::write(&file, "").unwrap();
        assert!(!Matcher::new("x").matches_file(&file, CHUNK, &AtomicBool::new(false)));
    }

    /// Dropping the handle has to stop every thread, not only the walker.
    #[test]
    fn a_cancelled_content_search_ends_promptly() {
        let root = TempDir::new("grep-cancel");
        for i in 0..400 {
            fs::write(root.join(format!("f{i}.txt")), "match me ".repeat(2000)).unwrap();
        }
        let handle = start_contents(root.path().to_path_buf(), "match".into(), true);
        handle.cancel();
        let started = std::time::Instant::now();
        // The channel closes once the coordinator returns.
        while handle.results.recv_blocking().is_ok() {}
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "search did not stop");
    }
}

