//! The searchable index: a sorted `u32` order over the entry store, plus a small
//! unsorted delta overlay so live updates never have to memmove the main array.
//!
//! # Why a sorted array
//!
//! A filename-prefix query resolves to two binary searches for the lower and
//! upper bound, after which the answer is a *contiguous range* of `order`. The
//! match count is therefore O(1) and nothing is materialised: the caller walks
//! the range directly. Query cost is ~2 * log2(n) comparisons — about 46 folded
//! string compares at 5M entries — and is effectively independent of corpus size.
//!
//! A radix tree would give O(k) descent but then still has to walk a subtree to
//! enumerate matches, and it costs pointer-chasing plus per-node allocation
//! across millions of entries. The one place it wins is insert cost, and that is
//! handled here by the delta overlay instead.
//!
//! # Reader/writer separation
//!
//! Live updates take the write lock only for O(1) work: appending to the store's
//! arrays and pushing an id onto `delta`. The expensive part (merging `delta`
//! into `order`) builds the replacement `Vec<u32>` entirely outside the lock and
//! then takes the lock just long enough to swap the pointer. Write critical
//! sections are therefore microseconds, never the milliseconds a merge takes.

use std::cmp::Ordering;
use std::ops::Range;
use std::sync::RwLock;

use rayon::prelude::*;

use crate::fold;
use crate::store::{EntryId, EntryRef, EntryStore, VolumeId, INVALID_ENTRY};

/// Merge the delta overlay once it grows past this, so the linear delta scan
/// stays far cheaper than the binary search it supplements.
pub const DELTA_MERGE_THRESHOLD: usize = 4096;

/// How the volume's contents were discovered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    /// Raw MFT read. Fast path, requires elevation.
    Mft,
    /// `FindFirstFile` recursive walk. Fallback for non-NTFS volumes, or when the
    /// user declines elevation.
    Walk,
}

pub struct VolumeIndex {
    pub id: VolumeId,
    pub kind: SourceKind,
    pub store: EntryStore,
    /// Live entry ids sorted by folded name. May transiently contain ids whose
    /// entries have since been deleted; iteration skips those.
    order: Vec<EntryId>,
    /// Ids added since the last merge, unsorted, scanned linearly by queries.
    delta: Vec<EntryId>,
    /// Number of ids in `order` that are no longer live.
    dead_in_order: u32,
}

impl VolumeIndex {
    /// Build the sorted order from a fully populated store.
    pub fn build(id: VolumeId, kind: SourceKind, store: EntryStore) -> Self {
        let mut order: Vec<EntryId> = store.live_ids().collect();
        sort_order(&mut order, &store);
        VolumeIndex { id, kind, store, order, delta: Vec::new(), dead_in_order: 0 }
    }

