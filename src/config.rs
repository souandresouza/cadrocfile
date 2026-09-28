//! Persisted user settings: theme, view state, favourites, window geometry.
//!
//! Written to `$XDG_CONFIG_HOME/cadrocfile/config.json`. Saving is debounced by the
//! caller (see `ui::window`) so that dragging a window edge doesn't spam the disk.
use crate::tr;

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Grid,
    List,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

impl SortKey {
    pub const ALL: [SortKey; 4] = [SortKey::Name, SortKey::Size, SortKey::Modified, SortKey::Kind];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => tr!("Name"),
            SortKey::Size => tr!("Size"),
            SortKey::Modified => tr!("Modified"),
            SortKey::Kind => tr!("Type"),
        }
    }
}

/// Icon edge lengths offered by the zoom control, in logical pixels.
///
/// The list is shared by both views: the grid uses the value directly, the list
/// clamps it (see `ui::file_view`) because a 128px row is unusable as a table.
pub const ICON_SIZES: [i32; 6] = [16, 24, 32, 48, 64, 96];
pub const DEFAULT_ICON_SIZE: i32 = 48;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: Theme,
    pub view_mode: ViewMode,
    pub icon_size: i32,
    pub show_hidden: bool,
    pub dirs_first: bool,
    pub sort_key: SortKey,
    pub sort_descending: bool,
    pub single_click_open: bool,
    /// Overwrite passes used by the shredder before the file is unlinked.
    pub shred_passes: u32,
    /// Threads long-running jobs may use. `0` sizes automatically, leaving
    /// about half the machine's cores free for everything else.
    pub worker_threads: u32,
    /// Accent colour as `#RRGGBB`, driving selection, focus and the meters.
    pub accent_color: String,
    /// UUIDs of NTFS volumes that only mount through `ntfs-3g`.
    ///
    /// Remembered so the next mount goes straight to the driver that worked.
    /// The first attempt on a dirty volume is a failed UDisks2 job, and other
    /// desktop components report those as errors even when the retry succeeds.
    pub ntfs_fuse_volumes: Vec<String>,
    pub confirm_trash: bool,
    pub show_thumbnails: bool,
    /// Files above this size are never thumbnailed (bytes).
    pub thumbnail_max_bytes: u64,
    pub favourites: Vec<PathBuf>,
    /// Network shares the user has saved, so they appear in the sidebar
    /// whether or not they are mounted right now. Credentials are never kept
    /// here: gvfs holds those in the login keyring.
    #[serde(default)]
    pub servers: Vec<crate::fs::remote::Server>,
    pub window_width: i32,
    pub window_height: i32,
    pub window_maximized: bool,
    pub sidebar_visible: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            view_mode: ViewMode::default(),
            icon_size: DEFAULT_ICON_SIZE,
            show_hidden: false,
            dirs_first: true,
            sort_key: SortKey::default(),
            sort_descending: false,
            single_click_open: false,
            shred_passes: 3,
            worker_threads: 0,
            accent_color: crate::ui::accent::DEFAULT.to_string(),
            ntfs_fuse_volumes: Vec::new(),
            confirm_trash: false,
            show_thumbnails: true,
            thumbnail_max_bytes: 32 * 1024 * 1024,
            favourites: Vec::new(),
            servers: Vec::new(),
            window_width: 1200,
            window_height: 760,
            window_maximized: false,
            sidebar_visible: true,
        }
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cadrocfile")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    /// Reads the config, falling back to defaults on any error.
    ///
    /// A corrupt or partially-written file must not stop the app from starting,
    /// so parse failures are logged and swallowed rather than propagated.
    pub fn load() -> Self {
        let path = Self::config_path();
        let Ok(text) = fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str::<Config>(&text) {
            Ok(mut cfg) => {
                cfg.sanitize();
                cfg
            }
            Err(err) => {
                eprintln!("cadrocfile: ignoring unreadable config at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Clamps values that would put the UI into an unusable state if a
    /// hand-edited config contained them.
    fn sanitize(&mut self) {
        if !ICON_SIZES.contains(&self.icon_size) {
            self.icon_size = DEFAULT_ICON_SIZE;
        }
        self.shred_passes = self.shred_passes.clamp(1, 35);
        self.worker_threads = self.worker_threads.min(64);
        self.accent_color = crate::ui::accent::normalise(&self.accent_color);
        self.ntfs_fuse_volumes.retain(|uuid| !uuid.is_empty());
        self.ntfs_fuse_volumes.sort();
        self.ntfs_fuse_volumes.dedup();
        self.window_width = self.window_width.max(480);
        self.window_height = self.window_height.max(360);
        self.favourites.retain(|p| p.is_absolute());
        self.favourites.dedup();
    }

    /// Writes atomically: a crash mid-write leaves the previous config intact
    /// rather than a truncated file that would reset every setting.
    pub fn save(&self) -> std::io::Result<()> {
        let dir = Self::config_dir();
        fs::create_dir_all(&dir)?;
        let final_path = Self::config_path();
        let tmp_path = dir.join("config.json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, &final_path)
    }

    pub fn is_favourite(&self, path: &Path) -> bool {
        self.favourites.iter().any(|p| p == path)
    }

    /// Returns true if the set actually changed, so callers can skip a save.
    pub fn toggle_favourite(&mut self, path: &Path) -> bool {
        if let Some(idx) = self.favourites.iter().position(|p| p == path) {
            self.favourites.remove(idx);
            true
        } else if path.is_dir() {
            self.favourites.push(path.to_path_buf());
            true
        } else {
            false
        }
    }

    /// Steps the icon size one notch along `ICON_SIZES`, saturating at the ends.
    pub fn zoom(&mut self, delta: i32) -> bool {
        let current = ICON_SIZES
            .iter()
            .position(|&s| s == self.icon_size)
            .unwrap_or(ICON_SIZES.len() / 2) as i32;
        let next = (current + delta).clamp(0, ICON_SIZES.len() as i32 - 1) as usize;
        if ICON_SIZES[next] == self.icon_size {
            return false;
        }
        self.icon_size = ICON_SIZES[next];
        true
    }
}
