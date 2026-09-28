//! A fast GTK4 file manager.

mod app;
mod archive;
mod config;
mod drives;
mod fs;
mod history;
mod i18n;
#[cfg(test)]
mod testing;
mod trace;
mod ui;

fn main() -> gtk::glib::ExitCode {
    i18n::init();
    app::run()
}
