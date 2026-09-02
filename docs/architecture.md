# Architecture

## The shape of it

```
NTFS volume ──raw MFT read──┐
                            ├──> EntryStore  (struct-of-arrays, packed names)
non-NTFS ───FindFirstFile───┘         │
                                      v
                            VolumeIndex  (sorted order + delta overlay)
                                      │
USN journal ──live updates───────────>│
                                      v
                               Searcher  (prefix binary search,
                                          incremental narrowing)
```

Everything above the `Searcher` is `velox-core`. The CLI, the benchmarks, and
the GUI are all just consumers of it; nothing in the engine draws anything or
owns a message loop.

## Why not walk directories

`FindFirstFile`/`FindNextFile` costs one syscall per directory and touches
metadata scattered across the volume. The MFT is a flat record per file, read
sequentially. Measured on this machine, `C:\Windows\System32` (19,754 entries)
takes 487 ms serially and 41 ms across all cores — and that is one directory
subtree, not a volume.

## Entry storage

One `EntryStore` per volume, struct-of-arrays. Nothing is heap-allocated per
file; names are packed end-to-end into a single byte blob addressed by
`(offset, len)`.

| Array | Per slot | Synthetic | Real `C:` |
|---|---|---|---|
| `name_off: u32`, `name_len: u16` | 6 | 6 | 7 |
| `parent: u32` | 4 | 4 | 5 |
| `size: u64`, `mtime: i64` | 16 | 16 | 19 |
| `seq: u16`, `flag: u8` | 3 | 3 | 4 |
| name blob (UTF-8, variable) | - | ~22 | **46** |
| `order: Vec<u32>` sorted by folded name | - | 4 | 4 |
| **Total bytes per indexed entry** | | **57.7** | **90.4** |

The two columns differ for two reasons, and it is worth being precise about
which is which. Real filenames on a development machine average ~46 bytes --
GUID temp files, `node_modules`, Go module cache escapes -- roughly double what
the synthetic corpus generates, and the name blob is 54% of the real total. The
per-slot arrays are also charged against *slots*, not entries, and a real MFT
carries ~16% unused records; that accounts for the inflation in the fixed
columns, about 6% of the total. Holes are the smaller effect by far.

**`EntryId` is the MFT record number.** MFT record numbers are dense, so using
them directly as the array index buys two things: the parallel build writes
results straight into a preallocated array with no merge step and no
synchronisation, and a USN journal record becomes an index update with a single
array subscript instead of a hash lookup. The cost is the small number of unused
MFT records left as holes — cheaper than the ~24 MB hash map it replaces.

**Paths are never stored.** They are reconstructed on demand by walking the
`parent` chain, and only for the handful of rows actually on screen.

## Why a sorted array rather than a radix tree

A filename-prefix query resolves to two binary searches — lower and upper bound
— after which the answer is a **contiguous range** of `order`. So:

- the match count is O(1) (`hi - lo`),
- nothing is materialised; the caller walks the range,
- cost is ~`2·log2(n)` folded string comparisons, ~46 at 5M entries.

Measured: 600 ns median at 5M entries, versus 400 ns at 100k. Flat, as predicted.

A radix tree gives O(k) descent but then still has to walk a subtree to
enumerate matches, and costs pointer-chasing plus per-node allocation across
millions of entries. Its one real advantage is cheap insertion — which is
handled separately, below.

## Case folding without a second copy

Ordering and matching are defined over the *folded* form of each name, but the
folded form is never stored: comparison folds on the fly through a 256-byte
ASCII table (branchless, one lookup per byte). Names containing non-ASCII carry
a flag bit and take a Unicode simple-fold slow path. At 5M entries this saves
roughly 110 MB against keeping a parallel folded-key blob, and costs nothing
measurable — a query is ~460 folded byte comparisons in total.

## Live updates: the delta overlay

Inserting into a sorted 20 MB array means a memmove per event. Instead:

- **Writes** append to the store's arrays and push one id onto an unsorted
  `delta` vector. O(1).
- **Queries** binary-search `order` *and* linearly scan `delta` (capped at 4096
  entries, so the scan is far cheaper than the search it supplements).
- **Deletes** clear the entry's `IN_USE` flag; the id stays in `order` as a
  tombstone and iteration skips it.
- **Merging** rebuilds `order` — but the replacement `Vec<u32>` is planned
  entirely *outside* the write lock, which is then taken only long enough to
  swap the pointer. Write critical sections are microseconds, never the
  milliseconds a merge takes.

An entry that is overwritten (renamed, or metadata changed) is in a subtle spot:
it is still live and still sitting at its now-wrong position in `order`, while a
corrected copy sits in `delta`. A `STAGED` flag bit marks it so the sorted order
skips it and the overlay serves it, which is what keeps a rename from returning
the entry twice. `commit_merge` clears those bits over the (bounded) overlay,
never over the corpus.

## Incremental narrowing

Substring and wildcard queries cannot use the binary search and must scan. But
when the user appends a character, the new result set is a **subset** of the old
one, so the scan runs over the previous results instead of the whole corpus.
Typing therefore gets cheaper per keystroke rather than more expensive.

`Query::refines` decides when this is sound. It requires the raw query text to
have grown by appending, no filter to have been relaxed, and no wildcard on
either side — because appending to a wildcard can *widen* a match (`a` → `a*`),
which would silently drop results. Shortening the query always re-scans.

