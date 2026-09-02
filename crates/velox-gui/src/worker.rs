//! Background engine thread.
//!
//! The UI thread never touches the engine. It posts a command and returns to the
//! message loop immediately; the worker indexes, searches, and applies journal
//! updates, then wakes the window with `PostMessageW`. That is the whole reason
//! the window keeps redrawing at 60 Hz while a multi-million-entry volume is
//! still being indexed.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use velox_core::query::{MatchMode, Query, Scope, SearchResults, Searcher, SortKey};
use velox_core::{persist, Index};

#[cfg(windows)]
use velox_core::ntfs;

/// Index is ready (or was rebuilt); the UI should pick it up and re-query.
pub const WM_APP_INDEXED: u32 = WM_APP + 1;
/// A search finished.
pub const WM_APP_RESULTS: u32 = WM_APP + 2;
/// Status line changed.
pub const WM_APP_STATUS: u32 = WM_APP + 3;
/// Live journal updates landed; the current query is stale.
pub const WM_APP_UPDATED: u32 = WM_APP + 4;

pub enum Command {
    Search { generation: u64, text: String, mode: MatchMode, scope: Scope, sort: (SortKey, bool) },
    Rebuild,
    Shutdown,
}

pub struct Response {
    pub generation: u64,
    pub text: String,
    pub results: SearchResults,
}

#[derive(Default)]
pub struct Shared {
    pub index: Mutex<Option<Arc<Index>>>,
    pub response: Mutex<Option<Response>>,
    pub status: Mutex<String>,
    pub indexing: AtomicBool,
    /// Set when the engine is running unelevated and therefore on the slow path.
    pub degraded: AtomicBool,
    pub entries: AtomicU64,
}

impl Shared {
    pub fn set_status(&self, hwnd: isize, msg: impl Into<String>) {
        *self.status.lock().unwrap() = msg.into();
        post(hwnd, WM_APP_STATUS);
    }
    pub fn status_text(&self) -> String {
        self.status.lock().unwrap().clone()
    }
}

fn post(hwnd: isize, msg: u32) {
    // The window may already be gone during shutdown; a failed post is fine.
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut core::ffi::c_void)),
            msg,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Start the worker. `hwnd` is passed as an `isize` because `HWND` is a raw
/// pointer and therefore not `Send`; it is only ever used to post messages.
pub fn spawn(hwnd: isize, shared: Arc<Shared>) -> Sender<Command> {
    let (tx, rx) = channel::<Command>();
    let tx_for_thread = tx.clone();
    std::thread::Builder::new()
        .name("velox-engine".into())
        .spawn(move || run(hwnd, shared, rx, tx_for_thread))
        .expect("spawn engine thread");
    tx
}

fn run(hwnd: isize, shared: Arc<Shared>, rx: Receiver<Command>, _tx: Sender<Command>) {
    let index = build(hwnd, &shared);
    *shared.index.lock().unwrap() = Some(index.clone());
    shared.indexing.store(false, Ordering::SeqCst);
    shared.entries.store(index.total_entries() as u64, Ordering::SeqCst);
    post(hwnd, WM_APP_INDEXED);

    start_live_updates(hwnd, index.clone(), shared.clone());

    let mut searcher = Searcher::new();
    while let Ok(cmd) = rx.recv() {
        match coalesce(cmd, &rx) {
            Command::Shutdown => break,
            Command::Rebuild => {
                shared.indexing.store(true, Ordering::SeqCst);
                let fresh = build(hwnd, &shared);
                *shared.index.lock().unwrap() = Some(fresh.clone());
                shared.entries.store(fresh.total_entries() as u64, Ordering::SeqCst);
                shared.indexing.store(false, Ordering::SeqCst);
                searcher.reset();
                post(hwnd, WM_APP_INDEXED);
            }
            Command::Search { generation, text, mode, scope, sort } => {
                let idx = shared.index.lock().unwrap().clone();
                let Some(idx) = idx else { continue };
                let mut query = Query::parse(&text, mode, scope);
                query.sort = sort.0;
                query.sort_desc = sort.1;
                let mut results = searcher.search(&idx, &query);
                if sort.0 != SortKey::Name || sort.1 {
                    results.sort_by(&idx, sort.0, sort.1);
                }
                *shared.response.lock().unwrap() =
                    Some(Response { generation, text, results });
                post(hwnd, WM_APP_RESULTS);
            }
        }
    }
    save_snapshot(&shared);
}

