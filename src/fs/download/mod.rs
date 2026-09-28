//! Downloading a file from a URL.
//!
//! A browser fetches a file over one connection. Most of the time the limit is
//! not your line but the server's per-connection shaping, so asking for several
//! byte ranges at once and writing them into one preallocated file is
//! materially faster on exactly the large files worth downloading from a file
//! manager. When the server won't play along — no ranges, no length, or a size
//! too small to be worth splitting — it falls back to one stream and nothing is
//! lost.
//!
//! The harder half of the problem is not speed but *honesty*. A download that
//! writes three kilobytes of error page under the name you asked for has
//! failed, however cheerfully it reported success. Everything below is arranged
//! so that cannot happen: the URL is resolved to where the bytes actually live
//! ([`resolve`]), the response is checked for being a web page rather than a
//! file, every range is required to deliver the bytes it promised, and the
//! finished file is measured against the length the server declared.

mod resolve;

use std::{
    fs,
    io::Read,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    },
    time::Duration,
};

use ureq::ResponseExt;

use super::{
    ops::{ITEM_INTERVAL_MS, JobHandle, JobKind, JobOutcome, Progress},
    parallel,
};

/// Read buffer per connection. Large enough that a fast link isn't syscall
/// bound, small enough that eight of them are not worth worrying about.
const CHUNK: usize = 256 * 1024;

/// Don't split anything smaller than this. Below it the extra requests cost
/// more in round-trips than the parallelism returns.
const MIN_PARALLEL_BYTES: u64 = 4 * 1024 * 1024;

/// Timeouts are per phase, never global.
///
/// A single global timeout is the obvious thing to reach for and is wrong here:
/// it bounds the whole transfer, so any download slower than the limit fails —
/// which is precisely the large files this feature exists for. A 140 MB fetch
/// died at 30 s under exactly that mistake. These instead bound the parts that
/// can hang without making progress, and leave the body free to take as long as
/// the file needs.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Sent as `User-Agent`.
///
/// Identifying as `cadrocfile/x.y` is the honest thing to do and it does not work:
/// a large share of file hosts and CDNs answer an unrecognised agent with a
/// challenge page or a 403, so the honest string produces a broken feature and
/// a confusing error. This asks for the file the same way the browser the user
/// copied the link from would.
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// What a probe of the URL tells us before committing to a download.
#[derive(Debug, Clone)]
pub struct Probe {
    /// Where the bytes are: host rewrites and redirects already applied, so
    /// every connection can go straight there.
    pub url: String,
    /// What the user pasted, kept so the UI can say what it was changed to.
    pub original_url: String,
    /// Total size, when the server declares one.
    pub size: Option<u64>,
    /// Whether byte ranges are accepted, which is what makes splitting possible.
    pub supports_ranges: bool,
    /// The name to save as, from `Content-Disposition`, the host's API, or the
    /// URL path.
    pub filename: String,
    /// Headers some hosts require before they will serve the file.
    referer: Option<String>,
    cookie: Option<String>,
}

