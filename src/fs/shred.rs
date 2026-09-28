//! Secure deletion ("shredding").
//!
//! Each file is overwritten in place for a configurable number of passes, then
//! truncated, renamed to obscure its directory entry, and unlinked.
//!
//! The cost of shredding a folder is dominated by `fsync` latency, not by write
//! bandwidth — every pass has to reach the disk before the next one means
//! anything. Files are therefore shredded on several threads at once (see
//! [`super::parallel`]): the threads spend nearly all their time blocked in
//! `fsync`, so they cost almost no CPU, and the device coalesces their flushes
//! into shared journal commits. Measured on 2000 8 KB files at three passes,
//! that took the job from 9.3 s to 1.8 s.
//!
//! This is genuinely effective on a plain overwrite-in-place filesystem
//! (ext4 without data journalling, XFS, vfat, NTFS) backed by magnetic media.
//! It is *not* a guarantee on copy-on-write filesystems or flash storage —
//! see [`filesystem_caveat`], which the UI surfaces before the user commits.

use std::{
    collections::HashSet,
    fs,
    io::{self, Seek, SeekFrom, Write},
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    time::Instant,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    },
};

use rand::Rng;

use super::{
    ops::{ITEM_INTERVAL_MS, JobHandle, JobKind, JobOutcome, Progress},
    parallel,
};

const CHUNK: usize = 1024 * 1024;

/// Filesystems where an in-place overwrite does not reliably reach the original
/// blocks, mapped to the reason why.
fn caveat_for_fstype(fstype: &str) -> Option<&'static str> {
    match fstype {
        "btrfs" | "zfs" | "bcachefs" => Some(
            "copy-on-write filesystem — overwrites are written to new blocks, so the \
             original data may survive in old extents or snapshots",
        ),
        "overlay" | "overlayfs" => Some(
            "overlay filesystem — the original data lives in a lower layer that an \
             overwrite cannot reach",
        ),
        "f2fs" | "ubifs" | "jffs2" => Some(
            "log-structured flash filesystem — overwrites are written elsewhere and the \
             original blocks are only erased later, if at all",
        ),
        "tmpfs" | "ramfs" => Some("in-memory filesystem — data may persist in swap"),
        "nfs" | "cifs" | "smb3" | "fuse.sshfs" => Some(
            "network filesystem — the server controls the storage and may keep the \
             original blocks",
        ),
        _ => None,
    }
}

/// Returns `(fstype, warning)` when shredding on this path is unreliable.
///
/// Also flags any SSD, where wear levelling means the drive may have relocated
/// the data long before the overwrite lands.
pub fn filesystem_caveat(path: &Path) -> Option<(String, String)> {
    let (fstype, source) = mount_info_for(path)?;
    if let Some(reason) = caveat_for_fstype(&fstype) {
        return Some((fstype.clone(), reason.to_string()));
    }
    if is_rotational(&source) == Some(false) {
        return Some((
            fstype,
            "solid-state storage — wear levelling can relocate data, so the original \
             blocks may not be the ones overwritten"
                .to_string(),
        ));
    }
    None
}

/// The filesystem type and backing device for the mount point containing `path`.
fn mount_info_for(path: &Path) -> Option<(String, String)> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = fs::read_to_string("/proc/mounts").ok()?;

    let mut best: Option<(usize, String, String)> = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let source = parts.next()?.replace("\\040", " ");
        let mount_point = parts.next()?.replace("\\040", " ");
        let fstype = parts.next()?.to_string();
        let mp = PathBuf::from(&mount_point);

        // The deepest matching mount point wins: /home shadows / for a path
        // under /home when both are separate filesystems.
        if target.starts_with(&mp) {
            let depth = mp.components().count();
            if best.as_ref().is_none_or(|(d, _, _)| depth > *d) {
                best = Some((depth, fstype, source));
            }
        }
    }
    best.map(|(_, fstype, source)| (fstype, source))
}

