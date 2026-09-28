//! Progress reporting for long-running file jobs.
//!
//! Each running job gets a card in a revealer strip along the bottom of the
//! window. The card owns the job's progress stream: it consumes messages,
//! updates itself, forwards conflicts to a dialog, and reports the outcome
//! back to the window when the stream ends.
use crate::tr;

use std::{
    cell::Cell,
    rc::Rc,
    time::Instant,
};

use gtk::{glib, prelude::*};

use crate::{
    fs::ops::{JobHandle, JobOutcome, Progress},
    ui::dialogs,
};

/// Minimum gap between progress-bar repaints.
///
/// Progress messages can arrive thousands of times a second on a fast NVMe
/// copy; repainting on each one would make the copy slower than the I/O.
const UI_INTERVAL_MS: u128 = 80;

pub struct JobMonitor {
    revealer: gtk::Revealer,
    list: gtk::Box,
    active: Cell<usize>,
}

impl JobMonitor {
    pub fn new() -> Rc<Self> {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();

        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .transition_duration(160)
            .reveal_child(false)
            .child(&list)
            .build();

        Rc::new(Self { revealer, list, active: Cell::new(0) })
    }

    pub fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    /// Takes ownership of a job, showing a card until it finishes.
    ///
    /// `on_finished` runs on the main context once the job's stream closes.
    pub fn run<F>(
        self: &Rc<Self>,
        job: JobHandle,
        parent: gtk::Widget,
        title: String,
        on_finished: F,
    ) where
        F: Fn(JobOutcome) + 'static,
    {
        let card = JobCard::new(&title);
        self.list.append(card.widget());
        self.active.set(self.active.get() + 1);
        self.revealer.set_reveal_child(true);

        let cancel_handle = Rc::new(job);
        let handle_for_button = Rc::clone(&cancel_handle);
        card.cancel_button.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_label("Cancelling…");
            handle_for_button.cancel();
        });

        let monitor = Rc::clone(self);
        glib::spawn_future_local(async move {
            let mut totals = Totals::default();
            let started = Instant::now();
            let mut last_paint = Instant::now();

            while let Ok(message) = cancel_handle.progress.recv().await {
                match message {
                    Progress::Prepared { total_bytes, total_items } => {
                        totals.bytes = total_bytes;
                        totals.items = total_items;
                        card.set_preparing(false);
                    }
                    Progress::Item { name, done_items } => {
                        // The count is always taken; only the repaint is
                        // throttled. Deleting or shredding a folder emits one
                        // of these per file, so painting each one would have
                        // the UI, not the filesystem, setting the pace.
                        totals.done_items = done_items;
                        if last_paint.elapsed().as_millis() >= UI_INTERVAL_MS {
                            last_paint = Instant::now();
                            card.set_detail(&name);
                            card.set_fraction(totals.fraction());
                            card.set_status(&totals.status_line(started));
                        }
                    }
                    Progress::Bytes { done_bytes } => {
                        totals.done_bytes = done_bytes;
                        if last_paint.elapsed().as_millis() >= UI_INTERVAL_MS {
                            last_paint = Instant::now();
                            card.set_fraction(totals.fraction());
                            card.set_status(&totals.status_line(started));
                        }
                    }
                    Progress::Conflict { source, dest, reply } => {
                        let choice =
                            dialogs::resolve_conflict(&parent, &source, &dest, totals.items > 1).await;
                        // The worker is blocked waiting on this; a send failure
                        // only happens if it already gave up, which is fine.
                        let _ = reply.send(choice).await;
                    }
                    Progress::ItemFailed { path, error } => {
                        // The full list is reported once at the end; showing the
                        // latest failure here keeps a long, partly-failing job
                        // from looking like it is silently succeeding.
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        card.set_failure(&format!("{name}: {error}"));
                    }
                    Progress::Finished(outcome) => {
                        card.widget().unparent_from(&monitor.list);
                        let remaining = monitor.active.get().saturating_sub(1);
                        monitor.active.set(remaining);
                        if remaining == 0 {
                            monitor.revealer.set_reveal_child(false);
                        }
                        on_finished(outcome);
                        return;
                    }
                }
            }

            // The channel closed without a Finished message, which means the
            // worker thread died. Clean up so the card isn't orphaned.
            card.widget().unparent_from(&monitor.list);
            let remaining = monitor.active.get().saturating_sub(1);
            monitor.active.set(remaining);
            if remaining == 0 {
                monitor.revealer.set_reveal_child(false);
            }
        });
    }
}

