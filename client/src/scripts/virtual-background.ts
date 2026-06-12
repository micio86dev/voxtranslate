// Real-time camera background blur (issue #6, MVP: blur only).
//
// Segmentation runs via MediaPipe Selfie Segmentation, lazily loaded from a CDN
// the first time the effect is enabled — the model is NOT bundled, so the build
// stays lean and users who never enable the effect pay nothing. Each frame is
// composited on a canvas (sharp subject over a blurred copy) and re-published as
// a video track through canvas.captureStream(). If the model can't load or the
// browser lacks canvas.captureStream, start() returns the raw track unchanged so
// the call keeps working (graceful degradation, surfaced via `active`).

export const MEDIAPIPE_BASE =
  'https://cdn.jsdelivr.net/npm/@mediapipe/selfie_segmentation';

/** CDN URL for a MediaPipe asset (script, wasm, model). */
export function mediaPipeAsset(file: string): string {
  return `${MEDIAPIPE_BASE}/${file}`;
}

const MODEL_SELECTION = 1; // 1 = general (256×256) — good speed/quality balance
const BLUR_PX = 8;
const CAPTURE_FPS = 24;

interface SegResults {
  image: CanvasImageSource;
  segmentationMask: CanvasImageSource;
}
interface SelfieSegmentation {
  setOptions(opts: { modelSelection: number; selfieMode?: boolean }): void;
  onResults(cb: (r: SegResults) => void): void;
  send(input: { image: HTMLVideoElement }): Promise<void>;
  close(): void;
}
type SelfieSegmentationCtor = new (cfg: {
  locateFile: (file: string) => string;
}) => SelfieSegmentation;

let loaderPromise: Promise<boolean> | null = null;

/** Inject the MediaPipe UMD script once. Resolves true when the global
 *  `SelfieSegmentation` constructor is available. */
export function loadMediaPipe(): Promise<boolean> {
  if (loaderPromise) return loaderPromise;
  loaderPromise = new Promise<boolean>((resolve) => {
    const g = globalThis as { SelfieSegmentation?: unknown };
    if (g.SelfieSegmentation) return resolve(true);
    if (typeof document === 'undefined') return resolve(false);
    const script = document.createElement('script');
    script.src = mediaPipeAsset('selfie_segmentation.js');
    script.crossOrigin = 'anonymous';
    script.onload = () => resolve(!!(globalThis as { SelfieSegmentation?: unknown }).SelfieSegmentation);
    script.onerror = () => resolve(false);
    document.head.appendChild(script);
  });
  return loaderPromise;
}

export class VirtualBackground {
  /** The raw camera track being processed (the caller owns its lifecycle). */
  source: MediaStreamTrack | null = null;

  private seg: SelfieSegmentation | null = null;
  private video: HTMLVideoElement | null = null;
  private canvas: HTMLCanvasElement | null = null;
  private ctx: CanvasRenderingContext2D | null = null;
  private output: MediaStream | null = null;
  private raf = 0;
  private running = false;

  /** True once segmentation is actually processing frames (false when we fell
   *  back to the raw track). */
  get active(): boolean {
    return this.running;
  }

  /**
   * Begin blurring `track`. Returns the processed output track, or `track`
   * itself when segmentation is unavailable (check `active` to tell them apart).
   */
  async start(track: MediaStreamTrack): Promise<MediaStreamTrack> {
    this.source = track;
    const canCapture =
      typeof document !== 'undefined' &&
      typeof HTMLCanvasElement !== 'undefined' &&
      typeof HTMLCanvasElement.prototype.captureStream === 'function';
    const loaded = await loadMediaPipe();
    if (!loaded || !canCapture) return track; // graceful fallback

    const Ctor = (globalThis as unknown as { SelfieSegmentation: SelfieSegmentationCtor })
      .SelfieSegmentation;
    this.seg = new Ctor({ locateFile: mediaPipeAsset });
    this.seg.setOptions({ modelSelection: MODEL_SELECTION });
    this.seg.onResults((r) => this.draw(r));

    const settings = track.getSettings();
    this.canvas = document.createElement('canvas');
    this.canvas.width = settings.width ?? 640;
    this.canvas.height = settings.height ?? 480;
    this.ctx = this.canvas.getContext('2d');

    this.video = document.createElement('video');
    this.video.muted = true;
    this.video.playsInline = true;
    this.video.srcObject = new MediaStream([track]);
    await this.video.play().catch(() => {});

    this.output = this.canvas.captureStream(CAPTURE_FPS);
    this.running = true;
    void this.pump();
    return this.output.getVideoTracks()[0] ?? track;
  }

  /** Tear down segmentation + the render loop. Does NOT stop `source` — the
   *  caller decides whether the camera device keeps running. */
  stop(): void {
    this.running = false;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    try {
      this.seg?.close();
    } catch {
      /* ignore teardown errors */
    }
    this.seg = null;
    this.output?.getTracks().forEach((t) => t.stop());
    this.output = null;
    if (this.video) this.video.srcObject = null;
    this.video = null;
    this.canvas = null;
    this.ctx = null;
    this.source = null;
  }

  private pump = async (): Promise<void> => {
    if (!this.running || !this.seg || !this.video) return;
    if (this.video.readyState >= 2) {
      try {
        await this.seg.send({ image: this.video });
      } catch {
        /* drop this frame, keep going */
      }
    }
    if (this.running) this.raf = requestAnimationFrame(() => void this.pump());
  };

  /** Composite the sharp subject over a blurred copy of the frame. */
  private draw(r: SegResults): void {
    const { ctx, canvas } = this;
    if (!ctx || !canvas) return;
    const { width, height } = canvas;
    ctx.save();
    ctx.clearRect(0, 0, width, height);
    // Mask, then keep the sharp frame only where the subject is.
    ctx.drawImage(r.segmentationMask, 0, 0, width, height);
    ctx.globalCompositeOperation = 'source-in';
    ctx.drawImage(r.image, 0, 0, width, height);
    // Fill everything behind the subject with a blurred copy of the frame.
    ctx.globalCompositeOperation = 'destination-over';
    ctx.filter = `blur(${BLUR_PX}px)`;
    ctx.drawImage(r.image, 0, 0, width, height);
    ctx.restore();
  }
}
