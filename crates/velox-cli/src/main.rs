//! `velox` — command-line harness for the engine.
//!
//! Exists so the engine can be exercised, timed, and compared without any UI in
//! the way. Every subcommand prints timings, because the whole project is a
//! performance claim and claims need numbers.
//!
//! ```text
//! velox-cli index                     build an index and report how long it took
//! velox-cli search <query>            build, then run one query
//! velox-cli repl                      build once, then time queries interactively
//! velox-cli bench                     query latency vs corpus size and query length
//! velox-cli compare <dir>             MFT read vs naive FindFirstFile walk
//! velox-cli watch                     live USN journal monitoring
//! ```

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use velox_core::index::synthetic_volume;
use velox_core::query::{MatchMode, Plan, Query, Scope, Searcher, SortKey};
use velox_core::{format_filetime, format_size, Index};

#[cfg(windows)]
use velox_core::ntfs;

const USAGE: &str = "\
Velox — instant file search engine

USAGE:
    velox-cli <COMMAND> [OPTIONS]

COMMANDS:
    index                 Build the index over all local volumes and report timings
    search <QUERY>        Build the index, run one query, print results
    repl                  Build the index once, then time queries interactively
    bench [--quick]       Query latency across corpus sizes and query lengths
    compare <DIR>         Time the MFT read against a naive FindFirstFile walk
    watch [--secs N]      Build the index, then apply live USN journal updates

OPTIONS (search / repl):
    --limit <N>           Max rows to print (default 20)
    --substring           Match anywhere in the name instead of prefix-only
    --path                Match against the full path instead of the filename
    --sort <KEY>          name | path | size | date  (suffix -desc to reverse)
    --folder <DIR>        Index only this folder (repeatable); implies no MFT read

QUERY SYNTAX:
    foo bar               implicit AND
    foo | bar             OR
    !foo                  NOT
    *.rs   setup??.exe    wildcards
    ext:rs;md  size:>1mb  dm:>2024-01-01  file:  folder:
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{USAGE}");
        return;
    }
    let opts = Options::parse(&args[1..]);
    match args[0].as_str() {
        "index" => cmd_index(&opts),
        "search" => cmd_search(&opts),
        "repl" => cmd_repl(&opts),
        "bench" => cmd_bench(&opts),
        "compare" => cmd_compare(&opts),
        "watch" => cmd_watch(&opts),
        other => {
            eprintln!("unknown command `{other}`\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    }
}

#[derive(Default)]
struct Options {
    positional: Vec<String>,
    limit: usize,
    substring: bool,
    path_scope: bool,
    sort: Option<String>,
    folders: Vec<PathBuf>,
    quick: bool,
    secs: u64,
}

impl Options {
    fn parse(args: &[String]) -> Options {
        let mut o = Options { limit: 20, secs: 10, ..Default::default() };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--limit" => {
                    o.limit = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20);
                    i += 1;
                }
                "--substring" => o.substring = true,
                "--path" => o.path_scope = true,
                "--quick" => o.quick = true,
                "--sort" => {
                    o.sort = args.get(i + 1).cloned();
                    i += 1;
                }
                "--secs" => {
                    o.secs = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(10);
                    i += 1;
                }
                "--folder" => {
                    if let Some(f) = args.get(i + 1) {
                        o.folders.push(PathBuf::from(f));
                    }
                    i += 1;
                }
                other => o.positional.push(other.to_string()),
            }
            i += 1;
        }
        o
    }

    fn mode(&self) -> MatchMode {
        if self.substring {
            MatchMode::Substring
        } else {
            MatchMode::Prefix
        }
    }
    fn scope(&self) -> Scope {
        if self.path_scope {
            Scope::Path
        } else {
            Scope::Name
        }
    }
    fn sort_key(&self) -> Option<(SortKey, bool)> {
        let s = self.sort.as_deref()?;
        let (name, desc) = match s.strip_suffix("-desc") {
            Some(n) => (n, true),
            None => (s, false),
        };
        let key = match name {
            "name" => SortKey::Name,
            "path" => SortKey::Path,
            "size" => SortKey::Size,
            "date" | "modified" => SortKey::Modified,
            _ => return None,
        };
        Some((key, desc))
    }
}

// ------------------------------------------------------------------ indexing

#[cfg(windows)]
fn build(opts: &Options) -> Index {
    let policy = ntfs::FallbackPolicy { roots: opts.folders.clone(), skip: false };
    let elevated = ntfs::is_elevated();
    if !elevated {
        eprintln!(
            "note: not running elevated — the raw MFT read is unavailable, \
             falling back to a FindFirstFile walk.\n      \
             Re-run from an administrator shell for the fast path."
        );
    }
    let (index, report) = ntfs::build_index(&policy);
    print_report(&report);
    index
}

