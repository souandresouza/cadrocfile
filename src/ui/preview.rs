//! Quick preview: press Space on a file to see it without opening an app.
//!
//! One window, reused. It follows the view's current order, so the arrow keys
//! step through exactly the files the user is looking at, and the selection in
//! the main view follows along — closing the preview leaves you on the last
//! file you looked at.
//!
//! Every loader runs off the main thread and is tagged with a generation, so
//! holding an arrow key through a folder of 40 MB photos never stalls the
//! window or lets a slow decode paint over the file that replaced it.
use crate::tr;

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gdk, glib, pango};

use crate::{fs::FileEntry, ui::file_object::FileObject};

/// Largest image or page rendered, in pixels on the long side.
const MAX_IMAGE: i32 = 2048;

/// How much of a text file is shown. Enough for any source file anyone reads
/// in a preview; a log of several gigabytes stays cheap.
const MAX_TEXT_BYTES: usize = 512 * 1024;

type Callback<T> = RefCell<Option<Rc<dyn Fn(T)>>>;

fn emit<T>(slot: &Callback<T>, arg: T) {
    let callback = slot.borrow().clone();
    if let Some(callback) = callback {
        callback(arg);
    }
}

/// What kind of preview a file gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Folder,
    Image,
    Video,
    Audio,
    Text,
    /// Rendered to a picture by a thumbnailer: PDF, office documents.
    Document,
    Other,
}

fn kind_of(entry: &FileEntry) -> Kind {
    if entry.is_dir {
        return Kind::Folder;
    }
    let ct = entry.content_type.as_str();
    let is_a = |parent: &str| gio::functions::content_type_is_a(ct, parent);
    if ct.starts_with("image/") || crate::fs::raw::is_raw(&entry.path) {
        Kind::Image
    } else if ct.starts_with("video/") || is_a("video/*") {
        Kind::Video
    } else if ct.starts_with("audio/") || is_a("audio/*") {
        Kind::Audio
    } else if is_a("text/plain") {
        Kind::Text
    } else if crate::ui::thumbnailers::method_for(ct).is_some() {
        Kind::Document
    } else if matches!(ct, "application/octet-stream" | "application/x-zerosize") {
        // Unknown type: often a text file with an unusual name (`LICENSE`,
        // `.env.local`). The reader checks for binary content before showing
        // anything, so guessing text here costs nothing when it is wrong.
        Kind::Text
    } else {
        Kind::Other
    }
}

pub struct Preview {
    window: adw::Window,
    title: adw::WindowTitle,
    body: adw::Bin,
    note: gtk::Label,
    items: RefCell<Vec<FileObject>>,
    index: Cell<usize>,
    /// Bumped per file shown; a loader that finishes for an older value is
    /// describing a file no longer on screen and is discarded.
    generation: Cell<u64>,
    /// The playing stream, so it can be stopped when the file changes or the
    /// window closes. Audio carrying on after the preview is gone is the bug
    /// everyone who has built one of these has shipped once.
    media: RefCell<Option<gtk::MediaStream>>,
    on_open: Callback<PathBuf>,
    on_moved: Callback<PathBuf>,
}

