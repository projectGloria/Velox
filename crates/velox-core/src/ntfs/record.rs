//! MFT FILE record parsing.
//!
//! Deliberately free of any Win32 dependency: everything here operates on a byte
//! slice, so the parser is unit-testable against synthetic records without a
//! disk, without NTFS, and without elevation.
//!
//! Layout reference (NTFS 3.1). All integers little-endian.
//!
//! ```text
//! FILE record header
//!   0x00 u32  magic "FILE"
//!   0x04 u16  update sequence array offset
//!   0x06 u16  update sequence array count (1 + one entry per sector)
//!   0x10 u16  sequence number
//!   0x14 u16  offset to first attribute
//!   0x16 u16  flags (0x01 in use, 0x02 directory)
//!   0x18 u32  used size
//!   0x20 u64  base file record reference (0 for a base record)
//!   0x2C u32  this record's number (NTFS 3.1+)
//!
//! attribute header
//!   0x00 u32  type
//!   0x04 u32  length (including header)
//!   0x08 u8   non-resident flag
//!   0x09 u8   name length
//!   resident:      0x10 u32 value length, 0x14 u16 value offset
//!   non-resident:  0x30 u64 real data size
//! ```

use crate::Error;

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_END: u32 = 0xFFFF_FFFF;

pub const FLAG_IN_USE: u16 = 0x0001;
pub const FLAG_DIRECTORY: u16 = 0x0002;

/// MFT record number of the volume root directory.
pub const ROOT_RECORD: u32 = 5;

/// Name namespaces from `$FILE_NAME`.
pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_DOS: u8 = 3;

const MAGIC_FILE: &[u8; 4] = b"FILE";
/// Records the volume has zeroed out.
const MAGIC_BAAD: &[u8; 4] = b"BAAD";

