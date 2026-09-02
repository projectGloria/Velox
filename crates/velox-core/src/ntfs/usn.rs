//! USN Change Journal reading and application.
//!
//! NTFS records every create, delete, rename, and metadata change in a per-volume
//! journal. Reading that stream and applying it to the in-memory index is what
//! keeps the index live without ever rescanning: the cost of staying current is
//! proportional to what changed, not to how many files exist.
//!
//! **Requires administrator privileges**, same as the MFT read — the journal is
//! read through the raw volume handle.
//!
//! The journal is a ring buffer. If Velox is away long enough for the ring to
//! wrap past our checkpoint, or if the journal is deleted or recreated, the
//! checkpoint is meaningless and the volume must be rebuilt from the MFT. That
//! case surfaces as [`Error::JournalInvalidated`].

use std::mem::size_of;
use std::path::PathBuf;

use windows::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0,
    USN_JOURNAL_DATA_V0, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::record::{frn_record, frn_sequence};
use super::volume::{RawVolume, VolumeInfo};
use crate::index::VolumeIndex;
use crate::Error;

/// Journal was deleted or is being deleted.
const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
/// Our checkpoint fell off the back of the ring buffer.
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
const ERROR_INVALID_PARAMETER: i32 = 87;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Read buffer. Each record is ~60 bytes plus the name, so this holds a few
/// thousand events per call.
const READ_BUFFER_BYTES: usize = 512 * 1024;

/// Where we are in a volume's journal. Persisted alongside the index snapshot so
/// a restart can catch up instead of rebuilding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JournalCheckpoint {
    /// Identifies the journal instance. A different id means the journal was
    /// recreated and every USN we hold is meaningless.
    pub journal_id: u64,
    /// The USN to resume reading from.
    pub next_usn: i64,
}

/// A single change, decoded from a `USN_RECORD_V2`.
#[derive(Debug, Clone)]
pub struct UsnEvent {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    /// Windows FILETIME of the change.
    pub timestamp: i64,
    pub reason: u32,
    pub attributes: u32,
    pub name: String,
}

impl UsnEvent {
    #[inline]
    pub fn record(&self) -> u32 {
        frn_record(self.frn)
    }
    #[inline]
    pub fn sequence(&self) -> u16 {
        frn_sequence(self.frn)
    }
    #[inline]
    pub fn parent_record(&self) -> u32 {
        frn_record(self.parent_frn)
    }
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
    #[inline]
    pub fn is_delete(&self) -> bool {
        self.reason & USN_REASON_FILE_DELETE != 0
    }
    #[inline]
    pub fn is_create(&self) -> bool {
        self.reason & USN_REASON_FILE_CREATE != 0
    }
    /// The "old name" half of a rename. The matching `RENAME_NEW_NAME` record
    /// carries the name to index, so this half is informational only.
    #[inline]
    pub fn is_rename_old(&self) -> bool {
        self.reason & USN_REASON_RENAME_OLD_NAME != 0
    }
    #[inline]
    pub fn is_rename_new(&self) -> bool {
        self.reason & USN_REASON_RENAME_NEW_NAME != 0
    }
}

fn map_journal_error(e: &windows::core::Error, label: &str) -> Error {
    let code = e.code().0 & 0xFFFF;
    match code {
        ERROR_JOURNAL_NOT_ACTIVE
        | ERROR_JOURNAL_DELETE_IN_PROGRESS
        | ERROR_JOURNAL_ENTRY_DELETED
        | ERROR_INVALID_PARAMETER => Error::JournalInvalidated(label.to_string()),
        _ => Error::Io(std::io::Error::from_raw_os_error(code)),
    }
}

/// `FSCTL_QUERY_USN_JOURNAL`: the journal's identity and current head.
pub fn query_journal(vol: &RawVolume) -> Result<JournalCheckpoint, Error> {
    let mut data = USN_JOURNAL_DATA_V0::default();
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            vol.handle(),
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut data as *mut _ as *mut _),
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| map_journal_error(&e, &vol.info.label))?;
    }
    Ok(JournalCheckpoint { journal_id: data.UsnJournalID, next_usn: data.NextUsn })
}

/// A live journal reader for one volume.
///
/// **Requires administrator privileges.**
pub struct UsnMonitor {
    vol: RawVolume,
    checkpoint: JournalCheckpoint,
    buf: Vec<u8>,
}