impl Probe {
    /// True when the address was rewritten to reach the actual file, which is
    /// worth telling the user so a wrong guess is visible rather than silent.
    pub fn was_redirected(&self) -> bool {
        self.url != self.original_url
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        // Statuses are inspected here rather than raised as errors, because a
        // 403 with an HTML body and a 206 with a partial one need very
        // different handling and both are "not 200".
        .http_status_as_error(false)
        .save_redirect_history(true)
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// Asks the server what it is offering, without downloading the body.
///
/// Used to fill in the size and the suggested filename before the user commits,
/// so the name can be changed with knowledge of what is actually there.
pub fn probe(url: &str) -> Result<Probe, String> {
    let original = normalise_url(url)?;
    let agent = agent();
    let resolved = resolve::resolve(&agent, &original)?;
    let mut probe = inspect(&agent, &resolved)?;
    probe.original_url = original;
    Ok(probe)
}

/// One ranged GET that answers every question at once.
///
/// `HEAD` is the textbook probe and is the wrong tool: plenty of storage nodes
/// reject it outright — gofile's answer 400 — and `Accept-Ranges` is frequently
/// absent from servers that honour ranges perfectly well. Asking for the first
/// byte instead settles size, range support and the post-redirect URL in a
/// single round trip, and the reply that comes back is the same reply the real
/// download will get, so a challenge page is caught here rather than on disk.
fn inspect(agent: &ureq::Agent, resolved: &resolve::Resolved) -> Result<Probe, String> {
    let mut request = agent
        .get(&resolved.url)
        .header("Range", "bytes=0-0")
        // Ranges are byte offsets into the *stored* file. A server that gzips
        // the response makes the offsets meaningless and the declared length
        // wrong, so compression is declined for the whole download.
        .header("Accept-Encoding", "identity");
    if let Some(referer) = &resolved.referer {
        request = request.header("Referer", referer);
    }
    if let Some(cookie) = &resolved.cookie {
        request = request.header("Cookie", cookie);
    }

    let response = request
        .call()
        .map_err(|e| transport_error(&resolved.url, &e.to_string()))?;

    let status = response.status().as_u16();
    let final_url = response.get_uri().to_string();
    let headers = response.headers();
    let header = |name: &str| {
        headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    };
    let content_type = header("content-type").unwrap_or_default();
    let disposition = header("content-disposition");
    let content_range = header("content-range");
    let content_length = header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
    let accept_ranges = header("accept-ranges")
        .map(|v| v.to_lowercase().contains("bytes"))
        .unwrap_or(false);

    if let Some(problem) = status_problem(status) {
        return Err(problem);
    }

    // 206 proves ranges work and carries the true total; 200 means the server
    // sent the whole thing and ranges are not on offer for this URL.
    let (size, supports_ranges) = match (status, content_range.as_deref()) {
        (206, Some(range)) => (total_from_content_range(range), true),
        (206, None) => (None, true),
        _ => (content_length, accept_ranges),
    };

    let filename = disposition
        .as_deref()
        .and_then(filename_from_disposition)
        .or_else(|| resolved.filename.clone())
        .or_else(|| filename_from_url(&final_url))
        .or_else(|| filename_from_url(&resolved.url))
        .unwrap_or_else(|| "download".to_string());
    let filename = sanitize_filename(&filename);

    if let Some(problem) = web_page_problem(&content_type, &filename, &resolved.url, &final_url) {
        return Err(problem);
    }

    Ok(Probe {
        url: final_url,
        original_url: resolved.url.clone(),
        size,
        supports_ranges,
        filename,
        referer: resolved.referer.clone(),
        cookie: resolved.cookie.clone(),
    })
}

/// `Content-Range: bytes 0-0/12345` — the part after the slash is the total,
/// or `*` when the server declines to say.
fn total_from_content_range(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse::<u64>().ok()
}

/// Turns a refusal into a sentence, or `None` when the status is usable.
fn status_problem(status: u16) -> Option<String> {
    match status {
        200..=299 => None,
        // A redirect reaching here means the chain was longer than the agent
        // would follow, which in practice is a login wall or a loop.
        300..=399 => Some("That address keeps redirecting without ever reaching a file.".into()),
        401 => Some("That file needs a sign-in, which a download link cannot provide.".into()),
        403 => Some(
            "The server refused the request (403). Links from file-sharing sites often \
             expire, or only work from the browser session that created them."
                .into(),
        ),
        404 => Some("That address does not exist on the server (404).".into()),
        410 => Some("That file has been removed from the server (410).".into()),
        429 => Some("The server is rate-limiting downloads. Try again in a few minutes.".into()),
        500..=599 => Some(format!("The server is having problems ({status}).")),
        other => Some(format!("The server answered {other}, which is not a file.")),
    }
}

/// Catches the failure that made this rewrite necessary.
///
/// A landing page, a cookie wall and an ISP block page are all "HTTP 200 with a
/// few kilobytes of HTML". Writing that to `something.zip` is the single worst
/// thing this code can do, because it looks like success. If the reply is a web
/// page and the user did not ask for a web page, it is not the download.
fn web_page_problem(
    content_type: &str,
    filename: &str,
    requested_url: &str,
    final_url: &str,
) -> Option<String> {
    let kind = content_type.split(';').next().unwrap_or("").trim().to_lowercase();
    let is_page = kind == "text/html" || kind == "application/xhtml+xml";
    if !is_page {
        return None;
    }
    let lower = filename.to_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        return None;
    }

    let requested_host = host_of(requested_url);
    let final_host = host_of(final_url);
    if let (Some(from), Some(to)) = (&requested_host, &final_host)
        && from != to
    {
        return Some(format!(
            "{from} sent us to {to}, which returned a web page instead of a file. \
             That usually means the network or ISP is intercepting the address, \
             or the link needs a browser sign-in."
        ));
    }

    Some(
        "That address returns a web page, not a file. Open it in a browser and copy \
         the link the download button itself points at."
            .to_string(),
    )
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Adds a scheme when the user pasted a bare host, and rejects anything that
/// isn't http(s) — `file://` here would be a confusing way to copy a file.
fn normalise_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a URL to download".into());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let lower = with_scheme.to_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("Only http and https addresses can be downloaded".into());
    }
    Ok(with_scheme)
}

