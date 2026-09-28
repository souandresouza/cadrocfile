//! Choosing which application opens a file.
//!
//! The MIME registry is not the whole answer, and treating it as one is why
//! this dialog used to look broken. An application appears in
//! `g_app_info_get_all_for_type` only if its `.desktop` file *declares* that
//! type, and plenty of programs that open a format perfectly well never
//! declare it: on this machine only VLC claims `video/mp4`, while Firefox,
//! Zen and Chromium — all of which play an MP4 — declare `audio/ogg` and
//! `video/webm` but not `video/mp4`. Listing only what the registry suggests
//! leaves the user staring at one entry, certain the file manager is wrong.
//!
//! So the list is in three parts: what the desktop recommends, anything else
//! that claims the type, and — one click away — every application installed.
//! The last is what makes the dialog able to answer "open it with *that* one",
//! which is the entire point of an Open With dialog.
use crate::tr;

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

/// Which part of the list an application came from.
///
/// The order is the order they appear, and it is the whole design: an
/// application that plays video belongs next to the other video players, not
/// forty rows below a settings panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    /// The desktop's own recommendation for this type.
    Recommended,
    /// Declares the type, but is not recommended for it.
    Declares,
    /// Declares some other type in the same family — a program that opens
    /// `video/webm` but never mentioned `video/mp4` almost certainly opens
    /// both, and this is where the browsers on this machine turn up.
    Family,
    /// A web browser. Browsers open most things and are what people reach for
    /// when the dedicated application is not what they want, yet they declare
    /// remarkably few types: on this machine Chromium claims no video type at
    /// all while playing video perfectly well.
    Browser,
    /// Everything else installed.
    Installed,
}

impl Section {
    fn title(self, kind: &str, family: Option<&str>) -> String {
        match self {
            Section::Recommended => format!("Recommended for {kind}"),
            Section::Declares => "Also opens this kind of file".to_string(),
            Section::Family => match family {
                Some(family) => format!("Other {family} applications"),
                None => "Related applications".to_string(),
            },
            Section::Browser => "Web browsers".to_string(),
            Section::Installed => "All applications".to_string(),
        }
    }
}

/// The media family of a content type, when grouping by it is meaningful.
///
/// Only for the families whose members really are interchangeable. Grouping
/// by `application` would put a PDF reader and an archive manager in one
/// bucket on the strength of sharing a prefix, which says nothing.
fn family_of(content_type: &str) -> Option<&'static str> {
    match content_type.split('/').next()? {
        "video" => Some("video"),
        "audio" => Some("audio"),
        "image" => Some("image"),
        "text" => Some("text"),
        _ => None,
    }
}

fn is_browser(types: &[glib::GString]) -> bool {
    types
        .iter()
        .any(|t| t == "x-scheme-handler/http" || t == "x-scheme-handler/https")
}

struct Candidate {
    app: gio::AppInfo,
    section: Section,
    /// Name, description and command, lowercased once for the search filter.
    haystack: String,
}

/// What the user chose.
pub struct Choice {
    pub app: gio::AppInfo,
    /// Make this the default for the type from now on.
    pub set_default: bool,
}

/// Every application worth offering for `content_type`, in display order.
fn candidates(content_type: &str) -> Vec<Candidate> {
    let recommended = gio::AppInfo::recommended_for_type(content_type);
    let declaring = gio::AppInfo::all_for_type(content_type);

    let mut out: Vec<Candidate> = Vec::new();
    let add = |app: gio::AppInfo, section: Section, out: &mut Vec<Candidate>| {
        // `id` is the desktop file name, so this also collapses the duplicate
        // entries a system with both /usr/share and ~/.local copies produces.
        if out.iter().any(|c| c.app.id() == app.id()) {
            return;
        }
        let haystack = format!(
            "{} {} {}",
            app.display_name(),
            app.description().unwrap_or_default(),
            app.commandline().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
        )
        .to_lowercase();
        out.push(Candidate { app, section, haystack });
    };

    for app in recommended {
        add(app, Section::Recommended, &mut out);
    }
    for app in declaring {
        add(app, Section::Declares, &mut out);
    }
    let listed = out.len();
    let family = family_of(content_type);
    for app in gio::AppInfo::all() {
        // Installed-but-hidden entries are helpers and settings panels, not
        // things to open a file with. Anything that claims the type is kept
        // above regardless, because claiming it is a deliberate statement.
        // Cadrocfile is skipped too: handing a file back to the file manager it
        // was chosen in is never what the user meant.
        if !app.should_show() || app.id().as_deref() == Some(SELF_ID) {
            continue;
        }
        let types = app.supported_types();
        let section = if family.is_some_and(|f| types.iter().any(|t| t.starts_with(&format!("{f}/")))) {
            Section::Family
        } else if is_browser(&types) {
            Section::Browser
        } else {
            Section::Installed
        };
        add(app, section, &mut out);
    }

    // The desktop's own order is meaningful for the recommendations — the
    // default comes first — but everything after them arrives in whatever
    // order the directories were read in, which is no order at all to scan.
    out[listed..].sort_by_key(|c| (c.section, c.app.display_name().to_lowercase()));
    out
}

