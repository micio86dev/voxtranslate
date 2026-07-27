// guest-latency.js — measure how far a webinar guest actually is behind the live edge.
//
// Paste this whole file into the DevTools console on https://voxtranslate.app/w/<code>
// while the webinar is live. It samples the <video> element directly, so it works on the
// CURRENTLY DEPLOYED build — that is the point: run it once before shipping a player
// change to get an honest "before", then again after to prove the delta.
//
// What it reports, per second:
//   drift   seconds between the playhead and the end of the seekable range. This is the
//           player-side latency: buffer the player holds but has not shown yet.
//   rate    the element's playbackRate. > 1 means hls.js is actively catching up
//           (maxLiveSyncPlaybackRate). A flat 1.00 while drift stays high means catch-up
//           is DISABLED — the default, and the reason startup delay becomes permanent.
//   state   paused / playing, so a stall is not mistaken for latency.
//
// Caveat, stated plainly: drift is PLAYER latency, not glass-to-glass. It does not
// include capture, encode, WHIP upload, remux, or CDN hops. For true end-to-end, point
// the host's webcam at a millisecond clock (time.is) and diff it against the same clock
// on the guest screen. Use drift for A/B on player changes; use the clock for the
// absolute number you quote to anyone.

(() => {
  const video = document.querySelector('video');
  if (!video) {
    console.error('[vox-latency] no <video> on this page — is the webinar live?');
    return;
  }

  const SAMPLE_MS = 1_000;
  const samples = [];

  /** Seconds behind the live edge, or null when nothing is buffered yet. */
  const drift = () => {
    const s = video.seekable;
    if (!s || s.length === 0) return null;
    const edge = s.end(s.length - 1);
    return Number.isFinite(edge) ? Math.max(0, edge - video.currentTime) : null;
  };

  const timer = setInterval(() => {
    const d = drift();
    if (d === null) return;
    samples.push(d);
    console.log(
      `[vox-latency] drift ${d.toFixed(2)}s   rate ${video.playbackRate.toFixed(2)}   ` +
        `${video.paused ? 'paused' : 'playing'}`,
    );
  }, SAMPLE_MS);

  /** Stop sampling and print the summary. Call `voxLatency.stop()` when done. */
  const stop = () => {
    clearInterval(timer);
    if (samples.length === 0) {
      console.warn('[vox-latency] no samples collected');
      return null;
    }
    const sorted = [...samples].sort((a, b) => a - b);
    const pick = (q) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))];
    const summary = {
      samples: samples.length,
      min: +sorted[0].toFixed(2),
      median: +pick(0.5).toFixed(2),
      p95: +pick(0.95).toFixed(2),
      max: +sorted[sorted.length - 1].toFixed(2),
      // A latency that only ever grows means nothing is pulling the playhead back to the
      // live edge — the signature of the default hls.js config.
      trend: +(samples[samples.length - 1] - samples[0]).toFixed(2),
    };
    console.table(summary);
    return summary;
  };

  globalThis.voxLatency = { stop, samples };
  console.log('[vox-latency] sampling every 1s — run voxLatency.stop() for the summary');
})();