impl UsnMonitor {
    /// Start monitoring from the journal's current head — i.e. only changes from
    /// now on. Use [`UsnMonitor::resume`] to catch up from a saved checkpoint.
    pub fn new(info: &VolumeInfo) -> Result<Self, Error> {
        let vol = RawVolume::open(info)?;
        let checkpoint = query_journal(&vol)?;
        Ok(UsnMonitor { vol, checkpoint, buf: vec![0u8; READ_BUFFER_BYTES] })
    }

    /// Resume from a checkpoint saved with a previous index snapshot.
    ///
    /// Returns [`Error::JournalInvalidated`] when the journal has been recreated
    /// since, which means the snapshot cannot be trusted and the volume needs a
    /// full MFT rebuild.
    pub fn resume(info: &VolumeInfo, checkpoint: JournalCheckpoint) -> Result<Self, Error> {
        let vol = RawVolume::open(info)?;
        let current = query_journal(&vol)?;
        if current.journal_id != checkpoint.journal_id {
            return Err(Error::JournalInvalidated(info.label.clone()));
        }
        Ok(UsnMonitor { vol, checkpoint, buf: vec![0u8; READ_BUFFER_BYTES] })
    }

    pub fn checkpoint(&self) -> JournalCheckpoint {
        self.checkpoint
    }
    pub fn label(&self) -> &str {
        &self.vol.info.label
    }

    /// Read whatever the journal has for us right now, without blocking.
    ///
    /// Returns an empty vector when nothing has changed. Callers poll this on a
    /// background thread; the journal itself does the buffering, so a slow poll
    /// loses nothing as long as the ring does not wrap.
    pub fn poll(&mut self) -> Result<Vec<UsnEvent>, Error> {
        let input = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: self.checkpoint.next_usn,
            ReasonMask: u32::MAX,
            // Report changes as they happen rather than only on handle close, so
            // the index reflects a file the moment it appears.
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: self.checkpoint.journal_id,
        };
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.vol.handle(),
                FSCTL_READ_USN_JOURNAL,
                Some(&input as *const _ as *const _),
                size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(self.buf.as_mut_ptr() as *mut _),
                self.buf.len() as u32,
                Some(&mut returned),
                None,
            )
            .map_err(|e| map_journal_error(&e, &self.vol.info.label))?;
        }
        // The first 8 bytes are the USN to resume from next time.
        if (returned as usize) < 8 {
            return Ok(Vec::new());
        }
        let next = i64::from_le_bytes(self.buf[0..8].try_into().unwrap());
        let events = parse_usn_records(&self.buf[8..returned as usize]);
        self.checkpoint.next_usn = next;
        Ok(events)
    }
}

/// Decode a `FSCTL_READ_USN_JOURNAL` output buffer (minus its leading USN).
///
/// Parsed field by field rather than by casting the buffer to `USN_RECORD_V2`:
/// the records are variable-length and unaligned, and the name is a trailing
/// UTF-16 array whose length comes from the header.
pub fn parse_usn_records(mut buf: &[u8]) -> Vec<UsnEvent> {
    let mut out = Vec::new();
    while buf.len() >= 60 {
        let len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        // A zero or oversized length would loop forever or read past the end.
        if len < 60 || len > buf.len() {
            break;
        }
        let rec = &buf[..len];
        let major = u16::from_le_bytes(rec[4..6].try_into().unwrap());
        // We request V0 input, which yields V2 records. Anything else is skipped
        // rather than misparsed.
        if major == 2 {
            let name_len = u16::from_le_bytes(rec[56..58].try_into().unwrap()) as usize;
            let name_off = u16::from_le_bytes(rec[58..60].try_into().unwrap()) as usize;
            if name_off + name_len <= len && name_len % 2 == 0 {
                let units: Vec<u16> = rec[name_off..name_off + name_len]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                out.push(UsnEvent {
                    frn: u64::from_le_bytes(rec[8..16].try_into().unwrap()),
                    parent_frn: u64::from_le_bytes(rec[16..24].try_into().unwrap()),
                    usn: i64::from_le_bytes(rec[24..32].try_into().unwrap()),
                    timestamp: i64::from_le_bytes(rec[32..40].try_into().unwrap()),
                    reason: u32::from_le_bytes(rec[40..44].try_into().unwrap()),
                    attributes: u32::from_le_bytes(rec[52..56].try_into().unwrap()),
                    name: String::from_utf16_lossy(&units),
                });
            }
        }
        buf = &buf[len..];
    }
    out
}

