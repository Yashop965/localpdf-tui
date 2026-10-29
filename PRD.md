# PRD — LocalPDF-TUI

**Version:** v0.1 (draft, 2026-10-28) · **Status:** M0 shipped, M1 scoped
**Repo:** `~/projects/localpdf-tui` · **Stack:** Rust, lopdf 0.46, ratatui 0.29, crossterm 0.28, clap 4

---

## 1. One-liner

A **private, offline, terminal-native PDF toolkit** — a local ILovePDF that never
uploads a byte to the network, has a polished TUI for humans, and a deterministic
JSON CLI for AI agents.

## 2. Problem & positioning

| Pain | Answer |
|---|---|
| ILovePDF/SmallPDF type tools ship your documents to a cloud | 100% on-device; zero network calls, ever |
| Desktop PDF tools are heavy GUIs with 200 features you don't use | One fast binary, keyboard-driven dashboard |
| Agents (Claude/Hermes/…) can't drive GUIs | `--json` agent mode: one clean JSON doc on stdout, exit code as status |
| Random CLI scripts are unsafe with paths | Sandbox engine: every path canonicalized and pinned under a workspace root |

Target users: (1) anyone handling sensitive PDFs locally, (2) local LLM agents that
need PDF ops as tools, (3) demo/teaching context (this class).

## 3. v0.1 — shipped (M0)

- **Merge** — N PDFs → one, object-closure copy with renumbered references.
- **Split** — extract 1-based page ranges (`1-3,5`); minimal-copy mode skips
  `/Parent` traversal so unselected pages never land in the output.
- **TUI dashboard** — left: File Queue · right: Operations + range input ·
  bottom: status log. Keys: `a` (add real generated sample), `←→` focus,
  `↑↓` move, `Enter` run, `q`/`Esc` quit.
- **Agent mode** — `--json --merge a.pdf b.pdf --output out.pdf` /
  `--json --split in.pdf --pages 1-3 --output out.pdf`. TUI bypassed; stdout
  carries exactly one JSON document, nothing else.
- **Sandbox** — `Engine::resolve` (existing files) and `resolve_destination`
  (files we create) canonicalize + containment-check every path; `../` and
  symlink escape → `PathEscape` error.
- **Deterministic JSON contract**
  - success: `{"status":"success","operation":"merge","output_file":"...","pages":N}`
  - error: `{"status":"error","operation":"...","error":"..."}`, exit 1
- **Sample generator** — `[a]` writes a real 1-page Helvetica PDF so every op
  is demoable with zero input files.

## 4. Architecture (fixed points — new features must respect these)

```
main.rs      CLI (clap) + mode routing
error.rs     AppError/AppResult — the only error type
engine.rs    sandbox + lopdf object-closure primitives + sample gen
tui.rs       terminal lifecycle, panic hook, event loop, App state
ui.rs        pure render fn (App → widgets), no state mutation in draw
```

Rules for M1+:
1. Every new operation is an `Engine` method returning `OperationReport` —
   TUI and agent mode call the same code, so both get the behavior for free.
2. Agent stdout stays pure JSON; progress/diagnostics go to **stderr** only.
3. Draw loop allocates only pre-cached strings (rebuild caches on state change).
4. No new I/O outside the sandbox; new file-producing ops use
   `resolve_destination`.

## 5. Feature roadmap ("the things we actually use locally")

### P0 — v0.2 (M1) — **shipped 2026-10-28**
| Op | CLI flag | Notes |
|---|---|---|
| Compress | `--json --compress in.pdf [--level 0-9]` | drop orphaned objects (closure-copy keep-only-reachable) + Flate-compress unfiltered streams at the requested level (0-9, default 9). `--level` implemented directly with `flate2::Compression` when M2 landed. |
| Rotate | `--json --rotate in.pdf --pages 1,3-5 --degrees 90\|180\|270` | set `/Rotate` on selected page dicts |
| Reorder | `--json --reorder in.pdf --order 3,1,2` | rewrite root Kids array |
| Delete pages | `--json --delete-pages in.pdf --pages 2,4-5` | split's inverse: keep-complement, same minimal-copy path |
| Info | `--json --info in.pdf` | JSON: pages, size, encryption, title/creator, object count |

TUI: right column grows to **Merge · Split · Compress · Rotate · Delete · Info**;
context input line under it changes shape per op (range / degrees / order / text).

### P1 — v0.3 (M2) — **shipped 2026-10-30**
| Op | CLI flag | Notes |
|---|---|---|
| Extract text | `--json --extract-text in.pdf [--pages 1-3] [--output text.md]` | lopdf `extract_text_with_limit` (32 MiB bomb cap); text is embedded in the JSON when no output file is given |
| Extract images | `--json --extract-images in.pdf [--out-dir img/]` | walk page `/XObject` tables; JPEG pass-through, Flate-decoded 8-bit gray/RGB → PNG (64 MiB cap); CMYK/indexed/16-bit/JPX/JBIG2 counted as skipped |
| Watermark | `--json --watermark in.pdf --text "CONFIDENTIAL"` | one shared diagonal Form overlay appended to every page's `/Contents` array; text 1-120 chars (`--opacity` parked — gray 50% for now) |
| Page numbers | `--json --pagenum in.pdf` | per-page overlay (numbered stamp appended to that page only), bottom-center-ish fixed position |
| Metadata edit | `--json --set-metadata in.pdf --meta "Title=x" [--meta …]` | repeatable `Key=Value` pairs; Info dict created if absent |
| Images → PDF | `--json --from-images a.png b.jpg [--out f]` | one page per image via `lopdf::xobject::image_from` (JPEG DCT pass-through, others re-encoded raw); MediaBox = pixel size |
| PDF → images (render) | `--json --to-png in.pdf` | **parked to M3** — needs a rasterizer (pdfium/magick interop); lopdf can't render. The *object* direction (extract embedded image streams) ships now via extract-images |

