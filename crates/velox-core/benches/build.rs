//! Index-construction benchmarks, plus the MFT-versus-`FindFirstFile`
//! comparison the whole architecture rests on.
//!
//! Run with `cargo bench -p velox-core --bench build`.
//!
//! The MFT benchmark needs administrator privileges and reports that it was
//! skipped rather than failing when run unprivileged, so `cargo bench` stays
//! usable from an ordinary shell.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use velox_core::index::{synthetic_volume, SourceKind, VolumeIndex};
use velox_core::store::EntryStore;
use velox_core::walk;

/// Sorting the order array is the dominant cost of turning a populated store into
/// a queryable index, so it is measured on its own.
fn sort_into_index(c: &mut Criterion) {
    let mut g = c.benchmark_group("build/sort");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(20));
    for n in [100_000usize, 1_000_000] {
        // Build the store once; each iteration re-sorts a fresh copy of it.
        let template = synthetic_volume(0, "C:", n);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter_batched(
                || clone_store(&template.store),
                |store| {
                    std::hint::black_box(VolumeIndex::build(0, SourceKind::Walk, store).len())
                },
                criterion::BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

/// `EntryStore` is deliberately not `Clone` (it is large and cloning it is almost
/// always a mistake), so the benchmark rebuilds one explicitly.
fn clone_store(src: &EntryStore) -> EntryStore {
    let mut out = EntryStore::new(src.label.clone());
    out.reserve(src.slots(), src.name_bytes());
    for id in 0..src.slots() as u32 {
        if src.is_live(id) {
            out.set_slot(
                id,
                src.name(id),
                src.parent(id),
                src.size(id),
                src.mtime(id),
                src.seq(id),
                src.is_dir(id),
            );
        }
    }
    out.set_root(src.root());
    out
}

/// The `FindFirstFile` baseline, serial and parallel, over a real directory tree.
fn directory_walk(c: &mut Criterion) {
    let root = walk_target();
    let (_, probe) = walk::walk_serial(std::slice::from_ref(&root), "BENCH");
    if probe.total() < 500 {
        eprintln!(
            "SKIP walk benchmarks: {} holds only {} entries",
            root.display(),
            probe.total()
        );
        return;
    }
    eprintln!("walk target: {} ({} entries)", root.display(), probe.total());

    let mut g = c.benchmark_group("build/walk");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(30));
    g.throughput(Throughput::Elements(probe.total() as u64));
    g.bench_function("serial_findfirstfile", |b| {
        b.iter(|| std::hint::black_box(walk::walk_serial(std::slice::from_ref(&root), "BENCH").1.total()))
    });
    g.bench_function("parallel_findfirstfile", |b| {
        b.iter(|| std::hint::black_box(walk::walk_parallel(std::slice::from_ref(&root), "BENCH").1.total()))
    });
    g.finish();
}

/// A directory big enough to be meaningful but cheap enough to walk repeatedly.
fn walk_target() -> PathBuf {
    if let Some(p) = std::env::var_os("VELOX_BENCH_DIR") {
        return PathBuf::from(p);
    }
    for candidate in [r"C:\Windows\System32", r"C:\Program Files"] {
        let p = PathBuf::from(candidate);
        if p.is_dir() {
            return p;
        }
    }
    std::env::current_dir().unwrap()
}

/// Raw MFT read of a whole volume, timed once. Criterion is the wrong tool for a
/// multi-second whole-volume read, so this reports a single measured run and the
/// per-entry rate to compare against the walk above.
#[cfg(windows)]
fn mft_read(c: &mut Criterion) {
    use velox_core::ntfs;

    if !ntfs::is_elevated() {
        eprintln!(
            "SKIP MFT benchmark: reading the MFT needs administrator privileges. \
             Re-run `cargo bench` from an elevated shell."
        );
        return;
    }
    let Some(info) = ntfs::enumerate_volumes().into_iter().find(|v| v.letter == 'C' && v.is_ntfs())
    else {
        eprintln!("SKIP MFT benchmark: no NTFS C:");
        return;
    };

    let t = Instant::now();
    match ntfs::mft::build_volume_index(0, &info) {
        Ok((idx, stats)) => {
            let elapsed = t.elapsed();
            eprintln!(
                "\nMFT read of C:  {} entries from {} records in {:?}\n\
                                 {:.0} entries/sec, {:.1} MB read, {:.1} bytes/entry resident\n",
                stats.entries_indexed,
                stats.records_scanned,
                elapsed,
                stats.records_per_sec(),
                stats.bytes_read as f64 / (1 << 20) as f64,
                idx.memory_bytes() as f64 / idx.len().max(1) as f64,
            );
            // Register a single-sample benchmark so the number lands in the report.
            let mut g = c.benchmark_group("build/mft");
            g.sample_size(10);
            g.measurement_time(Duration::from_secs(5));
            g.bench_function("prefix_query_on_real_volume", |b| {
                use velox_core::query::{MatchMode, Query, Scope, Searcher};
                let index = velox_core::Index::new(vec![
                    ntfs::mft::build_volume_index(0, &info).unwrap().0
                ]);
                let query = Query::parse("windows", MatchMode::Prefix, Scope::Name);
                let mut s = Searcher::new();
                b.iter(|| {
                    s.reset();
                    std::hint::black_box(s.search(&index, &query).total)
                })
            });
            g.finish();
        }
        Err(e) => eprintln!("SKIP MFT benchmark: {e}"),
    }
}

#[cfg(not(windows))]
fn mft_read(_c: &mut Criterion) {}

criterion_group!(benches, sort_into_index, directory_walk, mft_read);
criterion_main!(benches);