#[cfg(not(windows))]
fn build(opts: &Options) -> Index {
    let roots = if opts.folders.is_empty() {
        vec![std::env::current_dir().unwrap()]
    } else {
        opts.folders.clone()
    };
    let (vol, stats) = velox_core::walk::index_folders(0, &roots, "ROOT");
    println!("walked {} entries in {:?}", stats.total(), stats.elapsed);
    Index::new(vec![vol])
}

#[cfg(windows)]
fn print_report(report: &ntfs::BuildReport) {
    println!("Index built in {:?}  ({} elevated)", report.total, if report.elevated { "" } else { "not" });
    println!("{:<6} {:<8} {:>12} {:>10}  NOTE", "VOLUME", "SOURCE", "ENTRIES", "TIME");
    for v in &report.volumes {
        println!(
            "{:<6} {:<8} {:>12} {:>10?}  {}",
            v.label,
            match v.source {
                velox_core::SourceKind::Mft => "MFT",
                velox_core::SourceKind::Walk => "walk",
            },
            thousands(v.entries),
            v.elapsed,
            v.note.as_deref().unwrap_or("")
        );
    }
    let entries = report.total_entries();
    println!(
        "\nTotal: {} entries, {} resident ({:.1} bytes/entry), {:.0} entries/sec",
        thousands(entries),
        format_size(report.memory_bytes as u64),
        if entries > 0 { report.memory_bytes as f64 / entries as f64 } else { 0.0 },
        entries as f64 / report.total.as_secs_f64().max(1e-9),
    );
}

fn cmd_index(opts: &Options) {
    let _ = build(opts);
}

// ------------------------------------------------------------------ querying

fn cmd_search(opts: &Options) {
    let Some(q) = opts.positional.first() else {
        eprintln!("search needs a query\n");
        print!("{USAGE}");
        std::process::exit(2);
    };
    let index = build(opts);
    let mut searcher = Searcher::new();
    let query = Query::parse(q, opts.mode(), opts.scope());

    // Run twice: the second run is the honest steady-state number, since the
    // first also pays for whatever was still cold.
    let _warmup = searcher.search(&index, &query);
    searcher.reset();
    let mut res = searcher.search(&index, &query);

    if let Some((key, desc)) = opts.sort_key() {
        let t = Instant::now();
        res.sort_by(&index, key, desc);
        println!("(re-sorted {} results in {:?} without re-querying)", thousands(res.total), t.elapsed());
    }

    println!(
        "\n{} matches in {:?} [{}]\n",
        thousands(res.total),
        res.took,
        match res.plan {
            Plan::BinarySearch => "binary search",
            Plan::Scan => "full scan",
            Plan::Narrowed => "narrowed",
        }
    );
    print_rows(&index, &res, opts.limit);
}

fn print_rows(index: &Index, res: &velox_core::SearchResults, limit: usize) {
    let rows = res.window(index, 0, limit);
    println!("{:>12}  {:<19}  PATH", "SIZE", "MODIFIED");
    for r in &rows {
        let Some(info) = index.describe(*r) else { continue };
        println!(
            "{:>12}  {:<19}  {}",
            if info.is_dir { "<DIR>".to_string() } else { format_size(info.size) },
            format_filetime(info.mtime),
            info.full_path()
        );
    }
    if res.total > rows.len() {
        println!("... and {} more", thousands(res.total - rows.len()));
    }
}

/// Interactive as-you-type simulation. Builds once, then times each query
/// against the live index — the closest thing to what the GUI will do.
fn cmd_repl(opts: &Options) {
    let index = build(opts);
    let mut searcher = Searcher::new();
    println!("\nType a query (empty line or Ctrl-C to quit). Timings are per query.\n");
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        // Simulate typing it one character at a time, which is what actually
        // exercises the incremental narrowing path.
        let mut total = Duration::ZERO;
        let mut last = None;
        for end in line.char_indices().map(|(i, c)| i + c.len_utf8()) {
            let q = Query::parse(&line[..end], opts.mode(), opts.scope());
            let res = searcher.search(&index, &q);
            total += res.took;
            println!(
                "  {:>3} chars  {:>10}  {:>12?}  {}",
                end,
                thousands(res.total),
                res.took,
                match res.plan {
                    Plan::BinarySearch => "binary search",
                    Plan::Scan => "full scan",
                    Plan::Narrowed => "narrowed",
                }
            );
            last = Some(res);
        }
        println!("  total for the whole typed query: {total:?}\n");
        if let Some(res) = last {
            print_rows(&index, &res, opts.limit);
        }
        println!();
    }
}

// --------------------------------------------------------------- benchmarking

