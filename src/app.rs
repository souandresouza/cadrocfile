//! Application setup: the single `AdwApplication`, its command line, and the
//! stylesheet.
use crate::tr;

use std::path::PathBuf;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{config::Config, ui::window::Window};

pub const APP_ID: &str = "dev.cadrocfile.Files";

pub fn run() -> glib::ExitCode {
    // HANDLES_COMMAND_LINE so a second `cadrocfile <path>` opens a window in the
    // already-running instance instead of starting a whole second process.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    // `--help` and `--version` have to be answered without a display: they are
    // the first thing a packaging script or a person on a broken install runs,
    // and before this they simply opened a window and hung.
    app.add_main_option(
        "version",
        glib::Char::from(b'V'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Print the version and exit",
        None,
    );
    app.connect_handle_local_options(|_, options| {
        use std::ops::ControlFlow;
        if options.lookup::<bool>("version").ok().flatten().unwrap_or(false) {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            // `Break` short-circuits with this exit code; `Continue` carries on
            // into the normal startup path.
            return ControlFlow::Break(glib::ExitCode::SUCCESS);
        }
        ControlFlow::Continue(())
    });

    crate::trace::init();

    app.connect_startup(|_| {
        // Names the icon the desktop entry installs, so the window carries it
        // in task switchers and docks as well as in launchers.
        gtk::Window::set_default_icon_name(APP_ID);
        load_stylesheet(&Config::load().accent_color);
        crate::trace::install_stall_detector();
    });

    app.connect_command_line(|app, command_line| {
        let start = starting_directory(command_line);
        let config = Config::load();
        // Every parallel job reads its thread count from here, so it has to be
        // applied before any of them can be started.
        crate::fs::parallel::set_override(config.worker_threads);
        let window = Window::new(app, config, start);
        window.present();

        // `CADROCFILE_SNAPSHOT=<path>` renders the window to a PNG and exits.
        //
        // A companion to `CADROCFILE_TRACE`: it draws the widget tree straight to
        // a texture, so a layout change can be checked without a compositor
        // screenshot — which on a tiling WM means hijacking whichever workspace
        // the user is on. Inert unless the variable is set.
        if let Ok(target) = std::env::var("CADROCFILE_SNAPSHOT") {
            let w = window.window.clone();
            // `CADROCFILE_SNAPSHOT_DELAY_MS` buys time to arrange the window first.
            let delay = std::env::var("CADROCFILE_SNAPSHOT_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2500u64);
            // `CADROCFILE_SNAPSHOT_ACTIONS=select-all,properties` fires window
            // actions before the shot, so dialogs can be captured too.
            let actions: Vec<String> = std::env::var("CADROCFILE_SNAPSHOT_ACTIONS")
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
                .collect();
            if !actions.is_empty() {
                let w = w.clone();
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(delay.saturating_sub(700)),
                    move || {
                        use gtk::prelude::*;
                        for action in actions {
                            let _ = WidgetExt::activate_action(&w, &format!("win.{action}"), None);
                        }
                    },
                );
            }

            glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
                use gtk::prelude::*;
                let paintable = gtk::WidgetPaintable::new(Some(&w));
                let snapshot = gtk::Snapshot::new();
                let (width, height) = (w.width(), w.height());
                paintable.snapshot(&snapshot, width as f64, height as f64);
                if let Some(node) = snapshot.to_node()
                    && let Some(renderer) = w.native().and_then(|n| n.renderer())
                {
                    let texture = renderer.render_texture(&node, None);
                    if let Err(e) = texture.save_to_png(&target) {
                        eprintln!("snapshot failed: {e}");
                    } else {
                        eprintln!("snapshot written to {target} ({width}x{height})");
                    }
                }
                std::process::exit(0);
            });
        }
        // The application owns the window from here; the `Rc` is intentionally
        // leaked so the widget tree outlives this callback.
        std::mem::forget(window);

        if crate::trace::enabled() {
            // Report counters periodically so a short profiling run still says
            // something useful.
            glib::timeout_add_local(std::time::Duration::from_secs(3), || {
                crate::trace::summary();
                glib::ControlFlow::Continue
            });
        }
        glib::ExitCode::SUCCESS
    });

    // `run` would otherwise try to parse our arguments as GTK options.
    app.run()
}

/// Resolves the directory to open from the command line, falling back to home.
fn starting_directory(command_line: &gio::ApplicationCommandLine) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));

    let arguments = command_line.arguments();
    let Some(first) = arguments.get(1) else { return home() };

    let raw = PathBuf::from(first);
    // A relative argument is relative to where the *client* was invoked, which
    // is not this process's cwd when the instance is being reused.
    let path = if raw.is_absolute() {
        raw
    } else {
        command_line.cwd().unwrap_or_else(home).join(raw)
    };

    if path.is_dir() {
        return path;
    }
    // Opening a file's folder is more useful than refusing outright.
    path.parent().filter(|p| p.is_dir()).map(|p| p.to_path_buf()).unwrap_or_else(home)
}

/// Installs the stylesheet with `accent` substituted in.
///
/// Calling it again replaces the previous provider, so changing the accent in
/// Settings restyles the running window without a restart.
pub fn load_stylesheet(accent: &str) {
    use std::cell::RefCell;
    thread_local! {
        /// Kept so the old provider can be removed; adding a second one would
        /// leave both in the cascade and make the winner depend on order.
        static CURRENT: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    }

    let Some(display) = gdk::Display::default() else { return };

    CURRENT.with(|current| {
        if let Some(old) = current.borrow_mut().take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&crate::ui::accent::apply(include_str!("ui/style.css"), accent));
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            // Above the theme so our rules win, below user overrides in
            // ~/.config/gtk-4.0/gtk.css so the user still has the last word.
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *current.borrow_mut() = Some(provider);
    });
}
