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
    if ct.starts_with("image/") {
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
                    this.body.set_child(Some(&text_view(&text)));
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

fn text_view(text: &str) -> gtk::ScrolledWindow {
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
    view.buffer().set_text(text);
    gtk::ScrolledWindow::builder().child(&view).vexpand(true).build()
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
