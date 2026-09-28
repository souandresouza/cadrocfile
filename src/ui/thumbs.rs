//! Thumbnails for the icon view: images decoded in-process, and video, PDF
//! and office documents through external thumbnailers
//! ([`super::thumbnailers`]).
//!
//! Three things keep this off the critical path:
//!
//! 1. The **shared freedesktop thumbnail cache** is consulted first. Nautilus,
//!    Thunar and friends populate `~/.cache/thumbnails`, so most images in a
//!    typical folder already have a thumbnail on disk and never need decoding.
//! 2. Decoding is **concurrency-limited**. A folder of 25 MB photos would
//!    otherwise queue forty simultaneous full-resolution decodes the moment it
//!    opened, which is exactly what makes a file manager feel like it hung.
//! 3. A queued request is **re-checked after it acquires a permit**. Rows are
//!    recycled as you scroll, so most queued work is stale by the time it could
//!    run and is dropped instead of decoded.
//!
//! Anything we do generate is written back to the shared cache, so the cost is
//! paid once per image for the whole desktop rather than once per launch.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
};

use gtk::{gdk, prelude::*};

/// Cached textures, keyed by path plus the mtime and requested size, so an
/// edited image or a zoom change re-renders instead of showing a stale bitmap.
type CacheKey = (PathBuf, i64, i32);

/// Upper bound on cached textures. A 128px RGBA thumbnail is ~65 KB, so this
/// caps the in-memory cache at roughly 30 MB.
const MAX_CACHED: usize = 512;

/// The freedesktop cache directories, smallest first, with their pixel size.
const FDO_SIZES: &[(&str, i32)] =
    &[("normal", 128), ("large", 256), ("x-large", 512), ("xx-large", 1024)];

thread_local! {
    static CACHE: RefCell<ThumbCache> = RefCell::new(ThumbCache::default());

    /// Requests currently being served, so N rows wanting the same file cause
    /// one decode rather than N.
    ///
    /// Measured on a folder of 46 wallpapers, rebinding produced 92 decodes
    /// before this existed — every one of them duplicated work already in
    /// flight.
    static IN_FLIGHT: RefCell<HashMap<CacheKey, Vec<async_channel::Sender<Option<gdk::Texture>>>>> =
        RefCell::new(HashMap::new());

    /// Files that could not be thumbnailed this session.
    ///
    /// Rows are rebound constantly as you scroll, and each rebind would
    /// otherwise launch `ffmpeg` on the same broken video again. Kept in
    /// memory rather than written to the shared `fail/` directory, because a
    /// marker there outlives a fix — a thumbnailer installed tomorrow should get
    /// its chance.
    static FAILED: RefCell<std::collections::HashSet<CacheKey>> =
        RefCell::new(std::collections::HashSet::new());

    /// Subdirectories of `thumbnails/fail`, listed once.
    ///
    /// Re-reading the directory per image turned a cheap check into 46
    /// synchronous `readdir` calls on the main thread.
    static FAIL_DIRS: RefCell<Option<Vec<PathBuf>>> = const { RefCell::new(None) };

    /// Permits limiting how many decodes run at once.
    ///
    /// A bounded channel pre-filled with permits is the whole semaphore: taking
    /// one is a `recv`, returning it is a `try_send`. Everything here runs on
    /// the main context, so a thread-local is the right scope.
    static DECODE_PERMITS: (async_channel::Sender<()>, async_channel::Receiver<()>) = {
        let limit = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 6))
            .unwrap_or(4);
        let (tx, rx) = async_channel::bounded(limit);
        for _ in 0..limit {
            let _ = tx.try_send(());
        }
        (tx, rx)
    };
}

#[derive(Default)]
struct ThumbCache {
    map: HashMap<CacheKey, gdk::Texture>,
    /// Insertion order, used to evict the oldest entries in bulk.
    order: Vec<CacheKey>,
}

impl ThumbCache {
    fn get(&self, key: &CacheKey) -> Option<gdk::Texture> {
        self.map.get(key).cloned()
    }

