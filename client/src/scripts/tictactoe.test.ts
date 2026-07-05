// @vitest-environment jsdom
//
// Pure helpers (winningLine / isFull / isStaleSeq / nextSeats) plus the TicTacToe
// class: seating, moves, win/draw detection, the seq-guarded applyRemote (R3.2),
// join-as-O and spectator rendering — under jsdom with the relay `send` mocked.
import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  TicTacToe,
  isFull,
  isStaleSeq,
  nextSeats,
  winningLine,
  type GameState,
} from './tictactoe';

describe('winningLine', () => {
  it('detects rows, columns and diagonals', () => {
    expect(winningLine([1, 1, 1, 0, 0, 0, 0, 0, 0])).toEqual([0, 1, 2]); // top row
    expect(winningLine([2, 0, 0, 2, 0, 0, 2, 0, 0])).toEqual([0, 3, 6]); // left col
    expect(winningLine([1, 0, 0, 0, 1, 0, 0, 0, 1])).toEqual([0, 4, 8]); // diagonal
  });
  it('returns undefined with no line', () => {
    expect(winningLine([1, 2, 1, 0, 0, 0, 0, 0, 0])).toBeUndefined();
    expect(winningLine([0, 0, 0, 0, 0, 0, 0, 0, 0])).toBeUndefined();
  });
});

describe('isFull', () => {
  it('is true only when every cell is taken', () => {
    expect(isFull([1, 2, 1, 2, 1, 2, 1, 2, 1])).toBe(true);
    expect(isFull([1, 2, 1, 2, 0, 2, 1, 2, 1])).toBe(false);
  });
});

describe('isStaleSeq', () => {
  it('drops a seq not newer than the highest seen', () => {
    expect(isStaleSeq(5, 5)).toBe(true);
    expect(isStaleSeq(3, 5)).toBe(true);
  });
  it('accepts a strictly newer seq', () => {
    expect(isStaleSeq(6, 5)).toBe(false);
  });
  it('accepts a state with no seq (older client)', () => {
    expect(isStaleSeq(undefined, 5)).toBe(false);
  });
});

const won = (xId: string, oId: string, winner: 1 | 2): GameState => ({
  board: [], turn: 1, status: 'won', winner, xId, xName: xId, oId, oName: oId, seq: 1,
});
const draw = (xId: string, oId: string): GameState => ({
  board: [], turn: 1, status: 'draw', xId, xName: xId, oId, oName: oId, seq: 1,
});

describe('nextSeats', () => {
  it('seats the starter alone when no one else is present (→ waiting)', () => {
    expect(nextSeats(['a'], null, 'a')).toEqual({ xId: 'a', oId: undefined });
    // degenerate ring where every other id is the starter itself → no O seat
    expect(nextSeats(['a', 'a'], null, 'a')).toEqual({ xId: 'a', oId: undefined });
  });

  it('fresh start: starter is X, the next participant is O (immediately playable)', () => {
    expect(nextSeats(['a', 'b'], null, 'a')).toEqual({ xId: 'a', oId: 'b' });
    expect(nextSeats(['a', 'b', 'c'], null, 'a')).toEqual({ xId: 'a', oId: 'b' });
    expect(nextSeats(['a', 'b', 'c'], null, 'b')).toEqual({ xId: 'b', oId: 'c' });
    // a spectator starting wraps around to the next participant after itself
    expect(nextSeats(['a', 'b', 'c'], null, 'c')).toEqual({ xId: 'c', oId: 'a' });
  });

  it('two players: a finished game rematches (no spectators to rotate in)', () => {
    expect(nextSeats(['a', 'b'], won('a', 'b', 1), 'a')).toEqual({ xId: 'a', oId: 'b' });
    expect(nextSeats(['a', 'b'], won('a', 'b', 2), 'b')).toEqual({ xId: 'b', oId: 'a' });
  });

  it('three players: winner stays, the loser sits out and the spectator rotates in', () => {
    // a beat b → a stays, c (the spectator) comes in, b waits.
    expect(nextSeats(['a', 'b', 'c'], won('a', 'b', 1), 'a')).toEqual({ xId: 'a', oId: 'c' });
    // o (b) won → b stays, the next after loser a is b(skip)→c.
    expect(nextSeats(['a', 'b', 'c'], won('a', 'b', 2), 'b')).toEqual({ xId: 'b', oId: 'c' });
  });

  it('draw keeps X and rotates the next participant after O in', () => {
    expect(nextSeats(['a', 'b', 'c'], draw('a', 'b'), 'a')).toEqual({ xId: 'a', oId: 'c' });
  });

  it('re-seats current players after one leaves and rejoins (no ghost seat)', () => {
    // Winner A still here, loser B left and rejoined as "bb" → A keeps X, bb takes O
    // (the stale 'b' id is never seated).
    expect(nextSeats(['a', 'bb'], won('a', 'b', 1), 'a')).toEqual({ xId: 'a', oId: 'bb' });
    // The winner itself left and rejoined → its old id is gone, so fall back to a
    // fresh seating from whoever is present now.
    expect(nextSeats(['bb', 'c'], won('a', 'b', 1), 'bb')).toEqual({ xId: 'bb', oId: 'c' });
    // A stuck (still 'playing') game whose O dropped → New Game seats current players.
    const stuck: GameState = { board: Array(9).fill(0), turn: 2, status: 'playing', xId: 'a', xName: 'a', oId: 'gone', oName: 'gone', seq: 5 };
    expect(nextSeats(['a', 'bb'], stuck, 'a')).toEqual({ xId: 'a', oId: 'bb' });
  });

  it('four players: winner stays, the participant after the loser rotates in', () => {
    // X=a beat O=b → a stays, the participant after loser b (index 1) is c → c in, b & d wait.
    expect(nextSeats(['a', 'b', 'c', 'd'], won('a', 'b', 1), 'a')).toEqual({ xId: 'a', oId: 'c' });
    // O=c beat X=a → c stays, the participant after loser a (index 0) is b → b in, a & d wait.
    expect(nextSeats(['a', 'b', 'c', 'd'], won('a', 'c', 2), 'c')).toEqual({ xId: 'c', oId: 'b' });
  });
});