/// Pulls a filename out of a `Content-Disposition` header.
///
/// Handles the `filename*=UTF-8''…` form as well as plain `filename=`, because
/// the extended form is what anything serving non-ASCII names actually sends
/// and it takes precedence when both are present.
fn filename_from_disposition(header: &str) -> Option<String> {
    if let Some(start) = header.to_lowercase().find("filename*=") {
        let value = header[start + "filename*=".len()..].trim();
        let value = value.split(';').next()?.trim().trim_matches('"');
        // RFC 5987: charset'language'percent-encoded-value
        let encoded = value.rsplit('\'').next()?;
        let decoded = urlencoding::decode(encoded).ok()?.into_owned();
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }
    let start = header.to_lowercase().find("filename=")?;
    let value = header[start + "filename=".len()..].trim();
    let value = value.split(';').next()?.trim().trim_matches('"');
    (!value.is_empty()).then(|| value.to_string())
}

/// The last path segment, when it looks like a filename.
///
/// The authority is dropped first: `https://x.dev/` has no path, and taking the
/// last non-empty piece of the whole string suggests saving the download as
/// `x.dev`. A segment with no extension is rejected for the same reason — it is
/// an API route, not a name anybody wants on disk.
fn filename_from_url(url: &str) -> Option<String> {
    let path = url.split("://").nth(1)?.split_once('/')?.1;
    let without_query = path.split(['?', '#']).next()?;
    let last = without_query.rsplit('/').find(|s| !s.is_empty())?;
    let decoded = urlencoding::decode(last).ok()?.into_owned();
    (!decoded.is_empty() && decoded.contains('.')).then_some(decoded)
}

/// Reduces a server-supplied name to a single safe path component.
///
/// The name comes from a remote server, so it is treated the same way an
/// archive entry is: no directory separators, no `..`, nothing that could place
/// the file anywhere but where the user chose.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .replace(['/', '\\'], "_")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() { "download".to_string() } else { trimmed.to_string() }
}

/// How the work will be split, decided once so the UI can explain it.
fn connections_for(probe: &Probe) -> usize {
    let Some(size) = probe.size else { return 1 };
    if !probe.supports_ranges || size < MIN_PARALLEL_BYTES {
        return 1;
    }
    // Never more connections than there are whole megabytes to fetch, and never
    // more than the shared worker budget allows.
    let by_size = (size / (MIN_PARALLEL_BYTES / 4)).max(1) as usize;
    parallel::threads().min(by_size).max(1)
}

/// Spawns a download job writing to `dest`.
pub fn start(probe: Probe, dest: PathBuf) -> JobHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    std::thread::Builder::new()
        .name("cadrocfile-download".into())
        .spawn(move || {
            let total = probe.size.unwrap_or(0);
            let _ = tx.send_blocking(Progress::Prepared { total_bytes: total, total_items: 1 });
            let name = dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let _ = tx.send_blocking(Progress::Item { name, done_items: 0 });

            let done = Arc::new(AtomicU64::new(0));
            let result = run(&probe, &dest, &worker_cancel, &tx, &done);

            let cancelled = worker_cancel.load(AtomicOrdering::Relaxed);
            let mut errors = Vec::new();
            let mut created = Vec::new();

            match result {
                Ok(()) if !cancelled => created.push(dest.clone()),
                Ok(()) => {
                    // A cancelled download leaves a partial file that looks
                    // complete in the listing; remove it.
                    let _ = fs::remove_file(&dest);
                }
                Err(e) => {
                    let _ = fs::remove_file(&dest);
                    errors.push((dest.clone(), e));
                }
            }

            let _ = tx.send_blocking(Progress::Finished(JobOutcome {
                cancelled,
                items_done: u64::from(errors.is_empty() && !cancelled),
                bytes_done: done.load(AtomicOrdering::Relaxed),
                errors,
                created,
            }));
        })
        .expect("spawn download thread");

    JobHandle::new(JobKind::Download, rx, cancel)
}

