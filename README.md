# dush

A fast, keyboard-driven terminal UI for exploring disk usage. Like `du`, but you can walk the tree.

Built in Rust with [`ratatui`](https://ratatui.rs).

## Features

- Per-directory size totals with inline bar visualization
- Non-blocking background scans — the UI stays responsive while directories are measured
- Sort by size, name, or modification time (applied when the scan finishes)
- Apparent byte sizes (not allocated blocks). Hard links count once per name. Symlinks are listed as themselves and are not followed
- `g` to jump to any path (`~` and `~/...` expand to your home directory)
- Help overlay (`?`) with all key bindings

## Install

Requires a recent Rust toolchain (edition 2024).

```sh
git clone <this repo>
cd dush
cargo build --release
```

The binary will be at `target/release/dush`. Copy it somewhere on your `$PATH`, or run it directly.

## Usage

```sh
dush              # explore the current directory
dush ~/Downloads  # explore a specific path
```

### Keys

| Key                | Action                  |
| ------------------ | ----------------------- |
| `j` / `↓`          | Move down               |
| `k` / `↑`          | Move up                 |
| `l` / `enter` / `→`| Descend into directory (follows a symlink only for navigation) |
| `h` / `backspace` / `←` | Ascend to parent   |
| `/`                | Fuzzy filter            |
| `g`                | Goto path (`~` and `~/...`) |
| `s`                | Cycle sort (size desc, size asc, name, time) |
| `d`                | Delete with confirmation |
| `y`                | Copy path to clipboard  |
| `o`                | Open with the default app |
| `e`                | Export JSON to the cache directory |
| `t`                | File-type breakdown     |
| `T`                | Cycle theme (saved)     |
| `b`                | Bookmarks               |
| `B`                | Bookmark the current directory |
| `?`                | Toggle help overlay     |
| `q`                | Quit                    |
| `esc`              | Close overlay, clear filter, or quit |

While a scan is running, the title bar shows progress (`scanning N/M`) and the sort that will apply when it finishes. Directory sizes update live. Pressing `s` during a scan does not reorder rows until the walk completes, so sizes stay on the right entries. `e` writes `export.json` under the dush cache directory (`~/.cache/dush`, or `$XDG_CACHE_HOME/dush`) and does not create a file in the directory you are viewing.

## License

TBD.