impl Preview {
    pub fn new(parent: &impl IsA<gtk::Window>) -> Rc<Self> {
        let title = adw::WindowTitle::new("", "");
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let open = gtk::Button::builder()
            .label(tr!("Open"))
            .tooltip_text("Open with the default application (Enter)")
            .build();
        header.pack_end(&open);

        let previous = gtk::Button::from_icon_name("go-previous-symbolic");
        previous.set_tooltip_text(Some("Previous (←)"));
        let next = gtk::Button::from_icon_name("go-next-symbolic");
        next.set_tooltip_text(Some("Next (→)"));
        let nav = gtk::Box::builder().css_classes(["linked"]).build();
        nav.append(&previous);
        nav.append(&next);
        header.pack_start(&nav);

        let body = adw::Bin::builder().vexpand(true).hexpand(true).build();
        let note = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(10)
            .css_classes(["caption", "dim-label"])
            .visible(false)
            .build();

        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        content.append(&body);
        content.append(&note);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let window = adw::Window::builder()
            .default_width(960)
            .default_height(720)
            .transient_for(parent)
            .hide_on_close(true)
            .content(&toolbar)
            .build();

        let this = Rc::new(Self {
            window,
            title,
            body,
            note,
            items: RefCell::new(Vec::new()),
            index: Cell::new(0),
            generation: Cell::new(0),
            media: RefCell::new(None),
            on_open: RefCell::new(None),
            on_moved: RefCell::new(None),
        });

        let weak = Rc::downgrade(&this);
        open.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.open_current();
            }
        });
        let weak = Rc::downgrade(&this);
        previous.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.step(-1);
            }
        });
        let weak = Rc::downgrade(&this);
        next.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.step(1);
            }
        });

        // Capture phase: a video widget would otherwise take Space for
        // play/pause, and Space has to mean "close" everywhere in here for the
        // gesture that opened the preview to be the one that dismisses it.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::space | gdk::Key::Escape => this.close(),
                gdk::Key::Left | gdk::Key::Up | gdk::Key::Page_Up => this.step(-1),
                gdk::Key::Right | gdk::Key::Down | gdk::Key::Page_Down => this.step(1),
                gdk::Key::Return | gdk::Key::KP_Enter => this.open_current(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        this.window.add_controller(keys);

        let weak = Rc::downgrade(&this);
        this.window.connect_hide(move |_| {
            if let Some(this) = weak.upgrade() {
                this.stop_media();
                // Anything still loading is for a window nobody is looking at.
                this.generation.set(this.generation.get() + 1);
            }
        });

        this
    }

    /// Asked for when the user presses Enter or clicks Open.
    pub fn connect_open(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_open.borrow_mut() = Some(Rc::new(f));
    }

    /// Told which file is showing, so the main view can select it.
    pub fn connect_moved(&self, f: impl Fn(PathBuf) + 'static) {
        *self.on_moved.borrow_mut() = Some(Rc::new(f));
    }

    pub fn is_open(&self) -> bool {
        self.window.is_visible()
    }

    pub fn close(&self) {
        self.window.set_visible(false);
    }

    /// Shows `items[index]`, with the rest available to the arrow keys.
    pub fn show(self: &Rc<Self>, items: Vec<FileObject>, index: usize) {
        if items.is_empty() {
            return;
        }
        self.index.set(index.min(items.len() - 1));
        *self.items.borrow_mut() = items;
        self.present_current();
        self.window.present();
    }

    fn step(self: &Rc<Self>, delta: isize) {
        let count = self.items.borrow().len();
        if count < 2 {
            return;
        }
        // Wraps, so holding the arrow key cycles rather than dead-ending.
        let next = (self.index.get() as isize + delta).rem_euclid(count as isize) as usize;
        self.index.set(next);
        self.present_current();
        if let Some(object) = self.current() {
            emit(&self.on_moved, object.path());
        }
    }

    fn current(&self) -> Option<FileObject> {
        self.items.borrow().get(self.index.get()).cloned()
    }

    fn open_current(&self) {
        if let Some(object) = self.current() {
            let path = object.path();
            self.close();
            emit(&self.on_open, path);
        }
    }

    fn stop_media(&self) {
        if let Some(stream) = self.media.borrow_mut().take() {
            stream.set_playing(false);
        }
    }

    fn present_current(self: &Rc<Self>) {
        self.stop_media();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        let Some(object) = self.current() else { return };
        let entry = object.entry();
        let kind = kind_of(&entry);

        let position = format!("{} of {}", self.index.get() + 1, self.items.borrow().len());
        let detail = if entry.is_dir {
            position
        } else {
            format!("{position} · {} · {}", entry.size_label(), entry.kind_label())
        };
        self.title.set_title(&entry.display_name);
        self.title.set_subtitle(&detail);
        self.set_note(None);

        match kind {
            Kind::Image | Kind::Document => self.show_picture(&entry, generation, kind),
            Kind::Video | Kind::Audio => self.show_media(&entry, generation, kind),
            Kind::Text => self.show_text(&entry, generation),
            Kind::Folder => self.show_folder(&entry, generation),
            Kind::Other => self.body.set_child(Some(&info_card(&entry, None))),
        }
    }

    fn set_note(&self, text: Option<&str>) {
        match text {
            Some(text) => {
                self.note.set_label(text);
                self.note.set_visible(true);
            }
            None => self.note.set_visible(false),
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.get() == generation && self.window.is_visible()
    }

    fn show_loading(&self) {
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        self.body.set_child(Some(&spinner));
    }

    fn show_picture(self: &Rc<Self>, entry: &FileEntry, generation: u64, kind: Kind) {
        self.show_loading();
        let (path, content_type) = (entry.path.clone(), entry.content_type.clone());
        let entry = entry.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let texture = crate::ui::thumbs::render_large(path, &content_type, MAX_IMAGE).await;
            let Some(this) = weak.upgrade() else { return };
            if !this.is_current(generation) {
                return;
            }
            match texture {
                Some(texture) => {
                    this.body.set_child(Some(&picture(&texture)));
                    if kind == Kind::Document {
                        this.set_note(Some("First page. Press Enter to open the whole document."));
                    }
                    if kind == Kind::Image {
                        show_exif_note(&this, &entry, generation);
                    }
                }
                None => {
                    this.body.set_child(Some(&info_card(&entry, Some("This file could not be rendered."))));
                }
            }
        });
    }

    /// Plays video and audio in place, falling back to a still frame.
    ///
    /// GTK plays media through GStreamer, and which formats that can decode is
    /// down to the plugins a distribution installed. An MP4 needs the demuxer
    /// from gst-plugins-good, which plenty of systems lack. The stream reports
    /// that as an error some time after it starts, so the fallback is wired to
    /// the error rather than decided up front — decided up front, it would be
    /// wrong on every machine that *can* play the file.
    fn show_media(self: &Rc<Self>, entry: &FileEntry, generation: u64, kind: Kind) {
        let media = gtk::MediaFile::for_filename(&entry.path);
        let video = gtk::Video::builder()
            .autoplay(true)
            .vexpand(true)
            .hexpand(true)
            .build();
        video.set_media_stream(Some(&media));

        if kind == Kind::Audio {
            // A video widget playing audio is an empty rectangle over a
            // control bar. The file's icon fills the space instead.
            let column = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(18)
                .valign(gtk::Align::Center)
                .build();
            let icon = gtk::Image::from_gicon(&crate::ui::thumbs::icon_for(&entry.content_type, false, false));
            icon.set_pixel_size(128);
            column.append(&icon);
            video.set_vexpand(false);
            video.set_valign(gtk::Align::End);
            column.append(&video);
            self.body.set_child(Some(&column));
        } else {
            self.body.set_child(Some(&video));
        }
        *self.media.borrow_mut() = Some(media.clone().upcast());

        let weak = Rc::downgrade(self);
        let entry = entry.clone();
        media.connect_error_notify(move |stream| {
            let Some(this) = weak.upgrade() else { return };
            if stream.error().is_none() || !this.is_current(generation) {
                return;
            }
            this.stop_media();
            if kind == Kind::Video {
                this.set_note(Some(
                    "This video can't be played here — the codec isn't installed. Showing a \
                     still frame; press Enter to open it in a player.",
                ));
                this.show_picture(&entry, generation, Kind::Video);
            } else {
                this.body.set_child(Some(&info_card(
                    &entry,
                    Some("This audio can't be played here — the codec isn't installed."),
                )));
            }
        });
    }

    fn show_text(self: &Rc<Self>, entry: &FileEntry, generation: u64) {
        self.show_loading();
        let path = entry.path.clone();
        let entry = entry.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let text = crate::ui::actions::run_off_thread(move || read_text(&path)).await;
            let Some(this) = weak.upgrade() else { return };
            if !this.is_current(generation) {
                return;
            }
            match text {
                Some((text, truncated)) => {
                    let path = entry.path.clone();
                    this.body.set_child(Some(&text_view(&text, &path)));
                    if truncated {
                        this.set_note(Some("Showing the first 512 KB."));
                    }
                }
                // Looked like it might be text; it was not.
                None => this.body.set_child(Some(&info_card(&entry, None))),
            }
        });
    }

    fn show_folder(self: &Rc<Self>, entry: &FileEntry, generation: u64) {
        self.body.set_child(Some(&info_card(entry, Some("Counting…"))));
        let path = entry.path.clone();
        let entry = entry.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let count = crate::ui::actions::run_off_thread(move || {
                std::fs::read_dir(&path).map(|entries| entries.count()).ok()
            })
            .await;
            let Some(this) = weak.upgrade() else { return };
            if !this.is_current(generation) {
                return;
            }
            let line = match count {
                Some(0) => "Empty".to_string(),
                Some(1) => "1 item".to_string(),
                Some(n) => format!("{n} items"),
                None => "Cannot be read".to_string(),
            };
            this.body.set_child(Some(&info_card(&entry, Some(&line))));
        });
    }
}