### P2 — later (M3+)
- **Encrypt / decrypt** (`--password`; lopdf encryption support is present in 0.46)
- **Repair** — xref recovery for damaged files (lopdf `XrefRecovery` surfaced in JSON)
- **Batch / watch** — `--json --watch in/ out/` glob-queue mode for agents
- **Bookmark tree export/import** (lopdf `Toc`)
- **Form fill** — AcroForm field set + flatten
- **Compare/diff** — page hash list, "which pages changed"
- **Config** — `~/.localpdf.toml` (workspace, default levels, keymap)
- **Single-static-binary release** + optional ANSI-256 theme switch

### Non-goals
- No network of any kind. No cloud, no telemetry, no license keys.
- No server/API mode. No mobile.
- No full WYSIWYG rendering in the TUI (text + structure only).
- No OCR (out of scope; note in docs as an integration point).

## 6. Agent-mode contract (all ops)

```
localpdf-tui --json <op> [args] [--workspace DIR]
stdout: exactly one JSON document (stable field order)
stderr: human diagnostics only
exit: 0 success · 1 operation error · 2 bootstrap error (workspace/arg validation)
```

`--workspace` pins the sandbox for the whole invocation — this is how an agent
keeps all generated files in one folder.

## 7. TUI spec (v0.2 layout)

```
┌────────────────────────────────────────────────────────────────┐
│ LocalPDF-TUI · private pdf toolkit [a] add [←→] focus [↑↓] …  │
├──────────────────────────────┬─────────────────────────────────┤
│ FILE QUEUE                   │ OPERATIONS                      │
│ ▸  1. sample_1.pdf · 2 KB   │ ▸ Merge     concatenate queue   │
│    2. sample_2.pdf · 2 KB   │   Split     extract ranges      │
│                              │   Compress  rebuild + flate     │
│                              │   Rotate    set /Rotate         │
│                              │   Delete      remove pages      │
│                              │   Info        document report   │
│                              │ SPLIT RANGES [ 1-3▏ ]           │
├──────────────────────────────┴─────────────────────────────────┤
│ STATUS LOG  (last 3 lines, ✓ green / ✗ red)                    │
└────────────────────────────────────────────────────────────────┘
```

Context input line is per-op: ranges (`1-3,5`), page list, degrees (`90`),
order (`3,1,2`), text. Invalid input never executes; it flashes in the log.

## 8. Success criteria

1. **Demo bar (class):** open TUI → `[a]` twice → merge → split range →
   status log shows ✓ lines; open outputs in any viewer, correct pages.
2. **Agent bar:** run a 3-file merge + a split with Hermes/terminal, parse
   both JSON lines in one shell step without regex heroics.
3. **Safety bar:** `--workspace C:\tmp\w` + input `..\..\..\evil.pdf`
   → `PathEscape` JSON error, nothing outside `w` touched.
4. **Honesty bar:** `--compress` output opens in Acrobat/Chrome and is
   measurally smaller; `--info` matches viewer metadata.
5. **Quality bar:** `cargo build` warning-free; `cargo test` all green;
   no `unwrap()` outside `#[cfg(test)]` and the panic-hook restore.

## 9. Risks & open questions

| Risk | Mitigation |
|---|---|
| lopdf 0.46 is young, API churn (`Document` vs old `Pdf`) | pin `0.46`; wrap all lopdf calls in `engine.rs` — 1 module absorbs churn |
| `--to-png` needs a rasterizer; lopdf doesn't render | park to P1, ship as "best-effort" or add `pdfium`/`magick` interop behind a flag |
| Windows verbatim paths (`\\?\`) break naive containment checks — **hit this in M0** | parent-canonicalize in `resolve_destination`; both test cases covered |
| Encrypted inputs | `Document::load` auto-decrypts with empty password; non-empty → `Pdf` error JSON |
| Huge PDFs (1000s of pages) | closure-copy is O(reachable objects); document limit; M3: `lopdf` streaming loader |
| Terminal width < 100 cols | layout uses percentages; test at 80 cols in M1 |

## 10. Milestones

- **M0 — done:** skeleton, merge, split, agent JSON, sandbox, samples, 5 tests.
- **M1 — done (2026-10-28):** compress, rotate, reorder, delete-pages, info +
  TUI wiring (6-op dashboard, per-op context input) + 11 tests + CLI smoke.
- **M2 — done (2026-10-30):** extract-text (bounded), extract-images
  (JPEG pass-through + Flate→PNG), images→PDF (`from_images`), watermark,
  page numbers, metadata, compress `--level 0-9` + 17 tests + CLI smoke.
- **M3:** to-png rasterization (pdfium/magick interop), encryption, repair,
  watch/batch, config, release packaging.

## 11. Class demo script (2 minutes)

1. `cargo run` → dashboard appears, status line ready.
2. `[a]`, `[a]` — two real sample PDFs queued (generated on disk, not mocks).
3. `→` to Operations, `Enter` on Merge → `✓ merged 2 files → merged.pdf (2 pages)`.
4. Highlight `sample_1.pdf` up top, `Enter` on Split with range `1` →
   `✓ split '1' → split.pdf (1 pages)`.
5. Same op, agent style:
   `localpdf-tui --json --merge sample_1.pdf sample_2.pdf` → one JSON line,
   pipe into `jq`. “Same engine, two frontends.”
6. Security closer: `--json --split C:\Windows\win.ini --pages 1` →
   `PathEscape` JSON error. “Your agent can't exfiltrate through my tool.”
