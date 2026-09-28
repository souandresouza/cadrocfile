//! Archive inspection, extraction and creation.
//!
//! Extraction is done in-process through libarchive (via `compress-tools`),
//! which covers zip, 7z, rar/rar5, tar with every common compressor, cab, iso,
//! xar, lha and ar from one code path. Entries are written out one at a time
//! rather than with `uncompress_archive` so the job can report byte-level
//! progress, be cancelled mid-file, and — importantly — sanitise every entry
//! path before it is used.
//!
//! The archive is read exactly once. Listing a `.tar.gz` or `.tar.xz` means
//! decompressing all of it, so the old "inspect, then extract" pair paid the
//! decompression bill twice — 44% of the total on a `.tar.xz`. Everything the
//! listing was needed for is now derived during the single extracting pass:
//! see [`start_extract`] for how the destination folder is settled afterwards
//! with a rename instead of decided in advance.
use crate::tr;

use std::{
    fs,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    },
};

use compress_tools::{ArchiveContents, ArchiveIteratorBuilder, ArchivePassword};

use crate::fs::ops::{ITEM_INTERVAL_MS, JobHandle, JobKind, JobOutcome, Progress};

/// Extensions we offer to extract. Anything libarchive sniffs successfully will
/// still work; this list drives the tr!("Extract Here") menu item's visibility.
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "cbz", "jar", "war", "epub", "odt", "ods", "odp", "xpi", "apk", "whl", "7z", "cb7",
    "rar", "cbr", "tar", "tgz", "tbz", "tbz2", "txz", "tzst", "gz", "bz2", "xz", "zst", "lz4",
    "lzma", "lz", "lzo", "iso", "cab", "arj", "lha", "lzh", "ar", "deb", "rpm", "cpio", "xar",
    "cbt", "z",
];

/// Compound suffixes that must be stripped whole when naming the output folder,
/// so `linux-6.9.tar.xz` extracts to `linux-6.9/` and not `linux-6.9.tar/`.
const COMPOUND_SUFFIXES: &[&str] = &[
    ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz4", ".tar.lzma", ".tar.lz", ".tar.z",
];

/// Buffer size used on both ends of the extraction stream.
///
/// `compress-tools` hands libarchive a 16 KB window, so an unbuffered read of a
/// 500 MB archive is 32,000 syscalls; the same applies on the way out, where
/// libarchive's decompressed blocks are far smaller than a useful write. One
/// megabyte on each side turns both into a syscall per megabyte.
const STREAM_BUF: usize = 1024 * 1024;

/// Smallest per-entry write buffer.
///
/// Entries are buffered to their own size rather than to [`STREAM_BUF`], so a
/// tiny file gets a tiny buffer; this is the floor, chosen to still absorb the
/// handful of blocks libarchive splits a small entry into.
const WRITE_BUF_MIN: usize = 16 * 1024;

/// True when the path looks like something we can extract.
pub fn is_archive(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if COMPOUND_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|ext| ARCHIVE_EXTENSIONS.contains(&ext.as_str()))
}

/// The archive's base name with its extension (compound or not) removed.
pub fn archive_stem(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = name.to_lowercase();
    for suffix in COMPOUND_SUFFIXES {
        if lower.ends_with(suffix) {
            return name[..name.len() - suffix.len()].to_string();
        }
    }
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(name)
}

fn looks_encrypted(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("passphrase") || m.contains("password") || m.contains("encrypted")
}

/// Rewrites an archive entry name into a path guaranteed to stay under the
/// extraction root.
///
/// Absolute paths and `..` components are how a malicious archive escapes the
/// destination ("zip slip"); both are stripped rather than trusted. Returns
/// `None` for entries that reduce to nothing.
fn sanitize_entry_path(name: &str) -> Option<PathBuf> {
    // Windows-built archives may use backslashes as separators.
    let normalized = name.replace('\\', "/");
    let mut out = PathBuf::new();

    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) => out.push(part),
            // Drop these outright: RootDir/Prefix would make the path absolute,
            // ParentDir would walk out of the destination.
            Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }

    (!out.as_os_str().is_empty()).then_some(out)
}

