//! `FindFirstFile`-based directory walk.
//!
//! Two jobs:
//!
//! 1. **Fallback indexing.** Non-NTFS volumes have no MFT, and a user who
//!    declines the elevation prompt cannot open a raw volume handle. Rather than
//!    failing outright, Velox walks a set of user-selected folders. There is no
//!    instant-index guarantee on this path, and it is marked
//!    [`SourceKind::Walk`] so the UI can say so.
//!
//! 2. **The baseline the MFT path is measured against.** [`walk_serial`] is a
//!    deliberately naive single-threaded recursive walk — the thing everyone
//!    writes first — so the benchmark harness has an honest number to compare
//!    the MFT read to.
//!
//! On Windows, `std::fs::read_dir` is a thin wrapper over
//! `FindFirstFileW`/`FindNextFileW`, and the `Metadata` it yields comes from the
//! `WIN32_FIND_DATA` already returned by the enumeration, so neither walk pays
//! for a second `stat`-equivalent syscall per entry.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::index::{SourceKind, VolumeIndex};
use crate::store::{EntryId, EntryStore, VolumeId, INVALID_ENTRY};

#[derive(Debug, Clone, Default)]
pub struct WalkStats {
    pub files: usize,
    pub dirs: usize,
    pub errors: usize,
    pub elapsed: Duration,
}

impl WalkStats {
    pub fn total(&self) -> usize {
        self.files + self.dirs
    }
    pub fn rate(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 {
            self.total() as f64 / s
        } else {
            0.0
        }
    }
}

/// Unix epoch seconds -> Windows FILETIME ticks.
fn systime_to_filetime(t: std::io::Result<std::time::SystemTime>) -> i64 {
    let Ok(t) = t else { return 0 };
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => 116_444_736_000_000_000i64 + (d.as_nanos() / 100) as i64,
        Err(_) => 0,
    }
}

/// Naive single-threaded recursive walk. The benchmark baseline; also the
/// simplest correct fallback.
pub fn walk_serial(roots: &[PathBuf], label: &str) -> (EntryStore, WalkStats) {
    let t0 = Instant::now();
    let mut store = EntryStore::new(label);
    let mut stats = WalkStats::default();

    let root_id = store.push("", INVALID_ENTRY, 0, 0, true);
    store.set_root(root_id);

    // Explicit stack rather than recursion: deep trees must not blow the stack.
    let mut stack: Vec<(PathBuf, EntryId)> = Vec::new();
    for r in roots {
        stack.push((r.clone(), seed_root(&mut store, root_id, r)));
    }

    while let Some((dir, parent)) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        for entry in rd {
            let Ok(entry) = entry else {
                stats.errors += 1;
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let md = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            let is_dir = md.is_dir();
            let id = store.push(
                &name,
                parent,
                if is_dir { 0 } else { md.len() },
                systime_to_filetime(md.modified()),
                is_dir,
            );
            if is_dir {
                stats.dirs += 1;
                // Never follow reparse points: junctions and symlinks create cycles.
                if !md.file_type().is_symlink() {
                    stack.push((dir.join(&name), id));
                }
            } else {
                stats.files += 1;
            }
        }
    }

    stats.elapsed = t0.elapsed();
    (store, stats)
}

/// Work-stealing parallel walk. Used for the actual fallback index, since the
/// serial walk is unusably slow on a real tree.
pub fn walk_parallel(roots: &[PathBuf], label: &str) -> (EntryStore, WalkStats) {
    let t0 = Instant::now();
    let mut store = EntryStore::new(label);
    let root_id = store.push("", INVALID_ENTRY, 0, 0, true);
    store.set_root(root_id);

    let mut seeds: Vec<(PathBuf, EntryId)> = Vec::new();
    for r in roots {
        seeds.push((r.clone(), seed_root(&mut store, root_id, r)));
    }

    // The store is not `Sync`-friendly to write concurrently (names are appended
    // to one blob), so workers produce flat batches and a single thread folds
    // them in. Directory listing is the expensive part and that is what
    // parallelises; the fold-in is a memcpy.
    let store = Mutex::new(store);
    let pending: Mutex<Vec<(PathBuf, EntryId)>> = Mutex::new(seeds);
    let files = AtomicUsize::new(0);
    let dir_count = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let active = AtomicUsize::new(0);

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                // Claim the job and mark ourselves busy under the same lock: if
                // these were separate steps, another worker could observe an
                // empty queue with nobody yet marked active and quit early.
                let job = {
                    let mut p = pending.lock().unwrap();
                    match p.pop() {
                        Some(j) => {
                            active.fetch_add(1, AtomicOrdering::SeqCst);
                            Some(j)
                        }
                        None => None,
                    }
                };
                let Some((dir, parent)) = job else {
                    // No work right now; stop only once nobody else is producing.
                    if active.load(AtomicOrdering::SeqCst) == 0 {
                        break;
                    }
                    std::thread::yield_now();
                    continue;
                };

                let mut batch: Vec<(String, u64, i64, bool)> = Vec::new();
                match std::fs::read_dir(&dir) {
                    Ok(rd) => {
                        for entry in rd.flatten() {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            let Ok(md) = entry.metadata() else {
                                errors.fetch_add(1, AtomicOrdering::Relaxed);
                                continue;
                            };
                            batch.push((
                                name,
                                if md.is_dir() { 0 } else { md.len() },
                                systime_to_filetime(md.modified()),
                                md.is_dir() && !md.file_type().is_symlink(),
                            ));
                        }
                    }
                    Err(_) => {
                        errors.fetch_add(1, AtomicOrdering::Relaxed);
                    }
                }

                let mut new_dirs = Vec::new();
                {
                    let mut s = store.lock().unwrap();
                    for (name, size, mtime, is_dir) in batch {
                        let id = s.push(&name, parent, size, mtime, is_dir);
                        if is_dir {
                            dir_count.fetch_add(1, AtomicOrdering::Relaxed);
                            new_dirs.push((dir.join(&name), id));
                        } else {
                            files.fetch_add(1, AtomicOrdering::Relaxed);
                        }
                    }
                }
                if !new_dirs.is_empty() {
                    pending.lock().unwrap().extend(new_dirs);
                }
                active.fetch_sub(1, AtomicOrdering::SeqCst);
            });
        }
    });

    let store = store.into_inner().unwrap();
    let stats = WalkStats {
        files: files.load(AtomicOrdering::Relaxed),
        dirs: dir_count.load(AtomicOrdering::Relaxed),
        errors: errors.load(AtomicOrdering::Relaxed),
        elapsed: t0.elapsed(),
    };
    (store, stats)
}