// ---- TicTacToe class (jsdom) ------------------------------------------------------

const FIXTURE = `
  <div id="mg-root">
    <p id="mg-status"></p>
    <div id="mg-board"></div>
    <button id="mg-action"></button>
  </div>`;

// Translate stub keeping the {n} placeholders observable.
const tt = (k: string): string => (k === 'mgTurn' ? 'turn:{n}' : k === 'mgWin' ? 'win:{n}' : k);

function makeGame(opts: { peers?: string[] } = {}) {
  document.body.innerHTML = FIXTURE;
  const root = document.getElementById('mg-root') as HTMLElement;
  const send = vi.fn();
  const nameOf = (id: string): string => `N(${id})`;
  const peers = opts.peers;
  const game =
    peers != null
      ? new TicTacToe(root, 'me', nameOf, send, tt, () => peers)
      : new TicTacToe(root, 'me', nameOf, send, tt); // default peers = just me
  return {
    game,
    send,
    cells: [...root.querySelectorAll('.mg-cell')] as HTMLButtonElement[],
    status: root.querySelector('#mg-status') as HTMLElement,
    action: root.querySelector('#mg-action') as HTMLButtonElement,
    lastSent: () => send.mock.calls.at(-1)?.[0] as GameState,
  };
}

const playing = (over: Partial<GameState> = {}): GameState => ({
  board: Array(9).fill(0) as number[],
  turn: 1,
  status: 'playing',
  xId: 'p1',
  xName: 'P1',
  oId: 'p2',
  oName: 'P2',
  seq: 1,
  ...over,
});

afterEach(() => {
  vi.restoreAllMocks();
  document.body.innerHTML = '';
});

describe('TicTacToe — initial render', () => {
  it('boots to an empty, locked board with a Start action', () => {
    const h = makeGame();
    expect(h.cells).toHaveLength(9);
    expect(h.cells.every((c) => c.disabled && c.textContent === '')).toBe(true);
    expect(h.status.textContent).toBe('');
    expect(h.action.textContent).toBe('mgStart');
    expect(h.action.hidden).toBe(false);
    h.cells[0].click(); // no game yet → no-op
    expect(h.send).not.toHaveBeenCalled();
  });
});

describe('TicTacToe — starting', () => {
  it('solo start (default peers) waits for an opponent', () => {
    const h = makeGame();
    h.action.click();
    const s = h.lastSent();
    expect(s.status).toBe('waiting');
    expect(s.xId).toBe('me');
    expect(s.xName).toBe('N(me)');
    expect(s.oId).toBeUndefined();
    expect(s.seq).toBe(1);
    expect(h.status.textContent).toBe('mgWaiting'); // I can't join my own game
    expect(h.action.textContent).toBe('mgNew');
    expect(h.cells.every((c) => c.disabled)).toBe(true);
    h.action.click(); // as X of a waiting game → restart, seq keeps climbing
    expect(h.lastSent().seq).toBe(2);
    expect(h.lastSent().status).toBe('waiting');
  });

  it('with an opponent present the board is immediately playable (R3.1)', () => {
    const h = makeGame({ peers: ['me', 'p2'] });
    h.action.click();
    const s = h.lastSent();
    expect(s.status).toBe('playing');
    expect(s.xId).toBe('me');
    expect(s.oId).toBe('p2');
    expect(s.oName).toBe('N(p2)');
    expect(h.status.textContent).toBe('mgYourTurn');
    expect(h.cells.every((c) => !c.disabled)).toBe(true); // my turn, all empty
  });
});

