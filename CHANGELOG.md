# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Phones and cameras under **Portable devices**: Android over MTP, iPhone over
  AFC and cameras in PTP mode are mounted through gvfs and browsed as folders
- The installer pulls the matching gvfs backend for those devices
- Command palette on `Ctrl+K`: fuzzy search over every action, folder and open
  tab, with the most recently used commands first
- Open tabs, their history and the active tab are restored on the next launch
- Raw photo previews from the JPEG the camera embedded (CR2, NEF, ARW, ORF,
  RAF, DNG, RW2 and more), plus camera, lens, exposure and date in the preview
  caption and in **Properties**
- Photo sidecars (`.xmp`) follow their photo through rename, copy, move, trash
  and shred, including batch renames
- `F1` opens the keyboard shortcut list alongside `Ctrl+?`

## [0.4.1] - 2026-09-28

### Added

- Initial public release
- GTK4/libadwaita file manager with tabs, icon grid and details list
- File preview (images, PDFs, office documents, text, video/audio)
- Thumbnails with freedesktop cache
- Search by name or file contents
- Batch rename with preview and conflict detection
- Disk usage analysis
- UDisks2 drive management with NTFS dirty volume recovery
- Archive extraction and creation via libarchive
- Multi-connection downloads with progress
- Network shares (SMB, SFTP, FTP, WebDAV, NFS) via gvfs
- Cloud drives via rclone (Google Drive, Proton Drive, Icedrive, Dropbox, OneDrive, Nextcloud)
- Trash, permanent delete and shred
- Internationalization (pt-BR)
- Light/dark mode with custom accent colours
- Keyboard shortcuts for all common operations
