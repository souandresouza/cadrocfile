//! Sidecar files that belong to a photo, and moving them with it.
//!
//! darktable (and a few other editors) keep a photo's edits in a file next to
//! the original rather than inside it: `IMG_1234.ORF.xmp` for
//! `IMG_1234.ORF`, and sometimes the older `IMG_1234.xmp`. A file manager that
//! renames, copies or trashes the photo without its sidecar quietly throws the
//! edits away, so every operation here treats the pair as one thing.
//!
//! Only sidecars that already exist are returned: a `.xmp` mentioned in a
//! rename must not be created out of nothing.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

/// The suffixes appended to the full file name, in the order darktable and
/// friends write them. Compared case-sensitively because that is how the
/// filesystem stores them; on a case-insensitive one the duplicate is filtered
/// out by `canonicalize`.
const APPENDED: [&str; 3] = [".xmp", ".XMP", ".Xmp"];

/// Every existing sidecar that belongs to `photo`.
///
/// Both layouts are recognised: `<name>.<ext>.xmp` (what darktable writes) and
/// `<name>.xmp` (what several raw converters and Adobe tools write). The photo
/// itself is never returned.
pub fn companions(photo: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();

    for suffix in APPENDED {
        let mut candidate = photo.as_os_str().to_owned();
        candidate.push(suffix);
        push_if_file(&mut found, PathBuf::from(candidate));
    }

    // `IMG_1234.ORF` -> `IMG_1234.xmp`, but not `photo` -> `photo.xmp` twice.
    if let Some(stem) = photo.file_stem() {
        let mut stem_sidecar = photo.with_file_name(stem);
        stem_sidecar.set_extension("xmp");
        if stem_sidecar != photo {
            push_if_file(&mut found, stem_sidecar);
        }
        let mut upper = photo.with_file_name(stem);
        upper.set_extension("XMP");
        if upper != photo {
            push_if_file(&mut found, upper);
        }
    }

    found
}

fn push_if_file(found: &mut Vec<PathBuf>, candidate: PathBuf) {
    if found.contains(&candidate) {
        return;
    }
    if fs::symlink_metadata(&candidate).is_ok_and(|m| !m.is_dir()) {
        found.push(candidate);
    }
}

/// Where a sidecar of `photo` should land when the photo itself becomes
/// `destination`, whatever naming layout the sidecar used.
///
/// The suffix is derived from names rather than from the sidecar's own path so
/// that a conflict rename (`photo (1).ORF`) carries `photo (1).ORF.xmp` along
/// with it instead of colliding with the original's sidecar.
pub fn destination_for(sidecar: &Path, photo: &Path, destination: &Path) -> PathBuf {
    if let Some(suffix) = appended_suffix(sidecar, photo) {
        let mut target = destination.as_os_str().to_owned();
        target.push(suffix);
        return PathBuf::from(target);
    }
    // Stem layout: keep the destination's stem and the sidecar's own
    // (upper-case) extension.
    let extension = sidecar.extension().map(OsString::from).unwrap_or_else(|| OsString::from("xmp"));
    let mut target = destination.to_path_buf();
    target.set_extension(extension);
    target
}

/// The literal text this sidecar appends to the photo's name, if it uses that
/// layout.
fn appended_suffix(sidecar: &Path, photo: &Path) -> Option<String> {
    let sidecar_name = sidecar.file_name()?.to_str()?;
    let photo_name = photo.file_name()?.to_str()?;
    sidecar_name.strip_prefix(photo_name).filter(|rest| !rest.is_empty()).map(str::to_owned)
}

/// `paths` plus every sidecar belonging to them, for operations that act on a
/// whole selection at once (trash, shred).
///
/// A sidecar that is already selected is not added twice, and a sidecar whose
/// photo is absent from the selection is left alone: the sidecar of a photo
/// someone deliberately kept must not be swept up.
pub fn with_companions(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for path in paths {
        out.push(path.clone());
        for companion in companions(path) {
            if !out.contains(&companion) {
                out.push(companion);
            }
        }
    }
    out
}

/// Renames the sidecars of `old` so they follow it to `new`.
///
/// Returns the sidecars that could not be moved, with the reason. The caller
/// has already renamed the photo by the time this runs, so a failure here is
/// reported without undoing the photo: losing a sidecar's name is bad, but
/// rolling back a rename the user just watched succeed is worse.
pub fn rename_with(old: &Path, new: &Path) -> Vec<(PathBuf, String)> {
    let mut failures = Vec::new();
    for sidecar in companions(old) {
        let destination = destination_for(&sidecar, old, new);
        if destination == sidecar {
            continue;
        }
        if let Err(err) = fs::rename(&sidecar, &destination) {
            failures.push((sidecar, err.to_string()));
        }
    }
    failures
}