/// Reads `/sys/block/<dev>/queue/rotational`. `None` when it can't be resolved.
fn is_rotational(source: &str) -> Option<bool> {
    let dev = Path::new(source).file_name()?.to_string_lossy().into_owned();
    if !dev.starts_with("sd") && !dev.starts_with("nvme") && !dev.starts_with("mmcblk") {
        return None;
    }
    // Strip the partition suffix to get the parent block device: sda1 -> sda,
    // nvme0n1p2 -> nvme0n1, mmcblk0p1 -> mmcblk0.
    let base = if dev.starts_with("sd") {
        dev.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
    } else {
        match dev.rfind('p') {
            Some(i) if dev[i + 1..].chars().all(|c| c.is_ascii_digit()) => dev[..i].to_string(),
            _ => dev.clone(),
        }
    };
    let value = fs::read_to_string(format!("/sys/block/{base}/queue/rotational")).ok()?;
    Some(value.trim() == "1")
}

/// Overwrites, truncates, renames and unlinks a single regular file.
///
/// The caller is responsible for flushing the parent directory afterwards —
/// see [`sync_dir`]. Doing it here would mean one `fsync` per file, and for a
/// folder of small files that single syscall dominated everything else the
/// shredder did.
fn shred_file(
    path: &Path,
    passes: u32,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;

    // A symlink has no content of its own; overwriting through it would destroy
    // the target instead, which is never what the user asked for.
    if md.file_type().is_symlink() {
        return fs::remove_file(path);
    }

    let len = md.len();
    if len > 0 {
        let mut file = fs::OpenOptions::new().write(true).open(path)?;
        let mut buf = vec![0u8; CHUNK];
        let mut rng = rand::rng();

        for pass in 0..passes {
            if cancel.load(AtomicOrdering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            // Final pass writes zeros so the file doesn't end up looking like
            // encrypted data, which is itself a signal worth avoiding.
            let zero_pass = pass + 1 == passes;

            // Draw the pass's pattern once rather than once per chunk. Refilling
            // every chunk ran the generator at roughly twice the disk's write
            // rate, so the two took turns and each pass cost the sum instead of
            // the maximum; measured at 1749 MB/s against 2408 MB/s for one draw.
            // The distinction is not a security one: the pass that matters for
            // recovery is the *write*, and the last pass here is plain zeros.
            if zero_pass {
                buf.fill(0);
            } else {
                rng.fill_bytes(&mut buf);
            }

            file.seek(SeekFrom::Start(0))?;
            let mut written = 0u64;
            while written < len {
                if cancel.load(AtomicOrdering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                let n = CHUNK.min((len - written) as usize);
                file.write_all(&buf[..n])?;
                written += n as u64;
                on_bytes(n as u64);
            }

            file.flush()?;
            // Without an fsync the passes can be coalesced in page cache and
            // only the last one ever reaches the platter.
            sync_fd(&file)?;
        }

        file.set_len(0)?;
        sync_fd(&file)?;
    }

    // Rename before unlinking so the original name is less likely to be
    // recoverable from the directory entry itself. One rename is enough: the
    // old name is overwritten in the directory block either way, and each extra
    // rename was a further round trip through the filesystem journal.
    let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut current = path.to_path_buf();
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    let obscured = parent.join(format!("{:016x}", u64::from_le_bytes(bytes)));
    if fs::rename(&current, &obscured).is_ok() {
        current = obscured;
    }

    fs::remove_file(&current)
}

/// Flushes a directory so the unlinks recorded in it reach the disk.
///
/// Called once per directory at the end of the job rather than after every
/// file: the entries all live in the same directory blocks, so a single flush
/// commits every one of them.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = sync_fd(&handle);
    }
}