/// Counts the compressed bytes pulled off the archive as libarchive reads it.
///
/// This is what drives the progress bar. The uncompressed total is not knowable
/// without decompressing the whole archive first, but the compressed total is
/// just the file's size on disk — so measuring the *input* gives an exact,
/// monotonic percentage for one pass of the file, which is precisely what the
/// user is waiting on.
struct CountingReader<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, AtomicOrdering::Relaxed);
        Ok(n)
    }
}

impl<R: Seek> Seek for CountingReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A staging directory inside `into`, on the same filesystem as the final
/// destination so promoting it afterwards is a rename rather than a copy.
fn staging_dir(into: &Path) -> PathBuf {
    crate::fs::ops::unique_destination(&into.join(format!(".cadrocfile-extract-{}", std::process::id())))
}

/// The single top-level entry of `dir`, when it has exactly one and it is a
/// directory.
///
/// This is the "well-rooted archive" test, done after the fact by looking at
/// what was written instead of by listing the archive up front.
fn lone_child_directory(dir: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(dir).ok()?;
    let first = entries.next()?.ok()?;
    if entries.next().is_some() || !first.file_type().ok()?.is_dir() {
        return None;
    }
    Some(first.path())
}

/// Moves everything under `from` into `to`, which already exists.
///
/// Only reached when an extraction's natural destination is a folder that is
/// already there; the entries are renamed one by one so existing files are
/// replaced and unrelated ones are left alone, which is what extracting an
/// archive over a folder of the same name has always done here.
fn merge_into(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let is_dir = entry.file_type()?.is_dir();

        if is_dir && target.is_dir() {
            merge_into(&entry.path(), &target)?;
            let _ = fs::remove_dir(entry.path());
            continue;
        }
        if target.exists() {
            if target.is_dir() {
                fs::remove_dir_all(&target)?;
            } else {
                fs::remove_file(&target)?;
            }
        }
        fs::rename(entry.path(), &target)?;
    }
    Ok(())
}

/// Puts the extracted staging directory where the user expects it, and returns
/// that path.
///
/// A well-rooted archive (everything under one top-level folder) contributes
/// that folder to `into`; anything else keeps the container, named after the
/// archive, so a 400-file tarball doesn't explode into the user's Downloads.
/// Both cases are a rename within `into`, so this costs nothing regardless of
/// how much was extracted.
fn promote(staging: &Path, archive: &Path, into: &Path) -> io::Result<PathBuf> {
    let (source, wanted) = match lone_child_directory(staging) {
        Some(root) => {
            let name = root.file_name().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("extracted"));
            (root, into.join(name))
        }
        None => (staging.to_path_buf(), into.join(archive_stem(archive))),
    };

    if wanted.exists() {
        merge_into(&source, &wanted)?;
        // `merge_into` moves the entries out but leaves the now-empty directory
        // they came from; in the well-rooted case that is the root inside the
        // staging directory, which has to go before the staging directory can.
        let _ = fs::remove_dir(&source);
    } else {
        fs::rename(&source, &wanted)?;
    }

    // A no-op when `source` *was* the staging directory and has just been
    // renamed away; it only does something in the well-rooted case, where the
    // empty container is left behind.
    let _ = fs::remove_dir(staging);
    Ok(wanted)
}