    /// Rebuild from a snapshot: the sorted order is already correct on disk, so
    /// there is nothing to re-sort. Order entries that no longer point at a live
    /// slot are dropped rather than trusted.
    pub fn from_parts(
        id: VolumeId,
        kind: SourceKind,
        store: EntryStore,
        order: Vec<EntryId>,
    ) -> Self {
        let order: Vec<EntryId> =
            order.into_iter().filter(|e| store.visible_in_order(*e)).collect();
        VolumeIndex { id, kind, store, order, delta: Vec::new(), dead_in_order: 0 }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.order.len() + self.delta.len() - self.dead_in_order as usize
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    #[inline]
    pub fn order(&self) -> &[EntryId] {
        &self.order
    }
    #[inline]
    pub fn delta(&self) -> &[EntryId] {
        &self.delta
    }
    /// True when `order` is free of dead ids, so range lengths are exact counts.
    #[inline]
    pub fn is_clean(&self) -> bool {
        self.dead_in_order == 0
    }

    /// Locate the contiguous range of `order` whose names begin with `folded`.
    ///
    /// Two binary searches, no allocation. `folded` must already be case-folded
    /// (see [`crate::fold::fold_to_string`]).
    pub fn prefix_range(&self, folded: &[u8]) -> Range<u32> {
        if folded.is_empty() {
            return 0..self.order.len() as u32;
        }
        let s = &self.store;
        let ord = &self.order;
        // Lower bound: first name that is not strictly less than the prefix.
        let lo = partition_point(ord.len(), |i| {
            fold::fold_cmp(s.raw_name(ord[i]), folded) == Ordering::Less
        });
        // Upper bound: first name past the lower bound that no longer starts with it.
        let hi = lo
            + partition_point(ord.len() - lo, |i| {
                fold::fold_starts_with(s.raw_name(ord[lo + i]), folded)
            });
        lo as u32..hi as u32
    }

    /// Ids in the delta overlay whose names begin with `folded`.
    pub fn delta_prefix_matches(&self, folded: &[u8], out: &mut Vec<EntryRef>) {
        for &id in &self.delta {
            if self.store.is_live(id) && self.store.matches_prefix(id, folded) {
                out.push(EntryRef::new(self.id, id));
            }
        }
    }

    /// Iterate the live ids of a range of `order`, then the matching delta ids.
    pub fn iter_range(&self, r: Range<u32>) -> impl Iterator<Item = EntryId> + '_ {
        let lo = r.start as usize;
        let hi = (r.end as usize).min(self.order.len());
        self.order[lo..hi].iter().copied().filter(move |id| self.store.visible_in_order(*id))
    }

    /// Exact number of live entries in a range. O(1) while the index is clean,
    /// a sequential flag scan otherwise.
    pub fn count_range(&self, r: Range<u32>) -> usize {
        let span = (r.end.saturating_sub(r.start)) as usize;
        if self.dead_in_order == 0 {
            return span;
        }
        self.iter_range(r).count()
    }

    /// Full scan for predicates the sorted order cannot accelerate (substring,
    /// wildcard, regex). Parallel across chunks of the order array.
    pub fn scan<F>(&self, pred: F) -> Vec<EntryRef>
    where
        F: Fn(&EntryStore, EntryId) -> bool + Sync + Send,
    {
        let vol = self.id;
        let s = &self.store;
        // Collect per-chunk vectors and concatenate once. A rayon `reduce` that
        // appends would re-copy the growing result set at every tree level, which
        // dominates the scan whenever a query matches a large fraction of the
        // corpus.
        let parts: Vec<Vec<EntryRef>> = self
            .order
            .par_chunks(16 * 1024)
            .map(|chunk| {
                let mut local = Vec::new();
                for &id in chunk {
                    if s.visible_in_order(id) && pred(s, id) {
                        local.push(EntryRef::new(vol, id));
                    }
                }
                local
            })
            .collect();
        let found: usize = parts.iter().map(|p| p.len()).sum();
        let mut out = Vec::with_capacity(found + self.delta.len());
        for p in &parts {
            out.extend_from_slice(p);
        }
        drop(parts);
        for &id in &self.delta {
            if s.is_live(id) && pred(s, id) {
                out.push(EntryRef::new(vol, id));
            }
        }
        out
    }

    // ---- live update primitives (used by the USN applier) -------------------