/// Fills the note line with the camera's summary once EXIF has been read.
///
/// Read on a worker because the photo may be on a network mount; the note is a
/// bonus, so a slow or missing answer simply leaves it hidden.
fn show_exif_note(this: &Rc<Preview>, entry: &FileEntry, generation: u64) {
    let (tx, rx) = async_channel::bounded(1);
    let path = entry.path.clone();
    std::thread::Builder::new()
        .name("cadrocfile-exif".into())
        .spawn(move || {
            let _ = tx.send_blocking(crate::fs::exif::summary(&path));
        })
        .ok();
    let weak = Rc::downgrade(this);
    glib::spawn_future_local(async move {
        let Ok(Some(summary)) = rx.recv().await else { return };
        let Some(this) = weak.upgrade() else { return };
        if this.is_current(generation) {
            this.set_note(Some(&summary));
        }
    });
}

/// Reads the start of a file as text, or `None` if it is binary.
///
/// A NUL byte in the first 8 KB is the same test `file` and git use: text
/// essentially never contains one, and binary formats nearly always do early.
fn read_text(path: &Path) -> Option<(String, bool)> {
    use std::io::Read;
    let mut buffer = Vec::with_capacity(64 * 1024);
    std::fs::File::open(path)
        .ok()?
        .take(MAX_TEXT_BYTES as u64 + 1)
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer[..buffer.len().min(8192)].contains(&0) {
        return None;
    }
    let truncated = buffer.len() > MAX_TEXT_BYTES;
    buffer.truncate(MAX_TEXT_BYTES);
    // Cutting at a byte limit can split a multi-byte character; lossy decoding
    // turns the fragment into one replacement character instead of failing.
    Some((String::from_utf8_lossy(&buffer).into_owned(), truncated))
}

