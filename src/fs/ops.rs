//! Copy / move / delete, executed on a worker thread with progress reporting.
//!
//! Every long-running job follows the same shape: the caller gets a
//! [`JobHandle`] holding a progress receiver and a cancel flag, and the worker
//! streams [`Progress`] messages until it sends exactly one `Finished`.
//! Conflicts are resolved by handing the UI a one-shot reply channel and
//! blocking the worker on it, which keeps the decision synchronous with the
//! copy loop without the worker ever touching a widget.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Instant,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

/// Chunk size for the progress-reporting copy loop.
const COPY_BUF: usize = 1024 * 1024;

/// Minimum gap between `Item` messages. Comfortably finer than the UI's own
/// repaint interval, so the displayed name still keeps up with the work.
pub(crate) const ITEM_INTERVAL_MS: u128 = 20;

/// Files at or below this size are copied with `std::fs::copy`, which can use
/// `copy_file_range`/reflink. Sub-second copies don't need byte-level progress,
/// and giving up the kernel fast path for them would be a real slowdown.
const SMALL_FILE_FAST_PATH: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Skip,
    SkipAll,
    Replace,
    ReplaceAll,
    /// Copy alongside the existing file under a generated name.
    Rename,
    RenameAll,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    Delete,
    Trash,
    Shred,
    Download,
}

impl JobKind {
    pub fn verb(self) -> &'static str {
        match self {
            JobKind::Copy => "Copying",
            JobKind::Move => "Moving",
            JobKind::Delete => "Deleting",
            JobKind::Trash => "Moving to Trash",
            JobKind::Shred => "Shredding",
            JobKind::Download => "Downloading",
        }
    }
}

#[derive(Debug)]
pub enum Progress {
    /// Emitted after the pre-walk, once the totals are known.
    Prepared { total_bytes: u64, total_items: u64 },
    /// A new item started; `done_items` counts items already finished.
    Item { name: String, done_items: u64 },
    /// Cumulative bytes processed across the whole job.
    Bytes { done_bytes: u64 },
    /// The destination exists. The worker blocks until a choice is sent back.
    Conflict {
        source: PathBuf,
        dest: PathBuf,
        reply: async_channel::Sender<ConflictChoice>,
    },
    /// A single item failed; the job continues with the rest.
    ItemFailed { path: PathBuf, error: String },
    Finished(JobOutcome),
}

#[derive(Debug, Default)]
pub struct JobOutcome {
    pub cancelled: bool,
    pub items_done: u64,
    pub bytes_done: u64,
    pub errors: Vec<(PathBuf, String)>,
    /// Paths actually written, so the caller can select them afterwards.
    pub created: Vec<PathBuf>,
}

pub struct JobHandle {
    pub kind: JobKind,
    pub progress: async_channel::Receiver<Progress>,
    cancel: Arc<AtomicBool>,
}

impl JobHandle {
    pub(crate) fn new(
        kind: JobKind,
        progress: async_channel::Receiver<Progress>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self { kind, progress, cancel }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }
}

/// Shared worker state, threaded through the recursive helpers.
struct Job {
    tx: async_channel::Sender<Progress>,
    cancel: Arc<AtomicBool>,
    bytes_done: u64,
    items_done: u64,
    /// Sticky answer from a `*All` conflict choice.
    blanket: Option<ConflictChoice>,
    errors: Vec<(PathBuf, String)>,
    created: Vec<PathBuf>,
    /// Throttles `Bytes` messages so a fast copy doesn't flood the channel.
    last_report: u64,
    /// Throttles `Item` messages for the same reason.
    ///
    /// A permanent delete reports every file it removes, and a folder can hold
    /// tens of thousands; each message carries a heap-allocated name into an
    /// unbounded channel. The UI only repaints a few times a second, so almost
    /// all of that would be allocated, queued and dropped unseen.
    last_item: Instant,
}

impl Job {
    fn cancelled(&self) -> bool {
        self.cancel.load(AtomicOrdering::Relaxed)
    }

    fn send(&self, p: Progress) {
        // A closed channel means the window went away; the cancel flag is the
        // authority on stopping, so a failed send is simply ignored.
        let _ = self.tx.send_blocking(p);
    }

