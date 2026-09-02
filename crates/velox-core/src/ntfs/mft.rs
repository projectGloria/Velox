//! Reading the Master File Table.
//!
//! The whole point of the exercise: instead of asking the filesystem for one
//! directory at a time, read `$MFT` as a flat byte stream and parse every record
//! out of it. One large sequential read replaces millions of scattered metadata
//! lookups.
//!
//! Finding `$MFT` is a small bootstrap problem. `FSCTL_GET_NTFS_VOLUME_DATA`
//! gives the cluster where record 0 lives; record 0 *is* `$MFT`, and its
//! unnamed `$DATA` attribute holds the run list describing where the rest of the
//! MFT is. So: read one record, decode its run list, then stream the extents.
//!
//! Reading and parsing are pipelined across two threads, and each chunk is
//! parsed in parallel with rayon, so the CPU-side fixup/attribute walk overlaps
//! with the next disk read.
//!
//! **Requires administrator privileges** — see [`super::volume::RawVolume::open`].

use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use super::record::{self, ParsedRecord, ROOT_RECORD};
use super::volume::{NtfsGeometry, RawVolume, VolumeInfo};
use crate::index::{SourceKind, VolumeIndex};
use crate::store::{EntryStore, VolumeId, INVALID_ENTRY};
use crate::Error;

/// Target size of each raw read. Large enough that per-call overhead vanishes,
/// small enough that the pipeline's two in-flight buffers stay modest.
const TARGET_CHUNK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct MftStats {
    pub records_scanned: usize,
    pub entries_indexed: usize,
    pub bytes_read: u64,
    pub fixup_errors: usize,
    pub orphans: usize,
    /// Entry slots retained. Larger than `entries_indexed` because `EntryId` is
    /// the MFT record number, so unused records stay as interior holes.
    pub slots: usize,
    /// Bytes held by the packed name blob.
    pub name_bytes: usize,
    /// Resident bytes for the finished index, after shrinking.
    pub memory_bytes: usize,
    pub read_and_parse: Duration,
    /// Time actually inside `ReadFile`. Compared against the wall clock this
    /// separates a slow device from a pipeline stalled on the CPU.
    pub io: Duration,
    /// Time in the parallel record parse (fixups + attribute walk).
    pub parse: Duration,
    /// Time in the single-threaded fold into the store.
    pub fold: Duration,
    pub sort: Duration,
    pub total: Duration,
}

impl MftStats {
    /// Resident bytes per indexed entry — the figure worth quoting, since holes
    /// are real cost that a per-slot number would hide.
    pub fn bytes_per_entry(&self) -> f64 {
        if self.entries_indexed == 0 {
            0.0
        } else {
            self.memory_bytes as f64 / self.entries_indexed as f64
        }
    }

    /// Fraction of retained slots that hold nothing.
    pub fn hole_ratio(&self) -> f64 {
        if self.slots == 0 {
            0.0
        } else {
            1.0 - (self.entries_indexed as f64 / self.slots as f64)
        }
    }

    pub fn records_per_sec(&self) -> f64 {
        let s = self.total.as_secs_f64();
        if s > 0.0 {
            self.entries_indexed as f64 / s
        } else {
            0.0
        }
    }
}

/// One parsed entry, flattened for cheap transfer out of the parallel parse.
struct RawEntry {
    id: u32,
    name: String,
    parent: u32,
    size: u64,
    mtime: i64,
    seq: u16,
    is_dir: bool,
}

/// Read a volume's MFT and build a queryable index from it.
///
/// **Requires administrator privileges.** Returns [`Error::AccessDenied`] if the
/// process is not elevated; callers should fall back to [`crate::walk`].
pub fn build_volume_index(
    id: VolumeId,
    info: &VolumeInfo,
) -> Result<(VolumeIndex, MftStats), Error> {
    let t0 = Instant::now();
    let vol = RawVolume::open(info)?;
    let geom = vol.geometry()?;
    let runs = read_mft_run_list(&vol, &geom)?;

    let mut stats = MftStats::default();
    let store = read_and_build_store(&vol, &geom, &runs, &info.label, &mut stats)?;
    stats.read_and_parse = t0.elapsed();

    let t1 = Instant::now();
    let index = VolumeIndex::build(id, SourceKind::Mft, store);
    stats.sort = t1.elapsed();
    stats.slots = index.store.slots();
    stats.name_bytes = index.store.name_bytes();
    stats.memory_bytes = index.memory_bytes();
    stats.total = t0.elapsed();
    Ok((index, stats))
}