#[inline]
fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
#[inline]
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8).map(|s| {
        u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}

/// The low 48 bits of a file reference number are the MFT record number; the
/// high 16 are the sequence number, used to detect reuse of a record slot.
#[inline]
pub fn frn_record(frn: u64) -> u32 {
    (frn & 0x0000_FFFF_FFFF_FFFF) as u32
}
#[inline]
pub fn frn_sequence(frn: u64) -> u16 {
    (frn >> 48) as u16
}

/// A parsed MFT record, reduced to what the index actually stores.
#[derive(Debug, Clone, Default)]
pub struct ParsedRecord {
    /// The record's own number, as stated in its header.
    pub record_number: Option<u32>,
    pub sequence: u16,
    pub in_use: bool,
    pub is_dir: bool,
    /// Non-zero when this is an extension record belonging to another base
    /// record; such records carry no name of their own and are skipped.
    pub base_record: u64,
    pub name: Option<String>,
    /// Parent directory's MFT record number.
    pub parent: u32,
    pub size: u64,
    /// Last-modified time as a Windows FILETIME.
    pub mtime: i64,
    /// Record has an `$ATTRIBUTE_LIST`, so its `$FILE_NAME` may live elsewhere.
    pub has_attribute_list: bool,
}

/// Reverse the update-sequence "fixups" NTFS applies to every sector of a record.
///
/// NTFS overwrites the last two bytes of each sector with a sequence number so a
/// torn write is detectable, stashing the real bytes in the update sequence
/// array. Without undoing this, two bytes of every 512 are garbage — which
/// silently corrupts names that happen to straddle a sector boundary.
pub fn apply_fixups(buf: &mut [u8], bytes_per_sector: usize) -> Result<(), Error> {
    let usa_off = u16_at(buf, 0x04).ok_or_else(|| Error::Corrupt("record too short".into()))? as usize;
    let usa_count =
        u16_at(buf, 0x06).ok_or_else(|| Error::Corrupt("record too short".into()))? as usize;
    if usa_count == 0 {
        return Ok(());
    }
    let sectors = usa_count - 1;
    if usa_off + usa_count * 2 > buf.len() {
        return Err(Error::Corrupt("update sequence array out of bounds".into()));
    }
    if sectors * bytes_per_sector > buf.len() {
        return Err(Error::Corrupt("record shorter than its sector count".into()));
    }
    let usn = u16_at(buf, usa_off).unwrap();
    for i in 0..sectors {
        let tail = (i + 1) * bytes_per_sector - 2;
        let got = u16_at(buf, tail).unwrap();
        if got != usn {
            return Err(Error::Corrupt(format!("fixup mismatch in sector {i}")));
        }
        let repl = u16_at(buf, usa_off + 2 + i * 2).unwrap().to_le_bytes();
        buf[tail] = repl[0];
        buf[tail + 1] = repl[1];
    }
    Ok(())
}

/// Is this slot a FILE record at all? Unused MFT slots are zeroed.
pub fn is_file_record(buf: &[u8]) -> bool {
    buf.len() >= 4 && &buf[0..4] == MAGIC_FILE
}

pub fn is_bad_record(buf: &[u8]) -> bool {
    buf.len() >= 4 && &buf[0..4] == MAGIC_BAAD
}

/// Parse one fixed-up FILE record.
///
/// Returns `None` for slots that hold nothing indexable: unused records,
/// extension records, and records with no usable `$FILE_NAME`.
pub fn parse_record(buf: &[u8]) -> Option<ParsedRecord> {
    if !is_file_record(buf) {
        return None;
    }
    let flags = u16_at(buf, 0x16)?;
    let mut rec = ParsedRecord {
        record_number: u32_at(buf, 0x2C),
        sequence: u16_at(buf, 0x10)?,
        in_use: flags & FLAG_IN_USE != 0,
        is_dir: flags & FLAG_DIRECTORY != 0,
        base_record: u64_at(buf, 0x20)?,
        ..Default::default()
    };
    if !rec.in_use || rec.base_record != 0 {
        return None;
    }

    // Trust the record's own used-size over the buffer length, but never past it.
    let used = u32_at(buf, 0x18).unwrap_or(buf.len() as u32) as usize;
    let limit = used.min(buf.len());

    let mut off = u16_at(buf, 0x14)? as usize;
    // Track the best name seen: Win32 beats POSIX beats DOS (8.3).
    let mut best_ns = u8::MAX;
    let mut saw_data = false;

    while off + 8 <= limit {
        let attr_type = u32_at(buf, off)?;
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = u32_at(buf, off + 4)? as usize;
        // A zero or wildly oversized length would loop forever or read past the end.
        if attr_len < 16 || off + attr_len > limit {
            break;
        }
        let non_resident = *buf.get(off + 8)? != 0;
        let attr_name_len = *buf.get(off + 9)?;

        match attr_type {
            ATTR_STANDARD_INFORMATION if !non_resident => {
                if let Some(v) = resident_value(buf, off, attr_len) {
                    // 0x00 created, 0x08 modified, 0x10 MFT changed, 0x18 accessed.
                    if let Some(t) = u64_at(v, 0x08) {
                        rec.mtime = t as i64;
                    }
                }
            }
            ATTR_ATTRIBUTE_LIST => rec.has_attribute_list = true,
            ATTR_FILE_NAME if !non_resident => {
                if let Some(v) = resident_value(buf, off, attr_len) {
                    if let Some((name, ns, parent, alloc)) = parse_file_name(v) {
                        if ns_rank(ns) < ns_rank(best_ns) {
                            best_ns = ns;
                            rec.name = Some(name);
                            rec.parent = parent;
                            // $FILE_NAME's size is often stale; only a fallback.
                            if !saw_data && !rec.is_dir {
                                rec.size = alloc;
                            }
                        }
                    }
                }
            }
            ATTR_DATA if attr_name_len == 0 => {
                // The unnamed $DATA stream is "the file's size".
                rec.size = if non_resident {
                    u64_at(buf, off + 0x30).unwrap_or(0)
                } else {
                    u32_at(buf, off + 0x10).unwrap_or(0) as u64
                };
                saw_data = true;
            }
            _ => {}
        }
        off += attr_len;
    }

    rec.name.as_ref()?;
    if rec.is_dir {
        rec.size = 0;
    }
    Some(rec)
}

fn ns_rank(ns: u8) -> u8 {
    match ns {
        NS_WIN32 | NS_WIN32_DOS => 0,
        NS_POSIX => 1,
        NS_DOS => 2,
        _ => 3,
    }
}

/// Slice out a resident attribute's value.
fn resident_value(buf: &[u8], attr_off: usize, attr_len: usize) -> Option<&[u8]> {
    let val_len = u32_at(buf, attr_off + 0x10)? as usize;
    let val_off = u16_at(buf, attr_off + 0x14)? as usize;
    if val_off < 0x18 || val_off + val_len > attr_len {
        return None;
    }
    buf.get(attr_off + val_off..attr_off + val_off + val_len)
}

/// `$FILE_NAME` value -> `(name, namespace, parent record number, allocated size)`.
fn parse_file_name(v: &[u8]) -> Option<(String, u8, u32, u64)> {
    let parent_ref = u64_at(v, 0x00)?;
    let real_size = u64_at(v, 0x30).unwrap_or(0);
    let name_len = *v.get(0x40)? as usize;
    let ns = *v.get(0x41)?;
    if name_len == 0 {
        return None;
    }
    let bytes = v.get(0x42..0x42 + name_len * 2)?;
    let units: Vec<u16> =
        bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let name = String::from_utf16_lossy(&units);
    Some((name, ns, frn_record(parent_ref), real_size))
}

/// A `$DATA` run list entry: `lcn` is the starting cluster on the volume,
/// `clusters` how many follow. Sparse runs are dropped by the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    pub lcn: i64,
    pub clusters: u64,
}

