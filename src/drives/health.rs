//! Whether a mount point that claims to exist actually works.
//!
//! UDisks2 reports `MountPoints` from the kernel's mount table, and the mount
//! table is not a statement that the filesystem is usable. Yank a drive while a
//! process still holds a file on it and the mount stays registered with nothing
//! behind it: the entry is there, the device is gone, and every access returns
//! `ENOTCONN`. UDisks2 keeps reporting the path, so anything that trusts
//! `MountPoints` concludes the drive is mounted and refuses to mount it again —
//! which is exactly how a drive becomes impossible to remount without dropping
//! to a shell.
//!
//! Asking the kernel two cheap questions — is it still in the table, and does
//! touching it work — separates "mounted" from "registered but dead", and a
//! dead mount can then be cleared instead of believed.

use std::path::{Path, PathBuf};

/// Where the kernel records every mount, including ones whose device is gone.
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// What a reported mount point is really doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Registered and usable.
    Live,
    /// Registered but unusable: the aftermath of an unclean disconnect.
    Dead,
    /// Not mounted at all.
    Absent,
}

/// Reads `/proc/self/mountinfo` and reports what `path` is.
pub fn health(path: &Path) -> Health {
    let table = std::fs::read_to_string(MOUNTINFO).unwrap_or_default();
    health_in(&table, path, probe)
}

/// The testable core: mount table plus a way to poke the filesystem.
fn health_in(table: &str, path: &Path, probe: impl Fn(&Path) -> bool) -> Health {
    if !is_mount_point(table, path) {
        return Health::Absent;
    }
    if probe(path) { Health::Live } else { Health::Dead }
}

/// Whether `path` appears as a mount point in a mountinfo table.
fn is_mount_point(table: &str, path: &Path) -> bool {
    mount_points(table).any(|point| point == path)
}

/// Every mount point in the table, with mountinfo's octal escapes undone.
///
/// Field 5 is the mount point. Fields are space separated and the kernel
/// escapes space, tab, newline and backslash inside paths, so a folder called
/// `My Drive` arrives as `My\040Drive` and would never match otherwise.
fn mount_points(table: &str) -> impl Iterator<Item = PathBuf> + '_ {
    table.lines().filter_map(|line| {
        let field = line.split(' ').nth(4)?;
        Some(PathBuf::from(unescape(field)))
    })
}

fn unescape(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        // Exactly three octal digits, or a literal backslash if it isn't one.
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(byte as char);
                for _ in 0..3 {
                    chars.next();
                }
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// Touches the mount point and reports whether it answered.
///
/// `metadata` alone is not enough. A dead FUSE mount fails it outright with
/// `ENOTCONN`, but a kernel filesystem whose device vanished can still serve
/// the cached root inode while every read fails — so the directory is actually
/// opened. Both calls return immediately on a broken mount; neither walks the
/// filesystem.
fn probe(path: &Path) -> bool {
    if std::fs::metadata(path).is_err() {
        return false;
    }
    match std::fs::read_dir(path) {
        Ok(mut entries) => match entries.next() {
            // An empty directory is a perfectly healthy answer.
            None => true,
            Some(first) => first.is_ok(),
        },
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line per mount, in the kernel's format. Only field 5 matters here.
    const TABLE: &str = "\
25 30 0:22 / /proc rw,nosuid shared:5 - proc proc rw
26 30 0:23 / /run/media/ana/Backup rw,nosuid shared:6 - fuseblk /dev/sda1 rw
27 30 0:24 / /media/ana/My\\040Drive rw,nosuid shared:7 - fuseblk /dev/sdb1 rw
28 30 0:25 / /run/media/ana/Tab\\011bed rw shared:8 - ext4 /dev/sdc1 rw";

    #[test]
    fn a_path_in_the_table_is_a_mount_point() {
        assert!(is_mount_point(TABLE, Path::new("/proc")));
        assert!(is_mount_point(TABLE, Path::new("/run/media/ana/Backup")));
        assert!(!is_mount_point(TABLE, Path::new("/run/media/ana/Missing")));
        // A parent of a mount point is not itself one.
        assert!(!is_mount_point(TABLE, Path::new("/run/media/ana")));
    }

    /// Removable drives are routinely labelled with spaces, and the kernel
    /// escapes them — comparing the raw field would never match those drives,
    /// so every one of them would look unmounted.
    #[test]
    fn escaped_characters_in_mount_points_are_decoded() {
        assert!(is_mount_point(TABLE, Path::new("/media/ana/My Drive")));
        assert!(is_mount_point(TABLE, Path::new("/run/media/ana/Tab\tbed")));
        // The escape itself must not survive into the comparison.
        assert!(!is_mount_point(TABLE, Path::new("/media/ana/My\\040Drive")));
    }

    #[test]
    fn unescaping_leaves_ordinary_paths_alone() {
        assert_eq!(unescape("/run/media/ana/Backup"), "/run/media/ana/Backup");
        assert_eq!(unescape("/a\\040b"), "/a b");
        assert_eq!(unescape("/a\\134b"), "/a\\b");
        // A backslash that is not an escape is kept as written.
        assert_eq!(unescape("/a\\zb"), "/a\\zb");
    }

    /// The distinction the whole module exists for: a mount that is listed but
    /// does not answer is not a mount you can use.
    #[test]
    fn a_registered_mount_that_does_not_answer_is_dead_not_live() {
        let path = Path::new("/run/media/ana/Backup");
        assert_eq!(health_in(TABLE, path, |_| true), Health::Live);
        assert_eq!(health_in(TABLE, path, |_| false), Health::Dead);

        // Not in the table at all is a third, different answer: there is
        // nothing to clean up before mounting.
        let missing = Path::new("/run/media/ana/Missing");
        assert_eq!(health_in(TABLE, missing, |_| true), Health::Absent);
        assert_eq!(health_in(TABLE, missing, |_| false), Health::Absent);
    }

    #[test]
    fn real_mount_points_are_read_from_the_running_kernel() {
        // `/` is always mounted and always readable, which makes it the one
        // assertion that holds on any machine this runs on.
        assert_eq!(health(Path::new("/")), Health::Live);
        assert_eq!(health(Path::new("/nonexistent-mount-point-xyz")), Health::Absent);
    }
}