/// This application's own desktop entry.
const SELF_ID: &str = "dev.cadrocfile.Files.desktop";

/// Asks which application should open `names`, describing `content_type`.
///
/// `mixed_types` disables the "always use this" option: making one application
/// the default for several different kinds of file at once is not something
/// the user can have meant.
pub async fn ask(
    parent: &impl IsA<gtk::Widget>,
    names: &[String],
    content_type: &str,
    mixed_types: bool,
) -> Option<Choice> {
    let candidates = Rc::new(candidates(content_type));
    if candidates.is_empty() {
        return None;
    }

    let kind = gio::functions::content_type_get_description(content_type).to_string();
    let family = family_of(content_type);
    let subtitle = match names {
        [one] => one.clone(),
        many => format!("{} items", many.len()),
    };

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search applications…")
        .hexpand(true)
        .build();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .build();

    for candidate in candidates.iter() {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&candidate.app.display_name()))
            .activatable(true)
            .build();
        if let Some(description) = candidate.app.description() {
            row.set_subtitle(&glib::markup_escape_text(&description));
        } else if let Some(command) = candidate.app.commandline() {
            row.set_subtitle(&glib::markup_escape_text(&command.to_string_lossy()));
        }
        row.set_subtitle_lines(1);
        if let Some(icon) = candidate.app.icon() {
            let image = gtk::Image::from_gicon(&icon);
            image.set_pixel_size(32);
            row.add_prefix(&image);
        }
        list.append(&row);
    }

    // Section headings, inserted wherever the section changes. A plain list of
    // sixty applications with no headings tells the user nothing about which
    // ones actually understand the file.
    {
        let candidates = Rc::clone(&candidates);
        let kind = kind.clone();
        list.set_header_func(move |row, before| {
            let section = |r: &gtk::ListBoxRow| candidates.get(r.index() as usize).map(|c| c.section);
            let Some(current) = section(row) else { return };
            if before.and_then(section) == Some(current) {
                row.set_header(None::<&gtk::Widget>);
                return;
            }
            let header = gtk::Label::builder()
                .label(current.title(&kind, family))
                .xalign(0.0)
                .margin_top(if before.is_none() { 2 } else { 14 })
                .margin_bottom(6)
                .margin_start(4)
                .css_classes(["heading", "dim-label"])
                .build();
            row.set_header(Some(&header));
        });
    }

    // Filtering: the long tail of installed applications stays hidden until
    // the user asks for it, or types something that matches one.
    // Search narrows the list; nothing else hides anything. Collapsing the
    // long tail behind a toggle is what made the browsers invisible in the
    // first place, and a section the user cannot see may as well not exist.
    {
        let candidates = Rc::clone(&candidates);
        let search = search.clone();
        list.set_filter_func(move |row| {
            let Some(candidate) = candidates.get(row.index() as usize) else { return true };
            let query = search.text().to_lowercase();
            let query = query.trim().to_string();
            query.is_empty() || candidate.haystack.contains(&query)
        });
    }

    {
        let list = list.clone();
        search.connect_search_changed(move |_| list.invalidate_filter());
    }

    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let set_default = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .sensitive(!mixed_types)
        .build();
    let default_row = adw::ActionRow::builder()
        .title("Always open with this application")
        .subtitle(if mixed_types {
            "The selection holds more than one kind of file, so there is no single default to set"
                .to_string()
        } else {
            format!("Makes it the default for {kind} everywhere, not only in Cadrocfile")
        })
        .build();
    default_row.add_suffix(&set_default);
    default_row.set_activatable_widget(Some(&set_default));
    let default_group = adw::PreferencesGroup::new();
    default_group.add(&default_row);

    let cancel = gtk::Button::with_label(tr!("Cancel"));
    let accept = gtk::Button::builder()
        .label(tr!("Open"))
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::End).build();
    buttons.append(&cancel);
    buttons.append(&accept);

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&search);
    body.append(&scroller);
    body.append(&default_group);
    body.append(&buttons);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Open With", &subtitle)));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));

    let dialog = adw::Dialog::builder()
        .title("Open With")
        .content_width(620)
        .content_height(620)
        .child(&toolbar)
        .build();

    {
        let accept = accept.clone();
        list.connect_selected_rows_changed(move |list| {
            accept.set_sensitive(list.selected_row().is_some());
        });
    }

    let (tx, rx) = async_channel::bounded::<bool>(1);
    {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        // Double-click or Enter on a row opens straight away, which is what
        // everyone tries first.
        list.connect_row_activated(move |list, row| {
            list.select_row(Some(row));
            let _ = tx.try_send(true);
            dialog.close();
        });
    }
    {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        accept.connect_clicked(move |_| {
            let _ = tx.try_send(true);
            dialog.close();
        });
    }
    {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        cancel.connect_clicked(move |_| {
            let _ = tx.try_send(false);
            dialog.close();
        });
    }
    dialog.connect_closed(move |_| {
        let _ = tx.try_send(false);
    });

    // Start on the recommended application, so Enter alone does the obvious
    // thing without touching the mouse.
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }
    let focus = list.clone();
    glib::idle_add_local_once(move || {
        focus.grab_focus();
    });

    dialog.present(Some(parent));
    if !rx.recv().await.unwrap_or(false) {
        return None;
    }

    let index = list.selected_row()?.index() as usize;
    let app = candidates.get(index)?.app.clone();
    Some(Choice { app, set_default: set_default.is_active() && !mixed_types })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this dialog was rebuilt for: the registry lists one player
    /// for an MP4 on a machine with several programs that play one, because
    /// the others never declare the type. Every installed application has to
    /// be reachable, or the dialog cannot answer "open it with that one".
    #[test]
    fn every_installed_application_is_offered_not_just_the_ones_claiming_the_type() {
        let found = candidates("video/mp4");
        if found.is_empty() {
            eprintln!("skipped: no applications installed");
            return;
        }
        let declaring = gio::AppInfo::all_for_type("video/mp4").len();
        assert!(
            found.len() > declaring,
            "only the {declaring} applications declaring video/mp4 were offered",
        );

        // Every visible application must be reachable — that is the whole
        // point — except Cadrocfile itself.
        let offered: std::collections::HashSet<String> =
            found.iter().filter_map(|c| c.app.id().map(|i| i.to_string())).collect();
        for app in gio::AppInfo::all().iter().filter(|a| a.should_show()) {
            let Some(id) = app.id().map(|i| i.to_string()) else { continue };
            if id == SELF_ID {
                continue;
            }
            assert!(offered.contains(&id), "{id} is installed but was not offered");
        }
    }

    /// An application installed both system-wide and for the user appears
    /// twice in the sources; it must appear once in the list.
    #[test]
    fn an_application_is_listed_once() {
        let found = candidates("text/plain");
        let mut ids: Vec<String> =
            found.iter().filter_map(|c| c.app.id().map(|i| i.to_string())).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "the same application was listed twice");
    }

    /// The complaint that prompted this: a browser plays an MP4, but declares
    /// no MP4 type, so it never appeared. It must be near the top — grouped
    /// with the other video applications or under Web browsers — and never
    /// buried in the alphabetical tail with the settings panels.
    #[test]
    fn browsers_are_offered_near_the_top_for_a_video() {
        let found = candidates("video/mp4");
        let browsers: Vec<&Candidate> = found
            .iter()
            .filter(|c| is_browser(&c.app.supported_types()))
            .collect();
        if browsers.is_empty() {
            eprintln!("skipped: no browser installed");
            return;
        }
        for browser in browsers {
            assert!(
                browser.section < Section::Installed,
                "{} was buried in {:?}",
                browser.app.display_name(),
                browser.section,
            );
        }
    }

    /// Grouping by family only makes sense where the members really are
    /// interchangeable.
    #[test]
    fn only_media_families_are_grouped() {
        assert_eq!(family_of("video/mp4"), Some("video"));
        assert_eq!(family_of("audio/flac"), Some("audio"));
        assert_eq!(family_of("text/x-rust"), Some("text"));
        // A PDF reader and an archive manager share a prefix and nothing else.
        assert_eq!(family_of("application/pdf"), None);
        assert_eq!(family_of("inode/directory"), None);
    }

    /// The tail is long; unsorted it cannot be scanned for a name.
    #[test]
    fn all_applications_are_listed_alphabetically() {
        let found = candidates("video/mp4");
        let tail: Vec<String> = found
            .iter()
            .filter(|c| c.section == Section::Installed)
            .map(|c| c.app.display_name().to_lowercase())
            .collect();
        let mut sorted = tail.clone();
        sorted.sort();
        assert_eq!(tail, sorted, "the installed applications are not in name order");
    }

    /// Offering to open a file with the file manager it was chosen in is a
    /// dead end.
    #[test]
    fn cadrocfile_does_not_offer_itself() {
        let found = candidates("video/mp4");
        assert!(
            !found.iter().any(|c| c.app.id().as_deref() == Some(SELF_ID)),
            "Cadrocfile listed itself",
        );
    }

    /// Sections must stay grouped, or the headings interleave with the rows.
    #[test]
    fn the_list_is_grouped_by_section() {
        let found = candidates("text/plain");
        let sections: Vec<Section> = found.iter().map(|c| c.section).collect();
        let mut sorted = sections.clone();
        sorted.sort();
        assert_eq!(sections, sorted, "sections are interleaved");
    }

    /// Search has to find an application by its name whatever the casing.
    #[test]
    fn search_matches_names_case_insensitively() {
        let found = candidates("text/plain");
        let Some(first) = found.first() else { return };
        let name = first.app.display_name().to_string();
        assert!(first.haystack.contains(&name.to_lowercase()), "{:?}", first.haystack);
    }
}
