//! Struct-of-arrays entry storage.
//!
//! One `EntryStore` per volume. Nothing is heap-allocated per file: every field
//! lives in a contiguous `Vec` indexed by `EntryId`, and names are packed
//! end-to-end into a single byte blob addressed by `(offset, len)`.
//!
//! On NTFS volumes `EntryId` is deliberately *equal to the MFT record number*.
//! MFT record numbers are dense, so using them directly as the array index buys
//! two things: the parallel build can write straight into a preallocated array
//! with no merge step, and a USN journal record becomes an index update with a
//! single array subscript instead of a hash lookup. The cost is the small number
//! of unused MFT records left as holes.

use crate::fold;

pub type EntryId = u32;
pub const INVALID_ENTRY: EntryId = u32::MAX;

pub mod flags {
    /// Record is live (in use). Cleared records are holes or tombstones.
    pub const IN_USE: u8 = 1 << 0;
    pub const IS_DIR: u8 = 1 << 1;
    /// Name contains bytes >= 0x80; comparison must take the Unicode path.
    pub const NON_ASCII: u8 = 1 << 2;
    /// The volume root. Path walks stop here.
    pub const IS_ROOT: u8 = 1 << 3;
    /// Entry is currently served from the delta overlay. Its position in the
    /// sorted order (if it has one) is stale and must be skipped, otherwise the
    /// entry would be returned twice.
    pub const STAGED: u8 = 1 << 4;
}

/// Identifies a volume within an `Index`.
pub type VolumeId = u16;

/// A match: which volume, and which entry on it. Packed into 8 bytes so result
/// vectors stay cache-friendly at hundreds of thousands of rows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct EntryRef(pub u64);

impl EntryRef {
    #[inline]
    pub fn new(vol: VolumeId, id: EntryId) -> Self {
        EntryRef(((vol as u64) << 32) | id as u64)
    }
    #[inline]
    pub fn volume(self) -> VolumeId {
        (self.0 >> 32) as VolumeId
    }
    #[inline]
    pub fn entry(self) -> EntryId {
        self.0 as u32
    }
}

#[derive(Default)]
pub struct EntryStore {
    /// Drive letter or mount label, e.g. `C:`.
    pub label: String,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    parent: Vec<EntryId>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    /// NTFS sequence number, used to reject stale file reference numbers.
    seq: Vec<u16>,
    flag: Vec<u8>,
    names: Vec<u8>,
    root: EntryId,
    live: u32,
}