    fn insert(&mut self, key: CacheKey, texture: gdk::Texture) {
        if self.map.insert(key.clone(), texture).is_none() {
            self.order.push(key);
        }
        if self.order.len() > MAX_CACHED {
            // Evict a quarter at a time so this doesn't run on every insert
            // once the cache is warm.
            let drop_count = self.order.len() / 4;
            for key in self.order.drain(..drop_count) {
                self.map.remove(&key);
            }
        }
    }
}

/// Images gdk-pixbuf decodes in-process, which is the fast path.
fn decodes_natively(content_type: &str) -> bool {
    if !content_type.starts_with("image/") {
        return false;
    }
    // SVG is rendered by librsvg through the same loader when it's installed;
    // these are the formats gdk-pixbuf handles natively everywhere.
    !matches!(content_type, "image/x-xcf" | "image/vnd.adobe.photoshop")
}

/// Whether a file of this type and size should get a thumbnail at all.
///
/// The size cap exists because decoding a photo costs time in proportion to
/// its size, so it applies to images only. It would be wrong for anything
/// else: a video thumbnail seeks to one frame, and a 4 GB film costs the same
/// as a 40 MB clip. Applied across the board it hid every video there is.
pub fn worth_thumbnailing(content_type: &str, size: u64, max_image_bytes: u64) -> bool {
    if content_type.starts_with("image/") && size > max_image_bytes {
        return false;
    }
    decodes_natively(content_type) || super::thumbnailers::method_for(content_type).is_some()
}

pub fn cached(path: &Path, mtime: i64, size: i32) -> Option<gdk::Texture> {
    CACHE.with(|c| c.borrow().get(&(path.to_path_buf(), mtime, size)))
}

/// Loads a thumbnail, preferring the shared cache and falling back to decoding.
///
/// `still_wanted` is polled after the concurrency permit is acquired; returning
/// `false` abandons the request. Returns `None` when the file isn't a decodable
/// image, which is common enough (a `.png` that is really HTML) that it is not
/// worth surfacing as an error.
pub async fn load(
    path: PathBuf,
    content_type: &str,
    mtime: i64,
    size: i32,
    still_wanted: impl Fn() -> bool,
) -> Option<gdk::Texture> {
    let key = (path.clone(), mtime, size);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key)) {
        return Some(hit);
    }

    // Someone is already producing exactly this thumbnail — wait for theirs
    // instead of starting a second identical decode.
    if let Some(waiter) = join_in_flight(&key) {
        return waiter.recv().await.ok().flatten();
    }

    let texture = produce(&path, content_type, mtime, size, &still_wanted).await;

    if let Some(texture) = &texture {
        CACHE.with(|c| c.borrow_mut().insert(key.clone(), texture.clone()));
    }
    finish_in_flight(&key, texture.clone());
    texture
}

/// Registers interest in `key`.
///
/// Returns `Some(receiver)` if a request is already running — the caller should
/// await it — or `None` if this caller is now the one responsible for producing
/// the thumbnail.
fn join_in_flight(key: &CacheKey) -> Option<async_channel::Receiver<Option<gdk::Texture>>> {
    IN_FLIGHT.with(|map| {
        let mut map = map.borrow_mut();
        match map.get_mut(key) {
            Some(waiters) => {
                let (tx, rx) = async_channel::bounded(1);
                waiters.push(tx);
                Some(rx)
            }
            None => {
                map.insert(key.clone(), Vec::new());
                None
            }
        }
    })
}

/// Whether anyone else is waiting on this request.
fn has_waiters(key: &CacheKey) -> bool {
    IN_FLIGHT.with(|map| map.borrow().get(key).is_some_and(|w| !w.is_empty()))
}

/// Hands the finished thumbnail to everyone who queued behind it.
fn finish_in_flight(key: &CacheKey, texture: Option<gdk::Texture>) {
    let waiters = IN_FLIGHT.with(|map| map.borrow_mut().remove(key)).unwrap_or_default();
    for waiter in waiters {
        let _ = waiter.try_send(texture.clone());
    }
}

