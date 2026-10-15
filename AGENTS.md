# Repository Guidance

## Scope

Dust is a Rust CLI disk-usage tool. The Windows NTFS acceleration is an optional
workspace crate under `dust-indexer/`; the normal walker remains the portable
fallback.

## Architecture

- `src/main.rs` selects the ordinary walker, `--top` aggregation, or Windows
  indexed access based on CLI compatibility.
- `src/index_client.rs` is the CLI adapter. On Windows it tries the local
  `dust-indexer` named pipe first and SQLite second.
- `dust-indexer/src/scanner.rs` enumerates directories with
  `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` and records file IDs,
  parent IDs, allocation size, and end-of-file size.
- `dust-indexer/src/store.rs` owns the SQLite schema, aggregate totals, cursor
  transactions, and indexed queries.
- `dust-indexer/src/journal.rs` reads the NTFS USN Journal with
  `FSCTL_READ_USN_JOURNAL`. Cursor continuity failures must invalidate or rebuild
  the index; never advance a cursor while returning stale totals.
- `dust-indexer/src/service.rs` and `src/ipc.rs` implement the Windows Service
  and local named-pipe server. IPC replies must pass client-token access checks.
- `src/ntfs_index.rs` contains only the native UAC re-exec helper. Do not add a
  second index format there.

## Windows behavior

The SQLite database is stored below `%LOCALAPPDATA%\dust`. A normal Windows
report attempts service access, then a local snapshot, then the ordinary walker.
`--no-index` disables this path. `--indexed` requests indexed mode explicitly
and reports an unavailable index before falling back. `--elevate` is an explicit
UAC operation; normal reports must not prompt unexpectedly.

Indexed mode is limited to options whose semantics are represented by the
snapshot. Keep filters, ignored paths, link traversal, file counts, file types,
time filters, filesystem limits, and collapsed display on the ordinary walker.

## Development and verification

Run from the repository root:

```powershell
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo build --release --workspace
```

On Windows, smoke-test both paths with a small directory:

```powershell
dust --no-index --no-progress --no-colors --depth 2 PATH
dust --top --no-progress --no-colors PATH
dust --build-index PATH
dust --indexed --no-progress --no-colors --depth 2 PATH
```

Do not require UAC interaction in unattended tests. If an index is built without
USN access, it may be queried as a snapshot but cannot be incrementally refreshed
until rebuilt with elevation or maintained by the service.

## Change hygiene

Use `apply_patch` for manual edits. Keep generated completions and the man page
in sync with `src/cli.rs` when CLI flags change. Before committing, run
`git diff --check`, inspect `git status`, and include only task-related files.
