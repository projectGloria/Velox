# Velox

Instant file-name search for Windows. Reads the NTFS Master File Table once,
holds every file and folder name in memory in a structure built for prefix
lookup, and keeps that index live from the USN Change Journal instead of ever
rescanning the disk.

Typing is answered in **microseconds**, not milliseconds.

```
crates/
  velox-core/   engine: MFT reader, USN journal, index, query engine  (library)
  velox-cli/    velox-cli.exe — build, query, benchmark, compare      (test harness)
  velox-gui/    Velox.exe — Win32 + Direct2D virtualized result list  (app)
```

The CLI binary is `velox-cli.exe`, not `velox.exe`: Windows filenames are
case-insensitive, so `velox.exe` and the GUI's `Velox.exe` would collide in the
same output directory.

## Build and run

```bash
cargo build --release
```

```bash
cargo run --release -p velox-cli -- bench
```

```bash
cargo run --release -p velox-gui
```

## Measured results

Query latency, synthetic corpora, release build, one volume:

| Corpus | Prefix query (median) | P99 | Resident |
|---|---|---|---|
| 100,000 | 400 ns | 500 ns | 5.5 MB |
| 1,000,000 | 500 ns | 700 ns | 52.6 MB |
| 5,000,000 | 600 ns | 800 ns | 275.2 MB |

**Prefix latency is flat against corpus size** — a 50x larger corpus costs 1.5x
more per query, which is the `log2(n)` term and nothing else. That is the whole
argument for the sorted array: the answer to a prefix query is a *contiguous
range*, so the match count is O(1) and no result is ever materialised.

Memory on synthetic corpora lands at **57.7 bytes per entry**, including the
packed name blob and the sorted index. **Real volumes cost more** — see
"Memory on a real volume" below.

Substring and wildcard queries cannot use the binary search and must scan. This
is where incremental narrowing earns its place — typing `report` one character
at a time over 1M entries:

| | Total |
|---|---|
| With narrowing | 54 ms |
| Without (re-scan each keystroke) | 102 ms |

At 5M entries the same comparison is 470 ms vs 969 ms. Prefix mode is the
default precisely because it avoids this path entirely.

### On a real volume

Measured on a live machine, elevated, two NTFS volumes:

| | Entries | Time |
|---|---|---|
| `C:` raw MFT read | 1,158,111 | 5.44 s |
| `D:` raw MFT read | 19,731 | 0.66 s |
| **Total** | **1,177,842** | **216,503 entries/sec** |

Live USN journal batches (2–55 records) apply in **1.6–12 µs** each — the cost
of staying current is proportional to what changed, not to the 1.18M entries
already indexed.

### Memory on a real volume

Measured on `C:` — 1,158,191 entries across 1,378,069 slots, **99.9 MB resident,
90.4 bytes/entry**:

| | Bytes | Share |
|---|---|---|
| Packed name blob | 53.8 MB | **54%** |
| Fixed per-slot arrays (29 B x slots) | 40.0 MB | 40% |
| Sorted order (4 B x entries) | 4.6 MB | 5% |

The first run of this reported 136.7 B/entry. The gap was reserved-but-unused
capacity: the store is sized from the MFT's *valid length* and the name blob
from an estimate, both overshoot, and `memory_bytes()` counts `capacity()`.
`EntryStore::shrink_to_fit()` now runs at the end of the build.

What remains is mostly **names, not overhead**. Real filenames on a development
machine average ~46 bytes — GUID temp files, `node_modules`, Go module cache
escapes — roughly double what the synthetic corpus assumes, which is why the
synthetic 57.7 B/entry does not transfer.

Holes are a smaller cost than expected: 16% of slots, **6.4 MB, 6% of the
total**. They are inherent — `EntryId` *is* the MFT record number, which is what
makes the parallel build lock-free and turns a USN update into one array
subscript instead of a hash lookup — but they are not what to attack first.