    /// Insert or overwrite an entry, staging it in the delta overlay.
    ///
    /// An overwrite (rename, or a metadata change) leaves the entry's position in
    /// `order` stale, so the entry is marked staged: the sorted order skips it and
    /// the overlay serves it instead, which keeps it from being returned twice.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &mut self,
        id: EntryId,
        name: &str,
        parent: EntryId,
        size: u64,
        mtime: i64,
        seq: u16,
        is_dir: bool,
    ) {
        let was_live = self.store.is_live(id);
        let already_staged = was_live && self.store.is_staged(id);
        self.store.set_slot(id, name, parent, size, mtime, seq, is_dir);
        self.store.mark_staged(id);
        if !already_staged {
            self.delta.push(id);
            if was_live {
                self.dead_in_order += 1;
            }
        }
    }

    /// Remove an entry. The id stays in `order` as a tombstone until the merge.
    pub fn remove(&mut self, id: EntryId) {
        if !self.store.is_live(id) {
            return;
        }
        self.store.clear_slot(id);
        if let Some(pos) = self.delta.iter().position(|d| *d == id) {
            self.delta.swap_remove(pos);
        } else {
            self.dead_in_order += 1;
        }
    }

    #[inline]
    pub fn should_merge(&self) -> bool {
        self.delta.len() >= DELTA_MERGE_THRESHOLD
            || self.dead_in_order as usize >= DELTA_MERGE_THRESHOLD
    }

    /// Build the replacement order array. Pure function of the current state, so
    /// it can be computed while readers are still using the old array.
    pub fn plan_merge(&self) -> Vec<EntryId> {
        let mut added: Vec<EntryId> =
            self.delta.iter().copied().filter(|id| self.store.is_live(*id)).collect();
        sort_order(&mut added, &self.store);

        let s = &self.store;
        let mut merged = Vec::with_capacity(self.order.len() + added.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.order.len() && j < added.len() {
            let a = self.order[i];
            // Dead, or superseded by the overlay copy that `added` already holds.
            if !s.visible_in_order(a) {
                i += 1;
                continue;
            }
            let b = added[j];
            if fold::fold_cmp_raw(s.raw_name(a), s.raw_name(b)) == Ordering::Greater {
                merged.push(b);
                j += 1;
            } else {
                merged.push(a);
                i += 1;
            }
        }
        merged.extend(self.order[i..].iter().copied().filter(|id| s.visible_in_order(*id)));
        merged.extend_from_slice(&added[j..]);
        merged
    }

    /// Install a previously planned merge. O(1) plus clearing the staged bits of
    /// the (bounded) overlay — never proportional to the corpus.
    pub fn commit_merge(&mut self, merged: Vec<EntryId>) {
        for id in std::mem::take(&mut self.delta) {
            if self.store.is_live(id) {
                self.store.unmark_staged(id);
            }
        }
        self.order = merged;
        self.dead_in_order = 0;
    }

    /// Convenience for single-threaded callers (tests, CLI).
    pub fn merge_now(&mut self) {
        let m = self.plan_merge();
        self.commit_merge(m);
    }

    pub fn memory_bytes(&self) -> usize {
        self.store.memory_bytes() + self.order.capacity() * 4 + self.delta.capacity() * 4
    }
}

/// Sort ids by folded name, tie-broken by id so the order is deterministic.
fn sort_order(order: &mut [EntryId], store: &EntryStore) {
    order.par_sort_unstable_by(|a, b| {
        match fold::fold_cmp_raw(store.raw_name(*a), store.raw_name(*b)) {
            Ordering::Equal => a.cmp(b),
            o => o,
        }
    });
}

