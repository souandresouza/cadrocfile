//! Back/forward navigation history.
//!
//! Behaves like a browser: navigating somewhere new after going back discards
//! the forward entries rather than branching.

use std::path::{Path, PathBuf};

/// Cap on remembered locations, so a long session doesn't grow without bound.
const MAX_ENTRIES: usize = 128;

#[derive(Debug)]
pub struct History {
    entries: Vec<PathBuf>,
    /// Index of the current location within `entries`.
    index: usize,
}

impl History {
    pub fn new(start: PathBuf) -> Self {
        Self { entries: vec![start], index: 0 }
    }

    pub fn current(&self) -> &Path {
        &self.entries[self.index]
    }

    /// Records a new location. Navigating to where we already are is ignored so
    /// that a refresh doesn't fill the history with duplicates.
    pub fn push(&mut self, path: PathBuf) {
        if self.entries[self.index] == path {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(path);

        if self.entries.len() > MAX_ENTRIES {
            // Drop the oldest half rather than one entry at a time, so this
            // memmove happens rarely instead of on every navigation.
            let drop_count = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..drop_count);
        }
        self.index = self.entries.len() - 1;
    }

    /// The location `go_back` would move to, for labelling the button.
    pub fn previous(&self) -> &Path {
        if self.can_go_back() { &self.entries[self.index - 1] } else { self.current() }
    }

    /// The location `go_forward` would move to.
    pub fn next(&self) -> &Path {
        if self.can_go_forward() { &self.entries[self.index + 1] } else { self.current() }
    }

    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn go_back(&mut self) -> Option<PathBuf> {
        if !self.can_go_back() {
            return None;
        }
        self.index -= 1;
        Some(self.entries[self.index].clone())
    }

    pub fn go_forward(&mut self) -> Option<PathBuf> {
        if !self.can_go_forward() {
            return None;
        }
        self.index += 1;
        Some(self.entries[self.index].clone())
    }

    /// Rewrites history when a location disappears (an unmounted drive, a
    /// deleted folder), so back/forward can't strand the user on a dead path.
    pub fn forget(&mut self, gone: &Path) {
        let current = self.entries[self.index].clone();
        self.entries.retain(|p| !p.starts_with(gone));
        if self.entries.is_empty() {
            let fallback = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            self.entries.push(fallback);
            self.index = 0;
            return;
        }
        self.index = self
            .entries
            .iter()
            .position(|p| *p == current)
            .unwrap_or(self.entries.len() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_entries_are_dropped_on_a_new_branch() {
        let mut h = History::new(PathBuf::from("/a"));
        h.push(PathBuf::from("/b"));
        h.push(PathBuf::from("/c"));
        assert_eq!(h.go_back().unwrap(), PathBuf::from("/b"));
        assert!(h.can_go_forward());

        h.push(PathBuf::from("/d"));
        assert!(!h.can_go_forward());
        assert_eq!(h.current(), Path::new("/d"));
        assert_eq!(h.go_back().unwrap(), PathBuf::from("/b"));
    }

    #[test]
    fn repeating_the_current_location_is_not_recorded() {
        let mut h = History::new(PathBuf::from("/a"));
        h.push(PathBuf::from("/a"));
        assert!(!h.can_go_back());
    }

    #[test]
    fn forgetting_a_subtree_keeps_navigation_valid() {
        let mut h = History::new(PathBuf::from("/home"));
        h.push(PathBuf::from("/media/usb"));
        h.push(PathBuf::from("/media/usb/photos"));
        h.forget(Path::new("/media/usb"));
        assert_eq!(h.current(), Path::new("/home"));
        assert!(!h.can_go_forward());
    }
}