describe('TicTacToe — playing to a result', () => {
  it('plays a full game to an X win, then rematches with a monotonic seq', () => {
    const h = makeGame({ peers: ['me', 'p2'] });
    h.action.click();
    let cur = h.lastSent();
    const opp = (i: number): void => {
      const board = cur.board.slice();
      board[i] = 2;
      cur = { ...cur, board, turn: 1, seq: cur.seq + 1 };
      h.game.applyRemote(cur);
    };
    const mine = (i: number): void => {
      h.cells[i].click();
      cur = h.lastSent();
    };

    mine(0);
    expect(h.cells[0].textContent).toBe('✕');
    expect(h.cells[0].classList.contains('x')).toBe(true);
    expect(h.status.textContent).toBe('turn:N(p2)');
    expect(h.cells.every((c) => c.disabled)).toBe(true); // not my turn

    const sends = h.send.mock.calls.length;
    h.cells[1].click(); // not my turn → no-op
    expect(h.send.mock.calls.length).toBe(sends);

    opp(3);
    expect(h.cells[3].textContent).toBe('◯');
    expect(h.cells[3].classList.contains('o')).toBe(true);
    expect(h.status.textContent).toBe('mgYourTurn');
    h.cells[3].click(); // occupied → no-op
    expect(h.send.mock.calls.length).toBe(sends);

    mine(1);
    opp(4);
    mine(2); // top row → X wins
    expect(cur.status).toBe('won');
    expect(cur.winner).toBe(1);
    expect(cur.winLine).toEqual([0, 1, 2]);
    expect(h.status.textContent).toBe('win:you');
    for (const i of [0, 1, 2]) expect(h.cells[i].classList.contains('win')).toBe(true);
    expect(h.cells.every((c) => c.disabled)).toBe(true);
    expect(h.action.textContent).toBe('mgNew');

    // Rematch: two players → same seats; seq stays monotonic across games (R3.2).
    h.action.click();
    const next = h.lastSent();
    expect(next.status).toBe('playing');
    expect(next.xId).toBe('me');
    expect(next.oId).toBe('p2');
    expect(next.board).toEqual(Array(9).fill(0));
    expect(next.seq).toBe(7); // 1 start + 3 mine + 2 opp + 1 rematch
  });

  it('ends in a draw when the board fills with no line', () => {
    const h = makeGame({ peers: ['me', 'p2'] });
    h.action.click();
    let cur = h.lastSent();
    const opp = (i: number): void => {
      const board = cur.board.slice();
      board[i] = 2;
      cur = { ...cur, board, turn: 1, seq: cur.seq + 1 };
      h.game.applyRemote(cur);
    };
    const mine = (i: number): void => {
      h.cells[i].click();
      cur = h.lastSent();
    };

    mine(0); opp(1); mine(2); opp(4); mine(3); opp(5); mine(7); opp(6); mine(8);
    expect(cur.status).toBe('draw');
    expect(cur.winner).toBeUndefined();
    expect(h.status.textContent).toBe('mgDraw');
    expect(h.action.textContent).toBe('mgNew');
  });
});

describe('TicTacToe — joining an open seat', () => {
  it('a second player claims the free O seat and can then move', () => {
    const h = makeGame();
    const revealed = h.game.applyRemote(
      playing({ status: 'waiting', oId: undefined, oName: undefined, seq: 3 }),
    );
    expect(revealed).toBe(true); // game appeared → open the panel
    expect(h.status.textContent).toBe('turn:P1'); // join prompt names the waiting X
    expect(h.action.textContent).toBe('mgJoin');

    h.action.click(); // join as O
    const joined = h.lastSent();
    expect(joined.status).toBe('playing');
    expect(joined.oId).toBe('me');
    expect(joined.oName).toBe('N(me)');
    expect(joined.seq).toBe(4); // stamped past the highest seen (3)
    expect(h.status.textContent).toBe('turn:P1'); // X moves first

    // X moves, then it's my (O's) turn.
    const board = joined.board.slice();
    board[0] = 1;
    h.game.applyRemote({ ...joined, board, turn: 2, seq: 5 });
    expect(h.status.textContent).toBe('mgYourTurn');
    h.cells[1].click();
    expect(h.cells[1].textContent).toBe('◯');
    expect(h.lastSent().turn).toBe(1);
    expect(h.lastSent().seq).toBe(6);
  });
});

