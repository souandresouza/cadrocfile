//! The plain-data model for one directory entry.
//!
//! Deliberately free of GObject machinery so it can be built on a worker thread
//! and moved across threads; `ui::file_object::FileObject` is the GObject shell
//! the list views bind to.

use std::{cell::RefCell, cmp::Ordering, collections::HashMap, path::{Path, PathBuf}};

use chrono::{DateTime, Local, TimeZone};
use humansize::{FormatSizeOptions, format_size};

use crate::config::SortKey;

/// The gio attributes a listing needs, requested in one batch.
///
/// Enumerating with a single attribute string means one `getdents` + one `statx`
/// per entry; asking for these lazily instead costs a syscall per field per row
/// and is what makes naive file managers crawl on large directories.
pub const QUERY_ATTRS: &str = "standard::name,standard::display-name,standard::type,\
standard::is-hidden,standard::is-backup,standard::is-symlink,standard::symlink-target,\
standard::size,time::modified,\
access::can-read,access::can-write,access::can-execute,unix::mode";

/// Content type for one entry, worked out from its name without touching disk.
///
/// `standard::content-type` is deliberately *not* in [`QUERY_ATTRS`]. gio will
/// fill it in during enumeration, but it costs a mime lookup for every file and
/// — for anything it cannot name from the filename alone — an open and a read
/// as well. Measured on an 8,463-file folder, that one attribute was 548 ms of
/// the 613 ms the whole listing took.
///
/// Almost every directory is a handful of name suffixes repeated, so the answer
/// is memoised and the second `.json` in a folder of 8,463 costs nothing.
pub(crate) fn guess_content_type(name: &str, is_dir: bool, size: u64, mode: u32) -> String {
    if is_dir {
        return "inode/directory".to_string();
    }

    thread_local! {
        /// Scanning happens on the main thread, so this needs no locking.
        static CACHE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }

    let (key, probe) = probe_for(name);
    let cached = CACHE.with(|cache| cache.borrow().get(&key).cloned());
    let guessed = match cached {
        Some(hit) => hit,
        None => {
            let guess =
                gio::functions::content_type_guess(Some(Path::new(&probe)), None).0.to_string();
            CACHE.with(|cache| cache.borrow_mut().insert(key, guess.clone()));
            guess
        }
    };

    if guessed != UNKNOWN_TYPE {
        return guessed;
    }

    // Nothing in the name said anything. gio would open the file and sniff it;
    // these two rules cover what that sniffing actually finds — executables were
    // 2,012 of the 2,443 disagreements across 7,498 files — with no I/O at all.
    if size == 0 {
        return "application/x-zerosize".to_string();
    }
    if mode & 0o111 != 0 {
        return "application/x-executable".to_string();
    }
    guessed
}

/// The cache key for `name`, and a filename that will match the same mime globs.
///
/// Reusing an answer across files is only sound when the glob that matched
/// depends on nothing but the shared part of the name. Mime globs come in two
/// shapes: `*.<suffix>`, which cares only about everything from the first dot
/// onwards, and literal or stem-anchored names like `Makefile`, `README*` and
/// `.gitignore`, which care about the whole thing.
///
/// So a dotted name is keyed on its full suffix and probed with a synthetic
/// stem — `libc.so.6` becomes `x.so.6`, which still matches `*.so.*`, where
/// keying on the last extension alone would have made it `x.6` and reported
/// every shared library on the system as a man page. Everything else is keyed
/// and probed as itself, which is exact; those names are few enough that the
/// cache stays small.
fn probe_for(name: &str) -> (String, String) {
    match name.char_indices().find(|&(i, c)| c == '.' && i > 0) {
        Some((dot, _)) => {
            let suffix = name[dot..].to_lowercase();
            (suffix.clone(), format!("x{suffix}"))
        }
        None => (name.to_lowercase(), name.to_string()),
    }
}

/// What `content_type_guess` returns when the name tells it nothing.
const UNKNOWN_TYPE: &str = "application/octet-stream";

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Raw basename — used for path building and exact matching.
    pub name: String,
    /// Possibly localised/decoded name — used for display.
    pub display_name: String,
    /// `display_name` lowercased once at scan time.
    ///
    /// The live filter runs over every entry on each keystroke; lowercasing
    /// there would allocate a `String` per item per keypress, which is what
    /// makes search stutter on a large folder.
    pub search_key: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub is_hidden: bool,
    pub size: u64,
    /// Modification time as a Unix timestamp in seconds.
    pub modified: Option<i64>,
    pub content_type: String,
    pub can_read: bool,
    pub can_write: bool,
    pub can_execute: bool,
    pub mode: u32,
}

impl FileEntry {
    /// Builds an entry from an enumerated `gio::FileInfo`.
    pub fn from_info(parent: &std::path::Path, info: &gio::FileInfo) -> Self {
        let name = info.name().to_string_lossy().into_owned();
        let display_name = info
            .display_name()
            .to_string();
        let is_dir = info.file_type() == gio::FileType::Directory;
        let name_for_type = name.clone();

        // `is_hidden` covers dotfiles; `is_backup` covers editor `foo~` files,
        // which Nautilus also folds into the hidden set.
        let is_hidden = info.is_hidden() || info.is_backup();

        let modified = info
            .modification_date_time()
            .map(|dt| dt.to_unix());

        Self {
            path: parent.join(&name),
            name,
            search_key: display_name.to_lowercase(),
            display_name,
            is_dir,
            is_symlink: info.is_symlink(),
            // Enumerating with NONE follows symlinks, so gio only fills in
            // symlink-target for entries that are links. Reading it
            // unconditionally trips a GLib critical on every plain file.
            symlink_target: info
                .has_attribute(gio::FILE_ATTRIBUTE_STANDARD_SYMLINK_TARGET)
                .then(|| info.symlink_target())
                .flatten()
                .map(|s| s.to_string_lossy().into_owned()),
            is_hidden,
            size: info.size().max(0) as u64,
            modified,
            content_type: guess_content_type(
                &name_for_type,
                is_dir,
                info.size().max(0) as u64,
                info.attribute_uint32(gio::FILE_ATTRIBUTE_UNIX_MODE),
            ),
            can_read: info
                .boolean(gio::FILE_ATTRIBUTE_ACCESS_CAN_READ),
            can_write: info
                .boolean(gio::FILE_ATTRIBUTE_ACCESS_CAN_WRITE),
            can_execute: info
                .boolean(gio::FILE_ATTRIBUTE_ACCESS_CAN_EXECUTE),
            mode: info.attribute_uint32(gio::FILE_ATTRIBUTE_UNIX_MODE),
        }
    }

