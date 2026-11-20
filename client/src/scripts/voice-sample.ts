// Voice-sample recorder for Cartesia Instant Voice Cloning. Extracted verbatim from app.ts so
// Live Connect (which never loads app.ts) records the host's voice exactly the way Meet does.

/** Record a short mic sample, resolving once ≥3 s of actual speech is captured (energy
 *  threshold, not raw time), or null if too little speech by the time ceiling. */
export function recordVoiceSample(
  stream: MediaStream,
  onProgress?: (fraction: number) => void,
): Promise<Blob | null> {
  return new Promise((resolve) => {
    let recorder: MediaRecorder;
    try {
      const mime = ['audio/webm;codecs=opus', 'audio/mp4', 'audio/webm'].find((ty) =>
        MediaRecorder.isTypeSupported?.(ty),
      );
      recorder = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
    } catch {
      resolve(null);
      return;
    }
    const chunks: BlobPart[] = [];
    recorder.ondataavailable = (e) => {
      if (e.data.size) chunks.push(e.data);
    };
    const Ctx =
      window.AudioContext ??
      (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
    const ctx = new Ctx();
    const analyser = ctx.createAnalyser();
    analyser.fftSize = 512;
    ctx.createMediaStreamSource(stream).connect(analyser);
    const buf = new Float32Array(analyser.fftSize);
    let speechMs = 0;
    let elapsedMs = 0;
    const TICK = 100;
    const SPEECH_RMS = 0.02; // energy floor that counts as speech (not room noise)
    const MIN_SPEECH_MS = 3000; // Cartesia IVC wants ≥3 s of actual speech
    const MAX_MS = 8000; // hard ceiling so we never hold the join hostage
    recorder.onstop = () => {
      const type = recorder.mimeType || 'audio/webm';
      resolve(speechMs >= MIN_SPEECH_MS && chunks.length ? new Blob(chunks, { type }) : null);
    };
    const timer = setInterval(() => {
      analyser.getFloatTimeDomainData(buf);
      let sum = 0;
      for (const v of buf) sum += v * v;
      if (Math.sqrt(sum / buf.length) > SPEECH_RMS) speechMs += TICK;
      onProgress?.(Math.min(1, speechMs / MIN_SPEECH_MS));
      elapsedMs += TICK;
      if (speechMs >= MIN_SPEECH_MS + 500 || elapsedMs >= MAX_MS) {
        clearInterval(timer);
        void ctx.close().catch(() => {});
        try {
          recorder.stop();
        } catch {
          resolve(null);
        }
      }
    }, TICK);
    try {
      recorder.start();
    } catch {
      clearInterval(timer);
      resolve(null);
    }
  });
}
