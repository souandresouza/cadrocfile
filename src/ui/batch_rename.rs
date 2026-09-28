//! The rename dialog for several files at once.
//!
//! Every edit re-plans the whole batch and shows the result, so the user sees
//! each new name — and each clash — before anything happens. The Rename button
//! only unlocks for a batch with no problems in it; the planner in
//! [`crate::fs::batch_rename`] enforces the same rule again at execution.
use crate::tr;

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use adw::prelude::*;
use gtk::{glib, pango};

use crate::fs::batch_rename::{self, CaseMode, Planned, Rule, Status};

/// Rows drawn in the preview. Planning covers every file; drawing a row per
/// file for a batch of thousands on every keystroke would make typing lag.
const PREVIEW_ROWS: usize = 200;

pub async fn ask(parent: &impl IsA<gtk::Widget>, files: Vec<(PathBuf, bool)>) -> Option<Vec<Planned>> {
    let mode = gtk::DropDown::from_strings(&["Find and replace", "Number them", "Change case"]);
    mode.set_valign(gtk::Align::Center);

    let find = gtk::Entry::builder().placeholder_text("Text to find").hexpand(true).build();
    let with = gtk::Entry::builder().placeholder_text("Replace with").hexpand(true).build();
    let match_case = gtk::Switch::builder().valign(gtk::Align::Center).build();

    let pattern = gtk::Entry::builder()
        .text("File {n}")
        .tooltip_text("{n} is the number, {name} is the current name")
        .hexpand(true)
        .build();
    let start = gtk::SpinButton::with_range(0.0, 1_000_000.0, 1.0);
    start.set_value(1.0);
    start.set_valign(gtk::Align::Center);
    let pad = gtk::SpinButton::with_range(0.0, 8.0, 1.0);
    // Enough digits that the names sort in order in any file manager.
    pad.set_value(files.len().to_string().len().max(2) as f64);
    pad.set_valign(gtk::Align::Center);

    let case = gtk::DropDown::from_strings(&["lowercase", "UPPERCASE", "Title Case"]);
    case.set_valign(gtk::Align::Center);

    let keep_ext = gtk::Switch::builder().active(true).valign(gtk::Align::Center).build();

    let row = |title: &str, subtitle: Option<&str>, widget: &gtk::Widget| {
        let row = adw::ActionRow::builder().title(title).build();
        if let Some(subtitle) = subtitle {
            row.set_subtitle(subtitle);
        }
        row.add_suffix(widget);
        row
    };
    let mode_row = row("Rename by", None, mode.upcast_ref());
    let find_row = row(tr!("Find"), None, find.upcast_ref());
    let with_row = row("Replace with", None, with.upcast_ref());
    let case_match_row = row("Match case", None, match_case.upcast_ref());
    let pattern_row = row("Pattern", Some("{n} for the number, {name} for the current name"), pattern.upcast_ref());
    let start_row = row("Start at", None, start.upcast_ref());
    let pad_row = row("Digits", Some("Zero-padded so the names sort in order"), pad.upcast_ref());
    let case_row = row("Case", None, case.upcast_ref());
    let ext_row = row("Keep extensions", Some("Leave .jpg, .tar.gz and so on as they are"), keep_ext.upcast_ref());

    let group = adw::PreferencesGroup::new();
    for r in [&mode_row, &find_row, &with_row, &case_match_row, &pattern_row, &start_row, &pad_row, &case_row, &ext_row] {
        group.add(r);
    }

    let preview = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&preview)
        .min_content_height(220)
        .max_content_height(320)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let summary = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption", "dim-label"])
        .build();

    let cancel = gtk::Button::with_label(tr!("Cancel"));
    let accept = gtk::Button::builder()
        .label(tr!("Rename"))
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
    body.append(&group);
    body.append(&scroller);
    body.append(&summary);
    body.append(&buttons);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    let dialog = adw::Dialog::builder()
        .title(format!("Rename {} items", files.len()))
        .content_width(640)
        .child(&toolbar)
        .build();

    let current: Rc<RefCell<Vec<Planned>>> = Rc::new(RefCell::new(Vec::new()));

    let refresh = {
        let (mode, find, with, match_case, pattern, start, pad, case, keep_ext) = (
            mode.clone(),
            find.clone(),
            with.clone(),
            match_case.clone(),
            pattern.clone(),
            start.clone(),
            pad.clone(),
            case.clone(),
            keep_ext.clone(),
        );
        let rows = [
            (find_row.clone(), 0u32),
            (with_row.clone(), 0),
            (case_match_row.clone(), 0),
            (pattern_row.clone(), 1),
            (start_row.clone(), 1),
            (pad_row.clone(), 1),
            (case_row.clone(), 2),
        ];
        let (preview, summary, accept) = (preview.clone(), summary.clone(), accept.clone());
        let current = Rc::clone(&current);
        let files = files.clone();
        Rc::new(move || {
            let chosen = mode.selected();
            for (row, belongs) in &rows {
                row.set_visible(*belongs == chosen);
            }

            let rule = match chosen {
                0 => Rule::Replace {
                    find: find.text().to_string(),
                    with: with.text().to_string(),
                    match_case: match_case.is_active(),
                },
                1 => Rule::Template {
                    pattern: pattern.text().to_string(),
                    start: start.value() as u32,
                    pad: pad.value() as usize,
                },
                _ => Rule::Case(match case.selected() {
                    0 => CaseMode::Lower,
                    1 => CaseMode::Upper,
                    _ => CaseMode::Title,
                }),
            };
            let plan = batch_rename::plan(&files, &rule, keep_ext.is_active(), |p| {
                p.symlink_metadata().is_ok()
            });
            render(&preview, &plan);

            let renaming = plan.iter().filter(|p| p.status == Status::Ready).count();
            let problems = plan.iter().filter(|p| p.status.is_problem()).count();
            let unchanged = plan.iter().filter(|p| p.status == Status::Unchanged).count();
            let mut parts = Vec::new();
            if renaming > 0 {
                parts.push(format!("{renaming} will be renamed"));
            }
            if unchanged > 0 {
                parts.push(format!("{unchanged} unchanged"));
            }
            if problems > 0 {
                parts.push(format!(
                    "{problems} problem{} to fix first",
                    if problems == 1 { "" } else { "s" }
                ));
            }
            if parts.is_empty() || (renaming == 0 && problems == 0) {
                parts = vec![match chosen {
                    0 if find.text().is_empty() => "Type the text to find".to_string(),
                    _ => "Nothing would change".to_string(),
                }];
            }
            summary.set_label(&parts.join(" · "));
            if problems > 0 {
                summary.add_css_class("error");
            } else {
                summary.remove_css_class("error");
            }
            accept.set_sensitive(renaming > 0 && problems == 0);
            *current.borrow_mut() = plan;
        })
    };

    for entry in [&find, &with, &pattern] {
        let refresh = Rc::clone(&refresh);
        entry.connect_changed(move |_| refresh());
    }
    for switch in [&match_case, &keep_ext] {
        let refresh = Rc::clone(&refresh);
        switch.connect_active_notify(move |_| refresh());
    }
    for spin in [&start, &pad] {
        let refresh = Rc::clone(&refresh);
        spin.connect_value_changed(move |_| refresh());
    }
    for drop in [&mode, &case] {
        let refresh = Rc::clone(&refresh);
        drop.connect_selected_notify(move |_| refresh());
    }
    refresh();

    let (tx, rx) = async_channel::bounded::<bool>(1);
    {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        accept.connect_clicked(move |_| {
            let _ = tx.send_blocking(true);
            dialog.close();
        });
    }
    {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        cancel.connect_clicked(move |_| {
            let _ = tx.send_blocking(false);
            dialog.close();
        });
    }
    dialog.connect_closed(move |_| {
        let _ = tx.try_send(false);
    });

    let focus = find.clone();
    glib::idle_add_local_once(move || {
        focus.grab_focus();
    });
    dialog.present(Some(parent));

    if !rx.recv().await.unwrap_or(false) {
        return None;
    }
    let plan = current.borrow().clone();
    Some(plan)
}

