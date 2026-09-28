//! Modal dialogs, written as async functions.
//!
//! Every one of these returns a future that resolves to the user's choice, so
//! call sites read as straight-line code (`if confirm(...).await { ... }`)
//! rather than as a web of response callbacks.
use crate::tr;

use std::{cell::RefCell, path::{Path, PathBuf}, rc::Rc};

use adw::prelude::*;
use gtk::glib;

use crate::{
    archive::CompressFormat,
    drives::Volume,
    fs::ops::ConflictChoice,
};

/// Asks a yes/no question. `destructive` styles the accept button in red.
pub async fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    accept_label: &str,
    destructive: bool,
) -> bool {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("accept", accept_label);
    dialog.set_response_appearance(
        "accept",
        if destructive {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.choose_future(Some(parent)).await == "accept"
}

/// Shows an error. Fire-and-forget: there is nothing to decide.
pub fn show_error(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("ok", tr!("OK"));
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present(Some(parent));
}

/// Prompts for a single line of text, returning `None` if cancelled.
///
/// `select_stem` selects only the part before the extension, which is what you
/// want when renaming `photo.jpg` and almost never want otherwise.
pub async fn prompt_text(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: Option<&str>,
    initial: &str,
    accept_label: &str,
    select_stem: bool,
) -> Option<String> {
    let entry = gtk::Entry::builder()
        .text(initial)
        .activates_default(true)
        .margin_top(8)
        .build();

    if select_stem {
        let stem_len = Path::new(initial)
            .file_stem()
            .map(|s| s.to_string_lossy().chars().count() as i32)
            .unwrap_or(-1);
        entry.select_region(0, stem_len);
    } else {
        entry.select_region(0, -1);
    }

    let dialog = adw::AlertDialog::new(Some(heading), body);
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("accept", accept_label);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    // An empty name can never be valid, so keep the accept button off until
    // there is something to accept.
    let dialog_for_entry = dialog.clone();
    entry.connect_changed(move |entry| {
        dialog_for_entry.set_response_enabled("accept", !entry.text().trim().is_empty());
    });
    dialog.set_response_enabled("accept", !initial.trim().is_empty());

    // The entry must have focus for the pre-selection to be visible.
    let entry_for_focus = entry.clone();
    glib::idle_add_local_once(move || {
        entry_for_focus.grab_focus_without_selecting();
    });

    let response = dialog.choose_future(Some(parent)).await;
    (response == "accept").then(|| entry.text().trim().to_string())
}

/// Asks what to do about an existing destination during a copy or move.
pub async fn resolve_conflict(
    parent: &impl IsA<gtk::Widget>,
    source: &Path,
    dest: &Path,
    apply_to_all_possible: bool,
) -> ConflictChoice {
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    let body = format!(
        "“{name}” already exists in {}.\n\nReplacing it overwrites the existing file permanently.",
        dest.parent().map(|p| p.display().to_string()).unwrap_or_default()
    );

    let dialog = adw::AlertDialog::new(Some("A file with that name already exists"), Some(&body));

    // Show both files' details so the choice is informed rather than a guess.
    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .build();
    details.append(&file_summary_row("Existing file", dest));
    details.append(&file_summary_row("New file", source));

    let apply_all = gtk::CheckButton::builder()
        .label("Apply this to all remaining conflicts")
        .margin_top(10)
        .build();
    if apply_to_all_possible {
        details.append(&apply_all);
    }
    dialog.set_extra_child(Some(&details));

    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("skip", "Skip");
    dialog.add_response("rename", "Keep Both");
    dialog.add_response("replace", tr!("Replace"));
    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");

    let response = dialog.choose_future(Some(parent)).await;
    let all = apply_to_all_possible && apply_all.is_active();

    match (response.as_str(), all) {
        ("skip", false) => ConflictChoice::Skip,
        ("skip", true) => ConflictChoice::SkipAll,
        ("rename", false) => ConflictChoice::Rename,
        ("rename", true) => ConflictChoice::RenameAll,
        ("replace", false) => ConflictChoice::Replace,
        ("replace", true) => ConflictChoice::ReplaceAll,
        _ => ConflictChoice::Cancel,
    }
}

fn file_summary_row(caption: &str, path: &Path) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    row.append(
        &gtk::Label::builder()
            .label(caption)
            .xalign(0.0)
            .css_classes(["caption", "heading"])
            .build(),
    );

    let detail = match std::fs::metadata(path) {
        Ok(md) => {
            let size = if md.is_dir() {
                "folder".to_string()
            } else {
                humansize::format_size(md.len(), humansize::DECIMAL)
            };
            let modified = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| chrono::Local.timestamp_opt(d.as_secs() as i64, 0).single())
                .map(|dt| dt.format("%d %b %Y %H:%M").to_string())
                .unwrap_or_default();
            format!("{size} · {modified}")
        }
        Err(_) => "unavailable".to_string(),
    };

    row.append(
        &gtk::Label::builder()
            .label(&detail)
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    row
}

/// Confirms a shred, spelling out that it is unrecoverable and — when the
/// storage makes overwriting unreliable — that the guarantee does not hold.
pub async fn confirm_shred(
    parent: &impl IsA<gtk::Widget>,
    paths: &[std::path::PathBuf],
    passes: u32,
    caveat: Option<(String, String)>,
) -> bool {
    let what = if paths.len() == 1 {
        format!(
            "“{}”",
            paths[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        )
    } else {
        format!("{} items", paths.len())
    };

    let body = format!(
        "{what} will be overwritten {passes} time{} and then deleted.\n\n\
         This cannot be undone, and the files will not go to the Trash.",
        if passes == 1 { "" } else { "s" }
    );

    let dialog = adw::AlertDialog::new(Some("Shred permanently?"), Some(&body));

    if let Some((fstype, reason)) = caveat {
        // Being quiet about this would be the wrong call: the user is choosing
        // shredding specifically for a guarantee that this storage cannot give.
        let banner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(10)
            .css_classes(["card", "shred-warning"])
            .build();
        banner.append(
            &gtk::Label::builder()
                .label(format!("Overwriting may not be effective on {fstype}"))
                .xalign(0.0)
                .wrap(true)
                .margin_start(12)
                .margin_end(12)
                .margin_top(10)
                .css_classes(["heading"])
                .build(),
        );
        banner.append(
            &gtk::Label::builder()
                .label(format!(
                    "This is a {reason}.\n\nThe files will still be deleted, but treat the \
                     overwrite as best-effort. Full-disk encryption is the reliable answer here."
                ))
                .xalign(0.0)
                .wrap(true)
                .margin_start(12)
                .margin_end(12)
                .margin_bottom(10)
                .css_classes(["caption"])
                .build(),
        );
        dialog.set_extra_child(Some(&banner));
    }

    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("shred", "Shred");
    dialog.set_response_appearance("shred", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.choose_future(Some(parent)).await == "shred"
}

/// What the user chose to do about an NTFS volume Windows left dirty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NtfsChoice {
    ReadOnly,
    Repair,
    Force,
    Cancel,
}

/// Offers the three real ways out of a refused NTFS mount.
///
/// Ordered by risk: read-only is always safe and is the default; repair clears
/// the dirty flag; force discards a hibernated Windows session.
pub async fn ntfs_recovery(
    parent: &impl IsA<gtk::Widget>,
    volume: &Volume,
    error: &str,
    hibernated: bool,
    ntfsfix_available: bool,
) -> NtfsChoice {
    let cause = if hibernated {
        "Windows is hibernated or was shut down with Fast Startup enabled, so it still \
         considers this volume in use."
    } else {
        "Windows did not unmount this volume cleanly, so it is marked as needing a check."
    };

    let body = format!(
        "“{}” could not be mounted for writing.\n\n{cause}\n\nWhat would you like to do?",
        volume.label
    );

    let dialog = adw::AlertDialog::new(Some("This NTFS volume is not clean"), Some(&body));

    let details = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(8)
        .build();

    details.append(&option_explainer(
        "Open read-only",
        "Always safe. You can browse and copy files off the volume, but not change anything.",
    ));
    if ntfsfix_available {
        details.append(&option_explainer(
            "Repair and mount",
            "Fixes this for good: clears the dirty flag with ntfsfix and asks Windows to check \
             the volume on its next boot. Your files are not touched. Requires administrator \
             access.",
        ));
    } else {
        // Say why the good option is missing rather than quietly leaving it
        // out. Without this the dialog looks like read-only is all there is,
        // when a single package install would give the proper fix.
        details.append(&option_explainer(
            "Repair and mount — unavailable",
            "This clears the dirty flag properly, but it needs `ntfsfix`, which is not \
             installed. It comes with the ntfsprogs package.",
        ));
    }
    if hibernated {
        details.append(&option_explainer(
            "Force read-write",
            "Deletes the Windows hibernation file. Your files are untouched, but the suspended \
             Windows session cannot be resumed afterwards.",
        ));
    }

    // Surface the underlying driver message rather than hiding it — it is often
    // the only clue when none of the three options work.
    let expander = gtk::Expander::builder().label("Technical details").margin_top(6).build();
    expander.set_child(Some(
        &gtk::Label::builder()
            .label(error)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["caption", "dim-label", "monospace"])
            .build(),
    ));
    details.append(&expander);

    if !ntfsfix_available {
        details.append(
            &gtk::Label::builder()
                .label("Install the ntfsprogs package to enable repair.")
                .xalign(0.0)
                .wrap(true)
                .margin_top(4)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }

    dialog.set_extra_child(Some(&details));

    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("readonly", "Open Read-Only");
    if ntfsfix_available {
        dialog.add_response("repair", "Repair and Mount");
    }
    if hibernated {
        dialog.add_response("force", "Force Read-Write");
        dialog.set_response_appearance("force", adw::ResponseAppearance::Destructive);
    }
    // Repair is the only option that ends the problem rather than working
    // around it: it clears the dirty flag, so the volume mounts clean and on
    // the faster in-kernel driver from then on. Read-only leaves the drive
    // read-only and the same dialog waiting after every Windows boot. So Repair
    // leads when it is available — it still takes a deliberate click, and the
    // explainer above says what it does.
    if ntfsfix_available {
        dialog.set_response_appearance("repair", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("repair"));
    } else {
        dialog.set_response_appearance("readonly", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("readonly"));
    }
    dialog.set_close_response("cancel");

    match dialog.choose_future(Some(parent)).await.as_str() {
        "readonly" => NtfsChoice::ReadOnly,
        "repair" => NtfsChoice::Repair,
        "force" => NtfsChoice::Force,
        _ => NtfsChoice::Cancel,
    }
}

fn option_explainer(title: &str, body: &str) -> gtk::Box {
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    boxed.append(
        &gtk::Label::builder().label(title).xalign(0.0).css_classes(["heading", "caption"]).build(),
    );
    boxed.append(
        &gtk::Label::builder()
            .label(body)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    boxed
}

/// Prompts for an archive passphrase.
pub async fn ask_archive_password(
    parent: &impl IsA<gtk::Widget>,
    archive_name: &str,
) -> Option<String> {
    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .margin_top(8)
        .build();

    let dialog = adw::AlertDialog::new(
        Some("This archive is encrypted"),
        Some(&format!("Enter the password for “{archive_name}”.")),
    );
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("accept", tr!("Extract"));
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    let entry_for_focus = entry.clone();
    glib::idle_add_local_once(move || {
        entry_for_focus.grab_focus();
    });

    let response = dialog.choose_future(Some(parent)).await;
    (response == "accept").then(|| entry.text().to_string()).filter(|t| !t.is_empty())
}

/// Collects a URL, the name to save it as, and where to put it.
///
/// Built as a real dialog rather than an `AdwAlertDialog`: an alert is sized
/// for a sentence and a pair of buttons, and squeezing three fields and a
/// folder picker into one made every row too narrow to read a URL in.
///
/// The URL is probed as it is typed, so the suggested filename and the size
/// come from the server rather than from guessing at the address — and so a
/// wrong URL is caught here, before any job starts.
pub async fn ask_download(
    parent: &impl IsA<gtk::Widget>,
    start_dir: &Path,
) -> Option<(crate::fs::download::Probe, PathBuf)> {
    let url = gtk::Entry::builder()
        .placeholder_text("https://example.com/file.zip")
        .activates_default(true)
        .hexpand(true)
        .build();

    let name = gtk::Entry::builder()
        .placeholder_text("Saved as…")
        .activates_default(true)
        .hexpand(true)
        .sensitive(false)
        .build();

    let folder = Rc::new(RefCell::new(start_dir.to_path_buf()));
    let folder_button = gtk::Button::builder()
        .child(&folder_button_content(start_dir))
        .tooltip_text("Choose a different folder")
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .build();

    let status = gtk::Label::builder()
        .label("Paste a link to check it")
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(52)
        .css_classes(["caption", "dim-label"])
        .build();

    let group = adw::PreferencesGroup::new();
    let url_row = adw::ActionRow::builder().title("Address").build();
    url_row.add_suffix(&url);
    let name_row = adw::ActionRow::builder().title("Save as").build();
    name_row.add_suffix(&name);
    let folder_row = adw::ActionRow::builder().title("Into").build();
    folder_row.add_suffix(&folder_button);
    group.add(&url_row);
    group.add(&name_row);
    group.add(&folder_row);

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_top(8)
        .build();
    let cancel = gtk::Button::with_label(tr!("Cancel"));
    let accept = gtk::Button::builder()
        .label(tr!("Download"))
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    buttons.append(&cancel);
    buttons.append(&accept);

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&group);
    body.append(&status);
    body.append(&buttons);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));

    let dialog = adw::Dialog::builder()
        .title("Download a file")
        .content_width(560)
        .child(&toolbar)
        .build();

    let probed: Rc<RefCell<Option<crate::fs::download::Probe>>> = Rc::new(RefCell::new(None));
    let generation = Rc::new(std::cell::Cell::new(0u64));

    // Folder picker.
    {
        let folder = Rc::clone(&folder);
        let dialog = dialog.clone();
        let folder_button2 = folder_button.clone();
        folder_button.connect_clicked(move |_| {
            let (folder, dialog, button) =
                (Rc::clone(&folder), dialog.clone(), folder_button2.clone());
            glib::spawn_future_local(async move {
                let chooser = gtk::FileDialog::builder().title("Download into").build();
                let current = gio::File::for_path(folder.borrow().clone());
                chooser.set_initial_folder(Some(&current));
                let root = dialog.root().and_downcast::<gtk::Window>();
                if let Ok(picked) = chooser.select_folder_future(root.as_ref()).await
                    && let Some(path) = picked.path()
                {
                    button.set_child(Some(&folder_button_content(&path)));
                    *folder.borrow_mut() = path;
                }
            });
        });
    }

    // Probe as the address is typed.
    {
        let (name, status, accept) = (name.clone(), status.clone(), accept.clone());
        let probed = Rc::clone(&probed);
        let generation = Rc::clone(&generation);

        url.connect_changed(move |entry| {
            let text = entry.text().to_string();
            let this_generation = generation.get() + 1;
            generation.set(this_generation);
            probed.replace(None);
            accept.set_sensitive(false);
            name.set_sensitive(false);

            if text.trim().is_empty() {
                status.set_label("Paste a link to check it");
                status.remove_css_class("error");
                return;
            }
            status.set_label("Checking…");
            status.remove_css_class("error");

            let (name, status, accept) = (name.clone(), status.clone(), accept.clone());
            let (probed, generation) = (Rc::clone(&probed), Rc::clone(&generation));
            glib::spawn_future_local(async move {
                // Probing is a blocking network round-trip; keep it off the
                // main loop so typing stays responsive.
                let result = crate::ui::actions::run_off_thread(move || {
                    crate::fs::download::probe(&text)
                })
                .await;

                if generation.get() != this_generation {
                    return;
                }
                match result {
                    Ok(probe) => {
                        name.set_text(&probe.filename);
                        name.set_sensitive(true);
                        status.remove_css_class("error");
                        status.set_label(&describe(&probe));
                        probed.replace(Some(probe));
                        accept.set_sensitive(true);
                    }
                    Err(message) => {
                        status.set_label(&message);
                        status.add_css_class("error");
                    }
                }
            });
        });
    }

    let (tx, rx) = async_channel::bounded::<bool>(1);
    {
        let tx = tx.clone();
        let dialog = dialog.clone();
        accept.connect_clicked(move |_| {
            let _ = tx.send_blocking(true);
            dialog.close();
        });
    }
    {
        let tx = tx.clone();
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            let _ = tx.send_blocking(false);
            dialog.close();
        });
    }
    // Closing with Escape or the titlebar counts as cancelling.
    dialog.connect_closed(move |_| {
        let _ = tx.try_send(false);
    });

    let url_for_focus = url.clone();
    glib::idle_add_local_once(move || {
        url_for_focus.grab_focus();
    });

    dialog.present(Some(parent));
    if !rx.recv().await.unwrap_or(false) {
        return None;
    }

    let mut probe = probed.borrow().clone()?;
    // The user may have renamed it; their choice wins, still sanitised because
    // it ends up as a path.
    let chosen = name.text().to_string();
    if !chosen.trim().is_empty() {
        probe.filename = crate::fs::download::sanitize_filename(&chosen);
    }
    let destination = folder.borrow().clone();
    Some((probe, destination))
}