fn picture(texture: &gdk::Texture) -> gtk::Picture {
    gtk::Picture::builder()
        .paintable(texture)
        .content_fit(gtk::ContentFit::ScaleDown)
        .can_shrink(true)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

fn text_view(text: &str, path: &Path) -> gtk::ScrolledWindow {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(14)
        .right_margin(14)
        .build();
    let buffer = view.buffer();
    buffer.set_text(text);
    highlight(&buffer, text, path);
    gtk::ScrolledWindow::builder().child(&view).vexpand(true).build()
}

/// How much of a file is coloured in. Past this the spans outnumber the value.
const MAX_HIGHLIGHT_CHARS: usize = 200_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    CLike,
    Python,
    Shell,
    Json,
    Toml,
    Markdown,
    Plain,
}

impl Language {
    fn from_path(path: &Path) -> Self {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "rs" => Self::Rust,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "java" | "js" | "mjs" | "ts" | "tsx"
            | "jsx" | "go" | "swift" | "kt" | "kts" | "cs" => Self::CLike,
            "py" | "pyi" | "pyw" => Self::Python,
            "sh" | "bash" | "zsh" | "fish" | "ksh" | "dash" => Self::Shell,
            "json" | "jsonc" => Self::Json,
            "toml" | "ini" | "cfg" | "conf" => Self::Toml,
            "md" | "markdown" => Self::Markdown,
            _ => Self::Plain,
        }
    }

    fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Rust | Self::CLike => Some("//"),
            Self::Python | Self::Shell | Self::Toml => Some("#"),
            Self::Json | Self::Markdown | Self::Plain => None,
        }
    }

    fn has_block_comments(self) -> bool {
        matches!(self, Self::Rust | Self::CLike | Self::Toml)
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            Self::CLike => &[
                "abstract", "async", "await", "boolean", "break", "case", "catch", "char", "class",
                "const", "continue", "def", "default", "do", "double", "else", "enum", "extends",
                "false", "final", "finally", "float", "for", "foreach", "func", "function", "if",
                "implements", "import", "in", "int", "interface", "let", "long", "namespace",
                "new", "null", "package", "private", "protected", "public", "return", "short",
                "static", "struct", "switch", "this", "throw", "throws", "true", "try", "typedef",
                "typeof", "union", "unsigned", "var", "virtual", "void", "volatile", "while",
            ],
            Self::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
                "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield",
            ],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "readonly", "return", "select", "set", "then", "until",
                "while",
            ],
            Self::Json => &["false", "null", "true"],
            Self::Toml => &["false", "true"],
            Self::Markdown | Self::Plain => &[],
        }
    }
}

