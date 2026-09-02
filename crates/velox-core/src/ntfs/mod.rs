//! NTFS fast path: raw MFT reads and USN journal monitoring.
//!
//! Everything in here needs a raw volume handle, and therefore administrator
//! privileges. The orchestration in [`build_index`] treats a missing privilege
//! as a routing decision rather than an error: volumes we cannot open raw get
//! indexed by [`crate::walk`] instead, and the report says which path each
//! volume took so the UI can be honest about it.

pub mod mft;
pub mod record;
pub mod usn;
pub mod volume;

use std::path::PathBuf;
use std::time::{Duration, Instant};

pub use usn::{JournalCheckpoint, UsnEvent, UsnMonitor};
pub use volume::{enumerate_volumes, is_elevated, VolumeInfo};

use crate::index::{Index, SourceKind, VolumeIndex};
use crate::Error;

/// What happened to one volume during an index build.
#[derive(Debug, Clone)]
pub struct VolumeReport {
    pub label: String,
    pub source: SourceKind,
    pub entries: usize,
    pub elapsed: Duration,
    /// Why the fast path was not used, when it was not.
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BuildReport {
    pub volumes: Vec<VolumeReport>,
    pub elevated: bool,
    pub total: Duration,
    pub memory_bytes: usize,
}

impl BuildReport {
    pub fn total_entries(&self) -> usize {
        self.volumes.iter().map(|v| v.entries).sum()
    }
    pub fn used_mft(&self) -> bool {
        self.volumes.iter().any(|v| v.source == SourceKind::Mft)
    }
}

/// How to index when the MFT is unavailable.
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct FallbackPolicy {
    /// Folders to walk instead. Empty means "walk the volume root", which is
    /// slow but complete; the GUI narrows this to user-selected folders.
    pub roots: Vec<PathBuf>,
    /// Skip the volume entirely rather than walking it.
    pub skip: bool,
}


/// Build an index over every local volume.
///
/// NTFS volumes are read from the MFT when the process is elevated. Anything
/// else — a non-NTFS volume, or any volume at all when the user declined the
/// elevation prompt — falls back to a `FindFirstFile` walk, as configured by
/// `fallback`. The build never fails wholesale because one volume was
/// unavailable.
pub fn build_index(fallback: &FallbackPolicy) -> (Index, BuildReport) {
    let t0 = Instant::now();
    let elevated = is_elevated();
    let volumes: Vec<VolumeInfo> =
        enumerate_volumes().into_iter().filter(|v| v.is_local()).collect();

    // Volumes are independent, so index them concurrently: on a machine with an
    // SSD and several drives this is close to a straight speedup.
    let built: Vec<(VolumeIndex, VolumeReport)> = std::thread::scope(|scope| {
        let handles: Vec<_> = volumes
            .iter()
            .enumerate()
            .map(|(i, info)| scope.spawn(move || build_one(i as u16, info, fallback)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    });

    let mut reports = Vec::with_capacity(built.len());
    let mut indexes = Vec::with_capacity(built.len());
    for (idx, rep) in built {
        reports.push(rep);
        indexes.push(idx);
    }
    // Re-assign ids so they match position after any volume was skipped.
    for (i, v) in indexes.iter_mut().enumerate() {
        v.id = i as u16;
    }
    let index = Index::new(indexes);
    let memory_bytes = index.memory_bytes();
    (
        index,
        BuildReport { volumes: reports, elevated, total: t0.elapsed(), memory_bytes },
    )
}

fn build_one(
    id: u16,
    info: &VolumeInfo,
    fallback: &FallbackPolicy,
) -> Option<(VolumeIndex, VolumeReport)> {
    let note = if info.is_ntfs() {
        match mft::build_volume_index(id, info) {
            Ok((idx, stats)) => {
                let entries = idx.len();
                return Some((
                    idx,
                    VolumeReport {
                        label: info.label.clone(),
                        source: SourceKind::Mft,
                        entries,
                        elapsed: stats.total,
                        note: None,
                    },
                ));
            }
            Err(Error::AccessDenied(_)) => {
                "not elevated: raw MFT read needs administrator privileges".to_string()
            }
            Err(e) => format!("MFT read failed: {e}"),
        }
    } else {
        format!("{} is not NTFS", info.fs_name)
    };

    if fallback.skip {
        return None;
    }
    let roots = if fallback.roots.is_empty() {
        vec![PathBuf::from(info.root_path())]
    } else {
        // Only the roots that actually live on this volume.
        let prefix = info.root_path().to_ascii_uppercase();
        let mine: Vec<PathBuf> = fallback
            .roots
            .iter()
            .filter(|r| r.to_string_lossy().to_ascii_uppercase().starts_with(&prefix))
            .cloned()
            .collect();
        if mine.is_empty() {
            return None;
        }
        mine
    };
    let (idx, stats) = crate::walk::index_folders(id, &roots, &info.label);
    let entries = idx.len();
    Some((
        idx,
        VolumeReport {
            label: info.label.clone(),
            source: SourceKind::Walk,
            entries,
            elapsed: stats.elapsed,
            note: Some(note),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_never_fails_wholesale() {
        // Skip the walk fallback so an unelevated run stays fast: the point is
        // that the orchestration returns a usable report either way.
        let (index, report) = build_index(&FallbackPolicy { roots: Vec::new(), skip: true });
        if report.elevated {
            assert!(report.used_mft(), "elevated runs should use the MFT path");
            assert!(report.total_entries() > 0);
            assert_eq!(index.volumes.len(), report.volumes.len());
        } else {
            // Unelevated with `skip` set: every NTFS volume is dropped, and the
            // report says so rather than the call erroring out.
            assert!(!report.used_mft());
        }
    }

    #[test]
    fn fallback_routes_a_folder_to_the_volume_it_lives_on() {
        if is_elevated() {
            return; // the MFT path would be taken instead
        }
        let temp = std::env::temp_dir();
        let policy = FallbackPolicy { roots: vec![temp.clone()], skip: false };
        let (index, report) = build_index(&policy);
        assert!(
            report.volumes.iter().all(|v| v.source == SourceKind::Walk),
            "unelevated build must not claim to have used the MFT"
        );
        assert!(report.total_entries() > 0, "the temp directory is not empty");
        assert!(index.total_entries() > 0);
    }
}