/// Spawns an extraction job that writes into `into`.
///
/// The archive is read once. Because that means the layout is not known until
/// the last entry is out, entries land in a hidden staging directory inside
/// `into` and are moved into place by [`promote`] at the end — a rename, so the
/// choice costs nothing even for a large archive.
pub fn start_extract(archive: PathBuf, into: PathBuf, password: Option<String>) -> JobHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("cadrocfile-extract".into())
        .spawn(move || {
            // The compressed size is known immediately and is what the progress
            // bar is measured against; see `CountingReader`.
            let total_bytes = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
            let _ = tx.send_blocking(Progress::Prepared { total_bytes, total_items: 0 });

            let staging = staging_dir(&into);
            let mut errors: Vec<(PathBuf, String)> = Vec::new();
            let mut created = Vec::new();
            let mut items = 0u64;
            let mut bytes = 0u64;

            match extract_all(&archive, &staging, password.as_deref(), &worker_cancel, &tx) {
                Ok((done_items, done_bytes)) => {
                    items = done_items;
                    bytes = done_bytes;
                    if worker_cancel.load(AtomicOrdering::Relaxed) {
                        // A cancelled extraction leaves a partial tree; it lives
                        // entirely inside the staging directory, so nothing the
                        // user can see is left half-written.
                        let _ = fs::remove_dir_all(&staging);
                    } else {
                        match promote(&staging, &archive, &into) {
                            Ok(path) => created.push(path),
                            Err(e) => errors.push((staging.clone(), e.to_string())),
                        }
                    }
                }
                Err(err) => {
                    let _ = fs::remove_dir_all(&staging);
                    errors.push((archive.clone(), err));
                }
            }

            let _ = tx.send_blocking(Progress::Finished(JobOutcome {
                cancelled: worker_cancel.load(AtomicOrdering::Relaxed),
                items_done: items,
                bytes_done: bytes,
                errors,
                created,
            }));
        })
        .expect("spawn extract thread");

    JobHandle::new(JobKind::Copy, rx, cancel)
}

/// Streams the archive, writing each entry. Returns (items, uncompressed bytes).
fn extract_all(
    archive: &Path,
    dest: &Path,
    password: Option<&str>,
    cancel: &AtomicBool,
    tx: &async_channel::Sender<Progress>,
) -> Result<(u64, u64), String> {
    fs::create_dir_all(dest).map_err(|e| format!("Cannot create {}: {e}", dest.display()))?;

    let source = fs::File::open(archive).map_err(|e| format!("Cannot open archive: {e}"))?;
    let consumed = Arc::new(AtomicU64::new(0));
    let source = BufReader::with_capacity(
        STREAM_BUF,
        CountingReader { inner: source, read: Arc::clone(&consumed) },
    );

    let mut builder = ArchiveIteratorBuilder::new(source);
    if let Some(pw) = password {
        let pw = ArchivePassword::new(pw).map_err(|e| e.to_string())?;
        builder = builder.with_password(pw);
    }
    let iter = builder.build().map_err(|e| e.to_string())?;

    let mut items = 0u64;
    let mut bytes = 0u64;
    let mut last_report = 0u64;
    // An archive can hold tens of thousands of entries; reporting every one
    // would queue a heap-allocated name per file for a UI that repaints a few
    // times a second. The count stays exact either way.
    let mut last_item = std::time::Instant::now();
    let mut current: Option<(BufWriter<fs::File>, PathBuf, u32)> = None;

    for content in iter {
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }

        match content {
            ArchiveContents::StartOfEntry(name, stat) => {
                // Flushing here as well as on `EndOfEntry` guards the case where
                // libarchive moves to the next header without one, which would
                // otherwise drop the tail of the previous file.
                finish_entry(&mut current)?;

                let Some(relative) = sanitize_entry_path(&name) else {
                    continue;
                };
                let target = dest.join(&relative);
                let mode = stat.st_mode & 0o7777;
                let is_dir = name.ends_with('/') || (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR;

                items += 1;
                if last_item.elapsed().as_millis() >= ITEM_INTERVAL_MS {
                    last_item = std::time::Instant::now();
                    let _ = tx.send_blocking(Progress::Item {
                        name: relative.to_string_lossy().into_owned(),
                        done_items: items,
                    });
                }

                if is_dir {
                    fs::create_dir_all(&target).map_err(|e| e.to_string())?;
                    continue;
                }

                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let file = fs::File::create(&target)
                    .map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
                let size = stat.st_size.max(0) as u64;

                // Tell the filesystem the final size up front so it can pick one
                // extent instead of growing the file a block at a time. Only
                // worth a syscall on files big enough to be split up: an archive
                // of tens of thousands of small files would otherwise pay an
                // `ftruncate` each for no benefit.
                if size >= STREAM_BUF as u64 {
                    let _ = file.set_len(size);
                }

                // Size the buffer to the entry. A fixed megabyte per file means
                // allocating and faulting in a megabyte for a 500-byte file, and
                // across a large archive that cost more than the syscalls the
                // buffer was there to save.
                let capacity = size.clamp(WRITE_BUF_MIN as u64, STREAM_BUF as u64) as usize;
                current = Some((BufWriter::with_capacity(capacity, file), target, mode));
            }

            ArchiveContents::DataChunk(chunk) => {
                if let Some((file, target, _)) = current.as_mut() {
                    file.write_all(&chunk)
                        .map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
                    bytes += chunk.len() as u64;

                    let done = consumed.load(AtomicOrdering::Relaxed);
                    if done - last_report >= STREAM_BUF as u64 {
                        last_report = done;
                        let _ = tx.send_blocking(Progress::Bytes { done_bytes: done });
                    }
                }
            }

            ArchiveContents::EndOfEntry => finish_entry(&mut current)?,

            ArchiveContents::Err(err) => {
                let message = err.to_string();
                if looks_encrypted(&message) && password.is_none() {
                    return Err(PASSWORD_REQUIRED.to_string());
                }
                return Err(message);
            }
        }
    }

    finish_entry(&mut current)?;
    Ok((items, bytes))
}