fn cmd_bench(opts: &Options) {
    let sizes: &[usize] =
        if opts.quick { &[100_000, 1_000_000] } else { &[100_000, 1_000_000, 5_000_000] };

    println!("Building synthetic corpora and timing queries.");
    println!("Prefix queries take the binary-search plan; substring queries scan.\n");

    for &n in sizes {
        let t = Instant::now();
        let vol = synthetic_volume(0, "C:", n);
        let build_time = t.elapsed();
        let mem = vol.memory_bytes();
        let index = Index::new(vec![vol]);
        println!(
            "== {} entries ==  built in {:?}, {} resident ({:.1} bytes/entry)",
            thousands(n),
            build_time,
            format_size(mem as u64),
            mem as f64 / n as f64
        );

        println!("{:<10} {:<14} {:>12} {:>12} {:>10}", "QUERY", "PLAN", "MATCHES", "MEDIAN", "P99");
        for q in ["r", "re", "rep", "repo", "report", "report_a"] {
            let mut s = Searcher::new();
            let query = Query::parse(q, MatchMode::Prefix, Scope::Name);
            let (med, p99, total, plan) = time_query(&index, &mut s, &query, 200);
            println!(
                "{:<10} {:<14} {:>12} {:>12} {:>10}",
                q,
                plan_name(plan),
                thousands(total),
                fmt_dur(med),
                fmt_dur(p99)
            );
        }

        // Substring: the scan path, and the case incremental narrowing exists for.
        println!("\n  substring mode (full scan, then narrowing as characters arrive):");
        let mut s = Searcher::new();
        let mut narrowed_total = Duration::ZERO;
        for q in ["r", "re", "rep", "repo", "report"] {
            let query = Query::parse(q, MatchMode::Substring, Scope::Name);
            let t = Instant::now();
            let res = s.search(&index, &query);
            let took = t.elapsed();
            narrowed_total += took;
            println!(
                "  {:<10} {:<14} {:>12} {:>12}",
                q,
                plan_name(res.plan),
                thousands(res.total),
                fmt_dur(took)
            );
        }
        // Same sequence with the cache dropped each time: what it costs without
        // narrowing.
        let mut cold_total = Duration::ZERO;
        for q in ["r", "re", "rep", "repo", "report"] {
            let mut s = Searcher::new();
            let query = Query::parse(q, MatchMode::Substring, Scope::Name);
            let t = Instant::now();
            let _ = s.search(&index, &query);
            cold_total += t.elapsed();
        }
        println!(
            "  typing \"report\": {} with narrowing vs {} without ({:.1}x)",
            fmt_dur(narrowed_total),
            fmt_dur(cold_total),
            cold_total.as_secs_f64() / narrowed_total.as_secs_f64().max(1e-12)
        );
        println!();
    }
}

fn plan_name(p: Plan) -> &'static str {
    match p {
        Plan::BinarySearch => "binary search",
        Plan::Scan => "full scan",
        Plan::Narrowed => "narrowed",
    }
}

fn time_query(
    index: &Index,
    searcher: &mut Searcher,
    query: &Query,
    iters: usize,
) -> (Duration, Duration, usize, Plan) {
    let mut samples = Vec::with_capacity(iters);
    let mut total = 0usize;
    let mut plan = Plan::Scan;
    for _ in 0..iters {
        // Reset so every iteration measures the same plan rather than drifting
        // into the narrowing path.
        searcher.reset();
        let t = Instant::now();
        let res = searcher.search(index, query);
        samples.push(t.elapsed());
        total = res.total;
        plan = res.plan;
    }
    samples.sort();
    (samples[samples.len() / 2], samples[samples.len() * 99 / 100], total, plan)
}