fn run(
    probe: &Probe,
    dest: &Path,
    cancel: &AtomicBool,
    tx: &async_channel::Sender<Progress>,
    done: &Arc<AtomicU64>,
) -> Result<(), String> {
    let connections = connections_for(probe);
    let reporter = Reporter::new(tx.clone(), Arc::clone(done));

    let written = if connections <= 1 {
        single(probe, dest, cancel, &reporter)?
    } else {
        let size = probe.size.expect("connections_for only splits with a known size");
        match parallel_ranges(probe, dest, size, connections, cancel, &reporter) {
            Ok(written) => written,
            // The probe said ranges were fine and the download disagreed. One
            // stream still works, and retrying is far better than handing back
            // a failure the user can do nothing about.
            Err(RangeFailure::NotSupported) => {
                done.store(0, AtomicOrdering::Relaxed);
                single(probe, dest, cancel, &reporter)?
            }
            Err(RangeFailure::Fatal(e)) => return Err(e),
        }
    };

    if cancel.load(AtomicOrdering::Relaxed) {
        return Ok(());
    }
    verify_length(probe, written)
}

/// The check that turns a silent short download into a reported failure.
///
/// A truncated transfer that ends without an error is indistinguishable from a
/// complete one until something tries to open the file — which is how a 3 KB
/// "zip" gets saved and trusted. If the server said how many bytes it had, that
/// is exactly how many must have arrived.
fn verify_length(probe: &Probe, written: u64) -> Result<(), String> {
    let Some(expected) = probe.size else {
        // Nothing was promised, so nothing can be checked — but an empty file
        // is never a successful download.
        return if written == 0 {
            Err("The server sent no data.".to_string())
        } else {
            Ok(())
        };
    };
    if written == expected {
        return Ok(());
    }
    Err(format!(
        "The download stopped early — {} of {} arrived.",
        humansize::format_size(written, humansize::DECIMAL),
        humansize::format_size(expected, humansize::DECIMAL),
    ))
}

/// Applies the headers the host needs to any request for this download.
fn get(probe: &Probe) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let mut request = agent().get(&probe.url).header("Accept-Encoding", "identity");
    if let Some(referer) = &probe.referer {
        request = request.header("Referer", referer);
    }
    if let Some(cookie) = &probe.cookie {
        request = request.header("Cookie", cookie);
    }
    request
}

/// One connection, streamed straight to disk. Returns the bytes written.
fn single(
    probe: &Probe,
    dest: &Path,
    cancel: &AtomicBool,
    reporter: &Reporter,
) -> Result<u64, String> {
    let response = get(probe)
        .call()
        .map_err(|e| transport_error(&probe.url, &e.to_string()))?;

    if let Some(problem) = status_problem(response.status().as_u16()) {
        return Err(problem);
    }
    if let Some(kind) = response.headers().get("content-type").and_then(|v| v.to_str().ok())
        && let Some(problem) =
            web_page_problem(kind, &probe.filename, &probe.url, &response.get_uri().to_string())
    {
        return Err(problem);
    }

    let file = fs::File::create(dest).map_err(|e| format!("Cannot write {}: {e}", dest.display()))?;
    if let Some(size) = probe.size {
        let _ = file.set_len(size);
    }

    let mut body = response.into_body().into_reader();
    let mut buffer = vec![0u8; CHUNK];
    let mut offset = 0u64;

    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Ok(offset);
        }
        let read = body.read(&mut buffer).map_err(|e| format!("Download interrupted: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all_at(&buffer[..read], offset)
            .map_err(|e| format!("Cannot write {}: {e}", dest.display()))?;
        offset += read as u64;
        reporter.add(read as u64);
    }

    // The server may have declared more than it sent; trim rather than leave a
    // file padded with the zeros `set_len` created. Whether that shortfall is
    // acceptable is `verify_length`'s decision, not ours.
    let _ = file.set_len(offset);
    Ok(offset)
}

/// Why a split download stopped, separating "try another way" from "give up".
enum RangeFailure {
    /// The server does not really honour ranges, whatever the probe suggested.
    NotSupported,
    Fatal(String),
}

