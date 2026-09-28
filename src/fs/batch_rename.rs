//! Renaming many files at once: planning, checking, and doing it safely.
//!
//! Planning is pure — it computes every new name and flags every problem
//! before anything on disk is touched, which is what lets the dialog show a
//! live preview and refuse a batch that would go wrong.
//!
//! Executing is where the real danger lives. A batch can swap names (`a→b`
//! while `b→a`), or shift a numbered series by one, so some target names are
//! still held by files later in the same batch. Renaming in order would
//! overwrite them — silently, since `rename(2)` replaces its target. So every
//! file first moves to a unique temporary name, and only then to its final
//! one; and if anything fails part way, whatever has moved is put back.

use std::path::{Path, PathBuf};

/// How the new names are made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// Replace every occurrence of `find` with `with`.
    Replace { find: String, with: String, match_case: bool },
    /// A pattern with `{n}` for a counter and `{name}` for the original name.
    Template { pattern: String, start: u32, pad: usize },
    Case(CaseMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    Lower,
    Upper,
    Title,
}

/// What would happen to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The rule leaves this name as it is.
    Unchanged,
    Ready,
    /// The new name cannot exist: empty, `.`, a `/`, too long.
    Invalid(String),
    /// Two files in the batch would get this name.
    Duplicate,
    /// A file outside the batch already has this name.
    Exists,
}

impl Status {
    pub fn is_problem(&self) -> bool {
        !matches!(self, Status::Unchanged | Status::Ready)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub from: PathBuf,
    pub to: PathBuf,
    pub status: Status,
}

impl Planned {
    pub fn new_name(&self) -> String {
        name_of(&self.to)
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Splits a name into the part a rule edits and the extension it leaves alone.
///
/// A leading dot is a hidden file, not an extension: `.bashrc` has none. Nor
/// do folders — `v1.2` is a folder name, not `v1` with a `.2`. The compound
/// archive suffixes are kept whole, or renaming `backup.tar.gz` would edit
/// `backup.tar` and leave a file nobody can recognise as a tarball.
pub fn split_extension(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    let lower = name.to_ascii_lowercase();
    for compound in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz4"] {
        if lower.ends_with(compound) && name.len() > compound.len() {
            let at = name.len() - compound.len();
            return (&name[..at], &name[at..]);
        }
    }
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(at) => (&name[..at], &name[at..]),
    }
}

/// Applies a rule to one name. `index` counts from zero in the batch's order.
fn apply(rule: &Rule, stem: &str, index: usize) -> String {
    match rule {
        Rule::Replace { find, with, match_case } => replace(stem, find, with, *match_case),
        Rule::Template { pattern, start, pad } => {
            let number = format!("{:0pad$}", *start as usize + index, pad = *pad);
            pattern.replace("{n}", &number).replace("{name}", stem)
        }
        Rule::Case(CaseMode::Lower) => stem.to_lowercase(),
        Rule::Case(CaseMode::Upper) => stem.to_uppercase(),
        Rule::Case(CaseMode::Title) => title_case(stem),
    }
}

/// Replaces every occurrence, optionally ignoring case.
///
/// Case-insensitive matching compares characters, not bytes: lowercasing the
/// whole name and searching that would give byte offsets into a *different*
/// string whenever a character's lowercase form has a different length, and
/// the replacement would land in the wrong place.
fn replace(text: &str, find: &str, with: &str, match_case: bool) -> String {
    if find.is_empty() {
        return text.to_string();
    }
    if match_case {
        return text.replace(find, with);
    }
    let haystack: Vec<char> = text.chars().collect();
    let needle: Vec<char> = find.chars().collect();
    let same = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());

    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < haystack.len() {
        let hit = i + needle.len() <= haystack.len()
            && needle.iter().enumerate().all(|(k, &c)| same(haystack[i + k], c));
        if hit {
            out.push_str(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// Capitalises each word, leaving the separators between them untouched.
fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at_start = true;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if at_start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            at_start = false;
        } else {
            out.push(c);
            at_start = true;
        }
    }
    out
}

/// Why a name cannot exist, if it cannot.
fn invalid_reason(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("empty name".into());
    }
    if name == "." || name == ".." {
        return Some("reserved name".into());
    }
    if name.contains('/') {
        return Some("contains /".into());
    }
    if name.contains('\0') {
        return Some("contains a NUL".into());
    }
    // The limit on almost every Linux filesystem, in bytes, not characters.
    if name.len() > 255 {
        return Some("longer than 255 bytes".into());
    }
    None
}

