//! Volume discovery and raw volume handles.
//!
//! # Elevation
//!
//! Opening `\\.\C:` for raw read access requires administrator privileges. This
//! is the *only* thing in Velox that does, and it is needed for both the initial
//! MFT read and for reading the USN journal. Everything here reports a missing
//! privilege as [`Error::AccessDenied`] so the caller can fall back to
//! [`crate::walk`] rather than dying. See `docs/elevation.md`.

use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, FILE_ATTRIBUTE_NORMAL,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::IO::DeviceIoControl;

use crate::Error;

// `GetDriveTypeW` return values. Not re-exported by the `windows` crate's
// filesystem module, so they are spelled out here.
pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
pub const DRIVE_CDROM: u32 = 5;

/// Windows error code for a denied raw-volume open.
const ERROR_ACCESS_DENIED: i32 = 5;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Is this process running elevated?
///
/// Checked up front so the UI can prompt for elevation (or offer the folder-walk
/// fallback) instead of discovering the problem partway through indexing.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Drive letter, e.g. `C`.
    pub letter: char,
    /// Display label used as the root of reconstructed paths, e.g. `C:`.
    pub label: String,
    /// Filesystem name reported by the OS, e.g. `NTFS`, `exFAT`.
    pub fs_name: String,
    pub drive_type: u32,
}

impl VolumeInfo {
    pub fn is_ntfs(&self) -> bool {
        self.fs_name.eq_ignore_ascii_case("NTFS")
    }
    /// Local, non-network, and therefore in scope for v1.
    pub fn is_local(&self) -> bool {
        matches!(self.drive_type, DRIVE_FIXED | DRIVE_REMOVABLE)
    }
    /// Path for the raw device handle, e.g. `\\.\C:`.
    pub fn device_path(&self) -> String {
        format!(r"\\.\{}:", self.letter)
    }
    pub fn root_path(&self) -> String {
        format!(r"{}:\", self.letter)
    }
}

/// All mounted drive letters, with their filesystem type.
///
/// Removable drives are included so a USB stick plugged in later can be picked
/// up by re-enumerating; nothing here caches across calls.
pub fn enumerate_volumes() -> Vec<VolumeInfo> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = wide(&format!(r"{letter}:\"));
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
        // Network drives are explicitly out of scope for v1.
        if drive_type == DRIVE_REMOTE || drive_type == DRIVE_CDROM {
            continue;
        }
        let mut fs_buf = [0u16; 32];
        let fs_name = unsafe {
            match GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut fs_buf),
            ) {
                Ok(()) => {
                    let len = fs_buf.iter().position(|c| *c == 0).unwrap_or(fs_buf.len());
                    String::from_utf16_lossy(&fs_buf[..len])
                }
                // An empty card reader slot, or a drive we cannot query.
                Err(_) => continue,
            }
        };
        out.push(VolumeInfo {
            letter,
            label: format!("{letter}:"),
            fs_name,
            drive_type,
        });
    }
    out
}

/// Owning wrapper around a raw volume handle.
pub struct RawVolume {
    handle: HANDLE,
    pub info: VolumeInfo,
}

// SAFETY: `HANDLE` is a raw pointer and so not automatically `Send`/`Sync`, but
// a volume handle is safe to move between threads. The one shared-mutable piece
// of state is the file pointer that `read_at` moves, so the MFT reader confines
// all reads to a single thread; nothing else calls `read_at` concurrently.
unsafe impl Send for RawVolume {}
unsafe impl Sync for RawVolume {}