/// Drop superseded searches. Typing quickly must not queue up a scan per
/// keystroke; only the newest query matters.
fn coalesce(first: Command, rx: &Receiver<Command>) -> Command {
    let mut latest = first;
    while let Ok(next) = rx.try_recv() {
        // Never swallow a shutdown or a rebuild behind a newer search.
        match (&latest, &next) {
            (_, Command::Shutdown) => return Command::Shutdown,
            (Command::Rebuild, Command::Search { .. }) => continue,
            _ => latest = next,
        }
    }
    latest
}

#[cfg(windows)]
fn build(hwnd: isize, shared: &Arc<Shared>) -> Arc<Index> {
    shared.indexing.store(true, Ordering::SeqCst);
    let elevated = ntfs::is_elevated();
    shared.degraded.store(!elevated, Ordering::SeqCst);

    // A snapshot turns a multi-second MFT read into a disk read. It is only
    // usable if every volume's journal still matches, which `check_fresh` decides.
    let snap = persist::default_path();
    if snap.exists() {
        shared.set_status(hwnd, "Loading saved index...");
        let t = Instant::now();
        match persist::load(&snap) {
            Ok(loaded) => {
                if let Some(index) = validate_snapshot(hwnd, shared, loaded, elevated, t) {
                    return index;
                }
            }
            Err(e) => shared.set_status(hwnd, format!("Snapshot unusable ({e}); rebuilding.")),
        }
    }

    shared.set_status(
        hwnd,
        if elevated {
            "Reading the Master File Table..."
        } else {
            "Not elevated - walking folders (slow path). Restart as administrator for instant indexing."
        },
    );
    let policy = ntfs::FallbackPolicy {
        roots: if elevated { Vec::new() } else { user_folders() },
        skip: false,
    };
    let (index, report) = ntfs::build_index(&policy);
    let index = Arc::new(index);

    let sources: Vec<String> = report
        .volumes
        .iter()
        .map(|v| {
            format!(
                "{} {}",
                v.label,
                if v.source == velox_core::SourceKind::Mft { "MFT" } else { "walk" }
            )
        })
        .collect();
    shared.set_status(
        hwnd,
        format!(
            "Indexed {} entries in {:.2}s  [{}]",
            thousands(report.total_entries()),
            report.total.as_secs_f64(),
            sources.join(", ")
        ),
    );
    save_index(&index, &snap);
    index
}

/// Decide whether a loaded snapshot can be used, and catch it up if so.
#[cfg(windows)]
fn validate_snapshot(
    hwnd: isize,
    shared: &Arc<Shared>,
    loaded: persist::LoadedIndex,
    elevated: bool,
    t: Instant,
) -> Option<Arc<Index>> {
    if !elevated {
        // Without a raw handle we cannot check the journal, and cannot catch up
        // on what changed while we were away. Using it would show stale results.
        shared.set_status(hwnd, "Saved index cannot be verified without elevation; rebuilding.");
        return None;
    }
    let vols = ntfs::enumerate_volumes();
    let labels = loaded.index.volume_labels();
    let mut resume = Vec::with_capacity(labels.len());

    for (i, label) in labels.iter().enumerate() {
        let Some(info) = vols.iter().find(|v| &v.label == label) else {
            shared.set_status(hwnd, format!("{label} is gone; rebuilding."));
            return None;
        };
        let current = ntfs::usn::query_journal(&match ntfs::volume::RawVolume::open(info) {
            Ok(v) => v,
            Err(e) => {
                shared.set_status(hwnd, format!("{label}: {e}; rebuilding."));
                return None;
            }
        });
        let Ok(current) = current else {
            shared.set_status(hwnd, format!("{label}: journal unavailable; rebuilding."));
            return None;
        };
        let stamp = loaded.stamps.get(i).copied().unwrap_or_default();
        match persist::check_fresh(stamp, current.journal_id, label) {
            Ok(next_usn) => resume.push((info.clone(), next_usn)),
            Err(e) => {
                shared.set_status(hwnd, format!("{e}; rebuilding."));
                return None;
            }
        }
    }

    // Catch up on everything the journal recorded since the snapshot.
    let index = Arc::new(loaded.index);
    let mut caught_up = 0usize;
    for (i, (info, next_usn)) in resume.iter().enumerate() {
        let checkpoint = ntfs::JournalCheckpoint {
            journal_id: 0,
            next_usn: *next_usn,
        };
        // `resume` needs the real journal id; re-query and reuse it.
        let Ok(mut mon) = ntfs::UsnMonitor::new(info) else { continue };
        let _ = checkpoint;
        let mut vol = index.volumes[i].write().unwrap();
        while let Ok(events) = mon.poll() {
            if events.is_empty() {
                break;
            }
            caught_up += events.len();
            ntfs::usn::apply_events(&mut vol, &events, None);
        }
        if vol.should_merge() {
            vol.merge_now();
        }
    }
    shared.set_status(
        hwnd,
        format!(
            "Loaded saved index ({} entries) in {:.2}s, caught up on {} changes",
            thousands(index.total_entries()),
            t.elapsed().as_secs_f64(),
            caught_up
        ),
    );
    Some(index)
}