/// Redraws the preview list. Problems are listed first, because a clash at
/// row 180 of 200 that the user has to scroll to find is a clash they miss.
fn render(list: &gtk::ListBox, plan: &[Planned]) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let mut ordered: Vec<&Planned> = plan.iter().filter(|p| p.status.is_problem()).collect();
    ordered.extend(plan.iter().filter(|p| !p.status.is_problem()));

    for item in ordered.iter().take(PREVIEW_ROWS) {
        let old = item.from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (subtitle, class) = match &item.status {
            Status::Unchanged => ("unchanged".to_string(), Some("dim-label")),
            Status::Ready => (String::new(), None),
            Status::Invalid(reason) => (format!("not allowed: {reason}"), Some("error")),
            Status::Duplicate => ("another file would get this name".to_string(), Some("error")),
            Status::Exists => ("a file with this name already exists".to_string(), Some("error")),
        };

        let row = gtk::Box::builder()
            .spacing(8)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(10)
            .margin_end(10)
            .build();
        let label = |text: &str| {
            gtk::Label::builder()
                .label(text)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build()
        };
        let before = label(&old);
        before.add_css_class("dim-label");
        row.append(&before);
        row.append(&gtk::Image::from_icon_name("go-next-symbolic"));

        let right = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        let after = label(&item.new_name());
        if let Some(class) = class {
            after.add_css_class(class);
        }
        right.append(&after);
        if !subtitle.is_empty() {
            let note = label(&subtitle);
            note.add_css_class("caption");
            if let Some(class) = class {
                note.add_css_class(class);
            }
            right.append(&note);
        }
        row.append(&right);
        list.append(&row);
    }

    if plan.len() > PREVIEW_ROWS {
        list.append(
            &gtk::Label::builder()
                .label(format!("…and {} more", plan.len() - PREVIEW_ROWS))
                .margin_top(6)
                .margin_bottom(6)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
    }
}