/// `slice::partition_point` over an index range, so the predicate can close over
/// several arrays at once instead of a single slice element.
#[inline]
fn partition_point<F: Fn(usize) -> bool>(len: usize, pred: F) -> usize {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The whole index: one `VolumeIndex` per volume, each independently locked so a
/// USN update on `D:` never blocks a query against `C:`.
pub struct Index {
    pub volumes: Vec<RwLock<VolumeIndex>>,
}

impl Index {
    pub fn new(volumes: Vec<VolumeIndex>) -> Self {
        Index { volumes: volumes.into_iter().map(RwLock::new).collect() }
    }

    pub fn total_entries(&self) -> usize {
        self.volumes.iter().map(|v| v.read().unwrap().len()).sum()
    }

    pub fn memory_bytes(&self) -> usize {
        self.volumes.iter().map(|v| v.read().unwrap().memory_bytes()).sum()
    }

    pub fn volume_labels(&self) -> Vec<String> {
        self.volumes.iter().map(|v| v.read().unwrap().store.label.clone()).collect()
    }

    /// Resolve a match to `(full path, is_dir, size, mtime)` for display. Called
    /// only for rows actually on screen.
    pub fn describe(&self, r: EntryRef) -> Option<EntryInfo> {
        let vol = self.volumes.get(r.volume() as usize)?.read().unwrap();
        let id = r.entry();
        if !vol.store.is_live(id) {
            return None;
        }
        Some(EntryInfo {
            name: vol.store.name(id).to_string(),
            parent_path: vol.store.parent_path(id),
            is_dir: vol.store.is_dir(id),
            size: vol.store.size(id),
            mtime: vol.store.mtime(id),
        })
    }
}

#[derive(Clone, Debug)]
pub struct EntryInfo {
    pub name: String,
    pub parent_path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

impl EntryInfo {
    pub fn full_path(&self) -> String {
        if self.parent_path.ends_with('\\') {
            format!("{}{}", self.parent_path, self.name)
        } else {
            format!("{}\\{}", self.parent_path, self.name)
        }
    }
}

/// Build a synthetic volume index. Used by benchmarks and tests to exercise the
/// query path at arbitrary corpus sizes without touching a real disk.
pub fn synthetic_volume(id: VolumeId, label: &str, entries: usize) -> VolumeIndex {
    const WORDS: [&str; 24] = [
        "report", "Setup", "image", "backup", "config", "README", "index", "data", "cache",
        "Module", "driver", "system", "user", "temp", "log", "archive", "Project", "notes",
        "video", "audio", "script", "binary", "Sample", "test",
    ];
    const EXTS: [&str; 10] =
        [".txt", ".dll", ".exe", ".png", ".log", ".json", ".rs", ".md", ".dat", ""];

    // Reserve rather than pre-size: `push` appends, so pre-sizing would leave
    // `entries` empty slots in front of the real data and double the footprint.
    let mut store = EntryStore::new(label);
    store.reserve(entries + 1, entries * 22);
    let root = store.push("", INVALID_ENTRY, 0, 0, true);
    store.set_root(root);

    // Deterministic xorshift so benchmark numbers are reproducible run to run.
    let mut rng: u64 = 0x9E3779B97F4A7C15 ^ entries as u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let mut dirs: Vec<EntryId> = vec![root];
    for i in 0..entries {
        let r = next();
        let w1 = WORDS[(r % 24) as usize];
        let w2 = WORDS[((r >> 8) % 24) as usize];
        let ext = EXTS[((r >> 16) % 10) as usize];
        let name = format!("{}_{}{:04}{}", w1, w2, (r >> 24) % 10000, ext);
        let parent = dirs[(r >> 40) as usize % dirs.len()];
        let is_dir = i % 12 == 0;
        let id = store.push(&name, parent, (r >> 20) & 0xFF_FFFF, r as i64, is_dir);
        if is_dir && dirs.len() < 200_000 {
            dirs.push(id);
        }
    }
    VolumeIndex::build(id, SourceKind::Walk, store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::fold_to_string;

    fn small() -> VolumeIndex {
        let mut s = EntryStore::new("C:");
        let root = s.push("", INVALID_ENTRY, 0, 0, true);
        s.set_root(root);
        for n in ["apple.txt", "Apricot.md", "banana", "APPENDIX", "cherry", "app"] {
            s.push(n, root, 10, 0, false);
        }
        VolumeIndex::build(0, SourceKind::Walk, s)
    }

    fn names_in(v: &VolumeIndex, r: Range<u32>) -> Vec<String> {
        v.iter_range(r).map(|id| v.store.name(id).to_string()).collect()
    }

    #[test]
    fn order_is_case_insensitive_alphabetical() {
        let v = small();
        let all = names_in(&v, 0..v.order().len() as u32);
        assert_eq!(all, ["", "app", "APPENDIX", "apple.txt", "Apricot.md", "banana", "cherry"]);
    }

    #[test]
    fn prefix_range_is_contiguous_and_exact() {
        let v = small();
        let r = v.prefix_range(&fold_to_string("app"));
        assert_eq!(v.count_range(r.clone()), 3);
        assert_eq!(names_in(&v, r), ["app", "APPENDIX", "apple.txt"]);

        let r = v.prefix_range(&fold_to_string("APPLE"));
        assert_eq!(names_in(&v, r), ["apple.txt"]);

        let r = v.prefix_range(&fold_to_string("zzz"));
        assert_eq!(v.count_range(r), 0);
    }

    #[test]
    fn empty_prefix_matches_everything() {
        let v = small();
        let r = v.prefix_range(b"");
        assert_eq!(v.count_range(r), 7);
    }

    #[test]
    fn delta_serves_new_entries_before_merge() {
        let mut v = small();
        let id = v.store.slots() as EntryId;
        v.upsert(id, "appendix2.doc", 0, 5, 0, 0, false);

        // Not in the sorted range yet...
        let r = v.prefix_range(&fold_to_string("appendix"));
        assert_eq!(v.count_range(r), 1);
        // ...but visible through the overlay.
        let mut hits = Vec::new();
        v.delta_prefix_matches(&fold_to_string("appendix"), &mut hits);
        assert_eq!(hits.len(), 1);

        // After the merge it is in the main array and the overlay is empty.
        v.merge_now();
        assert!(v.delta().is_empty());
        let r = v.prefix_range(&fold_to_string("appendix"));
        assert_eq!(names_in(&v, r), ["APPENDIX", "appendix2.doc"]);
    }

    #[test]
    fn removal_tombstones_then_compacts() {
        let mut v = small();
        let target = v.order().iter().copied().find(|id| v.store.name(*id) == "banana").unwrap();
        v.remove(target);

        assert!(!v.is_clean());
        let r = v.prefix_range(&fold_to_string("ban"));
        assert_eq!(v.count_range(r), 0, "tombstoned entry must not be returned");
        assert_eq!(v.len(), 6);

        v.merge_now();
        assert!(v.is_clean());
        assert_eq!(v.order().len(), 6);
    }

    #[test]
    fn rename_moves_entry_in_the_order() {
        let mut v = small();
        let target = v.order().iter().copied().find(|id| v.store.name(*id) == "cherry").unwrap();
        v.upsert(target, "aardvark", 0, 1, 0, 0, false);
        v.merge_now();
        let all = names_in(&v, 0..v.order().len() as u32);
        assert_eq!(all, ["", "aardvark", "app", "APPENDIX", "apple.txt", "Apricot.md", "banana"]);
    }

    #[test]
    fn merge_preserves_sort_invariant_over_many_ops() {
        let mut v = synthetic_volume(0, "C:", 5_000);
        let base = v.store.slots() as EntryId;
        for k in 0..500u32 {
            v.upsert(base + k, &format!("zz_added_{k:04}"), 0, 1, 0, 0, false);
        }
        for k in 0..200u32 {
            v.remove(v.order()[k as usize * 7]);
        }
        v.merge_now();

        let ord = v.order().to_vec();
        for w in ord.windows(2) {
            assert_ne!(
                fold::fold_cmp_raw(v.store.raw_name(w[0]), v.store.raw_name(w[1])),
                Ordering::Greater,
                "order broken after merge"
            );
            assert!(v.store.is_live(w[0]));
        }
        assert_eq!(v.len(), 5_001 + 500 - 200);
    }

    #[test]
    fn scan_finds_substrings_the_prefix_path_cannot() {
        let v = small();
        let needle = fold_to_string("ricot");
        let hits = v.scan(|s, id| s.matches_substring(id, &needle));
        assert_eq!(hits.len(), 1);
        assert_eq!(v.store.name(hits[0].entry()), "Apricot.md");
    }
}