/// Fills in a size the journal does not carry, given an entry's name. Passing
/// `None` leaves sizes stale, which is what the GUI does while typing.
pub type SizeResolver<'a> = Option<&'a mut dyn FnMut(&str) -> Option<u64>>;

/// How an event batch changed the index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    /// Events referring to records outside the index's slot range.
    pub ignored: usize,
}

/// Apply a batch of journal events to a volume index.
///
/// Only the write-side primitives on [`VolumeIndex`] are used, so each event is
/// O(1): an array write plus an append to the delta overlay. The caller decides
/// when to merge the overlay back into the sorted array.
///
/// `resolve_size` is called for entries whose data changed, to fill in a size
/// the journal does not carry. Pass `None` to leave sizes stale, which is what
/// the GUI does while typing and reconciles later.
pub fn apply_events(
    index: &mut VolumeIndex,
    events: &[UsnEvent],
    mut resolve_size: SizeResolver<'_>,
) -> ApplyStats {
    let mut stats = ApplyStats::default();
    for ev in events {
        let id = ev.record();
        // The MFT-derived index sized its arrays from the MFT's valid length; a
        // record beyond that is a slot the MFT grew into after our scan.
        if ev.name.is_empty() {
            stats.ignored += 1;
            continue;
        }
        if ev.is_delete() {
            if index.store.is_live(id) {
                index.remove(id);
                stats.deleted += 1;
            } else {
                stats.ignored += 1;
            }
            continue;
        }
        // RENAME_OLD_NAME is immediately followed by RENAME_NEW_NAME for the same
        // record; acting on the old half would briefly index a stale name.
        if ev.is_rename_old() && !ev.is_rename_new() {
            continue;
        }

        let existed = index.store.is_live(id);
        let parent = ev.parent_record();
        let mut size = if existed { index.store.size(id) } else { 0 };
        if let Some(f) = resolve_size.as_deref_mut() {
            // Path must be built before the upsert so it reflects the old parent
            // only when the entry is new; for updates the name is what changed.
            if let Some(sz) = f(&ev.name) {
                size = sz;
            }
        }
        index.upsert(id, &ev.name, parent, size, ev.timestamp, ev.sequence(), ev.is_dir());
        if existed {
            stats.updated += 1;
        } else {
            stats.created += 1;
        }
    }
    stats
}