#[derive(Default)]
struct Totals {
    bytes: u64,
    items: u64,
    done_bytes: u64,
    done_items: u64,
}

impl Totals {
    /// Progress as a 0..1 fraction, preferring bytes when they're known.
    ///
    /// Item counts jump unevenly when files differ wildly in size, so bytes
    /// give a far smoother bar whenever they're available.
    fn fraction(&self) -> f64 {
        if self.bytes > 0 {
            (self.done_bytes as f64 / self.bytes as f64).clamp(0.0, 1.0)
        } else if self.items > 0 {
            (self.done_items as f64 / self.items as f64).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    fn status_line(&self, started: Instant) -> String {
        let items = if self.items > 0 {
            format!("{} of {}", self.done_items.min(self.items), self.items)
        } else {
            self.done_items.to_string()
        };

        if self.bytes == 0 {
            return items;
        }

        let done = humansize::format_size(self.done_bytes, humansize::DECIMAL);
        let total = humansize::format_size(self.bytes, humansize::DECIMAL);

        let elapsed = started.elapsed().as_secs_f64();
        // Wait for a second of data before quoting a rate; the first samples
        // are dominated by startup and produce absurd numbers.
        if elapsed < 1.0 || self.done_bytes == 0 {
            return format!("{items} · {done} of {total}");
        }

        let rate = self.done_bytes as f64 / elapsed;
        let remaining = self.bytes.saturating_sub(self.done_bytes) as f64;
        let eta = remaining / rate;

        format!(
            "{items} · {done} of {total} · {}/s · {} left",
            humansize::format_size(rate as u64, humansize::DECIMAL),
            format_duration(eta),
        )
    }
}

fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "—".to_string();
    }
    let secs = seconds.round() as u64;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

struct JobCard {
    root: gtk::Box,
    detail: gtk::Label,
    status: gtk::Label,
    bar: gtk::ProgressBar,
    cancel_button: gtk::Button,
}

impl JobCard {
    fn new(title: &str) -> Self {
        let heading = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .css_classes(["heading"])
            .build();

        let detail = gtk::Label::builder()
            .label(tr!("Preparing…"))
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .css_classes(["caption", "dim-label"])
            .build();

        let status = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["caption", "dim-label", "numeric"])
            .build();

        let bar = gtk::ProgressBar::builder().hexpand(true).build();
        // Until totals are known the job is genuinely indeterminate; pulsing
        // says "working" without inventing a percentage.
        bar.pulse();

        let cancel_button = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text(tr!("Cancel"))
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();

        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        text.append(&heading);
        text.append(&detail);
        text.append(&bar);
        text.append(&status);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["card", "job-card"])
            .build();
        root.set_margin_top(4);
        root.append(&text);
        root.append(&cancel_button);

        Self { root, detail, status, bar, cancel_button }
    }

    fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn set_preparing(&self, preparing: bool) {
        if !preparing {
            self.detail.set_text("");
        }
    }

    fn set_detail(&self, text: &str) {
        self.detail.set_text(text);
    }

    fn set_failure(&self, text: &str) {
        self.detail.set_text(text);
        self.detail.add_css_class("error");
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
    }

    fn set_fraction(&self, fraction: f64) {
        self.bar.set_fraction(fraction);
    }
}

/// Removing a child needs the parent, and `gtk::Box::remove` is the only way to
/// do it; this keeps the call sites readable.
trait UnparentFrom {
    fn unparent_from(&self, parent: &gtk::Box);
}

impl UnparentFrom for gtk::Box {
    fn unparent_from(&self, parent: &gtk::Box) {
        if self.parent().as_ref() == Some(parent.upcast_ref::<gtk::Widget>()) {
            parent.remove(self);
        }
    }
}
