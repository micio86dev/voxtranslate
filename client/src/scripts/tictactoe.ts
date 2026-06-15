// Tic-Tac-Toe mini-game (spec 0046, lifecycle fixes in spec 0070 S3). Turn-based, so
// it's client-authoritative: the player whose turn it is computes the next state and
// broadcasts the FULL state via the generic `game` relay; everyone else applies it.
// The server keeps the latest state and replays it to late-joiners (spectators).
//
// Spec 0070 changes:
//  - R3.1 "New Game" seats an opponent immediately when one is present (no manual
//    Join step), so the board is playable at once instead of stuck on `waiting`.
//  - R3.2 every broadcast carries a monotonic `seq`; stale/out-of-order frames are
//    dropped so clients can't diverge.
//  - R3.3/R3.4 exactly two seats (X/O); everyone else spectates read-only, and a new
//    game rotates a waiting spectator in (winner stays, loser sits out).

export interface GameState {
  board: number[]; // 9 cells: 0 empty, 1 = X, 2 = O
  turn: 1 | 2;
  status: 'waiting' | 'playing' | 'won' | 'draw';
  winner?: 1 | 2;
  winLine?: number[]; // 3 indices to highlight
  xId: string;
  xName: string;
  oId?: string;
  oName?: string;
  /** Monotonic sequence number; guards against stale/out-of-order applies (R3.2). */
  seq: number;
}

export const LINES = [
  [0, 1, 2], [3, 4, 5], [6, 7, 8], // rows
  [0, 3, 6], [1, 4, 7], [2, 5, 8], // cols
  [0, 4, 8], [2, 4, 6], // diagonals
];

/** The first winning line on the board, or undefined. */
export function winningLine(board: number[]): number[] | undefined {
  return LINES.find((l) => board[l[0]] && board[l[0]] === board[l[1]] && board[l[1]] === board[l[2]]);
}

/** Every cell filled. */
export function isFull(board: number[]): boolean {
  return board.every((v) => v !== 0);
}

/** Drop an incoming state whose seq is not newer than the highest we've seen, so a
 *  duplicate / reordered frame can't roll the board back (R3.2). Missing seq (an
 *  older client during rollout) is always accepted. */
export function isStaleSeq(incoming: number | undefined, highestSeen: number): boolean {
  if (incoming == null) return false;
  return incoming <= highestSeen;
}

/** First id cyclically after index `from` that isn't `exclude`. */
function nextOther(parts: string[], from: number, exclude: string): string | undefined {
  for (let k = 1; k <= parts.length; k++) {
    const cand = parts[(from + k) % parts.length];
    if (cand !== exclude) return cand;
  }
  return undefined;
}

/**
 * Pick the two seats (X then O) for a new game from the live participant ring
 * (R3.3/R3.4). A fresh start seats the starter as X and the next participant as O.
 * After a finished game the winner (or X on a draw) keeps a seat and the participant
 * cyclically after the loser rotates in — so with >2 players, spectators cycle
 * through and the loser sits out; with exactly 2 it's a rematch. `oId` is undefined
 * only when the starter is alone (→ a `waiting` board).
 */
export function nextSeats(
  parts: string[],
  prev: GameState | null,
  starterId: string,
): { xId: string; oId?: string } {
  if (parts.length <= 1) return { xId: parts[0] ?? starterId };
  // Rotation only if the previous game finished AND the seat we'd keep is still in
  // the call. A player who left and rejoined gets a NEW peer id, so a stale winner/
  // loser id from `prev` must never be seated — otherwise the board gets a "ghost"
  // seat nobody controls and the two present players can't actually play (the
  // reported leave/rejoin bug). When the kept seat is gone, fall through to a fresh
  // seating from the CURRENT participants.
  if (prev && (prev.status === 'won' || prev.status === 'draw')) {
    const keep = prev.status === 'won' ? (prev.winner === 1 ? prev.xId : prev.oId) : prev.xId;
    if (keep && parts.includes(keep)) {
      const loser = keep === prev.xId ? prev.oId : prev.xId;
      // If the loser also left, rotate from the kept seat's position instead.
      const from = loser != null && parts.indexOf(loser) >= 0 ? parts.indexOf(loser) : parts.indexOf(keep);
      return { xId: keep, oId: nextOther(parts, from, keep) };
    }
    // kept seat gone → fresh seating below
  }
  const starter = parts.includes(starterId) ? starterId : parts[0];
  const from = parts.indexOf(starter);
  return { xId: starter, oId: nextOther(parts, from, starter) };
}

export class TicTacToe {
  private state: GameState | null = null;
  private cells: HTMLButtonElement[] = [];
  /** Highest seq produced or seen — keeps our broadcasts monotonic even across
   *  resets, and drives the stale-frame guard (R3.2). */
  private seq = 0;

  constructor(
    private root: HTMLElement,
    private myId: string,
    private nameOf: (id: string) => string,
    private send: (state: unknown) => void,
    private t: (k: string) => string,
    /** All participant ids in the call (self + peers), in a stable local order. */
    private peers: () => string[] = () => [myId],
  ) {
    const grid = root.querySelector('#mg-board') as HTMLElement;
    for (let i = 0; i < 9; i++) {
      const c = document.createElement('button');
      c.className = 'mg-cell';
      c.dataset.cell = String(i);
      c.addEventListener('click', () => this.onCell(i));
      grid.appendChild(c);
      this.cells.push(c);
    }
    (root.querySelector('#mg-action') as HTMLButtonElement).addEventListener('click', () =>
      this.onAction(),
    );
    this.render();
  }

