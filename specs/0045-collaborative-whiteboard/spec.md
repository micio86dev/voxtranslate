# 0045 — Collaborative whiteboard (MVP)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0023](../0023-call-toolbar-overflow-menu/spec.md), [0033](../0033-screenshare-pan-zoom/spec.md) |

## 1. Context & Problem

Issue #21 asks for in-session interactivity: a **collaborative whiteboard** and
**mini-games**. Scoped (with the owner) to **whiteboard first**, over the **existing
WebSocket relay** (no new infra), with a **synced snapshot** so a late-joiner sees the
drawing already on the board. Mini-games are a separate follow-up.

## 2. Goals / Non-Goals

**Goals**
- A shared canvas where all peers draw in real time: pen, eraser, colour, clear.
- Late-joiners receive the current board.
- Reuse the room WS relay (same pattern as screen-share/reactions). No Supabase.

**Non-Goals (issue "future")**
- Save/export, sticky notes, shapes/text, undo, leaderboard.
- Mini-games (next spec).
- Stroke thickness control (fixed pen/eraser widths for MVP).

## 3. Requirements

- **R1 — Realtime draw.** A local stroke streams as batches of **normalised (0..1)**
  points relayed to peers (`broadcast_except`), so each client scales to its own
  canvas — no distortion across desktop/mobile.
- **R2 — Snapshot on join.** The server keeps the room's op-log and sends
  `WhiteboardSnapshot` to a joining peer; the op-log **is** the snapshot. Capped
  (`MAX_WHITEBOARD_OPS = 4000`, oldest dropped) to bound memory.
- **R3 — Tools.** Pen, eraser (destination-out → reveals the board), 5 colours, clear.
  Mouse + touch (pointer events). Coordinates + widths normalised so resize/rotation
  never stretches; the local op-log redraws on resize.
- **R4 — UI.** Opened from the ⋯ menu (no toolbar clutter, per 0023); a full-stage
  overlay with a floating toolbar; reset + hidden on leave.

## 4. Design & Architecture

**Protocol (`server/src/protocol.rs`)** — `WhiteboardOp` (`#[serde(tag="op")]`):
`Draw { id, tool, color, width, points: Vec<[f32;2]> }` | `Clear`.
`ClientMessage::Whiteboard { op }`; `ServerMessage::Whiteboard { peer_id, op }` +
`WhiteboardSnapshot { ops }`.

**Server state (`rooms.rs`)** — `Room.whiteboard: Vec<WhiteboardOp>`;
`whiteboard_apply` (Clear wipes, Draw appends with cap) + `whiteboard_snapshot`.
`lib.rs` relays each op (after persisting) and sends the snapshot right after
`RoomJoined` when non-empty.

**Client** — `whiteboard.ts` (`Whiteboard` class): pointer drawing → batched Draw ops
(every ~55 ms, each batch carries the previous batch's last point for seamless joins);
`applyOp` / `applySnapshot` render incoming ops; keeps a local op-log to redraw on
resize (DPR-aware). `app.ts` wires the ⋯ toggle, toolbar, and `whiteboard` /
`whiteboard_snapshot` dispatch; `index.astro` + CSS for the overlay; `icons.ts`
(`board`, `eraser`); i18n ×8.

**Key decisions:**
- *Op-log = snapshot.* The server stores exactly what it relays, so late-join replay
  is trivial and consistent; a cap bounds memory.
- *Normalised coords + widths.* The only robust way to keep the drawing identical
  across different canvas sizes/orientations.
- *Eraser = destination-out.* The canvas is transparent over a CSS "board" surface, so
  erasing reveals the board and replays correctly in order.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Protocol + room op-log + relay + snapshot | `protocol.rs`, `rooms.rs`, `lib.rs` |
| S1 | Canvas + tools + pointer drawing | `whiteboard.ts`, `index.astro` |
| S2 | Realtime sync + snapshot dispatch + ⋯ toggle | `app.ts` |
| S3 | Icons, i18n ×8 | `icons.ts`, `i18n.ts` |

## 6. Testing & Verification

- Server: `cargo build` + clippy clean.
- Client: `astro check` clean; **101/101** unit tests; production build OK.
- Manual: two peers draw simultaneously → both see strokes live; pen/eraser/colour/
  clear work; a third joiner sees the existing drawing; touch works on mobile; leaving
  resets the board.

## 7. Deployment & Operations

- **Server change** (new messages + room state) → needs `railway up`. Client → Vercel
  autodeploy. Forward-safe: the client only acts on the new message types, so deploy
  order doesn't break anything.

## 8. Risks / Open Items

- Op-log cap drops the oldest strokes in a very heavy session (degrades the snapshot
  for late-joiners only). Acceptable for MVP; tune `MAX_WHITEBOARD_OPS` if needed.
- No persistence beyond the live room (export/save is "future" per #21).
- **Mini-games** (#21 part 2) remain to do — separate spec.

## 9. References

- Issue: #21
- Files: `server/src/{protocol,rooms,lib}.rs`, `client/src/scripts/whiteboard.ts`,
  `client/src/scripts/app.ts`, `client/src/pages/index.astro`, `client/src/scripts/{icons,i18n}.ts`.