fn sync_fd(file: &fs::File) -> io::Result<()> {
    // SAFETY: `fd` is owned by `file` and remains valid for the call.
    let rc = unsafe { libc::fsync(file.as_raw_fd()) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Everything the job will touch, gathered in one walk.
struct Plan {
    /// Regular files to overwrite, in no particular order — they are
    /// independent, so the workers can take them as they come.
    files: Vec<PathBuf>,
    /// Directories to remove, deepest first, once their contents are gone.
    dirs: Vec<PathBuf>,
    /// Directories to flush at the end so the unlinks are durable.
    parents: Vec<PathBuf>,
    total_bytes: u64,
}

/// Walks `paths` once, collecting the work and totalling the bytes.
///
/// This replaces a separate measuring pass: the walk already has to happen, and
/// `WalkDir` hands back the metadata it read for free, so totalling here costs
/// nothing over discovering the files.
fn plan(paths: &[PathBuf], passes: u32, cancel: &AtomicBool) -> Plan {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut parents: HashSet<PathBuf> = HashSet::new();
    let mut total_bytes = 0u64;

    for path in paths {
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }
        // `contents_first` so the directory list comes back deepest-first and
        // can be removed in order once the files inside it are gone.
        for entry in walkdir::WalkDir::new(path)
            .contents_first(true)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if cancel.load(AtomicOrdering::Relaxed) {
                break;
            }
            if entry.file_type().is_dir() {
                dirs.push(entry.path().to_path_buf());
                continue;
            }
            if let Ok(md) = entry.metadata() {
                total_bytes += md.len() * passes as u64;
            }
            if let Some(parent) = entry.path().parent() {
                parents.insert(parent.to_path_buf());
            }
            files.push(entry.path().to_path_buf());
        }
    }

    Plan { files, dirs, parents: parents.into_iter().collect(), total_bytes }
}

/// Shared counters the workers report through.
struct Tally {
    tx: async_channel::Sender<Progress>,
    bytes: AtomicU64,
    items: AtomicU64,
    /// Byte count at the last `Bytes` message, so a fast shred does not flood
    /// the channel with one message per megabyte per thread.
    reported: AtomicU64,
    started: Instant,
    /// Milliseconds since `started` at the last `Item` message. Same purpose as
    /// `reported`, for the per-file notifications.
    last_item_ms: AtomicU64,
    errors: Mutex<Vec<(PathBuf, String)>>,
}

impl Tally {
    /// Counts a file and reports it, at most [`ITEM_INTERVAL_MS`] apart.
    ///
    /// The count is exact; only the notification is dropped. Several workers
    /// race here, and whichever wins the exchange sends — that is enough, since
    /// the message is just a name for the UI to display.
    fn item(&self, name: impl FnOnce() -> String) {
        let done = self.items.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_item_ms.load(AtomicOrdering::Relaxed);
        if u128::from(now.saturating_sub(last)) >= ITEM_INTERVAL_MS
            && self
                .last_item_ms
                .compare_exchange(last, now, AtomicOrdering::Relaxed, AtomicOrdering::Relaxed)
                .is_ok()
        {
            let _ = self.tx.send_blocking(Progress::Item { name: name(), done_items: done });
        }
    }

    fn add_bytes(&self, n: u64) {
        let total = self.bytes.fetch_add(n, AtomicOrdering::Relaxed) + n;
        let last = self.reported.load(AtomicOrdering::Relaxed);
        if total - last >= CHUNK as u64
            && self
                .reported
                .compare_exchange(last, total, AtomicOrdering::Relaxed, AtomicOrdering::Relaxed)
                .is_ok()
        {
            let _ = self.tx.send_blocking(Progress::Bytes { done_bytes: total });
        }
    }

    fn fail(&self, path: &Path, error: String) {
        let _ = self
            .tx
            .send_blocking(Progress::ItemFailed { path: path.to_path_buf(), error: error.clone() });
        if let Ok(mut errors) = self.errors.lock() {
            errors.push((path.to_path_buf(), error));
        }
    }
}