/// Folder name over its path, so the button says both where it is going and
/// exactly where that is.
fn folder_button_content(path: &Path) -> gtk::Box {
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    boxed.append(&gtk::Label::builder().label(&name).xalign(1.0).build());
    boxed.append(
        &gtk::Label::builder()
            .label(path.to_string_lossy())
            .xalign(1.0)
            .ellipsize(pango::EllipsizeMode::Start)
            .max_width_chars(34)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    boxed
}

fn describe(probe: &crate::fs::download::Probe) -> String {
    let transfer = match probe.size {
        Some(size) => format!(
            "{}{}",
            humansize::format_size(size, humansize::DECIMAL),
            if probe.supports_ranges {
                " · will be split across several connections"
            } else {
                " · single connection"
            }
        ),
        None => "Size unknown · single connection".to_string(),
    };

    // A share link usually points at a viewer page rather than the file, so the
    // address that will actually be fetched is often not the one that was
    // pasted. Showing it makes a wrong guess correctable instead of mysterious.
    if probe.was_redirected() {
        format!("{transfer}\nFetching from {}", probe.url)
    } else {
        transfer
    }
}

/// Asks for a LUKS passphrase.
///
/// `retry` switches the wording after a rejected attempt, so a mistyped
/// passphrase reads as a mistake rather than a failure of the drive. Nothing is
/// written to the volume either way — a wrong passphrase costs only the
/// attempt.
pub async fn ask_passphrase(
    parent: &impl IsA<gtk::Widget>,
    volume_label: &str,
    retry: bool,
) -> Option<String> {
    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .margin_top(8)
        .build();

    let body = if retry {
        format!("That passphrase did not work. Try again for “{volume_label}”.")
    } else {
        format!("Enter the passphrase to unlock “{volume_label}”.")
    };

    let dialog = adw::AlertDialog::new(Some("This drive is encrypted"), Some(&body));
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("accept", tr!("Unlock"));
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    let entry_for_focus = entry.clone();
    glib::idle_add_local_once(move || {
        entry_for_focus.grab_focus();
    });

    let response = dialog.choose_future(Some(parent)).await;
    (response == "accept").then(|| entry.text().to_string()).filter(|t| !t.is_empty())
}

/// Asks for an archive name and format when compressing a selection.
pub async fn ask_compress(
    parent: &impl IsA<gtk::Widget>,
    default_stem: &str,
) -> Option<(String, CompressFormat)> {
    let entry = gtk::Entry::builder().text(default_stem).activates_default(true).build();

    let formats = gtk::DropDown::from_strings(
        &CompressFormat::ALL.iter().map(|f| f.label()).collect::<Vec<_>>(),
    );

    let grid = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(8)
        .build();
    grid.append(&entry);
    grid.append(&formats);

    let dialog = adw::AlertDialog::new(Some(tr!("Compress")), Some("Choose a name and a format."));
    dialog.set_extra_child(Some(&grid));
    dialog.add_response("cancel", tr!("Cancel"));
    dialog.add_response("accept", tr!("Create"));
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    let response = dialog.choose_future(Some(parent)).await;
    if response != "accept" {
        return None;
    }

    let stem = entry.text().trim().to_string();
    if stem.is_empty() {
        return None;
    }
    let format = CompressFormat::ALL[formats.selected() as usize];
    Some((stem, format))
}

/// Summarises a finished job's failures without dumping a hundred lines.
pub fn show_job_errors(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    errors: &[(std::path::PathBuf, String)],
) {
    if errors.is_empty() {
        return;
    }

    const SHOWN: usize = 8;
    let mut body = String::new();
    for (path, error) in errors.iter().take(SHOWN) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        body.push_str(&format!("• {name}: {error}\n"));
    }
    if errors.len() > SHOWN {
        body.push_str(&format!("\n…and {} more.", errors.len() - SHOWN));
    }

    show_error(parent, heading, body.trim_end());
}

use chrono::TimeZone;
