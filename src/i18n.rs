//! Internationalisation via gettext.
//!
//! Translations live in `po/<lang>.po` and are compiled to `po/<lang>.mo` at
//! build time (see `build.rs`). The domain is `cadrocfile`.
//!
//! Use the [`tr!`] macro for user-facing strings:
//!
//! ```ignore
//! tr!("Open")
//! tr!("Delete {count} items", count = items.len())
//! ```

use std::sync::OnceLock;

static INITIALISED: OnceLock<()> = OnceLock::new();

/// Initializes gettext with the system locale.
///
/// Called once from `main` before any UI is built. Safe to call again.
pub fn init() {
    INITIALISED.get_or_init(|| {
        let domain = "cadrocfile";
        gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");
        gettextrs::textdomain(domain).expect("textdomain");
        gettextrs::bindtextdomain(domain, "/usr/share/locale").expect("bindtextdomain");
        gettextrs::bind_textdomain_codeset(domain, "UTF-8").expect("bind_textdomain_codeset");
        gettextrs::textdomain(domain).expect("textdomain");
    });
}

/// Translates a message using the current locale.
///
/// Falls back to the original string when no translation is found.
pub fn translate(message: &str) -> String {
    gettextrs::dgettext("cadrocfile", message)
}

/// Translates a message with a singular/plural form.
///
/// `n` selects the plural form. Falls back to the original strings.
pub fn translate_plural(singular: &str, plural: &str, n: u64) -> String {
    gettextrs::dngettext("cadrocfile", singular, plural, n as u32)
}

/// Macro for translating a string at the call site.
///
/// Returns `&'static str` — the translated string is leaked, which is fine for
/// UI strings that live for the process duration.
///
/// ```ignore
/// tr!("Open")
/// tr!("Delete {count} items", count = items.len())
/// ```
#[macro_export]
macro_rules! tr {
    ($message:expr) => {{
        let translated = $crate::i18n::translate($message);
        Box::leak(translated.into_boxed_str()) as &'static str
    }};
    ($message:expr, $($key:ident = $value:expr),+ $(,)?) => {{
        let mut translated = $crate::i18n::translate($message);
        $(
            translated = translated.replace(&format!("{{{}}}", stringify!($key)), &format!("{}", $value));
        )+
        Box::leak(translated.into_boxed_str()) as &'static str
    }};
}

/// Macro for translating a singular/plural string.
///
/// ```ignore
/// tr_plural!("One file", "{count} files", count = n)
/// ```
#[macro_export]
macro_rules! tr_plural {
    ($singular:expr, $plural:expr, $($key:ident = $value:expr),+ $(,)?) => {{
        let n: u64 = $($value)+;
        let mut translated = $crate::i18n::translate_plural($singular, $plural, n);
        $(
            translated = translated.replace(&format!("{{{}}}", stringify!($key)), &format!("{}", $value));
        )+
        Box::leak(translated.into_boxed_str()) as &'static str
    }};
}
