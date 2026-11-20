// Live Connect — "Prepare my voice" for the host (spec 0121, S5).
//
// The same flow as Meet's voice preparation and the webinar card: record 3-8 seconds of actual
// speech from the microphone (`recordVoiceSample`), upload it for Cartesia Instant Voice Cloning
// (`cloneVoice`), and tell the caller how it went. Best effort and never throws: a failure just
// leaves the host without a clone, and the conversation can still run on Enhanced's default voice.

import { cloneVoice } from '../api';
import { recordVoiceSample } from '../voice-sample';

export type VoicePrepState = 'recording' | 'saving' | 'cloned' | 'failed';

export interface VoicePrepDeps {
  getMedia?: () => Promise<MediaStream>;
  record?: typeof recordVoiceSample;
  clone?: typeof cloneVoice;
}

/** Returns whether a clone was stored. `onState` drives the inline feedback; `onProgress` is 0..1. */
export async function prepareVoice(
  lang: string,
  onState: (state: VoicePrepState) => void,
  onProgress: (fraction: number) => void = () => {},
  deps: VoicePrepDeps = {},
): Promise<boolean> {
  let stream: MediaStream;
  try {
    stream = await (deps.getMedia ?? (() => navigator.mediaDevices.getUserMedia({ audio: true })))();
  } catch {
    onState('failed'); // microphone denied or missing
    return false;
  }
  try {
    onState('recording');
    const blob = await (deps.record ?? recordVoiceSample)(stream, onProgress);
    if (!blob) {
      onState('failed'); // too little speech
      return false;
    }
    onState('saving');
    const res = await (deps.clone ?? cloneVoice)(blob, lang);
    onState(res.voice_id ? 'cloned' : 'failed');
    return !!res.voice_id;
  } catch {
    onState('failed');
    return false;
  } finally {
    stream.getTracks().forEach((track) => track.stop());
  }
}