/// Resolve a full path for an entry so its size can be re-read from disk.
pub fn entry_path(index: &VolumeIndex, id: u32) -> Option<PathBuf> {
    if !index.store.is_live(id) {
        return None;
    }
    Some(PathBuf::from(index.store.path(id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SourceKind;
    use windows::Win32::System::Ioctl::USN_REASON_CLOSE;
    use crate::store::{EntryStore, INVALID_ENTRY};

    /// Build a `USN_RECORD_V2` byte-for-byte so the decoder can be tested without
    /// a volume handle.
    fn make_record(
        frn: u64,
        parent: u64,
        usn: i64,
        timestamp: i64,
        reason: u32,
        attributes: u32,
        name: &str,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_off = 60usize;
        let name_len = units.len() * 2;
        let len = (name_off + name_len + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes()); // major version
        r[6..8].copy_from_slice(&0u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[32..40].copy_from_slice(&timestamp.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attributes.to_le_bytes());
        r[56..58].copy_from_slice(&(name_len as u16).to_le_bytes());
        r[58..60].copy_from_slice(&(name_off as u16).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[name_off + i * 2..name_off + 2 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    fn test_index() -> VolumeIndex {
        let mut s = EntryStore::new("C:");
        s.resize_slots(200);
        let root = 5u32;
        s.set_slot(root, "", root, 0, 0, 1, true);
        s.set_root(root);
        s.set_slot(10, "existing.txt", root, 100, 0, 1, false);
        s.set_slot(11, "keep.txt", root, 100, 0, 1, false);
        VolumeIndex::build(0, SourceKind::Mft, s)
    }

    #[test]
    fn decodes_a_record_stream() {
        let mut buf = Vec::new();
        buf.extend(make_record(1 << 48 | 20, 1 << 48 | 5, 100, 7, USN_REASON_FILE_CREATE, 0, "new.txt"));
        buf.extend(make_record(
            2 << 48 | 21,
            1 << 48 | 5,
            200,
            8,
            USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
            FILE_ATTRIBUTE_DIRECTORY,
            "gone",
        ));
        let evs = parse_usn_records(&buf);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].name, "new.txt");
        assert_eq!(evs[0].record(), 20);
        assert_eq!(evs[0].sequence(), 1);
        assert_eq!(evs[0].parent_record(), 5);
        assert!(evs[0].is_create());
        assert!(!evs[0].is_dir());
        assert_eq!(evs[1].name, "gone");
        assert!(evs[1].is_delete());
        assert!(evs[1].is_dir());
    }

    #[test]
    fn truncated_and_malformed_streams_do_not_panic() {
        let full = make_record(1, 2, 3, 4, USN_REASON_FILE_CREATE, 0, "abc.txt");
        for cut in 0..full.len() {
            let _ = parse_usn_records(&full[..cut]);
        }
        // A zero record length must terminate rather than spin.
        let mut zero = full.clone();
        zero[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_usn_records(&zero).is_empty());
        // A length past the buffer end must be rejected.
        let mut huge = full.clone();
        huge[0..4].copy_from_slice(&0xFFFFu32.to_le_bytes());
        assert!(parse_usn_records(&huge).is_empty());
    }

    #[test]
    fn newer_record_versions_are_skipped_not_misparsed() {
        let mut r = make_record(1, 2, 3, 4, USN_REASON_FILE_CREATE, 0, "x.txt");
        r[4..6].copy_from_slice(&3u16.to_le_bytes()); // V3: 128-bit file ids
        assert!(parse_usn_records(&r).is_empty());
    }

    #[test]
    fn create_event_becomes_a_searchable_entry() {
        let mut idx = test_index();
        let evs = parse_usn_records(&make_record(
            1 << 48 | 30,
            1 << 48 | 5,
            1,
            123,
            USN_REASON_FILE_CREATE,
            0,
            "brandnew.txt",
        ));
        let stats = apply_events(&mut idx, &evs, None);
        assert_eq!(stats.created, 1);
        assert!(idx.store.is_live(30));
        assert_eq!(idx.store.name(30), "brandnew.txt");

        // Visible through the overlay immediately, and through the sorted array
        // after a merge.
        let mut hits = Vec::new();
        idx.delta_prefix_matches(&crate::fold::fold_to_string("brandnew"), &mut hits);
        assert_eq!(hits.len(), 1);
        idx.merge_now();
        let r = idx.prefix_range(&crate::fold::fold_to_string("brandnew"));
        assert_eq!(idx.count_range(r), 1);
    }

    #[test]
    fn delete_event_removes_the_entry() {
        let mut idx = test_index();
        let evs = parse_usn_records(&make_record(
            1 << 48 | 10,
            1 << 48 | 5,
            1,
            0,
            USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
            0,
            "existing.txt",
        ));
        let stats = apply_events(&mut idx, &evs, None);
        assert_eq!(stats.deleted, 1);
        assert!(!idx.store.is_live(10));
        idx.merge_now();
        let r = idx.prefix_range(&crate::fold::fold_to_string("existing"));
        assert_eq!(idx.count_range(r), 0);
    }

    #[test]
    fn rename_updates_the_name_in_place() {
        let mut idx = test_index();
        let mut buf = Vec::new();
        buf.extend(make_record(
            1 << 48 | 10,
            1 << 48 | 5,
            1,
            0,
            USN_REASON_RENAME_OLD_NAME,
            0,
            "existing.txt",
        ));
        buf.extend(make_record(
            1 << 48 | 10,
            1 << 48 | 5,
            2,
            0,
            USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE,
            0,
            "renamed.txt",
        ));
        let stats = apply_events(&mut idx, &parse_usn_records(&buf), None);
        assert_eq!(stats.updated, 1);
        assert_eq!(stats.created, 0);
        assert_eq!(idx.store.name(10), "renamed.txt");

        idx.merge_now();
        // The old name must be gone, not merely shadowed.
        let old = idx.prefix_range(&crate::fold::fold_to_string("existing"));
        assert_eq!(idx.count_range(old), 0);
        let new = idx.prefix_range(&crate::fold::fold_to_string("renamed"));
        assert_eq!(idx.count_range(new), 1);
        // And the entry must not have been duplicated.
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn move_between_directories_updates_the_parent() {
        let mut idx = test_index();
        idx.upsert(50, "Target", 5, 0, 0, 1, true);
        idx.merge_now();
        let evs = parse_usn_records(&make_record(
            1 << 48 | 10,
            1 << 48 | 50,
            3,
            0,
            USN_REASON_RENAME_NEW_NAME,
            0,
            "existing.txt",
        ));
        apply_events(&mut idx, &evs, None);
        assert_eq!(idx.store.parent(10), 50);
        assert_eq!(idx.store.path(10), "C:\\Target\\existing.txt");
    }

    #[test]
    fn deleting_something_we_never_indexed_is_ignored() {
        let mut idx = test_index();
        let evs = parse_usn_records(&make_record(
            1 << 48 | 99,
            1 << 48 | 5,
            1,
            0,
            USN_REASON_FILE_DELETE,
            0,
            "never-seen.txt",
        ));
        let stats = apply_events(&mut idx, &evs, None);
        assert_eq!(stats, ApplyStats { ignored: 1, ..Default::default() });
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn size_resolver_fills_in_what_the_journal_omits() {
        let mut idx = test_index();
        let evs = parse_usn_records(&make_record(
            1 << 48 | 31,
            1 << 48 | 5,
            1,
            0,
            USN_REASON_FILE_CREATE,
            0,
            "sized.bin",
        ));
        let mut resolver = |name: &str| if name == "sized.bin" { Some(4096) } else { None };
        apply_events(&mut idx, &evs, Some(&mut resolver));
        assert_eq!(idx.store.size(31), 4096);
    }

    #[test]
    fn many_events_keep_the_index_consistent() {
        let mut idx = test_index();
        let mut buf = Vec::new();
        for k in 0..300u32 {
            buf.extend(make_record(
                1 << 48 | (100 + k) as u64,
                1 << 48 | 5,
                k as i64,
                0,
                USN_REASON_FILE_CREATE,
                0,
                &format!("gen_{k:04}.tmp"),
            ));
        }
        let evs = parse_usn_records(&buf);
        assert_eq!(evs.len(), 300);
        let stats = apply_events(&mut idx, &evs, None);
        assert_eq!(stats.created, 300);
        assert_eq!(idx.len(), 303);

        // Now delete half of them.
        let mut buf = Vec::new();
        for k in (0..300u32).step_by(2) {
            buf.extend(make_record(
                1 << 48 | (100 + k) as u64,
                1 << 48 | 5,
                1000 + k as i64,
                0,
                USN_REASON_FILE_DELETE,
                0,
                &format!("gen_{k:04}.tmp"),
            ));
        }
        let stats = apply_events(&mut idx, &parse_usn_records(&buf), None);
        assert_eq!(stats.deleted, 150);
        idx.merge_now();
        assert_eq!(idx.len(), 153);

        let r = idx.prefix_range(&crate::fold::fold_to_string("gen_"));
        assert_eq!(idx.count_range(r), 150);
        for id in idx.order() {
            assert!(idx.store.is_live(*id));
        }
    }

    #[test]
    fn journal_query_reports_denial_rather_than_panicking() {
        let vols = super::super::volume::enumerate_volumes();
        let Some(c) = vols.into_iter().find(|v| v.letter == 'C' && v.is_ntfs()) else {
            return;
        };
        match UsnMonitor::new(&c) {
            Ok(m) => {
                assert!(m.checkpoint().journal_id != 0, "an active journal has a non-zero id");
            }
            Err(Error::AccessDenied(_)) => {}
            Err(Error::JournalInvalidated(_)) => {}
            Err(e) => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn entry_path_resolves_or_declines() {
        let idx = test_index();
        assert_eq!(entry_path(&idx, 10).unwrap(), PathBuf::from("C:\\existing.txt"));
        assert!(entry_path(&idx, 99).is_none());
        let _ = INVALID_ENTRY;
    }
}
