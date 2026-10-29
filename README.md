# LocalPDF-TUI

A **private, offline, terminal-native PDF toolkit** — a local ILovePDF.
Zero network, zero cloud: every operation happens on your machine, in one
fast Rust binary, with two frontends over the same engine:

- a polished **TUI dashboard** for humans,
- a deterministic **JSON CLI** for AI agents.

## Features (v0.3)

| Operation | TUI | Agent mode |
|---|---|---|
| Merge PDFs | ✓ | `--json --merge a.pdf b.pdf` |
| Split page ranges | ✓ | `--json --split in.pdf --pages 1-3,5` |
| Compress (orphan drop + Flate) | ✓ | `--json --compress in.pdf [--level 0-9]` |
| Rotate pages | ✓ | `--json --rotate in.pdf --pages 1,3-5 --degrees 90` |
| Reorder pages | ✓ | `--json --reorder in.pdf --order 3,1,2` |
| Delete pages | ✓ | `--json --delete-pages in.pdf --pages 2,4-5` |
| Document info | ✓ | `--json --info in.pdf` |
| Text watermark (all pages) | agent | `--json --watermark in.pdf --text DRAFT` |
| Page numbers | agent | `--json --pagenum in.pdf` |
| Edit metadata | agent | `--json --set-metadata in.pdf --meta "Title=Q3"` |
| Extract text | agent | `--json --extract-text in.pdf [--pages 1-2]` |
| Extract images | agent | `--json --extract-images in.pdf [--out-dir dir]` |
| Assemble images → PDF | agent | `--json --from-images a.png b.jpg` |
| Generate sample PDF | `[a]` key | `engine::generate_sample` (used by tests/demo) |

See [`PRD.md`](./PRD.md) for the full feature roadmap (compress, rotate,
reorder, delete, extract text/images, watermark, encryption, batch mode…).

## Quickstart

```bash
cargo run --release
```

TUI keys:

```
[a]      queue a freshly generated sample PDF
[←][→]   focus between File Queue / Operations
[↑][↓]   move cursor
[enter]  run the selected operation
[q]/esc  quit
```

Agent mode (exactly one JSON document on stdout, status in the exit code):

```bash
# merge three files
localpdf-tui --json --merge a.pdf b.pdf c.pdf --output merged.pdf
# → {"status":"success","operation":"merge","output_file":"...\\merged.pdf","pages":9}

# split pages 1-3 and 5
localpdf-tui --json --split big.pdf --pages 1-3,5 --output part.pdf
# → {"status":"success","operation":"split","output_file":"...\\part.pdf","pages":4}

# rotate page 2 by 90°
localpdf-tui --json --rotate big.pdf --pages 2 --degrees 90
# → {"status":"success","operation":"rotate","output_file":"...\\rotated.pdf","pages":1}

# document facts, no file written
localpdf-tui --json --info big.pdf
# → {"status":"success","operation":"info","source_file":"...","pages":42,"file_size_bytes":183221,"object_count":310,"encrypted":false,"title":"Q3 Report","creator":"…"}

# pull the text out of pages 1-3 (no file written: text comes back in the JSON)
localpdf-tui --json --extract-text big.pdf --pages 1-3
# → {"status":"success","operation":"extract-text","source_file":"...","pages":3,"characters":1204,"text":"…"}

# stamp every page with a diagonal DRAFT
localpdf-tui --json --watermark big.pdf --text DRAFT --output big-draft.pdf
# → {"status":"success","operation":"watermark","output_file":"...\\big-draft.pdf","pages":42}

# on failure the only shape is:
# → {"status":"error","operation":"split","error":"page 4 is beyond the end of a 3-page document"}
```

All file access is pinned to a sandbox:

```bash
localpdf-tui --json --workspace C:\pdfs --merge in.pdf --output out.pdf
# .. and symlinks that escape the workspace → PathEscape JSON error, exit 1
```

Exit codes: `0` success · `1` operation error · `2` bootstrap error.

## Architecture

```
src/
├── main.rs    CLI + mode routing (TUI vs agent)
├── engine.rs  sandboxed I/O + lopdf object-closure merge/split
├── error.rs   AppError/AppResult — the only error type
├── tui.rs     terminal lifecycle, panic-safe restore, event loop
└── ui.rs      pure render fn: App state → widgets (no allocation in draw)
```

**Engine design note:** merge/split are object-closure copies between
`lopdf::Document`s. Only objects reachable from the kept pages are copied
with renumbered references; dangling references in damaged inputs become
`Null` placeholders so outputs are always self-consistent. Split skips
`/Parent` traversal on purpose — following it would pull every sibling page
into the output.

**Security model:** every path is treated as untrusted. `resolve()`
canonicalizes inputs; `resolve_destination()` canonicalizes the parent dir of
anything we create. Both then enforce containment under the workspace root —
`../` traversal and symlink escapes die as `PathEscape`.

## Development

```bash
cargo build
cargo test   # engine unit tests: merge/split/ranges/escape
```

Rust 1.85+ (lopdf uses edition 2024 features).