impl EntryStore {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), root: INVALID_ENTRY, ..Default::default() }
    }

    /// Preallocate for a known record count (the NTFS path knows this up front).
    pub fn with_capacity(label: impl Into<String>, records: usize, name_bytes: usize) -> Self {
        let mut s = Self::new(label);
        s.resize_slots(records);
        s.names.reserve(name_bytes);
        s
    }

    /// Reserve capacity without creating slots. Used by builders that append
    /// with `push`, so the arrays never pay for growth-doubling overshoot.
    pub fn reserve(&mut self, records: usize, name_bytes: usize) {
        self.name_off.reserve_exact(records);
        self.name_len.reserve_exact(records);
        self.parent.reserve_exact(records);
        self.size.reserve_exact(records);
        self.mtime.reserve_exact(records);
        self.seq.reserve_exact(records);
        self.flag.reserve_exact(records);
        self.names.reserve_exact(name_bytes);
    }

    /// Drop trailing dead slots and release reserved-but-unused capacity.
    ///
    /// The MFT build sizes the arrays from the MFT's *valid length*, which is an
    /// upper bound: the tail is usually unused records, and the name blob is
    /// reserved from an estimate. Both are pure overhead once the build is done,
    /// and both are charged in full by `memory_bytes`, which counts capacity.
    ///
    /// Interior holes are kept: `EntryId` is the MFT record number, and
    /// renumbering would break both parent references and USN application.
    pub fn shrink_to_fit(&mut self) {
        let last_live = (0..self.flag.len()).rev().find(|i| self.flag[*i] & flags::IN_USE != 0);
        let keep = last_live.map(|i| i + 1).unwrap_or(0);
        if keep < self.flag.len() {
            self.name_off.truncate(keep);
            self.name_len.truncate(keep);
            self.parent.truncate(keep);
            self.size.truncate(keep);
            self.mtime.truncate(keep);
            self.seq.truncate(keep);
            self.flag.truncate(keep);
        }
        self.name_off.shrink_to_fit();
        self.name_len.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.seq.shrink_to_fit();
        self.flag.shrink_to_fit();
        self.names.shrink_to_fit();
    }

    /// Grow the slot arrays, marking every new slot as a hole.
    pub fn resize_slots(&mut self, n: usize) {
        if n <= self.flag.len() {
            return;
        }
        self.name_off.resize(n, 0);
        self.name_len.resize(n, 0);
        self.parent.resize(n, INVALID_ENTRY);
        self.size.resize(n, 0);
        self.mtime.resize(n, 0);
        self.seq.resize(n, 0);
        self.flag.resize(n, 0);
    }

    #[inline]
    pub fn slots(&self) -> usize {
        self.flag.len()
    }
    #[inline]
    pub fn live_count(&self) -> u32 {
        self.live
    }
    #[inline]
    pub fn name_bytes(&self) -> usize {
        self.names.len()
    }
    #[inline]
    pub fn root(&self) -> EntryId {
        self.root
    }

    pub fn set_root(&mut self, id: EntryId) {
        self.root = id;
        if (id as usize) < self.slots() {
            self.flag[id as usize] |= flags::IS_ROOT;
        }
    }

    /// Append a name to the blob, returning `(offset, len, non_ascii)`.
    fn intern_name(&mut self, name: &str) -> (u32, u16, bool) {
        let off = self.names.len() as u32;
        let bytes = name.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.names.extend_from_slice(&bytes[..len]);
        (off, len as u16, !name.is_ascii())
    }

    /// Fill a specific slot. Used by the NTFS path, where the slot is the MFT
    /// record number.
    #[allow(clippy::too_many_arguments)]
    pub fn set_slot(
        &mut self,
        id: EntryId,
        name: &str,
        parent: EntryId,
        size: u64,
        mtime: i64,
        seq: u16,
        is_dir: bool,
    ) {
        let i = id as usize;
        if i >= self.slots() {
            self.resize_slots(i + 1);
        }
        let (off, len, non_ascii) = self.intern_name(name);
        if self.flag[i] & flags::IN_USE == 0 {
            self.live += 1;
        }
        self.name_off[i] = off;
        self.name_len[i] = len;
        self.parent[i] = parent;
        self.size[i] = size;
        self.mtime[i] = mtime;
        self.seq[i] = seq;
        let mut f = flags::IN_USE;
        if is_dir {
            f |= flags::IS_DIR;
        }
        if non_ascii {
            f |= flags::NON_ASCII;
        }
        self.flag[i] = f | (self.flag[i] & flags::IS_ROOT);
    }

    /// Append a new entry at the next free slot. Used by the `FindFirstFile`
    /// fallback, which has no natural dense id.
    pub fn push(
        &mut self,
        name: &str,
        parent: EntryId,
        size: u64,
        mtime: i64,
        is_dir: bool,
    ) -> EntryId {
        let id = self.slots() as EntryId;
        self.resize_slots(id as usize + 1);
        self.set_slot(id, name, parent, size, mtime, 0, is_dir);
        id
    }

    /// Point an entry at a different parent. Used to re-home orphans whose
    /// parent record did not survive the MFT scan.
    pub fn reparent(&mut self, id: EntryId, parent: EntryId) {
        self.parent[id as usize] = parent;
    }

    /// Mark a slot dead. Name bytes stay in the blob until a compaction.
    pub fn clear_slot(&mut self, id: EntryId) {
        let i = id as usize;
        if i < self.slots() && self.flag[i] & flags::IN_USE != 0 {
            self.flag[i] = 0;
            self.live -= 1;
        }
    }

    #[inline]
    pub fn is_live(&self, id: EntryId) -> bool {
        let i = id as usize;
        i < self.flag.len() && self.flag[i] & flags::IN_USE != 0
    }
    #[inline]
    pub fn is_dir(&self, id: EntryId) -> bool {
        self.flag[id as usize] & flags::IS_DIR != 0
    }
    #[inline]
    pub fn size(&self, id: EntryId) -> u64 {
        self.size[id as usize]
    }
    #[inline]
    pub fn mtime(&self, id: EntryId) -> i64 {
        self.mtime[id as usize]
    }
    #[inline]
    pub fn parent(&self, id: EntryId) -> EntryId {
        self.parent[id as usize]
    }
    #[inline]
    pub fn seq(&self, id: EntryId) -> u16 {
        self.seq[id as usize]
    }
    #[inline]
    pub fn is_non_ascii(&self, id: EntryId) -> bool {
        self.flag[id as usize] & flags::NON_ASCII != 0
    }
    #[inline]
    pub fn is_staged(&self, id: EntryId) -> bool {
        self.flag[id as usize] & flags::STAGED != 0
    }
    /// Live, and not superseded by a delta-overlay copy. This is the visibility
    /// test for anything walking the sorted order.
    #[inline]
    pub fn visible_in_order(&self, id: EntryId) -> bool {
        let i = id as usize;
        i < self.flag.len() && self.flag[i] & (flags::IN_USE | flags::STAGED) == flags::IN_USE
    }
    #[inline]
    pub fn mark_staged(&mut self, id: EntryId) {
        self.flag[id as usize] |= flags::STAGED;
    }
    #[inline]
    pub fn unmark_staged(&mut self, id: EntryId) {
        self.flag[id as usize] &= !flags::STAGED;
    }

    /// The raw (unfolded) name bytes for an entry.
    #[inline]
    pub fn name_bytes_of(&self, id: EntryId) -> &[u8] {
        let i = id as usize;
        let off = self.name_off[i] as usize;
        &self.names[off..off + self.name_len[i] as usize]
    }

    #[inline]
    pub fn name(&self, id: EntryId) -> &str {
        // Names came from `&str`, so the blob slice is valid UTF-8 by construction.
        std::str::from_utf8(self.name_bytes_of(id)).unwrap_or("")
    }

    /// Reconstruct the full path by walking the parent chain. Done on demand for
    /// the handful of rows actually on screen, never stored per entry.
    pub fn path(&self, id: EntryId) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(8);
        let mut cur = id;
        let mut guard = 0;
        while cur != INVALID_ENTRY && guard < 256 {
            let i = cur as usize;
            if i >= self.slots() || self.flag[i] & flags::IN_USE == 0 {
                break;
            }
            if self.flag[i] & flags::IS_ROOT != 0 {
                break;
            }
            parts.push(self.name(cur));
            let p = self.parent[i];
            if p == cur {
                break;
            }
            cur = p;
            guard += 1;
        }
        let mut out = String::with_capacity(self.label.len() + 1 + parts.len() * 12);
        out.push_str(&self.label);
        for part in parts.iter().rev() {
            out.push('\\');
            out.push_str(part);
        }
        if parts.is_empty() {
            out.push('\\');
        }
        out
    }

    /// Path of the containing directory (the UI's "Path" column).
    pub fn parent_path(&self, id: EntryId) -> String {
        let p = self.parent(id);
        if p == INVALID_ENTRY || p == id {
            format!("{}\\", self.label)
        } else {
            self.path(p)
        }
    }

    /// Iterate the ids of all live entries.
    pub fn live_ids(&self) -> impl Iterator<Item = EntryId> + '_ {
        (0..self.slots() as EntryId).filter(|id| self.flag[*id as usize] & flags::IN_USE != 0)
    }

    /// Approximate resident bytes, for the benchmark harness.
    pub fn memory_bytes(&self) -> usize {
        self.name_off.capacity() * 4
            + self.name_len.capacity() * 2
            + self.parent.capacity() * 4
            + self.size.capacity() * 8
            + self.mtime.capacity() * 8
            + self.seq.capacity() * 2
            + self.flag.capacity()
            + self.names.capacity()
    }

    #[inline]
    pub fn matches_prefix(&self, id: EntryId, folded: &[u8]) -> bool {
        fold::fold_starts_with(self.name_bytes_of(id), folded)
    }
    #[inline]
    pub fn matches_substring(&self, id: EntryId, folded: &[u8]) -> bool {
        fold::fold_contains(self.name_bytes_of(id), folded)
    }

    /// Raw access for the sorted-index builder, which needs to compare names
    /// without going through per-id bounds checks in the hot sort loop.
    #[inline]
    pub(crate) fn raw_name(&self, id: EntryId) -> &[u8] {
        self.name_bytes_of(id)
    }

    /// Borrow the backing arrays, for snapshot writing.
    pub fn raw_arrays(&self) -> RawArrays<'_> {
        RawArrays {
            name_off: &self.name_off,
            name_len: &self.name_len,
            parent: &self.parent,
            size: &self.size,
            mtime: &self.mtime,
            seq: &self.seq,
            flag: &self.flag,
            names: &self.names,
        }
    }

    /// Rebuild a store from arrays read back off disk.
    ///
    /// Everything a snapshot could get wrong is checked here rather than trusted:
    /// a corrupt file must produce an error, never an out-of-bounds read during a
    /// later query.
    #[allow(clippy::too_many_arguments)]
    pub fn from_raw_arrays(
        label: String,
        root: EntryId,
        name_off: Vec<u32>,
        name_len: Vec<u16>,
        parent: Vec<EntryId>,
        size: Vec<u64>,
        mtime: Vec<i64>,
        seq: Vec<u16>,
        flag: Vec<u8>,
        names: Vec<u8>,
    ) -> Result<Self, crate::Error> {
        let n = flag.len();
        if name_off.len() != n
            || name_len.len() != n
            || parent.len() != n
            || size.len() != n
            || mtime.len() != n
            || seq.len() != n
        {
            return Err(crate::Error::StaleSnapshot("entry arrays disagree on length".into()));
        }
        let mut live = 0u32;
        for i in 0..n {
            if flag[i] & flags::IN_USE == 0 {
                continue;
            }
            live += 1;
            let end = name_off[i] as usize + name_len[i] as usize;
            if end > names.len() {
                return Err(crate::Error::StaleSnapshot(
                    "entry name extends past the name blob".into(),
                ));
            }
        }
        let mut store = EntryStore {
            label,
            name_off,
            name_len,
            parent,
            size,
            mtime,
            seq,
            flag,
            names,
            root,
            live,
        };
        // A snapshot is always taken in the merged state; carrying a stale STAGED
        // bit would hide entries from the sorted order forever.
        for f in store.flag.iter_mut() {
            *f &= !flags::STAGED;
        }
        if (root as usize) < n {
            store.flag[root as usize] |= flags::IS_ROOT;
        }
        Ok(store)
    }
}