    /// Counts an item and reports it, at most [`ITEM_INTERVAL_MS`] apart.
    ///
    /// The count itself is always exact — only the notification is dropped —
    /// so the outcome and the progress denominator stay right. `name` is a
    /// closure so a skipped message costs nothing to allocate.
    fn item(&mut self, name: impl FnOnce() -> String) {
        self.items_done += 1;
        if self.last_item.elapsed().as_millis() >= ITEM_INTERVAL_MS {
            self.last_item = Instant::now();
            let done_items = self.items_done;
            self.send(Progress::Item { name: name(), done_items });
        }
    }

    fn add_bytes(&mut self, n: u64) {
        self.bytes_done += n;
        if self.bytes_done - self.last_report >= COPY_BUF as u64 {
            self.last_report = self.bytes_done;
            self.send(Progress::Bytes { done_bytes: self.bytes_done });
        }
    }

    fn fail(&mut self, path: &Path, err: impl std::fmt::Display) {
        let msg = err.to_string();
        self.send(Progress::ItemFailed { path: path.to_path_buf(), error: msg.clone() });
        self.errors.push((path.to_path_buf(), msg));
    }

    /// Asks the UI what to do about an existing destination, honouring any
    /// blanket answer already given.
    fn resolve_conflict(&mut self, source: &Path, dest: &Path) -> ConflictChoice {
        if let Some(blanket) = self.blanket {
            return blanket;
        }
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        self.send(Progress::Conflict {
            source: source.to_path_buf(),
            dest: dest.to_path_buf(),
            reply: reply_tx,
        });
        // If the UI drops the channel without answering (window closed), treat
        // it as a cancel rather than silently overwriting.
        let choice = reply_rx.recv_blocking().unwrap_or(ConflictChoice::Cancel);
        match choice {
            ConflictChoice::SkipAll | ConflictChoice::ReplaceAll | ConflictChoice::RenameAll => {
                self.blanket = Some(choice);
            }
            _ => {}
        }
        choice
    }
}

/// Walks `sources` to total up bytes and item count before any work starts.
fn measure(sources: &[PathBuf], cancel: &AtomicBool) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut items = 0u64;
    for src in sources {
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }
        match fs::symlink_metadata(src) {
            Ok(md) if md.is_dir() => {
                items += 1;
                for e in walkdir::WalkDir::new(src).follow_links(false).into_iter().filter_map(|e| e.ok()) {
                    if cancel.load(AtomicOrdering::Relaxed) {
                        break;
                    }
                    if e.depth() == 0 {
                        continue;
                    }
                    items += 1;
                    if let Ok(m) = e.metadata()
                        && !m.is_dir()
                    {
                        bytes += m.len();
                    }
                }
            }
            Ok(md) => {
                items += 1;
                bytes += md.len();
            }
            Err(_) => {}
        }
    }
    (bytes, items)
}