/// Works out every new name and every problem, touching nothing.
///
/// `files` is `(path, is_dir)` in the order numbering should follow.
/// `exists` answers whether a path is taken on disk; it is a parameter so the
/// planner can be tested without a filesystem, and so the dialog can pass a
/// check against the real one.
pub fn plan(
    files: &[(PathBuf, bool)],
    rule: &Rule,
    keep_extension: bool,
    exists: impl Fn(&Path) -> bool,
) -> Vec<Planned> {
    let mut planned: Vec<Planned> = files
        .iter()
        .enumerate()
        .map(|(index, (path, is_dir))| {
            let name = name_of(path);
            let (stem, ext) = if keep_extension {
                split_extension(&name, *is_dir)
            } else {
                (name.as_str(), "")
            };
            let new_name = format!("{}{ext}", apply(rule, stem, index));
            let to = path.with_file_name(&new_name);
            let status = if new_name == name {
                Status::Unchanged
            } else if let Some(reason) = invalid_reason(&new_name) {
                Status::Invalid(reason)
            } else {
                Status::Ready
            };
            Planned { from: path.clone(), to, status }
        })
        .collect();

    // Two files landing on one name. Compared per directory, since search
    // results mix folders and `a.txt` in two places is not a clash.
    for i in 0..planned.len() {
        if planned[i].status != Status::Ready {
            continue;
        }
        let clash = planned
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && other.to == planned[i].to);
        if clash {
            planned[i].status = Status::Duplicate;
        }
    }

    // A target already on disk is only a problem if it belongs to a file that
    // is not itself moving out of the way in this batch.
    let sources: std::collections::HashSet<PathBuf> =
        planned.iter().filter(|p| p.status == Status::Ready).map(|p| p.from.clone()).collect();
    for item in planned.iter_mut().filter(|p| p.status == Status::Ready) {
        if exists(&item.to) && !sources.contains(&item.to) && !same_file_other_case(&item.from, &item.to) {
            item.status = Status::Exists;
        }
    }
    planned
}