/// Borrowed view of the backing arrays.
pub struct RawArrays<'a> {
    pub name_off: &'a [u32],
    pub name_len: &'a [u16],
    pub parent: &'a [EntryId],
    pub size: &'a [u64],
    pub mtime: &'a [i64],
    pub seq: &'a [u16],
    pub flag: &'a [u8],
    pub names: &'a [u8],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_read_back() {
        let mut s = EntryStore::new("C:");
        let root = s.push("", INVALID_ENTRY, 0, 0, true);
        s.set_root(root);
        let dir = s.push("Windows", root, 0, 0, true);
        let file = s.push("notepad.exe", dir, 4096, 12345, false);

        assert_eq!(s.name(file), "notepad.exe");
        assert_eq!(s.size(file), 4096);
        assert!(s.is_dir(dir));
        assert!(!s.is_dir(file));
        assert_eq!(s.live_count(), 3);
        assert_eq!(s.path(file), "C:\\Windows\\notepad.exe");
        assert_eq!(s.parent_path(file), "C:\\Windows");
    }

    #[test]
    fn clear_makes_a_hole() {
        let mut s = EntryStore::new("C:");
        let a = s.push("a", INVALID_ENTRY, 0, 0, false);
        s.push("b", INVALID_ENTRY, 0, 0, false);
        assert_eq!(s.live_count(), 2);
        s.clear_slot(a);
        assert_eq!(s.live_count(), 1);
        assert!(!s.is_live(a));
        assert_eq!(s.live_ids().count(), 1);
    }

    #[test]
    fn path_walk_terminates_on_cycles() {
        let mut s = EntryStore::new("C:");
        let a = s.push("a", 0, 0, 0, true);
        // Self-parent without the root flag: must not spin.
        assert_eq!(s.path(a), "C:\\a");
    }

    #[test]
    fn shrink_reclaims_the_tail_but_keeps_interior_holes() {
        let mut s = EntryStore::with_capacity("C:", 1000, 100_000);
        s.set_slot(5, "", 5, 0, 0, 1, true);
        s.set_root(5);
        s.set_slot(10, "keep.txt", 5, 1, 0, 1, false);
        s.set_slot(20, "also.txt", 5, 1, 0, 1, false);
        s.clear_slot(10);
        assert_eq!(s.slots(), 1000);
        let before = s.memory_bytes();

        s.shrink_to_fit();

        // Trailing unused slots are gone; the hole at 10 is not renumbered,
        // because ids are MFT record numbers.
        assert_eq!(s.slots(), 21);
        assert!(s.memory_bytes() < before / 2, "{} vs {before}", s.memory_bytes());
        assert!(s.is_live(20));
        assert_eq!(s.name(20), "also.txt");
        assert!(!s.is_live(10));
        assert_eq!(s.path(20), "C:\\also.txt");
        assert_eq!(s.live_count(), 2);
    }

    #[test]
    fn shrink_of_an_empty_store_is_harmless() {
        let mut s = EntryStore::with_capacity("C:", 500, 5000);
        s.shrink_to_fit();
        assert_eq!(s.slots(), 0);
        assert_eq!(s.live_count(), 0);
    }

    #[test]
    fn entry_ref_packs_and_unpacks() {
        let r = EntryRef::new(3, 1234567);
        assert_eq!(r.volume(), 3);
        assert_eq!(r.entry(), 1234567);
    }
}
