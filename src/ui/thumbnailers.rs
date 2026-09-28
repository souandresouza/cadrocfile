//! Thumbnails for things gdk-pixbuf cannot decode: video, PDF, office
//! documents, and image formats whose loaders are not installed.
//!
//! The freedesktop answer is the *external thumbnailer*: a program registered
//! by a `.thumbnailer` file under `$XDG_DATA_DIRS/thumbnailers`, naming the MIME
//! types it handles and a command line. Nautilus, Thunar and Nemo all run the
//! same ones, so honouring them means Cadrocfile thumbnails whatever the rest of
//! the desktop does, including types nobody here has heard of.
//!
//! What a distribution registers varies wildly, though. A machine can have
//! `ffmpeg` and `pdftoppm` installed and no thumbnailer wired to either — this
//! one does — so for video and PDF there are built-in fallbacks that call
//! those tools directly. A registered thumbnailer always wins when present.
//!
//! Everything here runs out of process on a worker thread, under the same
//! concurrency limit as image decoding, with a hard timeout: a thumbnailer
//! handed a truncated video can hang, and one hung child must not pin a slot
//! for the rest of the session.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

/// Longest a thumbnailer may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(15);

/// How a given content type gets turned into a PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// A registered `.thumbnailer` command line, still containing its `%`
    /// placeholders.
    Registered(Vec<String>),
    /// First video frame via `ffmpeg`.
    Ffmpeg,
    /// First PDF page via `pdftoppm`.
    Pdftoppm,
}

/// Every registered thumbnailer, keyed by the MIME type it declares.
fn registry() -> &'static HashMap<String, Vec<String>> {
    static REGISTRY: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut map = HashMap::new();
        // Later directories are lower priority per the XDG spec, so the first
        // registration for a type wins and user entries come first.
        for dir in search_dirs() {
            let Ok(entries) = std::fs::read_dir(dir.join("thumbnailers")) else { continue };
            let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            // Directory order is arbitrary; sorting makes the winner stable.
            files.sort();
            for file in files {
                if file.extension().is_some_and(|ext| ext == "thumbnailer")
                    && let Ok(text) = std::fs::read_to_string(&file)
                    && let Some((types, exec)) = parse(&text)
                {
                    for mime in types {
                        map.entry(mime).or_insert_with(|| exec.clone());
                    }
                }
            }
        }
        map
    })
}