Two bugs the benchmark harness caught here, both worth stating because both made
narrowing *slower* than a fresh scan:

- `Query::matches` allocated two `Vec<bool>`s per candidate for OR-group
  tracking. Replaced with a pair of `u64` bitmasks and a single-term fast path.
- The narrowing loop took an `RwLock` read guard **per result**. Now it walks
  runs of same-volume results and locks once per run, then parallelises inside.

## Reader/writer separation

One `RwLock<VolumeIndex>` per volume, so a journal update on `D:` never blocks a
query against `C:`. Full rebuilds publish a whole new `Index` behind an `Arc`;
incremental updates take the per-volume lock for the O(1) work described above.

## Snapshots

The on-disk format is a flat dump of the struct-of-arrays, because that is
already the in-memory layout — loading is a read into pre-sized vectors, not a
parse. Writes go to a sibling file and are renamed into place, so an interrupted
save never leaves a half-written snapshot to be loaded next launch.

Freshness is decided by the USN journal id, not by a timestamp. A different id
means the journal was deleted and recreated (or the volume reformatted), so
every USN in the snapshot is meaningless and the volume is rebuilt from the MFT.
If the journal id matches, the saved `next_usn` is where the catch-up read
starts.

Everything a snapshot could get wrong is checked on load rather than trusted —
array lengths must agree, name offsets must lie inside the name blob, the order
array cannot be longer than the slot array, and an implausible volume count is
rejected before it can trigger a huge speculative allocation. A corrupt file
produces an error, never an out-of-bounds read during a later query.

## Parsing untrusted on-disk structures

The MFT record parser and the USN decoder are the two places reading bytes that
a corrupt volume controls. Both are pure functions over byte slices with no
Win32 dependency, which is what makes them testable against synthetic records —
no disk, no NTFS, no elevation. The guards that matter:

- **Fixups.** NTFS overwrites the last two bytes of every sector with an update
  sequence number. Without undoing that, two bytes in every 512 are garbage,
  which silently corrupts any name straddling a sector boundary. A mismatch is
  reported, not ignored — it means a torn write.
- **Attribute walks** bail on a zero or oversized attribute length. A zero-length
  attribute would otherwise loop forever.
- **Data runs** sign-extend the LCN delta from the run's own byte width; the MFT
  can and does fragment backwards on the volume.
- **Name namespaces.** NTFS emits both a Win32 and a DOS 8.3 name for many
  files. Indexing `PROGRA~1` instead of `Program Files` would be a visible bug,
  so Win32 wins regardless of attribute order.

## Indexing throughput: what the measurements said

Instrumenting the MFT read (`io` / `parse` / `fold`, printed by `velox-cli
compare`) located the cost precisely on a 16-core machine with a Crucial P3 Plus
NVMe:

| Phase | Time | Share |
|---|---|---|
| `io`, inside `ReadFile` | 3.96 s | 93% |
| `parse`, rayon across cores | 298 ms | 7% |
| `fold`, single-threaded into the store | 180 ms | 4% |
| `sort` | 139 ms | 3% |

All CPU work totals under half a second. The read ran at ~340 MB/s on a drive
rated near 5 GB/s, so the obvious read was "queue depth 1 — give the NVMe more
requests in flight".

**That turned out to be wrong, and the experiment is worth recording.** Splitting
the MFT into eight contiguous shards, each with its own volume handle, produced:

| | Single reader | 8 readers |
|---|---|---|
| Aggregate | 316-340 MB/s | 261 MB/s |
| Per reader | 340 MB/s | 44 MB/s |

Per-reader throughput collapsed by roughly the factor by which readers were
added, and the total did not improve. The raw-volume read path serialises
somewhere below us, so concurrency buys nothing and costs eight handles, a
shard-boundary correctness risk, and a concurrency boundary. It was reverted.

Two lessons, both about method:

- **Wall clock alone could not have settled this.** Whole-volume reads varied
  4.40-5.58 s run to run on identical code, a ±25% band that swamps the effect.
  The per-reader throughput number is what made the answer unambiguous. Anything
  measured here needs a rate, not just a duration.
- **The remaining lever is the buffering mode, not parallelism.** Raw volume
  reads go through the cache manager, which costs a copy. `FILE_FLAG_NO_BUFFERING`
  would DMA straight into our buffer, at the price of hard alignment rules:
  offset, length, *and buffer address* must all be sector-aligned, so the reader
  would need an aligned allocator and would fail with `ERROR_INVALID_PARAMETER`
  rather than degrade if any of the three slipped. That is the experiment to run
  next if indexing time ever matters enough -- and it should be judged on MB/s,
  not on one wall-clock sample.

Worth keeping in perspective: this runs once. After the first build there is a
snapshot to load and a USN journal to apply, and journal batches land in
1.6-12 microseconds.

## GUI

Win32 + Direct2D, custom-drawn. Not a stock `ListView`, because result sets run
to hundreds of thousands of rows: nothing is instantiated per row, and only the
rows physically on screen are ever touched. A frame is a handful of
`FillRectangle` calls plus one `DrawText` per visible cell.

The UI thread owns the window and the Direct2D device and does nothing else.
Queries go to the engine thread, which posts back with `PostMessageW` when it
has an answer. Superseded queries are coalesced — typing quickly must not queue
a scan per keystroke — and each reply carries a generation number so a late
answer for an abandoned query is discarded rather than displayed.
