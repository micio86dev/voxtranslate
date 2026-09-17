import { describe, expect, it, vi } from 'vitest';

import { createPhoneLegController, runPhoneDialSequence, skipsPrejoin } from './phone-call';

describe('skipsPrejoin', () => {
  it('R4: the phone entry path skips the camera/device-check prejoin screen', () => {
    expect(skipsPrejoin('phone')).toBe(true);
  });

  it('R7: an ordinary video-room join is unaffected — the prejoin screen still shows', () => {
    expect(skipsPrejoin('room')).toBe(false);
  });
});

describe('runPhoneDialSequence (money-safety a, R3)', () => {
  it('a denied/failed microphone never reaches quote or dial — nothing is charged', async () => {
    const acquireMic = vi.fn().mockRejectedValue(new DOMException('denied', 'NotAllowedError'));
    const quote = vi.fn();
    const dial = vi.fn();
    const releaseMic = vi.fn();

    const outcome = await runPhoneDialSequence({ acquireMic, quote, dial, releaseMic });

    expect(outcome.stage).toBe('mic');
    expect(quote).not.toHaveBeenCalled();
    expect(dial).not.toHaveBeenCalled();
    expect(releaseMic).not.toHaveBeenCalled(); // there is no mic to release — it was never acquired
  });

  it('a quote refusal releases the acquired mic and never reaches dial', async () => {
    const mic = { id: 'mic-1' };
    const acquireMic = vi.fn().mockResolvedValue(mic);
    const quote = vi.fn().mockResolvedValue({ ok: false, data: { error: 'insufficient_credits' } });
    const dial = vi.fn();
    const releaseMic = vi.fn();

    const outcome = await runPhoneDialSequence({ acquireMic, quote, dial, releaseMic });

    expect(outcome.stage).toBe('quote');
    expect(dial).not.toHaveBeenCalled();
    expect(releaseMic).toHaveBeenCalledWith(mic);
  });

  it('a dial refusal (server re-check failing, R10) releases the mic — call order is mic then quote then dial', async () => {
    const mic = { id: 'mic-1' };
    const calls: string[] = [];
    const acquireMic = vi.fn().mockImplementation(async () => {
      calls.push('mic');
      return mic;
    });
    const quote = vi.fn().mockImplementation(async () => {
      calls.push('quote');
      return { ok: true, data: { price_per_minute: '0.05' } };
    });
    const dial = vi.fn().mockImplementation(async () => {
      calls.push('dial');
      return { ok: false, data: { error: 'caller_id_unverified' } };
    });
    const releaseMic = vi.fn();

    const outcome = await runPhoneDialSequence({ acquireMic, quote, dial, releaseMic });

    expect(outcome.stage).toBe('dial');
    expect(calls).toEqual(['mic', 'quote', 'dial']);
    expect(releaseMic).toHaveBeenCalledWith(mic);
  });

  it('a successful mic → quote → dial returns the placed call and keeps the mic (about to join)', async () => {
    const mic = { id: 'mic-1' };
    const call = { call_id: 'c1', room: 'ph-abc' };
    const acquireMic = vi.fn().mockResolvedValue(mic);
    const quote = vi.fn().mockResolvedValue({ ok: true, data: { price_per_minute: '0.05' } });
    const dial = vi.fn().mockResolvedValue({ ok: true, data: call });
    const releaseMic = vi.fn();

    const outcome = await runPhoneDialSequence({ acquireMic, quote, dial, releaseMic });

    expect(outcome).toEqual({ stage: 'dialed', mic, call });
    expect(releaseMic).not.toHaveBeenCalled();
  });
});

describe('createPhoneLegController (money-safety b, R5/R6)', () => {
  it('end() hangs up the started call exactly once even when called twice (idempotent, e.g. leaveCall() + a racing pagehide)', () => {
    const hangup = vi.fn();
    const clearPoll = vi.fn();
    const leg = createPhoneLegController({ hangup, clearPoll });

    leg.start({ orgId: 'org-1', callId: 'c1' });
    leg.end();
    leg.end();

    expect(hangup).toHaveBeenCalledTimes(1);
    expect(hangup).toHaveBeenCalledWith('org-1', 'c1');
  });

  it('end() before any start() is a safe no-op — no hangup is posted for a call that never existed', () => {
    const hangup = vi.fn();
    const clearPoll = vi.fn();
    const leg = createPhoneLegController({ hangup, clearPoll });

    leg.end();

    expect(hangup).not.toHaveBeenCalled();
    expect(clearPoll).toHaveBeenCalledTimes(1); // still stops a timer if one happened to be running
  });

  it('isActive() reflects the held handle across start()/end()', () => {
    const leg = createPhoneLegController({ hangup: vi.fn(), clearPoll: vi.fn() });

    expect(leg.isActive()).toBe(false);
    leg.start({ orgId: 'org-1', callId: 'c1' });
    expect(leg.isActive()).toBe(true);
    leg.end();
    expect(leg.isActive()).toBe(false);
  });
});
