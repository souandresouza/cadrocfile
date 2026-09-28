//! How many threads long-running file operations are allowed to use.
//!
//! Every parallel path in the app draws from this one budget so the file
//! manager never takes the machine over. The default deliberately leaves
//! roughly half the cores free: a compress or shred is something the user
//! starts *alongside* their real work, not instead of it, and a job that
//! finishes 15% sooner is a bad trade for a desktop that stutters while it
//! runs.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Never exceed this many threads however many cores the machine has.
///
/// Past this point all three workloads are bound by the storage device rather
/// than the CPU, so extra threads buy nothing and only add contention.
const CEILING: usize = 8;

/// User override, `0` meaning "decide automatically". Written once at startup
/// from the config and read from worker threads, hence the atomic.
static OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Applies the configured thread count. `0` restores automatic sizing.
pub fn set_override(threads: u32) {
    OVERRIDE.store(threads as usize, Ordering::Relaxed);
}

fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// The sizing rule, separated from the global so it can be tested directly.
///
/// Automatic sizing is half the cores, so a 16-core machine runs jobs on 8
/// threads and still has 8 for everything else. An explicit override is
/// honoured as given, only clamped to something the machine can actually run.
fn resolve(requested: usize, cores: usize) -> usize {
    let cores = cores.max(1);
    match requested {
        0 => (cores / 2).clamp(1, CEILING),
        n => n.clamp(1, cores),
    }
}

/// Threads to use for one job.
pub fn threads() -> usize {
    resolve(OVERRIDE.load(Ordering::Relaxed), cores())
}

/// Threads for a job over `items` pieces of work.
///
/// Spawning eight threads to shred three files just pays eight thread setups
/// to leave five idle.
pub fn threads_for(items: usize) -> usize {
    threads().min(items.max(1))
}

/// Runs `work` over `items` on [`threads_for`] workers, in no particular order.
///
/// Work is claimed from a shared counter rather than sliced up front, because
/// the items are files and their costs differ by orders of magnitude — a
/// static split would leave one thread holding the only 4 GB file.
///
/// Falls back to running inline on the calling thread when only one worker is
/// warranted, which keeps the single-file case free of thread overhead.
pub fn for_each<T, F>(items: &[T], work: F)
where
    T: Sync,
    F: Fn(&T) + Sync,
{
    let workers = threads_for(items.len());
    if workers <= 1 {
        items.iter().for_each(&work);
        return;
    }

    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else { return };
                    work(item);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// `resolve` is tested rather than `threads()` because the override is
    /// process-global: reading it back from a test would race the other tests
    /// in this module, which run on their own threads in the same process.
    #[test]
    fn automatic_sizing_leaves_headroom_and_stays_capped() {
        for cores in [1, 2, 3, 4, 8, 12, 16, 32, 128] {
            let n = resolve(0, cores);
            assert!(n >= 1, "{cores} cores: must always allow at least one thread");
            assert!(n <= CEILING, "{cores} cores: must never exceed the ceiling");
            assert!(n <= cores, "{cores} cores: must never exceed the core count");
            if cores >= 4 {
                assert!(n <= cores / 2, "{cores} cores: must leave half the cores free");
            }
        }
        // The case the user actually has, spelled out.
        assert_eq!(resolve(0, 16), 8);
    }

    #[test]
    fn an_override_is_honoured_but_clamped_to_the_machine() {
        assert_eq!(resolve(3, 16), 3);
        assert_eq!(resolve(1, 16), 1);
        // Above the ceiling is allowed when asked for explicitly; the ceiling
        // only governs the automatic choice.
        assert_eq!(resolve(12, 16), 12);
        assert_eq!(resolve(10_000, 16), 16);
        assert_eq!(resolve(4, 2), 2);
    }

    #[test]
    fn work_is_never_dropped_or_duplicated() {
        let items: Vec<u64> = (0..1000).collect();
        let seen = AtomicU64::new(0);
        let runs = AtomicU64::new(0);
        for_each(&items, |n| {
            seen.fetch_add(*n, Ordering::Relaxed);
            runs.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(seen.load(Ordering::Relaxed), items.iter().sum::<u64>());
        assert_eq!(runs.load(Ordering::Relaxed), items.len() as u64);
    }

    #[test]
    fn a_single_item_does_not_spawn_threads() {
        assert_eq!(threads_for(1), 1);
        assert_eq!(threads_for(0), 1);
    }
}
