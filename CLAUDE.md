# dush

A terminal UI disk usage explorer written in Rust. Single-binary, single-file (`src/main.rs`).

## Stack
- Rust 2024 edition
- `ratatui` for TUI rendering, `crossterm` for terminal I/O
- `walkdir` for recursive directory traversal

## Architecture
Everything lives in `src/main.rs`. Key pieces:
- `App` — UI state (cwd, entries, list selection, goto buffer, help overlay, in-flight scan)
- `list_children` — synchronous, shallow read of a directory's immediate children
- `walk_size` — recursive byte sum for a single subdirectory, cancellable via `AtomicBool`
- `Scan` — background thread + `mpsc` channel that streams per-directory sizes back to the UI; the main loop polls it each tick via `poll_scan`
- `run` — event loop. Tick is 50ms while scanning, 200ms when idle

Sizing is two-phase by design: children appear immediately with `size = 0` for directories, then fill in as the background scan completes. Scan results are applied by index, so the entry vec is not reordered until the scan finishes. Symlinks are `lstat`ed and not walked. A directory is snapshotted only after its walk finishes, including a real size of 0.

## Conventions
- Keep it single-file unless something genuinely warrants splitting
- No `unwrap()` on user-reachable paths — surface errors into `App.error` and render in the footer
- Any new long-running work must be cancellable (follow the `AtomicBool` + `mpsc` pattern in `refresh`)
- Don't block the UI thread. If you're adding I/O, push it to a background thread and stream results
- Match the existing key-binding style in `run()`; update the help overlay in `ui()` when adding keys

## Build / run
- `cargo run` — opens at cwd
- `cargo run -- <path>` — opens at given path
- `cargo build --release` — release binary at `target/release/dush`