/// Marker error asking the caller to prompt for a passphrase and try again.
pub const PASSWORD_REQUIRED: &str = "PASSWORD_REQUIRED";

/// Flushes and closes the entry being written, applying its permission bits.
fn finish_entry(current: &mut Option<(BufWriter<fs::File>, PathBuf, u32)>) -> Result<(), String> {
    let Some((mut file, target, mode)) = current.take() else { return Ok(()) };
    file.flush().map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
    drop(file);
    apply_mode(&target, mode);
    Ok(())
}

/// Applies the archived permission bits, keeping the executable bit but never
/// granting setuid/setgid from an untrusted archive.
fn apply_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if mode == 0 {
        return;
    }
    let safe = mode & 0o0777;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(safe));
}

/// Archive formats offered when creating a new archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressFormat {
    Zip,
    TarGz,
    TarXz,
    TarZst,
    SevenZip,
}

impl CompressFormat {
    pub const ALL: [CompressFormat; 5] = [
        CompressFormat::Zip,
        CompressFormat::TarGz,
        CompressFormat::TarXz,
        CompressFormat::TarZst,
        CompressFormat::SevenZip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CompressFormat::Zip => "ZIP (.zip)",
            CompressFormat::TarGz => "Gzipped tar (.tar.gz)",
            CompressFormat::TarXz => "XZ tar (.tar.xz)",
            CompressFormat::TarZst => "Zstandard tar (.tar.zst)",
            CompressFormat::SevenZip => "7-Zip (.7z)",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            CompressFormat::Zip => ".zip",
            CompressFormat::TarGz => ".tar.gz",
            CompressFormat::TarXz => ".tar.xz",
            CompressFormat::TarZst => ".tar.zst",
            CompressFormat::SevenZip => ".7z",
        }
    }
}

/// Absolute path to libarchive's CLI.
///
/// Prefer the system copy: a `bsdtar` earlier in `PATH` (conda, homebrew) can
/// be built against a different libarchive with fewer formats compiled in.
fn bsdtar_binary() -> Option<PathBuf> {
    let system = Path::new("/usr/bin/bsdtar");
    if system.is_file() {
        return Some(system.to_path_buf());
    }
    which("bsdtar")
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

/// True when archive creation is available on this system.
pub fn can_compress() -> bool {
    bsdtar_binary().is_some()
}

/// Spawns a job that packs `sources` into a new archive next to them.
///
/// Creation goes through `bsdtar`, which writes every format above from the
/// same libarchive already linked in — reimplementing writers for five
/// container formats in-process would add a lot of surface for no user-visible
/// gain.
pub fn start_compress(
    sources: Vec<PathBuf>,
    dest: PathBuf,
    format: CompressFormat,
) -> JobHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("cadrocfile-compress".into())
        .spawn(move || {
            let total_items = sources.len() as u64;
            let _ = tx.send_blocking(Progress::Prepared { total_bytes: 0, total_items });

            let mut errors = Vec::new();
            let mut created = Vec::new();

            match run_bsdtar(&sources, &dest, format) {
                Ok(()) => created.push(dest.clone()),
                Err(err) => errors.push((dest.clone(), err)),
            }

            let _ = tx.send_blocking(Progress::Finished(JobOutcome {
                cancelled: worker_cancel.load(AtomicOrdering::Relaxed),
                items_done: total_items,
                bytes_done: 0,
                errors,
                created,
            }));
        })
        .expect("spawn compress thread");

    JobHandle::new(JobKind::Copy, rx, cancel)
}