/// The comparison the whole architecture rests on: reading the MFT versus
/// walking directories with `FindFirstFile`.
fn cmd_compare(opts: &Options) {
    let dir = opts
        .positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));

    println!("Naive recursive FindFirstFile walk of {}\n", dir.display());
    let (store, serial) = velox_core::walk::walk_serial(std::slice::from_ref(&dir), "WALK");
    println!(
        "  serial walk   {:>12} entries  {:>10?}  {:>12.0} entries/sec  ({} unreadable)",
        thousands(serial.total()),
        serial.elapsed,
        serial.rate(),
        serial.errors
    );
    drop(store);

    let (_, par) = velox_core::walk::walk_parallel(std::slice::from_ref(&dir), "WALK");
    println!(
        "  parallel walk {:>12} entries  {:>10?}  {:>12.0} entries/sec",
        thousands(par.total()),
        par.elapsed,
        par.rate()
    );

    #[cfg(windows)]
    {
        if !ntfs::is_elevated() {
            println!(
                "\n  MFT read      SKIPPED — needs administrator privileges.\n\
                 \n  Re-run this from an elevated shell to get the comparison."
            );
            return;
        }
        let letter = dir
            .to_string_lossy()
            .chars()
            .next()
            .unwrap_or('C')
            .to_ascii_uppercase();
        let Some(info) =
            ntfs::enumerate_volumes().into_iter().find(|v| v.letter == letter && v.is_ntfs())
        else {
            println!("\n  MFT read      SKIPPED — {letter}: is not NTFS");
            return;
        };
        println!("\nRaw MFT read of {}: (the whole volume, not just that folder)\n", letter);
        match ntfs::mft::build_volume_index(0, &info) {
            Ok((idx, stats)) => {
                println!(
                    "  MFT read      {:>12} entries  {:>10?}  {:>12.0} entries/sec",
                    thousands(idx.len()),
                    stats.total,
                    stats.records_per_sec()
                );
                println!(
                    "                scanned {} records, read {}, {} orphans re-homed",
                    thousands(stats.records_scanned),
                    format_size(stats.bytes_read),
                    stats.orphans
                );
                println!(
                    "                {} resident = {:.1} bytes/entry over {} slots                      ({:.0}% holes), {} of names",
                    format_size(stats.memory_bytes as u64),
                    stats.bytes_per_entry(),
                    thousands(stats.slots),
                    stats.hole_ratio() * 100.0,
                    format_size(stats.name_bytes as u64),
                );
                // Where the wall clock actually goes. If read+parse dominates and
                // the throughput is well under the disk's sequential rate, the
                // bottleneck is the single-threaded fold-in, not the device.
                let mb = stats.bytes_read as f64 / (1 << 20) as f64;
                println!(
                    "                phases: read+parse {:?} ({:.0} MB/s overall), sort {:?}",
                    stats.read_and_parse,
                    mb / stats.read_and_parse.as_secs_f64().max(1e-9),
                    stats.sort,
                );
                println!(
                    "                  of which: io {:?} ({:.0} MB/s in ReadFile),                      parse {:?}, fold {:?}",
                    stats.io,
                    mb / stats.io.as_secs_f64().max(1e-9),
                    stats.parse,
                    stats.fold,
                );
                let speedup = serial.rate().max(1e-9);
                println!(
                    "\n  MFT indexes {:.0}x more entries per second than the naive walk.",
                    stats.records_per_sec() / speedup
                );
            }
            Err(e) => println!("  MFT read failed: {e}"),
        }
    }
}

// ------------------------------------------------------------- live updates

#[cfg(windows)]
fn cmd_watch(opts: &Options) {
    use velox_core::ntfs::usn;

    let index = build(opts);
    if !ntfs::is_elevated() {
        eprintln!("\nwatch needs administrator privileges to read the USN journal.");
        std::process::exit(1);
    }
    let vols: Vec<_> = ntfs::enumerate_volumes().into_iter().filter(|v| v.is_ntfs()).collect();
    let labels = index.volume_labels();

    let mut monitors = Vec::new();
    for (i, _) in labels.iter().enumerate() {
        if let Some(info) = vols.get(i) {
            match usn::UsnMonitor::new(info) {
                Ok(m) => monitors.push((i, m)),
                Err(e) => eprintln!("  {}: cannot monitor ({e})", info.label),
            }
        }
    }
    if monitors.is_empty() {
        eprintln!("no volumes could be monitored");
        std::process::exit(1);
    }
    println!(
        "\nWatching {} volume(s) for {} seconds. Create or rename a file to see it appear.\n",
        monitors.len(),
        opts.secs
    );

    let deadline = Instant::now() + Duration::from_secs(opts.secs);
    let mut applied = 0usize;
    while Instant::now() < deadline {
        for (vi, mon) in monitors.iter_mut() {
            let events = match mon.poll() {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("  journal error: {e}");
                    continue;
                }
            };
            if events.is_empty() {
                continue;
            }
            let mut vol = index.volumes[*vi].write().unwrap();
            let t = Instant::now();
            let stats = usn::apply_events(&mut vol, &events, None);
            let took = t.elapsed();
            applied += events.len();
            println!(
                "  {} {:>4} events  (+{} ~{} -{})  applied in {}",
                vol.store.label,
                events.len(),
                stats.created,
                stats.updated,
                stats.deleted,
                fmt_dur(took)
            );
            for ev in events.iter().take(3) {
                println!("      {} {}", if ev.is_delete() { "-" } else { "+" }, ev.name);
            }
            if vol.should_merge() {
                // Plan outside the lock in the real app; here it is inline.
                let t = Instant::now();
                vol.merge_now();
                println!("      merged delta into the sorted index in {}", fmt_dur(t.elapsed()));
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("\napplied {} journal records", thousands(applied));
}

#[cfg(not(windows))]
fn cmd_watch(_opts: &Options) {
    eprintln!("watch is Windows-only");
    std::process::exit(1);
}

// ---------------------------------------------------------------- formatting

fn thousands(n: usize) -> String {
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

fn fmt_dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.2} us", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2} s", ns as f64 / 1_000_000_000.0)
    }
}