/// Does the actual work: shared cache first, then a rate-limited decode.
async fn produce(
    path: &Path,
    content_type: &str,
    mtime: i64,
    size: i32,
    still_wanted: &impl Fn() -> bool,
) -> Option<gdk::Texture> {
    let uri = gio::File::for_path(path).uri().to_string();
    let digest = md5_hex(&uri);

    // A previous attempt by any thumbnailer failed on this file; retrying would
    // burn the same time to reach the same conclusion.
    if has_failure_marker(&digest)
        || FAILED.with(|f| f.borrow().contains(&(path.to_path_buf(), mtime, size)))
    {
        return None;
    }

    // ── the cheap path: someone already made this thumbnail ────────────────
    if let Some(raw) = load_from_shared_cache(&digest, size, mtime).await {
        crate::trace::THUMB_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return raw.into_texture();
    }

    // ── the expensive path: decode, rate-limited ───────────────────────────
    let (permit_tx, permit_rx) = DECODE_PERMITS.with(|p| p.clone());
    permit_rx.recv().await.ok()?;

    // Between queueing and getting here the row may have been scrolled away and
    // rebound to a different file. Drop the work rather than decode it — unless
    // other rows have queued behind this request, in which case abandoning it
    // would hand them all a `None` and leave them with no thumbnail at all.
    if !still_wanted() && !has_waiters(&(path.to_path_buf(), mtime, size)) {
        crate::trace::THUMB_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = permit_tx.try_send(());
        return None;
    }

    crate::trace::THUMB_DECODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let target = fdo_target_size(size);
    // In-process first where it applies; an external thumbnailer second, both
    // for types gdk-pixbuf never handles and for images whose loader is
    // missing (HEIF and AVIF, commonly), where the decode above just fails.
    let mut result = None;
    if decodes_natively(content_type) {
        result = decode_on_worker(path.to_path_buf(), Some(target)).await;
    }
    if result.is_none()
        && let Some(method) = super::thumbnailers::method_for(content_type)
    {
        result = render_on_worker(method, path.to_path_buf(), target).await;
    }
    let _ = permit_tx.try_send(());

    let Some(raw) = result else {
        FAILED.with(|f| f.borrow_mut().insert((path.to_path_buf(), mtime, size)));
        return None;
    };
    // Hand the result to the rest of the desktop, not just this process.
    store_in_shared_cache(&raw, &digest, &uri, mtime, size);
    raw.into_texture()
}

/// Decoded pixels, detached from `GdkPixbuf` so they can cross a thread.
///
/// `Pixbuf` is `!Send`, so it is created, read and dropped entirely inside the
/// worker; only this plain buffer travels back to the main thread.
pub struct RawThumb {
    width: i32,
    height: i32,
    rowstride: usize,
    has_alpha: bool,
    pixels: Vec<u8>,
    /// `Thumb::MTime` from a shared-cache PNG, used to detect a stale entry.
    recorded_mtime: Option<i64>,
}

impl RawThumb {
    fn from_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf) -> Self {
        Self {
            width: pixbuf.width(),
            height: pixbuf.height(),
            rowstride: pixbuf.rowstride() as usize,
            has_alpha: pixbuf.has_alpha(),
            pixels: pixbuf.read_pixel_bytes().to_vec(),
            recorded_mtime: pixbuf
                .option("tEXt::Thumb::MTime")
                .and_then(|v| v.parse::<i64>().ok()),
        }
    }

    fn into_texture(self) -> Option<gdk::Texture> {
        let format = if self.has_alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        let bytes = glib::Bytes::from_owned(self.pixels);
        Some(
            gdk::MemoryTexture::new(self.width, self.height, format, &bytes, self.rowstride)
                .upcast(),
        )
    }
}

/// Decodes a file on a dedicated thread, scaling to `target` if given.
///
/// gdk-pixbuf's own `*_async` loaders are documented as threaded, but measuring
/// with a main-loop stall detector showed 8 seconds of main-thread blocking
/// across one folder of 4K wallpapers. Owning the thread removes the doubt:
/// nothing here can touch the main context until the pixels are ready.
async fn decode_on_worker(path: PathBuf, target: Option<i32>) -> Option<RawThumb> {
    let (tx, rx) = async_channel::bounded(1);

    std::thread::Builder::new()
        .name("cadrocfile-decode".into())
        .spawn(move || {
            let decoded = match target {
                // For JPEG this hands libjpeg a scale denominator, so it
                // decodes straight to roughly the target size instead of
                // expanding 8 megapixels first.
                Some(size) => gdk_pixbuf::Pixbuf::from_file_at_scale(&path, size, size, true),
                None => gdk_pixbuf::Pixbuf::from_file(&path),
            };
            let raw = decoded.ok().map(upright).as_ref().map(RawThumb::from_pixbuf);
            let _ = tx.send_blocking(raw);
        })
        .ok()?;

    rx.recv().await.ok().flatten()
}