    /// Human-readable type, e.g. "Folder" or "PNG image".
    pub fn kind_label(&self) -> String {
        if self.is_dir {
            return "Folder".to_string();
        }
        gio::functions::content_type_get_description(&self.content_type).to_string()
    }

    pub fn size_label(&self) -> String {
        if self.is_dir {
            // A directory's own inode size is meaningless to a user; the
            // recursive size is computed on demand in the properties dialog.
            return "—".to_string();
        }
        format_size(self.size, FormatSizeOptions::from(humansize::DECIMAL).decimal_places(1))
    }

    pub fn modified_label(&self) -> String {
        let Some(ts) = self.modified else {
            return "—".to_string();
        };
        let Some(dt) = Local.timestamp_opt(ts, 0).single() else {
            return "—".to_string();
        };
        let now = Local::now();
        if dt.date_naive() == now.date_naive() {
            format!("Today {}", dt.format("%H:%M"))
        } else if (now - dt).num_days() < 365 {
            dt.format("%d %b %H:%M").to_string()
        } else {
            dt.format("%d %b %Y").to_string()
        }
    }

    pub fn modified_datetime(&self) -> Option<DateTime<Local>> {
        self.modified.and_then(|ts| Local.timestamp_opt(ts, 0).single())
    }

    /// The lowercase extension without the dot, if any.
    pub fn extension(&self) -> Option<String> {
        self.path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
    }

    pub fn permissions_label(&self) -> String {
        // Render the low 12 bits the way `ls -l` does; the type nibble is
        // already conveyed by the icon and kind label.
        let m = self.mode;
        let bit = |mask: u32, ch: char| if m & mask != 0 { ch } else { '-' };
        format!(
            "{}{}{}{}{}{}{}{}{}",
            bit(0o400, 'r'), bit(0o200, 'w'), bit(0o100, 'x'),
            bit(0o040, 'r'), bit(0o020, 'w'), bit(0o010, 'x'),
            bit(0o004, 'r'), bit(0o002, 'w'), bit(0o001, 'x'),
        )
    }

    /// Orders two entries under the active sort, with the directories-first
    /// grouping applied *before* the key so that reversing the sort direction
    /// never interleaves folders into the file list.
    pub fn compare(&self, other: &Self, key: SortKey, descending: bool, dirs_first: bool) -> Ordering {
        if dirs_first && self.is_dir != other.is_dir {
            return if self.is_dir { Ordering::Less } else { Ordering::Greater };
        }

        let ord = match key {
            SortKey::Name => natural_cmp(&self.display_name, &other.display_name),
            SortKey::Size => self.size.cmp(&other.size),
            SortKey::Modified => self.modified.cmp(&other.modified),
            SortKey::Kind => self
                .kind_label()
                .to_lowercase()
                .cmp(&other.kind_label().to_lowercase()),
        };

        // Fall back to name so that equal keys (very common for Size and Kind)
        // produce a stable, predictable order instead of enumeration order.
        let ord = ord.then_with(|| natural_cmp(&self.display_name, &other.display_name));

        if descending { ord.reverse() } else { ord }
    }
}

/// Case-insensitive comparison that orders embedded digit runs numerically, so
/// `file2` sorts before `file10` the way a person expects.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();

    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ac), Some(bc)) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let an = take_number(&mut ai);
                    let bn = take_number(&mut bi);
                    // Compare by value; on a tie prefer fewer leading zeros so
                    // `01` and `1` still get a deterministic order.
                    match an.0.cmp(&bn.0).then(an.1.cmp(&bn.1)) {
                        Ordering::Equal => continue,
                        other => return other,
                    }
                } else {
                    let al = ac.to_lowercase().next().unwrap_or(ac);
                    let bl = bc.to_lowercase().next().unwrap_or(bc);
                    match al.cmp(&bl) {
                        Ordering::Equal => {
                            ai.next();
                            bi.next();
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

/// Consumes a run of digits, returning its numeric value and its text length.
///
/// The value is saturating: a 40-digit filename shouldn't overflow into a
/// nonsense ordering.
fn take_number(iter: &mut std::iter::Peekable<std::str::Chars<'_>>) -> (u128, usize) {
    let mut value: u128 = 0;
    let mut len = 0usize;
    while let Some(c) = iter.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        iter.next();
        len += 1;
        value = value.saturating_mul(10).saturating_add((c as u8 - b'0') as u128);
    }
    (value, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_sorts_digit_runs_by_value() {
        let mut names = vec!["file10", "file2", "File1", "file20"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["File1", "file2", "file10", "file20"]);
    }

    #[test]
    fn natural_order_is_case_insensitive() {
        assert_eq!(natural_cmp("Apple", "apple"), Ordering::Equal);
        assert_eq!(natural_cmp("apple", "Banana"), Ordering::Less);
    }
}