/// `photo.JPG` → `photo.jpg` on a case-insensitive filesystem (a USB stick,
/// an NTFS drive) finds its *own* file when it asks whether the target exists.
/// That is not a clash; it is the rename the user asked for.
fn same_file_other_case(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(from), std::fs::metadata(to)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Carries out a checked plan. Returns the new paths.
///
/// Refuses outright if the plan has any problem — the dialog should never
/// offer that, but the guarantee belongs here, next to the code that could
/// do the damage.
pub fn execute(plan: &[Planned]) -> Result<Vec<PathBuf>, String> {
    if plan.iter().any(|p| p.status.is_problem()) {
        return Err("Some of the new names clash or are not allowed.".into());
    }
    let moving: Vec<&Planned> = plan.iter().filter(|p| p.status == Status::Ready).collect();

    // Phase one: every file out of the way under a name nothing else uses.
    let mut staged: Vec<(PathBuf, &Planned)> = Vec::with_capacity(moving.len());
    for (index, item) in moving.iter().enumerate() {
        let temp = item
            .from
            .with_file_name(format!(".cadrocfile-rename-{}-{index}", std::process::id()));
        if let Err(e) = rename_no_clobber(&item.from, &temp) {
            let restored = undo(&staged, &[]);
            return Err(failed(format!("Could not rename {}: {e}", name_of(&item.from)), restored));
        }
        staged.push((temp, item));
    }

    // Phase two: into their final names. On failure at `i`, everything
    // before it has reached its final name and everything from it on is still
    // at its temporary one — so each group is undone from where it actually is.
    for (i, (temp, item)) in staged.iter().enumerate() {
        if let Err(e) = rename_no_clobber(temp, &item.to) {
            let done: Vec<&Planned> = staged[..i].iter().map(|(_, p)| *p).collect();
            let restored = undo(&staged[i..], &done);
            return Err(failed(
                format!("Could not rename {} to {}: {e}", name_of(&item.from), item.new_name()),
                restored,
            ));
        }
    }
    Ok(moving.iter().map(|p| p.to.clone()).collect())
}

/// Puts files back after a failure: staged ones from their temporary names,
/// finished ones from their new names.
///
/// Two-phase, for the same reason as [`execute`]. A finished rename can be
/// sitting on another file's *original* name — in a shifted series, file 2 is
/// at `3.txt`, which is where file 3 must go back to — so restoring them one
/// at a time collides in every order. Everything goes to a temporary name
/// first, and only then home.
fn undo(staged: &[(PathBuf, &Planned)], done: &[&Planned]) -> bool {
    let mut restored = true;
    let mut returning: Vec<(PathBuf, &Planned)> = staged
        .iter()
        .filter(|(temp, _)| temp.symlink_metadata().is_ok())
        .map(|(temp, item)| (temp.clone(), *item))
        .collect();
    for (index, item) in done.iter().enumerate() {
        let temp = item
            .to
            .with_file_name(format!(".cadrocfile-undo-{}-{index}", std::process::id()));
        match rename_no_clobber(&item.to, &temp) {
            Ok(()) => returning.push((temp, item)),
            Err(_) => restored = false,
        }
    }
    for (temp, item) in returning {
        restored &= rename_no_clobber(&temp, &item.from).is_ok();
    }
    restored
}

/// The error for a failed batch, saying truthfully where the files ended up.
fn failed(what: String, restored: bool) -> String {
    if restored {
        format!("{what}\n\nEvery file was put back under its original name.")
    } else {
        format!(
            "{what}\n\nSome files could not be put back. Nothing was deleted — look for \
             names beginning “.cadrocfile-rename” or “.cadrocfile-undo” in the folder."
        )
    }
}

/// `rename(2)`, except it will not replace an existing file.
///
/// The plan already checked, but the disk can change between planning and
/// doing — another program creates the target in between — and the one
/// outcome that must never happen here is a file silently destroyed.
/// `renameat2(RENAME_NOREPLACE)` makes that check atomic where the filesystem
/// supports it; elsewhere a check-then-rename is the best available.
fn rename_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let c_to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // Filesystems without RENAME_NOREPLACE — some FUSE ones, including older
    // ntfs-3g — say so with EINVAL. Fall back rather than refuse.
    if error.raw_os_error() == Some(libc::EINVAL) {
        if to.symlink_metadata().is_ok() && !same_file_other_case(from, to) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "a file with that name already exists",
            ));
        }
        return std::fs::rename(from, to);
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn files(names: &[&str]) -> Vec<(PathBuf, bool)> {
        names.iter().map(|n| (PathBuf::from("/d").join(n), false)).collect()
    }

    fn names(plan: &[Planned]) -> Vec<String> {
        plan.iter().map(Planned::new_name).collect()
    }

    const NOTHING_EXISTS: fn(&Path) -> bool = |_| false;

    #[test]
    fn extensions_are_split_the_way_people_expect() {
        assert_eq!(split_extension("photo.jpg", false), ("photo", ".jpg"));
        assert_eq!(split_extension("backup.tar.gz", false), ("backup", ".tar.gz"));
        assert_eq!(split_extension("BACKUP.TAR.ZST", false), ("BACKUP", ".TAR.ZST"));
        assert_eq!(split_extension(".bashrc", false), (".bashrc", ""), "a dotfile has no extension");
        assert_eq!(split_extension("README", false), ("README", ""));
        assert_eq!(split_extension("v1.2", true), ("v1.2", ""), "folders have no extension");
        assert_eq!(split_extension(".tar.gz", false), (".tar", ".gz"));
    }

    #[test]
    fn numbering_follows_the_given_order_and_keeps_extensions() {
        let rule = Rule::Template { pattern: "Trip {n}".into(), start: 1, pad: 2 };
        let plan = plan(&files(&["b.JPG", "a.png", "c.tar.gz"]), &rule, true, NOTHING_EXISTS);
        assert_eq!(names(&plan), ["Trip 01.JPG", "Trip 02.png", "Trip 03.tar.gz"]);
    }

    #[test]
    fn the_original_name_can_be_used_in_a_template() {
        let rule = Rule::Template { pattern: "{n}-{name}".into(), start: 7, pad: 0 };
        let plan = plan(&files(&["alpha.txt"]), &rule, true, NOTHING_EXISTS);
        assert_eq!(names(&plan), ["7-alpha.txt"]);
    }

    #[test]
    fn replacing_ignores_case_only_when_asked() {
        let insensitive = Rule::Replace { find: "img".into(), with: "Photo".into(), match_case: false };
        assert_eq!(names(&plan(&files(&["IMG_001.jpg"]), &insensitive, true, NOTHING_EXISTS)), ["Photo_001.jpg"]);

        let sensitive = Rule::Replace { find: "img".into(), with: "Photo".into(), match_case: true };
        let plan = plan(&files(&["IMG_001.jpg"]), &sensitive, true, NOTHING_EXISTS);
        assert_eq!(plan[0].status, Status::Unchanged);
    }

    /// Lowercasing can change a character's length in bytes; a naive
    /// lowercase-then-search would splice the replacement into the wrong spot.
    #[test]
    fn case_insensitive_replace_is_correct_for_non_ascii_names() {
        assert_eq!(replace("Straße_ÉTÉ", "été", "summer", false), "Straße_summer");
        assert_eq!(replace("İstanbul trip", "trip", "x", false), "İstanbul x");
    }

    #[test]
    fn the_extension_is_left_alone_unless_told_otherwise() {
        let rule = Rule::Case(CaseMode::Upper);
        assert_eq!(names(&plan(&files(&["notes.txt"]), &rule, true, NOTHING_EXISTS)), ["NOTES.txt"]);
        assert_eq!(names(&plan(&files(&["notes.txt"]), &rule, false, NOTHING_EXISTS)), ["NOTES.TXT"]);
    }

    #[test]
    fn title_case_capitalises_words_and_keeps_separators() {
        assert_eq!(title_case("my holiday_PHOTOS-final"), "My Holiday_Photos-Final");
    }

    #[test]
    fn two_files_given_one_name_are_both_flagged() {
        let rule = Rule::Template { pattern: "same".into(), start: 1, pad: 0 };
        let plan = plan(&files(&["a.txt", "b.txt", "c.md"]), &rule, true, NOTHING_EXISTS);
        assert_eq!(plan[0].status, Status::Duplicate);
        assert_eq!(plan[1].status, Status::Duplicate);
        assert_eq!(plan[2].status, Status::Ready, "different extension, different name");
    }

    #[test]
    fn names_that_cannot_exist_are_refused_with_a_reason() {
        let slash = Rule::Replace { find: "a".into(), with: "x/y".into(), match_case: true };
        assert!(matches!(plan(&files(&["a"]), &slash, true, NOTHING_EXISTS)[0].status, Status::Invalid(_)));

        let empty = Rule::Replace { find: "abc".into(), with: "".into(), match_case: true };
        assert!(matches!(plan(&files(&["abc"]), &empty, false, NOTHING_EXISTS)[0].status, Status::Invalid(_)));

        let long = Rule::Template { pattern: "x".repeat(300), start: 1, pad: 0 };
        assert!(matches!(plan(&files(&["a"]), &long, true, NOTHING_EXISTS)[0].status, Status::Invalid(_)));
    }

    /// A target held by a file that is itself moving is not a clash — it is
    /// what a swap or a renumbering looks like.
    #[test]
    fn a_target_freed_by_the_same_batch_is_not_a_clash() {
        let swap = files(&["1.txt", "2.txt"]);
        let rule = Rule::Template { pattern: "{n}".into(), start: 2, pad: 0 };
        // Renumbering 1,2 → 2,3: "2.txt" exists, but it is being moved to 3.
        let exists = |p: &Path| p == Path::new("/d/2.txt") || p == Path::new("/d/1.txt");
        let plan = plan(&swap, &rule, true, exists);
        assert!(plan.iter().all(|p| p.status == Status::Ready), "{plan:?}");
    }

    #[test]
    fn a_target_held_by_a_file_outside_the_batch_is_a_clash() {
        let rule = Rule::Template { pattern: "keep".into(), start: 1, pad: 0 };
        let exists = |p: &Path| p == Path::new("/d/keep.txt");
        let plan = plan(&files(&["a.txt"]), &rule, true, exists);
        assert_eq!(plan[0].status, Status::Exists);
    }

    // ── on disk ─────────────────────────────────────────────────────────

    fn real_plan(dir: &Path, rule: &Rule) -> Vec<Planned> {
        let mut entries: Vec<(PathBuf, bool)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| (e.unwrap().path(), false))
            .collect();
        entries.sort();
        plan(&entries, rule, true, |p| p.symlink_metadata().is_ok())
    }

    fn contents(dir: &Path) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let path = e.unwrap().path();
                (name_of(&path), std::fs::read_to_string(&path).unwrap())
            })
            .collect();
        out.sort();
        out
    }

    /// The case that destroys data in a naive implementation: shifting a
    /// numbered series up by one renames 1→2 while 2 still holds a file.
    #[test]
    fn renumbering_a_series_onto_itself_loses_nothing() {
        let dir = TempDir::new("rename-shift");
        for n in 1..=4 {
            std::fs::write(dir.join(format!("{n}.txt")), format!("file {n}")).unwrap();
        }
        let rule = Rule::Template { pattern: "{n}".into(), start: 2, pad: 0 };
        let plan = real_plan(dir.path(), &rule);
        execute(&plan).unwrap();

        assert_eq!(
            contents(dir.path()),
            [("2.txt", "file 1"), ("3.txt", "file 2"), ("4.txt", "file 3"), ("5.txt", "file 4")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }

    #[test]
    fn swapping_two_names_works() {
        let dir = TempDir::new("rename-swap");
        std::fs::write(dir.join("a.txt"), "was a").unwrap();
        std::fs::write(dir.join("b.txt"), "was b").unwrap();
        let plan = vec![
            Planned { from: dir.join("a.txt"), to: dir.join("b.txt"), status: Status::Ready },
            Planned { from: dir.join("b.txt"), to: dir.join("a.txt"), status: Status::Ready },
        ];
        execute(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "was b");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "was a");
    }

    #[test]
    fn a_plan_with_problems_is_refused_before_anything_moves() {
        let dir = TempDir::new("rename-refuse");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let rule = Rule::Template { pattern: "same".into(), start: 1, pad: 0 };
        let plan = real_plan(dir.path(), &rule);
        assert!(execute(&plan).is_err());
        assert_eq!(contents(dir.path()).len(), 2);
        assert!(dir.join("a.txt").exists() && dir.join("b.txt").exists(), "nothing may move");
    }

    /// A file created at a target name between planning and executing must
    /// survive, and the batch must be rolled back rather than half-applied.
    #[test]
    fn a_file_that_appears_after_planning_is_never_overwritten() {
        let dir = TempDir::new("rename-race");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let rule = Rule::Template { pattern: "new{n}".into(), start: 1, pad: 0 };
        let plan = real_plan(dir.path(), &rule);

        // Something else takes one of the targets in the meantime.
        std::fs::write(dir.join("new2.txt"), "precious").unwrap();

        let error = execute(&plan).expect_err("the batch must fail");
        assert!(error.contains("put back under its original name"), "{error}");
        assert_eq!(std::fs::read_to_string(dir.join("new2.txt")).unwrap(), "precious");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a", "a.txt was not restored");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b", "b.txt was not restored");
        assert!(
            !std::fs::read_dir(dir.path()).unwrap().any(|e| name_of(&e.unwrap().path()).starts_with(".cadrocfile-rename")),
            "a temporary name was left behind"
        );
    }

    /// A failure late in phase two has to unwind renames that now sit on each
    /// other's original names. Restoring them one by one in any order collides;
    /// this is the case that proves the unwinding is itself collision-safe.
    #[test]
    fn a_shifted_series_that_fails_at_the_last_step_is_fully_restored() {
        let dir = TempDir::new("rename-unwind");
        for n in 1..=4 {
            std::fs::write(dir.join(format!("{n}.txt")), format!("file {n}")).unwrap();
        }
        let rule = Rule::Template { pattern: "{n}".into(), start: 2, pad: 0 };
        let plan = real_plan(dir.path(), &rule);

        // The very last target is taken after planning, so three renames have
        // finished when the fourth fails.
        std::fs::write(dir.join("5.txt"), "precious").unwrap();
        assert!(execute(&plan).is_err());

        let expected: Vec<(String, String)> = [
            ("1.txt", "file 1"),
            ("2.txt", "file 2"),
            ("3.txt", "file 3"),
            ("4.txt", "file 4"),
            ("5.txt", "precious"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .into();
        assert_eq!(contents(dir.path()), expected);
    }
}