/// Turns a photo the way its camera says it was held.
///
/// Phones store portrait shots as landscape pixels plus an EXIF orientation
/// tag. gdk-pixbuf reports the tag but does not act on it, so without this
/// every portrait photo appears lying on its side.
fn upright(pixbuf: gdk_pixbuf::Pixbuf) -> gdk_pixbuf::Pixbuf {
    pixbuf.apply_embedded_orientation().unwrap_or(pixbuf)
}

/// A large rendering for the preview window.
///
/// Never cached: it is big, and wanted once. Never upscaled either — a 64 px
/// icon blown up to fill the window is blurrier and larger than the icon
/// itself, so an image already within `max` is decoded as it is.
pub async fn render_large(path: PathBuf, content_type: &str, max: i32) -> Option<gdk::Texture> {
    let mut raw = None;
    if decodes_natively(content_type) {
        let (tx, rx) = async_channel::bounded(1);
        let source = path.clone();
        std::thread::Builder::new()
            .name("cadrocfile-preview".into())
            .spawn(move || {
                let fits = gdk_pixbuf::Pixbuf::file_info(&source)
                    .is_some_and(|(_, w, h)| w.max(h) <= max);
                let decoded = if fits {
                    gdk_pixbuf::Pixbuf::from_file(&source)
                } else {
                    gdk_pixbuf::Pixbuf::from_file_at_scale(&source, max, max, true)
                };
                let _ = tx.send_blocking(decoded.ok().map(upright).as_ref().map(RawThumb::from_pixbuf));
            })
            .ok()?;
        raw = rx.recv().await.ok().flatten();
    }
    if raw.is_none()
        && let Some(method) = super::thumbnailers::method_for(content_type)
    {
        raw = render_on_worker(method, path, max).await;
    }
    raw?.into_texture()
}

/// Runs an external thumbnailer on its own thread and decodes what it wrote.
async fn render_on_worker(
    method: super::thumbnailers::Method,
    path: PathBuf,
    target: i32,
) -> Option<RawThumb> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("cadrocfile-thumbnailer".into())
        .spawn(move || {
            let raw = super::thumbnailers::render(&method, &path, target).and_then(|png| {
                // Thumbnailers treat the size as a hint; some return the full
                // frame. Scaling here keeps the cache and the tile honest.
                let decoded = gdk_pixbuf::Pixbuf::from_file_at_scale(&png, target, target, true);
                let _ = std::fs::remove_file(&png);
                decoded.ok().map(upright).as_ref().map(RawThumb::from_pixbuf)
            });
            let _ = tx.send_blocking(raw);
        })
        .ok()?;
    rx.recv().await.ok().flatten()
}

/// Which shared-cache bucket we generate into for a given on-screen size.
fn fdo_target_size(size: i32) -> i32 {
    if size <= 128 { 128 } else { 256 }
}

fn thumbnail_root() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("thumbnails"))
}

/// The spec keys thumbnails by the MD5 of the file's URI.
fn md5_hex(uri: &str) -> String {
    glib::compute_checksum_for_bytes(
        glib::ChecksumType::Md5,
        &glib::Bytes::from(uri.as_bytes()),
    )
    .map(|digest| digest.to_string())
    .unwrap_or_default()
}

/// True when some thumbnailer has recorded that this file cannot be rendered.
fn has_failure_marker(digest: &str) -> bool {
    FAIL_DIRS.with(|cell| {
        let mut cached = cell.borrow_mut();
        let dirs = cached.get_or_insert_with(|| {
            // The spec namespaces failures per application, so collect them all.
            let Some(fail_root) = thumbnail_root().map(|r| r.join("fail")) else {
                return Vec::new();
            };
            std::fs::read_dir(&fail_root)
                .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
                .unwrap_or_default()
        });
        dirs.iter().any(|dir| dir.join(format!("{digest}.png")).exists())
    })
}

