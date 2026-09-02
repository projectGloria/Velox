//! On-disk index snapshots.
//!
//! Re-reading the MFT on every launch is fast but not free. A snapshot lets a
//! restart load the index at disk-read speed and then catch up on whatever the
//! USN journal recorded while we were away.
//!
//! # Format
//!
//! A flat dump of the struct-of-arrays, because that is already the in-memory
//! layout: loading is a read into pre-sized vectors, not a parse. Nothing is
//! compressed and nothing is validated field by field, so the header carries a
//! version and per-volume identity, and a mismatch means "rebuild" rather than
//! "repair".
//!
//! ```text
//! "VELOXIDX"  u32 version  u32 volume_count
//! per volume:
//!   u16 id | u8 source_kind | u32 root | u64 slots | u64 name_bytes | u64 order_len
//!   u64 volume_serial | u64 journal_id | i64 next_usn | u32 label_len | label
//!   name_off[] name_len[] parent[] size[] mtime[] seq[] flag[] names[] order[]
//! ```
//!
//! # Freshness
//!
//! A snapshot is only usable if the volume's USN journal still has the same
//! journal id it had when the snapshot was taken. A different id means the
//! journal was deleted and recreated (or the volume reformatted), so every USN
//! we hold is meaningless and the volume must be rebuilt from the MFT. That is
//! reported as [`Error::StaleSnapshot`].

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::index::{Index, SourceKind, VolumeIndex};
use crate::store::{EntryId, EntryStore, VolumeId};
use crate::Error;

#[cfg(not(target_endian = "little"))]
compile_error!("the snapshot format dumps native little-endian integers");

const MAGIC: &[u8; 8] = b"VELOXIDX";
/// Bump whenever the store layout changes. Old snapshots are then discarded
/// rather than misread.
const FORMAT_VERSION: u32 = 1;

/// Per-volume identity recorded alongside the data, used to decide whether a
/// snapshot can still be trusted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VolumeStamp {
    pub volume_serial: u64,
    pub journal_id: u64,
    pub next_usn: i64,
}

/// Default snapshot location: `%LOCALAPPDATA%\Velox\index.veloxidx`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Velox").join("index.veloxidx")
}

// ---------------------------------------------------------- byte-level helpers

/// Types whose in-memory representation can be dumped and reloaded verbatim.
///
/// # Safety
/// Implementors must be plain integers with no padding and no invalid bit
/// patterns, so any byte sequence of the right length is a valid value.
unsafe trait Raw: Copy {}
unsafe impl Raw for u8 {}
unsafe impl Raw for u16 {}
unsafe impl Raw for u32 {}
unsafe impl Raw for u64 {}
unsafe impl Raw for i64 {}

fn write_slice<T: Raw, W: Write>(w: &mut W, v: &[T]) -> std::io::Result<()> {
    // SAFETY: `T: Raw` is a plain integer, and going from a higher alignment to
    // `u8` is always valid.
    let bytes =
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) };
    w.write_all(bytes)
}

fn read_vec<T: Raw + Default, R: Read>(r: &mut R, len: usize) -> std::io::Result<Vec<T>> {
    let mut v: Vec<T> = vec![T::default(); len];
    // SAFETY: the vector owns `len * size_of::<T>()` bytes, correctly aligned for
    // `T`; every bit pattern is a valid `T` by the `Raw` contract.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, len * std::mem::size_of::<T>())
    };
    r.read_exact(bytes)?;
    Ok(v)
}

