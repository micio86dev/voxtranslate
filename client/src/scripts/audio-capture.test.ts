import { describe, it, expect, vi, beforeEach } from 'vitest';
import { AudioCapture } from './audio-capture';

let lastRecorder: any;

class FakeRecorder {
  static supported = true;
  static shouldThrow = false;
  static isTypeSupported() {
    return FakeRecorder.supported;
  }
  state = 'inactive';
  ondataavailable: any = () => {};
  onstop: any = () => {};
  constructor(
    public stream: any,
    public opts: any,
  ) {
    if (FakeRecorder.shouldThrow) throw new Error('unsupported');
    lastRecorder = this;
  }
  start(_t: number) {
    void _t;
    this.state = 'recording';
  }
  stop() {
    this.state = 'inactive';
    this.onstop();
  }
}
(globalThis as any).MediaRecorder = FakeRecorder;
(globalThis as any).WebSocket = { OPEN: 1 };
(globalThis as any).MediaStream = class {
  constructor(public tracks: any[]) {}
};

const fakeWs = (open = true) => ({ readyState: open ? 1 : 0, send: vi.fn() }) as any;
const fakeStream = (audio = true) => ({ getAudioTracks: () => (audio ? [{ kind: 'audio' }] : []) }) as any;

describe('AudioCapture', () => {
  beforeEach(() => {
    FakeRecorder.supported = true;
    FakeRecorder.shouldThrow = false;
    lastRecorder = undefined;
  });

  it('start streams chunks and sends control frames; stop flushes', () => {
    const ws = fakeWs();
    const ac = new AudioCapture(fakeStream(), ws);
    ac.start();
    expect(ws.send).toHaveBeenCalledWith(JSON.stringify({ type: 'start' }));
    expect(lastRecorder.state).toBe('recording');

    lastRecorder.ondataavailable({ data: { size: 10 } });
    expect(ws.send).toHaveBeenCalledWith({ size: 10 });

    const calls = ws.send.mock.calls.length;
    lastRecorder.ondataavailable({ data: { size: 0 } }); // empty → not sent
    expect(ws.send.mock.calls.length).toBe(calls);

    ac.start(); // already active → no-op
    ac.stop();
    expect(ws.send).toHaveBeenCalledWith(JSON.stringify({ type: 'stop' }));
  });

  it('handles no audio track, unsupported mime, constructor failure, setMuted/setStream', () => {
    const ws = fakeWs();

    const ac0 = new AudioCapture(fakeStream(false), ws);
    ac0.start();
    expect(lastRecorder).toBeUndefined(); // nothing recorded without an audio track

    FakeRecorder.supported = false;
    const ac = new AudioCapture(fakeStream(), ws);
    ac.start();
    expect(lastRecorder.opts.mimeType).toBe('audio/webm'); // fallback mime
    ac.setMuted(true); // stop
    ac.setMuted(false); // start again
    expect(lastRecorder.state).toBe('recording');

    FakeRecorder.shouldThrow = true;
    const ac2 = new AudioCapture(fakeStream(), ws);
    ac2.start(); // constructor throws → swallowed
    ac2.setStream(fakeStream());
  });

  it('emits stop synchronously and drops the trailing chunk (race-free swap)', () => {
    const ws = fakeWs();
    const ac = new AudioCapture(fakeStream(), ws);
    ac.start();
    const rec = lastRecorder;
    ac.stop();
    // 'start' then 'stop', both before stop() returned (no async onstop) — so a
    // capture swap's following start() can never overtake this 'stop' on the wire.
    const frames = ws.send.mock.calls
      .map((c: any[]) => c[0])
      .filter((x: any) => typeof x === 'string');
    expect(frames).toEqual([JSON.stringify({ type: 'start' }), JSON.stringify({ type: 'stop' })]);
    // A late chunk from the stopped recorder must not reach the socket.
    const before = ws.send.mock.calls.length;
    rec.ondataavailable?.({ data: { size: 99 } });
    expect(ws.send.mock.calls.length).toBe(before);
  });

  it('restart() puts stop before start on the wire', () => {
    const ws = fakeWs();
    const ac = new AudioCapture(fakeStream(), ws);
    ac.start();
    ws.send.mockClear();
    ac.restart();
    const frames = ws.send.mock.calls
      .map((c: any[]) => c[0])
      .filter((x: any) => typeof x === 'string');
    expect(frames).toEqual([JSON.stringify({ type: 'stop' }), JSON.stringify({ type: 'start' })]);
    expect(lastRecorder.state).toBe('recording');
  });

  it('does not send control when the socket is closed', () => {
    const ws = fakeWs(false);
    const ac = new AudioCapture(fakeStream(), ws);
    ac.start();
    expect(ws.send).not.toHaveBeenCalled();
  });

  it('stop before start and stop on an already-inactive recorder are safe', () => {
    const ws = fakeWs();
    const ac = new AudioCapture(fakeStream(), ws);
    ac.stop(); // never started → no recorder, no control frame
    expect(ws.send).not.toHaveBeenCalled();

    ac.start();
    const rec = lastRecorder;
    rec.state = 'inactive'; // recorder died on its own
    rec.stop = vi.fn();
    ac.stop();
    expect(rec.stop).not.toHaveBeenCalled(); // no double-stop on a dead recorder
    expect(ws.send).toHaveBeenCalledWith(JSON.stringify({ type: 'stop' })); // server still told
  });

  it('drops chunks when the socket closed mid-capture', () => {
    const ws = fakeWs();
    const ac = new AudioCapture(fakeStream(), ws);
    ac.start();
    ws.send.mockClear();
    ws.readyState = 0; // socket dropped while the recorder is still flushing
    lastRecorder.ondataavailable({ data: { size: 10 } });
    expect(ws.send).not.toHaveBeenCalled();
  });
});