Measured on this machine's real filenames, 46% of names are duplicates but only
**19.2% of name bytes** are, because the repeated names are the short ones. So
hash-consing the blob is worth ~10 MB, not the ~27 MB the duplicate count
suggests.

Directory walking, `C:\Windows\System32`, 19,754 entries:

| | Time | Rate |
|---|---|---|
| Naive serial `FindFirstFile` | 487 ms | 40,548 entries/sec |
| Parallel `FindFirstFile` | 41 ms | 486,509 entries/sec |

Against the serial baseline the MFT path is **~5.3x** on raw entries/sec. Treat
the parallel walk's 486k/sec with suspicion: it is a small, hot-cache subtree,
and it does not extrapolate to a whole volume, where the walk pays a syscall per
directory against cold metadata scattered across the disk. Run `velox-cli
compare` on your own machine for a head-to-head on identical data.

## The MFT comparison needs an elevated shell

Opening `\\.\C:` for raw reads requires administrator privileges, so the
headline MFT-vs-walk number cannot be produced from an ordinary prompt. From an
**administrator** terminal:

```bash
cargo run --release -p velox-cli -- compare "C:\Windows\System32"
```

That prints the whole-volume MFT read alongside both walks, plus the ratio. The
same applies to `velox-cli index`, `velox-cli watch`, and the `build/mft`
criterion benchmark — each detects the missing privilege and says it skipped,
rather than failing.

Velox never *requires* elevation. Unelevated it degrades to a `FindFirstFile`
walk of the user's profile and says so in the status bar. See
[docs/elevation.md](docs/elevation.md).

## CLI

```
velox-cli index                 build the index over all local volumes, report timings
velox-cli search <QUERY>        build, run one query, print results
velox-cli repl                  build once, then time queries as you type them
velox-cli bench [--quick]       query latency across corpus sizes and query lengths
velox-cli compare <DIR>         MFT read vs naive FindFirstFile walk
velox-cli watch [--secs N]      live USN journal updates applied to the index
```

`repl` is the useful one for feeling the difference: it replays a typed query
one character at a time and prints the plan and latency per keystroke.

## Query syntax

| | |
|---|---|
| `foo bar` | implicit AND |
| `foo \| bar` | OR |
| `!foo` | NOT |
| `*.rs`, `setup??.exe` | wildcards |
| `"two words"` | quoted phrase |
| `ext:rs;md` | extension filter |
| `size:>1mb`, `size:<10kb` | size range |
| `dm:>2024-01-01` | modified-date range |
| `file:` / `folder:` | kind filter |
| `sort:size-desc` | sort without re-querying |

In the GUI: **F2** toggles prefix/substring, **F3** toggles name/path scope,
**F5** rebuilds, **Enter** reveals the selection in Explorer.

## Benchmarks

```bash
cargo bench -p velox-core
```

`benches/query.rs` covers prefix latency against corpus size and query length,
the substring scan, narrowing vs re-scanning, result windowing, and re-sorting.
`benches/build.rs` covers index construction and the `FindFirstFile` baselines,
and reports the MFT read when run elevated.

## Tests

```bash
cargo test --workspace
```

91 tests. The MFT record parser and the USN decoder are tested against
synthetic byte-level records, so the on-disk format logic is covered without a
disk, without NTFS, and without elevation — including truncated records, torn
sectors (fixup mismatches), zero-length attributes that would otherwise loop
forever, DOS-vs-Win32 name selection, and signed data-run offsets. Tests that
genuinely need a raw volume handle detect the missing privilege and skip with a
printed reason.

## Scope

v1 does file and folder **names**, not contents. NTFS volumes get the fast path;
anything else falls back to a directory walk with no instant-search guarantee.
No network drives. No telemetry, no network calls of any kind.

Regex mode (item 6 of the original query-feature list, marked a stretch goal) is
not implemented — it cannot use the prefix path and would need its own scan
plan. Everything else in that list is in.

See [docs/architecture.md](docs/architecture.md) for why the index is shaped the
way it is.