/// Returns a non-colliding sibling of `dest`: `name (copy).ext`, then
/// `name (copy 2).ext`, matching what GNOME generates.
pub fn unique_destination(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let parent = dest.parent().unwrap_or(Path::new("."));
    let stem = dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // Use the full compound extension for archives so `x.tar.gz` becomes
    // `x (copy).tar.gz` rather than `x.tar (copy).gz`.
    let (stem, ext) = split_compound_extension(dest, &stem);

    for n in 1..10_000u32 {
        let suffix = if n == 1 { " (copy)".to_string() } else { format!(" (copy {n})") };
        let candidate = match &ext {
            Some(e) => parent.join(format!("{stem}{suffix}.{e}")),
            None => parent.join(format!("{stem}{suffix}")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    // Pathological fallback; a timestamp will not collide in practice.
    parent.join(format!("{stem}-{}", chrono::Local::now().format("%Y%m%d%H%M%S")))
}

fn split_compound_extension(path: &Path, stem: &str) -> (String, Option<String>) {
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for compound in ["tar.gz", "tar.bz2", "tar.xz", "tar.zst", "tar.lz4", "tar.lzma"] {
        if let Some(base) = name.strip_suffix(&format!(".{compound}")) {
            return (base.to_string(), Some(compound.to_string()));
        }
    }
    (stem.to_string(), path.extension().map(|e| e.to_string_lossy().into_owned()))
}

/// Copies one regular file, streaming progress for anything large enough that
/// the user would otherwise watch a frozen bar.
fn copy_file(job: &mut Job, src: &Path, dest: &Path, len: u64) -> io::Result<()> {
    if len <= SMALL_FILE_FAST_PATH {
        let n = fs::copy(src, dest)?;
        job.add_bytes(n);
        return Ok(());
    }

    let mut reader = fs::File::open(src)?;
    let mut writer = fs::File::create(dest)?;
    let mut buf = vec![0u8; COPY_BUF];

    loop {
        if job.cancelled() {
            // Remove the half-written file so a cancelled copy doesn't leave a
            // truncated file that looks complete in the listing.
            drop(writer);
            let _ = fs::remove_file(dest);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        job.add_bytes(n as u64);
    }
    writer.flush()?;

    // Carry over the mode; times are restored by the caller via `copy_metadata`.
    if let Ok(md) = src.metadata() {
        let _ = writer.set_permissions(md.permissions());
    }
    Ok(())
}

/// Restores mtime (and mode for directories) after content is in place.
fn copy_metadata(src: &Path, dest: &Path) {
    let Ok(md) = fs::metadata(src) else { return };
    let _ = fs::set_permissions(dest, md.permissions());
    if let Ok(mtime) = md.modified() {
        let _ = fs::File::open(dest).and_then(|f| f.set_modified(mtime));
    }
}

/// Recursively copies `src` to `dest`, resolving conflicts as it goes.
fn copy_recursive(job: &mut Job, src: &Path, dest: &Path, top_level: bool) {
    if job.cancelled() {
        return;
    }

    let md = match fs::symlink_metadata(src) {
        Ok(md) => md,
        Err(e) => return job.fail(src, e),
    };

    let mut dest = dest.to_path_buf();
    if dest.symlink_metadata().is_ok() {
        // Merging two directories is the expected behaviour, not a conflict —
        // only leaf collisions need a decision.
        let both_dirs = md.is_dir() && dest.is_dir();
        if !both_dirs {
            match job.resolve_conflict(src, &dest) {
                ConflictChoice::Skip | ConflictChoice::SkipAll => return,
                ConflictChoice::Cancel => {
                    job.cancel.store(true, AtomicOrdering::Relaxed);
                    return;
                }
                ConflictChoice::Rename | ConflictChoice::RenameAll => {
                    dest = unique_destination(&dest);
                }
                ConflictChoice::Replace | ConflictChoice::ReplaceAll => {
                    let removed = if dest.is_dir() {
                        fs::remove_dir_all(&dest)
                    } else {
                        fs::remove_file(&dest)
                    };
                    if let Err(e) = removed {
                        return job.fail(&dest, e);
                    }
                }
            }
        }
    }

    job.item(|| src.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());

    if md.file_type().is_symlink() {
        // Copy the link itself, not its target: following it would silently
        // duplicate whole trees and break relative links.
        match fs::read_link(src) {
            Ok(target) => {
                let _ = fs::remove_file(&dest);
                if let Err(e) = std::os::unix::fs::symlink(&target, &dest) {
                    job.fail(src, e);
                    return;
                }
            }
            Err(e) => return job.fail(src, e),
        }
    } else if md.is_dir() {
        if let Err(e) = fs::create_dir_all(&dest) {
            return job.fail(&dest, e);
        }
        let entries = match fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => return job.fail(src, e),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            if job.cancelled() {
                return;
            }
            copy_recursive(job, &entry.path(), &dest.join(entry.file_name()), false);
        }
        copy_metadata(src, &dest);
    } else {
        if let Err(e) = copy_file(job, src, &dest, md.len()) {
            if e.kind() != io::ErrorKind::Interrupted {
                job.fail(src, e);
            }
            return;
        }
        copy_metadata(src, &dest);
    }

    if top_level {
        job.created.push(dest);
    }
}

/// Spawns a copy or move job. `is_move` renames where possible and only falls
/// back to copy+delete across filesystem boundaries.
pub fn start_transfer(sources: Vec<PathBuf>, dest_dir: PathBuf, is_move: bool) -> JobHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let kind = if is_move { JobKind::Move } else { JobKind::Copy };

    let worker_cancel = Arc::clone(&cancel);
    std::thread::Builder::new()
        .name("cadrocfile-transfer".into())
        .spawn(move || {
            let (total_bytes, total_items) = measure(&sources, &worker_cancel);
            let _ = tx.send_blocking(Progress::Prepared { total_bytes, total_items });

            let mut job = Job {
                tx,
                cancel: worker_cancel,
                bytes_done: 0,
                items_done: 0,
                blanket: None,
                errors: Vec::new(),
                created: Vec::new(),
                last_report: 0,
                last_item: Instant::now(),
            };

            for src in &sources {
                if job.cancelled() {
                    break;
                }
                let Some(name) = src.file_name() else {
                    job.fail(src, "path has no file name");
                    continue;
                };
                let dest = dest_dir.join(name);

                // Refuse to copy a directory into itself — the recursion would
                // otherwise grow forever and fill the disk.
                if src.is_dir() && dest.starts_with(src) {
                    job.fail(src, "cannot copy a folder into itself");
                    continue;
                }

                if is_move {
                    match try_rename(&mut job, src, &dest) {
                        RenameResult::Done(final_dest) => {
                            job.created.push(final_dest);
                            job.item(|| name.to_string_lossy().into_owned());
                            // A rename moves every byte at once; credit them so
                            // the bar reflects work actually completed.
                            if let Ok(md) = fs::symlink_metadata(src) {
                                job.add_bytes(md.len());
                            }
                            continue;
                        }
                        RenameResult::Skipped => continue,
                        RenameResult::NeedsCopy => {}
                    }
                }

                copy_recursive(&mut job, src, &dest, true);

                if is_move && !job.cancelled() {
                    let removed = if src.is_dir() {
                        fs::remove_dir_all(src)
                    } else {
                        fs::remove_file(src)
                    };
                    if let Err(e) = removed {
                        job.fail(src, e);
                    }
                }
            }

            let outcome = JobOutcome {
                cancelled: job.cancelled(),
                items_done: job.items_done,
                bytes_done: job.bytes_done,
                errors: std::mem::take(&mut job.errors),
                created: std::mem::take(&mut job.created),
            };
            job.send(Progress::Finished(outcome));
        })
        .expect("spawn transfer thread");

    JobHandle { kind, progress: rx, cancel }
}

enum RenameResult {
    Done(PathBuf),
    Skipped,
    /// Cross-device or otherwise un-renameable; fall back to copy + delete.
    NeedsCopy,
}

fn try_rename(job: &mut Job, src: &Path, dest: &Path) -> RenameResult {
    let mut dest = dest.to_path_buf();

    if dest.symlink_metadata().is_ok() {
        // Directory merges can't be done by rename; hand them to the copy path.
        if src.is_dir() && dest.is_dir() {
            return RenameResult::NeedsCopy;
        }
        match job.resolve_conflict(src, &dest) {
            ConflictChoice::Skip | ConflictChoice::SkipAll => return RenameResult::Skipped,
            ConflictChoice::Cancel => {
                job.cancel.store(true, AtomicOrdering::Relaxed);
                return RenameResult::Skipped;
            }
            ConflictChoice::Rename | ConflictChoice::RenameAll => dest = unique_destination(&dest),
            ConflictChoice::Replace | ConflictChoice::ReplaceAll => {
                let removed = if dest.is_dir() { fs::remove_dir_all(&dest) } else { fs::remove_file(&dest) };
                if let Err(e) = removed {
                    job.fail(&dest, e);
                    return RenameResult::Skipped;
                }
            }
        }
    }

    match fs::rename(src, &dest) {
        Ok(()) => RenameResult::Done(dest),
        // EXDEV: different filesystems, which is the normal case when moving to
        // a USB drive. Anything else is a real error, but copy+delete is still
        // a reasonable second attempt, so report only if that also fails.
        Err(_) => RenameResult::NeedsCopy,
    }
}

/// Removes a tree, reporting each file as it goes. Returns files removed.
///
/// This walks and unlinks in the same pass. The previous version measured the
/// tree first and then called `remove_dir_all`, which meant traversing every
/// inode twice to produce a total the progress bar never actually counted up
/// to — `remove_dir_all` reports nothing, so a 36,000-file folder showed
/// "1 of 36743" and then jumped to done. Walking once is both faster and the
/// only way to give the count any meaning.
fn delete_tree(job: &mut Job, root: &Path) -> Result<(), String> {
    let md = fs::symlink_metadata(root).map_err(|e| e.to_string())?;
    if !md.is_dir() {
        job.item(|| root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        return fs::remove_file(root).map_err(|e| e.to_string());
    }

    // `contents_first` so a directory is only removed once it is empty.
    for entry in walkdir::WalkDir::new(root)
        .contents_first(true)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if job.cancelled() {
            return Ok(());
        }
        let path = entry.path();
        let result = if entry.file_type().is_dir() {
            fs::remove_dir(path)
        } else {
            job.item(|| entry.file_name().to_string_lossy().into_owned());
            fs::remove_file(path)
        };
        if let Err(e) = result {
            job.fail(path, e);
        }
    }
    Ok(())
}

/// Counts the files under `paths` so the progress bar has a denominator.
///
/// Deliberately counts only — no `stat` per entry. Byte totals are meaningless
/// for a delete (nothing is written) and reading the size of every file was the
/// bulk of what the old measuring pass cost.
fn count_files(paths: &[PathBuf], cancel: &AtomicBool) -> u64 {
    let mut items = 0u64;
    for path in paths {
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }
        match fs::symlink_metadata(path) {
            Ok(md) if md.is_dir() => {
                items += walkdir::WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| !e.file_type().is_dir())
                    .count() as u64;
            }
            Ok(_) => items += 1,
            Err(_) => {}
        }
    }
    items
}