fn write_u32<W: Write>(w: &mut W, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn write_u64<W: Write>(w: &mut W, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn read_u32<R: Read>(r: &mut R) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64<R: Read>(r: &mut R) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

// -------------------------------------------------------------------- saving

/// Write a snapshot of the whole index.
///
/// `stamps` must be parallel to `index.volumes`; pass `VolumeStamp::default()`
/// for volumes with no journal (the `FindFirstFile` fallback), which simply
/// means the snapshot cannot be validated for them and they will be rebuilt.
pub fn save(index: &Index, stamps: &[VolumeStamp], path: &Path) -> Result<u64, Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write to a sibling and rename, so an interrupted save never leaves a
    // half-written snapshot that would be loaded on the next launch.
    let tmp = path.with_extension("veloxidx.tmp");
    {
        let file = File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(4 << 20, file);
        w.write_all(MAGIC)?;
        write_u32(&mut w, FORMAT_VERSION)?;
        write_u32(&mut w, index.volumes.len() as u32)?;

        for (i, vol) in index.volumes.iter().enumerate() {
            let vol = vol.read().unwrap();
            let stamp = stamps.get(i).copied().unwrap_or_default();
            let raw = vol.store.raw_arrays();

            w.write_all(&vol.id.to_le_bytes())?;
            w.write_all(&[match vol.kind {
                SourceKind::Mft => 0u8,
                SourceKind::Walk => 1u8,
            }])?;
            write_u32(&mut w, vol.store.root())?;
            write_u64(&mut w, raw.flag.len() as u64)?;
            write_u64(&mut w, raw.names.len() as u64)?;
            write_u64(&mut w, vol.order().len() as u64)?;
            write_u64(&mut w, stamp.volume_serial)?;
            write_u64(&mut w, stamp.journal_id)?;
            write_u64(&mut w, stamp.next_usn as u64)?;
            let label = vol.store.label.as_bytes();
            write_u32(&mut w, label.len() as u32)?;
            w.write_all(label)?;

            write_slice(&mut w, raw.name_off)?;
            write_slice(&mut w, raw.name_len)?;
            write_slice(&mut w, raw.parent)?;
            write_slice(&mut w, raw.size)?;
            write_slice(&mut w, raw.mtime)?;
            write_slice(&mut w, raw.seq)?;
            write_slice(&mut w, raw.flag)?;
            write_slice(&mut w, raw.names)?;
            write_slice(&mut w, vol.order())?;
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(std::fs::metadata(path)?.len())
}

// ------------------------------------------------------------------- loading

pub struct LoadedIndex {
    pub index: Index,
    pub stamps: Vec<VolumeStamp>,
}

/// Read a snapshot back.
///
/// Does *not* check freshness — that needs a live volume handle. Call
/// [`check_fresh`] per volume afterwards.
pub fn load(path: &Path) -> Result<LoadedIndex, Error> {
    let file = File::open(path)?;
    let mut r = BufReader::with_capacity(4 << 20, file);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(Error::StaleSnapshot("not a Velox index file".into()));
    }
    let version = read_u32(&mut r)?;
    if version != FORMAT_VERSION {
        return Err(Error::StaleSnapshot(format!(
            "snapshot format v{version}, this build writes v{FORMAT_VERSION}"
        )));
    }
    let count = read_u32(&mut r)? as usize;
    // A corrupt count must not trigger a huge speculative allocation.
    if count > 64 {
        return Err(Error::StaleSnapshot(format!("implausible volume count {count}")));
    }

    let mut volumes = Vec::with_capacity(count);
    let mut stamps = Vec::with_capacity(count);
    for _ in 0..count {
        let mut idb = [0u8; 2];
        r.read_exact(&mut idb)?;
        let id = VolumeId::from_le_bytes(idb);
        let mut kindb = [0u8; 1];
        r.read_exact(&mut kindb)?;
        let kind = if kindb[0] == 0 { SourceKind::Mft } else { SourceKind::Walk };
        let root = read_u32(&mut r)?;
        let slots = read_u64(&mut r)? as usize;
        let name_bytes = read_u64(&mut r)? as usize;
        let order_len = read_u64(&mut r)? as usize;
        let stamp = VolumeStamp {
            volume_serial: read_u64(&mut r)?,
            journal_id: read_u64(&mut r)?,
            next_usn: read_u64(&mut r)? as i64,
        };
        let label_len = read_u32(&mut r)? as usize;
        let mut label = vec![0u8; label_len];
        r.read_exact(&mut label)?;
        let label = String::from_utf8_lossy(&label).into_owned();

        if order_len > slots {
            return Err(Error::StaleSnapshot("order longer than the slot array".into()));
        }

        let store = EntryStore::from_raw_arrays(
            label,
            root,
            read_vec(&mut r, slots)?,
            read_vec(&mut r, slots)?,
            read_vec(&mut r, slots)?,
            read_vec(&mut r, slots)?,
            read_vec(&mut r, slots)?,
            read_vec(&mut r, slots)?,
            read_vec(&mut r, slots)?,
            read_vec(&mut r, name_bytes)?,
        )?;
        let order: Vec<EntryId> = read_vec(&mut r, order_len)?;
        volumes.push(VolumeIndex::from_parts(id, kind, store, order));
        stamps.push(stamp);
    }
    Ok(LoadedIndex { index: Index::new(volumes), stamps })
}

/// Is a loaded volume snapshot still usable?
///
/// Returns the checkpoint to resume the journal from, or an error saying the
/// volume must be rebuilt from the MFT.
pub fn check_fresh(stamp: VolumeStamp, current_journal_id: u64, label: &str) -> Result<i64, Error> {
    if stamp.journal_id == 0 {
        return Err(Error::StaleSnapshot(format!(
            "{label} was indexed without a journal checkpoint"
        )));
    }
    if stamp.journal_id != current_journal_id {
        return Err(Error::JournalInvalidated(label.to_string()));
    }
    Ok(stamp.next_usn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::fold_to_string;
    use crate::index::synthetic_volume;
    use crate::query::{MatchMode, Query, Scope, Searcher};

    fn temp_file(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("velox-persist-{tag}-{stamp}.veloxidx"))
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("veloxidx.tmp"));
        }
    }

    #[test]
    fn round_trip_preserves_every_entry_and_query_result() {
        let path = temp_file("roundtrip");
        let _c = Cleanup(path.clone());

        let index = Index::new(vec![synthetic_volume(0, "C:", 20_000)]);
        let before_total = index.total_entries();
        let stamps = vec![VolumeStamp { volume_serial: 7, journal_id: 42, next_usn: 999 }];
        let bytes = save(&index, &stamps, &path).expect("save");
        assert!(bytes > 0);

        let loaded = load(&path).expect("load");
        assert_eq!(loaded.stamps, stamps);
        assert_eq!(loaded.index.total_entries(), before_total);

        // Same queries, same answers, same order.
        let mut a = Searcher::new();
        let mut b = Searcher::new();
        for q in ["rep", "setup", "z", "config", ""] {
            let query = Query::parse(q, MatchMode::Prefix, Scope::Name);
            let ra = a.search(&index, &query);
            let rb = b.search(&loaded.index, &query);
            assert_eq!(ra.total, rb.total, "totals differ for `{q}`");
            let wa: Vec<String> = ra
                .window(&index, 0, 50)
                .iter()
                .filter_map(|r| index.describe(*r))
                .map(|i| i.full_path())
                .collect();
            let wb: Vec<String> = rb
                .window(&loaded.index, 0, 50)
                .iter()
                .filter_map(|r| loaded.index.describe(*r))
                .map(|i| i.full_path())
                .collect();
            assert_eq!(wa, wb, "results differ for `{q}`");
        }
    }

    #[test]
    fn loaded_index_still_accepts_live_updates() {
        let path = temp_file("live");
        let _c = Cleanup(path.clone());
        let index = Index::new(vec![synthetic_volume(0, "C:", 5_000)]);
        save(&index, &[VolumeStamp::default()], &path).unwrap();

        let loaded = load(&path).unwrap();
        {
            let mut vol = loaded.index.volumes[0].write().unwrap();
            let id = vol.store.slots() as EntryId;
            vol.upsert(id, "zzz_after_load.txt", 0, 10, 0, 0, false);
            vol.merge_now();
            let r = vol.prefix_range(&fold_to_string("zzz_after_load"));
            assert_eq!(vol.count_range(r), 1);
        }
    }

    #[test]
    fn multiple_volumes_round_trip_independently() {
        let path = temp_file("multi");
        let _c = Cleanup(path.clone());
        let index = Index::new(vec![
            synthetic_volume(0, "C:", 2_000),
            synthetic_volume(1, "D:", 1_000),
        ]);
        let stamps = vec![
            VolumeStamp { volume_serial: 1, journal_id: 10, next_usn: 5 },
            VolumeStamp { volume_serial: 2, journal_id: 20, next_usn: 6 },
        ];
        save(&index, &stamps, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.index.volumes.len(), 2);
        assert_eq!(loaded.index.volume_labels(), vec!["C:", "D:"]);
        assert_eq!(loaded.stamps, stamps);
        assert_eq!(loaded.index.total_entries(), index.total_entries());
    }

    #[test]
    fn rejects_foreign_and_versioned_files() {
        let path = temp_file("foreign");
        let _c = Cleanup(path.clone());
        std::fs::write(&path, b"not an index at all, just some bytes").unwrap();
        assert!(matches!(load(&path), Err(Error::StaleSnapshot(_))));

        let mut bad = MAGIC.to_vec();
        bad.extend_from_slice(&999u32.to_le_bytes());
        bad.extend_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, &bad).unwrap();
        match load(&path) {
            Err(Error::StaleSnapshot(m)) => assert!(m.contains("v999"), "{m}"),
            Err(e) => panic!("expected a version mismatch, got {e}"),
            Ok(_) => panic!("a v999 snapshot must not load"),
        }
    }

    #[test]
    fn truncated_snapshot_errors_rather_than_panicking() {
        let path = temp_file("truncated");
        let _c = Cleanup(path.clone());
        let index = Index::new(vec![synthetic_volume(0, "C:", 1_000)]);
        save(&index, &[VolumeStamp::default()], &path).unwrap();

        let full = std::fs::read(&path).unwrap();
        // Every proper prefix of the file must be rejected.
        for frac in [2, 3, 4, 8, 16, 64] {
            std::fs::write(&path, &full[..full.len() / frac]).unwrap();
            assert!(load(&path).is_err(), "truncation to 1/{frac} should not load");
        }
    }

    #[test]
    fn implausible_header_does_not_allocate_wildly() {
        let path = temp_file("bogus");
        let _c = Cleanup(path.clone());
        let mut bad = MAGIC.to_vec();
        bad.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        bad.extend_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, &bad).unwrap();
        assert!(matches!(load(&path), Err(Error::StaleSnapshot(_))));
    }

    #[test]
    fn freshness_check_distinguishes_the_three_outcomes() {
        // Same journal: resume from the saved USN.
        let s = VolumeStamp { volume_serial: 1, journal_id: 77, next_usn: 4242 };
        assert_eq!(check_fresh(s, 77, "C:").unwrap(), 4242);
        // Journal recreated: the snapshot is unusable.
        assert!(matches!(check_fresh(s, 78, "C:"), Err(Error::JournalInvalidated(_))));
        // Never had a journal (walk fallback): nothing to resume from.
        assert!(matches!(
            check_fresh(VolumeStamp::default(), 77, "C:"),
            Err(Error::StaleSnapshot(_))
        ));
    }

    #[test]
    fn default_path_is_under_localappdata() {
        let p = default_path();
        assert!(p.ends_with("Velox\\index.veloxidx") || p.ends_with("Velox/index.veloxidx"), "{p:?}");
    }
}