describe('TicTacToe — spectators and finished games', () => {
  it('spectators see the board read-only with a New game action (R3.3)', () => {
    const h = makeGame();
    expect(h.game.applyRemote(playing())).toBe(true);
    expect(h.cells.every((c) => c.disabled)).toBe(true); // mark 0 never has a turn
    expect(h.status.textContent).toBe('turn:P1');
    expect(h.action.textContent).toBe('mgNew');
    expect(h.action.hidden).toBe(false);
    h.cells[0].click(); // spectators can't move
    expect(h.send).not.toHaveBeenCalled();

    // O's turn, and a missing oName renders as empty
    expect(h.game.applyRemote(playing({ turn: 2, seq: 2 }))).toBe(false); // mid-game update
    expect(h.status.textContent).toBe('turn:P2');
    h.game.applyRemote(playing({ turn: 2, oName: undefined, seq: 3 }));
    expect(h.status.textContent).toBe('turn:');
  });

  it('renders won/draw endings with the right name (or "you")', () => {
    const h = makeGame();
    h.game.applyRemote(
      playing({ status: 'won', winner: 1, winLine: [0, 1, 2], board: [1, 1, 1, 2, 2, 0, 0, 0, 0] }),
    );
    expect(h.status.textContent).toBe('win:P1');
    expect(h.cells[0].classList.contains('win')).toBe(true);
    expect(h.cells[8].classList.contains('win')).toBe(false);

    // after a finished game a new state is revealed again
    expect(
      h.game.applyRemote(
        playing({ status: 'won', winner: 2, winLine: [3, 4, 5], board: [1, 1, 0, 2, 2, 2, 1, 0, 0], seq: 2 }),
      ),
    ).toBe(true);
    expect(h.status.textContent).toBe('win:P2');

    h.game.applyRemote(playing({ status: 'draw', board: [1, 2, 1, 1, 2, 2, 2, 1, 1], seq: 3 }));
    expect(h.status.textContent).toBe('mgDraw');
  });
});

describe('TicTacToe — applyRemote guards (R3.2)', () => {
  it('rejects malformed frames without touching the board', () => {
    const h = makeGame();
    h.game.applyRemote(playing({ seq: 5 }));
    expect(h.game.applyRemote({ board: [0, 0, 0], turn: 1, status: 'playing', seq: 9 })).toBe(false);
    expect(h.game.applyRemote({ board: 'nope', turn: 1, status: 'playing', seq: 9 })).toBe(false);
    expect(h.status.textContent).toBe('turn:P1'); // unchanged
  });

  it('drops stale or duplicate seqs so the board cannot roll back', () => {
    const h = makeGame();
    h.game.applyRemote(playing({ seq: 5 }));
    expect(h.game.applyRemote(playing({ turn: 2, seq: 5 }))).toBe(false); // duplicate
    expect(h.game.applyRemote(playing({ turn: 2, seq: 4 }))).toBe(false); // reordered
    expect(h.status.textContent).toBe('turn:P1'); // still X's turn
  });

  it('accepts a frame with no seq at all (older client during rollout)', () => {
    const h = makeGame();
    h.game.applyRemote(playing({ seq: 5 }));
    const noSeq: Record<string, unknown> = { ...playing({ turn: 2 }) };
    delete noSeq.seq;
    h.game.applyRemote(noSeq);
    expect(h.status.textContent).toBe('turn:P2'); // applied despite the missing seq
  });

  it('a null or non-object state clears the board (game ended)', () => {
    const h = makeGame();
    h.game.applyRemote(playing());
    expect(h.game.applyRemote(null)).toBe(false);
    expect(h.status.textContent).toBe('');
    expect(h.action.textContent).toBe('mgStart');
    h.game.applyRemote(playing({ seq: 2 }));
    expect(h.game.applyRemote(42)).toBe(false);
    expect(h.action.textContent).toBe('mgStart');
  });
});

describe('TicTacToe — reset', () => {
  it('drops the game locally without broadcasting', () => {
    const h = makeGame();
    h.game.applyRemote(playing());
    h.game.reset();
    expect(h.status.textContent).toBe('');
    expect(h.action.textContent).toBe('mgStart');
    expect(h.cells.every((c) => c.textContent === '' && c.disabled)).toBe(true);
    expect(h.send).not.toHaveBeenCalled();
  });
});