/// Spawns a delete job: `Trash` uses the freedesktop trash via gio, `Delete`
/// unlinks permanently.
pub fn start_delete(paths: Vec<PathBuf>, permanent: bool) -> JobHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let kind = if permanent { JobKind::Delete } else { JobKind::Trash };

    let worker_cancel = Arc::clone(&cancel);
    std::thread::Builder::new()
        .name("cadrocfile-delete".into())
        .spawn(move || {
            let total_items = if permanent {
                count_files(&paths, &worker_cancel)
            } else {
                // Trashing is a rename; a per-item count is the only useful
                // measure and walking the tree first would just add latency.
                paths.len() as u64
            };
            let _ = tx.send_blocking(Progress::Prepared { total_bytes: 0, total_items });

            let mut job = Job {
                tx,
                cancel: worker_cancel,
                bytes_done: 0,
                items_done: 0,
                blanket: None,
                errors: Vec::new(),
                created: Vec::new(),
                last_report: 0,
                last_item: Instant::now(),
            };

            for path in &paths {
                if job.cancelled() {
                    break;
                }

                let result = if permanent {
                    delete_tree(&mut job, path)
                } else {
                    job.item(|| {
                        path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                    });
                    super::trash::trash_path(path)
                };

                if let Err(e) = result {
                    job.fail(path, e);
                }
            }

            let outcome = JobOutcome {
                cancelled: job.cancelled(),
                items_done: job.items_done,
                bytes_done: job.bytes_done,
                errors: std::mem::take(&mut job.errors),
                created: Vec::new(),
            };
            job.send(Progress::Finished(outcome));
        })
        .expect("spawn delete thread");

    JobHandle { kind, progress: rx, cancel }
}

