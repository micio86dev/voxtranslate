# 0062 — Advanced whiteboard: multi-page, shapes, highlighter, PNG/PDF export

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0045](../0045-collaborative-whiteboard/spec.md) |

## 1. Context & Problem

Issue #96: the spec-0045 whiteboard is a single transparent canvas with pen + eraser +
5 colours. That's too basic for real meetings/lessons/brainstorming. Users want multiple
pages, more drawing tools (highlighter, shapes), and a way to take the work away (export).

## 2. Goals / Non-Goals

**Goals (the issue's stated MVP):**
- **Multi-page** — create, navigate, duplicate and delete pages; all peers + late-joiners
  stay in sync.
- **Tools** — pen, **highlighter** (semi-transparent), eraser, and **shapes**
  (line, arrow, rectangle, ellipse). Three stroke widths.
- **Export** — current page → **PNG**; all pages → **multi-page PDF**. Zero dependencies.

**Non-Goals (issue's "Future"/"Extra", deferred):**
- Text tool, ruler/grid-snap/alignment guides, sticky notes, drag-drop images.
- Live cursors, per-user undo/redo, versioning/auto-save, view-only share links.
- **Page reorder** (the op-log ordering model makes it non-trivial; add/dup/delete only).
- **Any server change** — the relay/persistence model is untouched (see §4).

## 3. Requirements

- **R1 — Tools.** *Given* the board is open, *then* I can pick pen, highlighter, eraser,
  line, arrow, rectangle or ellipse, a colour, and one of three widths; each draws/relays
  accordingly. Highlighter strokes are semi-transparent; shapes rubber-band from press to
  release and commit on release.
- **R2 — Multi-page.** *Then* I can add a page, duplicate the current page, delete a page
  (never the last), and step prev/next; the strip shows "n / N". New/duplicated/deleted
  pages appear for every peer and for a late-joiner replaying the snapshot.
- **R3 — Per-page clear.** *Then* "Clear" wipes only the current page (for everyone), not
  the whole board.
- **R4 — Export PNG.** *Then* "PNG" downloads the current page as a PNG on the board's
  surface colour.
- **R5 — Export PDF.** *Then* "PDF" downloads every page, in order, as one multi-page PDF.

## 4. Design & Architecture

**Key decision — client-only, encode into the existing relayed op (no server change).**
The server (`protocol.rs`/`rooms.rs`) stores `WhiteboardOp::Draw { id, tool, color, width,
points }` | `Clear` opaquely and replays the log; serde drops unknown fields, so new data
MUST ride existing fields:

- **Page** → encoded in `id` as `"<pageId>:<seq>"`. Legacy ids (no `:`) map to the default
  page `p1`. Pages are ordered by **first appearance in the op-log** (globally consistent
  across clients + snapshot replay). New page ids are `p-<myId>-<n>` (collision-free).
- **Shape/tool** → the free-string `tool`: `pen | highlighter | eraser | line | arrow |
  rect | ellipse`. Shapes carry exactly two normalised points `[start, end]`.
- **Page structure** → marker Draw ops (empty `points`) the server persists like any draw:
  `page-add`, `page-del`, `page-clear`. Replayed in order ⇒ late-joiners reconstruct the
  same page set + content. `page-del` drops that page's ops from each client's mirror;
  `page-clear` drops only its drawables.

**Files:**
- `client/src/scripts/whiteboard.ts` — tools, widths, pages, encode/parse, shape preview,
  `onPagesChanged` callback; `renderAll(ctx, ops, w, h)` shared by live redraw + export.
- `client/src/scripts/wb-export.ts` (new) — `pageToPng(blob)` + a tiny hand-rolled
  multi-page PDF (JPEG `/DCTDecode` XObjects); pure + unit-tested.
- `client/src/pages/index.astro` — richer `.wb-toolbar` (tools, 3 widths, colours, clear,
  export menu, close) + a bottom `.wb-pagebar` (prev · n/N · next · add · duplicate · delete).
- `client/src/scripts/app.ts` — wire the new controls; render the page strip from
  `onPagesChanged`.
- `client/src/scripts/icons.ts` — highlighter, line, arrow, square, circle, plus, download,
  chevron-left/right, copy-reuse.
- `client/src/scripts/i18n.ts` — labels/tooltips for the new controls (all 8 locales).

**Render:** export renders each page to an offscreen canvas filled with the board surface
`#0f1320` at 1600×900 (points are normalised, so any size works), then PNG via `toBlob`
or JPEG via `toDataURL` for the PDF.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Tools + widths + shape preview/commit; `renderAll` refactor | `whiteboard.ts`, `icons.ts` |
| S1 | Multi-page model (parse/encode, markers, `onPagesChanged`) + per-page clear | `whiteboard.ts` |
| S2 | Toolbar + page strip markup/CSS + wiring + i18n | `index.astro`, `app.ts`, `i18n.ts` |
| S3 | PNG + multi-page PDF export (zero-dep) | `wb-export.ts`, `whiteboard.ts`, `app.ts` |

## 6. Testing & Verification

- **Unit (`whiteboard.test.ts`, `wb-export.test.ts`):** page-id parse/encode + legacy
  fallback; op filtering per page; `page-del`/`page-clear` mutate the mirror; shape ops carry
  2 points; PDF bytes start with `%PDF`, carry `/Count N`, end with `%%EOF`; PNG blob type.
- **e2e (`whiteboard.spec.ts`, new):** open board, draw with pen, switch to a shape + a
  highlighter, add/duplicate/delete a page (strip count tracks), per-page clear, and a PNG
  download fires (`waitForEvent('download')`). a11y suite stays green (toolbar/strip buttons
  have names; board open is audited).
- **Automated:** `astro check` (0 errors), build, full unit suite, e2e green vs `:3001`.

## 7. Deployment & Operations

- Client-only. No env vars, migrations, or server/protocol changes. Vercel auto-deploys on
  `main`. Existing rooms keep working (legacy ops → page `p1`).

## 8. Risks / Open Items

- The room op-log cap (`MAX_WHITEBOARD_OPS`, server) still bounds total ops across all pages;
  very long sessions drop the oldest first (unchanged from 0045) — surfaced as a known limit.
- Page **reorder** is deferred (client-only ordering = op-log appearance). Text tool, grid/
  ruler, live cursors, undo/redo, versioning remain future work (#96 "Future").

## 9. References

- Issue: #96 (extends spec 0045's relay/op-log model).
- Files: `client/src/scripts/whiteboard.ts`, `client/src/scripts/wb-export.ts`,
  `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/src/scripts/icons.ts`, `client/src/scripts/i18n.ts`, `client/e2e/whiteboard.spec.ts`