/// The `--options` string that makes a format's compressor multi-threaded, if
/// it has one.
///
/// libarchive compresses on a single thread unless it is told otherwise, which
/// is why `.tar.xz` used to take over a minute for what the machine can do in
/// twelve seconds. Both liblzma and libzstd split the input into independent
/// blocks and compress them in parallel; the thread count comes from the shared
/// budget so a big archive does not take the whole machine with it.
///
/// Deflate has no threaded mode inside libarchive, so `.tar.gz` and `.zip` are
/// unavoidably serial here — [`gzip_pipeline`] covers the `.tar.gz` case
/// instead when a parallel gzip is installed.
fn threading_options(format: CompressFormat, threads: usize) -> Option<String> {
    match format {
        CompressFormat::TarXz => Some(format!("xz:threads={threads}")),
        CompressFormat::TarZst => Some(format!("zstd:threads={threads}")),
        // 7-Zip in libarchive uses LZMA without a threaded encoder path.
        CompressFormat::SevenZip | CompressFormat::TarGz | CompressFormat::Zip => None,
    }
}

/// A drop-in parallel `gzip`, if one is installed.
///
/// libarchive's deflate is single-threaded and there is no option to change
/// that, so the only way to use more than one core for a `.tar.gz` is to let
/// bsdtar write an uncompressed tar to a pipe and compress the pipe. `pigz`
/// produces a bit-for-bit ordinary gzip stream, so nothing downstream can tell
/// the difference.
fn parallel_gzip() -> Option<PathBuf> {
    which("pigz")
}