/// Decode a non-resident attribute's run list.
///
/// Each run starts with a header byte: low nibble is the byte-width of the
/// cluster count, high nibble the byte-width of the LCN delta (which is signed
/// and relative to the previous run, so the MFT can live anywhere on the volume).
/// A zero header ends the list; a zero offset width means a sparse run.
pub fn parse_data_runs(mut runs: &[u8]) -> Result<Vec<DataRun>, Error> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    while let Some(&header) = runs.first() {
        if header == 0 {
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return Err(Error::Corrupt(format!("bad run header {header:#04x}")));
        }
        if runs.len() < 1 + len_size + off_size {
            return Err(Error::Corrupt("run list truncated".into()));
        }
        let clusters = le_unsigned(&runs[1..1 + len_size]);
        if off_size == 0 {
            // Sparse run: no clusters on disk, so nothing to read.
            runs = &runs[1 + len_size..];
            continue;
        }
        let delta = le_signed(&runs[1 + len_size..1 + len_size + off_size]);
        lcn += delta;
        if lcn < 0 {
            return Err(Error::Corrupt("run list produced a negative LCN".into()));
        }
        out.push(DataRun { lcn, clusters });
        runs = &runs[1 + len_size + off_size..];
    }
    Ok(out)
}

fn le_unsigned(b: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, byte) in b.iter().enumerate() {
        v |= (*byte as u64) << (i * 8);
    }
    v
}

fn le_signed(b: &[u8]) -> i64 {
    let mut v = le_unsigned(b);
    let bits = b.len() * 8;
    // Sign-extend from the run's width.
    if bits < 64 && b.last().is_some_and(|last| last & 0x80 != 0) {
        v |= u64::MAX << bits;
    }
    v as i64
}