/// Bootstrap: read MFT record 0 and decode its own `$DATA` run list.
fn read_mft_run_list(
    vol: &RawVolume,
    geom: &NtfsGeometry,
) -> Result<Vec<record::DataRun>, Error> {
    let offset = geom.mft_start_lcn as u64 * geom.bytes_per_cluster as u64;
    // Read a whole cluster: the record is at most one cluster and reads must be
    // sector-aligned in both offset and length.
    let read_len = geom.bytes_per_cluster.max(geom.bytes_per_record) as usize;
    let mut buf = vec![0u8; read_len];
    let got = vol.read_at(offset, &mut buf)?;
    if got < geom.bytes_per_record as usize {
        return Err(Error::Corrupt("short read of MFT record 0".into()));
    }
    let rec = &mut buf[..geom.bytes_per_record as usize];
    if !record::is_file_record(rec) {
        return Err(Error::Corrupt("MFT record 0 is not a FILE record".into()));
    }
    record::apply_fixups(rec, geom.bytes_per_sector as usize)?;
    record::find_data_runs(rec)
}

/// Stream the MFT extents, parse every record, and fold the results into a store.
fn read_and_build_store(
    vol: &RawVolume,
    geom: &NtfsGeometry,
    runs: &[record::DataRun],
    label: &str,
    stats: &mut MftStats,
) -> Result<EntryStore, Error> {
    let bpr = geom.bytes_per_record as usize;
    let bps = geom.bytes_per_sector as usize;
    let max_records = geom.max_records();

    // `EntryId` == MFT record number, so the arrays are sized by record count and
    // the parallel parse can write results at their natural index with no merge.
    let mut store = EntryStore::with_capacity(label, max_records + 1, max_records * 18);

    // Reads must be a multiple of both the cluster size (device alignment) and
    // the record size (so chunks start on record boundaries).
    let align = geom.bytes_per_cluster.max(geom.bytes_per_record) as usize;
    let chunk_bytes = TARGET_CHUNK_BYTES.next_multiple_of(align);
    let total_valid = geom.mft_valid_length;

    let (tx, rx) = sync_channel::<(usize, Vec<u8>)>(2);

    let read_result = std::thread::scope(|scope| {
        // Reader thread: pure sequential I/O, no parsing, so the disk never waits
        // on the CPU.
        let reader = scope.spawn(move || -> Result<(u64, Duration), Error> {
            let mut logical: u64 = 0; // byte offset within the MFT stream
            let mut carry: Vec<u8> = Vec::new();
            let mut bytes_read = 0u64;
            let mut io = Duration::ZERO;

            'runs: for run in runs {
                let run_bytes = run.clusters * geom.bytes_per_cluster as u64;
                let mut done = 0u64;
                while done < run_bytes {
                    if logical >= total_valid {
                        break 'runs;
                    }
                    let want =
                        (run_bytes - done).min(chunk_bytes as u64).min(total_valid - logical);
                    // Round the request up to device alignment; the tail beyond
                    // `total_valid` is discarded below.
                    let req = (want as usize).next_multiple_of(bps).min((run_bytes - done) as usize);
                    let offset =
                        run.lcn as u64 * geom.bytes_per_cluster as u64 + done;

                    let mut buf = vec![0u8; req];
                    let t_io = Instant::now();
                    let got = vol.read_at(offset, &mut buf)?;
                    io += t_io.elapsed();
                    if got == 0 {
                        break 'runs;
                    }
                    buf.truncate(got);
                    bytes_read += got as u64;
                    done += got as u64;

                    // Splice with any partial record left over from the previous
                    // chunk, then hand off only whole records.
                    let base_record = (logical / bpr as u64) as usize;
                    let mut block = if carry.is_empty() {
                        buf
                    } else {
                        let mut b = std::mem::take(&mut carry);
                        b.extend_from_slice(&buf);
                        b
                    };
                    let whole = (block.len() / bpr) * bpr;
                    if whole < block.len() {
                        carry = block[whole..].to_vec();
                        block.truncate(whole);
                    }
                    if block.is_empty() {
                        continue;
                    }
                    logical += block.len() as u64;
                    if tx.send((base_record, block)).is_err() {
                        break 'runs; // consumer went away
                    }
                }
            }
            drop(tx);
            Ok((bytes_read, io))
        });

        // Consumer: parse each chunk in parallel, then fold sequentially. The fold
        // is a memcpy into the store's arrays; the expensive part is the parse.
        for (base_record, mut block) in rx {
            let t_parse = Instant::now();
            let parsed: Vec<RawEntry> = block
                .par_chunks_mut(bpr)
                .enumerate()
                .filter_map(|(i, rec)| {
                    if !record::is_file_record(rec) {
                        return None;
                    }
                    if record::apply_fixups(rec, bps).is_err() {
                        return None;
                    }
                    let p: ParsedRecord = record::parse_record(rec)?;
                    let name = p.name?;
                    Some(RawEntry {
                        id: (base_record + i) as u32,
                        name,
                        parent: p.parent,
                        size: p.size,
                        mtime: p.mtime,
                        seq: p.sequence,
                        is_dir: p.is_dir,
                    })
                })
                .collect();

            stats.parse += t_parse.elapsed();

            stats.records_scanned += block.len() / bpr;
            stats.entries_indexed += parsed.len();
            let t_fold = Instant::now();
            for e in parsed {
                store.set_slot(e.id, &e.name, e.parent, e.size, e.mtime, e.seq, e.is_dir);
            }
            stats.fold += t_fold.elapsed();
        }

        reader.join().unwrap_or(Err(Error::Corrupt("MFT reader thread panicked".into())))
    });

    let (bytes_read, io) = read_result?;
    stats.bytes_read = bytes_read;
    stats.io = io;
    finalize_tree(&mut store, stats);
    // The arrays were sized from the MFT's valid length and the name blob from an
    // estimate; both overshoot. Give the slack back before the index is built,
    // otherwise it is charged for the process lifetime.
    store.shrink_to_fit();
    Ok(store)
}

