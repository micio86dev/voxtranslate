# 0046 — Mini-game: Tic-Tac-Toe (issue #21 part 2)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0023](../0023-call-toolbar-overflow-menu/spec.md), [0045](../0045-collaborative-whiteboard/spec.md) |

## 1. Context & Problem

Part 2 of issue #21 (mini-games). MVP = **Tic-Tac-Toe**, 2 players, over the existing
WS relay (like the whiteboard, 0045). It also lays a **game-agnostic** relay so future
games (quiz, etc.) reuse the same plumbing.

## 2. Goals / Non-Goals

**Goals**
- Two players (X/O) play Tic-Tac-Toe in real time inside a call; a 3rd/4th watch.
- A late-joiner sees the game in progress (snapshot).
- Server stays game-agnostic: it relays + stores an opaque state, no game rules.

**Non-Goals**
- Other games (quiz, draw-and-guess, reaction) — future, same relay.
- Server-side rule enforcement / anti-cheat (turn-based among people in a call;
  client-authoritative is fine).
- Scores/leaderboard (issue "future").

## 3. Requirements

- **R1 — Game-agnostic relay.** `ClientMessage::Game { state }` → the server stores the
  latest `state` per room and `broadcast_except` `ServerMessage::Game { peer_id, state }`.
  `state == null` ends the game.
- **R2 — Snapshot on join.** If a game exists, the server sends `GameSnapshot { state }`
  right after `RoomJoined` (mirrors the whiteboard).
- **R3 — Tic-Tac-Toe client.** Turn-based, so client-authoritative: the player whose
  turn it is computes the next full state (move → win/draw check → flip turn) and
  broadcasts it; everyone applies it. Start → you're X; another joins → O; others
  spectate read-only. "New game" restarts (also un-sticks a game an opponent left).
- **R4 — UI.** Opened from the ⋯ menu; a compact card with status, 3×3 grid, and a
  Start/Join/New-game action. Reset + hidden on leave. i18n ×8.

## 4. Design & Architecture

- **Server** (`protocol.rs`, `rooms.rs`, `lib.rs`): `ClientMessage::Game{state}`,
  `ServerMessage::Game{peer_id,state}` + `GameSnapshot{state}`; `Room.game:
  Option<serde_json::Value>` with `game_set`/`game_snapshot`; relay + join-snapshot in
  `lib.rs`. The server never parses the state.
- **Client** (`tictactoe.ts`, `app.ts`, `index.astro`): `TicTacToe` class holds
  `GameState { board[9], turn, status, winner?, winLine?, xId/xName, oId/oName }`,
  renders the grid, validates locally (your turn + empty cell), broadcasts the full
  state; `applyRemote` for peer updates + the snapshot. Toggle in the ⋯ menu; `game`
  icon; strings ×8.
- **Key decisions:**
  - *Game-agnostic server.* Storing/relaying an opaque `state` (not Tic-Tac-Toe types)
    keeps the server tiny and lets future mini-games reuse `Game{state}` unchanged.
  - *Client-authoritative.* Turns serialize writes (only the player on-move sends), so
    there's no contention; full-state broadcast keeps everyone in sync simply.
  - *Full state, not deltas.* A 9-cell board is tiny; sending the whole state makes
    late-join/snapshot and reconciliation trivial.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Game-agnostic relay + room state + snapshot | `protocol.rs`, `rooms.rs`, `lib.rs` |
| S1 | Tic-Tac-Toe logic + UI | `tictactoe.ts`, `index.astro` |
| S2 | ⋯ toggle + dispatch + leave reset | `app.ts` |
| S3 | Icon + i18n ×8 | `icons.ts`, `i18n.ts` |

## 6. Testing & Verification

- Server: `cargo build` + clippy clean.
- Client: `astro check` clean; **101/101** unit tests; production build OK.
- Manual: two peers play to a win/draw; a third sees the live board on join; "New
  game" restarts; leaving resets locally.

## 7. Deployment & Operations

- **Server change** (new messages + room state) → `railway up`. Client → Vercel
  autodeploy. Forward-safe (client only acts on the new message types).

## 8. Risks / Open Items

- If a player leaves mid-game the board is stuck on their turn; **New game** (open to
  anyone) recovers it. A future "forfeit on leave" could be cleaner.
- Client-authoritative → a tampered client could send a bogus state; acceptable for a
  casual in-call game.
- Future games (quiz/draw-and-guess/reaction) build on this same `Game{state}` relay.

## 9. References

- Issue: #21 (part 2)
- Files: `server/src/{protocol,rooms,lib}.rs`, `client/src/scripts/tictactoe.ts`,
  `client/src/scripts/app.ts`, `client/src/pages/index.astro`, `client/src/scripts/{icons,i18n}.ts`.