/// Renames a single entry in place, rejecting names that would escape the
/// directory or collide.
pub fn rename_in_place(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err("Name cannot be empty".into());
    }
    if trimmed.contains('/') {
        return Err("Name cannot contain '/'".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("Reserved name".into());
    }
    let parent = path.parent().ok_or("Cannot rename the filesystem root")?;
    let dest = parent.join(trimmed);
    if dest == path {
        return Ok(dest);
    }
    if dest.symlink_metadata().is_ok() {
        return Err(format!("“{trimmed}” already exists here"));
    }
    fs::rename(path, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

pub fn create_directory(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let path = validated_child(parent, name)?;
    fs::create_dir(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

pub fn create_file(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let path = validated_child(parent, name)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    Ok(path)
}

fn validated_child(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Name cannot be empty".into());
    }
    if trimmed.contains('/') {
        return Err("Name cannot contain '/'".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("Reserved name".into());
    }
    let path = parent.join(trimmed);
    if path.symlink_metadata().is_ok() {
        return Err(format!("“{trimmed}” already exists here"));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn unique_destination_preserves_compound_extensions() {
        let dir = tempdir();
        let target = dir.join("backup.tar.gz");
        fs::write(&target, b"x").unwrap();
        let unique = unique_destination(&target);
        assert_eq!(unique.file_name().unwrap(), "backup (copy).tar.gz");
    }

    #[test]
    fn unique_destination_returns_input_when_free() {
        let dir = tempdir();
        let target = dir.join("fresh.txt");
        assert_eq!(unique_destination(&target), target);
    }

    #[test]
    fn rename_rejects_path_separators() {
        let dir = tempdir();
        let file = dir.join("a.txt");
        fs::write(&file, b"x").unwrap();
        assert!(rename_in_place(&file, "../escape").is_err());
        assert!(file.exists());
    }

    /// Runs a job to completion, answering any conflict with `choice`.
    fn drain(job: JobHandle, choice: ConflictChoice) -> JobOutcome {
        loop {
            match job.progress.recv_blocking().expect("job ended without Finished") {
                Progress::Conflict { reply, .. } => {
                    reply.send_blocking(choice).expect("worker stopped listening");
                }
                Progress::Finished(outcome) => return outcome,
                _ => {}
            }
        }
    }

    #[test]
    fn copying_a_tree_reproduces_its_contents() {
        let root = tempdir();
        let src = root.join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.txt"), b"alpha").unwrap();
        fs::write(src.join("nested/b.bin"), vec![9u8; 32_000]).unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();

        let dest = root.join("dest");
        fs::create_dir_all(&dest).unwrap();

        let outcome = drain(start_transfer(vec![src.clone()], dest.clone(), false), ConflictChoice::Skip);

        assert!(outcome.errors.is_empty(), "unexpected errors: {:?}", outcome.errors);
        assert_eq!(fs::read(dest.join("src/a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(dest.join("src/nested/b.bin")).unwrap().len(), 32_000);
        // The link must be copied as a link, not resolved into a second copy.
        assert!(fs::symlink_metadata(dest.join("src/link")).unwrap().file_type().is_symlink());
        // The source is untouched by a copy.
        assert!(src.join("a.txt").exists());
    }

    #[test]
    fn moving_within_a_filesystem_removes_the_source() {
        let root = tempdir();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("move-me.txt");
        fs::write(&file, b"payload").unwrap();

        let dest = root.join("dest");
        fs::create_dir_all(&dest).unwrap();

        let outcome = drain(start_transfer(vec![file.clone()], dest.clone(), true), ConflictChoice::Skip);

        assert!(outcome.errors.is_empty(), "unexpected errors: {:?}", outcome.errors);
        assert_eq!(fs::read(dest.join("move-me.txt")).unwrap(), b"payload");
        assert!(!file.exists());
    }

    #[test]
    fn keep_both_writes_alongside_the_existing_file() {
        let root = tempdir();
        let src = root.join("src");
        let dest = root.join("dest");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("dup.txt"), b"new").unwrap();
        fs::write(dest.join("dup.txt"), b"old").unwrap();

        let outcome = drain(
            start_transfer(vec![src.join("dup.txt")], dest.clone(), false),
            ConflictChoice::Rename,
        );

        assert!(outcome.errors.is_empty(), "unexpected errors: {:?}", outcome.errors);
        assert_eq!(fs::read(dest.join("dup.txt")).unwrap(), b"old");
        assert_eq!(fs::read(dest.join("dup (copy).txt")).unwrap(), b"new");
    }

    #[test]
    fn skipping_a_conflict_leaves_the_destination_alone() {
        let root = tempdir();
        let src = root.join("src");
        let dest = root.join("dest");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(src.join("dup.txt"), b"new").unwrap();
        fs::write(dest.join("dup.txt"), b"old").unwrap();

        drain(start_transfer(vec![src.join("dup.txt")], dest.clone(), false), ConflictChoice::Skip);

        assert_eq!(fs::read(dest.join("dup.txt")).unwrap(), b"old");
        assert!(!dest.join("dup (copy).txt").exists());
    }

    #[test]
    fn copying_a_folder_into_itself_is_refused() {
        let root = tempdir();
        let src = root.join("self");
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::write(src.join("f.txt"), b"x").unwrap();

        let outcome =
            drain(start_transfer(vec![src.clone()], src.join("inner"), false), ConflictChoice::Skip);

        assert_eq!(outcome.errors.len(), 1);
        assert!(outcome.errors[0].1.contains("into itself"));
    }

    #[test]
    fn permanent_delete_removes_a_whole_tree() {
        let root = tempdir();
        let victim = root.join("victim");
        fs::create_dir_all(victim.join("deep")).unwrap();
        fs::write(victim.join("deep/f.txt"), b"x").unwrap();

        let outcome = drain(start_delete(vec![victim.clone()], true), ConflictChoice::Skip);

        assert!(outcome.errors.is_empty(), "unexpected errors: {:?}", outcome.errors);
        assert!(!victim.exists());
    }

    fn tempdir() -> TempDir {
        let p = TempDir::new("ops");
        fs::create_dir_all(&p).unwrap();
        p
    }
}
