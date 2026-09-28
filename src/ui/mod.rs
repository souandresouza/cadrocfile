pub mod accent;
pub mod actions;
pub mod batch_rename;
pub mod connect;
pub mod dialogs;
pub mod file_object;
pub mod file_view;
pub mod menu;
pub mod open_with;
pub mod pathbar;
pub mod preview;
pub mod progress;
pub mod properties;
pub mod sidebar;
pub mod thumbnailers;
pub mod usage;
pub mod thumbs;
pub mod window;

/// Builds a signal handler that holds only a weak reference to the window state.
///
/// Widgets outlive nothing here — the window owns them and they own their
/// handlers — so capturing a strong `Rc` would form a cycle and leak the whole
/// window on close. Every handler upgrades first and quietly does nothing if
/// the window is already gone.
///
/// ```ignore
/// button.connect_clicked(handler!(this = &inner, |_| { this.go_back(); }));
/// ```
#[macro_export]
macro_rules! handler {
    ($this:ident = $inner:expr, |$($arg:pat_param),*| $body:block) => {{
        let weak = std::rc::Rc::downgrade($inner);
        move |$($arg),*| {
            let Some($this) = weak.upgrade() else { return };
            $body
        }
    }};
}