/// Several connections, each fetching its own byte range into one file.
fn parallel_ranges(
    probe: &Probe,
    dest: &Path,
    size: u64,
    connections: usize,
    cancel: &AtomicBool,
    reporter: &Reporter,
) -> Result<u64, RangeFailure> {
    let file = fs::File::create(dest)
        .map_err(|e| RangeFailure::Fatal(format!("Cannot write {}: {e}", dest.display())))?;
    // Preallocate so the ranges can be written in any order and the filesystem
    // can pick one extent instead of growing the file under eight writers.
    file.set_len(size)
        .map_err(|e| RangeFailure::Fatal(format!("Cannot size {}: {e}", dest.display())))?;
    let file = Arc::new(file);

    let ranges = tile(size, connections);
    let failure: std::sync::Mutex<Option<RangeFailure>> = std::sync::Mutex::new(None);

    parallel::for_each(&ranges, |&(start, end)| {
        if cancel.load(AtomicOrdering::Relaxed) || failure.lock().is_ok_and(|f| f.is_some()) {
            return;
        }
        if let Err(e) = fetch_range(probe, &file, start, end, cancel, reporter)
            && let Ok(mut slot) = failure.lock()
        {
            slot.get_or_insert(e);
        }
    });

    match failure.into_inner() {
        Ok(Some(e)) => Err(e),
        _ => Ok(size),
    }
}

/// Divides a file into one contiguous range per connection.
fn tile(size: u64, connections: usize) -> Vec<(u64, u64)> {
    let span = size.div_ceil(connections as u64);
    (0..connections as u64)
        .map(|i| (i * span, ((i + 1) * span).min(size).saturating_sub(1)))
        .filter(|(start, end)| start <= end)
        .collect()
}

fn fetch_range(
    probe: &Probe,
    file: &Arc<fs::File>,
    start: u64,
    end: u64,
    cancel: &AtomicBool,
    reporter: &Reporter,
) -> Result<(), RangeFailure> {
    let response = get(probe)
        .header("Range", format!("bytes={start}-{end}"))
        .call()
        .map_err(|e| RangeFailure::Fatal(transport_error(&probe.url, &e.to_string())))?;

    let status = response.status().as_u16();
    // A server that ignores Range answers 200 with the whole body. Writing that
    // into one slice would corrupt the file, so the whole download restarts as
    // a single stream instead.
    if status == 200 {
        return Err(RangeFailure::NotSupported);
    }
    if status != 206 {
        return Err(match status_problem(status) {
            Some(problem) => RangeFailure::Fatal(problem),
            None => RangeFailure::NotSupported,
        });
    }

    let mut body = response.into_body().into_reader();
    let mut buffer = vec![0u8; CHUNK];
    let mut offset = start;

    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Ok(());
        }
        let read = body
            .read(&mut buffer)
            .map_err(|e| RangeFailure::Fatal(format!("Download interrupted: {e}")))?;
        if read == 0 {
            break;
        }
        // Never write past the range this connection owns, however much the
        // server decides to send.
        let allowed = (end + 1).saturating_sub(offset).min(read as u64) as usize;
        if allowed == 0 {
            break;
        }
        file.write_all_at(&buffer[..allowed], offset)
            .map_err(|e| RangeFailure::Fatal(format!("Cannot write to the file: {e}")))?;
        offset += allowed as u64;
        reporter.add(allowed as u64);
    }

    // A range that ends early leaves a hole of zeros in the middle of the file,
    // and nothing downstream would ever notice. Every connection must deliver
    // the bytes it was assigned.
    if !cancel.load(AtomicOrdering::Relaxed) && offset <= end {
        return Err(RangeFailure::Fatal(
            "The server closed one of the connections before it finished.".to_string(),
        ));
    }
    Ok(())
}

/// Totals bytes across connections and throttles what reaches the UI.
struct Reporter {
    tx: async_channel::Sender<Progress>,
    done: Arc<AtomicU64>,
    started: std::time::Instant,
    last_ms: AtomicU64,
}

impl Reporter {
    fn new(tx: async_channel::Sender<Progress>, done: Arc<AtomicU64>) -> Self {
        Self { tx, done, started: std::time::Instant::now(), last_ms: AtomicU64::new(0) }
    }