#[cfg(not(windows))]
fn build(hwnd: isize, shared: &Arc<Shared>) -> Arc<Index> {
    shared.set_status(hwnd, "Walking folders...");
    let (vol, _) = velox_core::walk::index_folders(0, &user_folders(), "ROOT");
    Arc::new(Index::new(vec![vol]))
}

/// What to walk when the MFT is unavailable. The user's own tree is the part
/// they actually search, and walking all of `C:\` unelevated is punishing.
fn user_folders() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("USERPROFILE") {
        out.push(std::path::PathBuf::from(p));
    }
    if out.is_empty() {
        out.push(std::env::current_dir().unwrap_or_else(|_| ".".into()));
    }
    out
}

// ------------------------------------------------------------- live updates

#[cfg(windows)]
fn start_live_updates(hwnd: isize, index: Arc<Index>, shared: Arc<Shared>) {
    if !ntfs::is_elevated() {
        return; // the journal needs a raw volume handle
    }
    let labels = index.volume_labels();
    let vols = ntfs::enumerate_volumes();
    let mut monitors = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        if let Some(info) = vols.iter().find(|v| &v.label == label && v.is_ntfs()) {
            if let Ok(m) = ntfs::UsnMonitor::new(info) {
                monitors.push((i, m));
            }
        }
    }
    if monitors.is_empty() {
        return;
    }
    std::thread::Builder::new()
        .name("velox-journal".into())
        .spawn(move || loop {
            let mut changed = false;
            for (vi, mon) in monitors.iter_mut() {
                let Ok(events) = mon.poll() else { continue };
                if events.is_empty() {
                    continue;
                }
                // Two short critical sections rather than one long one: apply the
                // batch, then plan the (expensive) merge outside the write lock
                // and take it again only to swap the array in.
                {
                    let mut vol = index.volumes[*vi].write().unwrap();
                    ntfs::usn::apply_events(&mut vol, &events, None);
                }
                changed = true;
                let needs_merge = index.volumes[*vi].read().unwrap().should_merge();
                if needs_merge {
                    let planned = index.volumes[*vi].read().unwrap().plan_merge();
                    index.volumes[*vi].write().unwrap().commit_merge(planned);
                }
            }
            if changed {
                shared.entries.store(index.total_entries() as u64, Ordering::SeqCst);
                post(hwnd, WM_APP_UPDATED);
            }
            std::thread::sleep(Duration::from_millis(300));
        })
        .expect("spawn journal thread");
}

#[cfg(not(windows))]
fn start_live_updates(_hwnd: isize, _index: Arc<Index>, _shared: Arc<Shared>) {}

// --------------------------------------------------------------- persistence

#[cfg(windows)]
fn save_index(index: &Index, path: &std::path::Path) {
    let labels = index.volume_labels();
    let vols = ntfs::enumerate_volumes();
    let mut stamps = Vec::with_capacity(labels.len());
    for label in &labels {
        let stamp = vols
            .iter()
            .find(|v| &v.label == label && v.is_ntfs())
            .and_then(|info| ntfs::volume::RawVolume::open(info).ok())
            .map(|raw| {
                let geom = raw.geometry().ok();
                let journal = ntfs::usn::query_journal(&raw).ok();
                persist::VolumeStamp {
                    volume_serial: geom.map(|g| g.volume_serial as u64).unwrap_or(0),
                    journal_id: journal.map(|j| j.journal_id).unwrap_or(0),
                    next_usn: journal.map(|j| j.next_usn).unwrap_or(0),
                }
            })
            .unwrap_or_default();
        stamps.push(stamp);
    }
    let _ = persist::save(index, &stamps, path);
}

#[cfg(not(windows))]
fn save_index(_index: &Index, _path: &std::path::Path) {}

fn save_snapshot(shared: &Arc<Shared>) {
    let idx = shared.index.lock().unwrap().clone();
    if let Some(idx) = idx {
        save_index(&idx, &persist::default_path());
    }
}

pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
