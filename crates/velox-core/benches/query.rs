//! Query latency benchmarks.
//!
//! The claim under test: a filename-prefix query is sub-millisecond and
//! *independent of corpus size*, because it resolves to two binary searches over
//! the sorted order rather than any per-entry work.
//!
//! Run with `cargo bench -p velox-core --bench query`.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use velox_core::index::synthetic_volume;
use velox_core::query::{MatchMode, Query, Scope, Searcher, SortKey};
use velox_core::Index;

const SIZES: [usize; 3] = [100_000, 1_000_000, 5_000_000];

fn corpus(n: usize) -> Index {
    Index::new(vec![synthetic_volume(0, "C:", n)])
}

/// Prefix latency against corpus size, at a fixed query length. Should be flat.
fn prefix_vs_corpus_size(c: &mut Criterion) {
    let mut g = c.benchmark_group("prefix/corpus_size");
    g.sample_size(200);
    for n in SIZES {
        let index = corpus(n);
        let query = Query::parse("report", MatchMode::Prefix, Scope::Name);
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            let mut s = Searcher::new();
            b.iter(|| {
                s.reset();
                std::hint::black_box(s.search(&index, &query).total)
            })
        });
    }
    g.finish();
}

/// Prefix latency against query length at a fixed corpus size. Longer queries do
/// slightly more work per comparison but narrow the range, so this should also be
/// close to flat.
fn prefix_vs_query_length(c: &mut Criterion) {
    let index = corpus(1_000_000);
    let mut g = c.benchmark_group("prefix/query_length");
    g.sample_size(200);
    for q in ["r", "re", "rep", "repo", "repor", "report", "report_a", "report_alpha"] {
        let query = Query::parse(q, MatchMode::Prefix, Scope::Name);
        g.bench_with_input(BenchmarkId::from_parameter(q.len()), &q, |b, _| {
            let mut s = Searcher::new();
            b.iter(|| {
                s.reset();
                std::hint::black_box(s.search(&index, &query).total)
            })
        });
    }
    g.finish();
}

/// The scan path, which is what substring and wildcard queries must use.
fn substring_scan(c: &mut Criterion) {
    let mut g = c.benchmark_group("substring/scan");
    g.sample_size(20);
    g.measurement_time(Duration::from_secs(12));
    for n in SIZES {
        let index = corpus(n);
        let query = Query::parse("report", MatchMode::Substring, Scope::Name);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            let mut s = Searcher::new();
            b.iter(|| {
                s.reset();
                std::hint::black_box(s.search(&index, &query).total)
            })
        });
    }
    g.finish();
}

/// Typing a query one character at a time, with and without incremental
/// narrowing. This is the comparison that justifies keeping the previous result
/// set around.
fn incremental_narrowing(c: &mut Criterion) {
    let index = corpus(1_000_000);
    let steps: Vec<Query> = ["r", "re", "rep", "repo", "repor", "report"]
        .iter()
        .map(|q| Query::parse(q, MatchMode::Substring, Scope::Name))
        .collect();

    let mut g = c.benchmark_group("substring/typing_report");
    g.sample_size(20);
    g.measurement_time(Duration::from_secs(15));

    g.bench_function("with_narrowing", |b| {
        b.iter(|| {
            let mut s = Searcher::new();
            let mut total = 0;
            for q in &steps {
                total += s.search(&index, q).total;
            }
            std::hint::black_box(total)
        })
    });
    g.bench_function("without_narrowing", |b| {
        b.iter(|| {
            let mut total = 0;
            for q in &steps {
                // A fresh searcher each keystroke: no previous result set to
                // narrow, so every step rescans the whole corpus.
                let mut s = Searcher::new();
                total += s.search(&index, q).total;
            }
            std::hint::black_box(total)
        })
    });
    g.finish();
}

/// Fetching the rows a virtualized list would actually draw. Must not depend on
/// how large the result set is.
fn result_windowing(c: &mut Criterion) {
    let index = corpus(1_000_000);
    let mut s = Searcher::new();
    let query = Query::parse("r", MatchMode::Prefix, Scope::Name);
    let res = s.search(&index, &query);

    let mut g = c.benchmark_group("window");
    g.bench_function("first_50_rows", |b| {
        b.iter(|| std::hint::black_box(res.window(&index, 0, 50).len()))
    });
    g.bench_function("50_rows_at_offset_40k", |b| {
        b.iter(|| std::hint::black_box(res.window(&index, 40_000, 50).len()))
    });
    g.finish();
}

/// Re-sorting an existing result set, which must never re-run the query.
fn resort_without_requery(c: &mut Criterion) {
    let index = corpus(1_000_000);
    let mut s = Searcher::new();
    let query = Query::parse("report", MatchMode::Prefix, Scope::Name);
    let res = s.search(&index, &query);

    let mut g = c.benchmark_group("sort");
    g.sample_size(20);
    g.bench_function("by_size_desc", |b| {
        b.iter(|| {
            let mut r = res.clone();
            r.sort_by(&index, SortKey::Size, true);
            std::hint::black_box(r.total)
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    prefix_vs_corpus_size,
    prefix_vs_query_length,
    substring_scan,
    incremental_narrowing,
    result_windowing,
    resort_without_requery
);
criterion_main!(benches);