/// Give the volume a root and re-home anything whose parent did not survive.
///
/// Parent references are MFT record numbers, which are exactly our entry ids, so
/// no second resolution pass is needed — only validation.
fn finalize_tree(store: &mut EntryStore, stats: &mut MftStats) {
    let slots = store.slots();
    if (ROOT_RECORD as usize) < slots && store.is_live(ROOT_RECORD) {
        // NTFS names the root ".", which would otherwise show up in every path.
        let mtime = store.mtime(ROOT_RECORD);
        store.set_slot(ROOT_RECORD, "", ROOT_RECORD, 0, mtime, 0, true);
        store.set_root(ROOT_RECORD);
    } else {
        let root = store.push("", INVALID_ENTRY, 0, 0, true);
        store.set_root(root);
    }
    let root = store.root();

    let ids: Vec<u32> = store.live_ids().collect();
    for id in ids {
        if id == root {
            continue;
        }
        let p = store.parent(id);
        if p as usize >= slots || !store.is_live(p) || p == id {
            store.reparent(id, root);
            stats.orphans += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{MatchMode, Query, Scope, Searcher};
    use crate::store::EntryRef;
    use crate::Index;

    /// End-to-end against the real `C:` MFT. Skips itself when not elevated, so
    /// the suite stays green for unprivileged runs; run `cargo test -- --nocapture`
    /// from an elevated shell to actually exercise it.
    #[test]
    fn reads_the_real_mft_when_elevated() {
        if !super::super::volume::is_elevated() {
            eprintln!("SKIP: not elevated; MFT read requires administrator privileges");
            return;
        }
        let vols = super::super::volume::enumerate_volumes();
        let Some(c) = vols.into_iter().find(|v| v.letter == 'C' && v.is_ntfs()) else {
            eprintln!("SKIP: no NTFS C:");
            return;
        };
        let (index, stats) = build_volume_index(0, &c).expect("MFT build");
        eprintln!(
            "indexed {} entries from {} records in {:?} ({:.0} entries/s, {} orphans)",
            stats.entries_indexed,
            stats.records_scanned,
            stats.total,
            stats.records_per_sec(),
            stats.orphans
        );
        assert!(stats.entries_indexed > 1000, "a real C: has more than 1000 files");

        // The Windows directory must be findable, and its path must reconstruct.
        let idx = Index::new(vec![index]);
        let mut se = Searcher::new();
        let res = se.search(&idx, &Query::parse("windows", MatchMode::Prefix, Scope::Name));
        assert!(res.total > 0, "expected to find something starting with 'windows'");
        let hits: Vec<EntryRef> = res.window(&idx, 0, 50);
        let paths: Vec<String> =
            hits.iter().filter_map(|r| idx.describe(*r)).map(|i| i.full_path()).collect();
        assert!(
            paths.iter().any(|p| p.eq_ignore_ascii_case("C:\\Windows")),
            "expected C:\\Windows among {paths:?}"
        );
    }

    #[test]
    fn unelevated_build_reports_access_denied() {
        if super::super::volume::is_elevated() {
            return;
        }
        let vols = super::super::volume::enumerate_volumes();
        let Some(c) = vols.into_iter().find(|v| v.letter == 'C' && v.is_ntfs()) else {
            return;
        };
        assert!(matches!(build_volume_index(0, &c), Err(Error::AccessDenied(_))));
    }
}