/// Moves the sidecars of many photos at once, for a batch rename.
///
/// Two phases, for the same reason the photo plan needs them: in a shifted
/// series one photo's new name is another's old one, so a sidecar's destination
/// can be a sidecar that still has to move. Staging everything first means no
/// sidecar is ever overwritten.
pub fn rename_many(moves: &[(PathBuf, PathBuf)]) -> Vec<(PathBuf, String)> {
    let mut failures = Vec::new();
    let mut staged: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();

    for (from, to) in moves {
        for sidecar in companions(from) {
            let destination = destination_for(&sidecar, from, to);
            if destination == sidecar {
                continue;
            }
            let parent = sidecar.parent().unwrap_or(Path::new("."));
            let temp = parent.join(format!(
                ".cadrocfile-sidecar-{}-{}",
                std::process::id(),
                staged.len()
            ));
            match fs::rename(&sidecar, &temp) {
                Ok(()) => staged.push((temp, destination, sidecar)),
                Err(err) => failures.push((sidecar, err.to_string())),
            }
        }
    }

    for (temp, destination, original) in staged {
        if let Err(err) = fs::rename(&temp, &destination) {
            // Put it back rather than leaving a dot-file behind.
            let _ = fs::rename(&temp, &original);
            failures.push((original, err.to_string()));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "cadrocfile-sidecar-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("tempdir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn touch(path: &Path) {
        fs::write(path, b"x").expect("write");
    }

    #[test]
    fn finds_appended_and_stem_sidecars() {
        let dir = TempDir::new("find");
        let photo = dir.0.join("IMG_0001.ORF");
        touch(&photo);
        let appended = dir.0.join("IMG_0001.ORF.xmp");
        touch(&appended);
        assert_eq!(companions(&photo), vec![appended]);

        fs::remove_file(dir.0.join("IMG_0001.ORF.xmp")).unwrap();
        let stem = dir.0.join("IMG_0001.xmp");
        touch(&stem);
        assert_eq!(companions(&photo), vec![stem]);
    }

    #[test]
    fn a_photo_without_sidecars_has_none() {
        let dir = TempDir::new("none");
        let photo = dir.0.join("IMG_0002.JPG");
        touch(&photo);
        assert!(companions(&photo).is_empty());
    }

    #[test]
    fn sidecars_follow_a_conflict_rename() {
        let photo = Path::new("/fotos/IMG_0001.ORF");
        let destination = Path::new("/dest/IMG_0001 (1).ORF");
        assert_eq!(
            destination_for(Path::new("/fotos/IMG_0001.ORF.xmp"), photo, destination),
            PathBuf::from("/dest/IMG_0001 (1).ORF.xmp")
        );
        assert_eq!(
            destination_for(Path::new("/fotos/IMG_0001.xmp"), photo, destination),
            PathBuf::from("/dest/IMG_0001 (1).xmp")
        );
    }

    #[test]
    fn renaming_moves_both_layouts() {
        let dir = TempDir::new("rename");
        let old = dir.0.join("A.CR2");
        touch(&old);
        let sidecar = dir.0.join("A.CR2.xmp");
        touch(&sidecar);
        let new = dir.0.join("B.CR2");

        assert!(rename_with(&old, &new).is_empty());
        assert!(dir.0.join("B.CR2.xmp").exists());
        assert!(!dir.0.join("A.CR2.xmp").exists());
    }

    #[test]
    fn sidecars_follow_a_shifted_series_without_overwriting_each_other() {
        let dir = TempDir::new("series");
        let a = dir.0.join("1.CR2");
        let b = dir.0.join("2.CR2");
        touch(&a);
        touch(&b);
        for name in ["1.CR2.xmp", "2.CR2.xmp"] {
            fs::write(dir.0.join(name), name.as_bytes()).unwrap();
        }

        // 1 -> 2 and 2 -> 3: moving 1's sidecar first would clobber 2's.
        let failures = rename_many(&[(a.clone(), dir.0.join("2.CR2")), (b, dir.0.join("3.CR2"))]);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(fs::read(dir.0.join("2.CR2.xmp")).unwrap(), b"1.CR2.xmp");
        assert_eq!(fs::read(dir.0.join("3.CR2.xmp")).unwrap(), b"2.CR2.xmp");
        assert!(!dir.0.join("1.CR2.xmp").exists());
    }

    #[test]
    fn a_directory_named_like_a_sidecar_is_ignored() {
        let dir = TempDir::new("dir");
        let photo = dir.0.join("A.NEF");
        touch(&photo);
        fs::create_dir(dir.0.join("A.NEF.xmp")).unwrap();
        assert!(companions(&photo).is_empty());
    }
}