/// Locate the unnamed non-resident `$DATA` attribute's run list within a record.
/// Used to find where `$MFT` itself lives on the volume.
pub fn find_data_runs(buf: &[u8]) -> Result<Vec<DataRun>, Error> {
    if !is_file_record(buf) {
        return Err(Error::Corrupt("not a FILE record".into()));
    }
    let used = u32_at(buf, 0x18).unwrap_or(buf.len() as u32) as usize;
    let limit = used.min(buf.len());
    let mut off = u16_at(buf, 0x14).ok_or_else(|| Error::Corrupt("short record".into()))? as usize;

    while off + 8 <= limit {
        let attr_type = u32_at(buf, off).ok_or_else(|| Error::Corrupt("short attr".into()))?;
        if attr_type == ATTR_END {
            break;
        }
        let attr_len =
            u32_at(buf, off + 4).ok_or_else(|| Error::Corrupt("short attr".into()))? as usize;
        if attr_len < 16 || off + attr_len > limit {
            break;
        }
        let non_resident = buf[off + 8] != 0;
        let attr_name_len = buf[off + 9];
        if attr_type == ATTR_DATA && attr_name_len == 0 && non_resident {
            let run_off = u16_at(buf, off + 0x20)
                .ok_or_else(|| Error::Corrupt("no run offset".into()))?
                as usize;
            if run_off >= attr_len {
                return Err(Error::Corrupt("run offset past attribute".into()));
            }
            return parse_data_runs(&buf[off + run_off..off + attr_len]);
        }
        off += attr_len;
    }
    Err(Error::Corrupt("$MFT has no unnamed non-resident $DATA attribute".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic 1024-byte FILE record so the parser can be tested
    /// without an NTFS volume.
    struct RecordBuilder {
        buf: Vec<u8>,
        attrs: Vec<u8>,
        flags: u16,
        seq: u16,
        number: u32,
    }

    impl RecordBuilder {
        fn new(number: u32) -> Self {
            RecordBuilder {
                buf: vec![0u8; 1024],
                attrs: Vec::new(),
                flags: FLAG_IN_USE,
                seq: 1,
                number,
            }
        }

        fn dir(mut self) -> Self {
            self.flags |= FLAG_DIRECTORY;
            self
        }
        fn not_in_use(mut self) -> Self {
            self.flags &= !FLAG_IN_USE;
            self
        }

        fn attr(mut self, ty: u32, non_resident: bool, name_len: u8, value: &[u8]) -> Self {
            // Resident header is 0x18 bytes; pad the total to 8-byte alignment.
            let body = 0x18 + value.len();
            let total = (body + 7) & !7;
            let mut a = vec![0u8; total];
            a[0..4].copy_from_slice(&ty.to_le_bytes());
            a[4..8].copy_from_slice(&(total as u32).to_le_bytes());
            a[8] = non_resident as u8;
            a[9] = name_len;
            a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
            a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
            a[0x18..0x18 + value.len()].copy_from_slice(value);
            self.attrs.extend_from_slice(&a);
            self
        }

        fn std_info(self, mtime: i64) -> Self {
            let mut v = vec![0u8; 0x30];
            v[0x08..0x10].copy_from_slice(&mtime.to_le_bytes());
            self.attr(ATTR_STANDARD_INFORMATION, false, 0, &v)
        }

        fn file_name(self, name: &str, ns: u8, parent: u32, size: u64) -> Self {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut v = vec![0u8; 0x42 + units.len() * 2];
            let parent_ref = parent as u64 | (1u64 << 48);
            v[0x00..0x08].copy_from_slice(&parent_ref.to_le_bytes());
            v[0x30..0x38].copy_from_slice(&size.to_le_bytes());
            v[0x40] = units.len() as u8;
            v[0x41] = ns;
            for (i, u) in units.iter().enumerate() {
                v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
            }
            self.attr(ATTR_FILE_NAME, false, 0, &v)
        }

        fn data_resident(self, size: usize) -> Self {
            self.attr(ATTR_DATA, false, 0, &vec![7u8; size])
        }

        fn data_nonresident(mut self, real_size: u64, runs: &[u8]) -> Self {
            let run_off = 0x40usize;
            let total = (run_off + runs.len() + 7) & !7;
            let mut a = vec![0u8; total];
            a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            a[4..8].copy_from_slice(&(total as u32).to_le_bytes());
            a[8] = 1; // non-resident
            a[9] = 0;
            a[0x20..0x22].copy_from_slice(&(run_off as u16).to_le_bytes());
            a[0x30..0x38].copy_from_slice(&real_size.to_le_bytes());
            a[run_off..run_off + runs.len()].copy_from_slice(runs);
            self.attrs.extend_from_slice(&a);
            self
        }

        /// Finish the record. `fixups` re-applies NTFS's sector stamping so the
        /// round trip through `apply_fixups` can be tested.
        fn build(mut self, fixups: bool) -> Vec<u8> {
            const FIRST_ATTR: usize = 0x38;
            self.buf[0..4].copy_from_slice(MAGIC_FILE);
            self.buf[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes()); // usa offset
            self.buf[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 sectors
            self.buf[0x10..0x12].copy_from_slice(&self.seq.to_le_bytes());
            self.buf[0x14..0x16].copy_from_slice(&(FIRST_ATTR as u16).to_le_bytes());
            self.buf[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
            let used = FIRST_ATTR + self.attrs.len() + 4;
            self.buf[0x18..0x1C].copy_from_slice(&(used as u32).to_le_bytes());
            self.buf[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
            self.buf[0x2C..0x30].copy_from_slice(&self.number.to_le_bytes());
            self.buf[FIRST_ATTR..FIRST_ATTR + self.attrs.len()].copy_from_slice(&self.attrs);
            let end = FIRST_ATTR + self.attrs.len();
            self.buf[end..end + 4].copy_from_slice(&ATTR_END.to_le_bytes());

            if fixups {
                let usn: u16 = 0xBEEF;
                self.buf[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
                for i in 0..2usize {
                    let tail = (i + 1) * 512 - 2;
                    // Stash the real bytes, then stamp the sequence number.
                    let real = [self.buf[tail], self.buf[tail + 1]];
                    self.buf[0x32 + i * 2..0x34 + i * 2].copy_from_slice(&real);
                    self.buf[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
                }
            }
            self.buf
        }
    }

    #[test]
    fn parses_a_plain_file() {
        let buf = RecordBuilder::new(42)
            .std_info(133_485_408_000_000_000)
            .file_name("notepad.exe", NS_WIN32, 5, 0)
            .data_nonresident(4096, &[0x11, 0x10, 0x20])
            .build(false);
        let r = parse_record(&buf).expect("should parse");
        assert_eq!(r.name.as_deref(), Some("notepad.exe"));
        assert_eq!(r.parent, 5);
        assert_eq!(r.record_number, Some(42));
        assert_eq!(r.size, 4096);
        assert_eq!(r.mtime, 133_485_408_000_000_000);
        assert!(!r.is_dir);
    }

    #[test]
    fn resident_data_size_comes_from_the_value_length() {
        let buf = RecordBuilder::new(9)
            .file_name("tiny.txt", NS_WIN32, 5, 0)
            .data_resident(17)
            .build(false);
        let r = parse_record(&buf).unwrap();
        assert_eq!(r.size, 17);
    }

    #[test]
    fn directories_are_flagged_and_have_no_size() {
        let buf = RecordBuilder::new(7)
            .dir()
            .file_name("Windows", NS_WIN32, ROOT_RECORD, 999)
            .build(false);
        let r = parse_record(&buf).unwrap();
        assert!(r.is_dir);
        assert_eq!(r.size, 0);
        assert_eq!(r.parent, ROOT_RECORD);
    }

    #[test]
    fn win32_name_wins_over_the_dos_short_name() {
        // NTFS emits both; indexing the 8.3 name would show users garbage.
        let buf = RecordBuilder::new(11)
            .file_name("PROGRA~1", NS_DOS, 5, 0)
            .file_name("Program Files", NS_WIN32, 5, 0)
            .build(false);
        let r = parse_record(&buf).unwrap();
        assert_eq!(r.name.as_deref(), Some("Program Files"));

        // Order must not matter.
        let buf = RecordBuilder::new(11)
            .file_name("Program Files", NS_WIN32, 5, 0)
            .file_name("PROGRA~1", NS_DOS, 5, 0)
            .build(false);
        assert_eq!(parse_record(&buf).unwrap().name.as_deref(), Some("Program Files"));
    }

    #[test]
    fn dos_only_name_is_still_better_than_nothing() {
        let buf = RecordBuilder::new(12).file_name("SHORT~1.TXT", NS_DOS, 5, 0).build(false);
        assert_eq!(parse_record(&buf).unwrap().name.as_deref(), Some("SHORT~1.TXT"));
    }

    #[test]
    fn unicode_names_survive_utf16_decoding() {
        let buf =
            RecordBuilder::new(13).file_name("日本語 Ünïcode.txt", NS_WIN32, 5, 0).build(false);
        assert_eq!(parse_record(&buf).unwrap().name.as_deref(), Some("日本語 Ünïcode.txt"));
    }

    #[test]
    fn skips_unused_and_extension_records() {
        let unused =
            RecordBuilder::new(1).not_in_use().file_name("gone.txt", NS_WIN32, 5, 0).build(false);
        assert!(parse_record(&unused).is_none());

        let mut ext = RecordBuilder::new(2).file_name("frag.txt", NS_WIN32, 5, 0).build(false);
        ext[0x20..0x28].copy_from_slice(&99u64.to_le_bytes()); // base record ref
        assert!(parse_record(&ext).is_none());
    }

    #[test]
    fn records_without_a_name_are_skipped() {
        let buf = RecordBuilder::new(3).std_info(1).data_resident(4).build(false);
        assert!(parse_record(&buf).is_none());
    }

    #[test]
    fn zeroed_and_bad_slots_are_rejected() {
        assert!(parse_record(&[0u8; 1024]).is_none());
        let mut bad = vec![0u8; 1024];
        bad[0..4].copy_from_slice(MAGIC_BAAD);
        assert!(parse_record(&bad).is_none());
        assert!(is_bad_record(&bad));
    }

    #[test]
    fn fixups_round_trip() {
        let mut buf = RecordBuilder::new(42)
            .file_name("across-sector-boundary.txt", NS_WIN32, 5, 0)
            .build(true);
        // With the stamping in place the tail bytes are the sequence number.
        assert_eq!(u16_at(&buf, 510), Some(0xBEEF));
        apply_fixups(&mut buf, 512).expect("fixups apply");
        // And afterwards they are whatever the record really held.
        assert_eq!(u16_at(&buf, 510), Some(0));
        assert!(parse_record(&buf).is_some());
    }

    #[test]
    fn fixup_mismatch_is_reported_not_ignored() {
        let mut buf = RecordBuilder::new(1).file_name("x", NS_WIN32, 5, 0).build(true);
        buf[510] ^= 0xFF; // simulate a torn sector
        assert!(apply_fixups(&mut buf, 512).is_err());
    }

    #[test]
    fn attribute_walk_cannot_loop_or_overrun() {
        // A zero-length attribute would spin forever without the guard.
        let mut buf = RecordBuilder::new(1).file_name("a.txt", NS_WIN32, 5, 0).build(false);
        buf[0x38 + 4..0x38 + 8].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_record(&buf).is_none()); // bails out, does not hang

        // An attribute claiming to be enormous must not read past the buffer.
        let mut buf = RecordBuilder::new(1).file_name("a.txt", NS_WIN32, 5, 0).build(false);
        buf[0x38 + 4..0x38 + 8].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        assert!(parse_record(&buf).is_none());
    }

    #[test]
    fn truncated_records_do_not_panic() {
        let full = RecordBuilder::new(1).file_name("a.txt", NS_WIN32, 5, 0).build(false);
        for cut in [0, 1, 4, 8, 0x2C, 0x38, 0x40, 0x50] {
            let _ = parse_record(&full[..cut.min(full.len())]);
        }
    }

    #[test]
    fn data_runs_decode_single_and_multi_extent() {
        // 0x11 0x10 0x20 => 1 length byte, 1 offset byte: 0x10 clusters at LCN 0x20.
        let runs = parse_data_runs(&[0x11, 0x10, 0x20, 0x00]).unwrap();
        assert_eq!(runs, vec![DataRun { lcn: 0x20, clusters: 0x10 }]);

        // Second extent's offset is a delta from the first.
        let runs = parse_data_runs(&[0x11, 0x10, 0x20, 0x11, 0x08, 0x10, 0x00]).unwrap();
        assert_eq!(
            runs,
            vec![DataRun { lcn: 0x20, clusters: 0x10 }, DataRun { lcn: 0x30, clusters: 0x08 }]
        );
    }

    #[test]
    fn data_run_offsets_are_signed() {
        // 0xFF as a one-byte delta means -1, i.e. the MFT fragment moved backwards.
        let runs = parse_data_runs(&[0x11, 0x04, 0x40, 0x11, 0x04, 0xF0, 0x00]).unwrap();
        assert_eq!(runs[1].lcn, 0x40 - 0x10);
    }

    #[test]
    fn sparse_runs_are_skipped() {
        // High nibble 0 => sparse: no clusters to read.
        let runs = parse_data_runs(&[0x01, 0x08, 0x11, 0x04, 0x20, 0x00]).unwrap();
        assert_eq!(runs, vec![DataRun { lcn: 0x20, clusters: 4 }]);
    }

    #[test]
    fn malformed_run_lists_error_rather_than_panic() {
        assert!(parse_data_runs(&[0x11, 0x10]).is_err()); // truncated
        assert!(parse_data_runs(&[0x09, 0x01]).is_err()); // absurd length width
    }

    #[test]
    fn finds_the_mft_data_runs() {
        let buf = RecordBuilder::new(0)
            .file_name("$MFT", NS_WIN32, ROOT_RECORD, 0)
            .data_nonresident(1 << 20, &[0x32, 0x00, 0x10, 0x00, 0x00, 0x0C, 0x00])
            .build(false);
        let runs = find_data_runs(&buf).unwrap();
        assert_eq!(runs, vec![DataRun { lcn: 0xC0000, clusters: 0x1000 }]);
    }

    #[test]
    fn frn_splits_into_record_and_sequence() {
        let frn = 0x0007_0000_0001_2345u64;
        assert_eq!(frn_record(frn), 0x12345);
        assert_eq!(frn_sequence(frn), 7);
    }
}