fn run_bsdtar(sources: &[PathBuf], dest: &Path, format: CompressFormat) -> Result<(), String> {
    let binary = bsdtar_binary().ok_or("bsdtar is not installed (part of the libarchive package)")?;
    let parent = sources
        .first()
        .and_then(|p| p.parent())
        .ok_or("Nothing to compress")?
        .to_path_buf();

    let mut cmd = Command::new(binary);
    // `-C parent` plus bare file names keeps the archive relative: without it
    // every entry would carry the full absolute path from the user's disk.
    cmd.arg("-C").arg(&parent);

    let threads = crate::fs::parallel::threads();
    let gzip = (format == CompressFormat::TarGz && threads > 1)
        .then(parallel_gzip)
        .flatten();

    match format {
        CompressFormat::Zip => cmd.arg("--format=zip"),
        CompressFormat::SevenZip => cmd.arg("--format=7zip"),
        // With a parallel gzip available the tar goes to a pipe uncompressed
        // and `pigz` does the deflate; otherwise bsdtar compresses it itself.
        CompressFormat::TarGz if gzip.is_some() => cmd.arg("--format=ustar"),
        CompressFormat::TarGz => cmd.args(["--format=ustar", "--gzip"]),
        CompressFormat::TarXz => cmd.args(["--format=ustar", "--xz"]),
        CompressFormat::TarZst => cmd.args(["--format=ustar", "--zstd"]),
    };

    if gzip.is_none()
        && let Some(options) = threading_options(format, threads)
    {
        cmd.arg(format!("--options={options}"));
    }

    if let Some(pigz) = &gzip {
        return run_gzip_pipeline(cmd, sources, dest, pigz, threads);
    }

    cmd.arg("-cf").arg(dest);
    for src in sources {
        let Some(name) = src.file_name() else { continue };
        cmd.arg(name);
    }

    let output = cmd.output().map_err(|e| format!("Could not run bsdtar: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    // A failed run can leave a partial archive behind; don't present it as real.
    let _ = fs::remove_file(dest);
    Err(if stderr.trim().is_empty() {
        format!("bsdtar exited with {}", output.status)
    } else {
        stderr.trim().to_string()
    })
}

/// Runs `bsdtar -cf - … | pigz > dest`.
///
/// The archive is only claimed to be written if *both* halves exit cleanly: a
/// bsdtar that dies partway through still produces a valid gzip stream of a
/// truncated tar, which would otherwise look like success.
fn run_gzip_pipeline(
    mut tar: Command,
    sources: &[PathBuf],
    dest: &Path,
    pigz: &Path,
    threads: usize,
) -> Result<(), String> {
    use std::process::Stdio;

    tar.arg("-cf").arg("-");
    for src in sources {
        let Some(name) = src.file_name() else { continue };
        tar.arg(name);
    }
    tar.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut tar_child = tar.spawn().map_err(|e| format!("Could not run bsdtar: {e}"))?;
    let stdout = tar_child.stdout.take().ok_or("bsdtar produced no output")?;
    let target = fs::File::create(dest).map_err(|e| format!("Cannot create {}: {e}", dest.display()))?;

    let gzip = Command::new(pigz)
        .arg(format!("-p{threads}"))
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::from(target))
        .stderr(Stdio::piped())
        .spawn();

    let gzip = match gzip {
        Ok(child) => child,
        Err(e) => {
            let _ = tar_child.kill();
            let _ = tar_child.wait();
            let _ = fs::remove_file(dest);
            return Err(format!("Could not run pigz: {e}"));
        }
    };

    let gzip_out = gzip.wait_with_output().map_err(|e| e.to_string())?;
    let tar_out = tar_child.wait_with_output().map_err(|e| e.to_string())?;

    if tar_out.status.success() && gzip_out.status.success() {
        return Ok(());
    }

    let _ = fs::remove_file(dest);
    let stderr = if tar_out.status.success() {
        String::from_utf8_lossy(&gzip_out.stderr).trim().to_string()
    } else {
        String::from_utf8_lossy(&tar_out.stderr).trim().to_string()
    };
    Err(if stderr.is_empty() { "Archive creation failed".to_string() } else { stderr })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn entry_paths_cannot_escape_the_destination() {
        assert_eq!(sanitize_entry_path("../../etc/passwd"), Some(PathBuf::from("etc/passwd")));
        assert_eq!(sanitize_entry_path("/etc/shadow"), Some(PathBuf::from("etc/shadow")));
        assert_eq!(sanitize_entry_path("a/../../b"), Some(PathBuf::from("a/b")));
        assert_eq!(sanitize_entry_path(".."), None);
        assert_eq!(sanitize_entry_path("windows\\style\\path"), Some(PathBuf::from("windows/style/path")));
    }

    #[test]
    fn compound_extensions_are_stripped_whole() {
        assert_eq!(archive_stem(Path::new("/x/linux-6.9.tar.xz")), "linux-6.9");
        assert_eq!(archive_stem(Path::new("/x/photos.zip")), "photos");
        assert_eq!(archive_stem(Path::new("/x/no-extension")), "no-extension");
    }

    /// End-to-end check that the streaming extractor actually reconstructs an
    /// archive: creation via bsdtar, extraction via the libarchive iterator.
    #[test]
    fn round_trips_a_zip_through_bsdtar_and_the_extractor() {
        if !can_compress() {
            eprintln!("skipping: bsdtar unavailable");
            return;
        }
        let root = TempDir::new("arc");
        let src = root.join("payload");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("top.txt"), b"hello").unwrap();
        fs::write(src.join("nested/deep.bin"), vec![7u8; 5000]).unwrap();

        let archive = root.join("out.zip");
        run_bsdtar(std::slice::from_ref(&src), &archive, CompressFormat::Zip).unwrap();

        let dest = root.join("staging");
        let (tx, _rx) = async_channel::unbounded();
        let cancel = AtomicBool::new(false);
        let (items, bytes) = extract_all(&archive, &dest, None, &cancel, &tx).unwrap();
        assert!(items >= 2, "expected both files to be reported, got {items}");
        assert_eq!(bytes, 5005, "expected the uncompressed payload size");

        assert_eq!(fs::read(dest.join("payload/top.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dest.join("payload/nested/deep.bin")).unwrap().len(), 5000);
    }

    /// The well-rooted case: one top-level folder is lifted out of the staging
    /// directory into the destination under its own name.
    #[test]
    fn a_well_rooted_archive_promotes_its_root_folder() {
        let root = TempDir::new("promote-rooted");
        let into = root.join("into");
        let staging = into.join(".staging");
        fs::create_dir_all(staging.join("payload/nested")).unwrap();
        fs::write(staging.join("payload/a.txt"), b"a").unwrap();

        let placed = promote(&staging, Path::new("/x/payload.zip"), &into).unwrap();

        assert_eq!(placed, into.join("payload"));
        assert_eq!(fs::read(into.join("payload/a.txt")).unwrap(), b"a");
        assert!(!staging.exists(), "the staging directory must not be left behind");
    }

    /// The loose case: several top-level entries keep the container, which is
    /// named after the archive so they don't scatter into the folder.
    #[test]
    fn a_loose_archive_keeps_a_container_named_after_it() {
        let root = TempDir::new("promote-loose");
        let into = root.join("into");
        let staging = into.join(".staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("a.txt"), b"a").unwrap();
        fs::write(staging.join("b.txt"), b"b").unwrap();

        let placed = promote(&staging, Path::new("/x/photos.tar.gz"), &into).unwrap();

        assert_eq!(placed, into.join("photos"));
        assert_eq!(fs::read(into.join("photos/a.txt")).unwrap(), b"a");
        assert!(into.join("photos/b.txt").exists());
    }

    /// Extracting over a folder that is already there replaces the entries the
    /// archive carries and leaves everything else alone — the behaviour the
    /// old extract-straight-into-place path had.
    #[test]
    fn promoting_onto_an_existing_folder_merges_into_it() {
        let root = TempDir::new("promote-merge");
        let into = root.join("into");
        let existing = into.join("payload");
        fs::create_dir_all(existing.join("nested")).unwrap();
        fs::write(existing.join("keep.txt"), b"keep").unwrap();
        fs::write(existing.join("nested/old.txt"), b"old").unwrap();

        let staging = into.join(".staging");
        fs::create_dir_all(staging.join("payload/nested")).unwrap();
        fs::write(staging.join("payload/nested/old.txt"), b"new").unwrap();

        let placed = promote(&staging, Path::new("/x/payload.zip"), &into).unwrap();

        assert_eq!(placed, existing);
        assert_eq!(fs::read(existing.join("keep.txt")).unwrap(), b"keep", "untouched file must survive");
        assert_eq!(fs::read(existing.join("nested/old.txt")).unwrap(), b"new", "archived file must win");
        assert!(!staging.exists());
    }

    /// A single top-level *file* is not a root; that archive keeps its container.
    #[test]
    fn a_lone_top_level_file_is_not_a_root() {
        let root = TempDir::new("lone");
        fs::write(root.join("only.txt"), b"x").unwrap();
        assert_eq!(lone_child_directory(root.path()), None);

        fs::remove_file(root.join("only.txt")).unwrap();
        let dir = root.join("sub");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(lone_child_directory(root.path()), Some(dir));
    }

    /// Only the codecs with a parallel encoder get a thread count; passing one
    /// where libarchive has no threaded path makes bsdtar reject the whole run.
    #[test]
    fn only_threadable_codecs_are_given_a_thread_count() {
        assert_eq!(threading_options(CompressFormat::TarXz, 6).as_deref(), Some("xz:threads=6"));
        assert_eq!(threading_options(CompressFormat::TarZst, 4).as_deref(), Some("zstd:threads=4"));
        assert_eq!(threading_options(CompressFormat::TarGz, 8), None);
        assert_eq!(threading_options(CompressFormat::Zip, 8), None);
        assert_eq!(threading_options(CompressFormat::SevenZip, 8), None);
    }

    /// The whole extraction job, end to end: an archive on disk goes in, and a
    /// correctly placed tree with the right contents comes out — including the
    /// staging directory being cleaned up and the created path being reported
    /// so the window can select it.
    #[test]
    fn the_extract_job_places_a_correct_tree_and_reports_it() {
        if !can_compress() {
            eprintln!("skipping: bsdtar unavailable");
            return;
        }
        let root = TempDir::new("extract-job");
        let src = root.join("project");
        fs::create_dir_all(src.join("deep/deeper")).unwrap();
        fs::write(src.join("readme.md"), b"# hi").unwrap();
        fs::write(src.join("deep/deeper/data.bin"), vec![9u8; 40_000]).unwrap();

        let archive = root.join("project.tar.zst");
        run_bsdtar(std::slice::from_ref(&src), &archive, CompressFormat::TarZst).unwrap();

        let into = root.join("into");
        fs::create_dir_all(&into).unwrap();
        let job = start_extract(archive, into.clone(), None);

        let mut outcome = None;
        while let Ok(message) = job.progress.recv_blocking() {
            if let Progress::Finished(done) = message {
                outcome = Some(done);
            }
        }
        let outcome = outcome.expect("the job must report exactly one outcome");

        assert!(outcome.errors.is_empty(), "unexpected errors: {:?}", outcome.errors);
        assert!(!outcome.cancelled);
        assert_eq!(outcome.created, vec![into.join("project")], "must report where it landed");
        assert_eq!(fs::read(into.join("project/readme.md")).unwrap(), b"# hi");
        assert_eq!(fs::read(into.join("project/deep/deeper/data.bin")).unwrap().len(), 40_000);

        let leftovers: Vec<_> = fs::read_dir(&into)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "project")
            .collect();
        assert!(leftovers.is_empty(), "staging directory left behind: {leftovers:?}");
    }

}

#[cfg(test)]
mod pigz_tests {
    use super::*;
    use crate::testing::TempDir;

    /// The parallel-gzip pipeline is only taken when a `pigz` is on `PATH`, and
    /// most machines (including CI) do not have one. A shim with the same
    /// stdin→stdout gzip contract exercises the plumbing — pipe wiring, exit
    /// codes, cleanup on failure — without depending on it being installed.
    fn write_shim(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let shim = dir.join("pigz-shim");
        fs::write(&shim, "#!/bin/sh\nexec gzip -c\n").unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        shim
    }

    #[test]
    fn the_gzip_pipeline_writes_a_readable_archive() {
        if !can_compress() {
            eprintln!("skipping: bsdtar unavailable");
            return;
        }
        let root = TempDir::new("pigz");
        fs::create_dir_all(root.join("payload")).unwrap();
        fs::write(root.join("payload/a.txt"), b"hello").unwrap();
        fs::write(root.join("payload/b.txt"), b"world").unwrap();
        let shim = write_shim(root.path());

        let src = root.join("payload");
        let dest = root.join("out.tar.gz");
        let mut cmd = Command::new(bsdtar_binary().unwrap());
        cmd.arg("-C").arg(root.path()).arg("--format=ustar");
        run_gzip_pipeline(cmd, std::slice::from_ref(&src), &dest, &shim, 4).unwrap();

        // Read it back through the real extractor: had the pipe mangled the
        // stream at all, this would fail.
        let out = root.join("out");
        let (tx, _rx) = async_channel::unbounded();
        let cancel = AtomicBool::new(false);
        extract_all(&dest, &out, None, &cancel, &tx).unwrap();
        assert_eq!(fs::read(out.join("payload/a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(out.join("payload/b.txt")).unwrap(), b"world");
    }

    /// A tar that dies partway still produces a *valid* gzip stream of a
    /// truncated archive, so the pipeline has to check both halves; otherwise a
    /// failed run leaves behind a file that looks like a real archive.
    #[test]
    fn a_failed_run_leaves_no_partial_archive() {
        if !can_compress() {
            eprintln!("skipping: bsdtar unavailable");
            return;
        }
        let root = TempDir::new("pigz-fail");
        let shim = write_shim(root.path());
        let dest = root.join("bad.tar.gz");

        let mut cmd = Command::new(bsdtar_binary().unwrap());
        cmd.arg("-C").arg(root.path()).arg("--format=ustar");
        let result = run_gzip_pipeline(cmd, &[root.join("does-not-exist")], &dest, &shim, 4);

        assert!(result.is_err(), "a missing source must fail the job");
        assert!(!dest.exists(), "a failed run must not leave a partial archive");
    }
}