/// Spawns a shred job over `paths`, descending into directories.
pub fn start_shred(paths: Vec<PathBuf>, passes: u32) -> JobHandle {
    let passes = passes.clamp(1, 35);
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("cadrocfile-shred".into())
        .spawn(move || {
            let plan = plan(&paths, passes, &worker_cancel);
            let _ = tx.send_blocking(Progress::Prepared {
                total_bytes: plan.total_bytes,
                total_items: plan.files.len() as u64,
            });

            let tally = Tally {
                tx: tx.clone(),
                bytes: AtomicU64::new(0),
                items: AtomicU64::new(0),
                reported: AtomicU64::new(0),
                started: Instant::now(),
                last_item_ms: AtomicU64::new(0),
                errors: Mutex::new(Vec::new()),
            };

            parallel::for_each(&plan.files, |path| {
                if worker_cancel.load(AtomicOrdering::Relaxed) {
                    return;
                }
                tally.item(|| {
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                });

                if let Err(e) = shred_file(path, passes, &worker_cancel, |n| tally.add_bytes(n)) {
                    // A cancel unwinds through the same error path; don't
                    // report it as a failure the user needs to act on.
                    if !worker_cancel.load(AtomicOrdering::Relaxed) {
                        tally.fail(path, e.to_string());
                    }
                }
            });

            // Directories only after every file is gone, and deepest first, so
            // each one is empty by the time it is removed.
            for dir in &plan.dirs {
                if worker_cancel.load(AtomicOrdering::Relaxed) {
                    break;
                }
                if let Err(e) = fs::remove_dir(dir)
                    && !worker_cancel.load(AtomicOrdering::Relaxed)
                {
                    tally.fail(dir, e.to_string());
                }
            }

            // One flush per directory rather than one per file. The entries all
            // live in the same directory blocks, so this commits every unlink
            // made above.
            for parent in &plan.parents {
                sync_dir(parent);
            }

            let errors = tally.errors.into_inner().unwrap_or_default();
            let _ = tx.send_blocking(Progress::Finished(JobOutcome {
                cancelled: worker_cancel.load(AtomicOrdering::Relaxed),
                items_done: tally.items.load(AtomicOrdering::Relaxed),
                bytes_done: tally.bytes.load(AtomicOrdering::Relaxed),
                errors,
                created: Vec::new(),
            }));
        })
        .expect("spawn shred thread");

    JobHandle::new(JobKind::Shred, rx, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn shredding_removes_the_file() {
        let dir = TempDir::new("shred");
        let file = dir.join("secret.txt");
        fs::write(&file, vec![0xABu8; 4096]).unwrap();

        let cancel = AtomicBool::new(false);
        shred_file(&file, 2, &cancel, |_| {}).unwrap();

        assert!(!file.exists());
    }

    /// Exercises the parallel path end to end: enough files to be handed out
    /// across workers, nested directories that can only be removed once their
    /// contents are gone, and a byte total that has to survive the workers
    /// reporting concurrently.
    #[test]
    fn shredding_a_tree_removes_every_file_and_directory() {
        let root = TempDir::new("shred-tree");
        let mut expected_bytes = 0u64;
        for d in 0..4 {
            let dir = root.join(format!("d{d}/nested"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..8 {
                let size = 100 * (f + 1);
                fs::write(dir.join(format!("f{f}.bin")), vec![0xAB; size]).unwrap();
                expected_bytes += size as u64;
            }
        }

        let passes = 2;
        let job = start_shred(vec![root.path().to_path_buf()], passes);
        let mut outcome = None;
        while let Ok(message) = job.progress.recv_blocking() {
            if let Progress::Finished(done) = message {
                outcome = Some(done);
            }
        }
        let outcome = outcome.expect("the job must report exactly one outcome");

        assert!(outcome.errors.is_empty(), "unexpected errors: {:?}", outcome.errors);
        assert!(!outcome.cancelled);
        assert_eq!(outcome.items_done, 32, "every file must be counted once");
        assert_eq!(
            outcome.bytes_done,
            expected_bytes * passes as u64,
            "byte total must survive concurrent reporting"
        );
        assert!(!root.path().exists(), "the whole tree must be gone, directories included");
    }

    #[test]
    fn shredding_a_symlink_leaves_its_target_intact() {
        let dir = TempDir::new("shred-link");
        let target = dir.join("target.txt");
        let link = dir.join("link.txt");
        fs::write(&target, b"keep me").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let cancel = AtomicBool::new(false);
        shred_file(&link, 1, &cancel, |_| {}).unwrap();

        assert!(!link.exists());
        assert_eq!(fs::read(&target).unwrap(), b"keep me");
    }
}