/// Colours strings, comments, numbers and keywords in the preview.
///
/// Deliberately a scanner rather than a parser: it runs on text that may be
/// half a megabyte of anything, and being wrong about one token's colour costs
/// nothing. Keywords and punctuation come from a per-language list, which is
/// enough for a preview without a grammar for every language in existence.
fn highlight(buffer: &gtk::TextBuffer, text: &str, path: &Path) {
    let language = Language::from_path(path);
    if language == Language::Plain {
        return;
    }

    let dark = adw::StyleManager::default().is_dark();
    let (keyword, string, comment, number, heading) = if dark {
        ("#c678dd", "#98c379", "#7f848e", "#d19a66", "#61afef")
    } else {
        ("#a626a4", "#50a14f", "#a0a1a7", "#986801", "#4078f2")
    };
    for (name, colour) in [
        ("syntax-keyword", keyword),
        ("syntax-string", string),
        ("syntax-comment", comment),
        ("syntax-number", number),
        ("syntax-heading", heading),
    ] {
        let tag = gtk::TextTag::builder().name(name).foreground(colour).build();
        buffer.tag_table().add(&tag);
    }

    let chars: Vec<char> = text.chars().take(MAX_HIGHLIGHT_CHARS).collect();
    let keywords = language.keywords();
    let mut index = 0;

    while index < chars.len() {
        let at_line_start = index == 0 || chars[index - 1] == '\n';

        if language == Language::Markdown && at_line_start && chars[index] == '#' {
            let start = index;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            tag_span(buffer, "syntax-heading", start, index);
            continue;
        }

        if let Some(marker) = language.line_comment()
            && starts_with(&chars, index, marker)
        {
            let start = index;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            tag_span(buffer, "syntax-comment", start, index);
            continue;
        }

        if language.has_block_comments() && starts_with(&chars, index, "/*") {
            let start = index;
            index += 2;
            while index < chars.len() && !starts_with(&chars, index, "*/") {
                index += 1;
            }
            index = (index + 2).min(chars.len());
            tag_span(buffer, "syntax-comment", start, index);
            continue;
        }

        let character = chars[index];
        if character == '"' || character == '\'' || (language == Language::Markdown && character == '`')
        {
            let quote = character;
            // Python's triple-quoted string, and nothing else, spans lines.
            let triple = language == Language::Python
                && starts_with(&chars, index, &quote.to_string().repeat(3));
            let start = index;
            index += if triple { 3 } else { 1 };
            while index < chars.len() {
                if chars[index] == '\\' && !triple {
                    index += 2;
                    continue;
                }
                if triple && starts_with(&chars, index, &quote.to_string().repeat(3)) {
                    index += 3;
                    break;
                }
                if !triple && chars[index] == quote {
                    index += 1;
                    break;
                }
                if !triple && chars[index] == '\n' {
                    break;
                }
                index += 1;
            }
            tag_span(buffer, "syntax-string", start, index.min(chars.len()));
            continue;
        }

        if character.is_ascii_digit() {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '.' || chars[index] == '_')
            {
                index += 1;
            }
            tag_span(buffer, "syntax-number", start, index);
            continue;
        }

        if character.is_alphabetic() || character == '_' {
            let start = index;
            while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            if keywords.contains(&word.as_str()) {
                tag_span(buffer, "syntax-keyword", start, index);
            }
            continue;
        }

        index += 1;
    }
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    needle.chars().enumerate().all(|(offset, wanted)| chars.get(at + offset) == Some(&wanted))
}