/// Build a ready-to-query volume index from a set of folders.
pub fn index_folders(id: VolumeId, roots: &[PathBuf], label: &str) -> (VolumeIndex, WalkStats) {
    let (store, stats) = walk_parallel(roots, label);
    (VolumeIndex::build(id, SourceKind::Walk, store), stats)
}

/// Create the ancestor directory chain for a walk root and return the id of its
/// deepest component.
///
/// The store's label already supplies the volume, so a root of `C:\Users\Nihil`
/// becomes two nested entries, `Users` then `Nihil`. Pushing the whole path as a
/// single name instead made every reconstructed path read `C:\C:\Users\...`.
fn seed_root(store: &mut EntryStore, root_id: EntryId, path: &Path) -> EntryId {
    let text = path.to_string_lossy().replace('/', "\\");
    // Drop the drive specifier if there is one; the label already covers it.
    let rest = match text.split_once(':') {
        Some((_, after)) => after.to_string(),
        None => text,
    };
    let mut parent = root_id;
    for comp in rest.split('\\').filter(|c| !c.is_empty()) {
        parent = store.push(comp, parent, 0, 0, true);
    }
    parent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempdir::TempTree {
        tempdir::TempTree::new(&[
            "alpha.txt",
            "beta.log",
            "sub/gamma.txt",
            "sub/deeper/delta.md",
            "sub/deeper/epsilon.md",
        ])
    }

    #[test]
    fn serial_walk_finds_everything() {
        let t = fixture();
        let (store, stats) = walk_serial(std::slice::from_ref(&t.root), "TEST");
        assert_eq!(stats.files, 5);
        // `sub` and `sub/deeper`; the seeded ancestor chain is scaffolding.
        assert_eq!(stats.dirs, 2);
        let names: Vec<&str> = store.live_ids().map(|id| store.name(id)).collect();
        assert!(names.contains(&"epsilon.md"));
        assert!(names.contains(&"deeper"));
    }

    #[test]
    fn parallel_walk_agrees_with_serial() {
        let t = fixture();
        let (a, sa) = walk_serial(std::slice::from_ref(&t.root), "TEST");
        let (b, sb) = walk_parallel(std::slice::from_ref(&t.root), "TEST");
        assert_eq!(sa.files, sb.files);
        assert_eq!(sa.dirs, sb.dirs);

        let mut na: Vec<&str> = a.live_ids().map(|id| a.name(id)).collect();
        let mut nb: Vec<&str> = b.live_ids().map(|id| b.name(id)).collect();
        na.sort();
        nb.sort();
        assert_eq!(na, nb);
    }

    #[test]
    fn paths_reconstruct_from_the_parent_chain() {
        let t = fixture();
        let (store, _) = walk_serial(std::slice::from_ref(&t.root), "C:");
        let id = store.live_ids().find(|id| store.name(*id) == "delta.md").unwrap();
        let p = store.path(id);
        assert!(p.ends_with("sub\\deeper\\delta.md"), "got {p}");
        // The volume must appear exactly once, at the front: a walk root becomes
        // a chain of directories, not one entry named after an absolute path.
        assert!(p.starts_with("C:\\"), "got {p}");
        assert_eq!(p.matches("C:").count(), 1, "drive duplicated in {p}");
        assert_eq!(p, format!("{}\\sub\\deeper\\delta.md", t.root.display()));
    }

    /// Minimal scratch-tree helper so the walk tests do not need a dev-dependency.
    mod tempdir {
        use std::path::PathBuf;

        pub struct TempTree {
            pub root: PathBuf,
        }

        impl TempTree {
            pub fn new(files: &[&str]) -> Self {
                let mut root = std::env::temp_dir();
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                root.push(format!("velox-walk-test-{stamp}-{:?}", std::thread::current().id()));
                std::fs::create_dir_all(&root).unwrap();
                for f in files {
                    let p = root.join(f.replace('/', std::path::MAIN_SEPARATOR_STR));
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    std::fs::write(&p, b"x").unwrap();
                }
                TempTree { root }
            }
        }

        impl Drop for TempTree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
    }
}