/// Reads an existing shared thumbnail, if one is present and still current.
async fn load_from_shared_cache(digest: &str, size: i32, mtime: i64) -> Option<RawThumb> {
    let root = thumbnail_root()?;
    let wanted = fdo_target_size(size);

    // Prefer the smallest bucket that is still at least as large as we need;
    // upscaling a 128px thumbnail into a 256px tile looks visibly soft.
    let order: Vec<&str> = FDO_SIZES
        .iter()
        .filter(|(_, px)| *px >= wanted)
        .chain(FDO_SIZES.iter().rev().filter(|(_, px)| *px < wanted))
        .map(|(dir, _)| *dir)
        .collect();

    for dir in order {
        let candidate = root.join(dir).join(format!("{digest}.png"));
        // A plain `stat` is cheap enough to keep inline; the decode is not.
        if !candidate.is_file() {
            continue;
        }
        let Some(raw) = decode_on_worker(candidate, None).await else {
            continue;
        };
        // The spec stores the source's mtime; a mismatch means the file changed
        // since the thumbnail was made and the thumbnail is a lie.
        if raw.recorded_mtime.is_some_and(|recorded| recorded != mtime) {
            continue;
        }
        return Some(raw);
    }
    None
}

/// Writes a generated thumbnail into the shared cache, per the spec.
///
/// Failures are ignored throughout: a thumbnail that could not be cached is a
/// missed optimisation, never something worth interrupting the user about.
/// Encoding and writing both happen on a worker thread — this is housekeeping
/// for later launches and must never cost the user a frame now.
fn store_in_shared_cache(raw: &RawThumb, digest: &str, uri: &str, mtime: i64, size: i32) {
    let Some(root) = thumbnail_root() else { return };
    let bucket = if fdo_target_size(size) <= 128 { "normal" } else { "large" };
    let dir = root.join(bucket);

    let final_path = dir.join(format!("{digest}.png"));
    let temp_path = dir.join(format!("{digest}.png.{}.tmp", std::process::id()));
    let uri = uri.to_string();

    let (width, height, rowstride, has_alpha) =
        (raw.width, raw.height, raw.rowstride, raw.has_alpha);
    let pixels = raw.pixels.clone();

    std::thread::Builder::new()
        .name("cadrocfile-thumb-store".into())
        .spawn(move || {
            if std::fs::create_dir_all(&dir).is_err() {
                return;
            }
            let bytes = glib::Bytes::from_owned(pixels);
            let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
                &bytes,
                gdk_pixbuf::Colorspace::Rgb,
                has_alpha,
                8,
                width,
                height,
                rowstride as i32,
            );

            // Write to a temporary name and rename into place: a reader must
            // never see a half-written PNG, and several apps may be
            // thumbnailing the same file at once.
            let saved = pixbuf.savev(
                &temp_path,
                "png",
                &[("tEXt::Thumb::URI", &uri), ("tEXt::Thumb::MTime", &mtime.to_string())],
            );
            if saved.is_err() {
                let _ = std::fs::remove_file(&temp_path);
                return;
            }

            // The spec requires 0600: a thumbnail can reveal the contents of a
            // file whose own permissions are stricter than the cache directory.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600));
            if std::fs::rename(&temp_path, &final_path).is_err() {
                let _ = std::fs::remove_file(&temp_path);
            }
        })
        .ok();
}

/// Drops every cached texture, e.g. when the icon size changes wholesale.
pub fn clear() {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        cache.map.clear();
        cache.order.clear();
    });
}

/// The themed icon for an entry, used whenever there is no thumbnail.
pub fn icon_for(content_type: &str, is_dir: bool, is_symlink: bool) -> gio::Icon {
    if is_dir {
        let name = if is_symlink { "folder-symbolic" } else { "folder" };
        return gio::ThemedIcon::new(name).upcast();
    }
    gio::functions::content_type_get_icon(content_type)
}

use gtk::{gio, glib};
