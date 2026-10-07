# AGENTS.md

WinMerge-style diff tool in Rust + egui (eframe 0.36). Must build for Linux, Windows and macOS
(Apple Silicon).

## Build & check

```sh
cargo build                     # native dev build (on an M-series Mac this is Apple Silicon)
cargo build --release           # native release: target/release/rsmerge
cargo test                      # unit tests (all logic is tested here)
cargo test --release -- --ignored --nocapture   # 200k-line diff timing check
cargo clippy --all-targets      # keep warning-free
cargo check --target aarch64-apple-darwin       # macOS type-check from Linux/Windows (linking needs a Mac)
cargo build --release --target x86_64-pc-windows-gnu   # Windows .exe from Linux via mingw
```

- macOS builds are done natively on the user's Mac (no CI); no extra dependencies needed.
- If the user has a Windows `.exe` open while rebuilding from WSL, the build fails with
  "os error 5"; copy the newest `deps/rsmerge-*.exe` to `rsmerge-new.exe` and tell them.
- `rsmerge LEFT RIGHT` opens two files or two folders directly (handy for testing).

## GUI testing

On a Mac, just `cargo run`. From WSL, windows show up on the user's desktop and often open
minimized (off-screen at -32730), and can't be restored from there; there's no Xvfb. Prefer unit
tests, keep any GUI runs short (`WAYLAND_DISPLAY= ./target/debug/rsmerge …`, screenshot with
`import -window`, python-xlib XTEST for input) and ask the user to check visuals.

## Layout

- `main.rs`: app shell, tabs, unsaved-changes prompts, Ctrl+W, F5 routing.
  `App::open_file_compare` is the single entry point into the file view (Open screen, folder
  double-click, CLI).
- `open_dialog.rs`: Select Files or Folders screen.
- `compare.rs`: editable side-by-side view. Custom painted, one row model for both panes. Every
  edit is a line-range replacement (`edit`), which drives undo/redo, copy-difference and filler
  typing; the diff re-runs after each edit.
- `diff.rs`: line diff (interned line ids + Myers; Histogram in `similar` is far too slow),
  aligned rows with filler lines, word-level inline diff.
- `json_format.rs`: canonical JSON (sorted keys, 4-space indent) for the file view's "Format JSON"
  button, which rewrites both sides as one joined undo step.
- `text_file.rs`: load/save keeping encoding, BOM, EOL; missing files open empty and are created
  on save.
- `image_diff.rs`: pixel diff (no egui) with a tolerance, grouped into regions for Prev/Next.
  `image_compare.rs`: read-only image view (side by side or blended overlay, shared zoom/pan).
  Files with an image extension open here instead of the text view.
- `folder_scan.rs`: background folder scan/compare (no egui). `folder_view.rs`: tree/flat
  table with Show filters.

## Conventions

- Match surrounding style; short doc comments on intent, not mechanics.
- egui 0.36 APIs differ from older docs (e.g. `App::ui`, `Panel::top`, `ui.close()`); check
  `~/.cargo/registry/src/*/egui-0.36.2` when unsure.
- Some glyphs (e.g. ⇅) aren't in the default fonts; ⏷ ⏵ × 📁 📄 render fine.
- Colours: left = red (removed), right = green (added), filler = grey. User likes Cobalt2.
- Out of scope unless asked: syntax highlighting, reports/patches, CLI options, plugins, image
  editing/merging, 3-way compare.

## Likely next

- Folder view: copy/delete files between sides (with confirmation).
- Option to compare text ignoring line endings (CRLF vs LF) in folder "Contents" mode.
- Optional difftastic (`difft --display json`, `DFT_UNSTABLE=yes`) engine feeding the same diff model.
- Colour theme setting (e.g. Cobalt2).
- macOS `.app` bundle/packaging; verify on a real Mac.
- Smaller gaps: IME preedit display, keeping mixed line endings on save.
