import { describe, expect, it } from 'vitest';

import { formatClock, parseTimerCommand, spokenDuration } from './timer-intent';

describe('parseTimerCommand — Italian (MVP)', () => {
  it('parses "imposta timer di X minuti" with digits', () => {
    expect(parseTimerCommand('imposta timer di 10 minuti')).toEqual({ seconds: 600, isBreak: false });
  });

  it('parses the article variant "imposta un timer di 15 minuti"', () => {
    expect(parseTimerCommand('imposta un timer di 15 minuti')?.seconds).toBe(900);
  });

  it('parses "fai partire un timer di 5 minuti"', () => {
    expect(parseTimerCommand('Fai partire un timer di 5 minuti')?.seconds).toBe(300);
  });

  it('parses spoken number words ("dieci minuti")', () => {
    expect(parseTimerCommand('timer di dieci minuti')?.seconds).toBe(600);
  });

  it('parses "mezz\'ora" and "mezzora" idioms to 30 minutes', () => {
    expect(parseTimerCommand("imposta timer di mezz'ora")?.seconds).toBe(1800);
    expect(parseTimerCommand('timer di mezzora')?.seconds).toBe(1800);
  });

  it('parses "un quarto d\'ora" to 15 minutes', () => {
    expect(parseTimerCommand("metti un timer di un quarto d'ora")?.seconds).toBe(900);
  });

  it("parses \"un'ora\" to 60 minutes", () => {
    expect(parseTimerCommand("imposta un timer di un'ora")?.seconds).toBe(3600);
  });

  it('parses "un minuto" (implicit one)', () => {
    expect(parseTimerCommand('imposta timer di un minuto')?.seconds).toBe(60);
  });

  it('flags a "pausa" as a break', () => {
    expect(parseTimerCommand('imposta una pausa di 10 minuti')).toEqual({ seconds: 600, isBreak: true });
  });

  it('parses seconds ("90 secondi")', () => {
    expect(parseTimerCommand('imposta timer di 90 secondi')?.seconds).toBe(90);
  });
});

describe('parseTimerCommand — English', () => {
  it('parses "set a 5 minute timer"', () => {
    expect(parseTimerCommand('set a 5 minute timer')?.seconds).toBe(300);
  });

  it('parses "start a 10 minute timer"', () => {
    expect(parseTimerCommand('start a 10 minute timer')?.seconds).toBe(600);
  });

  it('parses "timer for 15 minutes"', () => {
    expect(parseTimerCommand('timer for 15 minutes')?.seconds).toBe(900);
  });

  it('flags "set a 20 minute break" as a break', () => {
    expect(parseTimerCommand('set a 20 minute break')).toEqual({ seconds: 1200, isBreak: true });
  });

  it('parses spoken words ("five minute timer")', () => {
    expect(parseTimerCommand('set a five minute timer')?.seconds).toBe(300);
  });

  it('parses tens+unit compounds ("twenty five minute timer")', () => {
    expect(parseTimerCommand('set a twenty five minute timer')?.seconds).toBe(1500);
  });

  it('parses "half an hour" / "quarter hour" idioms', () => {
    expect(parseTimerCommand('set a timer for half an hour')?.seconds).toBe(1800);
    expect(parseTimerCommand('set a quarter hour timer')?.seconds).toBe(900);
  });

  it('parses seconds ("30 second timer")', () => {
    expect(parseTimerCommand('set a 30 second timer')?.seconds).toBe(30);
  });

  it('sums mixed hour + minute components', () => {
    expect(parseTimerCommand('set a timer for 1 hour 30 minutes')?.seconds).toBe(5400);
  });

  it('parses "a minute" (implicit one)', () => {
    expect(parseTimerCommand('set a timer for a minute')?.seconds).toBe(60);
  });
});

describe('parseTimerCommand — gating & guards (no false positives)', () => {
  it('ignores empty / blank input', () => {
    expect(parseTimerCommand('')).toBeNull();
    expect(parseTimerCommand('   ')).toBeNull();
  });

  it('requires a trigger keyword — plain talk about minutes is not a command', () => {
    expect(parseTimerCommand('the meeting runs for 30 minutes')).toBeNull();
    expect(parseTimerCommand('how many minutes are in an hour')).toBeNull();
  });

  it('requires a duration — a bare trigger is not a command', () => {
    expect(parseTimerCommand('set a timer')).toBeNull();
    expect(parseTimerCommand("let's take a break")).toBeNull();
    expect(parseTimerCommand('ho perso il timer')).toBeNull();
  });

  it('rejects a zero / non-positive duration', () => {
    expect(parseTimerCommand('imposta timer di 0 minuti')).toBeNull();
  });

  it('rejects an absurdly long duration (mis-parse guard)', () => {
    expect(parseTimerCommand('set a 10 hour timer')).toBeNull();
  });

  it('accepts the 6-hour boundary but not beyond', () => {
    expect(parseTimerCommand('set a 6 hour timer')?.seconds).toBe(21600);
    expect(parseTimerCommand('set a 7 hour timer')).toBeNull();
  });
});

describe('formatClock', () => {
  it('formats sub-hour durations as MM:SS', () => {
    expect(formatClock(0)).toBe('00:00');
    expect(formatClock(5)).toBe('00:05');
    expect(formatClock(65)).toBe('01:05');
    expect(formatClock(600)).toBe('10:00');
  });

  it('formats hour+ durations as H:MM:SS', () => {
    expect(formatClock(3600)).toBe('1:00:00');
    expect(formatClock(3661)).toBe('1:01:01');
    expect(formatClock(5400)).toBe('1:30:00');
  });

  it('never goes negative', () => {
    expect(formatClock(-10)).toBe('00:00');
  });
});

describe('spokenDuration', () => {
  // Stand-in i18n: returns the English unit words so assertions stay readable.
  const words: Record<string, string> = {
    unitH1: 'hour', unitHN: 'hours',
    unitM1: 'minute', unitMN: 'minutes',
    unitS1: 'second', unitSN: 'seconds',
  };
  const t = (k: string): string => words[k] ?? k;

  it('uses plural / singular unit words', () => {
    expect(spokenDuration(600, t)).toBe('10 minutes');
    expect(spokenDuration(60, t)).toBe('1 minute');
    expect(spokenDuration(3600, t)).toBe('1 hour');
    expect(spokenDuration(30, t)).toBe('30 seconds');
  });

  it('joins multiple components', () => {
    expect(spokenDuration(90, t)).toBe('1 minute 30 seconds');
    expect(spokenDuration(5400, t)).toBe('1 hour 30 minutes');
  });

  it('falls back to "0 seconds" for an empty duration', () => {
    expect(spokenDuration(0, t)).toBe('0 seconds');
  });
});
