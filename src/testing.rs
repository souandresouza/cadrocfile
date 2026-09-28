//! Scratch directories for tests.
//!
//! Tests used to name their directories after the process id alone. Two
//! problems followed from that. A test that panicked left its directory on
//! disk, and a later run that happened to be given the same pid then started
//! against someone else's leftovers — which is exactly how an
//! `assert!(!staging.exists())` fails once in a hundred runs and passes every
//! time you go looking for it.
//!
//! [`TempDir`] fixes both: the name carries enough entropy that two runs cannot
//! collide, and cleanup happens in `Drop`, so it runs on the panic path too.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Distinguishes directories created within the same nanosecond, which happens
/// readily when tests run in parallel on several threads.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A unique directory that removes itself when the guard goes out of scope.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates `/tmp/cadrocfile-<tag>-<pid>-<nanos>-<n>` and returns a guard.
    ///
    /// `tag` only exists to make a stray directory identifiable while a test is
    /// running; uniqueness comes from the three fields after it.
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cadrocfile-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));

        // Belt and braces: the name should already be unique, but starting from
        // a known-empty directory is what the tests actually depend on.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create scratch directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the scratch directory. It is not created.
    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }
}

/// Lets a guard be passed wherever a `&Path` is expected, so tests read the
/// same as they did before the guard existed.
impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Deliberately ignores errors: this runs while a panic may already be
        // unwinding, and panicking again there would abort the process and hide
        // the failure that mattered.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_directory_is_unique_and_empty() {
        let a = TempDir::new("unique");
        let b = TempDir::new("unique");
        assert_ne!(a.path(), b.path(), "two guards must not share a directory");
        assert!(a.path().is_dir() && b.path().is_dir());
        assert_eq!(fs::read_dir(a.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_directory_is_removed_when_the_guard_drops() {
        let path = {
            let dir = TempDir::new("dropped");
            fs::write(dir.join("f"), b"x").unwrap();
            dir.path().to_path_buf()
        };
        assert!(!path.exists(), "guard must clean up its directory");
    }

    /// The case that caused the flake: cleanup has to happen even when the test
    /// body panics, or the directory outlives the run.
    #[test]
    fn cleanup_survives_a_panic() {
        let path = std::sync::Mutex::new(PathBuf::new());
        let result = std::panic::catch_unwind(|| {
            let dir = TempDir::new("panicking");
            *path.lock().unwrap() = dir.path().to_path_buf();
            panic!("boom");
        });
        assert!(result.is_err(), "the panic should have propagated");
        let leaked = path.lock().unwrap().clone();
        assert!(!leaked.exists(), "a panicking test must not leave {leaked:?} behind");
    }
}
