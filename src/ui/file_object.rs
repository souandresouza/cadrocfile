//! GObject shell around [`FileEntry`] so list models can hold it.
//!
//! The entry is stored behind a `RefCell` and replaced wholesale on refresh,
//! which lets a re-scan reuse existing objects instead of rebuilding the model
//! and losing the selection.

use std::{cell::RefCell, cmp::Ordering};

use gtk::{glib, subclass::prelude::*};

use crate::{config::SortKey, fs::FileEntry};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FileObject {
        pub entry: RefCell<Option<FileEntry>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileObject {
        const NAME: &'static str = "CadrocfileFileObject";
        type Type = super::FileObject;
    }

    impl ObjectImpl for FileObject {}
}

glib::wrapper! {
    pub struct FileObject(ObjectSubclass<imp::FileObject>);
}

impl FileObject {
    pub fn new(entry: FileEntry) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().entry.replace(Some(entry));
        obj
    }

    /// Clones out the entry. Callers need an owned value because the `RefCell`
    /// borrow cannot outlive a list-item bind callback.
    pub fn entry(&self) -> FileEntry {
        self.imp()
            .entry
            .borrow()
            .clone()
            .expect("FileObject always holds an entry")
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.imp().entry.borrow().as_ref().map(|e| e.path.clone()).unwrap_or_default()
    }

    pub fn is_dir(&self) -> bool {
        self.imp().entry.borrow().as_ref().is_some_and(|e| e.is_dir)
    }

    pub fn display_name(&self) -> String {
        self.imp()
            .entry
            .borrow()
            .as_ref()
            .map(|e| e.display_name.clone())
            .unwrap_or_default()
    }

    /// Substring match against the precomputed lowercase name.
    ///
    /// Borrows rather than cloning: `entry()` would deep-copy six `String`s per
    /// item per keystroke.
    pub fn matches_search(&self, lowercase_query: &str) -> bool {
        self.imp()
            .entry
            .borrow()
            .as_ref()
            .is_some_and(|e| e.search_key.contains(lowercase_query))
    }

    /// Orders two objects without cloning either entry.
    ///
    /// The sorter is the hottest path in the app: sorting an 8,500-entry folder
    /// is on the order of 110,000 comparisons, and reaching the entries through
    /// [`Self::entry`] deep-copies a `PathBuf` and five `String`s on each side
    /// of every one of them — a quarter of a million allocations to answer
    /// questions that only ever read a name, a size or a flag.
    pub fn compare_to(
        &self,
        other: &Self,
        key: SortKey,
        descending: bool,
        dirs_first: bool,
    ) -> Ordering {
        // Two immutable borrows, so this is still sound when a sorter compares
        // an object with itself.
        let this = self.imp().entry.borrow();
        let that = other.imp().entry.borrow();
        match (this.as_ref(), that.as_ref()) {
            (Some(a), Some(b)) => a.compare(b, key, descending, dirs_first),
            _ => Ordering::Equal,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.imp().entry.borrow().as_ref().is_some_and(|e| e.is_hidden)
    }

    pub fn replace(&self, entry: FileEntry) {
        self.imp().entry.replace(Some(entry));
    }

    /// Whether this object already shows exactly what `entry` describes.
    ///
    /// Compares only what a row displays or acts on. A rescan produces a fresh
    /// `FileEntry` for every file whether or not it changed, so without this
    /// every row would be rebuilt — which is the flicker this exists to avoid.
    pub fn shows(&self, entry: &FileEntry) -> bool {
        let current = self.imp().entry.borrow();
        let Some(current) = current.as_ref() else { return false };
        current.size == entry.size
            && current.modified == entry.modified
            && current.is_dir == entry.is_dir
            && current.is_symlink == entry.is_symlink
            && current.symlink_target == entry.symlink_target
            && current.is_hidden == entry.is_hidden
            && current.display_name == entry.display_name
            && current.content_type == entry.content_type
            && current.can_read == entry.can_read
            && current.can_write == entry.can_write
            && current.can_execute == entry.can_execute
    }
}
