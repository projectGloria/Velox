# Elevation

## What needs it, and why

Exactly one thing needs administrator privileges: opening a **raw volume
handle**.

```rust
CreateFileW(r"\\.\C:", GENERIC_READ, ...)
```

Windows treats a raw volume handle as a bypass of the filesystem's entire access
control layer — with it you can read any byte of the volume, including files the
current user has no permission to open. So the handle itself is gated on
`SeManageVolumePrivilege`/administrator, regardless of what you intend to read.

Both fast-path features go through that handle:

| Operation | IOCTL | Needs raw handle |
|---|---|---|
| Volume geometry | `FSCTL_GET_NTFS_VOLUME_DATA` | yes |
| Reading `$MFT` | direct sector reads | yes |
| Journal identity | `FSCTL_QUERY_USN_JOURNAL` | yes |
| Live change stream | `FSCTL_READ_USN_JOURNAL` | yes |

Enumerating drive letters, checking the filesystem type, and the whole
`FindFirstFile` fallback need nothing special.

## Velox does not run elevated all the time

The engine checks once, at startup, with `ntfs::volume::is_elevated()`
(`OpenProcessToken` + `GetTokenInformation(TokenElevation)`), and routes
accordingly. It never demands elevation and never fails because it is missing.

Every entry point that touches a raw handle returns `Error::AccessDenied`
rather than panicking, and the orchestration in `ntfs::build_index` treats that
as a **routing decision**, not an error:

```
volume is NTFS  ──┬── elevated ────────> raw MFT read      (SourceKind::Mft)
                  └── not elevated ────> FindFirstFile walk (SourceKind::Walk)
volume is not NTFS ───────────────────> FindFirstFile walk (SourceKind::Walk)
```

`BuildReport` records which path each volume took and why, so the CLI and the
GUI status bar can state it plainly instead of silently being slow.

## What you lose without elevation

1. **Index build speed.** A walk is one syscall per directory against metadata
   scattered across the volume; the MFT is one large sequential read.
2. **Coverage.** The fallback walks the user's profile (`%USERPROFILE%`), not
   the whole volume. Walking all of `C:\` unelevated is punishing and would hit
   permission errors throughout.
3. **Live updates.** No USN journal, so the index goes stale until a rebuild
   (F5 in the GUI).
4. **Snapshot reuse.** A saved index is only loaded when its USN journal id
   still matches. Unelevated we cannot query the journal at all, so the snapshot
   cannot be verified and is discarded rather than risk showing stale results.
   This is deliberate — a fast wrong answer is worse than a slow right one.

Prefix search itself is unaffected. Once entries are in the index, query latency
does not depend on how they got there.

## Design options considered

**Elevate the whole app.** Simplest, and what v1 does — the user gets one UAC
prompt at launch. The cost is that a long-running search tool sits at high
integrity for its whole lifetime, and it cannot be launched from the startup
folder without a prompt every time.

**Elevate only the index build.** Attractive on paper, but the USN journal needs
the raw handle *continuously*, not once. A build-only elevation would give a
fast initial index that immediately begins going stale.

**Split into an elevated service plus an unelevated client** — what Everything
does, and the correct end state. A service holds the index and the journal
readers; the GUI runs at normal integrity and asks over a named pipe for the
~50 rows currently on screen plus a match count, so the IPC payload stays tiny
no matter how large the result set. The engine is already a library with a
clean query API (`Searcher::search` → `SearchResults::window`), so this is a
hosting change rather than a rewrite. It was staged after v1 deliberately: the
service installer, pipe protocol, and privilege boundary are a meaningful chunk
of work that would have landed before any search code.

## Verifying behaviour both ways

The test suite covers both branches without needing two runs at different
privilege levels — tests that need a raw handle detect the situation and assert
the correct outcome for whichever way they were launched:

```rust
match RawVolume::open(&c) {
    Ok(vol)                  => { /* elevated: geometry must be sane */ }
    Err(Error::AccessDenied(l)) => assert_eq!(l, "C:"),
    Err(e)                   => panic!("unexpected error: {e}"),
}
```

Tests that can only run elevated print a `SKIP:` line with the reason, so a
green suite from an ordinary shell never silently means "the MFT path was never
exercised".