impl Drop for RawVolume {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Geometry needed to locate and read MFT records.
#[derive(Debug, Clone, Copy)]
pub struct NtfsGeometry {
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_record: u32,
    /// Bytes of the MFT that actually contain records.
    pub mft_valid_length: u64,
    pub mft_start_lcn: i64,
    pub total_clusters: i64,
    pub volume_serial: i64,
}

impl NtfsGeometry {
    /// Upper bound on MFT record slots, which is also the number of entry slots
    /// the store must preallocate (`EntryId` == MFT record number).
    pub fn max_records(&self) -> usize {
        if self.bytes_per_record == 0 {
            return 0;
        }
        (self.mft_valid_length / self.bytes_per_record as u64) as usize
    }
}

impl RawVolume {
    /// Open a volume for raw reads.
    ///
    /// **Requires administrator privileges.** Returns [`Error::AccessDenied`]
    /// when the process is not elevated, which callers should treat as "fall
    /// back to the folder walk", not as a fatal error.
    pub fn open(info: &VolumeInfo) -> Result<RawVolume, Error> {
        if !info.is_ntfs() {
            return Err(Error::NotNtfs(info.label.clone()));
        }
        let path = wide(&info.device_path());
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0,
                // Share everything: the volume is mounted and in active use.
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        match handle {
            Ok(h) => Ok(RawVolume { handle: h, info: info.clone() }),
            Err(e) if e.code().0 & 0xFFFF == ERROR_ACCESS_DENIED => {
                Err(Error::AccessDenied(info.label.clone()))
            }
            Err(e) => Err(Error::Io(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))),
        }
    }

    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    /// `FSCTL_GET_NTFS_VOLUME_DATA`: where the MFT starts and how big its records
    /// are. Everything else in the MFT reader is derived from this.
    pub fn geometry(&self) -> Result<NtfsGeometry, Error> {
        let mut data = NTFS_VOLUME_DATA_BUFFER::default();
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some(&mut data as *mut _ as *mut _),
                size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                Some(&mut returned),
                None,
            )
            .map_err(|e| {
                Error::Io(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
            })?;
        }
        if data.BytesPerFileRecordSegment == 0 || data.BytesPerCluster == 0 {
            return Err(Error::Corrupt("volume reported zero record or cluster size".into()));
        }
        Ok(NtfsGeometry {
            bytes_per_sector: data.BytesPerSector,
            bytes_per_cluster: data.BytesPerCluster,
            bytes_per_record: data.BytesPerFileRecordSegment,
            mft_valid_length: data.MftValidDataLength as u64,
            mft_start_lcn: data.MftStartLcn,
            total_clusters: data.TotalClusters,
            volume_serial: data.VolumeSerialNumber,
        })
    }

    /// Read `buf.len()` bytes from a byte offset on the volume.
    ///
    /// Raw volume handles require sector-aligned offsets and lengths; callers
    /// work in whole clusters, which always satisfies that.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        use windows::Win32::Storage::FileSystem::{ReadFile, SetFilePointerEx, FILE_BEGIN};
        unsafe {
            SetFilePointerEx(self.handle, offset as i64, None, FILE_BEGIN).map_err(|e| {
                Error::Io(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
            })?;
            let mut read = 0u32;
            ReadFile(self.handle, Some(buf), Some(&mut read), None).map_err(|e| {
                Error::Io(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
            })?;
            Ok(read as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_finds_the_system_drive() {
        let vols = enumerate_volumes();
        assert!(!vols.is_empty(), "expected at least one local volume");
        let c = vols.iter().find(|v| v.letter == 'C').expect("C: should exist");
        assert_eq!(c.label, "C:");
        assert_eq!(c.device_path(), r"\\.\C:");
        assert_eq!(c.root_path(), r"C:\");
    }

    #[test]
    fn ntfs_detection_is_case_insensitive() {
        let v = VolumeInfo {
            letter: 'X',
            label: "X:".into(),
            fs_name: "ntfs".into(),
            drive_type: DRIVE_FIXED,
        };
        assert!(v.is_ntfs());
        assert!(v.is_local());
        let v = VolumeInfo { fs_name: "exFAT".into(), ..v };
        assert!(!v.is_ntfs());
    }

    #[test]
    fn opening_a_non_ntfs_volume_is_rejected_before_any_syscall() {
        let v = VolumeInfo {
            letter: 'X',
            label: "X:".into(),
            fs_name: "FAT32".into(),
            drive_type: DRIVE_REMOVABLE,
        };
        assert!(matches!(RawVolume::open(&v), Err(Error::NotNtfs(_))));
    }

    #[test]
    fn geometry_reports_record_capacity() {
        let g = NtfsGeometry {
            bytes_per_sector: 512,
            bytes_per_cluster: 4096,
            bytes_per_record: 1024,
            mft_valid_length: 1024 * 5_000_000,
            mft_start_lcn: 786_432,
            total_clusters: 1 << 26,
            volume_serial: 0,
        };
        assert_eq!(g.max_records(), 5_000_000);
    }

    /// Not a correctness assertion — the result depends on how the test runner
    /// was launched. It exists so the elevation probe is exercised at all.
    #[test]
    fn elevation_probe_runs() {
        let _ = is_elevated();
    }

    /// The unelevated case must be a clean `AccessDenied`, never a panic. When
    /// the suite *is* run elevated this asserts the open succeeds instead.
    #[test]
    fn raw_open_reports_denial_rather_than_panicking() {
        let vols = enumerate_volumes();
        let Some(c) = vols.into_iter().find(|v| v.letter == 'C' && v.is_ntfs()) else {
            return;
        };
        match RawVolume::open(&c) {
            Ok(vol) => {
                let g = vol.geometry().expect("geometry on an open NTFS volume");
                assert!(g.bytes_per_record >= 512);
                assert!(g.bytes_per_cluster >= 512);
            }
            Err(Error::AccessDenied(l)) => assert_eq!(l, "C:"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