fn tag_span(buffer: &gtk::TextBuffer, name: &str, from: usize, to: usize) {
    if to <= from || from > i32::MAX as usize || to > i32::MAX as usize {
        return;
    }
    let start = buffer.iter_at_offset(from as i32);
    let end = buffer.iter_at_offset(to as i32);
    buffer.apply_tag_by_name(name, &start, &end);
}

/// A large icon with the file's name and details, for anything not rendered.
fn info_card(entry: &FileEntry, extra: Option<&str>) -> gtk::Box {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .margin_start(24)
        .margin_end(24)
        .build();

    let icon = gtk::Image::from_gicon(&crate::ui::thumbs::icon_for(
        &entry.content_type,
        entry.is_dir,
        entry.is_symlink,
    ));
    icon.set_pixel_size(128);
    icon.set_margin_bottom(8);
    column.append(&icon);

    column.append(
        &gtk::Label::builder()
            .label(&entry.display_name)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .justify(gtk::Justification::Center)
            .css_classes(["title-2"])
            .build(),
    );

    let mut details = Vec::new();
    if !entry.is_dir {
        details.push(entry.kind_label());
        details.push(entry.size_label());
    }
    details.push(entry.modified_label());
    column.append(
        &gtk::Label::builder()
            .label(details.join(" · "))
            .css_classes(["dim-label"])
            .build(),
    );

    if let Some(extra) = extra {
        column.append(
            &gtk::Label::builder()
                .label(extra)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .margin_top(6)
                .build(),
        );
    }
    column
}

use gtk::gio;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_read_and_binary_is_refused() {
        let dir = crate::testing::TempDir::new("preview-text");
        let text = dir.join("notes");
        std::fs::write(&text, "hello\nworld\n").unwrap();
        assert_eq!(read_text(&text), Some(("hello\nworld\n".to_string(), false)));

        let binary = dir.join("blob");
        std::fs::write(&binary, [0x7f, b'E', b'L', b'F', 0, 0, 1]).unwrap();
        assert_eq!(read_text(&binary), None, "a NUL byte means binary");
    }

    /// Huge files must cost a bounded read, and say they were cut short.
    #[test]
    fn a_large_text_file_is_truncated_and_says_so() {
        let dir = crate::testing::TempDir::new("preview-big");
        let big = dir.join("big.log");
        std::fs::write(&big, "x".repeat(MAX_TEXT_BYTES + 5000)).unwrap();
        let (text, truncated) = read_text(&big).unwrap();
        assert!(truncated);
        assert_eq!(text.len(), MAX_TEXT_BYTES);
    }

    /// The byte limit can land inside a multi-byte character, which must not
    /// turn the whole preview into a failure.
    #[test]
    fn truncation_inside_a_multibyte_character_is_harmless() {
        let dir = crate::testing::TempDir::new("preview-utf8");
        let file = dir.join("wide.txt");
        // 3-byte characters, so the limit cannot fall on a boundary.
        std::fs::write(&file, "界".repeat(MAX_TEXT_BYTES / 3 + 10)).unwrap();
        let (text, truncated) = read_text(&file).expect("valid UTF-8 cut short is still text");
        assert!(truncated);
        assert!(text.starts_with('界'));
    }
}