  /** My mark this game: 1 (X), 2 (O), or 0 (spectator). */
  private mark(): 0 | 1 | 2 {
    if (!this.state) return 0;
    if (this.myId === this.state.xId) return 1;
    if (this.myId === this.state.oId) return 2;
    return 0;
  }

  private onCell(i: number): void {
    const s = this.state;
    if (!s || s.status !== 'playing' || s.turn !== this.mark() || s.board[i] !== 0) return;
    const board = s.board.slice();
    board[i] = this.mark();
    const win = winningLine(board);
    const full = isFull(board);
    const next: GameState = {
      ...s,
      board,
      turn: s.turn === 1 ? 2 : 1,
      status: win ? 'won' : full ? 'draw' : 'playing',
      winner: win ? (this.mark() as 1 | 2) : undefined,
      winLine: win || undefined,
    };
    this.setState(next);
  }

  private onAction(): void {
    const s = this.state;
    if (!s) return this.startNew();
    // A waiting game with a free O slot → join as O (if I'm not already X). This
    // covers the solo-start case: a second player claims the open seat.
    if (s.status === 'waiting' && !s.oId && this.myId !== s.xId) {
      this.setState({ ...s, oId: this.myId, oName: this.nameOf(this.myId), status: 'playing' });
      return;
    }
    // Otherwise (finished, or a stuck game) → start a fresh one.
    this.startNew();
  }

  private startNew(): void {
    // Seat from the live participant list: with an opponent present the board is
    // immediately playable (R3.1); after a finish, spectators rotate in (R3.4).
    const { xId, oId } = nextSeats(this.peers(), this.state, this.myId);
    this.setState({
      board: Array(9).fill(0),
      turn: 1,
      status: oId ? 'playing' : 'waiting',
      xId,
      xName: this.nameOf(xId),
      oId,
      oName: oId ? this.nameOf(oId) : undefined,
      seq: 0, // stamped by setState
    });
  }

  /** Update locally + broadcast the full state, stamped with the next seq. */
  private setState(s: GameState | null): void {
    if (s) {
      this.seq += 1;
      s.seq = this.seq;
    }
    this.state = s;
    this.render();
    this.send(s ?? null);
  }

  /**
   * Apply a peer's broadcast / the join snapshot (no re-broadcast). Drops stale or
   * out-of-order frames (R3.2). Returns true when the panel should be revealed —
   * i.e. a game just appeared (fresh start, or a late-joiner's snapshot) — so the
   * app can open the mini-game for spectators/opponents (R3.3).
   */
  applyRemote(state: unknown): boolean {
    const incoming = state && typeof state === 'object' ? (state as GameState) : null;
    // Reject malformed frames (R3.2): a non-9-cell board would crash render and
    // desync everyone. A null state (game ended) is valid and clears the board.
    if (incoming && (!Array.isArray(incoming.board) || incoming.board.length !== 9)) return false;
    if (incoming && isStaleSeq(incoming.seq, this.seq)) return false; // stale / reordered
    const prev = this.state;
    this.state = incoming;
    if (incoming?.seq != null) this.seq = Math.max(this.seq, incoming.seq);
    this.render();
    // Reveal when a game becomes present from nothing or after a finished round.
    return !!incoming && (!prev || prev.status === 'won' || prev.status === 'draw');
  }

  /** Drop the game locally (leaving the call) without touching the server. */
  reset(): void {
    this.state = null;
    this.render();
  }

  private render(): void {
    const s = this.state;
    const statusEl = this.root.querySelector('#mg-status') as HTMLElement;
    const actionEl = this.root.querySelector('#mg-action') as HTMLButtonElement;
    const mark = this.mark();

    this.cells.forEach((c, i) => {
      const v = s?.board[i] ?? 0;
      c.textContent = v === 1 ? '✕' : v === 2 ? '◯' : '';
      c.classList.toggle('x', v === 1);
      c.classList.toggle('o', v === 2);
      c.classList.toggle('win', !!s?.winLine?.includes(i));
      const myTurn = !!s && s.status === 'playing' && s.turn === mark; // mark 0 (spectator) never equals turn (1|2)
      c.disabled = !(myTurn && v === 0);
    });

    if (!s) {
      statusEl.textContent = '';
      actionEl.textContent = this.t('mgStart');
      actionEl.hidden = false;
      return;
    }
    if (s.status === 'waiting') {
      const canJoin = !s.oId && this.myId !== s.xId;
      statusEl.textContent = canJoin
        ? this.t('mgTurn').replace('{n}', s.xName)
        : this.t('mgWaiting');
      actionEl.textContent = canJoin ? this.t('mgJoin') : this.t('mgNew');
      actionEl.hidden = false;
    } else if (s.status === 'playing') {
      const turnId = s.turn === 1 ? s.xId : s.oId;
      const turnName = s.turn === 1 ? s.xName : s.oName || '';
      statusEl.textContent =
        turnId === this.myId ? this.t('mgYourTurn') : this.t('mgTurn').replace('{n}', turnName);
      // Spectators (mark 0) can restart; players use New game after a finish.
      actionEl.textContent = this.t('mgNew');
      actionEl.hidden = false;
    } else {
      // finished
      if (s.status === 'draw') {
        statusEl.textContent = this.t('mgDraw');
      } else {
        const wid = s.winner === 1 ? s.xId : s.oId;
        const wname = wid === this.myId ? this.t('you') : s.winner === 1 ? s.xName : s.oName || '';
        statusEl.textContent = this.t('mgWin').replace('{n}', wname);
      }
      actionEl.textContent = this.t('mgNew');
      actionEl.hidden = false;
    }
  }
}
