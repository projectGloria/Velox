//! # velox-core
//!
//! The Velox indexing and search engine.
//!
//! The engine is a library so that the CLI, the benchmark harness, and the GUI
//! are all just consumers of the same code. Nothing here draws anything or owns
//! a message loop.
//!
//! ## Shape of the thing
//!
//! ```text
//!   NTFS volume ──raw MFT read──┐
//!                               ├──> EntryStore (struct-of-arrays, packed names)
//!   non-NTFS ──FindFirstFile────┘            │
//!                                            v
//!                                    VolumeIndex (sorted order + delta overlay)
//!                                            │
//!   USN journal ──live updates──────────────>│
//!                                            v
//!                                     Searcher (prefix binary search,
//!                                               incremental narrowing)
//! ```
//!
//! ## Elevation
//!
//! Reading the MFT and the USN journal requires opening a raw volume handle
//! (`\\.\C:`), which requires administrator privileges. Every entry point that
//! needs elevation says so in its documentation and returns
//! [`Error::AccessDenied`] rather than panicking, so callers can fall back to
//! [`walk`] over user-selected folders. See `docs/elevation.md`.

pub mod fold;
pub mod index;
pub mod query;
pub mod store;
pub mod walk;

#[cfg(windows)]
pub mod ntfs;

pub mod persist;

use std::fmt;

pub use index::{EntryInfo, Index, SourceKind, VolumeIndex};
pub use query::{MatchMode, Matches, Plan, Query, Scope, SearchResults, Searcher, SortKey};
pub use store::{EntryId, EntryRef, EntryStore, VolumeId};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The operation needs administrator privileges. Callers should degrade to
    /// the `FindFirstFile` walk rather than failing.
    AccessDenied(String),
    /// Volume exists but is not NTFS, so there is no MFT to read.
    NotNtfs(String),
    /// The USN journal was deleted, disabled, or wrapped past our checkpoint. The
    /// index must be rebuilt from scratch for this volume.
    JournalInvalidated(String),
    /// On-disk snapshot is from a different version or a different volume state.
    StaleSnapshot(String),
    Io(std::io::Error),
    Corrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::AccessDenied(s) => {
                write!(f, "access denied ({s}): reading the MFT requires running as administrator")
            }
            Error::NotNtfs(s) => write!(f, "{s} is not an NTFS volume"),
            Error::JournalInvalidated(s) => write!(f, "USN journal invalidated on {s}"),
            Error::StaleSnapshot(s) => write!(f, "index snapshot unusable: {s}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corrupt(s) => write!(f, "corrupt on-disk structure: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Windows FILETIME (100 ns ticks since 1601-01-01 UTC) as `YYYY-MM-DD HH:MM:SS`.
pub fn format_filetime(ft: i64) -> String {
    if ft <= 0 {
        return "-".to_string();
    }
    let secs = ft / 10_000_000;
    let days = secs / 86_400;
    let tod = secs % 86_400;
    // Inverse of the civil-days algorithm, rebased from 1601 to the 1970 epoch.
    let z = days - 134_774 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Human-readable byte count for display.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_roundtrips_known_dates() {
        // 1970-01-01T00:00:00Z
        assert_eq!(format_filetime(116_444_736_000_000_000), "1970-01-01 00:00:00");
        // 2024-01-01T00:00:00Z == (1704067200 + 11644473600) * 10^7
        assert_eq!(format_filetime(133_485_408_000_000_000), "2024-01-01 00:00:00");
        assert_eq!(format_filetime(0), "-");
    }

    #[test]
    fn sizes_format() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
