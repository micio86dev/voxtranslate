// Tic-Tac-Toe mini-game (spec 0046). Turn-based, so it's client-authoritative: the
// player whose turn it is computes the next state and broadcasts the FULL state via
// the generic `game` relay; everyone else applies it. The server keeps the latest
// state and replays it to late-joiners (spectators). 2 players (X/O); a 3rd/4th in
// the call watch read-only.

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
}

const LINES = [
  [0, 1, 2], [3, 4, 5], [6, 7, 8], // rows
  [0, 3, 6], [1, 4, 7], [2, 5, 8], // cols
  [0, 4, 8], [2, 4, 6], // diagonals
];

export class TicTacToe {
  private state: GameState | null = null;
  private cells: HTMLButtonElement[] = [];

  constructor(
    private root: HTMLElement,
    private myId: string,
    private nameOf: (id: string) => string,
    private send: (state: unknown) => void,
    private t: (k: string) => string,
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
    const win = LINES.find((l) => board[l[0]] && board[l[0]] === board[l[1]] && board[l[1]] === board[l[2]]);
    const full = board.every((v) => v !== 0);
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
    // A waiting game with a free O slot → join as O (if I'm not already X).
    if (s.status === 'waiting' && !s.oId && this.myId !== s.xId) {
      this.setState({ ...s, oId: this.myId, oName: this.nameOf(this.myId), status: 'playing' });
      return;
    }
    // Otherwise (finished, or a stuck game) → start a fresh one.
    this.startNew();
  }

  private startNew(): void {
    this.setState({
      board: Array(9).fill(0),
      turn: 1,
      status: 'waiting',
      xId: this.myId,
      xName: this.nameOf(this.myId),
    });
  }

  /** Update locally + broadcast the full state. */
  private setState(s: GameState | null): void {
    this.state = s;
    this.render();
    this.send(s ?? null);
  }

  /** Apply a peer's broadcast / the join snapshot (no re-broadcast). */
  applyRemote(state: unknown): void {
    this.state = (state && typeof state === 'object' ? (state as GameState) : null) || null;
    this.render();
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