/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`, in priority order.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::data_dir() {
        dirs.push(home);
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(system.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    dirs
}

/// Reads a `.thumbnailer` file into its MIME types and tokenised command.
///
/// Returns `None` for a thumbnailer whose `TryExec` program is missing — the
/// file is left behind when the package that provided the program is removed,
/// and running it would just fail for every file.
fn parse(text: &str) -> Option<(Vec<String>, Vec<String>)> {
    let mut in_entry = false;
    let (mut exec, mut try_exec, mut types) = (None, None, Vec::new());
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Thumbnailer Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(value) = line.strip_prefix("Exec=") {
            exec = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("TryExec=") {
            try_exec = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("MimeType=") {
            types = value.split(';').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect();
        }
    }

    let exec = shell_words::split(&exec?).ok()?;
    let program = try_exec.unwrap_or_else(|| exec.first().cloned().unwrap_or_default());
    if types.is_empty() || exec.is_empty() || !program_exists(&program) {
        return None;
    }
    Some((types, exec))
}

fn program_exists(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    which(program).is_some()
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| candidate.is_file())
}

/// How `content_type` can be thumbnailed out of process, if at all.
///
/// Registered thumbnailers are matched exactly first, then through MIME
/// inheritance: `video/x-matroska` is a `video/*`, and a thumbnailer that lists
/// only the parent type still handles the child. Answers are cached per type,
/// because this runs every time a row is bound.
pub fn method_for(content_type: &str) -> Option<Method> {
    thread_local! {
        static ANSWERS: std::cell::RefCell<HashMap<String, Option<Method>>> =
            std::cell::RefCell::new(HashMap::new());
    }
    if let Some(answer) = ANSWERS.with(|a| a.borrow().get(content_type).cloned()) {
        return answer;
    }
    let answer = resolve(content_type);
    ANSWERS.with(|a| a.borrow_mut().insert(content_type.to_string(), answer.clone()));
    answer
}

fn resolve(content_type: &str) -> Option<Method> {
    let registry = registry();
    if let Some(exec) = registry.get(content_type) {
        return Some(Method::Registered(exec.clone()));
    }
    // Sorted so that when two registered parents both match, the choice does
    // not depend on hash order.
    let mut parents: Vec<&String> = registry.keys().collect();
    parents.sort();
    for parent in parents {
        if gio::functions::content_type_is_a(content_type, parent) {
            return Some(Method::Registered(registry[parent].clone()));
        }
    }
    builtin(content_type)
}

/// The fallbacks for when nothing is registered.
fn builtin(content_type: &str) -> Option<Method> {
    let is_video = content_type.starts_with("video/")
        || gio::functions::content_type_is_a(content_type, "video/*");
    if is_video && which("ffmpeg").is_some() {
        return Some(Method::Ffmpeg);
    }
    if content_type == "application/pdf" && which("pdftoppm").is_some() {
        return Some(Method::Pdftoppm);
    }
    None
}

/// Produces a PNG for `input` at roughly `size` pixels, returning its path.
///
/// Blocking: call from a worker thread. The caller owns the returned file and
/// deletes it once decoded.
pub fn render(method: &Method, input: &Path, size: i32) -> Option<PathBuf> {
    let output = scratch_path();
    let ok = match method {
        Method::Registered(exec) => {
            let args = substitute(exec, input, &output, size)?;
            run(Command::new(&args[0]).args(&args[1..]))
        }
        Method::Ffmpeg => {
            // Three seconds in skips the black lead-in most videos open with.
            // A clip shorter than that produces nothing, so retry from zero.
            ffmpeg_frame(input, &output, size, "3") || ffmpeg_frame(input, &output, size, "0")
        }
        Method::Pdftoppm => {
            // pdftoppm appends `.png` to the prefix it is given.
            let prefix = output.with_extension("");
            let ok = run(Command::new("pdftoppm")
                .args(["-png", "-singlefile", "-f", "1", "-l", "1", "-scale-to"])
                .arg(size.to_string())
                .arg(input)
                .arg(&prefix));
            ok && prefix.with_extension("png") == output
        }
    };

    let produced = ok && output.metadata().is_ok_and(|m| m.len() > 0);
    if produced {
        Some(output)
    } else {
        let _ = std::fs::remove_file(&output);
        None
    }
}

fn ffmpeg_frame(input: &Path, output: &Path, size: i32, seek: &str) -> bool {
    // `file:` stops ffmpeg reading a colon in the name as a protocol — a video
    // called `clip:final.mp4` is otherwise an attempt to open protocol `clip`.
    let mut source = std::ffi::OsString::from("file:");
    source.push(input.as_os_str());
    let scale = format!("scale={size}:{size}:force_original_aspect_ratio=decrease");
    let ok = run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-ss", seek, "-i"])
        .arg(&source)
        .args(["-frames:v", "1", "-vf", &scale, "-y"])
        .arg(output));
    ok && output.metadata().is_ok_and(|m| m.len() > 0)
}

/// Fills a registered command line's placeholders, per the thumbnailer spec.
///
/// Returns `None` for a command with no output placeholder: it cannot tell us
/// where it wrote, so there is nothing to read back.
fn substitute(exec: &[String], input: &Path, output: &Path, size: i32) -> Option<Vec<String>> {
    let uri = gio::File::for_path(input).uri().to_string();
    let mut saw_output = false;
    let args = exec
        .iter()
        .map(|arg| {
            let mut out = String::with_capacity(arg.len());
            let mut chars = arg.chars();
            while let Some(c) = chars.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('i') => out.push_str(&input.to_string_lossy()),
                    Some('u') => out.push_str(&uri),
                    Some('o') => {
                        saw_output = true;
                        out.push_str(&output.to_string_lossy());
                    }
                    Some('s') => out.push_str(&size.to_string()),
                    Some('%') => out.push('%'),
                    // An unknown placeholder expands to nothing, as the spec's
                    // reference implementation does.
                    _ => {}
                }
            }
            out
        })
        .collect();
    saw_output.then_some(args)
}

/// Runs a command to completion or until [`TIMEOUT`], whichever comes first.
fn run(command: &mut Command) -> bool {
    run_within(command, TIMEOUT)
}

fn run_within(command: &mut Command, timeout: Duration) -> bool {
    let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => return false,
        }
    }
}

/// A private, unique path for one thumbnailer's output.
fn scratch_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("cadrocfile-thumbnailers");
    let _ = std::fs::create_dir_all(&dir);
    // Only this user should read thumbnails of this user's files.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    dir.join(format!(
        "{}-{}.png",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

use gtk::{gio, prelude::*};

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Thumbnailer Entry]
TryExec=sh
Exec=sh -c 'make %i into %o at %s'
MimeType=application/x-sample;application/x-other;
";

    #[test]
    fn a_thumbnailer_file_is_read_into_types_and_a_command() {
        let (types, exec) = parse(SAMPLE).expect("a valid thumbnailer was rejected");
        assert_eq!(types, ["application/x-sample", "application/x-other"]);
        assert_eq!(exec, ["sh", "-c", "make %i into %o at %s"]);
    }

    /// A `.thumbnailer` file outlives the package that installed its program.
    /// Offering it would fail on every file of that type.
    #[test]
    fn a_thumbnailer_whose_program_is_gone_is_ignored() {
        let orphan = SAMPLE.replace("TryExec=sh", "TryExec=/nonexistent/cadrocfile-test-tool");
        assert_eq!(parse(&orphan), None);
    }

    #[test]
    fn only_the_thumbnailer_group_is_read() {
        let text = "[Other]\nExec=sh\nMimeType=a/b;\n[Thumbnailer Entry]\nTryExec=sh\nExec=sh %o\nMimeType=c/d;\n";
        let (types, _) = parse(text).unwrap();
        assert_eq!(types, ["c/d"]);
    }

    #[test]
    fn placeholders_are_filled_as_the_spec_describes() {
        let exec: Vec<String> =
            ["tool", "-i", "%i", "-u", "%u", "-o", "%o", "-s", "%s", "100%%"].map(String::from).into();
        let args = substitute(&exec, Path::new("/tmp/a b.mp4"), Path::new("/tmp/out.png"), 256).unwrap();
        assert_eq!(args[2], "/tmp/a b.mp4", "a space must not split the argument");
        assert_eq!(args[4], "file:///tmp/a%20b.mp4");
        assert_eq!(args[6], "/tmp/out.png");
        assert_eq!(args[8], "256");
        assert_eq!(args[9], "100%");
    }

    /// Without an output placeholder the result cannot be found.
    #[test]
    fn a_command_that_never_says_where_it_writes_is_refused() {
        let exec: Vec<String> = ["tool", "%i"].map(String::from).into();
        assert_eq!(substitute(&exec, Path::new("/a"), Path::new("/b"), 128), None);
    }

    #[test]
    fn a_command_reports_success_failure_and_absence() {
        assert!(run(&mut Command::new("true")));
        assert!(!run(&mut Command::new("false")));
        assert!(!run(&mut Command::new("/nonexistent/cadrocfile-test-tool")));
    }

    /// A thumbnailer fed a truncated file can hang forever. It must be killed
    /// on schedule, not waited on, or it holds a decode slot for the session.
    #[test]
    fn a_hung_thumbnailer_is_killed_rather_than_waited_on() {
        let started = Instant::now();
        let finished = run_within(Command::new("sleep").arg("30"), Duration::from_millis(200));
        assert!(!finished, "a command that overran must count as a failure");
        assert!(started.elapsed() < Duration::from_secs(5), "the child was waited on, not killed");
    }

    /// End to end against the tools actually installed, skipped where absent.
    #[test]
    fn a_video_frame_is_rendered_when_ffmpeg_is_present() {
        if which("ffmpeg").is_none() {
            eprintln!("skipped: ffmpeg not installed");
            return;
        }
        let dir = crate::testing::TempDir::new("thumb-video");
        let video = dir.join("clip:with colon.mp4");
        let made = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc=duration=1:size=320x240:rate=10"])
            .arg(&video)
            .status()
            .is_ok_and(|s| s.success());
        assert!(made, "could not create a test video");

        // One second long, so the 3 s seek produces nothing and the retry at
        // zero has to carry it.
        let png = render(&Method::Ffmpeg, &video, 128).expect("no frame was rendered");
        let (w, h) = image_size(&png);
        assert!(w <= 128 && h <= 128 && w > 0, "frame is {w}x{h}, not within 128");
        std::fs::remove_file(png).unwrap();
    }

    #[test]
    fn a_pdf_page_is_rendered_when_pdftoppm_is_present() {
        if which("pdftoppm").is_none() {
            eprintln!("skipped: pdftoppm not installed");
            return;
        }
        let dir = crate::testing::TempDir::new("thumb-pdf");
        let pdf = dir.join("doc.pdf");
        std::fs::write(&pdf, MINIMAL_PDF).unwrap();
        let png = render(&Method::Pdftoppm, &pdf, 128).expect("no page was rendered");
        let (w, h) = image_size(&png);
        assert!(w.max(h) == 128, "page is {w}x{h}; the long side should be 128");
        std::fs::remove_file(png).unwrap();
    }

    fn image_size(png: &Path) -> (i32, i32) {
        let pixbuf = gdk_pixbuf::Pixbuf::from_file(png).expect("output is not a readable image");
        (pixbuf.width(), pixbuf.height())
    }

    /// The smallest PDF poppler will render: one blank page.
    const MINIMAL_PDF: &[u8] = b"%PDF-1.1
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 300]>>endobj
trailer<</Root 1 0 R>>
%%EOF
";
}