    fn add(&self, n: u64) {
        let total = self.done.fetch_add(n, AtomicOrdering::Relaxed) + n;
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_ms.load(AtomicOrdering::Relaxed);
        if u128::from(now.saturating_sub(last)) >= ITEM_INTERVAL_MS
            && self
                .last_ms
                .compare_exchange(last, now, AtomicOrdering::Relaxed, AtomicOrdering::Relaxed)
                .is_ok()
        {
            let _ = self.tx.send_blocking(Progress::Bytes { done_bytes: total });
        }
    }
}

/// Turns a transport error into something worth showing a person.
fn transport_error(url: &str, raw: &str) -> String {
    let lower = raw.to_lowercase();
    let host = host_of(url).unwrap_or_else(|| "that server".to_string());

    if lower.contains("dns") || lower.contains("resolve") || lower.contains("name or service") {
        return format!(
            "{host} could not be looked up. If the site works in a browser on another \
             network, your ISP or DNS provider is probably blocking it."
        );
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return format!(
            "{host} accepted the address but never answered. A blocked or filtered \
             connection usually looks exactly like this."
        );
    }
    if lower.contains("connection refused") || lower.contains("connect") {
        return format!("Could not open a connection to {host}.");
    }
    if lower.contains("certificate") || lower.contains("tls") || lower.contains("invalid peer") {
        return format!(
            "{host} presented a security certificate that could not be verified, which \
             can mean the connection is being intercepted."
        );
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_for(size: Option<u64>, supports_ranges: bool) -> Probe {
        Probe {
            url: "https://x.dev/f".into(),
            original_url: "https://x.dev/f".into(),
            size,
            supports_ranges,
            filename: "f.zip".into(),
            referer: None,
            cookie: None,
        }
    }

    #[test]
    fn a_bare_host_gets_https_and_other_schemes_are_refused() {
        assert_eq!(normalise_url("example.com/f.zip").unwrap(), "https://example.com/f.zip");
        assert_eq!(normalise_url("http://x.dev/a").unwrap(), "http://x.dev/a");
        assert!(normalise_url("ftp://x.dev/a").is_err());
        assert!(normalise_url("file:///etc/passwd").is_err());
        assert!(normalise_url("   ").is_err());
    }

    #[test]
    fn filenames_come_from_the_disposition_header_when_present() {
        assert_eq!(
            filename_from_disposition("attachment; filename=\"report.pdf\"").as_deref(),
            Some("report.pdf")
        );
        // The extended form wins, and is percent-decoded.
        assert_eq!(
            filename_from_disposition(
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''caf%C3%A9%20menu.pdf"
            )
            .as_deref(),
            Some("café menu.pdf")
        );
        assert_eq!(filename_from_disposition("inline").as_deref(), None);
    }

    #[test]
    fn filenames_fall_back_to_the_url_path() {
        assert_eq!(filename_from_url("https://x.dev/a/b/linux.tar.xz").as_deref(), Some("linux.tar.xz"));
        assert_eq!(filename_from_url("https://x.dev/a/file%20name.zip").as_deref(), Some("file name.zip"));
        assert_eq!(filename_from_url("https://x.dev/f.bin?token=1#frag").as_deref(), Some("f.bin"));
        // A bare host or an extensionless path segment is not a filename; the
        // old code cheerfully suggested saving a download as "x.dev".
        assert_eq!(filename_from_url("https://x.dev/"), None);
        assert_eq!(filename_from_url("https://x.dev/api/file/abc123"), None);
    }

    /// The name is chosen by a remote server, so it must not be able to escape
    /// the folder the user picked.
    #[test]
    fn a_server_cannot_choose_a_path() {
        // The property that matters is that whatever comes back is one inert
        // path component, not any particular spelling of it.
        for hostile in [
            "../../etc/passwd",
            "/etc/shadow",
            "..",
            "...",
            "",
            "  ",
            "with\nnewline.txt",
            "C:\\Windows\\system32",
            "a/b/c",
        ] {
            let safe = sanitize_filename(hostile);
            assert!(!safe.is_empty(), "{hostile:?} produced an empty name");
            assert!(!safe.contains('/'), "{hostile:?} kept a separator: {safe:?}");
            assert!(!safe.contains('\\'), "{hostile:?} kept a separator: {safe:?}");
            assert!(!safe.starts_with('.'), "{hostile:?} stayed hidden or relative: {safe:?}");
            assert!(!safe.chars().any(char::is_control), "{hostile:?} kept a control char");
            assert_eq!(
                Path::new(&safe).components().count(),
                1,
                "{hostile:?} produced more than one component: {safe:?}"
            );
        }

        // Ordinary names survive untouched.
        assert_eq!(sanitize_filename("  spaced.txt  "), "spaced.txt");
        assert_eq!(sanitize_filename("linux-6.9.tar.xz"), "linux-6.9.tar.xz");
    }

    #[test]
    fn splitting_only_happens_when_it_can_help() {
        assert!(
            connections_for(&probe_for(Some(500 * 1024 * 1024), true)) > 1,
            "a large ranged file should split"
        );
        assert_eq!(connections_for(&probe_for(Some(500 * 1024 * 1024), false)), 1);
        assert_eq!(connections_for(&probe_for(None, true)), 1, "cannot split without a length");
        assert_eq!(connections_for(&probe_for(Some(100 * 1024), true)), 1, "too small to split");
    }

    /// Every byte of the file has to be owned by exactly one connection.
    #[test]
    fn ranges_tile_the_file_exactly_once() {
        for size in [1u64, 5, 1000, 1_048_576, 999_983] {
            for connections in 1..=8usize {
                let ranges = tile(size, connections);
                let covered: u64 = ranges.iter().map(|(s, e)| e - s + 1).sum();
                assert_eq!(covered, size, "size {size} over {connections} connections");
                for pair in ranges.windows(2) {
                    assert_eq!(pair[0].1 + 1, pair[1].0, "gap or overlap in {ranges:?}");
                }
                assert!(ranges.iter().all(|&(_, e)| e < size), "range past the end");
            }
        }
    }

    #[test]
    fn the_total_size_is_read_from_a_content_range_header() {
        assert_eq!(total_from_content_range("bytes 0-0/12345"), Some(12345));
        assert_eq!(total_from_content_range("bytes 0-0/*"), None, "an unknown total is not a size");
        assert_eq!(total_from_content_range("nonsense"), None);
    }

    /// The bug this rewrite exists for: three kilobytes of HTML saved as a zip.
    #[test]
    fn a_web_page_is_never_mistaken_for_a_download() {
        let problem = web_page_problem("text/html; charset=utf-8", "archive.zip", "https://x.dev/a", "https://x.dev/a");
        assert!(problem.is_some(), "an HTML body under a .zip name must be refused");

        // Being sent somewhere else entirely is worth naming, because that is
        // what an ISP block or a captive portal looks like.
        let intercepted = web_page_problem(
            "text/html",
            "archive.zip",
            "https://pixeldrain.com/api/file/a",
            "https://blockpage.isp.net/notice",
        )
        .expect("a cross-host HTML reply must be refused");
        assert!(intercepted.contains("blockpage.isp.net"), "{intercepted}");
        assert!(intercepted.contains("pixeldrain.com"), "{intercepted}");

        // Real files pass, and so does an HTML file that was actually asked for.
        assert!(web_page_problem("application/zip", "archive.zip", "https://x.dev/a", "https://x.dev/a").is_none());
        assert!(web_page_problem("text/html", "page.html", "https://x.dev/a", "https://x.dev/a").is_none());
    }

    /// The other half of that bug: a transfer that stops early and says nothing.
    #[test]
    fn a_short_download_is_reported_rather_than_kept() {
        let known = probe_for(Some(140 * 1024 * 1024), true);
        let short = verify_length(&known, 3_440).expect_err("3 KB of a 140 MB file is a failure");
        assert!(short.contains("stopped early"), "{short}");
        assert!(verify_length(&known, 140 * 1024 * 1024).is_ok());

        // With no declared length there is nothing to compare against, but an
        // empty file is still not a download.
        let unknown = probe_for(None, false);
        assert!(verify_length(&unknown, 0).is_err());
        assert!(verify_length(&unknown, 1).is_ok());
    }

    #[test]
    fn refusals_are_explained_rather_than_shown_as_numbers() {
        assert!(status_problem(200).is_none());
        assert!(status_problem(206).is_none());
        for status in [401, 403, 404, 410, 429, 500, 503] {
            assert!(status_problem(status).is_some(), "{status} should be reported");
        }
    }
}
