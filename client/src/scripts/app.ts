// VoxTranslate V2 client orchestrator: home/lobby → pre-join (camera + devices)
// → WebRTC video call with translated subtitles + chat.

import { applyI18n, detectLang, ENDONYM, FLAG, getUiLang, loadLocale, setUiLang, SUPPORTED, t } from './i18n';
import {
  type EngineInfo,
  DEFAULT_ENGINE_ID,
  cheapestTier,
  commonLangs,
  enforceEngineForNetwork,
  engineDescKey,
  engineIsClientDirect,
  engineNeedsPcm,
  formatRate,
  getAvailableTiers,
  languagesByRegion,
  loadEnginePref,
  offeredLanguageCodes,
  resolveEnginePref,
  saveEnginePref,
  searchLanguages,
  selectableEngines,
} from './engines';
import { type LangMeta, LANGUAGES, langMeta } from './langmap';
// In-call modules are lazy-loaded at pre-join (spec 0105) — keep only their TYPES here so the
// landing entry chunk doesn't statically pull them in; the runtime values come from the
// `CallModules` namespaces returned by `loadCallModules()`.
import type { CartesiaManager, CartesiaSession } from './cartesia';
import { type CallModules, loadCallModules } from './call-modules';
import { loadRemoteI18n } from './content';
import { setupScheduling } from './meetings';
import { initAnalytics, grantAnalyticsConsent, track } from './analytics';
import { setupGeoOptIn } from './geo';
import { enablePush, maybeSubscribePush } from './push';
import {
  fetchPreferences,
  fetchUnread,
  type InAppNotification,
  markRead,
  NOTIF_CHANNELS,
  NOTIF_EVENTS,
  type Prefs,
  savePreferences,
} from './notifications';
import { icon } from './icons';
import type { MeshManager } from './webrtc';
import { resolvePeerId } from './peer-id';
import type { AudioCapture } from './audio-capture';
import type { PcmCapture } from './pcm-capture';
import { pcmPlayback } from './pcm-playback';
import { renderSubtitleInto } from './subtitle-render';
import type { MicMeter } from './mic-meter';
import { ttsManager } from './tts/manager';
import { registerVoxIfInstalled } from './tts/register';
import type { ChatManager, ChatPayload } from './chat';
import { CHAT_MAX_HEIGHT, counterLabel, counterState, insertAt, recTimeLabel, resizeBox } from './chat-input';
import { checkUploadFile, cloneVoice, fetchAiPricing, fetchEnhancedSession, fileUploadEnabled, generateAiQuiz, saveQuizHistory, sendInvites, UPLOAD_ACCEPT, UPLOAD_MAX_BYTES, uploadChatFile } from './api';
import { buildInviteLink, MAX_INVITE_EMAILS, parseRoomParam, validateInviteEmails } from './invite';
import {
  acceptFriend,
  type Friend,
  fetchFriendRequests,
  fetchFriends,
  inviteFriendToCall,
  removeFriend,
  sendFriendRequest,
} from './friends';
import * as auth from './auth';
import {
  bindRoom,
  canCloudRecord,
  DASHBOARD_URL,
  getRoomBinding,
  listMyOrgs,
  listProjectVoiceMessages,
  listProjects,
  uploadProjectVoiceMessage,
  uploadRecording,
  voiceMessageAudioUrl,
  type BusinessOrg,
  type ProjectVoiceMessage,
} from './business';
import {
  addToCalendar,
  archiveWebinar,
  cancelWebinar,
  canHostWebinar,
  createWebinar,
  formatScheduledStart,
  fromDatetimeLocalValue,
  isWebinarLive,
  listPublicWebinars,
  listWebinars,
  type PublicWebinarListItem,
  qrDownloadFilename,
  showVoiceCloneToggle,
  showWebinarCloneAction,
  unarchiveWebinar,
  validateSchedule,
  WebinarError,
  type WebinarView,
} from './webinar';
import { PresenceClient, type ChatEvent } from './webinar-presence';
import { ChatPanel } from './webinar-chat';
import { WhipPublisher, type WhipState } from './whip-publisher';
import { WebinarSttClient } from './webinar-stt';
import { AudioCapture as WebinarAudioCapture } from './audio-capture';
import { initBookmarks, setBookmarkSession } from './bookmarks';
import { initBugReport } from './bug-report';
import * as onboarding from './onboarding';
import { initGlossary, onGlossaryActive, refreshGlossaryHome, setGlossaryRoom } from './glossary';
import type { Whiteboard, WbTool, WbWidth } from './whiteboard';
import type { TicTacToe } from './tictactoe';
import type { Quiz } from './quiz';
import { CallTimer, spokenDuration, formatClock } from './timer';
import { dismissLangToast, initLangDetect, onLanguageDetected } from './lang-detect';
import { initNetStatus, setNetworkDegraded } from './net-status';
import { isRestrictedNetwork } from './restricted-net';
import { toast } from './toast';
import {
  playCallEnterSound,
  playCallLeaveSound,
  playHandRaiseSound,
  playJoinSound,
  playLeaveSound,
  playRecordingStartSound,
  playScreenShareSound,
  playTimerDoneSound,
  playTimerSetSound,
} from './sfx';
import { RateLimiter } from './reaction-rate-limit';
// Local-only call features — split into their own chunks, dynamically imported on the button
// that activates each (spec 0105); only their types are needed here.
import type { VirtualBackground } from './virtual-background';
import type { ScreenSharePip } from './screenshare-pip';
import type { CompositeRecorder } from './recording/composite-recorder';
import { formatElapsed, isRecordingSupported, recordingFilename } from './recording/utils';
import type { ParticipantSource } from './recording/types';

// A lazily-imported chunk (e.g. the post-call session screen or the in-call
// modules) can 404 when a new frontend deploy rewrote the hashed filenames while
// this tab was still open. Vite fires `vite:preloadError` for that failed dynamic
// import — recover by reloading once into the fresh build instead of leaving a
// broken feature. A sessionStorage guard prevents a reload loop if it's a genuine
// outage rather than a stale chunk.
window.addEventListener('vite:preloadError', () => {
  const KEY = 'vox-stale-chunk-reloaded';
  try {
    if (sessionStorage.getItem(KEY)) return;
    sessionStorage.setItem(KEY, '1');
  } catch {
    /* storage blocked → still reload once below */
  }
  location.reload();
});

// ---- Config ----------------------------------------------------------------
const WS_HOST = import.meta.env.PUBLIC_WS_HOST || location.host;
const WS_PROTO = location.protocol === 'https:' ? 'wss:' : 'ws:';
const WS_BASE = `${WS_PROTO}//${WS_HOST}`;
const HTTP_BASE = WS_BASE.replace(/^ws/, 'http');

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

// Coarse mobile/tablet detection (iPadOS masquerades as macOS but is multi-touch).
// navigator.platform is deprecated, but Safari has no userAgentData — it stays
// the only iPadOS signal; the untyped cast keeps the deprecation hint quiet.
const IS_MOBILE =
  /Android|iPhone|iPad|iPod/i.test(navigator.userAgent) ||
  (navigator.maxTouchPoints > 1 &&
    /Mac/.test((navigator as unknown as { platform?: string }).platform ?? ''));

// Whether this browser can open a peer connection at all. False in in-app browsers /
// restricted WebViews that strip WebRTC, privacy setups that delete RTCPeerConnection,
// and insecure (non-HTTPS) contexts. Gates the join so we show a clear message instead
// of crashing with "RTCPeerConnection is not a constructor" deep in the mesh. Kept
// inline (not imported from ./webrtc) so the heavy call module stays lazy-loaded.
function webrtcSupported(): boolean {
  const g = globalThis as unknown as {
    RTCPeerConnection?: unknown;
    webkitRTCPeerConnection?: unknown;
  };
  const hasCtor =
    typeof g.RTCPeerConnection === 'function' || typeof g.webkitRTCPeerConnection === 'function';
  return hasCtor && (typeof isSecureContext === 'undefined' || isSecureContext);
}

// Total video upload budget (bit/s), split per-peer and network-adapted (specs
// 0030–0032). Tunable via Vercel BUILD-TIME env (PUBLIC_*), falling back to the
// previous hardcoded values when unset — so behaviour is identical until the env
// vars are added on Vercel (spec 0044). Build-time: a change needs a client redeploy.
const VIDEO_BUDGET_MOBILE = Number(import.meta.env.PUBLIC_VIDEO_BUDGET_MOBILE) || 1_200_000;
const VIDEO_BUDGET_DESKTOP = Number(import.meta.env.PUBLIC_VIDEO_BUDGET_DESKTOP) || 2_400_000;
// Screen sharing pushes text/UI (not a face), which needs a higher cap than the
// camera or the shared content looks grainy. Separate + env-tunable; desktop-only
// (screen share is hidden on mobile). Default 4 Mbit/s (spec 0088).
const VIDEO_BUDGET_SCREEN = Number(import.meta.env.PUBLIC_VIDEO_BUDGET_SCREEN) || 4_000_000;

// ---- Screens ---------------------------------------------------------------
const loginScreen = $('login');
const homeScreen = $('home');
const prejoinScreen = $('prejoin');
const callScreen = $('call');

// ---- Auth / billing refs ---------------------------------------------------
const accountBar = $('account-bar');
const guestBar = $('guest-bar');
const accountAvatar = $<HTMLImageElement>('account-avatar');
const accountName = $('account-name');
const accountBalance = $('account-balance');
const billingBalance = $('billing-balance');
const callBalance = $('call-balance');
const lowBanner = $('low-banner');
const lowBannerText = $('low-banner-text');
const buyModal = $('buy-modal');
const packagesList = $('packages-list');
// Cached credit packages (spec 0028) so the checkout handler can resolve the picked one for
// analytics without re-fetching. Populated by renderPackages().
let creditPackages: auth.CreditPackage[] = [];
const ledgerList = $('ledger-list');
const modalBalance = $('modal-balance');
const buyStatus = $('buy-status');
const exhaustedModal = $('exhausted-modal');
const consentModal = $('consent-modal');
const reportModal = $('report-modal');
const cookieBanner = $('cookie-banner');

let billing = false; // accounts/credits enabled on this backend
// Translation engines available on this backend (spec 0093). The selector renders
// from this list; `selectedEngine` is the user's persisted choice (default 'standard').
let availableEngines: EngineInfo[] = [];
let selectedEngine = 'standard';
// Language-first picker (spec 0102): enabled by the LANGUAGE_FIRST_UX backend flag (read
// from /api/engines). `selectedLang` is the chosen TARGET language; the hidden #lang
// <select> stays the canonical value holder so the join payload / UI language are unchanged.
let languageFirstUx = false;
// Enhanced voice cloning (spec 0108): gates the pre-join voice-preparation step. Server flag
// (`CARTESIA_VOICE_CLONING_ENABLED`) surfaced via `/api/engines` flags.
let voiceCloningEnabled = false;
let selectedLang = 'en';
let langPickerWired = false;
let exhaustedIsGuest = false; // last balance_exhausted was a guest trial vs a billed user
const blockedPeers = new Set<string>(); // peers blocked locally (muted + hidden)
let reportTargetId = ''; // peer currently being reported

// ---- Home refs -------------------------------------------------------------
const roomInput = $<HTMLInputElement>('room');
const nameInput = $<HTMLInputElement>('name');
const langSel = $<HTMLSelectElement>('lang');
const engineField = $('engine-field');

// Remember the last NAME + LANGUAGE used to join (guests included) so a returning
// visitor doesn't re-enter them. Best-effort localStorage (private mode → no-op), in
// the existing `voxtranslate_*` key namespace.
const NAME_CACHE_KEY = 'voxtranslate_name';
const LANG_CACHE_KEY = 'voxtranslate_lang';
function readCache(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function writeCache(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* private mode / storage blocked */
  }
}
const engineOptions = $('engine-options');
// Language-first picker refs (spec 0102) — present in index.astro, hidden until enabled.
const langField = $('lang-field');
const langfirstField = $('langfirst-field');
const langTrigger = $<HTMLButtonElement>('lang-trigger');
const langTriggerFlag = $('lang-trigger-flag');
const langTriggerText = $('lang-trigger-text');
const langPanel = $('lang-panel');
const langSearch = $<HTMLInputElement>('lang-search');
const langPanelList = $('lang-panel-list');
const langPanelEmpty = $('lang-panel-empty');
const tierField = $('tier-field');
const tierOptions = $('tier-options');
const tierNote = $('tier-note');
const RECENT_LANGS_KEY = 'voxtranslate_recent_langs';
const enterBtn = $<HTMLButtonElement>('enter');
const homeStatus = $('home-status');
const visGroup = $('vis-group');
const visHint = $('vis-hint');
const roomsList = $('rooms-list');
// Public webinars lobby section (sibling of the rooms list); the card is hidden when empty.
const publicWebinarsCard = $('public-webinars-card');
const publicWebinarsList = $('public-webinars');

// ---- Pre-join refs ---------------------------------------------------------
const previewVideo = $<HTMLVideoElement>('preview');
const camSelect = $<HTMLSelectElement>('cam-select');
const micSelect = $<HTMLSelectElement>('mic-select');
const preMic = $<HTMLButtonElement>('pre-mic');
const preCam = $<HTMLButtonElement>('pre-cam');
const previewOff = $('preview-off');
const previewAvatar = $('preview-avatar');
const prejoinRoom = $('prejoin-room');
const prejoinVis = $('prejoin-vis');
const prejoinStatus = $('prejoin-status');

// ---- Call refs -------------------------------------------------------------
const videoGrid = $('video-grid');
const callRoom = $('call-room');
const callVis = $('call-vis');
// Self identity folded into the meta row (replaces the self tile's .video-overlay).
const stageSelf = $('stage-self');
const stageSelfName = $('stage-self-name');
const stageSelfLang = $('stage-self-lang');
const chatPanel = $('chat-panel');
const chatMessages = $('chat-messages');
const chatInput = $<HTMLTextAreaElement>('chat-input');
const chatCounter = $('chat-counter');
const chatBadge = $('chat-badge');
// Chat file upload (spec 0018).
const chatAttach = $('chat-attach');
const chatFileInput = $<HTMLInputElement>('chat-file-input');
const chatDrop = $('chat-drop');
const chatUpload = $('chat-upload');
const chatUploadFill = $('chat-upload-fill');
const chatUploadLabel = $('chat-upload-label');
// Voice messages: record a clip → upload via the same /files endpoint → server
// transcribes (Deepgram) + translates → renders as an audio attachment + transcript.
const chatRecord = $('chat-record');
const chatInputRow = $('chat-input-row');
const chatRecTime = $('chat-rec-time');
const btnMic = $('btn-mic');
const btnCam = $('btn-cam');
const btnBg = $('btn-bg');
const btnTts = $('btn-tts');
const btnSubtitle = $('btn-subtitle');
const btnHand = $('btn-hand');
const btnChat = $('btn-chat');
const btnFullscreen = $('btn-fullscreen');
const btnPip = $('btn-pip');
const btnParticipants = $('btn-participants');
const btnView = $('btn-view');
const btnShare = $('btn-share');
const btnRecord = $('btn-record');
const btnMore = $('btn-more');
const notifBanner = $('notif-banner');
const participantsPanel = $('participants-panel');
const participantsList = $('participants-list');
const partClose = $('part-close');
// ---- Invite panel refs (spec 0082) -----------------------------------------
const miInvite = $('mi-invite'); // the overflow-menu item (hidden when room is full)
const miMinigame = $('mi-minigame'); // tic-tac-toe item (hidden until there's an opponent)
const btnInvite = $('btn-invite');
const invitePanel = $('invite-panel');
const inviteClose = $('invite-close');
const inviteLinkInput = $<HTMLInputElement>('invite-link');
const inviteCopyBtn = $('invite-copy');
const inviteEmailBlock = $('invite-email-block');
const inviteEmailInput = $<HTMLInputElement>('invite-email');
const inviteSendBtn = $<HTMLButtonElement>('invite-send');
const inviteGuestHint = $('invite-guest-hint');
const inviteStatus = $('invite-status');
const MAX_ROOM = 4; // mirrors server rooms::MAX_PEERS

// ---- State -----------------------------------------------------------------
// Stable across reloads within this tab so a rejoin is recognised as a
// reconnect (server evicts the ghost) — no phantom tile / inflated count (#219).
const myId = resolvePeerId();

let session: { room: string; lang: string; name: string; isPublic: boolean; engine: string } | null =
  null;
let localStream: MediaStream | null = null;
let ws: WebSocket | null = null;
let mesh: MeshManager | null = null;

// Collaborative whiteboard (spec 0045 → advanced in 0062) + mini-games (0046–0048): ops/state
// relay over the same WS. These are COLLABORATIVE — a remote peer can start them — so they are
// constructed once when the lazy call modules load at pre-join (spec 0105), NOT on a local
// button click; otherwise a remote op arriving before the local user opened the feature would
// have nothing to render. Definite-assignment holders: every use is in-call, after construction.
const wbOverlay = $('whiteboard');
let whiteboard!: Whiteboard;

// Mini-games (spec 0046/0047): state relays over the same WS `game` channel.
const gameName = (id: string): string =>
  id === myId ? session?.name || t('you') : peerNames.get(id)?.name || '';
const sendGame = (state: unknown): void => ws?.send(JSON.stringify({ type: 'game', state }));
const minigameEl = $('minigame');
let tictactoe!: TicTacToe;
const quizEl = $('quiz');
let quiz!: Quiz;

// Construct the collaborative features from the lazily-loaded call modules. Called once via
// `ensureCallModules()` (guarded by `callFeaturesInited`) the first time we enter pre-join.
function initCallFeatures(m: CallModules): void {
  whiteboard = new m.whiteboard.Whiteboard(
    $<HTMLCanvasElement>('wb-canvas'),
    (op) => ws?.send(JSON.stringify({ type: 'whiteboard', op })),
    (count, index) => renderWbPages(count, index),
  );
  // peers() feeds seat assignment + spectator rotation (spec 0070 S3): self first,
  // then peers in their join order.
  tictactoe = new m.tictactoe.TicTacToe(minigameEl, myId, gameName, sendGame, t, () => [
    myId,
    ...peerNames.keys(),
  ]);
  // Each client renders the quiz in its own language (spec 0048). The modal callback
  // opens the quiz for EVERY participant when one starts, and closes it on cancel,
  // with a toast (spec 0070 R4.1/R4.3).
  quiz = new m.quiz.Quiz(
    quizEl,
    myId,
    gameName,
    () => session?.lang || 'en',
    sendGame,
    t,
    (open) => {
      toggleQuiz(open);
      if (!open) toast(t('quizCancelled'));
    },
    // Host-only: persist the finished quiz + scores for the session history (#221).
    // Best-effort; needs a recorded session (activeSessionId) + an authed host.
    (summary) => {
      if (activeSessionId) void saveQuizHistory(activeSessionId, summary);
    },
  );
}

// The lazily-loaded in-call module namespaces (spec 0105). Non-null for the whole call: warmed
// at pre-join and awaited at join, so every in-call construction site reads it via `callMods!`.
let callMods: CallModules | null = null;
let callFeaturesInited = false;
/** Ensure the in-call modules are loaded and the collaborative features constructed. Warm this
 *  at pre-join (download overlaps camera setup); await it at join. Cached — one fetch per page. */
async function ensureCallModules(): Promise<CallModules> {
  const m = await loadCallModules();
  callMods = m;
  if (!callFeaturesInited) {
    initCallFeatures(m);
    callFeaturesInited = true;
  }
  return m;
}

// Voice-command countdown timer (spec 0052): started from your own Deepgram
// transcript ("imposta timer di 10 minuti") or the manual popover. Local-only —
// the badge, sound, and spoken confirmation all happen on the device that set it.
const callTimer = new CallTimer({
  badge: $('timer-badge'),
  remaining: $('timer-remaining'),
  cancelBtn: $('timer-cancel'),
  t,
  onSet: (cmd) => {
    const human = spokenDuration(cmd.seconds, t);
    toast(t('timerSet').replace('{d}', human));
    playTimerSetSound();
    // Optional spoken confirmation, gated on the "translated voice" output toggle
    // so it stays opt-in; the visual badge + cue always fire.
    if (ttsOn) speakSystem(t('timerSetSpeak').replace('{d}', human), getUiLang());
  },
  onDone: () => {
    toast(t('timerDone'));
    playTimerDoneSound();
    if (ttsOn) speakSystem(t('timerDoneSpeak'), getUiLang());
  },
  onCancel: () => toast(t('timerCancelled')),
});
let audioCapture: AudioCapture | PcmCapture | null = null;
// Enhanced (Cartesia, spec 0108): client-direct receive pipeline. Lazily created; only
// activated for a signed-in, listener-pays listener whose engine is client-direct — for
// every other engine/mode it stays null/inert.
let cartesiaManager: CartesiaManager | null = null;
// Peers' Cartesia cloned-voice ids (from presence) so we can speak each in their own voice,
// even for peers who joined before the manager existed (seeded on activate).
const peerVoiceIds = new Map<string, string>();
function ensureCartesiaManager(): CartesiaManager {
  if (!cartesiaManager) {
    cartesiaManager = new callMods!.cartesia.CartesiaManager({
      fetchSession: async () => {
        const s = await fetchEnhancedSession();
        if (!s) return null;
        return {
          token: s.token,
          expiresAt: s.expires_at,
          cartesiaVersion: s.cartesia_version,
          sttEndpoint: s.stt.endpoint,
          sttModel: s.stt.model,
          sttModelsByLang: s.stt.models_by_lang ?? {},
          ttsEndpoint: s.tts.endpoint,
          ttsModel: s.tts.model,
          voiceCloningEnabled: s.voice_cloning_enabled,
          defaultVoiceId: s.default_voice_id ?? undefined,
        } satisfies CartesiaSession;
      },
      // Cartesia does STT + TTS but not translation — route each finalized segment through
      // the server's Groq translator over the room WS (spec 0108).
      translate: (text, source, target) => translateViaServer(text, source, target),
      onSubtitle: (speakerId, text, interim, original) => {
        // Cartesia STT interim results are the SOURCE (untranslated) text — only the
        // flushed FINAL is the Groq translation (carrying the source as `original`). Match
        // the server tiers: interim → `original` (so 'translated' mode doesn't flash the
        // foreign source); final → `translation` + `original` so 'both' mode shows the
        // translation big with the original small beneath, like every other tier.
        if (interim) showSubtitle(speakerId, { original: text, interim: true });
        else showSubtitle(speakerId, { translation: text, original, interim: false });
      },
      // Speak the translation in the SPEAKER's cloned voice via the shared PCM graph; the
      // "translated voice" toggle gates it (see `ttsEnabled`).
      playAudio: (speakerId, seq, b64) => {
        if (speakerId !== myId) pcmPlayback.enqueue(speakerId, seq, b64);
      },
      ttsEnabled: () => ttsOn,
      onError: (speakerId, status, message) => {
        // A pipeline gave up (permanent error, or transient survived all retries). Ask the
        // server to fall this listener back to Standard translation for the rest of the call
        // (spec 0108): it switches our receive engine, re-bills at the Standard rate, and
        // replies with `engine_downgraded` — which is where we deactivate + notify the user.
        // We send no toast here so there's exactly one, server-confirmed message.
        console.warn(`[cartesia] giving up on ${speakerId} (${status}: ${message})`);
        ws?.send(JSON.stringify({ type: 'enhanced_fallback', speaker_id: speakerId }));
      },
    });
  }
  return cartesiaManager;
}
/** Start/stop the Enhanced receive pipeline for the current call (spec 0108). Inert
 *  unless a signed-in, listener-pays listener is on a client-direct engine. */
function syncCartesiaForCall(): void {
  const enabled =
    auth.isLoggedIn() &&
    auth.isListenerPays() &&
    engineIsClientDirect(session?.engine, availableEngines) &&
    callMods!.cartesia.CartesiaManager.supported;
  if (enabled) {
    const mgr = ensureCartesiaManager();
    mgr.activate(session?.lang || 'en');
    // Seed everything captured before the manager existed (or cleared by a prior
    // deactivate). MUST include the STREAMS: without them `reconcile` sees no audio and
    // never starts a peer's pipeline, so an Enhanced listener saw NO foreign-speaker
    // subtitles whenever the manager was (re)created after the WebRTC streams had already
    // arrived. Langs/voice-ids alone are not enough.
    for (const [pid, stream] of remoteStreams) mgr.setPeerStream(pid, stream);
    for (const [pid, info] of peerNames) mgr.setPeerLang(pid, info.lang);
    for (const [pid, vid] of peerVoiceIds) mgr.setPeerVoiceId(pid, vid);
  } else cartesiaManager?.deactivate();
}
// Enhanced translate hop (spec 0108): Cartesia can't translate, so each finalized segment is
// sent as `translate_text` over the room WS and matched to its `translated_text` reply by
// `request_id`. Resolves to null on timeout so a lost reply never hangs a pipeline.
const pendingTranslations = new Map<string, (text: string | null) => void>();
let translateReqSeq = 0;
function translateViaServer(
  text: string,
  source: string,
  target: string,
): Promise<string | null> {
  if (!ws || ws.readyState !== WebSocket.OPEN) return Promise.resolve(null);
  const requestId = `tr-${myId}-${translateReqSeq++}`;
  return new Promise<string | null>((resolve) => {
    const settle = (v: string | null) => {
      if (pendingTranslations.delete(requestId)) resolve(v);
    };
    pendingTranslations.set(requestId, settle);
    setTimeout(() => settle(null), 5000);
    ws!.send(
      JSON.stringify({ type: 'translate_text', request_id: requestId, text, source, target }),
    );
  });
}

// ---- Enhanced voice preparation (spec 0108) -------------------------------
// Whether to re-record the signed-in user's voice. The account is the source of truth:
// `/api/user/me` → `has_voice_clone` is set once the server has stored a `cartesia_voice_id`,
// so the prompt is skipped on EVERY device once cloned. This per-device localStorage flag is
// just a fast local short-circuit within a session (and a fallback before `me` is fetched).
const VOICE_CLONED_KEY = 'vox_voice_cloned';
const voiceprepEl = $('voiceprep');
const vpMeter = $('voiceprep-meter');
const vpMeterFill = $('voiceprep-meter-fill');
const vpResult = $('voiceprep-result');
const vpRecordBtn = $('voiceprep-record') as HTMLButtonElement;

type VoicePrepState = 'idle' | 'recording' | 'saving' | 'cloned' | 'failed';

/** Whether this account already has a cloned voice (server truth, or this device's flag). */
function hasVoiceClone(): boolean {
  return !!auth.getUser()?.has_voice_clone || !!localStorage.getItem(VOICE_CLONED_KEY);
}

/** Width of the live capture meter (0..1 of the speech needed for a clone). */
function setVoicePrepMeter(fraction: number): void {
  vpMeterFill.style.width = `${Math.min(100, Math.max(0, fraction * 100))}%`;
}

/** Drive the voice-prep panel through its states, giving live + persistent final feedback. */
function setVoicePrepState(s: VoicePrepState): void {
  voiceprepEl.classList.toggle('recording', s === 'recording');
  show(vpMeter, s === 'recording');
  if (s === 'recording') setVoicePrepMeter(0);
  vpRecordBtn.disabled = s === 'recording' || s === 'saving';
  // "Record my voice" the first time; "Re-record" once a clone exists or after any attempt.
  const redo = hasVoiceClone() || s === 'cloned' || s === 'failed';
  vpRecordBtn.dataset.i18n = redo ? 'voicePrepRerecord' : 'voicePrepRecord';
  vpRecordBtn.textContent = t(redo ? 'voicePrepRerecord' : 'voicePrepRecord');
  // Persistent ✓/✗ outcome; transient status goes to the shared pre-join status line.
  const terminal = s === 'cloned' || s === 'failed';
  show(vpResult, terminal);
  if (s === 'cloned') {
    vpResult.textContent = `✓ ${t('voicePrepSaved')}`;
    vpResult.className = 'voiceprep-result ok';
  } else if (s === 'failed') {
    vpResult.textContent = `✗ ${t('voicePrepFailed')}`;
    vpResult.className = 'voiceprep-result err';
  }
  prejoinStatus.classList.remove('error');
  prejoinStatus.textContent =
    s === 'recording' ? t('voicePrepRecording') : s === 'saving' ? t('voicePrepSaving') : '';
}

/** Record a short mic sample, resolving once ≥3 s of actual speech is captured (energy
 *  threshold, not raw time), or null if too little speech by the time ceiling. */
function recordVoiceSample(
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

/** Show the voice-prep panel for eligible users (signed-in, cloning on, client-direct engine)
 *  in its initial state: a "ready" outcome + Re-record when a clone already exists, otherwise
 *  a prompt to record. Called on pre-join entry and whenever the chosen engine changes. */
function syncVoicePrep(): void {
  const eligible =
    auth.isLoggedIn() &&
    voiceCloningEnabled &&
    engineIsClientDirect(selectedEngine, availableEngines);
  show(voiceprepEl, eligible);
  if (eligible) setVoicePrepState(hasVoiceClone() ? 'cloned' : 'idle');
}

/** Record the user's voice and clone it (spec 0108). Explicit + repeatable from the pre-join,
 *  with live capture feedback and a clear cloned/not-cloned outcome. Best-effort: too little
 *  speech or a clone failure just falls back to a default voice — never blocks the call. */
async function runVoiceClone(): Promise<void> {
  const track = localStream?.getAudioTracks()[0];
  if (!track) return;
  setVoicePrepState('recording');
  try {
    const blob = await recordVoiceSample(new MediaStream([track]), setVoicePrepMeter);
    if (!blob) {
      setVoicePrepState('failed'); // too little speech captured — let them try again
      return;
    }
    setVoicePrepState('saving');
    const res = await cloneVoice(blob, session?.lang);
    if (res.voice_id) {
      localStorage.setItem(VOICE_CLONED_KEY, '1');
      setVoicePrepState('cloned');
    } else {
      setVoicePrepState('failed');
    }
  } catch {
    setVoicePrepState('failed');
  }
}
vpRecordBtn.addEventListener('click', () => void runVoiceClone());
// Listener-pays (spec 0099): once the server sends a `capture_format` message our
// capture is server-driven (PCM iff a Premium listener is present), not chosen from
// our own engine. `null` until the first message → speaker-pays (self-engine) mode.
let serverCaptureFormat: 'pcm' | 'webm' | null = null;
// Speakers whose translated audio arrives from the server (Premium engine, spec
// 0093). We never also browser-TTS them — they'd be heard twice, out of sync.
const premiumSpeakers = new Set<string>();

// Listener-pays (spec 0099): switch our outgoing-audio capture to the format the
// server asks for (PCM16 when a Premium listener is present, else WebM/Opus), so the
// one captured stream can feed both OpenAI and Deepgram. Reuses the same swap as the
// engine-downgrade path; no-ops when the format is already correct.
function applyCaptureFormat(pcm: boolean) {
  const want: 'pcm' | 'webm' = pcm ? 'pcm' : 'webm';
  if (serverCaptureFormat === want) return;
  serverCaptureFormat = want;
  if (!ws || !localStream) return;
  const wasActive = micOn && !!audioCapture;
  audioCapture?.stop();
  audioCapture = pcm
    ? new callMods!.pcmCapture.PcmCapture(localStream, ws)
    : new callMods!.audioCapture.AudioCapture(localStream, ws);
  if (wasActive) audioCapture.start();
}
let micMeter: MicMeter | null = null; // mic-button voice halo (input working)
let chat: ChatManager | null = null;
let lobbyTimer: number | null = null;
let visibilityPublic = true;
let micOn = true;
let camOn = true;
// Virtual background (issue #6, MVP: blur only). `bgMode` is the desired effect;
// `vbg` processes the raw camera into the outgoing track when active.
let bgMode: 'none' | 'blur' = 'none';
let vbg: VirtualBackground | null = null;
// Serializes the camera + background toggles: both mutate the outgoing video
// track and the background swap can await a lazy model load, so overlapping ops
// would race on `vbg` / `localStream`.
let videoBusy = false;
let ttsOn = true; // "translated voice" mode: hear the translation, mute foreign originals
// Subtitle overlay mode, cycled by the bottom-bar button (4-state toggle):
//   'both'       — translation + source line (default, green)
//   'translated' — translation only           (yellow)
//   'original'   — source language only        (orange)
//   'off'        — hidden
// Each click advances both → translated → original → off → both.
type SubtitleMode = 'both' | 'translated' | 'original' | 'off';
const SUBTITLE_CYCLE: SubtitleMode[] = ['both', 'translated', 'original', 'off'];
let subtitleMode: SubtitleMode = 'both';
let handRaised = false;
let pipWindow: Window | null = null;
// PiP-window control buttons (spec 0057): live in the floating window, driven by
// the same toggle fns as the main bar; null whenever PiP is closed.
let pipCtl: { mic: HTMLButtonElement; cam: HTMLButtonElement; share: HTMLButtonElement; hand: HTMLButtonElement; end: HTMLButtonElement } | null = null;
// Keeps the PiP window's grid in sync with the live call (spec 0057 / PiP fixes):
// the PiP is a static clone, so without this a peer leaving leaves a black tile and
// the layout drifts. The observer watches the live grid; rAF coalesces bursts.
let pipObserver: MutationObserver | null = null;
let pipSyncRaf = 0;
let manualClose = false;
let viewMode: 'grid' | 'speaker' = 'grid';
let pinnedPeerId: string | null = null;
let lastSpeakerId: string | null = null;
// Auto-spotlight while someone shares their screen (spec 0089): the sharer's tile
// zooms to the focus view for everyone; on stop we restore whatever was focused
// before. `sharingSpotlightId` is the peer currently auto-spotlighted.
let sharingSpotlightId: string | null = null;
let pinBeforeShare: string | null = null;
let isSharingScreen = false;
let screenStream: MediaStream | null = null;
// Compositor that draws the camera as a PiP overlay onto the shared screen and
// captureStream()s a single composited track (spec 0053). Null when not sharing.
let screenPip: ScreenSharePip | null = null;
// While sharing a tab/window WITH audio, we mix the screen audio into the mic
// and send the mix to peers; these hold the WebAudio graph + mixed track so stop
// can revert the audio sender and release them (spec 0085).
let shareAudioCtx: AudioContext | null = null;
let shareMixTrack: MediaStreamTrack | null = null;
// Composite recording (spec 0010): one WebM with every participant tiled +
// mixed audio. `remoteStreams` is the live source registry the recorder reads
// from (streams weren't stored anywhere before).
let recorder: CompositeRecorder | null = null;
let isRecording = false;
let recTimerId = 0; // 1s interval driving the REC badge MM:SS label
const remoteStreams = new Map<string, MediaStream>();
// Transcript recording (spec 0009): set from room_joined.session_id when the
// backend persists transcripts; drives the in-call indicator + post-call modal.
let activeSessionId: string | null = null;
let transcriptEvents = 0; // speech finals + chat lines seen this call
let callStartedAt = 0; // ms epoch of room_joined (0 = never actually joined)

// VoxTranslate for Business (spec 0106): orgs the signed-in user belongs to
// (fetched once), and whether THIS call is cloud-recording (chosen in pre-join) —
// which drives the recording notice + the end-of-call upload.
let bizOrgs: BusinessOrg[] | null = null;
let bizRecording = false;
let sessionTimerId = 0; // 1s interval driving the session-duration chip (spec 0055)

const peerNames = new Map<
  string,
  { name: string; lang: string; avatar?: string | null; userId?: string | null }
>();
const peerCamOff = new Map<string, boolean>(); // camera-off state from peer_muted
const peerMicMuted = new Map<string, boolean>(); // mic muted state from peer_muted
const peerHandRaised = new Map<string, boolean>(); // hand-raise state
const subtitleTimers = new Map<string, number>();

// ============================================================================
// i18n
// ============================================================================
// Restore the last-used name + language (cached locally, guests included). The
// language drives both the call and the UI, so sync `setUiLang` when restoring it;
// fall back to browser detection when nothing is cached.
const cachedLang = readCache(LANG_CACHE_KEY);
if (cachedLang) {
  langSel.value = cachedLang;
  setUiLang(cachedLang);
} else {
  langSel.value = detectLang();
}
const cachedName = readCache(NAME_CACHE_KEY);
if (cachedName && !nameInput.value) nameInput.value = cachedName;

/** Lazy-load the locale chunk for `lang` (only `en` ships eagerly), then run `after`. The
 *  active UI language's dictionary streams in on demand (spec 0104); until it lands `t()`
 *  falls back to English, which is also what the page server-renders, so there is no flash.
 *  Resolves on a microtask when the locale is already in memory. */
const withLocale = (lang: string, after: () => void): void => {
  void loadLocale(lang).then(after);
};

// Paint immediately with what we have (English, server-rendered into the HTML), then repaint
// once the active locale's chunk arrives. English needs no fetch, so skip the round-trip.
applyI18n();
if (getUiLang() !== 'en') withLocale(getUiLang(), applyI18n);
// Discover translation engines + restore the saved choice (spec 0093). Async;
// the selector reveals itself once the list arrives. Default engine until then.
void initEngines();
// Global connection-status banner (offline / reconnecting / back online).
initNetStatus();
langSel.addEventListener('change', () => {
  setUiLang(langSel.value);
  writeCache(LANG_CACHE_KEY, langSel.value);
  withLocale(langSel.value, repaintLocale);
});

function updateVisHint(): void {
  visHint.textContent = visibilityPublic ? '' : t('privateHint');
}

/** Re-render every locale-dependent home surface after a language switch. Both entry
 *  points — the #lang <select> and the language-first panel — route through here so a new
 *  locale repaints the data-i18n strings, the tier/engine picker (whose card copy is
 *  JS-rendered, not data-i18n) and the visibility hint together. Previously the <select>
 *  path skipped the picker, leaving its copy in the old language until a page refresh. */
function repaintLocale(): void {
  applyI18n();
  renderEngineSelector(); // mode-aware: tier cards (language-first) or the engine selector (legacy)
  updateVisHint();
}

// ============================================================================
// Translation-engine selection (spec 0093)
// ============================================================================
// Fetch the engines this backend offers, restore the persisted choice, render the
// selector (only when there's a real choice), and drive the language dropdown from
// the chosen engine. The selector stays hidden in single-engine deployments.
async function initEngines(): Promise<void> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/engines`);
    if (!res.ok) return; // keep the default engine; selector stays hidden
    const data: unknown = await res.json();
    // Tolerate BOTH the legacy bare array and the {engines, flags} shape (spec 0102), so a
    // client and server can deploy in any order without the picker breaking.
    if (Array.isArray(data)) {
      availableEngines = data as EngineInfo[];
    } else if (data && typeof data === 'object') {
      const obj = data as {
        engines?: EngineInfo[];
        flags?: { language_first_ux?: boolean; voice_cloning_enabled?: boolean };
      };
      availableEngines = obj.engines ?? [];
      languageFirstUx = !!obj.flags?.language_first_ux;
      voiceCloningEnabled = !!obj.flags?.voice_cloning_enabled;
    }
  } catch {
    return; // offline / unreachable → default engine, selector hidden
  }
  selectedEngine = resolveEnginePref(loadEnginePref(), availableEngines);
  if (languageFirstUx) {
    // Language-first flow (spec 0102): pick a target language, then the tiers that output it.
    renderLanguageFirstPicker();
  } else {
    renderEngineSelector();
    rebuildLangOptions();
  }
  // Great-Firewall check (restricted-net.ts): the client-direct Enhanced tier connects the
  // browser straight to a blocked domain, so a restricted network can't use it. The probe is
  // async and fails open, so the picker renders immediately and only re-renders (dropping
  // Enhanced) in the rare restricted case. Silent here — the one user-facing notice fires at
  // join, if a still-selected Enhanced choice has to be overridden (see startCall).
  void isRestrictedNetwork().then((restricted) => {
    if (!restricted || restrictedNet) return;
    restrictedNet = true;
    selectedEngine = resolveEnginePref(loadEnginePref(), enginePool());
    if (languageFirstUx) {
      renderLanguageFirstPicker();
    } else {
      renderEngineSelector();
      rebuildLangOptions();
    }
  });
}

function renderEngineSelector(): void {
  // Language-first mode (spec 0102) owns the picker UI. `renderAccount()` calls this on
  // every auth change, so route those calls to the language-first picker instead — keep
  // the legacy language <select> + engine field hidden and re-render the tier cards for the
  // current language with the (possibly auth-changed) engine pool, WITHOUT resetting the
  // user's chosen language.
  if (languageFirstUx) {
    if (langPickerWired) {
      langField.hidden = true;
      engineField.hidden = true;
      langfirstField.hidden = false;
      renderTierCards();
    } else {
      renderLanguageFirstPicker();
    }
    return;
  }
  // Guests always use Standard — Premium/Pro need credits. Hide the selector and pin
  // the choice so the join always sends 'standard'; only signed-in users get to pick.
  if (!auth.isLoggedIn()) {
    selectedEngine = 'standard';
    engineField.hidden = true;
    return;
  }
  // A one-engine deployment (the common case until Premium is provisioned) has no
  // choice to make — keep the selector out of the way.
  if (availableEngines.length < 2) {
    engineField.hidden = true;
    return;
  }
  engineOptions.replaceChildren();
  for (const e of enginePool()) {
    const active = e.id === selectedEngine;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'engine-opt' + (active ? ' active' : '');
    btn.setAttribute('role', 'radio');
    btn.setAttribute('aria-checked', String(active));
    btn.dataset.engine = e.id;
    const head = document.createElement('span');
    head.className = 'engine-opt-head';
    const name = document.createElement('span');
    name.className = 'engine-opt-name';
    name.textContent = e.display_name;
    head.append(name);
    // "Fastest" chip on the client-direct (Enhanced) tier — browser ↔ provider with
    // no server relay hop, so it's the lowest-latency option (spec 0101).
    if (e.capabilities.client_direct) {
      const badge = document.createElement('span');
      badge.className = 'engine-opt-badge';
      badge.textContent = t('engineBadgeEnhanced');
      head.append(badge);
    }
    const rate = document.createElement('span');
    rate.className = 'engine-opt-rate';
    rate.textContent = formatRate(e.rate_per_minute);
    head.append(rate);
    const desc = document.createElement('span');
    desc.className = 'engine-opt-desc';
    // Localized, jargon-free copy keyed by tier; fall back to the server string for
    // an unknown/future engine (#236).
    const descKey = engineDescKey(e.tier);
    desc.textContent = descKey ? t(descKey) : e.description;
    btn.append(head, desc);
    // Transparency: explain how the rate scales. Listener-pays (spec 0099) bills the
    // listener per source they hear; speaker-pays (spec 0093) bills per target language.
    if (e.capabilities.cost_scales_per_language) {
      const note = document.createElement('span');
      note.className = 'engine-opt-note';
      note.textContent = t(auth.isListenerPays() ? 'engineCostPerSource' : 'engineCostPerLanguage');
      btn.append(note);
    }
    btn.addEventListener('click', () => selectEngine(e.id));
    engineOptions.appendChild(btn);
  }
  engineField.hidden = false;
}

function selectEngine(id: string): void {
  if (id === selectedEngine) return;
  selectedEngine = id;
  saveEnginePref(id);
  renderEngineSelector();
  rebuildLangOptions();
  syncVoicePrep(); // voice-prep only applies to the client-direct (Enhanced) engine.
  syncAudioSettingsBtn();
}

// Rebuild the language dropdown from the COMMON languages across all engines
// (spec 0094): a room can mix engines, so a language is only safe to offer if
// every engine can produce it — otherwise a peer on another engine couldn't
// translate to it. So the list is stable regardless of the selected engine.
// Preserves the current selection when still valid; keeps "auto" first.
function rebuildLangOptions(): void {
  const allowed = commonLangs(availableEngines, [...SUPPORTED]);
  const prev = langSel.value;
  const codes = ['auto', ...allowed];
  langSel.replaceChildren();
  for (const code of codes) {
    const o = document.createElement('option');
    o.value = code;
    if (code === 'auto') {
      o.setAttribute('data-i18n', 'langAuto');
      o.textContent = t('langAuto');
    } else {
      o.textContent = ENDONYM[code] ?? code;
    }
    langSel.appendChild(o);
  }
  const next = codes.includes(prev) ? prev : (allowed[0] ?? 'auto');
  if (next !== prev) {
    // The chosen engine dropped the current UI language — follow the new value
    // (this select doubles as the UI language; see the change handler above).
    langSel.value = next;
    setUiLang(next);
    withLocale(next, () => {
      applyI18n();
      updateVisHint();
    });
  } else {
    langSel.value = next;
  }
}

// ============================================================================
// Language-first picker (spec 0102)
// ============================================================================
// Flip the flow: pick a TARGET language from the full union (region-grouped, searchable),
// then choose among the tiers that can output it (cheapest pre-selected). Gated by the
// LANGUAGE_FIRST_UX flag. The hidden #lang <select> stays the canonical value holder, so
// the join payload, UI language and reconnect path are unchanged — this just drives it.

/** The engines a user may choose among: guests are pinned to Standard (premium tiers need
 *  credits), exactly as the legacy selector does (`renderEngineSelector`). */
function enginePool(): EngineInfo[] {
  // On a Great-Firewall-restricted network, drop the client-direct tier(s) (Enhanced
  // connects the browser straight to a blocked domain). No-op until the reachability
  // probe flags `restrictedNet`, so non-China pickers are unchanged.
  if (!auth.isLoggedIn()) {
    const std = availableEngines.filter((e) => e.id === DEFAULT_ENGINE_ID);
    return selectableEngines(std.length ? std : availableEngines, restrictedNet);
  }
  return selectableEngines(availableEngines, restrictedNet);
}

/** Recently-used target languages. Best-effort localStorage, SEEDED from the browser's
 *  preferred languages so a fresh device still surfaces sensible choices (no reliance on
 *  storage alone). Deduped, browser langs after explicit picks. */
function readRecentLangs(): string[] {
  const raw = readCache(RECENT_LANGS_KEY);
  const fromCache = raw ? raw.split(',').filter(Boolean) : [];
  const fromNav = (navigator.languages ?? []).map((l) => l.slice(0, 2).toLowerCase());
  return [...new Set([...fromCache, ...fromNav])];
}

function pushRecentLang(code: string): void {
  const next = [code, ...readRecentLangs().filter((c) => c !== code)].slice(0, 5);
  writeCache(RECENT_LANGS_KEY, next.join(','));
}

/** Swap the legacy language <select> + engine selector for the language-first picker. */
function renderLanguageFirstPicker(): void {
  langField.hidden = true;
  engineField.hidden = true;
  langfirstField.hidden = false;

  const offered = offeredLanguageCodes(enginePool());
  const cached = readCache(LANG_CACHE_KEY);
  let code = cached && offered.has(cached) ? cached : detectLang();
  if (!offered.has(code)) {
    code = readRecentLangs().find((c) => offered.has(c)) ?? [...offered][0] ?? 'en';
  }
  wireLangPicker();
  selectLang(code, false); // restore without re-stamping "recent"
}

function wireLangPicker(): void {
  if (langPickerWired) return;
  langPickerWired = true;
  langTrigger.addEventListener('click', toggleLangPanel);
  langSearch.addEventListener('input', () => renderLangPanelList(langSearch.value));
  langSearch.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      closeLangPanel();
      langTrigger.focus();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      langPanelList.querySelector<HTMLButtonElement>('.lang-opt')?.click();
    }
  });
  // Close when clicking outside the picker.
  document.addEventListener('click', (e) => {
    if (langPanel.hidden) return;
    if (!langfirstField.contains(e.target as Node)) closeLangPanel();
  });
}

function toggleLangPanel(): void {
  if (langPanel.hidden) openLangPanel();
  else closeLangPanel();
}

function openLangPanel(): void {
  langPanel.hidden = false;
  langTrigger.setAttribute('aria-expanded', 'true');
  langSearch.value = '';
  renderLangPanelList('');
  langSearch.focus();
}

function closeLangPanel(): void {
  langPanel.hidden = true;
  langTrigger.setAttribute('aria-expanded', 'false');
}

/** Build one selectable language row. */
function makeLangOption(m: LangMeta): HTMLButtonElement {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'lang-opt' + (m.code === selectedLang ? ' selected' : '');
  btn.setAttribute('role', 'option');
  btn.setAttribute('aria-selected', String(m.code === selectedLang));
  btn.dir = m.rtl ? 'rtl' : 'ltr';
  const flag = document.createElement('span');
  flag.className = 'lang-opt-flag';
  flag.textContent = m.flag;
  const native = document.createElement('span');
  native.className = 'lang-opt-native';
  native.textContent = m.native;
  const en = document.createElement('span');
  en.className = 'lang-opt-en';
  en.textContent = m.english;
  btn.append(flag, native, en);
  btn.addEventListener('click', () => selectLang(m.code));
  return btn;
}

/** Render the panel: a flat match list when searching, else recently-used (pinned) + the
 *  region-grouped collapsible sections (the section holding the current language is open). */
function renderLangPanelList(query: string): void {
  const pool = enginePool();
  langPanelList.replaceChildren();
  const q = query.trim();

  if (q) {
    const matches = searchLanguages(q, pool);
    langPanelEmpty.hidden = matches.length > 0;
    for (const m of matches) langPanelList.appendChild(makeLangOption(m));
    return;
  }
  langPanelEmpty.hidden = true;

  // Recently-used, pinned at the top (only languages this deployment offers).
  const offered = offeredLanguageCodes(pool);
  const recents = readRecentLangs()
    .filter((c) => offered.has(c))
    .map((c) => langMeta(c))
    .filter((m): m is LangMeta => !!m)
    .slice(0, 5);
  if (recents.length) {
    const label = document.createElement('div');
    label.className = 'lang-recent-label';
    label.textContent = t('langRecent');
    langPanelList.appendChild(label);
    for (const m of recents) langPanelList.appendChild(makeLangOption(m));
  }

  // Region-grouped collapsible sections.
  for (const group of languagesByRegion(pool)) {
    const details = document.createElement('details');
    details.className = 'lang-region';
    if (group.languages.some((l) => l.code === selectedLang)) details.open = true;
    const summary = document.createElement('summary');
    summary.textContent = t(`region_${group.region}`);
    details.appendChild(summary);
    for (const m of group.languages) details.appendChild(makeLangOption(m));
    langPanelList.appendChild(details);
  }
}

/** Keep the hidden legacy <select> able to hold `code` (the join + UI-language source). */
function ensureLangOption(code: string): void {
  if (![...langSel.options].some((o) => o.value === code)) {
    const o = document.createElement('option');
    o.value = code;
    o.textContent = langMeta(code)?.native ?? code;
    langSel.appendChild(o);
  }
}

/** Commit a target-language choice: drives the canonical <select>, the UI language (with
 *  RTL), the trigger label, the tier cards, and persistence. */
function selectLang(code: string, persist = true): void {
  selectedLang = code;
  ensureLangOption(code);
  langSel.value = code;
  setUiLang(code); // a no-op for a language without a UI translation; falls back to English
  if (persist) {
    writeCache(LANG_CACHE_KEY, code);
    pushRecentLang(code);
  }
  // The chosen UI locale streams in on demand (spec 0104); repaint every locale-dependent
  // surface once it lands (shared with the #lang <select> path). `applyI18n` also flips
  // document dir for RTL (i18n.ts).
  withLocale(code, repaintLocale);
  updateLangTrigger(); // langmap-driven (flag/endonym) — no UI dictionary needed
  closeLangPanel();
}

function updateLangTrigger(): void {
  const m = langMeta(selectedLang);
  langTriggerFlag.textContent = m?.flag ?? '🌐';
  langTriggerText.textContent = m ? `${m.native} (${m.english})` : selectedLang;
  langTrigger.dir = m?.rtl ? 'rtl' : 'ltr';
}

/** Render the tier cards for the chosen language (cheapest pre-selected), reusing the
 *  legacy `.engine-opt*` styling, plus a balance/estimate + low-credit upsell note. */
function renderTierCards(): void {
  const pool = enginePool();
  const tiers = getAvailableTiers(selectedLang, pool);
  tierOptions.replaceChildren();
  if (tiers.length === 0) {
    tierField.hidden = true;
    return;
  }
  // Pre-select the cheapest tier whenever the current choice can't output this language.
  if (!tiers.some((e) => e.id === selectedEngine)) {
    selectedEngine = cheapestTier(selectedLang, pool)?.id ?? tiers[0].id;
    saveEnginePref(selectedEngine);
  }
  for (const e of tiers) {
    const active = e.id === selectedEngine;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'engine-opt' + (active ? ' active' : '');
    btn.setAttribute('role', 'radio');
    btn.setAttribute('aria-checked', String(active));
    btn.dataset.engine = e.id;
    const head = document.createElement('span');
    head.className = 'engine-opt-head';
    const name = document.createElement('span');
    name.className = 'engine-opt-name';
    name.textContent = e.display_name;
    head.append(name);
    if (e.capabilities.client_direct) {
      const badge = document.createElement('span');
      badge.className = 'engine-opt-badge';
      badge.textContent = t('engineBadgeEnhanced');
      head.append(badge);
    }
    const rate = document.createElement('span');
    rate.className = 'engine-opt-rate';
    rate.textContent = formatRate(e.rate_per_minute);
    head.append(rate);
    const desc = document.createElement('span');
    desc.className = 'engine-opt-desc';
    const descKey = engineDescKey(e.tier);
    desc.textContent = descKey ? t(descKey) : e.description;
    btn.append(head, desc);
    // Estimated minutes at this tier from the user's balance (1 credit = $1; spec 0102).
    const bal = auth.getUser()?.balance;
    if (billing && typeof bal === 'number' && e.rate_per_minute > 0) {
      const est = document.createElement('span');
      est.className = 'engine-opt-est';
      est.textContent = t('tierEstMinutes').replace(
        '{min}',
        String(Math.floor(bal / e.rate_per_minute)),
      );
      btn.append(est);
    }
    btn.addEventListener('click', () => selectTier(e.id));
    tierOptions.appendChild(btn);
  }
  tierField.hidden = false;
  renderTierNote(tiers);
}

function selectTier(id: string): void {
  if (id === selectedEngine) return;
  selectedEngine = id;
  saveEnginePref(id);
  renderTierCards();
  syncVoicePrep(); // voice-prep only applies to the client-direct (Enhanced) engine.
  syncAudioSettingsBtn();
}

/** The note below the tier cards: a single-tier explanation, the credit balance, and a
 *  low-balance "top up" nudge — never blocking, just informative (spec 0102). */
function renderTierNote(tiers: EngineInfo[]): void {
  tierNote.replaceChildren();
  const parts: Node[] = [];
  const langName = langMeta(selectedLang)?.native ?? selectedLang;

  if (tiers.length === 1) {
    const only = document.createElement('span');
    only.textContent = t('tierOnlyOption')
      .replace('{tier}', tiers[0].display_name)
      .replace('{lang}', langName);
    parts.push(only);
  }

  const bal = auth.getUser()?.balance;
  if (billing && typeof bal === 'number') {
    const balLine = document.createElement('span');
    balLine.textContent = t('tierBalance').replace('{balance}', auth.formatCredits(bal));
    if (parts.length) parts.push(document.createElement('br'));
    parts.push(balLine);
    // Low-balance upsell: fewer than ~5 minutes left on the selected tier.
    const sel = tiers.find((e) => e.id === selectedEngine);
    if (sel && sel.rate_per_minute > 0 && bal < sel.rate_per_minute * 5) {
      const link = document.createElement('a');
      link.href = '#';
      link.textContent = t('tierTopUp');
      link.addEventListener('click', (e) => {
        e.preventDefault();
        openBuyModal();
      });
      parts.push(document.createTextNode(' · '));
      parts.push(link);
    }
  }

  if (parts.length === 0) {
    tierNote.hidden = true;
    return;
  }
  tierNote.append(...parts);
  tierNote.hidden = false;
}

// ============================================================================
// Home + lobby
// ============================================================================
function randomRoom(): string {
  const chars = 'abcdefghjkmnpqrstuvwxyz23456789';
  let s = '';
  for (let i = 0; i < 6; i++) s += chars[Math.floor(Math.random() * chars.length)];
  return s;
}
// A shared invite link (spec 0082) lands as `?room=CODE`: prefill it AND drop the
// invitee straight into the pre-join preview once home is ready (consumed once in
// enterHome), so they just confirm name + camera and join — no extra home-screen tap.
// Otherwise start from a fresh random room.
let pendingInviteRoom = parseRoomParam(location.search);
// Entry URL captured at load (before any client-side nav) so join analytics can tell how the
// user arrived: `&src=meeting` (scheduled), `?room=` (shared invite link), or direct.
const entrySearch = location.search;
roomInput.value = pendingInviteRoom ?? randomRoom();
$('dice').addEventListener('click', () => (roomInput.value = randomRoom()));

visGroup.addEventListener('click', (e) => {
  const btn = (e.target as HTMLElement).closest('.seg-btn') as HTMLElement | null;
  if (!btn) return;
  // Public rooms need an account: a guest tapping "public" gets the benefits modal
  // (→ sign in), not a silent switch (spec 0022 / 0036).
  if (btn.dataset.vis === 'public' && billing && !auth.isLoggedIn()) {
    openSigninGate();
    return;
  }
  visibilityPublic = btn.dataset.vis === 'public';
  visGroup.querySelectorAll('.seg-btn').forEach((b) => {
    b.classList.toggle('active', b === btn);
    b.setAttribute('aria-pressed', String(b === btn));
  });
  updateVisHint();
});

function homeStatusMsg(msg: string, isError = false): void {
  homeStatus.textContent = msg;
  homeStatus.classList.toggle('error', isError);
}

enterBtn.addEventListener('click', () => {
  const room = roomInput.value.trim().toLowerCase();
  if (!room) return homeStatusMsg(t('enterRoom'), true);
  // Belt-and-suspenders: a guest can't create a public room (spec 0022 / 0036).
  if (visibilityPublic && billing && !auth.isLoggedIn()) return openSigninGate();
  track('start_call', { visibility: visibilityPublic ? 'public' : 'private' });
  goPrejoin(room, visibilityPublic);
});

async function fetchRooms(): Promise<void> {
  try {
    const res = await fetch(`${HTTP_BASE}/rooms`, { cache: 'no-store' });
    const data = await res.json();
    renderRooms(data.rooms || []);
  } catch {
    /* keep last render */
  }
}

function renderRooms(
  rooms: Array<{
    room: string;
    count: number;
    participants: Array<{ name: string; lang: string; avatar?: string | null }>;
  }>,
): void {
  // Hide full rooms: a 4/4 mesh is at capacity (server rooms::MAX_PEERS), so
  // tapping one would just bounce with room_full — never list a room the user
  // can't actually join.
  const joinable = rooms.filter((r) => r.count < 4);
  roomsList.innerHTML = '';
  if (!joinable.length) {
    const empty = document.createElement('div');
    empty.className = 'lobby-empty';
    empty.textContent = t('noPublicRooms');
    roomsList.appendChild(empty);
    return;
  }
  for (const r of joinable) {
    const item = document.createElement('button');
    item.className = 'room-item';
    item.type = 'button';
    const main = document.createElement('div');
    main.className = 'room-item-main';
    const code = document.createElement('span');
    code.className = 'room-item-code';
    code.textContent = r.room;
    const count = document.createElement('span');
    count.className = 'room-item-count';
    count.innerHTML = `${icon('users', 13)} ${r.count}/4`;
    main.append(code, count);
    // Overlapping avatar stack — a social-style "who's here" glance (spec 0072).
    const avatars = document.createElement('div');
    avatars.className = 'room-item-avatars';
    for (const m of r.participants) {
      const av = document.createElement('span');
      av.className = 'ri-av';
      av.title = m.name;
      fillAvatar(av, m.name, m.avatar, 56, 1);
      avatars.appendChild(av);
    }
    const members = document.createElement('div');
    members.className = 'room-item-members';
    for (const m of r.participants) {
      const chip = document.createElement('span');
      chip.className = 'chip';
      chip.textContent = `${FLAG[m.lang] || ''} ${m.name}`.trim();
      members.appendChild(chip);
    }
    item.append(main, avatars, members);
    item.addEventListener('click', () => {
      // Guests can't join public rooms (spec 0022): explain why + the perks of an
      // account instead of sending them to pre-join.
      if (billing && !auth.isLoggedIn()) return openSigninGate();
      goPrejoin(r.room, true);
    });
    roomsList.appendChild(item);
  }
}

// Public webinars discovery on the home page (spec: webinar visibility). Best-effort:
// listPublicWebinars() swallows failures to [], and an empty list hides the whole
// card so the lobby never shows an empty "Public webinars" section.
async function fetchPublicWebinars(): Promise<void> {
  const webinars = await listPublicWebinars();
  renderPublicWebinars(webinars);
}

function renderPublicWebinars(webinars: PublicWebinarListItem[]): void {
  publicWebinarsList.innerHTML = '';
  if (!webinars.length) {
    show(publicWebinarsCard, false); // hide the section entirely when there's nothing to show
    return;
  }
  show(publicWebinarsCard, true);
  for (const w of webinars) {
    const item = document.createElement('button');
    item.className = 'webinar-item';
    item.type = 'button';

    const main = document.createElement('div');
    main.className = 'webinar-item-main';
    const title = document.createElement('span');
    title.className = 'webinar-item-title';
    title.textContent = w.title;
    main.appendChild(title);
    const live = isWebinarLive(w);
    if (live) {
      // Viewers count on the right of the title, only while broadcasting.
      const viewers = document.createElement('span');
      viewers.className = 'webinar-item-viewers';
      viewers.innerHTML = `${icon('users', 13)} ${t('webinarViewersCount').replace('{n}', String(w.viewers))}`;
      main.appendChild(viewers);
    }

    // Secondary meta row: LIVE badge (or a scheduled hint) + the source language.
    const meta = document.createElement('div');
    meta.className = 'webinar-item-meta';
    if (live) {
      const badge = document.createElement('span');
      badge.className = 'webinar-live-badge';
      badge.textContent = t('webinarLiveNow');
      meta.appendChild(badge);
    } else {
      const when = formatScheduledStart(w.scheduled_start);
      if (when) {
        const hint = document.createElement('span');
        hint.textContent = when;
        meta.appendChild(hint);
      }
    }
    const lang = document.createElement('span');
    lang.textContent = `${FLAG[w.source_language] || ''} ${(ENDONYM[w.source_language] ?? w.source_language)}`.trim();
    meta.appendChild(lang);

    item.append(main, meta);
    // A webinar card is a FULL navigation to the participant page (/w/{code}), not a
    // room prejoin — open the server-provided join_url verbatim.
    item.addEventListener('click', () => {
      window.location.href = w.join_url;
    });
    publicWebinarsList.appendChild(item);
  }
}

function startLobby(): void {
  fetchRooms();
  void fetchPublicWebinars();
  if (!lobbyTimer) {
    lobbyTimer = window.setInterval(() => {
      fetchRooms();
      void fetchPublicWebinars();
    }, 3000);
  }
  startNotifPolling(); // poll in-app alerts while on the home/profile screens
}
function stopLobby(): void {
  if (lobbyTimer) {
    clearInterval(lobbyTimer);
    lobbyTimer = null;
  }
  stopNotifPolling(); // no alerts while in pre-join / a call
}
$('refresh').addEventListener('click', () => {
  fetchRooms();
  void fetchPublicWebinars();
});

// Guest sign-in gate (spec 0022): a guest clicking an online public room is shown
// why an account is needed and what they gain, instead of reaching pre-join.
const signinGateModal = $('signin-gate-modal');
function openSigninGate(): void {
  show(signinGateModal, true);
}
$('signin-gate-dismiss').addEventListener('click', () => show(signinGateModal, false));
$('signin-gate-signin').addEventListener('click', () => {
  show(signinGateModal, false);
  showLogin();
});

// ============================================================================
// Pre-join: camera preview + device selectors
// ============================================================================
async function goPrejoin(room: string, isPublic: boolean): Promise<void> {
  session = { room, lang: langSel.value, name: nameInput.value.trim(), isPublic, engine: selectedEngine };
  // Remember what was actually used to join, so it's pre-filled next time (guests too).
  writeCache(NAME_CACHE_KEY, session.name);
  writeCache(LANG_CACHE_KEY, session.lang);
  // Warm the lazy in-call modules (spec 0105) now, so the chunk downloads while the camera and
  // devices initialise — by the time the user clicks join, `startCall` awaits an already-settled
  // promise. Errors are swallowed here; startCall re-awaits and surfaces a real failure.
  void ensureCallModules().catch(() => {});
  // Warm the active locale's lazy i18n chunk alongside the call modules, so it is
  // already in memory by the time `startCall` awaits it (no in-call English flash).
  void loadLocale(getUiLang());
  stopLobby();
  homeScreen.classList.add('hidden');
  prejoinScreen.classList.remove('hidden');
  prejoinRoom.textContent = room;
  prejoinVis.textContent = isPublic ? t('public') : t('private');
  prejoinStatus.textContent = '';
  void setupBizPrejoin();
  syncVoicePrep(); // Enhanced voice-prep panel (spec 0108): show/init for eligible users.
  syncAudioSettingsBtn(); // Audio settings only relevant for Standard tier.
  micOn = true;
  camOn = true;
  try {
    await acquireMedia();
    await populateDevices();
  } catch {
    prejoinStatus.textContent = t('camMicDenied');
    prejoinStatus.classList.add('error');
  }
}

// Apply the current mic/camera toggle state to the preview stream + UI. Used in
// the pre-join screen so you enter the room already muted / camera-off.
function applyPreToggles(): void {
  if (localStream) {
    localStream.getAudioTracks().forEach((tr) => (tr.enabled = micOn));
    // Camera off must fully release the device so the hardware LED turns off —
    // disabling the track alone keeps the camera active. We stop the track but
    // leave it in the stream as an (ended) placeholder so a video sender is still
    // negotiated at join; togglePreCam swaps in a fresh track when re-enabled.
    if (!camOn) localStream.getVideoTracks().forEach((tr) => tr.stop());
  }
  const hasLiveVideo =
    !!localStream && localStream.getVideoTracks().some((tr) => tr.readyState === 'live');
  if (camOn && !hasLiveVideo) camOn = false;
  // Preview overlay when the camera is off: show the Google photo when logged in,
  // initials otherwise (same as the in-call camera-off cell).
  previewOff.hidden = camOn && hasLiveVideo;
  if (!previewOff.hidden) {
    const name = nameInput.value.trim() || t('namePlaceholder');
    const avatar =
      billing && auth.isLoggedIn() ? auth.avatarUrl(auth.getUser()?.avatar_url, 192) : null;
    if (avatar) {
      previewAvatar.textContent = '';
      previewAvatar.style.background = 'none';
      const img = document.createElement('img');
      img.className = 'preview-avatar-img';
      img.referrerPolicy = 'no-referrer';
      img.alt = '';
      img.src = avatar;
      img.addEventListener('error', () => {
        // Fall back to initials if the photo fails to load.
        img.remove();
        previewAvatar.textContent = name.slice(0, 2).toUpperCase();
        previewAvatar.style.background = avatarGradient(name);
      });
      previewAvatar.appendChild(img);
    } else {
      previewAvatar.textContent = name.slice(0, 2).toUpperCase();
      previewAvatar.style.background = avatarGradient(name);
    }
  }
  preMic.classList.toggle('active-danger', !micOn);
  preMic.innerHTML = icon(micOn ? 'mic' : 'mic-off');
  preCam.classList.toggle('active-danger', !camOn);
  preCam.innerHTML = icon(camOn ? 'video' : 'video-off');
}

preMic.addEventListener('click', () => {
  micOn = !micOn;
  applyPreToggles();
});
preCam.addEventListener('click', () => {
  void togglePreCam();
});

async function togglePreCam(): Promise<void> {
  camOn = !camOn;
  // Turning the camera back on re-acquires the released device, swapping the
  // ended placeholder for a fresh track (the audio track is left untouched).
  const hasLiveVideo = !!localStream && localStream.getVideoTracks().some((t) => t.readyState === 'live');
  if (camOn && localStream && !hasLiveVideo) {
    const track = await acquireVideoTrack();
    if (track) {
      localStream.getVideoTracks().forEach((t) => {
        t.stop();
        localStream!.removeTrack(t);
      });
      localStream.addTrack(track);
      previewVideo.srcObject = localStream;
      void previewVideo.play().catch(() => {});
    }
  }
  applyPreToggles();
}

/** Video constraints honouring the selected camera device. */
function videoConstraints(): MediaTrackConstraints {
  const camId = camSelect.value;
  // Mobile sends to up to 3 peers over a mesh on a metered/variable uplink, so
  // capture lower (480p) — desktop keeps 720p (spec 0030).
  const cap = IS_MOBILE ? { w: 640, h: 480 } : { w: 1280, h: 720 };
  return {
    width: { ideal: cap.w, max: cap.w },
    height: { ideal: cap.h, max: cap.h },
    frameRate: { ideal: 24, max: 30 },
    ...(camId ? { deviceId: { exact: camId } } : {}),
  };
}

/** Open the selected camera and return its video track (null on failure). */
async function acquireVideoTrack(): Promise<MediaStreamTrack | null> {
  try {
    const s = await navigator.mediaDevices.getUserMedia({ video: videoConstraints() });
    return s.getVideoTracks()[0] ?? null;
  } catch {
    return null;
  }
}

async function acquireMedia(): Promise<void> {
  const micId = micSelect.value;
  const audio: MediaTrackConstraints = {
    channelCount: 1,
    echoCancellation: true,
    noiseSuppression: true,
    autoGainControl: true,
    ...(micId ? { deviceId: { exact: micId } } : {}),
  };
  if (localStream) localStream.getTracks().forEach((t2) => t2.stop());
  try {
    localStream = await navigator.mediaDevices.getUserMedia({ audio, video: videoConstraints() });
  } catch (e) {
    // Fall back to audio-only (no camera available / denied video).
    if (e instanceof Error && e.name === 'NotAllowedError') {
      track('camera_permission_denied');
    }
    try {
      localStream = await navigator.mediaDevices.getUserMedia({ audio });
    } catch (audioErr) {
      if (audioErr instanceof Error && audioErr.name === 'NotAllowedError') {
        track('mic_permission_denied');
      }
      throw audioErr;
    }
  }
  // applyPreToggles releases the camera again if it's currently toggled off.
  previewVideo.srcObject = localStream;
  void previewVideo.play().catch(() => {});
  // Re-apply the current mic/camera toggle state to the new tracks.
  applyPreToggles();
}

async function populateDevices(): Promise<void> {
  const devices = await navigator.mediaDevices.enumerateDevices();
  const cams = devices.filter((d) => d.kind === 'videoinput');
  const mics = devices.filter((d) => d.kind === 'audioinput');
  const curCam = localStream?.getVideoTracks()[0]?.getSettings().deviceId || '';
  const curMic = localStream?.getAudioTracks()[0]?.getSettings().deviceId || '';
  fillDeviceSelect(camSelect, cams, curCam, 'Camera');
  fillDeviceSelect(micSelect, mics, curMic, 'Mic');
}

function fillDeviceSelect(sel: HTMLSelectElement, devices: MediaDeviceInfo[], current: string, fallback: string): void {
  sel.innerHTML = '';
  devices.forEach((d, i) => {
    const opt = document.createElement('option');
    opt.value = d.deviceId;
    opt.textContent = d.label || `${fallback} ${i + 1}`;
    if (d.deviceId === current) opt.selected = true;
    sel.appendChild(opt);
  });
  sel.disabled = devices.length === 0;
}

camSelect.addEventListener('change', () => acquireMedia());
micSelect.addEventListener('change', () => acquireMedia());

$('back-btn').addEventListener('click', () => {
  if (localStream) localStream.getTracks().forEach((tr) => tr.stop());
  localStream = null;
  prejoinScreen.classList.add('hidden');
  homeScreen.classList.remove('hidden');
  startLobby();
});

$('join-btn').addEventListener('click', () => {
  if (!localStream || !session) return;
  unlockTts(); // iOS: prime speechSynthesis inside the tap so translations play
  pcmPlayback.unlock(); // same for the Premium translated-audio context (spec 0093)
  // Voice cloning (spec 0108) is now an explicit, repeatable pre-join step (the voice-prep
  // panel) — not coupled to join. Whatever the user did there (clone / re-record / skip) is
  // already applied; just enter the call.
  void startCall();
});

// ============================================================================
// Call
// ============================================================================

// ICE servers for WebRTC (spec 0026): fetched per-call from the server, which
// returns public STUN plus a time-limited TURN relay when coturn is configured.
// Passed to the mesh; falls back to the mesh's built-in STUN on failure.
let iceServers: RTCIceServer[] | undefined;
// Set once per call from the Great-Firewall reachability probe (restricted-net.ts).
// When true we ask /api/ice for the turns://:443 profile AND force the mesh through
// relay — scoped to this (China-side) client, so non-China calls are unchanged.
let restrictedNet = false;
async function fetchIceServers(restricted: boolean): Promise<RTCIceServer[] | undefined> {
  try {
    // Restricted clients get the GFW-survivable profile (turns://:443 TLS relay);
    // everyone else gets the default response, byte-for-byte as before.
    const url = restricted ? `${HTTP_BASE}/api/ice?restricted=1` : `${HTTP_BASE}/api/ice`;
    const res = await fetch(url, { cache: 'no-store' });
    const data = await res.json();
    return Array.isArray(data?.iceServers) ? (data.iceServers as RTCIceServer[]) : undefined;
  } catch {
    return undefined;
  }
}

async function startCall(): Promise<void> {
  // Track call started with context
  // Guests have no server-side consent record; enforce the 18+/ToS self-attestation
  // here too so a guest can't reach a call without it (accounts are gated server-side).
  if (billing && !auth.isLoggedIn() && !auth.guestConsentGiven()) {
    show(consentModal, true);
    return;
  }
  if (!session || !localStream) return;
  // Bail before entering a call this browser can't run: in-app browsers / restricted
  // WebViews (the call link opened inside Instagram, Gmail, etc.) and insecure contexts
  // don't expose RTCPeerConnection, so the mesh would crash on the first peer. Stay on
  // pre-join and tell the user to open the link in a real browser.
  if (!webrtcSupported()) {
    prejoinStatus.textContent = t('webrtcUnsupported');
    prejoinStatus.classList.add('error');
    return;
  }
  // The in-call modules (warmed at pre-join, usually already settled) must be present before we
  // show the call UI. On failure — e.g. the chunk couldn't be fetched offline — stay on pre-join
  // and surface an error instead of entering a broken call (spec 0105).
  try {
    await ensureCallModules();
  } catch {
    prejoinStatus.textContent = t('loadFailed');
    prejoinStatus.classList.add('error');
    return;
  }
  // i18n is lazy-loaded per locale (spec 0104). Make sure the active UI language's
  // dictionary has landed BEFORE we render any in-call UI, otherwise the
  // dynamically-created labels (cells, badges, tooltips) fall back to English
  // because `t()` reads synchronously and there is no re-translate pass for them.
  await loadLocale(getUiLang());
  prejoinScreen.classList.add('hidden');
  callScreen.classList.remove('hidden');
  
  // Track successful call join. How the user reached this call: a scheduled meeting (flagged
  // in sessionStorage by meetings.ts, surviving both the in-app and full-navigation join
  // paths), a shared invite link (`?room=` in the entry URL), or a direct/manual join.
  let fromMeeting = false;
  try {
    fromMeeting = sessionStorage.getItem('vox_join_src') === 'meeting';
    sessionStorage.removeItem('vox_join_src');
  } catch {
    /* sessionStorage unavailable — fall through to URL detection */
  }
  const method = fromMeeting
    ? 'scheduled'
    : new URLSearchParams(entrySearch).has('room')
      ? 'invite_email'
      : 'direct_link';
  track('room_joined', { method, is_returning_user: !!billing && auth.isLoggedIn() });
  callRoom.textContent = session.room;
  callVis.textContent = session.isPublic ? t('public') : t('private');
  // Your name + lang live in the meta row instead of on the self tile (see #stage-self).
  stageSelfName.textContent = session.name || t('you');
  stageSelfLang.textContent = `${FLAG[session.lang] || ''} ${session.lang.toUpperCase()}`.trim();
  show(stageSelf, true);
  // First-entry call tour (deferred to the next frames so the control bar has laid out).
  onboarding.maybeAutoStartCall();
  videoGrid.innerHTML = '';
  videoGrid.dataset.mode = 'grid';
  peerNames.clear();

  // micOn / camOn carry over from the pre-join toggles.
  setControlState();
  // Hide controls whose APIs are unavailable or unusable in this browser.
  show(btnRecord, isRecordingSupported()); // Safari etc.: no MediaRecorder → no button
  show(btnShare, !IS_MOBILE && !!navigator.mediaDevices?.getDisplayMedia); // no screen share on mobile
  show(btnPip, 'documentPictureInPicture' in window); // Document PiP: desktop Chromium only
  show(btnFullscreen, !IS_MOBILE && !!document.documentElement.requestFullscreen); // not needed on mobile

  // Self cell — reflect the pre-join mic/camera choice.
  const myAvatar = billing && auth.isLoggedIn() ? auth.getUser()?.avatar_url : null;
  addCell(myId, session.name || t('namePlaceholder'), session.lang, true, myAvatar);
  attachStream(myId, localStream);
  setCameraOff(myId, !camOn);
  setAudioMuted(myId, !micOn);

  // Mic input meter: green halo on the mic button while the input picks up
  // sound (muted track → silence → halo off). Join click = user gesture, so
  // the AudioContext is allowed to start.
  if (localStream.getAudioTracks().length > 0) {
    micMeter = new callMods!.micMeter.MicMeter(localStream, (level) =>
      btnMic.style.setProperty('--mic-level', level.toFixed(3)),
    );
  }

  manualClose = false;
  // Detect a Great-Firewall-restricted network (reachability probe, not geo-IP) so we
  // can request the turns://:443 relay + force-relay for this China-side client. Fails
  // open (false) so it never blocks a normal join. Cached per page load.
  restrictedNet = await isRestrictedNetwork();
  // GFW: the client-direct Enhanced tier can't reach its provider behind the firewall,
  // so downgrade this join to server-proxied Standard and tell the user once. No-op unless
  // the network is restricted AND Enhanced is still selected (e.g. a very fast join before
  // the pre-join picker re-rendered). The probe is awaited above, so this is deterministic.
  if (session?.engine && restrictedNet) {
    const safe = enforceEngineForNetwork(session.engine, availableEngines, true);
    if (safe !== session.engine) {
      session.engine = safe;
      saveEnginePref(safe);
      toast(t('engineRestrictedNetwork'));
    }
  }
  // Fetch ICE servers (incl. the TURN relay if configured) before opening the
  // socket, so the mesh has them ready when peers arrive — no race (spec 0026).
  iceServers = await fetchIceServers(restrictedNet);
  // Associate this room with the chosen org/project (+recording) before joining,
  // so the server's call_session inherits it (business users only; no-op otherwise).
  await bindRoomIfBusiness();
  openSocket();
  // Cloud recording: auto-start the composite recorder and notify participants.
  if (bizRecording) {
    showNotif(t('bizRecordingNotice'));
    void startRecording();
  }
}

function openSocket(): void {
  if (!session) return;
  const params = new URLSearchParams({ room: session.room, lang: session.lang, id: myId, public: String(session.isPublic) });
  if (session.name) params.set('name', session.name);
  if (session.engine) params.set('engine', session.engine);
  ws = new WebSocket(auth.buildWsUrl(params));

  ws.onopen = () => {
    setNetworkDegraded(false); // transport (re)connected — clears the amber pill / flashes green
    // On a reconnect this runs again: tear down the previous mesh first so its
    // stale RTCPeerConnections + stats timer don't leak, then rebuild from the
    // fresh room_joined/peer_joined the server sends on (re)join.
    mesh?.destroy();
    mesh = new callMods!.webrtc.MeshManager(
      localStream!,
      (sig) => ws?.send(JSON.stringify(sig)),
      iceServers,
      IS_MOBILE ? VIDEO_BUDGET_MOBILE : VIDEO_BUDGET_DESKTOP, // total upload budget, split per-peer (spec 0030/0031, env-tunable 0044)
      myId, // own id → picks the polite/impolite negotiation role per peer
      // GFW-restricted client: force every peer connection through the turns://:443 relay,
      // skipping host/srflx UDP candidates the Great Firewall resets. Undefined otherwise.
      restrictedNet ? 'relay' : undefined,
    );
    mesh.onNetworkWeak = showWeakNetworkWarning;
    mesh.onRemoteStream = (peerId, stream) => {
      remoteStreams.set(peerId, stream);
      recorder?.addParticipant(participantSource(peerId, stream));
      attachStream(peerId, stream);
      // Enhanced (spec 0108): feed this peer's audio to its in-browser Cartesia pipeline.
      cartesiaManager?.setPeerStream(peerId, stream);
    };
    mesh.onPeerRemoved = (peerId) => removeCell(peerId);
    mesh.setAudioEnabled(micOn);
    mesh.setVideoEnabled(camOn);
    // Enhanced (spec 0108): turn the client-direct receive pipeline on for this call
    // (no-op for every other engine / speaker-pays / guest).
    syncCartesiaForCall();

    // We recreate `audioCapture` fresh on every (re)connect, so CLEAR the listener-pays
    // server-driven format (spec 0099) first: otherwise a stale `serverCaptureFormat`
    // makes the next `capture_format` message no-op (early-return), leaving the new
    // capture in the wrong format — e.g. after leave→change-plan→rejoin (no page reload)
    // the speaker sends WebM while the server reads linear16, so listeners get no
    // translation until a reload. The server re-sends `capture_format` on (re)join, which
    // now applies cleanly against the null flag.
    // Speech-to-speech engines (OpenAI, Gemini) capture raw PCM16/24k; Standard
    // streams WebM/Opus for Deepgram. Decide by the engine's `translated_audio`
    // capability — keying on `id === 'premium'` missed the Gemini engine (id
    // `gemini_live_translate`), which then sent WebM that its PCM session read as
    // noise: no transcript, no translated voice. (In listener-pays mode this is just
    // the initial guess; `capture_format` then dictates the real format.)
    const guessPcm = engineNeedsPcm(session?.engine, availableEngines);
    // Seed the listener-pays format with THIS guess (not null), so the server's
    // first `capture_format` only swaps captures when it genuinely differs (e.g. a
    // Premium listener forces PCM on a Standard speaker). Resetting to null made
    // every join swap once, and that swap raced the control frames — killing a
    // Standard/Enhanced speaker's Deepgram session until a manual mic toggle. This
    // still defeats the stale-format bug (#267): ws.onopen recomputes the guess on
    // every (re)connect, so no value survives across a leave→change-plan→rejoin.
    serverCaptureFormat = guessPcm ? 'pcm' : 'webm';
    audioCapture = guessPcm
      ? new callMods!.pcmCapture.PcmCapture(localStream!, ws!)
      : new callMods!.audioCapture.AudioCapture(localStream!, ws!);
    if (micOn) audioCapture.start();

    // Tell peers if we joined already muted / camera-off so their UI matches.
    if (!micOn) ws?.send(JSON.stringify({ type: 'mute_audio', muted: true }));
    if (!camOn) ws?.send(JSON.stringify({ type: 'mute_video', muted: true }));

    chat = new callMods!.chat.ChatManager({ myLang: session!.lang, myId, container: chatMessages, ws: ws! });
    chat.onUnread = (n) => {
      chatBadge.textContent = String(n);
      chatBadge.hidden = n === 0;
    };
  };

  ws.onmessage = (e) => {
    let msg: any;
    try {
      msg = JSON.parse(e.data);
    } catch {
      return;
    }
    handleServer(msg);
  };

  ws.onclose = (e) => {
    // Unexpected drop while we still want the call → amber "reconnecting" pill.
    if (!manualClose && e.code !== 1000) {
      setNetworkDegraded(true);
      setTimeout(() => !manualClose && openSocket(), 2000);
    }
  };
}

async function handleServer(msg: any): Promise<void> {
  switch (msg.type) {
    case 'room_joined':
      playCallEnterSound(); // Meet-style cue: you joined the call (spec 0024)
      // The room's REAL visibility comes from the server: joining a private code
      // with the default "public" toggle otherwise left a wrong/mixed badge (#50).
      if (typeof msg.public === 'boolean' && session) {
        session.isPublic = msg.public;
        callVis.textContent = msg.public ? t('public') : t('private');
      }
      // session_id present = the backend records a transcript of this call.
      activeSessionId = typeof msg.session_id === 'string' ? msg.session_id : null;
      callStartedAt = Date.now();
      startSessionTimer(); // reveal + tick the session-duration chip (spec 0055)
      show($('transcript-indicator'), !!activeSessionId);
      setBookmarkSession(activeSessionId); // 🔖 button appears (authed users only)
      setGlossaryRoom(session?.room ?? null); // 📖 badge target (spec 0011)
      for (const p of msg.peers) {
        peerNames.set(p.id, { name: p.user_name, lang: p.lang, avatar: p.avatar_url, userId: p.user_id });
        addCell(p.id, p.user_name, p.lang, false, p.avatar_url);
        if (p.cartesia_voice_id) peerVoiceIds.set(p.id, p.cartesia_voice_id); // spec 0108
        cartesiaManager?.setPeerLang(p.id, p.lang); // spec 0108: source lang for Enhanced
        cartesiaManager?.setPeerVoiceId(p.id, p.cartesia_voice_id); // spec 0108: their voice
        await mesh?.addPeer(p.id, false); // they'll initiate the offer
      }
      updateParticipantsList();
      break;
    case 'peer_joined': {
      // A rejoin within the grace window (#233) is a recovered blip, not a new
      // peer — cancel the pending removal so the tile never flickers out, and skip
      // the join chime.
      const reconnected = cancelPendingRemoval(msg.peer_id);
      // Clear any stale "premium speaker" flag: engine is picked on the pre-join
      // screen, so switching tier = leave+rejoin under the SAME (per-tab) peer id. If
      // they were Premium/Pro before and rejoin on Standard, a leftover flag would keep
      // suppressing their TTS forever (only subtitles). They're re-flagged on their next
      // `translated_audio` frame if still on a speech-to-speech engine.
      premiumSpeakers.delete(msg.peer_id);
      peerNames.set(msg.peer_id, { name: msg.user_name, lang: msg.lang, avatar: msg.avatar_url, userId: msg.user_id });
      addCell(msg.peer_id, msg.user_name, msg.lang, false, msg.avatar_url);
      if (msg.cartesia_voice_id) peerVoiceIds.set(msg.peer_id, msg.cartesia_voice_id); // spec 0108
      cartesiaManager?.setPeerLang(msg.peer_id, msg.lang); // spec 0108: source lang for Enhanced
      cartesiaManager?.setPeerVoiceId(msg.peer_id, msg.cartesia_voice_id); // spec 0108: their voice
      if (!reconnected) playJoinSound(); // audible cue only for a genuinely new peer
      await mesh?.addPeer(msg.peer_id, true); // we initiate toward the newcomer
      // Re-announce our current mute/camera state so the newcomer's UI matches.
      if (!micOn) ws?.send(JSON.stringify({ type: 'mute_audio', muted: true }));
      if (!camOn) ws?.send(JSON.stringify({ type: 'mute_video', muted: true }));
      updateParticipantsList();
      break;
    }
    case 'peer_left':
      // Tolerance window (#233): a peer_left may be a transient WS drop, not a real
      // departure. Keep the tile (its last received frame held) in a "reconnecting"
      // state for a grace period; a same-id rejoin (#219) cancels the removal — no
      // flicker. Only if they don't return do we actually drop them.
      schedulePeerRemoval(msg.peer_id);
      break;
    case 'room_full':
      track('call_failed', { reason: 'room_full' });
      leaveCall();
      homeStatusMsg(t('roomFull'), true);
      break;
    case 'offer':
      await mesh?.handleOffer(msg.from, msg.sdp);
      break;
    case 'answer':
      await mesh?.handleAnswer(msg.from, msg.sdp);
      break;
    case 'ice':
      await mesh?.handleIce(msg.from, msg.candidate);
      break;
    case 'chat_message':
      chat?.addMessage(msg as ChatPayload);
      transcriptEvents++;
      break;
    case 'peer_muted':
      if (msg.kind === 'audio') {
        peerMicMuted.set(msg.peer_id, msg.muted);
        setAudioMuted(msg.peer_id, msg.muted);
      } else {
        peerCamOff.set(msg.peer_id, msg.muted);
        setCameraOff(msg.peer_id, msg.muted);
        recorder?.setVideoOff(msg.peer_id, msg.muted);
      }
      updateParticipantsList();
      break;
    case 'emoji_reaction':
      showEmojiReaction(msg.peer_id, msg.emoji);
      break;
    case 'glossary_active':
      // Sent on join and re-broadcast after edits; entries 0 hides the badge.
      onGlossaryActive(msg.name ?? null, msg.entries);
      break;
    case 'hand_raised':
      peerHandRaised.set(msg.peer_id, msg.raised);
      setHandIndicator(msg.peer_id, msg.raised);
      if (msg.raised && msg.peer_id !== myId) {
        const pname = peerNames.get(msg.peer_id)?.name || 'Someone';
        showNotif(`✋ ${pname} ${t('handRaisedNotif')}`);
        playHandRaiseSound();
      }
      updateParticipantsList();
      break;
    case 'screen_share':
      setScreenShareIndicator(msg.peer_id, msg.active, msg.audio ?? false);
      applyAudioMode(); // re-evaluate muting: only a *with-audio* share is exempt, so a mic-only share still ducks (#229 follow-up)
      spotlightShare(msg.peer_id, msg.active); // zoom the sharer's tile into focus (spec 0089)
      break;
    case 'whiteboard': // a peer's stroke/clear (spec 0045)
      whiteboard.applyOp(msg.op);
      break;
    case 'whiteboard_snapshot': // the board state on join (late-joiner)
      whiteboard.applySnapshot(msg.ops);
      break;
    case 'game': // a mini-game state update (spec 0046/0047)
    case 'game_snapshot': // the current game on join
      // One `game` channel, routed by a discriminator: quiz states carry
      // `game:'quiz'`; Tic-Tac-Toe states have none (the default). applyRemote
      // returns true when a game just appeared, so we open the panel for peers /
      // spectators / late-joiners (spec 0070 S3 R3.3).
      if (msg.state && msg.state.game === 'quiz') quiz.applyRemote(msg.state);
      else if (tictactoe.applyRemote(msg.state)) toggleMinigame(true);
      break;
    case 'language_detected': {
      // A peer's "auto" was resolved by the server probe (confidence present)
      // or manually corrected via set_lang (confidence absent). Refresh their
      // badges; for our own detection, offer the Change toast (spec 0012).
      const info = peerNames.get(msg.peer_id);
      if (info) info.lang = msg.lang;
      const badge = videoGrid.querySelector(`[data-peer="${cssEsc(msg.peer_id)}"] .peer-lang`);
      if (badge) badge.textContent = `${FLAG[msg.lang] || ''} ${msg.lang.toUpperCase()}`.trim();
      // Enhanced (spec 0108): a peer's resolved source language, or — for us — our own
      // translation target moving, both restart the affected Cartesia pipeline(s).
      if (msg.peer_id === myId) cartesiaManager?.setMyLang(msg.lang);
      else cartesiaManager?.setPeerLang(msg.peer_id, msg.lang);
      if (msg.peer_id === myId && session) {
        session.lang = msg.lang;
        stageSelfLang.textContent = `${FLAG[msg.lang] || ''} ${msg.lang.toUpperCase()}`.trim();
        chat?.setMyLang(msg.lang);
        // Manual-correction echo (no confidence) must not re-open the toast,
        // or accepting a correction would loop forever.
        if (msg.confidence != null) onLanguageDetected(msg.lang);
        else toast(t('langChanged'));
      }
      updateParticipantsList();
      break;
    }
    case 'subtitle_interim':
      // Interim transcript is in the speaker's original language (translation lands
      // on the final frame), so feed it as `original`.
      showSubtitle(msg.speaker_id, { original: msg.text, interim: true });
      break;
    case 'translated_audio': {
      // Premium engine (spec 0093): real translated speech from the server. The
      // server already targets only our language, so just play it (gated on the
      // "translated voice" toggle, like TTS). Marks the speaker so we don't also
      // synthesize them. Their original WebRTC voice is ducked by applyAudioMode().
      premiumSpeakers.add(msg.speaker_id);
      if (ttsOn && msg.speaker_id !== myId) pcmPlayback.enqueue(msg.speaker_id, msg.seq, msg.pcm16_b64);
      break;
    }
    case 'translated_text': {
      // Enhanced (spec 0108): the Groq translation of a `translate_text` we sent. Hand it to
      // the waiting pipeline (matched by request_id); unknown/late ids are ignored.
      pendingTranslations.get(msg.request_id)?.(
        typeof msg.text === 'string' ? msg.text : null,
      );
      break;
    }
    case 'capture_format': {
      // Listener-pays (spec 0099): the server dictates our capture format from the
      // room's Premium composition. From now on capture is server-driven.
      applyCaptureFormat(!!msg.pcm);
      break;
    }
    case 'engine_downgraded': {
      // A speaker's engine was switched mid-call (spec 0093), e.g. Premium → Standard
      // when credits ran low.
      if (msg.peer_id === myId) {
        if (serverCaptureFormat !== null) {
          // Listener-pays (spec 0099): it's MY receive engine that changed (e.g. I
          // ran out of credit → now I receive Standard). Capture is server-driven
          // (capture_format), so we do NOT swap it here; just record + notify.
          if (session) session.engine = msg.to;
          // Enhanced (spec 0108): if we were the client-direct (Cartesia) tier and got
          // downgraded, tear down the in-browser pipelines — the server now delivers
          // Standard subtitles for us. No-op when we weren't on Enhanced.
          cartesiaManager?.deactivate();
          const notifKey =
            msg.reason === 'enhanced_unavailable'
              ? 'engineEnhancedUnavailable'
              : msg.reason === 'insufficient_balance'
                ? 'enginePremiumPaused'
                : 'enginePremiumBusy';
          showNotif(t(notifKey));
        } else {
          // Speaker-pays: match our capture to the new engine's format (Standard → WebM
          // today, but stay capability-correct via engineNeedsPcm for any engine).
          if (session) session.engine = msg.to;
          const wasActive = micOn;
          audioCapture?.stop();
          if (ws && localStream) {
            audioCapture = engineNeedsPcm(msg.to, availableEngines)
              ? new callMods!.pcmCapture.PcmCapture(localStream, ws)
              : new callMods!.audioCapture.AudioCapture(localStream, ws);
            if (wasActive) audioCapture.start();
          }
          showNotif(t(msg.reason === 'premium_at_capacity' ? 'enginePremiumBusy' : 'enginePremiumPaused'));
        }
      } else {
        // A peer downgraded: stop expecting their premium audio so TTS resumes.
        premiumSpeakers.delete(msg.peer_id);
      }
      break;
    }
    case 'subtitle_final': {
      transcriptEvents++;
      const myLang = session?.lang || 'en';
      // Prefer my-language translation, but fall back to the source when it's MISSING OR
      // EMPTY. The Pro (OpenAI gpt-realtime-translate) engine sometimes ships an empty
      // output transcript — `?? original` doesn't catch `''`, so a foreign speaker's
      // caption rendered blank (you'd hear the translated audio but see nothing).
      const translated = msg.translations?.[myLang];
      const text = translated && translated.trim() ? translated : msg.original;
      // `text` is the best line for me (translation, or the original when the speaker
      // already talks my language / no translation text arrived); `msg.original` is the source.
      showSubtitle(msg.speaker_id, { translation: text, original: msg.original, interim: false });
      // Track active speaker for speaker view
      if (msg.speaker_id !== myId) {
        lastSpeakerId = msg.speaker_id;
        if (viewMode === 'speaker') layoutVideos();
      }
      // Speak only foreign-language speakers (same-language → you hear their
      // real voice). Their original WebRTC audio is muted by applyAudioMode().
      // While our own lang is still "auto" (detection pending) there is no
      // valid TTS voice/translation to pick — skip until it resolves. Premium
      // speakers stream real translated audio (translated_audio) — never TTS them.
      if (
        ttsOn &&
        msg.speaker_id !== myId &&
        msg.lang !== myLang &&
        myLang !== 'auto' &&
        !premiumSpeakers.has(msg.speaker_id)
      )
        speak(text, myLang);
      // Voice-command timer (spec 0052): only OUR OWN final transcript can arm a
      // timer — a peer's speech never controls your clock. Parsed from the raw
      // (untranslated) text in the speaker's own language.
      if (msg.speaker_id === myId) callTimer.handleTranscript(msg.original || '');
      break;
    }
    // ---- Billing (only sent to authenticated speakers) ----
    case 'balance_update':
      if (typeof msg.balance === 'number') {
        auth.setBalance(msg.balance);
        setBalanceUi(msg.balance);
        show(lowBanner, false);
      }
      break;
    case 'low_balance':
      if (typeof msg.balance === 'number') {
        auth.setBalance(msg.balance);
        setBalanceUi(msg.balance);
        lowBannerText.textContent = `${t('lowBalanceWarn')} · ${auth.formatCredits(msg.balance)}`;
        show(lowBanner, true);
      }
      break;
    case 'balance_exhausted': {
      // The server closed our STT session; stop feeding it audio (WebRTC stays
      // up so peers still hear us). The modal adapts: a billed user is out of
      // credits (→ buy); a guest's free trial ended (→ sign in).
      audioCapture?.stop();
      const loggedIn = billing && auth.isLoggedIn();
      exhaustedIsGuest = !loggedIn;
      $('exhausted-title').textContent = t(loggedIn ? 'outOfCredits' : 'trialEnded');
      $('exhausted-text').textContent = t(loggedIn ? 'outOfCreditsText' : 'trialEndedText');
      $('exhausted-buy').textContent = t(loggedIn ? 'buyCredits' : 'signIn');
      if (loggedIn) {
        auth.setBalance(0);
        setBalanceUi(0);
      }
      show(exhaustedModal, true);
      break;
    }
    // A transcript of ours tripped the moderation filter — the server dropped
    // that line (peers never saw it) and warned us. Surface it as a toast.
    case 'moderation_warning':
      toast(t('moderationBlocked'));
      break;
    case 'error':
      if (msg.code === 'insufficient_balance') {
        track('limit_reached', { type: 'credits_exhausted' });
        leaveCall();
        homeStatusMsg(t('outOfCredits'), true);
        if (billing) openBuyModal();
      } else if (msg.code === 'login_required') {
        // Public rooms require an account; bounce a guest back to the login gate.
        leaveCall();
        homeStatusMsg(t('publicNeedsLogin'), true);
        if (billing) showLogin();
      } else if (msg.code === 'banned') {
        track('call_failed', { reason: 'banned' });
        leaveCall();
        homeStatusMsg(msg.message || t('bannedMsg'), true);
      } else if (msg.code === 'consent_required') {
        // Server backstop for the 18+/ToS gate — bounce out and re-show the
        // (blocking) consent modal so the user can't proceed without confirming.
        leaveCall();
        ensureConsent();
      } else if (msg.code === 'detect_failed') {
        // Auto-detect probe failed; the server fell back to English (spec 0012).
        toast(t('langDetectFailed'));
      } else if (msg.message) {
        // Non-fatal; surface transiently in the call header area.
        callVis.textContent = msg.message;
      }
      break;
  }
}

// ---- Video grid ------------------------------------------------------------
function addCell(id: string, name: string, lang: string, isSelf: boolean, avatarSrc?: string | null): void {
  if (videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`)) return;
  const cell = document.createElement('div');
  cell.className = `video-cell${isSelf ? ' self' : ''}`;
  cell.dataset.peer = id;

  const video = document.createElement('video');
  video.autoplay = true;
  video.playsInline = true;
  if (isSelf) video.muted = true; // never echo yourself
  cell.appendChild(video);

  const avatar = document.createElement('div');
  avatar.className = 'avatar';
  avatar.hidden = true;
  avatar.style.background = avatarGradient(name);
  const av = auth.avatarUrl(avatarSrc, 168);
  if (av) {
    const img = document.createElement('img');
    img.className = 'avatar-img';
    img.referrerPolicy = 'no-referrer';
    img.alt = name;
    img.src = av;
    img.addEventListener('error', () => {
      // Fall back to initials if the Google image fails to load.
      img.remove();
      const initials = document.createElement('span');
      initials.className = 'avatar-initials';
      initials.textContent = name.slice(0, 2).toUpperCase();
      avatar.appendChild(initials);
    });
    avatar.appendChild(img);
  } else {
    const initials = document.createElement('span');
    initials.className = 'avatar-initials';
    initials.textContent = name.slice(0, 2).toUpperCase();
    avatar.appendChild(initials);
  }
  cell.appendChild(avatar);

  const overlay = document.createElement('div');
  overlay.className = 'video-overlay';
  // Compact identity badge (top-left): the user's avatar + their language flag.
  // Top-right is reserved for the raised-hand indicator, so identity sits left.
  const avBadge = document.createElement('span');
  avBadge.className = 'peer-avatar';
  fillAvatar(avBadge, name, avatarSrc, 40, 1);
  const nameEl = document.createElement('span');
  nameEl.className = 'peer-name';
  nameEl.textContent = isSelf ? t('you') : name;
  const langEl = document.createElement('span');
  langEl.className = 'peer-lang';
  langEl.textContent = `${FLAG[lang] || ''} ${lang.toUpperCase()}`.trim();
  const mute = document.createElement('span');
  mute.className = 'mute-indicator';
  mute.hidden = true;
  mute.innerHTML = icon('mic-off', 14);
  overlay.append(avBadge, nameEl, langEl, mute);
  if (!isSelf) {
    // A real <button> so pinning works from the keyboard too.
    const pinBtn = document.createElement('button');
    pinBtn.type = 'button';
    pinBtn.className = 'pin-btn';
    pinBtn.innerHTML = icon('pin', 14);
    pinBtn.title = t('pinTip');
    pinBtn.setAttribute('aria-label', t('pinTip'));
    pinBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      togglePin(id);
    });
    overlay.appendChild(pinBtn);
  }
  cell.appendChild(overlay);

  const subs = document.createElement('div');
  subs.className = 'subtitle-area';
  cell.appendChild(subs);

  // Per-peer moderation controls (remote peers only): report to the server
  // (needs an account) and a local block (mute + hide, no account needed).
  if (!isSelf) {
    const actions = document.createElement('div');
    actions.className = 'cell-actions';
    // Add/remove this peer as a friend (only for a logged-in viewer + an account peer).
    // Never for a tile that is MY OWN account (e.g. the same login open in another tab /
    // device): befriending yourself is a no-op the server rejects, so hide the control.
    const friendUid = auth.isLoggedIn() ? peerNames.get(id)?.userId : undefined;
    if (friendUid && friendUid !== auth.getUser()?.id) actions.appendChild(tileFriendButton(friendUid));
    if (billing && auth.isLoggedIn()) {
      const reportBtn = document.createElement('button');
      reportBtn.className = 'cell-action';
      reportBtn.type = 'button';
      reportBtn.title = t('reportTip');
      reportBtn.setAttribute('aria-label', t('reportTip'));
      reportBtn.innerHTML = icon('flag', 15);
      reportBtn.addEventListener('click', () => openReport(id, peerNames.get(id)?.name || name));
      actions.appendChild(reportBtn);
    }
    const blockBtn = document.createElement('button');
    blockBtn.className = 'cell-action';
    blockBtn.type = 'button';
    blockBtn.title = t('blockTip');
    blockBtn.setAttribute('aria-label', t('blockTip'));
    blockBtn.innerHTML = icon('block', 15);
    blockBtn.addEventListener('click', () => toggleBlock(id));
    actions.appendChild(blockBtn);
    cell.appendChild(actions);
  }

  videoGrid.appendChild(cell);
  if (blockedPeers.has(id)) applyBlocked(id);
  updateGridCount();
}

// ---- Transient-drop tolerance (#233) ----------------------------------------
// A `peer_left` may be a short network blip (WS dropped + about to reconnect)
// rather than a real departure. Defer the teardown by a grace window, holding the
// tile (and its last received frame) in a "reconnecting" state; a same-id rejoin
// (#219) within the window cancels it, so brief drops no longer flicker users out
// and back in. Only a peer that stays gone past the window is actually removed.
const PEER_LEAVE_GRACE_MS = 4000;
const pendingRemovals = new Map<string, number>();

function schedulePeerRemoval(id: string): void {
  if (pendingRemovals.has(id)) return; // already counting down
  videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`)?.classList.add('reconnecting');
  const timer = window.setTimeout(() => {
    pendingRemovals.delete(id);
    mesh?.removePeer(id);
    removeCell(id);
    peerHandRaised.delete(id);
    playLeaveSound(); // audible cue only once we're sure they've actually left
    updateParticipantsList();
  }, PEER_LEAVE_GRACE_MS);
  pendingRemovals.set(id, timer);
}

/** Cancel a pending removal (the peer came back). Returns true if one was pending. */
function cancelPendingRemoval(id: string): boolean {
  const timer = pendingRemovals.get(id);
  if (timer === undefined) return false;
  clearTimeout(timer);
  pendingRemovals.delete(id);
  videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`)?.classList.remove('reconnecting');
  return true;
}

/** Drop all pending-removal timers (e.g. on leaving the call) so none fire late. */
function clearPendingRemovals(): void {
  for (const timer of pendingRemovals.values()) clearTimeout(timer);
  pendingRemovals.clear();
}

function removeCell(id: string): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`);
  if (cell) cell.remove();
  peerNames.delete(id);
  peerCamOff.delete(id);
  remoteStreams.delete(id);
  recorder?.removeParticipant(id);
  cartesiaManager?.removePeer(id); // spec 0108: tear down this peer's Enhanced pipeline
  peerVoiceIds.delete(id);
  if (pinnedPeerId === id) pinnedPeerId = null;
  if (lastSpeakerId === id) lastSpeakerId = null;
  updateGridCount();
}

function updateGridCount(): void {
  videoGrid.dataset.peers = String(videoGrid.querySelectorAll('.video-cell').length);
  layoutVideos();
}

// ---- Screen share / focus-cell pan (mobile) ---------------------------------

function disablePan(cell: HTMLElement): void {
  cell.classList.remove('pan-mode');
  const v = cell.querySelector<HTMLVideoElement>('video');
  if (v) v.style.transform = '';
  const panned = cell as unknown as { _panAbort?: AbortController };
  panned._panAbort?.abort(); // tears down the touch + button listeners
  delete panned._panAbort;
  cell.querySelector('.pan-toggle')?.remove();
  cell.querySelector('.pan-hint')?.remove();
}

// Pan + pinch-zoom a screen-share tile on mobile (spec 0033): the ⊕ button enters
// "pan-mode", then one finger drags (translate) and two fingers pinch (scale).
// Listeners + button are scoped to an AbortController so re-sharing rebuilds cleanly.
function setupPan(cell: HTMLElement): void {
  const panned = cell as unknown as { _panAbort?: AbortController };
  if (panned._panAbort) return; // already set up
  const ac = new AbortController();
  panned._panAbort = ac;
  const sig = ac.signal;

  let tx = 0, ty = 0, scale = 1;
  let startX = 0, startY = 0; // 1-finger pan anchor
  let startDist = 0, startScale = 1; // 2-finger pinch anchor
  const video = () => cell.querySelector<HTMLVideoElement>('video');
  const apply = () => {
    const v = video();
    if (v) v.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
  };
  const reset = () => { tx = 0; ty = 0; scale = 1; apply(); };
  const dist = (touches: TouchList) =>
    Math.hypot(touches[0].clientX - touches[1].clientX, touches[0].clientY - touches[1].clientY);

  cell.addEventListener('touchstart', (e: TouchEvent) => {
    if (!cell.classList.contains('pan-mode')) return;
    if (e.touches.length === 1) {
      startX = e.touches[0].clientX - tx;
      startY = e.touches[0].clientY - ty;
    } else if (e.touches.length === 2) {
      startDist = dist(e.touches);
      startScale = scale;
    }
  }, { passive: true, signal: sig });

  cell.addEventListener('touchmove', (e: TouchEvent) => {
    if (!cell.classList.contains('pan-mode')) return;
    e.preventDefault();
    if (e.touches.length === 2 && startDist > 0) {
      scale = Math.min(4, Math.max(1, startScale * (dist(e.touches) / startDist)));
    } else if (e.touches.length === 1) {
      tx = e.touches[0].clientX - startX;
      ty = e.touches[0].clientY - startY;
    }
    apply();
  }, { passive: false, signal: sig });

  const btn = document.createElement('button');
  btn.className = 'pan-toggle';
  btn.title = t('panZoomHint');
  btn.innerHTML = icon('move', 24);
  cell.appendChild(btn);

  btn.addEventListener('click', (e) => {
    e.stopPropagation();
    const active = cell.classList.toggle('pan-mode');
    btn.classList.toggle('active', active);
    reset(); // recenter + reset zoom on every toggle
    if (active && !cell.querySelector('.pan-hint')) {
      const hint = document.createElement('span');
      hint.className = 'pan-hint';
      hint.textContent = t('panZoomHint');
      cell.appendChild(hint);
      hint.addEventListener('animationend', () => hint.remove());
    }
  }, { signal: sig });
}

// The grid fills the whole stage. In focus mode (pinned or speaker), the main
// cell fills the stage and others become small overlays at the bottom-right.
function layoutVideos(): void {
  const stage = document.querySelector('.video-stage') as HTMLElement | null;
  if (!stage) return;
  const allCells = [...videoGrid.querySelectorAll<HTMLElement>('.video-cell')];
  const n = Math.max(allCells.length, 1);
  const sw = stage.clientWidth;
  const sh = stage.clientHeight;
  if (sw === 0 || sh === 0) return;

  // Determine focus id
  const focusId = pinnedPeerId || (viewMode === 'speaker' ? lastSpeakerId : null);
  const focusCell = focusId ? videoGrid.querySelector<HTMLElement>(`[data-peer="${cssEsc(focusId)}"]`) : null;

  // Remove all special classes first; reset pan state on cells leaving focus
  allCells.forEach((c) => {
    c.classList.remove('main-cell', 'video-thumb', 'active-speaker');
    c.style.removeProperty('--thumb-i'); // drop any prior focus-column position
    if (c.classList.contains('pan-mode')) disablePan(c);
  });

  if (focusCell && focusId && n > 1) {
    // Focus mode: one main + thumbnails
    videoGrid.dataset.mode = 'focus';
    videoGrid.style.gridTemplateColumns = '';
    videoGrid.style.gridTemplateRows = '';
    videoGrid.style.position = 'relative';
    videoGrid.style.width = '100%';
    videoGrid.style.height = '100%';

    focusCell.classList.add('main-cell');
    // Mobile pan/zoom only on a screen-share tile (spec 0033): a shared screen in
    // portrait is cropped, so dragging/pinching to read it helps; camera tiles don't.
    if (IS_MOBILE && focusCell.classList.contains('sharing')) setupPan(focusCell);
    else disablePan(focusCell);

    let thumbIndex = 0;
    for (const cell of allCells) {
      if (cell === focusCell) continue;
      cell.classList.add('video-thumb');
      // Stack thumbnails up the right edge (index 0 = bottom) so they never pile up.
      cell.style.setProperty('--thumb-i', String(thumbIndex++));
      // Click thumbnail to pin
      const id = cell.dataset.peer || '';
      cell.addEventListener('click', () => { if (id) togglePin(id); }, { once: true });
    }

    // Mark active speaker
    if (lastSpeakerId && lastSpeakerId !== pinnedPeerId) {
      const as = videoGrid.querySelector<HTMLElement>(`[data-peer="${cssEsc(lastSpeakerId)}"]`);
      if (as) as.classList.add('active-speaker');
    }
  } else {
    // Grid mode (default)
    videoGrid.dataset.mode = 'grid';
    let cols: number, rows: number;
    if (n <= 1) {
      cols = 1; rows = 1;
    } else if (n === 2) {
      if (sw >= sh) { cols = 2; rows = 1; }
      else { cols = 1; rows = 2; }
    } else {
      cols = 2; rows = 2;
    }
    videoGrid.style.gridTemplateColumns = `repeat(${cols}, 1fr)`;
    videoGrid.style.gridTemplateRows = `repeat(${rows}, 1fr)`;
    videoGrid.style.position = '';
    videoGrid.style.width = '';
    videoGrid.style.height = '';

    // Mark active speaker in grid mode
    if (lastSpeakerId) {
      const as = videoGrid.querySelector<HTMLElement>(`[data-peer="${cssEsc(lastSpeakerId)}"]`);
      if (as) as.classList.add('active-speaker');
    }
  }
}

function togglePin(id: string): void {
  if (pinnedPeerId === id) {
    pinnedPeerId = null;
  } else {
    pinnedPeerId = id;
    if (viewMode === 'speaker') viewMode = 'grid';
  }
  setControlState();
  layoutVideos();
  updatePinButtons();
}

// Auto-spotlight the screen sharer for everyone (spec 0089): on start their tile
// takes the focus view and zooms in; on stop we restore the prior focus (zoom-out
// back to the grid / previous pin).
function spotlightShare(peerId: string, active: boolean): void {
  if (active) {
    if (sharingSpotlightId === peerId) return;
    // Remember the pre-share focus once, so we can restore it on stop.
    if (!sharingSpotlightId) pinBeforeShare = pinnedPeerId;
    sharingSpotlightId = peerId;
    pinnedPeerId = peerId;
    updatePinButtons();
    layoutVideos();
    // One-shot zoom-in on the now-focused share tile (replays each share).
    const cell = videoGrid.querySelector<HTMLElement>(`[data-peer="${cssEsc(peerId)}"]`);
    if (cell) {
      cell.classList.add('share-zoom');
      setTimeout(() => cell.classList.remove('share-zoom'), 450);
    }
  } else {
    if (sharingSpotlightId !== peerId) return; // a different/older share is in focus
    sharingSpotlightId = null;
    pinnedPeerId = pinBeforeShare;
    pinBeforeShare = null;
    updatePinButtons();
    layoutVideos(); // tile returns to the grid (zoom-out)
  }
}

function updatePinButtons(): void {
  videoGrid.querySelectorAll<HTMLElement>('.pin-btn').forEach((btn) => {
    const cell = btn.closest<HTMLElement>('[data-peer]');
    const id = cell?.dataset.peer || '';
    const isPinned = id === pinnedPeerId;
    btn.innerHTML = icon(isPinned ? 'pin-off' : 'pin', 14);
    btn.title = isPinned ? t('unpinTip') : t('pinTip');
    btn.setAttribute('aria-label', btn.title);
    btn.setAttribute('aria-pressed', String(isPinned));
  });
}

function attachStream(id: string, stream: MediaStream): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`);
  if (!cell) return;
  const video = cell.querySelector('video') as HTMLVideoElement;
  video.srcObject = stream;
  void video.play().catch(() => {});
  // A disabled remote track still counts, so a known camera-off state (from
  // peer_muted) takes precedence over the raw track count.
  const hasVideo = stream.getVideoTracks().length > 0;
  if (id !== myId) setCameraOff(id, peerCamOff.get(id) ?? !hasVideo);
  applyAudioMode();
  schedulePipSync(); // a stream just attached — push it into the PiP clone too (spec 0057)
}

// "Translated voice" mode: when on, mute the original WebRTC audio of peers who
// speak a different language (you'll hear their TTS translation instead), so the
// original and translated voices never overlap. Same-language peers keep their
// real audio (no robotic dubbing of your own language). Self is always muted.
function applyAudioMode(): void {
  const myLang = session?.lang;
  videoGrid.querySelectorAll<HTMLElement>('.video-cell').forEach((cell) => {
    const id = cell.dataset.peer || '';
    const video = cell.querySelector('video') as HTMLVideoElement | null;
    if (!video) return;
    if (id === myId) {
      video.muted = true;
    } else if (blockedPeers.has(id)) {
      video.muted = true; // locally blocked → always silent
    } else {
      const peerLang = peerNames.get(id)?.lang;
      // A peer who shares with "share audio" ticked sends shared tab/system audio
      // (music, a video) on the WebRTC track — everyone must hear it, so never mute
      // it for the translated-voice setting, or that audio is lost (#229). But a
      // share WITHOUT audio leaves the bare mic on the wire, so it must stay
      // duckable like normal — otherwise the original voice doubles the TTS while
      // sharing (#229 follow-up). `share-audio`, not `sharing`, gates the exemption.
      const shareAudio = cell.classList.contains('share-audio');
      video.muted = !shareAudio && !!(ttsOn && peerLang && myLang && peerLang !== myLang);
    }
    // PiP clones are display-only and always muted (audio stays on these live
    // elements), so there's nothing to keep in sync here.
  });
}

function setCameraOff(id: string, off: boolean): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`);
  if (!cell) return;
  (cell.querySelector('video') as HTMLElement).style.visibility = off ? 'hidden' : 'visible';
  (cell.querySelector('.avatar') as HTMLElement).hidden = !off;
}

function setAudioMuted(id: string, muted: boolean): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`);
  if (cell) (cell.querySelector('.mute-indicator') as HTMLElement).hidden = !muted;
}

function setHandIndicator(id: string, raised: boolean): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`);
  if (!cell) return;
  cell.classList.toggle('hand-raised', raised); // yellow border via CSS
  let indicator = cell.querySelector('.hand-indicator') as HTMLElement | null;
  if (raised) {
    if (!indicator) {
      indicator = document.createElement('span');
      indicator.className = 'hand-indicator';
      indicator.textContent = '✋';
      cell.appendChild(indicator);
    }
  } else if (indicator) {
    indicator.remove();
  }
}

// A peer started/stopped screen-sharing (spec 0033): mark the tile with `.sharing`
// (gates the mobile pan/zoom) + the 🖥 badge, mirroring the self-share treatment.
// `shareAudio` is true only when the peer routed shared tab/system audio to the
// room (spec 0085); it gets its own `.share-audio` class so applyAudioMode knows
// not to duck that track for the translated voice (#229 follow-up).
function setScreenShareIndicator(id: string, active: boolean, shareAudio = false): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`);
  if (!cell) return;
  cell.classList.toggle('sharing', active);
  cell.classList.toggle('share-audio', active && shareAudio);
  let badge = cell.querySelector('.screen-share-badge') as HTMLElement | null;
  if (active) {
    if (!badge) {
      badge = document.createElement('span');
      badge.className = 'screen-share-badge';
      badge.textContent = '🖥';
      cell.querySelector('.video-overlay')?.appendChild(badge);
    }
  } else {
    badge?.remove();
    // Share stopped → remove the mobile pan/zoom ⊕ button + reset any transform, so
    // it can't linger and let you drag the peer's now-camera video (spec 0033 fix).
    disablePan(cell as HTMLElement);
  }
}

function showEmojiReaction(peerId: string, emoji: string): void {
  const stage = document.querySelector('.video-stage');
  if (!stage) return;
  // Meet-style: a big emoji rising from the centre of the whole stage + who reacted
  // (spec 0035). Random horizontal start + drift so a burst scatters rather than
  // stacking exactly.
  const name = peerId === myId ? session?.name || t('you') : peerNames.get(peerId)?.name || '';
  const float = document.createElement('div');
  float.className = 'reaction-float';
  float.style.setProperty('--x', `${Math.round((Math.random() - 0.5) * 220)}px`);
  float.style.setProperty('--drift', `${Math.round((Math.random() - 0.5) * 80)}px`);
  const e = document.createElement('span');
  e.className = 'reaction-emoji';
  e.textContent = emoji;
  float.appendChild(e);
  if (name) {
    const n = document.createElement('span');
    n.className = 'reaction-name';
    n.textContent = name;
    float.appendChild(n);
  }
  stage.appendChild(float);
  setTimeout(() => float.remove(), 3700);
}

// ---- Notification banner ---------------------------------------------------
let notifTimer: number | null = null;
// Weak-network nudge (spec 0030): getStats flagged a sustained bandwidth-limited /
// lossy uplink. Suggest the camera off — at most once a minute so it isn't spammy.
let lastWeakWarn = 0;
function showWeakNetworkWarning(): void {
  const now = Date.now();
  if (now - lastWeakWarn < 60_000) return;
  lastWeakWarn = now;
  toast(t('weakNetwork'));
}

function showNotif(text: string): void {
  notifBanner.textContent = text;
  notifBanner.classList.remove('hidden');
  if (notifTimer) clearTimeout(notifTimer);
  notifTimer = window.setTimeout(() => notifBanner.classList.add('hidden'), 4000);
}

// ---- Participants panel ----------------------------------------------------
function toggleParticipants(force?: boolean): void {
  const open = force ?? participantsPanel.classList.contains('closed');
  participantsPanel.classList.toggle('open', open);
  participantsPanel.classList.toggle('closed', !open);
  btnParticipants.setAttribute('aria-expanded', String(open));
  if (open) updateParticipantsList();
  setTimeout(layoutVideos, 320);
}

partClose.addEventListener('click', () => toggleParticipants(false));

// ---- Invite panel (spec 0082) ---------------------------------------------
function setInviteStatus(msg: string, kind: '' | 'ok' | 'err'): void {
  inviteStatus.textContent = msg;
  inviteStatus.classList.toggle('ok', kind === 'ok');
  inviteStatus.classList.toggle('err', kind === 'err');
}

function toggleInvite(force?: boolean): void {
  const open = force ?? invitePanel.classList.contains('closed');
  invitePanel.classList.toggle('open', open);
  invitePanel.classList.toggle('closed', !open);
  btnInvite.setAttribute('aria-expanded', String(open));
  if (open) {
    inviteLinkInput.value = session ? buildInviteLink(location.origin, session.room) : '';
    // Email send is for signed-in users (the server 401s guests); guests still
    // get the copy-link row. We never list registered users — you invite people
    // whose address you already know.
    const signedIn = auth.isLoggedIn();
    show(inviteEmailBlock, signedIn);
    show(inviteGuestHint, !signedIn);
    setInviteStatus('', '');
  }
  setTimeout(layoutVideos, 320);
}

// Offer the invite affordance only while a seat is free (spec 0082). Hide and
// collapse it the moment the room fills.
function updateInviteAvailability(count: number): void {
  const canInvite = !!session && count < MAX_ROOM;
  show(miInvite, canInvite);
  if (!canInvite && !invitePanel.classList.contains('closed')) toggleInvite(false);
}

// Tic-tac-toe needs an opponent: only offer the button with 2+ in the room. The
// moment we're alone again (the other player left) close the panel and drop the
// board locally, so a half-played game can't linger with no one to play against.
function updateMinigameAvailability(count: number): void {
  const canPlay = count >= 2;
  show(miMinigame, canPlay);
  if (!canPlay) {
    toggleMinigame(false);
    tictactoe?.reset();
  }
}

btnInvite.addEventListener('click', () => {
  setMoreOpen(false);
  toggleInvite();
});
inviteClose.addEventListener('click', () => toggleInvite(false));

inviteCopyBtn.addEventListener('click', async () => {
  const link = inviteLinkInput.value;
  if (!link) return;
  try {
    await navigator.clipboard.writeText(link);
    track('invite_link_copied');
  } catch {
    inviteLinkInput.select(); // fallback: select it for a manual copy
  }
  setInviteStatus(t('inviteCopied'), 'ok');
});

async function sendInvitesFromForm(): Promise<void> {
  if (!session) return;
  const { emails, invalid } = validateInviteEmails(inviteEmailInput.value);
  if (invalid.length) {
    setInviteStatus(t('inviteBadEmail').replace('{x}', invalid[0]), 'err');
    return;
  }
  if (!emails.length) {
    setInviteStatus(t('inviteNoEmails'), 'err');
    return;
  }
  if (emails.length > MAX_INVITE_EMAILS) {
    setInviteStatus(t('inviteTooMany').replace('{n}', String(MAX_INVITE_EMAILS)), 'err');
    return;
  }
  inviteSendBtn.disabled = true;
  setInviteStatus(t('inviteSending'), '');
  const res = await sendInvites(session.room, emails, getUiLang());
  inviteSendBtn.disabled = false;
  if (res.error) {
    setInviteStatus(t('inviteSendFailed'), 'err');
    return;
  }
  inviteEmailInput.value = '';
  track('invite_sent', { count: res.sent });
  let msg = t('inviteSent').replace('{n}', String(res.sent));
  if (res.failed) msg += ` · ${t('inviteSomeFailed').replace('{n}', String(res.failed))}`;
  setInviteStatus(msg, 'ok');
}

inviteSendBtn.addEventListener('click', () => void sendInvitesFromForm());
inviteEmailInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    void sendInvitesFromForm();
  }
});

// ---- Session header: live duration + participant count (spec 0055) ----------
// Both chips stay hidden until join. The duration is THIS client's elapsed time
// since room_joined; the count is set from updateParticipantsList (peerNames).
const sessionTimerEl = $('session-timer');
const partCountEl = $('part-count');
const partAvatarEl = $('part-avatar'); // your initial + gradient in the on-video badge (spec 0061)
const headerClock = $('header-clock'); // live wall-clock (spec 0060 / #94)
sessionTimerEl.querySelector<HTMLElement>('.sb-ico')!.innerHTML = icon('timer', 13);

// One 1s tick drives both the wall-clock (HH:MM, locale-aware) and the elapsed
// session duration (MM:SS) — re-rendering HH:MM each second is cheap and keeps them
// in lock-step without a second interval.
function renderHeaderTimes(): void {
  headerClock.textContent = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  $('session-elapsed').textContent = formatClock((Date.now() - callStartedAt) / 1000);
}
function startSessionTimer(): void {
  renderHeaderTimes(); // paint immediately, don't wait a tick
  // The on-video clock and participant badge appear on join; room visibility,
  // session duration and balance live permanently in the on-video info strip
  // below the meta cluster (#127 — replaces the old ⓘ info popover). (spec 0061)
  show(headerClock, true);
  show(partCountEl, true);
  clearInterval(sessionTimerId);
  sessionTimerId = window.setInterval(renderHeaderTimes, 1000);
}
function stopSessionTimer(): void {
  clearInterval(sessionTimerId);
  sessionTimerId = 0;
  show(headerClock, false);
  show(partCountEl, false);
}

function updateParticipantsList(): void {
  const myLang = session?.lang || 'en';
  const myName = session?.name || t('namePlaceholder');
  const myAvatar = auth.getUser()?.avatar_url ?? null;
  const items: Array<{ id: string; name: string; lang: string; isSelf: boolean; micMuted: boolean; handRaised: boolean; avatar: string | null; userId?: string | null }> = [];

  items.push({ id: myId, name: myName, lang: myLang, isSelf: true, micMuted: !micOn, handRaised, avatar: myAvatar });
  for (const [id, info] of peerNames) {
    items.push({ id, name: info.name, lang: info.lang, isSelf: false, micMuted: peerMicMuted.get(id) ?? false, handRaised: peerHandRaised.get(id) ?? false, avatar: info.avatar ?? null, userId: info.userId ?? null });
  }

  $('part-count-n').textContent = String(items.length); // live count (spec 0055)
  // Keep the quiz cost estimate current while its panel is open (room languages
  // may have just changed with this join/leave/lang update).
  if (!quizEl.classList.contains('hidden')) void refreshQuizCost();
  updateInviteAvailability(items.length); // show "Invite" only while a seat is free (spec 0082)
  updateMinigameAvailability(items.length); // tic-tac-toe needs 2+; close it if we end up alone
  // Your avatar (image when available, else initial + gradient) in the on-video
  // participant badge (spec 0061 / #98 → avatars in spec 0070 R2.3).
  fillAvatar(partAvatarEl, myName, myAvatar, 48, 1);

  participantsList.innerHTML = '';
  for (const p of items) {
    const el = document.createElement('div');
    el.className = `part-item${p.isSelf ? ' self' : ''}`;

    const avatar = document.createElement('span');
    avatar.className = 'part-avatar';
    fillAvatar(avatar, p.name, p.avatar, 64, 2);

    const info = document.createElement('div');
    info.className = 'part-info';
    const nameEl = document.createElement('div');
    nameEl.className = 'part-name';
    // textContent, NOT innerHTML: a peer's display name is attacker-controlled, so
    // innerHTML here was a relayed XSS (e.g. a name of `<img onerror=…>` ran JS in
    // every participant's tab and could exfiltrate the localStorage JWT). The value
    // is plain text (flag emoji + name + suffix), so no HTML is needed. (spec 0028)
    nameEl.textContent = `${FLAG[p.lang] || ''} ${p.name}${p.isSelf ? ` · ${t('you')}` : ''}`.trim();
    const langEl = document.createElement('div');
    langEl.className = 'part-lang';
    langEl.textContent = p.lang.toUpperCase();
    info.append(nameEl, langEl);

    const status = document.createElement('div');
    status.className = 'part-status';
    if (p.handRaised) {
      const hand = document.createElement('span');
      hand.className = 'part-hand';
      hand.textContent = '✋';
      status.appendChild(hand);
    }
    if (p.micMuted) {
      status.innerHTML += icon('mic-off', 16);
      status.querySelector('.ico')?.classList.add('part-status-danger');
    } else {
      // Show a plain mic icon for unmuted peers so everyone's mic state is visible at
      // a glance, not just the muted ones (incorporated from contributor PR #141).
      status.innerHTML += icon('mic', 16);
    }

    // Add-friend: only for a logged-in peer (has an account id) who isn't me, isn't
    // already a friend, and has no request pending either way (spec: friends).
    if (
      !p.isSelf &&
      p.userId &&
      auth.isLoggedIn() &&
      !friendIds.has(p.userId) &&
      !friendOutgoingIds.has(p.userId) &&
      !friendIncomingIds.has(p.userId)
    ) {
      const add = document.createElement('button');
      add.className = 'btn-ghost icon-btn part-addfriend';
      add.title = t('friendAdd');
      add.setAttribute('aria-label', t('friendAdd'));
      add.innerHTML = icon('user-plus', 16);
      const uid = p.userId;
      add.addEventListener('click', () => void addFriendByPeer(uid, add));
      status.appendChild(add);
    }

    el.append(avatar, info, status);
    participantsList.appendChild(el);
  }
}

// ---- Subtitles -------------------------------------------------------------
// `translation` / `original` are passed explicitly because the two STT engines
// disagree on what an interim frame carries: the server path sends interim text in
// the ORIGINAL language (translation arrives only on the final frame), while the
// Cartesia client-direct path (spec 0108) only ever yields TRANSLATED text and never
// the source. Each caller declares which text it actually has, so the per-mode
// rendering below stays correct regardless of engine.
function showSubtitle(
  speakerId: string,
  { translation, original, interim }: { translation?: string; original?: string; interim: boolean },
): void {
  if (subtitleMode === 'off') return;
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(speakerId)}"]`);
  if (!cell) return;
  const area = cell.querySelector('.subtitle-area') as HTMLElement;

  // Render the line(s) the current mode wants (pure logic in subtitle-render.ts, jsdom-tested).
  // `false` means nothing to show for this mode (e.g. an interim frame while showing only
  // translations): leave whatever is on screen rather than blanking it.
  if (!renderSubtitleInto(area, subtitleMode, { translation, original, interim })) return;

  const prev = subtitleTimers.get(speakerId);
  if (prev) clearTimeout(prev);
  if (!interim) {
    subtitleTimers.set(
      speakerId,
      window.setTimeout(() => {
        area.innerHTML = '';
        subtitleTimers.delete(speakerId);
      }, 6000),
    );
  }
}

// ---- Controls --------------------------------------------------------------
/** Toggle button state for assistive tech: aria-pressed + a label matching the tooltip. */
function setToggleState(btn: HTMLElement, pressed: boolean, label?: string): void {
  btn.setAttribute('aria-pressed', String(pressed));
  if (label) {
    btn.title = label;
    btn.setAttribute('aria-label', label);
  }
}

function setControlState(): void {
  btnMic.classList.toggle('active-danger', !micOn);
  btnMic.innerHTML = icon(micOn ? 'mic' : 'mic-off');
  setToggleState(btnMic, micOn);
  btnCam.classList.toggle('active-danger', !camOn);
  btnCam.innerHTML = icon(camOn ? 'video' : 'video-off');
  setToggleState(btnCam, camOn);
  const bgOn = bgMode === 'blur';
  btnBg.classList.toggle('active-success', bgOn);
  btnBg.innerHTML = icon('sparkles');
  setToggleState(btnBg, bgOn, t(bgOn ? 'bgBlurOn' : 'bgBlurTip'));
  btnTts.classList.toggle('active-success', ttsOn);
  btnTts.innerHTML = icon(ttsOn ? 'volume-on' : 'volume-off');
  setToggleState(btnTts, ttsOn);
  const subOn = subtitleMode !== 'off';
  btnSubtitle.classList.toggle('active-success', subtitleMode === 'both');
  btnSubtitle.classList.toggle('active-warning', subtitleMode === 'translated');
  btnSubtitle.classList.toggle('active-orange', subtitleMode === 'original');
  btnSubtitle.innerHTML = icon(subOn ? 'subtitle' : 'subtitle-off');
  const subKey =
    subtitleMode === 'translated' ? 'subTranslated'
    : subtitleMode === 'original' ? 'subOriginal'
    : subtitleMode === 'off' ? 'subtitleOffTip'
    : 'subtitleTip';
  // Keep data-i18n-title in sync so a later applyI18n() (language switch) reapplies
  // the right per-mode tooltip instead of reverting to the plain "Subtitles on".
  btnSubtitle.dataset.i18nTitle = subKey;
  setToggleState(btnSubtitle, subOn, t(subKey));
  btnHand.classList.toggle('active-success', handRaised);
  btnHand.innerHTML = icon(handRaised ? 'hand-raised' : 'hand');
  setToggleState(btnHand, handRaised, handRaised ? t('handUp') : t('handTip'));
  btnFullscreen.innerHTML = icon(document.fullscreenElement ? 'fullscreen-off' : 'fullscreen');
  btnPip.innerHTML = icon('pip');
  btnView.innerHTML = icon(viewMode === 'speaker' ? 'speaker' : 'grid');
  btnView.title = t(viewMode === 'speaker' ? 'viewGrid' : 'viewSpeaker');
  btnView.setAttribute('aria-label', btnView.title);
  btnShare.innerHTML = icon(isSharingScreen ? 'monitor' : 'monitor');
  btnShare.classList.toggle('active-success', isSharingScreen);
  setToggleState(btnShare, isSharingScreen, isSharingScreen ? t('stopShare') : t('screenShareTip'));
  btnRecord.innerHTML = icon('recording');
  btnRecord.classList.toggle('active-danger', isRecording);
  setToggleState(btnRecord, isRecording, isRecording ? t('recording') : t('recordingTip'));
  const partIco = btnParticipants.querySelector('.part-ico');
  if (partIco) partIco.innerHTML = icon('users');
  btnInvite.innerHTML = icon('user-plus');
  const chatIco = btnChat.querySelector('.chat-ico');
  if (chatIco) chatIco.innerHTML = icon('chat');
  const leave = document.getElementById('btn-leave');
  if (leave) leave.innerHTML = icon('leave');
  btnMore.innerHTML = icon('more');
  // Dot on ⋯ when a collapsed action is active, so its state isn't hidden.
  btnMore.classList.toggle('has-active', isSharingScreen || isRecording || handRaised);
  syncPipControls(); // keep the PiP-window bar in lock-step with the main bar (spec 0057)
}

// Overflow "More" menu (spec 0023): collapse secondary controls behind ⋯.
const moreMenu = $('more-menu');
let moreCloseTimer = 0;
function setMoreOpen(open: boolean): void {
  clearTimeout(moreCloseTimer); // cancel any pending auto-close so it can't shut a reopened menu
  moreMenu.classList.toggle('hidden', !open);
  btnMore.setAttribute('aria-expanded', String(open));
  if (open) moreMenu.querySelector<HTMLButtonElement>('.control-btn:not(.hidden)')?.focus();
}
btnMore.addEventListener('click', (e) => {
  e.stopPropagation();
  setMoreOpen(moreMenu.classList.contains('hidden'));
});
// Unified close behavior (#226): clicking ANY action in the ⋯ menu keeps it open
// briefly (~0.25s) — long enough to glimpse the result (a toggle's dot flipping, a
// panel opening) — then auto-closes. Consistent across toggles (tts/hand/share) and
// one-shot actions (timer/invite/label); rapid repeat clicks reset the timer.
// (Supersedes spec 0036's "stay open until dismissed" so the behavior is
// predictable across every action.)
moreMenu.addEventListener('click', (e) => {
  const btn = (e.target as HTMLElement).closest('.control-btn');
  if (!btn || !moreMenu.contains(btn)) return;
  clearTimeout(moreCloseTimer);
  moreCloseTimer = window.setTimeout(() => setMoreOpen(false), 250);
});
document.addEventListener('click', (e) => {
  if (!moreMenu.classList.contains('hidden') && !moreMenu.contains(e.target as Node)) setMoreOpen(false);
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !moreMenu.classList.contains('hidden')) setMoreOpen(false);
});

// Named so the PiP-window mic button can drive the exact same path (spec 0057).
function toggleMicrophone(): void {
  micOn = !micOn;
  mesh?.setAudioEnabled(micOn);
  audioCapture?.setMuted(!micOn);
  setAudioMuted(myId, !micOn);
  ws?.send(JSON.stringify({ type: 'mute_audio', muted: !micOn }));
  setControlState();
  updateParticipantsList(); // reflect your own mic state in the roster at once (#141)
}
btnMic.addEventListener('click', () => toggleMicrophone());

btnCam.addEventListener('click', () => {
  void toggleCamera();
});

async function toggleCamera(): Promise<void> {
  if (videoBusy) return;
  videoBusy = true;
  try {
    camOn = !camOn;
    // Acquire / release the physical camera so the hardware LED matches the UI
    // (enableCamera may revert camOn to false if the device can't be opened).
    if (camOn) {
      await enableCamera();
    } else {
      disableCamera();
    }
    // While screen-sharing the composite (screen + camera PiP) is always the
    // outgoing track, so toggling the camera must NOT hide our tile or tell peers
    // to hide video — the PiP just appears/disappears inside the composite (handled
    // by setOutgoingVideo / disableCamera). Outside a share, reflect camera-off on
    // our tile, the recorder, and peers as usual (spec 0053).
    if (!isSharingScreen) {
      setCameraOff(myId, !camOn);
      recorder?.setVideoOff(myId, !camOn);
      ws?.send(JSON.stringify({ type: 'mute_video', muted: !camOn }));
    }
  } finally {
    videoBusy = false;
    setControlState();
  }
}

btnBg.addEventListener('click', () => {
  void toggleBgBlur();
});

// Toggle the camera background blur. When the camera is live we reprocess the
// current raw track into the new outgoing track immediately; otherwise the mode
// is just recorded and applied next time the camera turns on (enableCamera).
async function toggleBgBlur(): Promise<void> {
  if (videoBusy) return;
  videoBusy = true;
  bgMode = bgMode === 'blur' ? 'none' : 'blur';
  setControlState(); // reflect intent right away (segmentation may load lazily)
  try {
    if (camOn && localStream) {
      // Rebuild the outgoing track in localStream even while screen-sharing —
      // setOutgoingVideo skips the peer push during a share, and stopScreenShare
      // then restores whatever (raw or blurred) track is in localStream.
      const raw = currentRawCameraTrack();
      if (raw) await setOutgoingVideo(raw);
    }
  } finally {
    videoBusy = false;
    setControlState(); // settle (buildOutgoing may have reverted the mode)
  }
}

// Fully release the camera device — track.stop() turns the hardware LED off,
// unlike track.enabled = false which keeps the device powered. The outgoing
// video is cleared on peers via replaceVideoTrack(null); the always-present
// video transceiver lets a later enableCamera swap a track back in with no
// renegotiation. While screen-sharing the sender carries the screen, so leave it.
function disableCamera(): void {
  // With background blur on, the real camera is the VB's source (not in
  // localStream); stop it too so the hardware LED actually turns off.
  if (vbg) {
    vbg.source?.stop();
    vbg.stop();
    vbg = null;
  }
  if (localStream) {
    for (const v of localStream.getVideoTracks()) {
      v.stop();
      localStream.removeTrack(v);
    }
  }
  // During a share the screen keeps flowing on the sender; just drop the camera
  // PiP from the composite (spec 0053). Otherwise clear the peers' video.
  if (isSharingScreen) screenPip?.setCamera(null);
  else mesh?.replaceVideoTrack(null);
}

// Re-open the camera and route its fresh track (raw or blurred) to peers + our
// tile. Reverts the toggle if the device can't be opened (busy / denied).
async function enableCamera(): Promise<void> {
  const track = await acquireVideoTrack();
  if (!track || !localStream) {
    track?.stop();
    camOn = false;
    if (!track) toast(t('camMicDenied'));
    return;
  }
  await setOutgoingVideo(track);
}

/** The live raw camera track, wherever it currently lives: held by the VB when
 *  blur is active, otherwise the localStream video track. */
function currentRawCameraTrack(): MediaStreamTrack | null {
  if (vbg?.source && vbg.source.readyState === 'live') return vbg.source;
  return localStream?.getVideoTracks().find((tr) => tr.readyState === 'live') ?? null;
}

// Produce the outgoing video track for `raw` honouring bgMode (raw camera, or a
// blurred track from the VirtualBackground), swap it into localStream — keeping
// `raw` alive when the VB reuses it as its source — and push it to peers + tile.
async function setOutgoingVideo(raw: MediaStreamTrack): Promise<void> {
  if (!localStream) return;
  const outgoing = await buildOutgoing(raw);
  if (!localStream) {
    // The call ended while a lazy model load was in flight.
    if (outgoing !== raw) outgoing.stop();
    return;
  }
  for (const v of localStream.getVideoTracks()) {
    if (v !== raw && v !== outgoing) v.stop(); // drop stale placeholder / old processed track
    localStream.removeTrack(v);
  }
  localStream.addTrack(outgoing);
  // While screen-sharing the track waits in localStream until sharing stops
  // (stopScreenShare restores it via replaceVideoTrack); don't disturb the screen
  // sender — instead route the (raw or blurred) track into the camera PiP so the
  // overlay reflects the camera turning on / blur toggling mid-share (spec 0053).
  if (!isSharingScreen) {
    mesh?.replaceVideoTrack(outgoing); // swap the video sender (transceiver-backed)
    setSelfVideo(localStream);
    recorder?.updateStream(myId, localStream);
  } else {
    screenPip?.setCamera(outgoing);
  }
}

// Returns the track to send for `raw`: the raw camera (no effect) or a blurred
// track from the VirtualBackground. Falls back to the raw track and resets the
// mode if the segmentation model can't load.
async function buildOutgoing(raw: MediaStreamTrack): Promise<MediaStreamTrack> {
  if (bgMode === 'none') {
    if (vbg) { vbg.stop(); vbg = null; }
    return raw;
  }
  // Lazy chunk (spec 0105): the segmentation model + its module load only when blur is used.
  const { VirtualBackground } = await import('./virtual-background');
  const instance = vbg ?? (vbg = new VirtualBackground());
  const track = await instance.start(raw);
  // disableCamera / leaveCall may have torn us down during the model load.
  if (vbg !== instance) {
    instance.stop();
    return raw;
  }
  if (!instance.active) {
    instance.stop();
    vbg = null;
    bgMode = 'none';
    toast(t('bgUnavailable'));
    return raw;
  }
  return track;
}

btnTts.addEventListener('click', () => {
  ttsOn = !ttsOn;
  if (ttsOn) {
    unlockTts(); // iOS: re-prime within this tap when turning voice on
    pcmPlayback.unlock();
  } else {
    stopTts();
    pcmPlayback.reset(); // stop any queued translated audio when muting voice
  }
  applyAudioMode(); // mute/unmute foreign originals to match the mode
  setControlState();
});

// ---- Audio settings (Vox Voices) -------------------------------------------
// The modal contents are lazy-loaded on first open; show() gives the overlay its
// focus trap. Reachable from the pre-join card and the in-call ⋯ menu.
const audioModal = $('audio-modal');
let audioSettingsMod: typeof import('./audio-settings') | null = null;
async function openAudioSettings(): Promise<void> {
  show(audioModal, true);
  try {
    audioSettingsMod = audioSettingsMod ?? (await import('./audio-settings'));
    await audioSettingsMod.openAudioSettings();
  } catch {
    /* settings unavailable — Browser Voice still works */
  }
}

// Show the audio-settings entry points (pre-join button + in-call "…" menu item) only
// for Standard tier (other tiers have server-side or Cartesia voice synthesis, so the
// browser TTS voice choice is irrelevant).
const prejoinAudioBtn = $('prejoin-audio-btn');
const miAudioSettings = $('mi-audio-settings');
function syncAudioSettingsBtn(): void {
  const standard = selectedEngine === 'standard';
  prejoinAudioBtn.classList.toggle('hidden', !standard);
  miAudioSettings.classList.toggle('hidden', !standard);
}

$('btn-audio-settings').innerHTML = icon('headphones', 22); // match pre-join icon + menu sizing
$('prejoin-audio-btn').addEventListener('click', () => void openAudioSettings());
$('btn-audio-settings').addEventListener('click', () => void openAudioSettings());
$('audio-close').addEventListener('click', () => show(audioModal, false));
audioModal.addEventListener('click', (e) => {
  if (e.target === audioModal) show(audioModal, false);
});

btnSubtitle.addEventListener('click', () => {
  const i = SUBTITLE_CYCLE.indexOf(subtitleMode);
  subtitleMode = SUBTITLE_CYCLE[(i + 1) % SUBTITLE_CYCLE.length];
  // Clear current overlays so the new mode renders cleanly from the next frame
  // (and so 'off' blanks immediately).
  document.querySelectorAll<HTMLElement>('.subtitle-area').forEach((a) => { a.innerHTML = ''; });
  setControlState();
});

function toggleHand(): void {
  handRaised = !handRaised;
  ws?.send(JSON.stringify({ type: 'hand_raise', raised: handRaised }));
  if (handRaised) {
    track('hand_raised');
    playHandRaiseSound(); // confirmation cue for the local user
  }
  // The server relays hand_raised to peers only — update our own tile + list.
  setHandIndicator(myId, handRaised);
  updateParticipantsList();
  setControlState();
}
btnHand.addEventListener('click', () => toggleHand());

btnFullscreen.addEventListener('click', () => {
  if (!document.fullscreenElement) {
    document.documentElement.requestFullscreen().catch(() => {});
  } else {
    document.exitFullscreen().catch(() => {});
  }
});

btnPip.addEventListener('click', () => {
  if (pipWindow && !pipWindow.closed) {
    pipObserver?.disconnect();
    pipObserver = null;
    pipWindow.close();
    pipWindow = null;
    pipCtl = null;
    return;
  }
  if (!('documentPictureInPicture' in window)) return;
  // Size the floating window to the current participant count instead of a fixed
  // oversized box: a compact 16:9 single feed when solo, a roomier grid otherwise.
  const tileCount = Math.max(videoGrid.querySelectorAll('.video-cell').length, 1);
  const pipSize = tileCount <= 1 ? { width: 320, height: 180 } : { width: 420, height: 320 };
  (window as any).documentPictureInPicture
    .requestWindow(pipSize)
    .then((w: Window) => {
      pipWindow = w;
      // Copy stylesheets into the PiP window. Use <link> for external sheets (preserves
      // browser cache and avoids SecurityError on cssRules access in some contexts).
      [...document.styleSheets].forEach((sheet) => {
        if (sheet.href) {
          const link = w.document.createElement('link');
          link.rel = 'stylesheet';
          link.href = sheet.href;
          w.document.head.appendChild(link);
        } else {
          try {
            const style = w.document.createElement('style');
            style.textContent = [...sheet.cssRules].map((r) => r.cssText).join('\n');
            w.document.head.appendChild(style);
          } catch { /* cross-origin sheet — skip */ }
        }
      });
      // When the PiP window is small the copied stylesheet drops the name overlay to
      // the bottom, where the long name gets clipped and sits under the control bar.
      // Keep the identity badge top-left (avatar + flag) and hide only the name at
      // small sizes; enlarging the window past 768px restores the full name, as before.
      const pipFix = w.document.createElement('style');
      pipFix.textContent =
        '@media (max-width:768px){' +
        '.video-grid .video-overlay{top:8px!important;bottom:auto!important;}' +
        '.video-grid .peer-name{display:none!important;}' +
        '}';
      w.document.head.appendChild(pipFix);
      w.document.body.style.cssText = 'margin:0;background:#000;overflow:hidden';
      // Build a BARE stage with just the video grid — NOT a clone of the whole
      // stage. This drops the session-meta overlays (call timer, room code,
      // public/private, balance, participant count) that don't belong in the mini
      // view. Tiles + their live streams are populated by syncPip().
      const pipStage = w.document.createElement('div');
      pipStage.className = 'video-stage';
      pipStage.style.cssText = 'position:relative;width:100%;height:100dvh';
      const pipGrid = w.document.createElement('div');
      pipGrid.className = 'video-grid';
      pipStage.appendChild(pipGrid);
      // Carry Astro's component scope attribute(s) onto the fresh PiP stage + grid. The
      // .video-cell background and the video sizing (object-fit / width / height / display)
      // are scoped UNDER .video-stage / .video-grid (index.astro: "target them with
      // :global() under .video-grid"), so without the cid the cloned tiles get no sizing
      // and the feeds never fill — a grey stage with no video. (#246 regressed this by
      // building a bare grid instead of cloning the scoped stage.)
      const carryScope = (from: Element | null, to: HTMLElement): void => {
        for (const a of from?.getAttributeNames() ?? [])
          if (a.startsWith('data-astro-cid')) to.setAttribute(a, from?.getAttribute(a) ?? '');
      };
      carryScope(document.querySelector('.video-stage'), pipStage);
      carryScope(videoGrid, pipGrid);
      w.document.body.appendChild(pipStage);
      syncPip();
      // Keep the PiP grid in lock-step with the live call: a peer leaving removes its
      // tile (no black box), joins add one, and relayouts mirror — instead of a stale
      // one-time clone. Display-only tiles stay muted (audio plays from the main
      // window). Observer disconnected on close.
      pipObserver = new MutationObserver(schedulePipSync);
      pipObserver.observe(videoGrid, {
        childList: true,
        subtree: true,
        attributes: true,
        attributeFilter: ['class', 'style', 'data-peer', 'data-peers', 'data-mode'],
      });
      buildPipControls(w); // spec 0057: live controls inside the floating window
      syncPipControls();
      w.addEventListener('pagehide', () => {
        pipObserver?.disconnect();
        pipObserver = null;
        pipWindow = null;
        pipCtl = null;
      });
    })
    .catch(() => {});
});

// Mirror the live call grid into the PiP window (spec 0057 + PiP fixes): same tiles
// and grid template, live srcObjects, state classes — and crucially REMOVE tiles for
// peers who left (no more black box) and ADD tiles for joiners. Clones are
// display-only (muted; audio plays from the main window). Per-tile controls
// (ban/report, pan/zoom) are stripped from the mini view.
function syncPip(): void {
  if (!pipWindow || pipWindow.closed) return;
  const pipGrid = pipWindow.document.querySelector<HTMLElement>('.video-grid');
  if (!pipGrid) return;
  pipGrid.style.gridTemplateColumns = videoGrid.style.gridTemplateColumns;
  pipGrid.style.gridTemplateRows = videoGrid.style.gridTemplateRows;
  pipGrid.dataset.peers = videoGrid.dataset.peers ?? '';
  if (videoGrid.dataset.mode) pipGrid.dataset.mode = videoGrid.dataset.mode;
  else delete pipGrid.dataset.mode;

  const live = new Set(
    [...videoGrid.querySelectorAll<HTMLElement>('.video-cell[data-peer]')].map((c) => c.dataset.peer),
  );
  pipGrid.querySelectorAll<HTMLElement>('.video-cell[data-peer]').forEach((c) => {
    if (!live.has(c.dataset.peer)) c.remove(); // peer left → drop the tile (no black box)
  });

  videoGrid.querySelectorAll<HTMLElement>('.video-cell[data-peer]').forEach((liveCell) => {
    const peer = liveCell.dataset.peer ?? '';
    let cell = pipGrid.querySelector<HTMLElement>(`.video-cell[data-peer="${cssEsc(peer)}"]`);
    if (!cell) {
      cell = liveCell.cloneNode(true) as HTMLElement;
      cell.querySelectorAll('.cell-actions, .pan-toggle, .pan-hint').forEach((e) => e.remove());
      pipGrid.appendChild(cell);
    } else {
      cell.className = liveCell.className; // mirror state: camera-off / sharing / speaking / reconnecting
    }
    const orig = liveCell.querySelector<HTMLVideoElement>('video');
    const cv = cell.querySelector<HTMLVideoElement>('video');
    if (cv) {
      cv.muted = true;
      if (orig?.srcObject && cv.srcObject !== orig.srcObject) {
        cv.srcObject = orig.srcObject;
        void cv.play().catch(() => {});
      }
    }
  });
}

/** Coalesce PiP re-syncs to one per frame. */
function schedulePipSync(): void {
  if (!pipWindow || pipWindow.closed) return;
  cancelAnimationFrame(pipSyncRaf);
  pipSyncRaf = requestAnimationFrame(syncPip);
}

// Build the in-PiP control bar (spec 0057). Buttons live in the PiP document but
// their listeners are closures here, so they drive the main call state directly.
function buildPipControls(w: Window): void {
  const bar = w.document.createElement('div');
  bar.className = 'pip-controls';
  const mk = (title: string, onClick: () => void, danger = false): HTMLButtonElement => {
    const b = w.document.createElement('button');
    b.type = 'button';
    b.className = 'pip-ctl-btn' + (danger ? ' danger' : '');
    b.title = title;
    b.setAttribute('aria-label', title);
    b.addEventListener('click', onClick);
    bar.appendChild(b);
    return b;
  };
  const mic = mk(t('muteTip'), () => toggleMicrophone());
  const cam = mk(t('camTip'), () => void toggleCamera());
  const share = mk(t('screenShareTip'), () => toggleScreenShare());
  const hand = mk(t('handTip'), () => toggleHand());
  const end = mk(t('leaveTip'), () => leaveCall(), true); // leaveCall() also closes PiP
  pipCtl = { mic, cam, share, hand, end };
  w.document.body.appendChild(bar);
}

// Mirror live call state onto the PiP buttons. Called from setControlState() so the
// PiP bar can never drift from the main bar, and once on open.
function syncPipControls(): void {
  if (!pipCtl || !pipWindow || pipWindow.closed) return;
  pipCtl.mic.innerHTML = icon(micOn ? 'mic' : 'mic-off');
  pipCtl.mic.classList.toggle('active-danger', !micOn);
  pipCtl.cam.innerHTML = icon(camOn ? 'video' : 'video-off');
  pipCtl.cam.classList.toggle('active-danger', !camOn);
  pipCtl.share.innerHTML = icon('monitor');
  pipCtl.share.classList.toggle('active-success', isSharingScreen);
  pipCtl.hand.innerHTML = icon(handRaised ? 'hand-raised' : 'hand');
  pipCtl.hand.classList.toggle('active-success', handRaised);
  pipCtl.end.innerHTML = icon('leave');
}

// PiP discoverability (spec 0057): Document PiP can't auto-open (it needs a user
// gesture), so rather than surprising anyone we hint — ONCE, ever — that the call can
// be popped out to stay visible, shown when they return from another tab mid-call.
let pipTabAway = false;
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden') {
    if (callStartedAt > 0) pipTabAway = true;
    return;
  }
  if (
    pipTabAway &&
    callStartedAt > 0 &&
    'documentPictureInPicture' in window &&
    !(pipWindow && !pipWindow.closed) &&
    !localStorage.getItem('pipHintSeen')
  ) {
    localStorage.setItem('pipHintSeen', '1');
    showNotif(t('pipHint'));
  }
  pipTabAway = false;
});

btnView.addEventListener('click', () => {
  viewMode = viewMode === 'grid' ? 'speaker' : 'grid';
  if (viewMode === 'grid') pinnedPeerId = null;
  setControlState();
  layoutVideos();
  updatePinButtons();
});

function toggleScreenShare(): void {
  if (isSharingScreen) stopScreenShare();
  else startScreenShare();
}
btnShare.addEventListener('click', () => toggleScreenShare());

async function startScreenShare(): Promise<void> {
  // Independent of the camera: works whether you're camera-on, camera-off, or
  // joined audio-only. We keep `localStream` as the real mic/camera stream and
  // only swap the outgoing *video* track for the screen.
  track('screen_share_started');
  if (!mesh) return;
  try {
    // audio: true makes the browser offer the "share tab/system audio" checkbox
    // (spec 0085). Chrome/Edge desktop only; Firefox/Safari/mobile just ignore it.
    const s = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: true });
    screenStream = s;
    isSharingScreen = true;
    // If the user ticked "share audio", route ONLY the SCREEN audio to peers — not
    // the mic. The sharer's voice reaches listeners as translated speech (TTS) +
    // subtitles, so it is no longer doubled with the untranslated original (which a
    // sharing peer is never ducked for, see applyAudioMode/#229). The mic is still
    // mixed with the screen audio into `shareMixTrack`, but that mix is used ONLY
    // for the LOCAL recording (selfRecordingStream) — it is never sent to peers, so
    // the recording still captures both voice and shared audio. The mic continues to
    // feed STT unchanged. It leaves on the always-present audio sender via
    // replaceTrack — no renegotiation. No screen-audio track (box unticked /
    // unsupported) → the mic path is untouched.
    // True once the shared tab/system audio is actually on the wire to peers, so
    // we can tell them to keep it audible across a language gap (spec 0085). A
    // share without audio leaves it false → the bare mic stays duckable as usual.
    let shareHasAudio = false;
    const shareAudio = s.getAudioTracks();
    if (shareAudio.length) {
      try {
        shareAudioCtx = new AudioContext();
        await shareAudioCtx.resume(); // a suspended context yields a SILENT mix → no audio reaches peers
        const dest = shareAudioCtx.createMediaStreamDestination();
        shareAudioCtx.createMediaStreamSource(new MediaStream(shareAudio)).connect(dest);
        const mic = localStream?.getAudioTracks() ?? [];
        if (mic.length) {
          shareAudioCtx.createMediaStreamSource(new MediaStream(mic)).connect(dest);
        }
        // mic+screen mix — for the local recording only.
        shareMixTrack = dest.stream.getAudioTracks()[0] ?? null;
        // Peers get the screen audio alone (mic excluded → no doubled voice).
        mesh.replaceAudioTrack(shareAudio[0]);
        shareHasAudio = true;
      } catch {
        // WebAudio unavailable → mic-only audio (if any); the screen video still shares.
        shareAudioCtx = null;
        shareMixTrack = null;
      }
    }
    // Composite the camera as a PiP overlay onto the screen (spec 0053) and send
    // that single track on every peer's video sender (mic audio untouched). With
    // the camera off / audio-only it's just the screen — no PiP. The always-present
    // video transceiver means this reaches peers even when we joined without a
    // camera, and replaceTrack keeps the mesh free of renegotiation.
    const { ScreenSharePip } = await import('./screenshare-pip'); // lazy chunk (spec 0105)
    screenPip = new ScreenSharePip(s);
    screenPip.setCamera(camOn ? (localStream?.getVideoTracks()[0] ?? null) : null);
    const composite = screenPip.start();
    const shareTrack = composite.getVideoTracks()[0] ?? null;
    if (shareTrack) shareTrack.contentHint = 'detail'; // favour sharpness over framerate for text/UI (spec 0088)
    mesh.replaceVideoTrack(shareTrack);
    mesh.setVideoBudget(VIDEO_BUDGET_SCREEN); // higher cap so the shared screen isn't grainy (spec 0088)
    // Peers may have us flagged camera-off (their tile would hide the video);
    // tell them to reveal it so the shared screen actually shows.
    ws?.send(JSON.stringify({ type: 'mute_video', muted: false }));
    // Our own tile + recorder show the composite (screen + our PiP) — exactly what
    // peers see — regardless of camera state.
    setSelfVideo(composite);
    setCameraOff(myId, false);
    recorder?.updateStream(myId, selfRecordingStream()); // composite video + mic/screen audio mix (#230)
    recorder?.setVideoOff(myId, false);
    // Show indicator on self cell; mark it sharing so the self-view mirror is
    // dropped (a flipped screen share would render its text backwards).
    const cell = videoGrid.querySelector(`[data-peer="${cssEsc(myId)}"]`);
    cell?.classList.add('sharing');
    if (cell) {
      let badge = cell.querySelector('.screen-share-badge') as HTMLElement | null;
      if (!badge) {
        badge = document.createElement('span');
        badge.className = 'screen-share-badge';
        badge.textContent = '🖥';
        cell.querySelector('.video-overlay')?.appendChild(badge);
      }
    }
    // Stop sharing when user clicks "Stop sharing" in browser
    s.getVideoTracks()[0]?.addEventListener('ended', stopScreenShare);
    playScreenShareSound(); // audible cue that screen sharing has started
    ws?.send(JSON.stringify({ type: 'screen_share', active: true, audio: shareHasAudio })); // tell peers (spec 0033/0085)
    spotlightShare(myId, true); // zoom my own share into focus too (spec 0089)
    setControlState();
  } catch {
    // User cancelled the picker (or a rare post-acquire failure) — roll back the
    // optimistic state and release anything we already grabbed.
    isSharingScreen = false;
    screenPip?.stop();
    screenPip = null;
    screenStream?.getTracks().forEach((t) => t.stop());
    screenStream = null;
    if (shareMixTrack) {
      mesh?.replaceAudioTrack(localStream?.getAudioTracks()[0] ?? null);
      shareMixTrack.stop();
      shareMixTrack = null;
    }
    if (shareAudioCtx) {
      void shareAudioCtx.close();
      shareAudioCtx = null;
    }
  }
}

function stopScreenShare(): void {
  track('screen_share_stopped');
  if (!isSharingScreen || !mesh) return;
  isSharingScreen = false;
  // Tear down the compositor (RAF loop + canvas track) before releasing the
  // screen so no draw loop is left running (spec 0053).
  screenPip?.stop();
  screenPip = null;
  if (screenStream) {
    screenStream.getTracks().forEach((t) => t.stop());
    screenStream = null;
  }
  // Revert the audio sender to the plain mic track and release the mix (spec 0085).
  if (shareMixTrack) {
    mesh.replaceAudioTrack(localStream?.getAudioTracks()[0] ?? null);
    shareMixTrack.stop();
    shareMixTrack = null;
  }
  if (shareAudioCtx) {
    void shareAudioCtx.close();
    shareAudioCtx = null;
  }
  // Restore the camera feed for peers (or clear video when the camera is off /
  // we joined audio-only), honouring the current camera toggle.
  const camTrack = localStream?.getVideoTracks()[0] ?? null;
  mesh.replaceVideoTrack(camTrack);
  mesh.setVideoBudget(IS_MOBILE ? VIDEO_BUDGET_MOBILE : VIDEO_BUDGET_DESKTOP); // back to the camera budget (spec 0088)
  mesh.setVideoEnabled(camOn);
  ws?.send(JSON.stringify({ type: 'mute_video', muted: !camOn }));
  // Our own tile + recorder back to the camera (or camera-off avatar).
  setSelfVideo(localStream);
  setCameraOff(myId, !camOn);
  recorder?.updateStream(myId, localStream);
  recorder?.setVideoOff(myId, !camOn);
  // Remove badge + restore the camera self-view mirror.
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(myId)}"]`);
  cell?.classList.remove('sharing');
  cell?.querySelector('.screen-share-badge')?.remove();
  if (cell) disablePan(cell as HTMLElement); // drop the mobile pan/zoom ⊕ on the self tile too
  ws?.send(JSON.stringify({ type: 'screen_share', active: false, audio: false })); // tell peers (spec 0033)
  spotlightShare(myId, false); // restore the prior focus (zoom-out) (spec 0089)
  setControlState();
  showNotif(t('stopShare'));
}

/** Point the self tile's <video> at a stream (camera or screen). Re-assigning the
 *  same MediaStream object is a no-op, so null it first to force a re-render when
 *  the stream's video track was swapped in place (camera ↔ blur). */
function setSelfVideo(stream: MediaStream | null): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(myId)}"]`);
  const video = cell?.querySelector('video') as HTMLVideoElement | null;
  if (!video || !stream) return;
  if (video.srcObject === stream) video.srcObject = null;
  video.srcObject = stream;
  void video.play().catch(() => {});
}

btnRecord.addEventListener('click', () => {
  if (isRecording) {
    void stopRecording();
  } else {
    void startRecording();
  }
});

// Build a recorder source for one participant. Self is special: the tile shows
// whatever peers see (screen share wins over camera) and `videoOff` must stay
// false while sharing even if the camera toggle is off.
function participantSource(peerId: string, stream: MediaStream | null): ParticipantSource {
  const isSelf = peerId === myId;
  return {
    peerId,
    name: isSelf ? session?.name || t('namePlaceholder') : peerNames.get(peerId)?.name || 'Guest',
    stream,
    videoOff: isSelf ? !camOn && !isSharingScreen : !!peerCamOff.get(peerId),
  };
}

/** The self stream the composite recorder should capture: the current self VIDEO
 *  (screen-share composite while sharing, else the camera/blur track from
 *  localStream) PLUS the self AUDIO going to peers — the mic+screen-audio mix
 *  while sharing with audio, otherwise the mic. The ScreenSharePip canvas track is
 *  video-only, so without folding the audio back in, the recorder lost ALL self
 *  audio (mic AND the shared screen audio) during a share (#230). */
function selfRecordingStream(): MediaStream {
  const video =
    isSharingScreen && screenPip?.stream
      ? screenPip.stream.getVideoTracks()
      : (localStream?.getVideoTracks() ?? []);
  const audio =
    isSharingScreen && shareMixTrack ? shareMixTrack : (localStream?.getAudioTracks()[0] ?? null);
  return new MediaStream([...video, ...(audio ? [audio] : [])]);
}

// Whiteboard-as-recording-tile (#230): when the board is open we feed its live
// canvas into the composite recorder like an extra participant, so collaborative
// drawing shows up in the saved file.
const WB_RECORDING_ID = '__whiteboard__';
function isWhiteboardOpen(): boolean {
  return !wbOverlay.classList.contains('hidden');
}
function whiteboardRecordingSource(): ParticipantSource {
  return { peerId: WB_RECORDING_ID, name: t('whiteboardTip'), stream: whiteboard.captureStream(), videoOff: false };
}

/** Current roster for the compositor: self first, then peers in join order. */
function recorderSources(): ParticipantSource[] {
  // During a share the self source is the composite (screen + camera PiP) plus the
  // mic+screen audio mix, so a recording started mid-share captures exactly what
  // peers see AND hear (spec 0053 / #230).
  const sources = [participantSource(myId, selfRecordingStream())];
  for (const [peerId] of peerNames) {
    sources.push(participantSource(peerId, remoteStreams.get(peerId) ?? null));
  }
  // Capture the whiteboard too when it's open at record start (#230).
  if (isWhiteboardOpen()) sources.push(whiteboardRecordingSource());
  return sources;
}

async function startRecording(): Promise<void> {
  track('recording_started');
  if (recorder || !localStream) return;
  // Lazy chunk (spec 0105): the canvas compositor + audio mixer + encoder load only on record.
  let CompositeRecorder: typeof import('./recording/composite-recorder').CompositeRecorder;
  try {
    ({ CompositeRecorder } = await import('./recording/composite-recorder'));
  } catch {
    showNotif(t('loadFailed'));
    return;
  }
  // The chunk load is async — bail if a concurrent click or call-end intervened.
  if (recorder || !localStream) return;
  recorder = new CompositeRecorder({
    sources: recorderSources(),
    // Mid-session failure: stop gracefully and save the chunks collected so far.
    onError: () => void stopRecording(true),
  });
  isRecording = true;
  playRecordingStartSound(); // audible cue that recording has started
  showNotif(t('recording'));
  // Prominent one-time notice so the user knows exactly what the file will contain
  // (whole call, live-updating layout, camera-off = initials tile).
  toast(t('recordingNotice'));
  $('rec-timer').textContent = '00:00';
  show($('rec-badge'), true);
  // Reserve the centre lane for the REC badge so the meta/participant badges can't
  // slide under it (spec 0070 R2.1).
  document.querySelector('.video-stage')?.classList.add('recording');
  recTimerId = window.setInterval(() => {
    if (!recorder) return;
    $('rec-timer').textContent = formatElapsed(Date.now() - recorder.startedAt);
    // Self-heal the recording roster from the live call so anyone who joins or
    // leaves is captured even if their add/remove event was missed (#"records
    // only me"): keep the composite tiles + audio mix matching who's actually here.
    recorder.syncRoster(recorderSources());
  }, 1000);
  setControlState();
}

async function stopRecording(partial = false): Promise<void> {
  const rec = recorder;
  if (!rec) return;
  recorder = null;
  // Capture the cloud-upload target NOW, synchronously. The leave/end-call path nulls
  // `activeSessionId` during teardown, and the upload only happens after the async
  // `rec.stop()` below resolves — reading the module global at that point would see
  // null and silently fall back to a LOCAL download. That stranded every recording of
  // a business call that was ended via hang-up (only manual mid-call stops uploaded).
  const sessionId = activeSessionId;
  // Use the recorder's own start time — there is NO module-level `recordingStartedAt`.
  // Referencing that undeclared name here threw a ReferenceError that aborted stopRecording
  // right after nulling `recorder`, wedging the call: the MediaRecorder kept running,
  // `isRecording` stayed true, `recorder` was null, so every later stop click no-op'd
  // (#"can't stop the recording", any platform/account).
  const duration = Math.floor((Date.now() - rec.startedAt) / 1000);
  if (!partial) track('recording_stopped', { duration_seconds: duration });
  isRecording = false;
  clearInterval(recTimerId);
  show($('rec-badge'), false);
  document.querySelector('.video-stage')?.classList.remove('recording');
  setControlState();
  showNotif(t('processing'));
  const blob = await rec.stop();
  if (blob.size > 0) {
    // Business cloud recording → upload to the workspace; otherwise download locally.
    if (bizRecording && sessionId && billing && auth.isLoggedIn()) {
      const dur = Math.round((Date.now() - rec.startedAt) / 1000);
      const ok = await uploadRecording(sessionId, blob, dur);
      if (ok) {
        showNotif(t('bizRecordingUploaded'));
      } else {
        auth.downloadBlob(blob, recordingFilename(session?.room || 'call', new Date()));
      }
    } else {
      auth.downloadBlob(blob, recordingFilename(session?.room || 'call', new Date()));
    }
  }
  if (partial) toast(t('recordingPartial'));
}

btnParticipants.addEventListener('click', () => toggleParticipants());

btnChat.addEventListener('click', () => toggleChat());
$('chat-close').addEventListener('click', () => toggleChat(false));
function toggleChat(force?: boolean): void {
  const open = force ?? !chatPanel.classList.contains('open');
  chatPanel.classList.toggle('open', open);
  chatPanel.classList.toggle('closed', !open);
  btnChat.setAttribute('aria-expanded', String(open));
  chat?.setOpen(open);
  if (open) chatInput.focus();
  // The desktop sidebar narrows call-main — re-fit after the transition.
  setTimeout(layoutVideos, 320);
}

function sendChat(): void {
  const text = chatInput.value;
  if (!text.trim()) return; // nothing to send; also avoids clearing on a stray Enter
  chat?.sendMessage(text);
  chatInput.value = '';
  fitChatInput(); // shrink the textarea back to one row
  updateChatCounter();
}
// Grow the textarea with its content up to the cap, then scroll (spec 0070).
function fitChatInput(): void {
  chatInput.style.height = 'auto';
  const { height, overflowY } = resizeBox(chatInput.scrollHeight, CHAT_MAX_HEIGHT);
  chatInput.style.height = `${height}px`;
  chatInput.style.overflowY = overflowY;
}
// Show "{used}/{max}" once the message nears the cap; warn at/near the limit.
function updateChatCounter(): void {
  const used = chatInput.value.length;
  const state = counterState(used);
  chatCounter.textContent = counterLabel(used);
  chatCounter.classList.toggle('hidden', state === 'hidden');
  chatCounter.classList.toggle('warn', state === 'warn');
}
$('chat-send').addEventListener('click', sendChat);
chatInput.addEventListener('keydown', (e) => {
  // Enter sends; Shift+Enter inserts a newline (default textarea behaviour).
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    sendChat();
  }
});
chatInput.addEventListener('input', () => {
  fitChatInput();
  updateChatCounter();
});
fitChatInput(); // size correctly on first paint

// ---- Chat file upload (spec 0018) ------------------------------------------
// The attach button + drag-and-drop appear only when the backend has Supabase
// Storage configured (probed once at startup; the chat panel is in-call only).
// Voice messages ride the same /files pipeline, so reveal the mic alongside attach.
void fileUploadEnabled().then((on) => {
  if (on) {
    chatAttach.hidden = false;
    chatRecord.hidden = false;
  }
});

// Single source of truth for the picker filter (extensions + MIME types), so the
// mobile file picker doesn't grey out documents (see UPLOAD_ACCEPT).
chatFileInput.accept = UPLOAD_ACCEPT;
chatAttach.addEventListener('click', () => chatFileInput.click());
chatFileInput.addEventListener('change', () => {
  const file = chatFileInput.files?.[0];
  chatFileInput.value = ''; // allow re-picking the same file
  if (file) void handleFileUpload(file);
});

// Drag-and-drop onto the chat panel. `dragDepth` tracks enter/leave across child
// elements so the overlay doesn't flicker as the cursor crosses nested nodes.
let dragDepth = 0;
chatPanel.addEventListener('dragenter', (e) => {
  if (chatAttach.hidden) return;
  e.preventDefault();
  dragDepth++;
  chatDrop.classList.remove('hidden');
});
chatPanel.addEventListener('dragover', (e) => {
  if (!chatAttach.hidden) e.preventDefault();
});
chatPanel.addEventListener('dragleave', () => {
  if (chatAttach.hidden) return;
  dragDepth = Math.max(0, dragDepth - 1);
  if (dragDepth === 0) chatDrop.classList.add('hidden');
});
chatPanel.addEventListener('drop', (e) => {
  if (chatAttach.hidden) return;
  e.preventDefault();
  dragDepth = 0;
  chatDrop.classList.add('hidden');
  const file = e.dataTransfer?.files?.[0];
  if (file) void handleFileUpload(file);
});

async function handleFileUpload(file: File): Promise<void> {
  if (chatAttach.hidden || !session) return;
  const reject = checkUploadFile(file);
  if (reject) {
    showNotif(reject === 'size' ? t('fileTooBig') : t('fileType'));
    return;
  }
  // Show progress; the translated message itself arrives over the WebSocket.
  chatUpload.classList.remove('hidden');
  chatUploadFill.style.width = '0%';
  chatUploadLabel.textContent = t('uploading');
  chatAttach.setAttribute('disabled', 'true');
  const res = await uploadChatFile(session.room, myId, file, (frac) => {
    chatUploadFill.style.width = `${Math.round(frac * 100)}%`;
  });
  chatUpload.classList.add('hidden');
  chatAttach.removeAttribute('disabled');
  if (!res.ok) {
    showNotif(t('uploadFailed'));
  } else if (res.translateBlocked) {
    // File shared, but the document text wasn't translated (pay-to-translate).
    showNotif(
      res.translateBlocked === 'signin'
        ? t('uploadNotTranslatedSignin')
        : t('uploadNotTranslatedCredits'),
    );
  }
}

// ---- Voice messages --------------------------------------------------------
// Tap the mic to record (a fresh getUserMedia stream, independent of the call
// mic so a muted mic still records), then stop & send or discard from the bar.
// The clip uploads through the same /files endpoint; the server transcribes it
// (Deepgram batch) and translates the transcript like any chat message.
let voiceRecorder: MediaRecorder | null = null;
let voiceChunks: BlobPart[] = [];
let voiceStream: MediaStream | null = null;
let voiceTimer: number | null = null;
let voiceStartMs = 0;
let voiceShouldSend = false;
/** Hard cap on a voice message; well within the 5 MB upload ceiling. */
const VOICE_MAX_MS = 60_000;

async function startVoiceRecording(): Promise<void> {
  if (voiceRecorder || chatRecord.hidden || !session) return; // recording / storage off
  let stream: MediaStream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch {
    showNotif(t('camMicDenied'));
    return;
  }
  const mime = ['audio/webm;codecs=opus', 'audio/webm', 'audio/mp4'].find((ty) =>
    MediaRecorder.isTypeSupported?.(ty),
  );
  voiceChunks = [];
  voiceShouldSend = false;
  try {
    voiceRecorder = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
  } catch {
    stream.getTracks().forEach((tr) => tr.stop());
    showNotif(t('uploadFailed'));
    return;
  }
  voiceStream = stream;
  voiceRecorder.ondataavailable = (e) => {
    if (e.data.size) voiceChunks.push(e.data);
  };
  voiceRecorder.onstop = finalizeVoiceRecording;
  voiceRecorder.start();
  voiceStartMs = Date.now();
  chatRecTime.textContent = '0:00';
  chatInputRow.classList.add('recording');
  voiceTimer = window.setInterval(() => {
    const elapsed = Date.now() - voiceStartMs;
    chatRecTime.textContent = recTimeLabel(elapsed);
    if (elapsed >= VOICE_MAX_MS) stopVoiceRecording(true); // auto-send at the cap
  }, 200);
}

/** Stop recording; `send` decides whether the clip is uploaded or discarded.
 *  The actual blob assembly + send happens in `finalizeVoiceRecording` (onstop). */
function stopVoiceRecording(send: boolean): void {
  if (!voiceRecorder) return;
  voiceShouldSend = send;
  if (voiceTimer !== null) {
    clearInterval(voiceTimer);
    voiceTimer = null;
  }
  if (voiceRecorder.state !== 'inactive') voiceRecorder.stop();
}

function finalizeVoiceRecording(): void {
  const chunks = voiceChunks;
  const type = voiceRecorder?.mimeType || 'audio/webm';
  voiceRecorder = null;
  voiceChunks = [];
  if (voiceStream) {
    voiceStream.getTracks().forEach((tr) => tr.stop());
    voiceStream = null;
  }
  chatInputRow.classList.remove('recording');
  if (!voiceShouldSend || !chunks.length) return;
  void sendVoiceMessage(new Blob(chunks, { type }));
}

async function sendVoiceMessage(blob: Blob): Promise<void> {
  if (chatRecord.hidden || !session) return;
  // Name the file by container so the server derives the right audio MIME + ext.
  const ext = /mp4|m4a|aac/.test(blob.type) ? 'm4a' : /ogg/.test(blob.type) ? 'ogg' : 'webm';
  const file = new File([blob], `voice-message.${ext}`, { type: blob.type || `audio/${ext}` });
  if (file.size === 0 || file.size > UPLOAD_MAX_BYTES) {
    showNotif(t('uploadFailed'));
    return;
  }
  // Reuse the upload-progress UI; the transcribed + translated message arrives over WS.
  chatUpload.classList.remove('hidden');
  chatUploadFill.style.width = '0%';
  chatUploadLabel.textContent = t('uploading');
  const res = await uploadChatFile(session.room, myId, file, (frac) => {
    chatUploadFill.style.width = `${Math.round(frac * 100)}%`;
  });
  chatUpload.classList.add('hidden');
  if (!res.ok) {
    showNotif(t('uploadFailed'));
  } else if (res.translateBlocked) {
    showNotif(
      res.translateBlocked === 'signin'
        ? t('uploadNotTranslatedSignin')
        : t('uploadNotTranslatedCredits'),
    );
  }
}

chatRecord.addEventListener('click', () => void startVoiceRecording());
$('chat-rec-send').addEventListener('click', () => stopVoiceRecording(true));
$('chat-rec-cancel').addEventListener('click', () => stopVoiceRecording(false));

$('btn-leave').addEventListener('click', leaveCall);
function leaveCall(): void {
  // Meet-style cue: you left the call — only if we actually joined (callStartedAt
  // stays 0 on a room-full bounce), so it never fires for a non-entry (spec 0024).
  if (callStartedAt > 0) {
    playCallLeaveSound();
    // Track call ended with duration and participants
    const duration = Math.floor((Date.now() - callStartedAt) / 1000);
    const participantCount = videoGrid.querySelectorAll('.cell').length;
    track('call_ended', { 
      duration_seconds: duration,
      participants: participantCount
    });
  }
  // Snapshot transcript state before teardown wipes it (spec 0009); the
  // post-call download modal opens once we're back on the home screen.
  const ended =
    activeSessionId && callStartedAt > 0
      ? {
          id: activeSessionId,
          room: session?.room || '',
          events: transcriptEvents,
          durationMs: Date.now() - callStartedAt,
        }
      : null;
  // Stop + upload the cloud recording while activeSessionId is still set:
  // stopRecording captures it synchronously, so the null on the next line can't
  // strand the upload into a local download. Chunks are already collected, so the
  // async Blob assembly survives the teardown below.
  if (isRecording) void stopRecording();
  activeSessionId = null;
  transcriptEvents = 0;
  callStartedAt = 0;
  cartesiaManager?.deactivate(); // spec 0108: stop all Enhanced pipelines on leave
  peerVoiceIds.clear();
  clearPendingRemovals(); // drop any in-flight reconnect grace timers (#233)
  show($('transcript-indicator'), false);
  manualClose = true;
  setNetworkDegraded(false); // leaving on purpose — don't show "reconnecting"
  audioCapture?.stop();
  micMeter?.stop();
  micMeter = null;
  // (Recording stop was already initiated above, before activeSessionId was nulled.)
  mesh?.destroy();
  if (pipWindow && !pipWindow.closed) { pipWindow.close(); pipWindow = null; }
  pipCtl = null;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  if (isSharingScreen) stopScreenShare();
  if (screenPip) { screenPip.stop(); screenPip = null; }
  if (screenStream) { screenStream.getTracks().forEach((t) => t.stop()); screenStream = null; }
  if (ws) {
    ws.close(1000, 'leave');
    ws = null;
  }
  // Tear down the background-blur pipeline; its source camera lives outside
  // localStream when active, so stop it explicitly.
  if (vbg) { vbg.source?.stop(); vbg.stop(); vbg = null; }
  bgMode = 'none';
  if (localStream) {
    localStream.getTracks().forEach((tr) => tr.stop());
    localStream = null;
  }
  stopTts();
  pcmPlayback.stop(); // tear down the Premium translated-audio graph (spec 0093)
  premiumSpeakers.clear();
  handRaised = false;
  viewMode = 'grid';
  pinnedPeerId = null;
  lastSpeakerId = null;
  mesh = null;
  audioCapture = null;
  chat = null;
  if (voiceRecorder) stopVoiceRecording(false); // drop any in-progress voice message
  remoteStreams.clear();
  chatPanel.classList.remove('open');
  participantsPanel.classList.remove('open');
  participantsPanel.classList.add('closed');
  setBookmarkSession(null); // hides the 🔖 button + closes its panel
  setGlossaryRoom(null); // hides the 📖 badge + closes the editor
  toggleWhiteboard(false); // hide the whiteboard overlay + drop its strokes (spec 0045)
  whiteboard.reset();
  toggleMinigame(false); // hide the mini-game + drop local state (spec 0046)
  tictactoe.reset();
  toggleQuiz(false); // hide the quiz + drop local state (spec 0047)
  quiz.reset();
  toggleTimerPop(false); // close the manual timer popover (spec 0052)
  callTimer.reset(); // stop any running countdown + hide the badge
  stopSessionTimer(); // stop + hide the session-duration / participant-count chips (spec 0055)
  dismissLangToast(); // drop a pending "Detected language" toast (spec 0012)
  callScreen.classList.add('hidden');
  homeScreen.classList.remove('hidden');
  roomInput.value = randomRoom();
  startLobby();
  if (ended && billing && auth.isLoggedIn()) openPostCallModal(ended);
}

// ---- Helpers ---------------------------------------------------------------
function avatarGradient(name: string): string {
  let hash = 0;
  for (const ch of name) hash = ch.charCodeAt(0) + ((hash << 5) - hash);
  const hue = Math.abs(hash) % 360;
  return `linear-gradient(135deg, hsl(${hue},60%,25%), hsl(${(hue + 40) % 360},60%,15%))`;
}

/**
 * Fill a circular avatar element with the user's picture, falling back to a
 * gradient + initials when no URL exists or the image fails to load (spec 0070
 * R2.3). Mirrors the in-cell avatar so the participant badge + roster show real
 * faces instead of a single letter.
 */
function fillAvatar(
  el: HTMLElement,
  name: string,
  avatarSrc: string | null | undefined,
  sizePx: number,
  initialsLen = 2,
): void {
  const initials = name.slice(0, initialsLen).toUpperCase();
  el.textContent = '';
  el.style.background = avatarGradient(name);
  const av = auth.avatarUrl(avatarSrc, sizePx);
  if (!av) {
    el.textContent = initials;
    return;
  }
  const img = document.createElement('img');
  img.referrerPolicy = 'no-referrer';
  img.alt = '';
  img.src = av;
  // Keep the gradient + initials if a (Google) avatar 404s or is blocked.
  img.addEventListener('error', () => {
    img.remove();
    el.textContent = initials;
  });
  el.appendChild(img);
}

function cssEsc(s: string): string {
  return (window.CSS && CSS.escape ? CSS.escape(s) : s.replace(/["\\]/g, '\\$&'));
}

// Translated-voice TTS now lives behind the provider-based TTSManager ("Vox Voices"):
// the app still calls speak() / unlockTts() / stopTts() exactly as before, but the
// manager owns the queue (no-cut, drop-oldest past cap — spec 0040), picks Browser vs
// a high-quality local engine by capability/health, and silently falls back to Browser
// on any failure. The ORIGINAL SpeechSynthesis path (delay-first voice pick, rate 1.1,
// iOS unlock) is preserved verbatim inside BrowserSpeechProvider (tts/providers/browser.ts).
// A one-time, non-blocking notice if we ever fall back mid-session.
ttsManager.onFallback(() => toast(t('ttsFallbackNotice')));
// If a Vox Voices pack is already installed, register its provider (dynamic-imported,
// so the heavy engine only loads when actually used). No-op unless the feature is
// configured AND a pack is installed — Browser Voice remains the default otherwise.
void registerVoxIfInstalled();

/** Speak a translated line — manager routes it to the best available engine. */
function speak(text: string, lang: string): void {
  ttsManager.speak(text, lang);
}

/** Speak a system phrase (timer confirmations) — always Browser Voice, never Vox. */
function speakSystem(text: string, lang: string): void {
  ttsManager.speakSystem(text, lang);
}

/** Prime audio inside a user gesture (iOS/WebKit unlock), on the join tap / toggle. */
function unlockTts(): void {
  ttsManager.unlock();
}

/** Stop playback and drop the queue (TTS toggled off / leaving the call). */
function stopTts(): void {
  ttsManager.stop();
}

// Copy room code from the on-video meta cluster. Brief "Copied" feedback swaps the
// room badge's OWN text (spec 0061: #call-vis moved into the info popover).
callRoom.addEventListener('click', async () => {
  const code = callRoom.textContent?.trim() || '';
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code);
    callRoom.textContent = t('copied');
    setTimeout(() => (callRoom.textContent = code), 1200);
  } catch {
    /* ignore */
  }
});

// ============================================================================
// Auth + billing
// ============================================================================
// ---- Modal a11y: focus trap + Escape + focus restore (WCAG 2.1.2 / 2.4.3) --
const FOCUSABLE =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
let openOverlay: HTMLElement | null = null;
let overlayRestoreFocus: HTMLElement | null = null;

function overlayKeydown(e: KeyboardEvent): void {
  if (!openOverlay) return;
  if (e.key === 'Escape') {
    // The consent gate is a mandatory choice — not dismissable via Escape.
    if (openOverlay !== consentModal) {
      e.preventDefault();
      show(openOverlay, false);
    }
    return;
  }
  if (e.key !== 'Tab') return;
  const focusables = Array.from(openOverlay.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (f) => f.offsetParent !== null, // skip display:none descendants
  );
  if (focusables.length === 0) return;
  const first = focusables[0];
  const last = focusables[focusables.length - 1];
  const active = document.activeElement as HTMLElement | null;
  const inside = !!active && openOverlay.contains(active);
  if (e.shiftKey && (active === first || !inside)) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && (active === last || !inside)) {
    e.preventDefault();
    first.focus();
  }
}

function escHtml(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function show(el: HTMLElement, visible: boolean): void {
  el.classList.toggle('hidden', !visible);
  if (!el.classList.contains('modal-overlay')) return;
  // Modal overlays additionally trap focus and restore it on close.
  if (visible) {
    openOverlay = el;
    overlayRestoreFocus = document.activeElement as HTMLElement | null;
    document.addEventListener('keydown', overlayKeydown, true);
    el.querySelector<HTMLElement>(FOCUSABLE)?.focus();
  } else if (openOverlay === el) {
    openOverlay = null;
    document.removeEventListener('keydown', overlayKeydown, true);
    overlayRestoreFocus?.focus();
    overlayRestoreFocus = null;
  }
}

async function boot(): Promise<void> {
  // Record where the visitor came from (?source / utm_source) before anything
  // can tidy the URL — stamped on the account at first login for campaign KPIs.
  auth.captureAcquisitionSource();
  // Pull any DB-managed UI strings over the bundled defaults, then re-render
  // (fails safe — keeps the bundled strings if the API is down).
  if (await loadRemoteI18n(HTTP_BASE)) applyI18n();
  billing = await auth.billingEnabled();
  // Validate a stored token up front. isLoggedIn() only checks the token EXISTS,
  // not that it's still valid — so a stale/expired one would render authed-only UI
  // (the 🔖 bookmark button, public rooms) while the server rejects every authed
  // action "as a guest". refreshMe() clears it on a 401 (and keeps it on a mere
  // network error), so after this the client's auth state matches the server's.
  if (billing && auth.isLoggedIn()) await auth.refreshMe();
  if (billing && !auth.isLoggedIn()) {
    showLogin();
  } else {
    enterHome();
  }
  // Returned from a Stripe checkout → refresh balance + tidy the URL.
  if (billing && auth.isLoggedIn() && location.search.includes('checkout=success')) {
    // Track successful payment
    track('payment_completed');
    await auth.refreshMe();
    renderAccount();
    history.replaceState(null, '', location.pathname);
  }
}

function showLogin(): void {
  // Boot has decided — hand screen control back to .hidden (see the boot-login
  // pre-paint script in Base.astro). No-op past the first call.
  document.documentElement.classList.remove('boot-login');
  loginScreen.classList.remove('hidden');
  homeScreen.classList.add('hidden');
  setupGoogleSignIn();
}

function enterHome(): void {
  document.documentElement.classList.remove('boot-login');
  loginScreen.classList.add('hidden');
  homeScreen.classList.remove('hidden');
  if (billing && auth.isLoggedIn()) {
    const u = auth.getUser()!;
    if (u.name && !nameInput.value) nameInput.value = u.name;
    renderAccount();
    void auth.refreshMe().then(() => {
      renderAccount();
      ensureConsent();
    });
    ensureConsent();
    // Re-subscribe to push if already permitted (silent); scheduling card below.
    void maybeSubscribePush();
  } else if (billing) {
    // Guests have no account; gate them with the same blocking 18+/ToS modal.
    ensureConsent();
  }
  updatePublicGate();
  // Scheduled-meetings card: shows when signed in, hides for guests (idempotent).
  setupScheduling(t);
  setupGeoOptIn(); // optional location opt-in (signed-in, once)
  refreshGlossaryHome(); // 📖 home button is auth-only
  startLobby();
  // Invite deep-link (spec 0082): the FIRST time we reach home carrying an invite
  // code, go straight to the pre-join preview. Consumed once, so leaving a call back
  // to home — or a guest who had to sign in first — doesn't loop back into pre-join.
  // Private by default: a guest can join a private invited room, and the server's
  // canonical visibility (RoomJoined.public) corrects the label on join.
  if (pendingInviteRoom) {
    const room = pendingInviteRoom;
    pendingInviteRoom = null;
    void goPrejoin(room, false);
  } else {
    // First-visit home wizard — skipped while the blocking 18+/ToS consent gate is up (the
    // consent-accept handler re-runs this once it closes) or when home isn't the visible screen.
    autoStartHomeWizard();
  }
}

/// Everyone must accept age (18+) + ToS before using the app: logged-in users are
/// recorded server-side; guests self-attest client-side (no account to gate against).
function ensureConsent(): void {
  if (!billing) return;
  if (auth.isLoggedIn()) {
    if (!auth.consentGiven()) show(consentModal, true);
  } else if (!auth.guestConsentGiven()) {
    show(consentModal, true);
  }
}

/// Public rooms require an account when billing is on; disable the option for
/// guests and steer them to a private room.
function updatePublicGate(): void {
  const guest = billing && !auth.isLoggedIn();
  // The guest gets the sign-in bar (their only route back to login); a signed-in
  // user gets the account bar instead. `billing` off → neither (no accounts).
  guestBar.classList.toggle('hidden', !guest);
  const pubBtn = visGroup.querySelector('.seg-btn[data-vis="public"]') as HTMLButtonElement | null;
  if (!pubBtn) return;
  // Keep it clickable (a native `disabled` swallows clicks) but mark it locked, so
  // a guest tapping it gets the sign-in benefits modal instead of dead silence.
  pubBtn.disabled = false;
  pubBtn.classList.toggle('locked', !!guest);
  if (guest && visibilityPublic) {
    // Force private for guests.
    visibilityPublic = false;
    visGroup.querySelectorAll('.seg-btn').forEach((b) => {
      const isPrivate = (b as HTMLElement).dataset.vis === 'private';
      b.classList.toggle('active', isPrivate);
      b.setAttribute('aria-pressed', String(isPrivate));
    });
    updateVisHint();
  }
  visHint.textContent = guest ? t('publicNeedsLogin') : visibilityPublic ? '' : t('privateHint');
}

function renderAccount(): void {
  renderEngineSelector(); // keep the engine selector in sync with auth (guests: hidden + Standard)
  const u = auth.getUser();
  if (!billing || !u) {
    accountBar.classList.add('hidden');
    // Guest (billing on, no user) → offer the sign-in bar; guest-only mode → nothing.
    guestBar.classList.toggle('hidden', !billing);
    clearFriendState(); // no account → no friends cache / request badge
    return;
  }
  accountBar.classList.remove('hidden');
  guestBar.classList.add('hidden');
  void loadFriendState(false); // populate the request badge + cached ids for this user
  accountName.textContent = u.name;
  const av = auth.avatarUrl(u.avatar_url, 72);
  if (av) {
    accountAvatar.src = av;
    accountAvatar.style.display = '';
  } else {
    accountAvatar.style.display = 'none';
  }
  setBalanceUi(u.balance);
  void updateWorkspaceLink();
}

// --- VoxTranslate for Business hooks (spec 0106) ---

async function ensureBizOrgs(): Promise<BusinessOrg[]> {
  if (bizOrgs) return bizOrgs;
  bizOrgs = billing && auth.isLoggedIn() ? await listMyOrgs() : [];
  return bizOrgs;
}

// Kick off the first-visit home wizard, but warm the org cache first so the B2B-only
// webinar-explainer step (which reads `bizOrgs` synchronously via isB2B) is included for
// members of ≥1 org. `maybeAutoStartHome` is idempotent (marks HOME_FLAG on open), so the
// warmed call is a no-op once the wizard has already been shown.
function autoStartHomeWizard(): void {
  const canStart = () => openOverlay === null && !homeScreen.classList.contains('hidden');
  void ensureBizOrgs().then(() => onboarding.maybeAutoStartHome(canStart));
}

// Reveal the navbar "Workspace" link once we know the user belongs to ≥1 org, and
// the Account → Workspace tab (project voice notes) once an org has an active plan.
async function updateWorkspaceLink(): Promise<void> {
  const orgs = await ensureBizOrgs();
  if (orgs.length === 0) return;
  const btn = $<HTMLAnchorElement>('workspace-btn');
  btn.href = DASHBOARD_URL;
  show(btn, true);
  // Project voice notes are gated on an active subscription (same as cloud recording).
  if (orgs.some((o) => canCloudRecord(o))) show($('acct-tab-workspace'), true);
  // Hosting webinars is gated on an active subscription too (webinar phase 0).
  if (orgs.some((o) => canHostWebinar(o))) show($('webinars-btn'), true);
}

// ---- Workspace: project voice notes (spec: B2B project voice notes) --------
// Record a clip and attach it to a project (no call); it's transcribed +
// translated server-side and lands in the project's insights data. Its own
// recorder state (independent of the in-call chat recorder).
const wsOrgSel = $<HTMLSelectElement>('ws-org');
const wsProjectSel = $<HTMLSelectElement>('ws-project');
const wsRecordBtn = $<HTMLButtonElement>('ws-record');
const wsRecordingBar = $('ws-recording');
const wsRecTime = $('ws-rec-time');
const wsUpload = $('ws-upload');
const wsUploadFill = $('ws-upload-fill');
const wsUploadLabel = $('ws-upload-label');
const wsNotes = $('ws-notes');
$('ws-rec-send').innerHTML = icon('send', 20);
$('ws-rec-cancel').innerHTML = icon('trash', 18);

let wsOrgsLoaded = false;
let wsRecorder: MediaRecorder | null = null;
let wsChunks: BlobPart[] = [];
let wsStream: MediaStream | null = null;
let wsTimer: number | null = null;
let wsStartMs = 0;
let wsShouldSend = false;
let wsDurationS = 0;

/** Populate the org picker (active-sub orgs only) the first time the section opens. */
async function setupWorkspaceVoice(): Promise<void> {
  if (wsOrgsLoaded) return;
  const orgs = (await ensureBizOrgs()).filter((o) => canCloudRecord(o));
  wsOrgSel.innerHTML = '';
  for (const o of orgs) {
    const opt = document.createElement('option');
    opt.value = o.id;
    opt.textContent = o.name;
    wsOrgSel.appendChild(opt);
  }
  show($('ws-org-field'), orgs.length > 1); // single org → hide the picker
  wsOrgsLoaded = true;
  if (orgs.length) await loadWorkspaceProjects();
}

async function loadWorkspaceProjects(): Promise<void> {
  const orgId = wsOrgSel.value;
  wsProjectSel.innerHTML = '';
  wsRecordBtn.disabled = true;
  wsNotes.innerHTML = '';
  if (!orgId) return;
  const projects = await listProjects(orgId);
  if (!projects.length) {
    const opt = document.createElement('option');
    opt.value = '';
    opt.textContent = t('bizNoProject');
    wsProjectSel.appendChild(opt);
    return;
  }
  for (const p of projects) {
    const opt = document.createElement('option');
    opt.value = p.id;
    opt.textContent = p.name;
    wsProjectSel.appendChild(opt);
  }
  await loadWorkspaceNotes();
}

async function loadWorkspaceNotes(): Promise<void> {
  const orgId = wsOrgSel.value;
  const projectId = wsProjectSel.value;
  wsRecordBtn.disabled = !(orgId && projectId);
  if (!orgId || !projectId) return;
  renderWorkspaceNotes(orgId, projectId, await listProjectVoiceMessages(orgId, projectId));
}

function renderWorkspaceNotes(
  orgId: string,
  projectId: string,
  notes: ProjectVoiceMessage[],
): void {
  wsNotes.innerHTML = '';
  if (!notes.length) {
    const empty = document.createElement('p');
    empty.className = 'ws-note-empty';
    empty.textContent = t('wsNoNotes');
    wsNotes.appendChild(empty);
    return;
  }
  for (const n of notes) {
    const card = document.createElement('div');
    card.className = 'ws-note';
    const head = document.createElement('div');
    head.className = 'ws-note-head';
    const who = document.createElement('span');
    who.textContent = `${n.created_by_name} · ${n.source_language.toUpperCase()}${n.translated ? ' ✓' : ''}`;
    const when = document.createElement('span');
    when.textContent = new Date(n.created_at).toLocaleString();
    head.append(who, when);
    card.appendChild(head);
    // Lazy playback: fetch a signed URL only when the user asks to listen.
    const play = document.createElement('button');
    play.type = 'button';
    play.className = 'btn-ghost';
    play.textContent = `▶ ${recTimeLabel((n.duration_seconds ?? 0) * 1000)}`;
    play.addEventListener('click', async () => {
      play.disabled = true;
      const url = await voiceMessageAudioUrl(orgId, projectId, n.id);
      if (!url) {
        play.disabled = false;
        toast(t('uploadFailed'), 'err');
        return;
      }
      const audio = document.createElement('audio');
      audio.controls = true;
      audio.src = url;
      audio.autoplay = true;
      play.replaceWith(audio);
    });
    card.appendChild(play);
    wsNotes.appendChild(card);
  }
}

async function startWsRecording(): Promise<void> {
  if (wsRecorder || !wsOrgSel.value || !wsProjectSel.value) return;
  let stream: MediaStream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch {
    toast(t('camMicDenied'), 'err');
    return;
  }
  const mime = ['audio/webm;codecs=opus', 'audio/webm', 'audio/mp4'].find((ty) =>
    MediaRecorder.isTypeSupported?.(ty),
  );
  wsChunks = [];
  wsShouldSend = false;
  try {
    wsRecorder = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
  } catch {
    stream.getTracks().forEach((tr) => tr.stop());
    toast(t('uploadFailed'), 'err');
    return;
  }
  wsStream = stream;
  wsRecorder.ondataavailable = (e) => {
    if (e.data.size) wsChunks.push(e.data);
  };
  wsRecorder.onstop = finalizeWsRecording;
  wsRecorder.start();
  wsStartMs = Date.now();
  wsRecTime.textContent = '0:00';
  show(wsRecordBtn, false);
  show(wsRecordingBar, true);
  wsTimer = window.setInterval(() => {
    const elapsed = Date.now() - wsStartMs;
    wsRecTime.textContent = recTimeLabel(elapsed);
    if (elapsed >= VOICE_MAX_MS) stopWsRecording(true);
  }, 200);
}

function stopWsRecording(send: boolean): void {
  if (!wsRecorder) return;
  wsShouldSend = send;
  wsDurationS = Math.round((Date.now() - wsStartMs) / 1000);
  if (wsTimer !== null) {
    clearInterval(wsTimer);
    wsTimer = null;
  }
  if (wsRecorder.state !== 'inactive') wsRecorder.stop();
}

function finalizeWsRecording(): void {
  const chunks = wsChunks;
  const type = wsRecorder?.mimeType || 'audio/webm';
  wsRecorder = null;
  wsChunks = [];
  if (wsStream) {
    wsStream.getTracks().forEach((tr) => tr.stop());
    wsStream = null;
  }
  show(wsRecordingBar, false);
  show(wsRecordBtn, true);
  if (!wsShouldSend || !chunks.length) return;
  void sendWsVoice(new Blob(chunks, { type }), wsDurationS);
}

async function sendWsVoice(blob: Blob, durationS: number): Promise<void> {
  const orgId = wsOrgSel.value;
  const projectId = wsProjectSel.value;
  if (!orgId || !projectId) return;
  const ext = /mp4|m4a|aac/.test(blob.type) ? 'm4a' : /ogg/.test(blob.type) ? 'ogg' : 'webm';
  const file = new File([blob], `voice-message.${ext}`, { type: blob.type || `audio/${ext}` });
  if (file.size === 0 || file.size > UPLOAD_MAX_BYTES) {
    toast(t('uploadFailed'), 'err');
    return;
  }
  wsRecordBtn.disabled = true;
  show(wsUpload, true);
  wsUploadFill.style.width = '0%';
  wsUploadLabel.textContent = t('uploading');
  const res = await uploadProjectVoiceMessage(orgId, projectId, file, durationS, (frac) => {
    wsUploadFill.style.width = `${Math.round(frac * 100)}%`;
  });
  show(wsUpload, false);
  wsRecordBtn.disabled = false;
  if (!res.ok) {
    toast(t('uploadFailed'), 'err');
    return;
  }
  toast(res.translateBlocked === 'credits' ? t('uploadNotTranslatedCredits') : t('wsVoiceSent'), res.translateBlocked === 'credits' ? 'err' : 'ok');
  await loadWorkspaceNotes(); // refresh the project's history
}

wsOrgSel.addEventListener('change', () => void loadWorkspaceProjects());
wsProjectSel.addEventListener('change', () => void loadWorkspaceNotes());
wsRecordBtn.addEventListener('click', () => void startWsRecording());
$('ws-rec-send').addEventListener('click', () => stopWsRecording(true));
$('ws-rec-cancel').addEventListener('click', () => stopWsRecording(false));

// ---- Webinars: B2B host hub (webinar phase 0) ------------------------------
// A signed-in host with an active-subscription org creates and manages webinars.
// Each webinar shows a copyable public join link + a QR encoding that link. Screen
// switch mirrors the Account hub (homeScreen.hidden ↔ webinarsScreen.hidden).
const webinarsScreen = $('webinars');
const webinarOrgSel = $<HTMLSelectElement>('webinar-org');
const webinarProjectSel = $<HTMLSelectElement>('webinar-project');
const webinarLangSel = $<HTMLSelectElement>('webinar-lang');
const webinarTierSel = $<HTMLSelectElement>('webinar-tier');
const webinarTitleInput = $<HTMLInputElement>('webinar-title');
const webinarStartInput = $<HTMLInputElement>('webinar-start');
const webinarEndInput = $<HTMLInputElement>('webinar-end');
const webinarForm = $<HTMLFormElement>('webinar-create');
const webinarCreateBtn = $<HTMLButtonElement>('webinar-create-btn');
const webinarCreateToggle = $<HTMLButtonElement>('webinar-create-toggle');
const webinarCreateCancel = $<HTMLButtonElement>('webinar-create-cancel');
const webinarCreateStatus = $('webinar-create-status');
const webinarList = $('webinar-list');
const webinarListEmpty = $('webinar-list-empty');
// Active | Archived segmented control above the list (buttons carry data-archived).
const webinarTabs = $('webinar-tabs');
// Create-form toggle switches (role=switch buttons; state lives in aria-checked).
const webinarRecordVideoSw = $<HTMLButtonElement>('webinar-record-video');
const webinarRecordTranscriptSw = $<HTMLButtonElement>('webinar-record-transcript');
const webinarChatEnabledSw = $<HTMLButtonElement>('webinar-chat-enabled');
// Visibility toggle: OFF (default) = private/link-only; ON = public (listed on home).
const webinarVisibilitySw = $<HTMLButtonElement>('webinar-visibility-public');
const webinarVoiceCloneSw = $<HTMLButtonElement>('webinar-voice-clone');
const webinarVoiceCloneRow = $('webinar-voice-clone-row');
const webinarVoiceClonedHint = $('webinar-voice-cloned-hint');
// Fullscreen QR zoom overlay.
const webinarQrModal = $('webinar-qr-modal');
const webinarQrModalImg = $<HTMLImageElement>('webinar-qr-modal-img');
const webinarQrModalTitle = $('webinar-qr-modal-title');
const webinarQrModalUrl = $('webinar-qr-modal-url');
$('webinars-back').innerHTML = icon('chevron-left', 18);

let webinarOrgsLoaded = false;
// Which list the Webinars screen is showing: false = active, true = archived.
let webinarShowArchived = false;

/** Read a role=switch button's on/off state from its `aria-checked` attribute. */
function switchOn(sw: HTMLButtonElement): boolean {
  return sw.getAttribute('aria-checked') === 'true';
}

/** Set a role=switch button's on/off state. */
function setSwitch(sw: HTMLButtonElement, on: boolean): void {
  sw.setAttribute('aria-checked', String(on));
}

/** Wire a role=switch button: click + Space/Enter toggle its `aria-checked`, then
 *  run the optional callback with the new state. Native buttons already fire click
 *  on Space/Enter, so a click listener covers keyboard operation. */
function wireSwitch(sw: HTMLButtonElement, onChange?: (on: boolean) => void): void {
  sw.addEventListener('click', () => {
    const next = !switchOn(sw);
    setSwitch(sw, next);
    onChange?.(next);
  });
}
wireSwitch(webinarRecordVideoSw);
wireSwitch(webinarRecordTranscriptSw);
wireSwitch(webinarChatEnabledSw);
wireSwitch(webinarVisibilitySw);
wireSwitch(webinarVoiceCloneSw);

/** Show the voice-clone toggle only for Enhanced when the host hasn't cloned yet;
 *  otherwise show the "already cloned" hint (cloned) or nothing (Standard). Called on
 *  entry and whenever the tier changes. */
function syncWebinarVoiceClone(): void {
  const enhanced = webinarTierSel.value === 'enhanced';
  const cloned = hasVoiceClone();
  const showToggle = showVoiceCloneToggle(webinarTierSel.value, cloned);
  show(webinarVoiceCloneRow, showToggle);
  // "Voice already cloned ✓" hint: only meaningful for the tier that uses it.
  show(webinarVoiceClonedHint, enhanced && cloned);
  if (!showToggle) setSwitch(webinarVoiceCloneSw, false); // never send voice_clone otherwise
}
webinarTierSel.addEventListener('change', syncWebinarVoiceClone);

/** Map a WebinarError HTTP status to a localized message. */
function webinarErrorMessage(err: unknown): string {
  const status = err instanceof WebinarError ? err.status : 0;
  switch (status) {
    case 401:
      return t('webinarErrAuth');
    case 402:
      return t('webinarErrInactive');
    case 404:
      return t('webinarErrOrg');
    case 409:
      return t('webinarErrLocked');
    case 400:
      return t('webinarErrInput');
    default:
      return t('webinarErrGeneric');
  }
}

function setWebinarStatus(msg: string, kind: 'ok' | 'err' | ''): void {
  webinarCreateStatus.textContent = msg;
  webinarCreateStatus.classList.toggle('ok', kind === 'ok');
  webinarCreateStatus.classList.toggle('err', kind === 'err');
  show(webinarCreateStatus, !!msg);
}

/** Populate the source-language select once (every union language, endonym-labelled). */
function fillWebinarLangs(): void {
  if (webinarLangSel.options.length) return;
  for (const l of LANGUAGES) {
    const opt = document.createElement('option');
    opt.value = l.code;
    opt.textContent = `${l.native} (${l.english})`;
    webinarLangSel.appendChild(opt);
  }
  // Default to the host's UI language when it's in the union, else English.
  const ui = getUiLang();
  webinarLangSel.value = LANGUAGES.some((l) => l.code === ui) ? ui : 'en';
}

/** Open the Webinars screen: load the host's active-sub orgs on first entry, then
 *  render that org's webinars. */
/** Collapse the create form behind the "Create a webinar" button — the default state
 *  on entry and after a successful create, so the screen leads with the list. */
function collapseWebinarForm(): void {
  show(webinarForm, false);
  show(webinarCreateToggle, true);
}

/** Reveal the create form and hide the toggle button (focus the first field). */
function revealWebinarForm(): void {
  show(webinarCreateToggle, false);
  show(webinarForm, true);
  webinarTitleInput.focus();
}

async function openWebinars(): Promise<void> {
  homeScreen.classList.add('hidden');
  show(recapScreen, false);
  webinarsScreen.classList.remove('hidden');
  collapseWebinarForm(); // always re-enter with the form collapsed
  resetWebinarTabs(); // always re-enter on the Active list
  fillWebinarLangs();
  syncWebinarVoiceClone(); // tier-aware toggle/hint (voice clone is Enhanced-only)
  if (!webinarOrgsLoaded) {
    const orgs = (await ensureBizOrgs()).filter((o) => canHostWebinar(o));
    webinarOrgSel.innerHTML = '';
    for (const o of orgs) {
      const opt = document.createElement('option');
      opt.value = o.id;
      opt.textContent = o.name;
      webinarOrgSel.appendChild(opt);
    }
    show($('webinar-org-field'), orgs.length > 1); // single org → hide the picker
    webinarCreateBtn.disabled = orgs.length === 0;
    webinarCreateToggle.disabled = orgs.length === 0; // no host-capable org → can't create
    webinarOrgsLoaded = true;
  }
  await loadWebinarProjects();
  await loadWebinars();
}

/** Populate the optional "Project" picker from the selected org's projects. Always
 *  starts with a "No project" default; the field is hidden when the org has none.
 *  Called on entry and whenever the chosen org changes. */
async function loadWebinarProjects(): Promise<void> {
  const orgId = webinarOrgSel.value;
  webinarProjectSel.innerHTML = '';
  const projectField = $('webinar-project-field');
  if (!orgId) {
    show(projectField, false);
    return;
  }
  // "No project" default — value "" means the create call omits project_id.
  const none = document.createElement('option');
  none.value = '';
  none.textContent = t('webinarNoProject');
  webinarProjectSel.appendChild(none);
  const projects = await listProjects(orgId);
  for (const p of projects) {
    const opt = document.createElement('option');
    opt.value = p.id;
    opt.textContent = p.name;
    webinarProjectSel.appendChild(opt);
  }
  show(projectField, projects.length > 0); // hide the picker when the org has none
}

function closeWebinars(): void {
  webinarsScreen.classList.add('hidden');
  homeScreen.classList.remove('hidden');
}

/** Fetch + render the selected org's webinars (newest server order preserved). Honors
 *  the Active | Archived toggle: archived cards are historical (title, code, created
 *  date, Restore); active cards get the full go-live / QR / archive actions. */
async function loadWebinars(): Promise<void> {
  const orgId = webinarOrgSel.value;
  webinarList.innerHTML = '';
  show(webinarListEmpty, false);
  if (!orgId) return;
  let webinars: WebinarView[];
  try {
    webinars = await listWebinars(orgId, webinarShowArchived);
  } catch (err) {
    toast(webinarErrorMessage(err), 'err');
    return;
  }
  if (!webinars.length) {
    webinarListEmpty.textContent = t(
      webinarShowArchived ? 'webinarNoneArchived' : 'webinarNone',
    );
    show(webinarListEmpty, true);
    return;
  }
  for (const w of webinars) renderWebinarCard(w);
}

/** Reset the Active | Archived toggle back to Active (screen state + button styling).
 *  Called on every entry to the Webinars screen so it never opens on the archived list. */
function resetWebinarTabs(): void {
  webinarShowArchived = false;
  webinarTabs.querySelectorAll('.seg-btn').forEach((b) => {
    const on = (b as HTMLElement).dataset.archived === 'false';
    b.classList.toggle('active', on);
    b.setAttribute('aria-selected', String(on));
  });
}

/** Switch the Webinars screen between the active and archived lists, reflect the
 *  choice on the segmented control, persist it in screen state, and reload. */
async function selectWebinarTab(archived: boolean): Promise<void> {
  if (webinarShowArchived === archived) return;
  webinarShowArchived = archived;
  webinarTabs.querySelectorAll('.seg-btn').forEach((b) => {
    const on = (b as HTMLElement).dataset.archived === String(archived);
    b.classList.toggle('active', on);
    b.setAttribute('aria-selected', String(on));
  });
  await loadWebinars();
}

/** One webinar card: title/status, the copyable join link, a QR of that link, and
 *  a Cancel/Archive action while it's active. Archived webinars render a slimmed-down
 *  historical card (title, code, created date, Restore) via `renderArchivedWebinarCard`. */
function renderWebinarCard(w: WebinarView): void {
  if (w.archived_at) {
    renderArchivedWebinarCard(w);
    return;
  }

  const card = document.createElement('div');
  card.className = 'webinar-card';
  card.dataset.webinarId = w.id;

  const head = document.createElement('div');
  head.className = 'webinar-card-head';
  const title = document.createElement('span');
  title.className = 'webinar-card-title';
  title.textContent = w.title;
  const status = document.createElement('span');
  status.className = `webinar-status webinar-status-${w.status}`;
  status.textContent = t(`webinarStatus_${w.status}`) || w.status;
  head.append(title, status);
  card.appendChild(head);

  const meta = document.createElement('span');
  meta.className = 'webinar-card-meta';
  meta.textContent = `${(langMeta(w.source_language)?.native ?? w.source_language)} · ${t(`webinarTier_${w.tier}`) || w.tier}`;
  card.appendChild(meta);

  // Copyable join link. The button briefly swaps to "Copied" (room-code pattern).
  const linkRow = document.createElement('div');
  linkRow.className = 'webinar-link-row';
  const linkInput = document.createElement('input');
  linkInput.className = 'webinar-link-input';
  linkInput.readOnly = true;
  linkInput.value = w.join_url;
  linkInput.setAttribute('aria-label', t('webinarJoinLink'));
  const copyBtn = document.createElement('button');
  copyBtn.type = 'button';
  copyBtn.className = 'btn-ghost webinar-copy-btn';
  copyBtn.textContent = t('copy');
  copyBtn.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(w.join_url);
      copyBtn.textContent = t('copied');
      setTimeout(() => (copyBtn.textContent = t('copy')), 1200);
    } catch {
      linkInput.select(); // fallback: select for a manual copy
      toast(t('copyFailed'), 'err');
    }
  });
  linkRow.append(linkInput, copyBtn);
  card.appendChild(linkRow);

  // QR of the EXACT join_url from the API (never rebuilt). Lazy-imported chunk.
  // Tappable (mouse + keyboard) → fullscreen zoom overlay.
  const qr = document.createElement('img');
  qr.className = 'webinar-qr';
  qr.alt = t('webinarQrAlt');
  qr.width = 160;
  qr.height = 160;
  qr.tabIndex = 0;
  qr.setAttribute('role', 'button');
  qr.setAttribute('aria-label', t('webinarQrZoom'));
  qr.title = t('webinarQrZoom');
  qr.addEventListener('click', () => openQrModal(w, qr.src));
  qr.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      openQrModal(w, qr.src);
    }
  });
  card.appendChild(qr);
  void renderQr(qr, w.join_url);

  // QR actions: download the QR as a PNG, or print just the QR + join URL.
  const qrActions = document.createElement('div');
  qrActions.className = 'webinar-qr-actions';
  const dlBtn = document.createElement('button');
  dlBtn.type = 'button';
  dlBtn.className = 'btn-ghost';
  dlBtn.textContent = t('webinarQrDownload');
  dlBtn.addEventListener('click', async () => {
    dlBtn.disabled = true;
    try {
      downloadQr(await qrDataUrl(w.join_url, qr.src), w.code);
    } finally {
      dlBtn.disabled = false;
    }
  });
  const printBtn = document.createElement('button');
  printBtn.type = 'button';
  printBtn.className = 'btn-ghost';
  printBtn.textContent = t('webinarQrPrint');
  printBtn.addEventListener('click', async () => {
    printBtn.disabled = true;
    try {
      printQr(await qrDataUrl(w.join_url, qr.src), w.title, w.join_url);
    } finally {
      printBtn.disabled = false;
    }
  });
  qrActions.append(dlBtn, printBtn);
  card.appendChild(qrActions);

  // Pre-live "Clone your voice" (webinar-ui-fixes #5): only for Enhanced webinars whose
  // host hasn't cloned yet, and only while it can still be broadcast. Reuses the exact
  // capture + clone flow from the call pre-join.
  if (
    (w.status === 'scheduled' || w.status === 'live') &&
    showWebinarCloneAction(w.tier, hasVoiceClone())
  ) {
    card.appendChild(buildWebinarCloneRow());
  }

  // Go-live / publish control (webinar phase 1): capture mic (+ optional cam) and
  // publish to the media server over WHIP. Offered while the webinar can still be
  // broadcast (scheduled → live). `ended` webinars can't be re-broadcast. The
  // control now opens a Meet-style pre-live step (device pickers) before publishing —
  // device selection lives there, so the old standalone "activate webcam" button is gone.
  if (w.status === 'scheduled' || w.status === 'live') {
    card.appendChild(buildGoLiveControl(w));
  }

  // "Add to Google Calendar" — only for a scheduled webinar that has a start time.
  // On 409 (calendar not connected) it routes the host into the connect-calendar flow
  // and retries; on 400 (unscheduled) it explains a start time is required.
  if (w.status === 'scheduled' && w.scheduled_start) {
    card.appendChild(buildAddCalendarRow(w));
  }

  // Cancel — only while the webinar is still scheduled.
  if (w.status === 'scheduled') {
    const cancelBtn = document.createElement('button');
    cancelBtn.type = 'button';
    cancelBtn.className = 'btn-ghost webinar-cancel-btn danger';
    cancelBtn.textContent = t('webinarCancel');
    cancelBtn.addEventListener('click', async () => {
      if (!confirm(t('webinarCancelConfirm'))) return;
      cancelBtn.disabled = true;
      try {
        await cancelWebinar(w.id);
        toast(t('webinarCancelled'), 'ok');
        await loadWebinars();
      } catch (err) {
        cancelBtn.disabled = false;
        toast(webinarErrorMessage(err), 'err');
      }
    });
    card.appendChild(cancelBtn);
  }

  // Archive — move any active webinar into the historical (Archived) list. Available
  // regardless of status; a soft-archive that can be undone from the Archived tab.
  const archiveBtn = document.createElement('button');
  archiveBtn.type = 'button';
  archiveBtn.className = 'btn-ghost webinar-archive-btn';
  archiveBtn.textContent = t('webinarArchive');
  archiveBtn.addEventListener('click', async () => {
    archiveBtn.disabled = true;
    try {
      await archiveWebinar(w.id);
      toast(t('webinarArchived'), 'ok');
      await loadWebinars();
    } catch (err) {
      archiveBtn.disabled = false;
      toast(webinarErrorMessage(err), 'err');
    }
  });
  card.appendChild(archiveBtn);

  webinarList.appendChild(card);
}

/** A slimmed-down historical card for an archived webinar: title, short code, created
 *  date, and a Restore action. No go-live / QR / calendar actions — archived webinars
 *  are read-only history until restored back into the active list. */
function renderArchivedWebinarCard(w: WebinarView): void {
  const card = document.createElement('div');
  card.className = 'webinar-card webinar-card-archived';
  card.dataset.webinarId = w.id;

  const head = document.createElement('div');
  head.className = 'webinar-card-head';
  const title = document.createElement('span');
  title.className = 'webinar-card-title';
  title.textContent = w.title;
  head.appendChild(title);
  card.appendChild(head);

  const meta = document.createElement('span');
  meta.className = 'webinar-card-meta';
  meta.textContent = `${t('webinarCode')}: ${w.code} · ${t('webinarCreatedOn')} ${new Date(w.created_at).toLocaleDateString()}`;
  card.appendChild(meta);

  const restoreBtn = document.createElement('button');
  restoreBtn.type = 'button';
  restoreBtn.className = 'btn-ghost webinar-restore-btn';
  restoreBtn.textContent = t('webinarRestore');
  restoreBtn.addEventListener('click', async () => {
    restoreBtn.disabled = true;
    try {
      await unarchiveWebinar(w.id);
      toast(t('webinarRestored'), 'ok');
      await loadWebinars();
    } catch (err) {
      restoreBtn.disabled = false;
      toast(webinarErrorMessage(err), 'err');
    }
  });
  card.appendChild(restoreBtn);

  webinarList.appendChild(card);
}

// Only one webinar can be broadcast from a device at a time. `activePublisher` holds
// the live WhipPublisher; `activePublisherId` is the webinar it's publishing.
let activePublisher: WhipPublisher | null = null;
let activePublisherId: string | null = null;
// The host's live STT bridge (webinar Fase 2): streams the SAME mic track to the API's
// Deepgram ingest so the server can transcribe + translate and fan subtitle frames out to
// every viewer over the presence WS. Runs for the life of a broadcast; self-reconnects on
// a silent socket drop so subtitles don't stop while the video keeps flowing.
let activeWebinarStt: WebinarSttClient | null = null;

/**
 * Build the per-webinar "Go live" control. Clicking it no longer publishes immediately —
 * it opens a Meet-style pre-live step (camera preview + device pickers). If THIS webinar
 * is already live, the button re-opens the studio view instead.
 */
function buildGoLiveControl(w: WebinarView): HTMLElement {
  const wrap = document.createElement('div');
  wrap.className = 'webinar-live-row';

  const goBtn = document.createElement('button');
  goBtn.type = 'button';
  goBtn.className = 'btn-primary webinar-golive-btn';

  const live = activePublisher != null && activePublisherId === w.id;
  goBtn.textContent = live ? t('webinarBackToStudio') : t('webinarGoLive');

  goBtn.addEventListener('click', () => {
    if (activePublisher && activePublisherId === w.id) {
      openWebinarStudio(w); // resume the running broadcast's studio
      return;
    }
    if (activePublisher) {
      // A different webinar is already broadcasting from this device.
      toast(t('webinarAlreadyLive'), 'err');
      return;
    }
    void openWebinarPrelive(w);
  });

  wrap.append(goBtn);
  return wrap;
}

// ---- Webinar pre-live (Meet-style): camera preview + device pickers -----------
// Mirrors the call pre-join. The chosen mic/camera device ids + toggle state flow into
// the WhipPublisher when the host taps "Go on air".
const wpScreen = $('webinar-prelive');
const wpPreviewVideo = $<HTMLVideoElement>('webinar-preview');
const wpPreviewOff = $('webinar-preview-off');
const wpPreviewAvatar = $('webinar-preview-avatar');
const wpCamSelect = $<HTMLSelectElement>('webinar-cam-select');
const wpMicSelect = $<HTMLSelectElement>('webinar-mic-select');
const wpPreMic = $<HTMLButtonElement>('webinar-pre-mic');
const wpPreCam = $<HTMLButtonElement>('webinar-pre-cam');
const wpName = $('webinar-prelive-name');
const wpStatus = $('webinar-prelive-status');
const wpGoBtn = $<HTMLButtonElement>('webinar-prelive-go');
const wpCancelBtn = $<HTMLButtonElement>('webinar-prelive-cancel');

let wpStream: MediaStream | null = null;
let wpMicOn = true;
let wpCamOn = true;
let wpWebinar: WebinarView | null = null;

/** Video constraints for the pre-live preview honouring the selected camera device. */
function wpVideoConstraints(): MediaTrackConstraints {
  const camId = wpCamSelect.value;
  return {
    width: { ideal: 1280, max: 1280 },
    height: { ideal: 720, max: 720 },
    frameRate: { ideal: 24, max: 30 },
    ...(camId ? { deviceId: { exact: camId } } : {}),
  };
}

/** (Re)acquire the preview stream for the selected mic/camera devices. */
async function wpAcquireMedia(): Promise<void> {
  const micId = wpMicSelect.value;
  const audio: MediaTrackConstraints = {
    echoCancellation: true,
    noiseSuppression: true,
    autoGainControl: true,
    ...(micId ? { deviceId: { exact: micId } } : {}),
  };
  if (wpStream) wpStream.getTracks().forEach((tr) => tr.stop());
  try {
    wpStream = await navigator.mediaDevices.getUserMedia({ audio, video: wpVideoConstraints() });
  } catch (e) {
    if (e instanceof Error && e.name === 'NotAllowedError') track('camera_permission_denied');
    // Fall back to audio-only (no camera / video denied) — mic is required to publish.
    wpStream = await navigator.mediaDevices.getUserMedia({ audio });
  }
  wpPreviewVideo.srcObject = wpStream;
  void wpPreviewVideo.play().catch(() => {});
  wpApplyToggles();
}

/** Apply the mic/camera toggle state to the preview stream + control buttons. */
function wpApplyToggles(): void {
  if (wpStream) {
    wpStream.getAudioTracks().forEach((tr) => (tr.enabled = wpMicOn));
    if (!wpCamOn) wpStream.getVideoTracks().forEach((tr) => tr.stop());
  }
  const hasLiveVideo =
    !!wpStream && wpStream.getVideoTracks().some((tr) => tr.readyState === 'live');
  if (wpCamOn && !hasLiveVideo) wpCamOn = false;
  wpPreviewOff.hidden = wpCamOn && hasLiveVideo;
  if (!wpPreviewOff.hidden) {
    const name = wpWebinar?.title || t('webinarsTitle');
    const avatar =
      billing && auth.isLoggedIn() ? auth.avatarUrl(auth.getUser()?.avatar_url, 192) : null;
    if (avatar) {
      wpPreviewAvatar.textContent = '';
      wpPreviewAvatar.style.background = 'none';
      const img = document.createElement('img');
      img.className = 'preview-avatar-img';
      img.referrerPolicy = 'no-referrer';
      img.alt = '';
      img.src = avatar;
      img.addEventListener('error', () => {
        img.remove();
        wpPreviewAvatar.textContent = name.slice(0, 2).toUpperCase();
        wpPreviewAvatar.style.background = avatarGradient(name);
      });
      wpPreviewAvatar.appendChild(img);
    } else {
      wpPreviewAvatar.textContent = name.slice(0, 2).toUpperCase();
      wpPreviewAvatar.style.background = avatarGradient(name);
    }
  }
  wpPreMic.classList.toggle('active-danger', !wpMicOn);
  wpPreMic.innerHTML = icon(wpMicOn ? 'mic' : 'mic-off');
  wpPreCam.classList.toggle('active-danger', !wpCamOn);
  wpPreCam.innerHTML = icon(wpCamOn ? 'video' : 'video-off');
}

async function wpTogglePreCam(): Promise<void> {
  wpCamOn = !wpCamOn;
  const hasLiveVideo =
    !!wpStream && wpStream.getVideoTracks().some((tr) => tr.readyState === 'live');
  if (wpCamOn && wpStream && !hasLiveVideo) {
    try {
      const s = await navigator.mediaDevices.getUserMedia({ video: wpVideoConstraints() });
      const track2 = s.getVideoTracks()[0] ?? null;
      if (track2) {
        wpStream.getVideoTracks().forEach((tr) => {
          tr.stop();
          wpStream!.removeTrack(tr);
        });
        wpStream.addTrack(track2);
        wpPreviewVideo.srcObject = wpStream;
        void wpPreviewVideo.play().catch(() => {});
      }
    } catch {
      wpCamOn = false; // camera unavailable — stay off
    }
  }
  wpApplyToggles();
}

/** Populate the pre-live camera + mic device selectors from the current permissions. */
async function wpPopulateDevices(): Promise<void> {
  const devices = await navigator.mediaDevices.enumerateDevices();
  const cams = devices.filter((d) => d.kind === 'videoinput');
  const mics = devices.filter((d) => d.kind === 'audioinput');
  const curCam = wpStream?.getVideoTracks()[0]?.getSettings().deviceId || '';
  const curMic = wpStream?.getAudioTracks()[0]?.getSettings().deviceId || '';
  fillDeviceSelect(wpCamSelect, cams, curCam, 'Camera');
  fillDeviceSelect(wpMicSelect, mics, curMic, 'Mic');
}

/** Release the pre-live preview stream (on cancel or once handed to the publisher). */
function wpTeardown(): void {
  if (wpStream) wpStream.getTracks().forEach((tr) => tr.stop());
  wpStream = null;
  wpPreviewVideo.srcObject = null;
}

/** Open the Meet-style pre-live step for a webinar: preview + device pickers, then a
 *  "Go on air" button that publishes with the chosen device ids. */
async function openWebinarPrelive(w: WebinarView): Promise<void> {
  wpWebinar = w;
  wpMicOn = true;
  wpCamOn = true;
  wpStatus.textContent = '';
  wpStatus.classList.remove('error');
  wpName.textContent = w.title;
  wpGoBtn.disabled = false;
  show(webinarsScreen, false); // leave the list; do NOT re-show #home under the pre-live
  show(wpScreen, true);
  try {
    await wpAcquireMedia();
    await wpPopulateDevices();
  } catch {
    wpStatus.textContent = t('webinarPreliveDenied');
    wpStatus.classList.add('error');
    wpGoBtn.disabled = true;
  }
}

/** Close the pre-live step, releasing media, and return to the Webinars list. */
function closeWebinarPrelive(): void {
  wpTeardown();
  show(wpScreen, false);
  void openWebinars();
}

wpPreMic.addEventListener('click', () => {
  wpMicOn = !wpMicOn;
  wpApplyToggles();
});
wpPreCam.addEventListener('click', () => void wpTogglePreCam());
wpCamSelect.addEventListener('change', () => void wpAcquireMedia());
wpMicSelect.addEventListener('change', () => void wpAcquireMedia());
wpCancelBtn.addEventListener('click', closeWebinarPrelive);

wpGoBtn.addEventListener('click', () => {
  if (!wpWebinar) return;
  const w = wpWebinar;
  // Capture the chosen device ids + toggle state before tearing the preview down.
  const audioDeviceId = wpMicSelect.value || undefined;
  const videoDeviceId = wpCamSelect.value || undefined;
  const withCamera = wpCamOn;
  const withMic = wpMicOn; // carry the pre-live mute state into the broadcast
  wpTeardown(); // release the preview device; the publisher re-acquires with the same ids
  show(wpScreen, false);
  void startWebinarBroadcast(w, { audioDeviceId, videoDeviceId, withCamera, withMic });
});

// ---- Webinar studio (Meet-style host screen while broadcasting) --------------
const wsScreen = $('webinar-studio');
const wsVideo = $<HTMLVideoElement>('webinar-studio-video');
const wsVideoOff = $('webinar-studio-video-off');
const wsAvatar = $('webinar-studio-avatar');
const wsTitle = $('webinar-studio-title');
const wsCode = $('webinar-studio-code');
const wsMicBtn = $<HTMLButtonElement>('webinar-studio-mic');
const wsCamBtn = $<HTMLButtonElement>('webinar-studio-cam');
const wsEndBtn = $<HTMLButtonElement>('webinar-studio-end');
const wsOnairText = $('webinar-onair-text');
const wsCountN = $('webinar-count-n');
const wsEndModal = $('webinar-end-modal');
const wsEndConfirm = $<HTMLButtonElement>('webinar-end-confirm');
const wsEndCancel = $<HTMLButtonElement>('webinar-end-cancel');
// Host chat panel (Feature ⑤): the same auto-translated chat the viewers see. The host
// sends WITH their auth token (→ sender_kind:"host"); the panel is revealed only when the
// active webinar has chat enabled.
const wsChat = $('webinar-studio-chat');
const wsChatToggle = $<HTMLButtonElement>('webinar-studio-chat-toggle');
const wsChatList = $('webinar-studio-chat-list');
const wsChatInput = $<HTMLInputElement>('webinar-studio-chat-input');
const wsChatSend = $<HTMLButtonElement>('webinar-studio-chat-send');
const wsChatNotice = $('webinar-studio-chat-notice');
const wsChatForm = $<HTMLFormElement>('webinar-studio-chat-form');

// ---- Webinar recap screen (shown after broadcast ends when transcripts exist) ----
const recapScreen = $('webinar-recap');
const recapCloseBtn = $<HTMLButtonElement>('recap-close');
const recapTabTranscript = $<HTMLButtonElement>('recap-tab-transcript');
const recapTabChat = $<HTMLButtonElement>('recap-tab-chat');
const recapTranscriptPanel = $('recap-transcript-panel');
const recapChatPanel = $('recap-chat-panel');
const recapTranscriptList = $('recap-transcript-list');
const recapTranscriptEmpty = $('recap-transcript-empty');
const recapChatList = $('recap-chat-list');
const recapChatEmpty = $('recap-chat-empty');

/** The host studio's live-presence connection (opened while the studio is on screen,
 *  closed when it leaves). The host watches the audience count but is NOT counted. */
let webinarPresence: PresenceClient | null = null;
/** The host studio's chat controller, live while a chat-enabled webinar's studio is open. */
let webinarChat: ChatPanel | null = null;

/** Render the live audience count into its own element. */
function renderWebinarCount(count: number): void {
  wsCountN.textContent = String(count);
}

/** Localized strings the host chat panel needs. */
function webinarChatStrings() {
  return {
    send: t('wvChatSend'),
    hostTag: t('wvChatHost'),
    empty: t('wvChatEmpty'),
    rateLimited: t('wvChatRateLimited'),
    blocked: t('wvChatBlocked'),
    genericError: t('wvChatBlocked'),
  };
}

/** Set up the host chat panel for a chat-enabled webinar: reveal it, mount a ChatPanel
 *  (host token → sender_kind:"host"), load history, and return the `onChat` handler to feed
 *  live WS messages in. Returns null when the webinar has chat disabled (panel stays hidden). */
function openWebinarChat(w: WebinarView): ((event: ChatEvent) => void) | null {
  webinarChat = null;
  if (!w.chat_enabled) {
    show(wsChat, false);
    return null;
  }
  show(wsChat, true);
  wsChatList.innerHTML = '';
  wsChat.classList.remove('is-collapsed');
  wsChatToggle.setAttribute('aria-pressed', 'true');
  wsChatToggle.textContent = t('wvChatHide');
  const panel = new ChatPanel({
    list: wsChatList,
    input: wsChatInput,
    sendBtn: wsChatSend,
    notice: wsChatNotice,
    httpBase: HTTP_BASE,
    code: w.code,
    myLang: () => getUiLang(),
    senderLang: () => getUiLang(),
    displayName: () => auth.getUser()?.name || t('wvChatHost'),
    token: () => auth.getToken(), // host sends authenticated → sender_kind:"host"
    strings: webinarChatStrings(),
  });
  webinarChat = panel;
  void panel.loadHistory();
  return (event) => panel.append(event);
}

/** Open the host presence WS for a webinar and stream its count into the studio badge.
 *  When the webinar has chat enabled, the same WS also feeds the host chat panel. Closes any
 *  previous connection first (idempotent across studio re-opens). */
function openWebinarPresence(w: WebinarView): void {
  closeWebinarPresence();
  renderWebinarCount(0); // reset until the first frame arrives
  const onChat = openWebinarChat(w);
  webinarPresence = new PresenceClient({
    wsBase: WS_BASE,
    code: w.code,
    guestId: myId, // host isn't counted; guest_id just satisfies the endpoint contract
    host: true,
    token: auth.getToken(), // proves org membership so the server honors host=true
    onCount: renderWebinarCount,
    onChat: onChat ?? undefined,
  });
}

/** Close the host presence WS (broadcast ended / studio closed). Idempotent. */
function closeWebinarPresence(): void {
  webinarPresence?.close();
  webinarPresence = null;
  webinarChat = null;
}

// Host chat: submit sends with the host token (→ sender_kind:"host"); a collapse toggle
// hides the body without tearing down the WS.
wsChatForm.addEventListener('submit', (e) => {
  e.preventDefault();
  void webinarChat?.send();
});
wsChatToggle.addEventListener('click', () => {
  const collapsed = wsChat.classList.toggle('is-collapsed');
  wsChatToggle.setAttribute('aria-pressed', collapsed ? 'false' : 'true');
  wsChatToggle.textContent = t(collapsed ? 'wvChatShow' : 'wvChatHide');
  if (!collapsed) wsChatInput.focus();
});

/** Open the host STT bridge for a live broadcast: stream the publisher's mic track to the
 *  API's Deepgram ingest WS (binary WebM/Opus chunks) so subtitles fan out to viewers.
 *  Requires an authenticated host (the ingest WS is token-gated) and a captured stream.
 *  Idempotent — closes any previous bridge first. No-op for guests / before capture. */
function openWebinarStt(webinarId: string, publisher: WhipPublisher): void {
  closeWebinarStt();
  const token = auth.getToken();
  const stream = publisher.getLocalStream();
  if (!token || !stream) return; // only an authenticated host with a live mic can ingest
  activeWebinarStt = new WebinarSttClient({
    wsBase: WS_BASE,
    webinarId,
    token,
    // A fresh AudioCapture per (re)connect: a new MediaRecorder emits a header-bearing
    // first WebM chunk, which each new Deepgram stream requires. It sends binary chunks;
    // its harmless start/stop text frames are ignored by the ingest server.
    makeCapture: (socket) => new WebinarAudioCapture(stream, socket as unknown as WebSocket),
  });
  activeWebinarStt.start();
}

/** Close the host STT bridge (broadcast ended). Closing the ingest socket flushes any
 *  pending finals server-side. Idempotent. */
function closeWebinarStt(): void {
  activeWebinarStt?.stop();
  activeWebinarStt = null;
}

/** Reflect the WhipPublisher state on the studio's ON AIR badge. */
function wsPaintState(state: WhipState): void {
  const label =
    state === 'reconnecting'
      ? t('webinarReconnecting')
      : state === 'connecting'
        ? t('webinarGoingLive')
        : t('webinarOnAir');
  wsOnairText.textContent = label;
}

/** Render the studio's round mic + cam toggles from the active publisher's live state,
 *  mirroring the call bar's `setControlState`: mic/mic-off + video/video-off icons, a
 *  red `.active-danger` background when off, and `setToggleState` for aria-pressed. */
function wsUpdateControls(): void {
  const micOn = !!activePublisher?.isMicrophoneOn();
  wsMicBtn.classList.toggle('active-danger', !micOn);
  wsMicBtn.innerHTML = icon(micOn ? 'mic' : 'mic-off');
  setToggleState(wsMicBtn, micOn, t('muteTip'));
  const camOn = !!activePublisher?.isCameraOn();
  wsCamBtn.classList.toggle('active-danger', !camOn);
  wsCamBtn.innerHTML = icon(camOn ? 'video' : 'video-off');
  setToggleState(wsCamBtn, camOn, t('camTip'));
}

/** Show the studio's local preview from the active publisher's captured stream. */
function wsAttachLocalVideo(): void {
  const stream = activePublisher?.getLocalStream() ?? null;
  const on = !!activePublisher?.isCameraOn();
  if (stream && on) {
    wsVideo.srcObject = stream;
    void wsVideo.play().catch(() => {});
    wsVideoOff.hidden = true;
  } else {
    wsVideo.srcObject = null;
    wsVideoOff.hidden = false;
    const name = activeWebinar?.title || t('webinarsTitle');
    const avatar =
      billing && auth.isLoggedIn() ? auth.avatarUrl(auth.getUser()?.avatar_url, 192) : null;
    if (avatar) {
      wsAvatar.textContent = '';
      wsAvatar.style.background = 'none';
      const img = document.createElement('img');
      img.className = 'preview-avatar-img';
      img.referrerPolicy = 'no-referrer';
      img.alt = '';
      img.src = avatar;
      wsAvatar.replaceChildren(img);
    } else {
      wsAvatar.textContent = name.slice(0, 2).toUpperCase();
      wsAvatar.style.background = avatarGradient(name);
    }
  }
}

let activeWebinar: WebinarView | null = null;

/** Open the studio view for a (live or just-started) webinar. */
function openWebinarStudio(w: WebinarView): void {
  activeWebinar = w;
  show(webinarsScreen, false); // leave the list; do NOT re-show #home under the studio
  show(wpScreen, false);
  show(wsScreen, true);
  wsTitle.textContent = w.title;
  wsCode.textContent = w.code;
  openWebinarPresence(w);
  wsUpdateControls();
  wsPaintState(activePublisher?.getState() ?? 'on-air');
  wsAttachLocalVideo();
}

/** Start the broadcast with the pre-live device choice, then show the studio. */
async function startWebinarBroadcast(
  w: WebinarView,
  choice: {
    audioDeviceId?: string;
    videoDeviceId?: string;
    withCamera: boolean;
    withMic?: boolean;
  },
): Promise<void> {
  if (activePublisher) {
    toast(t('webinarAlreadyLive'), 'err');
    void openWebinars();
    return;
  }
  const publisher = new WhipPublisher({
    webinarId: w.id,
    withCamera: choice.withCamera,
    audioDeviceId: choice.audioDeviceId,
    videoDeviceId: choice.videoDeviceId,
    onState: (state) => {
      if (activePublisherId === w.id) {
        wsPaintState(state);
        wsAttachLocalVideo();
      }
      if (state === 'mic-denied') toast(t('webinarMicDenied'), 'err');
      if (state === 'error') toast(t('webinarPublishError'), 'err');
    },
  });
  try {
    await publisher.start();
    activePublisher = publisher;
    activePublisherId = w.id;
    // Carry the pre-live mute choice into the broadcast (disabling the shared audio
    // track also silences the STT ingest, so a host who went on air muted emits no
    // subtitles until they unmute).
    if (choice.withMic === false) publisher.toggleMicrophone(false);
    // Bridge the mic to the server STT ingest so viewers get live subtitles. Best-effort:
    // a guest (no token) or a missing stream just skips it — the video still broadcasts.
    openWebinarStt(w.id, publisher);
    openWebinarStudio(w);
  } catch {
    // start() surfaced mic-denied / error via onState + toast — back to the list.
    void openWebinars();
  }
}

/** Open the post-webinar recap screen: fetch transcripts immediately, lazy-load
 *  the chat tab on first click. Called after `endWebinarBroadcast` cleans up state. */
async function openWebinarRecap(webinarId: string, webinarCode: string): Promise<void> {
  // Switch to the recap screen immediately.
  show(wsScreen, false);
  show(recapScreen, true);
  // Active tab = transcript by default.
  recapTabTranscript.classList.add('active');
  recapTabTranscript.setAttribute('aria-selected', 'true');
  recapTabChat.classList.remove('active');
  recapTabChat.setAttribute('aria-selected', 'false');
  show(recapTranscriptPanel, true);
  show(recapChatPanel, false);
  recapTranscriptList.innerHTML = '';
  recapChatList.innerHTML = '';
  show(recapTranscriptEmpty, false);
  show(recapChatEmpty, false);

  const myLang = getUiLang();

  // Fetch transcripts (authenticated — host must be an org member).
  try {
    const res = await fetch(`${auth.HTTP_BASE}/api/webinars/${encodeURIComponent(webinarId)}/transcripts`, {
      headers: auth.authHeaders(),
    });
    if (res.ok) {
      const rows: Array<{ original_text: string; original_lang: string; translations: Record<string, string>; spoken_at: string }> = await res.json() as Array<{ original_text: string; original_lang: string; translations: Record<string, string>; spoken_at: string }>;
      if (rows.length === 0) {
        show(recapTranscriptEmpty, true);
      } else {
        recapTranscriptList.innerHTML = rows
          .map((r) => {
            const d = new Date(r.spoken_at);
            const timeStr = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
            return `<div class="recap-utterance"><time>${escHtml(timeStr)}</time>${escHtml(r.original_text)}</div>`;
          })
          .join('');
      }
    } else {
      show(recapTranscriptEmpty, true);
    }
  } catch {
    show(recapTranscriptEmpty, true);
  }

  // Lazy-load chat tab on first click.
  let chatLoaded = false;
  async function loadChatTab(): Promise<void> {
    if (chatLoaded) return;
    chatLoaded = true;
    try {
      // Chat endpoint is public (guests can read it); plain fetch, no auth headers needed.
      const res = await fetch(
        `${auth.HTTP_BASE}/api/w/${encodeURIComponent(webinarCode)}/chat?limit=500`,
      );
      if (res.ok) {
        const msgs: Array<{ text: string; display_name: string; sender_lang: string; translations: Record<string, string>; created_at: string }> = await res.json() as Array<{ text: string; display_name: string; sender_lang: string; translations: Record<string, string>; created_at: string }>;
        if (msgs.length === 0) {
          show(recapChatEmpty, true);
        } else {
          recapChatList.innerHTML = msgs
            .map((m) => {
              const text = m.translations?.[myLang] ?? m.text;
              return `<div class="recap-chat-msg"><div class="recap-sender">${escHtml(m.display_name ?? '')}</div>${escHtml(text)}</div>`;
            })
            .join('');
        }
      } else {
        show(recapChatEmpty, true);
      }
    } catch {
      show(recapChatEmpty, true);
    }
  }

  recapTabTranscript.addEventListener('click', () => {
    recapTabTranscript.classList.add('active');
    recapTabTranscript.setAttribute('aria-selected', 'true');
    recapTabChat.classList.remove('active');
    recapTabChat.setAttribute('aria-selected', 'false');
    show(recapTranscriptPanel, true);
    show(recapChatPanel, false);
  });

  recapTabChat.addEventListener('click', () => {
    recapTabChat.classList.add('active');
    recapTabChat.setAttribute('aria-selected', 'true');
    recapTabTranscript.classList.remove('active');
    recapTabTranscript.setAttribute('aria-selected', 'false');
    show(recapChatPanel, true);
    show(recapTranscriptPanel, false);
    void loadChatTab();
  });
}

/** Stop the active broadcast (host confirmed End), then return to the Webinars list. */
async function endWebinarBroadcast(): Promise<void> {
  // Capture webinar info BEFORE clearing state — needed for the recap screen.
  const recapId = activeWebinar?.id ?? null;
  const recapCode = activeWebinar?.code ?? null;
  // Close the STT bridge FIRST: closing the ingest socket flushes pending finals before we
  // stop capturing, so the last words still reach viewers as a subtitle.
  closeWebinarStt();
  if (activePublisher) {
    await activePublisher.stop();
    activePublisher = null;
    activePublisherId = null;
  }
  closeWebinarPresence();
  activeWebinar = null;
  wsVideo.srcObject = null;

  if (recapId && recapCode) {
    await openWebinarRecap(recapId, recapCode);
  } else {
    show(wsScreen, false);
    void openWebinars();
  }
}

// Round mic toggle: mute/unmute the captured audio track. Because the STT bridge's
// MediaRecorder wraps the SAME track, disabling it silences BOTH the WHIP broadcast and
// the server STT ingest (no viewer audio, no subtitles) — one call covers both paths.
wsMicBtn.addEventListener('click', () => {
  if (!activePublisher || activePublisherId !== activeWebinar?.id) return;
  activePublisher.toggleMicrophone(!activePublisher.isMicrophoneOn());
  wsUpdateControls();
});

wsCamBtn.addEventListener('click', async () => {
  if (!activePublisher || activePublisherId !== activeWebinar?.id) return;
  wsCamBtn.disabled = true;
  await activePublisher.toggleCamera(!activePublisher.isCameraOn());
  wsUpdateControls();
  wsAttachLocalVideo();
  wsCamBtn.disabled = false;
});

wsEndBtn.addEventListener('click', () => show(wsEndModal, true));
wsEndCancel.addEventListener('click', () => show(wsEndModal, false));
wsEndConfirm.addEventListener('click', async () => {
  wsEndConfirm.disabled = true;
  try {
    show(wsEndModal, false);
    await endWebinarBroadcast();
  } finally {
    wsEndConfirm.disabled = false;
  }
});
wsEndModal.addEventListener('click', (e) => {
  if (e.target === wsEndModal) show(wsEndModal, false);
});

recapCloseBtn?.addEventListener('click', () => {
  show(recapScreen, false);
  void openWebinars();
});

// ---- Add to Google Calendar (scheduled webinars) -----------------------------
// A pending calendar-add retried after the connect-calendar (OAuth re-consent) flow.
let pendingCalendarWebinarId: string | null = null;

/** Build the "Add to Google Calendar" row for a scheduled webinar. On 409 (calendar
 *  not connected) it routes the host into the existing connect-calendar flow and retries
 *  after re-consent; on 400 (unscheduled) it explains a start time is needed. On success
 *  it surfaces a link to the created event. */
function buildAddCalendarRow(w: WebinarView): HTMLElement {
  const row = document.createElement('div');
  row.className = 'webinar-calendar-row';
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'btn-ghost webinar-calendar-btn';
  btn.textContent = t('webinarAddCalendar');
  const note = document.createElement('span');
  note.className = 'webinar-calendar-note';
  const setNote = (msg: string, kind: 'ok' | 'err' | '', link?: string) => {
    note.replaceChildren();
    note.classList.toggle('ok', kind === 'ok');
    note.classList.toggle('err', kind === 'err');
    note.append(document.createTextNode(msg));
    if (link) {
      const a = document.createElement('a');
      a.href = link;
      a.target = '_blank';
      a.rel = 'noopener noreferrer';
      a.textContent = ` ${t('webinarCalendarView')}`;
      note.append(a);
    }
  };
  btn.addEventListener('click', async () => {
    btn.disabled = true;
    setNote(t('webinarCalendarAdding'), '');
    try {
      const evt = await addToCalendar(w.id);
      setNote(t('webinarCalendarAdded'), 'ok', evt.html_link);
    } catch (err) {
      const status = err instanceof WebinarError ? err.status : 0;
      if (status === 409) {
        // Calendar not connected — route into the connect-calendar OAuth flow, then retry.
        setNote(t('webinarCalendarConnecting'), '');
        pendingCalendarWebinarId = w.id;
        connectCalendar();
      } else if (status === 400) {
        setNote(t('webinarCalendarNotScheduled'), 'err');
      } else {
        setNote(webinarErrorMessage(err), 'err');
      }
    } finally {
      btn.disabled = false;
    }
  });
  row.append(btn, note);
  return row;
}

/** Render a QR of `text` into `img` as a data URL. The `qrcode` browser bundle is a
 *  lazy chunk (dynamic import), so it never weighs on the main app load. */
async function renderQr(img: HTMLImageElement, text: string): Promise<void> {
  try {
    const { toDataURL } = await import('qrcode');
    img.src = await toDataURL(text, { margin: 1, width: 160 });
  } catch {
    // QR is a convenience — the copyable link still works if the chunk fails.
    show(img, false);
  }
}

/** Generate a larger, print/download-quality QR data URL for `text`. Falls back to
 *  the already-rendered card `<img>` src if the lazy chunk fails to load. */
async function qrDataUrl(text: string, fallbackSrc: string): Promise<string> {
  try {
    const { toDataURL } = await import('qrcode');
    return await toDataURL(text, { margin: 2, width: 512 });
  } catch {
    return fallbackSrc;
  }
}

/** Save a QR data URL as a PNG file (`webinar-{code}.png`). */
function downloadQr(dataUrl: string, code: string): void {
  const a = document.createElement('a');
  a.href = dataUrl;
  a.download = qrDownloadFilename(code);
  document.body.appendChild(a);
  a.click();
  a.remove();
}

/** Open a print window showing just the QR + the webinar title and join URL, centered.
 *  Built with DOM APIs (no `document.write`); user-supplied text is set via
 *  `textContent`, so there's no HTML-injection surface. */
function printQr(dataUrl: string, title: string, joinUrl: string): void {
  const win = window.open('', '_blank', 'noopener,noreferrer,width=520,height=640');
  if (!win) {
    toast(t('webinarQrPrintBlocked'), 'err');
    return;
  }
  const doc = win.document;
  doc.title = title;
  const style = doc.createElement('style');
  style.textContent =
    'html,body{height:100%;margin:0}' +
    'body{display:flex;flex-direction:column;align-items:center;justify-content:center;' +
    'gap:16px;font-family:system-ui,-apple-system,sans-serif;padding:24px;text-align:center}' +
    'h1{font-size:20px;margin:0}img{width:320px;height:320px}' +
    'p{margin:0;color:#444;word-break:break-all;max-width:360px}';
  doc.head.appendChild(style);
  const h1 = doc.createElement('h1');
  h1.textContent = title;
  const img = doc.createElement('img');
  img.src = dataUrl;
  img.alt = '';
  const p = doc.createElement('p');
  p.textContent = joinUrl;
  doc.body.append(h1, img, p);
  const fire = () => {
    win.focus();
    win.print();
  };
  // Print once the QR image has decoded, else fall back to a short delay.
  if (img.complete) fire();
  else {
    img.addEventListener('load', fire, { once: true });
    img.addEventListener('error', fire, { once: true });
  }
}

/** Open the fullscreen QR zoom overlay for a webinar (reuses the shared modal focus
 *  trap + Escape via `show()` on a `.modal-overlay`). */
function openQrModal(w: WebinarView, cardQrSrc: string): void {
  webinarQrModalTitle.textContent = w.title;
  webinarQrModalUrl.textContent = w.join_url;
  webinarQrModalImg.alt = t('webinarQrAlt');
  webinarQrModalImg.src = cardQrSrc; // instant; upgraded to a crisp large QR below
  void qrDataUrl(w.join_url, cardQrSrc).then((url) => (webinarQrModalImg.src = url));
  show(webinarQrModal, true);
}
webinarQrModal.addEventListener('click', (e) => {
  if (e.target === webinarQrModal) show(webinarQrModal, false); // click-outside to close
});
$('webinar-qr-close').addEventListener('click', () => show(webinarQrModal, false));

/** Pre-live voice clone from a webinar card (webinar-ui-fixes #5). Reuses the exact
 *  capture + clone flow the call pre-join uses (`recordVoiceSample` → `cloneVoice`):
 *  grab the mic, record ≥3 s of speech, clone it, and mark the account cloned so the
 *  toggle/hint update everywhere. Best-effort — never throws. `onState` drives the
 *  card's inline feedback. Returns whether a clone was stored. */
async function runWebinarVoiceClone(
  onState: (s: VoicePrepState) => void,
): Promise<boolean> {
  let stream: MediaStream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch {
    onState('failed'); // mic denied / unavailable
    return false;
  }
  onState('recording');
  try {
    const blob = await recordVoiceSample(stream);
    if (!blob) {
      onState('failed'); // too little speech captured
      return false;
    }
    onState('saving');
    const res = await cloneVoice(blob, webinarLangSel.value || getUiLang());
    if (res.voice_id) {
      localStorage.setItem(VOICE_CLONED_KEY, '1');
      onState('cloned');
      return true;
    }
    onState('failed');
    return false;
  } catch {
    onState('failed');
    return false;
  } finally {
    stream.getTracks().forEach((tr) => tr.stop());
  }
}

/** A "Clone your voice" row for a webinar card: a button that records + clones the
 *  host's voice, plus an inline status note. On success it re-renders the list so the
 *  toggle/hint update per the now-cloned state. */
function buildWebinarCloneRow(): HTMLElement {
  const row = document.createElement('div');
  row.className = 'webinar-clone-row';
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'btn-ghost webinar-clone-btn';
  btn.textContent = t('webinarCloneVoice');
  const note = document.createElement('span');
  note.className = 'webinar-clone-note';
  const setNote = (msg: string, kind: 'ok' | 'err' | '') => {
    note.textContent = msg;
    note.classList.toggle('ok', kind === 'ok');
    note.classList.toggle('err', kind === 'err');
  };
  const onState = (s: VoicePrepState): void => {
    btn.disabled = s === 'recording' || s === 'saving';
    if (s === 'recording') setNote(t('voicePrepRecording'), '');
    else if (s === 'saving') setNote(t('voicePrepSaving'), '');
    else if (s === 'cloned') setNote(`✓ ${t('voicePrepSaved')}`, 'ok');
    else if (s === 'failed') setNote(`✗ ${t('voicePrepFailed')}`, 'err');
  };
  btn.addEventListener('click', async () => {
    const ok = await runWebinarVoiceClone(onState);
    // Refresh the list so the create-form toggle/hint and this row reflect the clone.
    if (ok) {
      syncWebinarVoiceClone();
      await loadWebinars();
    }
  });
  row.append(btn, note);
  return row;
}

async function submitWebinar(): Promise<void> {
  const orgId = webinarOrgSel.value;
  const title = webinarTitleInput.value.trim();
  if (!orgId) {
    setWebinarStatus(t('webinarErrOrg'), 'err');
    return;
  }
  if (!title) {
    setWebinarStatus(t('webinarErrTitle'), 'err');
    return;
  }
  // Optional schedule: empty inputs → immediate webinar (null start/end).
  const scheduledStart = fromDatetimeLocalValue(webinarStartInput.value);
  const scheduledEnd = fromDatetimeLocalValue(webinarEndInput.value);
  // Friendly inline validation before the API call (server re-checks, 400).
  const schedule = validateSchedule(scheduledStart, scheduledEnd, Date.now());
  if (schedule === 'startPast') {
    setWebinarStatus(t('webinarErrStartPast'), 'err');
    return;
  }
  if (schedule === 'endBeforeStart') {
    setWebinarStatus(t('webinarErrEndBeforeStart'), 'err');
    return;
  }
  webinarCreateBtn.disabled = true;
  setWebinarStatus(t('webinarCreating'), '');
  try {
    // Voice cloning is Enhanced-only and moot once already cloned — only send it
    // when the tier-aware toggle is actually offered AND on.
    const cloneOffered = showVoiceCloneToggle(webinarTierSel.value, hasVoiceClone());
    // Optional project: the "No project" default has an empty value → omit project_id.
    const projectId = webinarProjectSel.value || undefined;
    await createWebinar({
      org_id: orgId,
      title,
      source_language: webinarLangSel.value,
      ...(projectId ? { project_id: projectId } : {}),
      tier: webinarTierSel.value as WebinarView['tier'],
      record_video: switchOn(webinarRecordVideoSw),
      record_transcript: switchOn(webinarRecordTranscriptSw),
      chat_enabled: switchOn(webinarChatEnabledSw),
      visibility: switchOn(webinarVisibilitySw) ? 'public' : 'private',
      voice_clone: cloneOffered && switchOn(webinarVoiceCloneSw),
      scheduled_start: scheduledStart,
      scheduled_end: scheduledEnd,
    });
    webinarTitleInput.value = '';
    webinarStartInput.value = '';
    webinarEndInput.value = '';
    webinarProjectSel.value = ''; // reset to "No project" for the next create
    setWebinarStatus(t('webinarCreated'), 'ok');
    collapseWebinarForm(); // tidy back to the button; the new webinar shows in the list
    await loadWebinars();
  } catch (err) {
    setWebinarStatus(webinarErrorMessage(err), 'err');
  } finally {
    webinarCreateBtn.disabled = false;
  }
}

webinarForm.addEventListener('submit', (e) => {
  e.preventDefault();
  void submitWebinar();
});
webinarCreateToggle.addEventListener('click', () => revealWebinarForm());
webinarCreateCancel.addEventListener('click', () => collapseWebinarForm());
webinarOrgSel.addEventListener('change', () => {
  void loadWebinarProjects();
  void loadWebinars();
});
// Active | Archived segmented control: switch the shown list on click.
webinarTabs.addEventListener('click', (e) => {
  const btn = (e.target as HTMLElement).closest('.seg-btn') as HTMLElement | null;
  if (!btn) return;
  void selectWebinarTab(btn.dataset.archived === 'true');
});
$('webinars-back').addEventListener('click', closeWebinars);
$('webinars-btn').addEventListener('click', () => {
  closeAccountMenu();
  void openWebinars();
});

// Populate the pre-join org/project selector for business users.
async function setupBizPrejoin(): Promise<void> {
  const block = $('biz-prejoin');
  const recRow = $('biz-record-row');
  const orgs = await ensureBizOrgs();
  if (orgs.length === 0) {
    show(block, false);
    show(recRow, false);
    return;
  }
  const orgSel = $<HTMLSelectElement>('biz-org');
  const projSel = $<HTMLSelectElement>('biz-project');
  orgSel.replaceChildren(...orgs.map((o) => new Option(o.name, o.id)));
  // Hide the org picker when there's only one org (its value is still used).
  const orgLabel = orgSel.closest('label');
  if (orgLabel) (orgLabel as HTMLElement).style.display = orgs.length > 1 ? '' : 'none';
  const loadProjects = async () => {
    const projects = await listProjects(orgSel.value);
    projSel.replaceChildren(
      new Option(t('bizNoProject'), ''),
      ...projects.map((p) => new Option(p.name, p.id)),
    );
  };
  // Cloud recording is a paid feature: only offer it for the selected org when it
  // has an active Business/Enterprise subscription. Re-evaluated when the org
  // changes; the toggle is reset off when it's not available.
  const updateRecordRow = () => {
    const org = orgs.find((o) => o.id === orgSel.value);
    const allowed = canCloudRecord(org);
    show(recRow, allowed);
    if (!allowed) $<HTMLInputElement>('biz-record').checked = false;
  };
  orgSel.onchange = () => {
    updateRecordRow();
    void loadProjects();
  };
  await loadProjects();
  updateRecordRow();
  // Pre-select the org/project this room is already bound to (e.g. a scheduled
  // meeting created in the dashboard) so connecting doesn't clobber the project.
  const boundRoom = roomInput.value.trim();
  if (boundRoom) {
    const binding = await getRoomBinding(boundRoom);
    if (binding && orgs.some((o) => o.id === binding.org_id)) {
      orgSel.value = binding.org_id;
      updateRecordRow();
      await loadProjects();
      if (binding.project_id) projSel.value = binding.project_id;
    }
  }
  show(block, true);
}

// Before opening the socket: bind the room to the chosen org/project (+recording)
// so the server's session_started inherits it. Sets `bizRecording`.
async function bindRoomIfBusiness(): Promise<void> {
  bizRecording = false;
  if (!session || $('biz-prejoin').classList.contains('hidden')) return;
  const orgId = $<HTMLSelectElement>('biz-org').value;
  if (!orgId) return;
  const record = $<HTMLInputElement>('biz-record').checked;
  const ok = await bindRoom(session.room, {
    org_id: orgId,
    project_id: $<HTMLSelectElement>('biz-project').value || null,
    cloud_recording_enabled: record,
  });
  if (ok) bizRecording = record;
}

function setBalanceUi(balance: number): void {
  const low = balance < 0.5;
  const formatted = auth.formatCredits(balance);
  accountBalance.textContent = formatted;
  accountBalance.classList.toggle('low', low);
  billingBalance.textContent = formatted; // Account → Billing section mirror
  billingBalance.classList.toggle('low', low);
  callBalance.classList.remove('hidden');
  callBalance.textContent = formatted;
  callBalance.classList.toggle('low', low);
}

// --- Google Identity Services (OAuth code flow) ---
// We use the OAuth *authorization-code* flow (popup) so the server can obtain a
// refresh token and schedule meetings on the user's Google Calendar (spec: scheduled
// meetings). This replaces the older ID-token / FedCM chooser — the popup is now the
// single sign-in path. The OAuth consent screen must list the Calendar scope.
let gsiLoaded = false;
// Scopes requested at sign-in; keep in sync with the server's GOOGLE_CALENDAR_SCOPES.
const OAUTH_SCOPE = 'openid email profile https://www.googleapis.com/auth/calendar.events';
let googleCodeClient: { requestCode: () => void } | undefined;

function setupGoogleSignIn(): void {
  const clientId = auth.getGoogleClientId();
  const customBtn = document.getElementById('gsi-signin');
  if (!clientId || !customBtn) return;
  loadGsi()
    .then(() => {
      const g = (window as unknown as { google?: any }).google;
      if (!g?.accounts?.oauth2) return;
      googleCodeClient = g.accounts.oauth2.initCodeClient({
        client_id: clientId,
        scope: OAUTH_SCOPE,
        ux_mode: 'popup',
        callback: onGoogleCode,
      });
      show(customBtn, true);
      customBtn.onclick = () => googleCodeClient?.requestCode();
    })
    .catch(() => {});
}

function loadGsi(): Promise<void> {
  if (gsiLoaded) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const s = document.createElement('script');
    s.src = 'https://accounts.google.com/gsi/client';
    s.async = true;
    s.defer = true;
    s.onload = () => {
      gsiLoaded = true;
      resolve();
    };
    s.onerror = () => reject(new Error('gsi load failed'));
    document.head.appendChild(s);
  });
}

async function onGoogleCode(resp: { code?: string; error?: string }): Promise<void> {
  if (!resp.code) return;
  try {
    await auth.exchangeGoogleCode(resp.code);
    // A pending "Add to Google Calendar" retry (re-consent path): the server now holds a
    // fresh refresh token with the Calendar scope, so retry the calendar add in place —
    // don't navigate away from the Webinars screen the host is on.
    if (pendingCalendarWebinarId) {
      const id = pendingCalendarWebinarId;
      pendingCalendarWebinarId = null;
      try {
        const evt = await addToCalendar(id);
        toast(t('webinarCalendarAdded'), 'ok');
        void openEventLink(evt.html_link);
      } catch (err) {
        toast(webinarErrorMessage(err), 'err');
      }
      return;
    }
    track('login', { method: 'google' });
    enterHome();
  } catch {
    /* stay on the current screen; the user can retry */
  }
}

/** Open the created calendar event in a new tab (noopener). */
function openEventLink(url: string): void {
  window.open(url, '_blank', 'noopener,noreferrer');
}

/** Route the host into the existing Google connect-calendar flow (OAuth code popup).
 *  On success `onGoogleCode` retries any `pendingCalendarWebinarId`. Falls back to a
 *  toast if the Google client isn't ready. */
function connectCalendar(): void {
  if (googleCodeClient) {
    googleCodeClient.requestCode();
  } else {
    toast(t('scheduleConnectCalendar'), 'err');
  }
}

$('guest-btn').addEventListener('click', () => enterHome());
// Guest's route back to the login screen (spec 0037).
$('guest-signin-btn').addEventListener('click', () => showLogin());
$('logout-btn').addEventListener('click', () => {
  auth.clearSession();
  accountBar.classList.add('hidden');
  showLogin();
});

// --- Buy-credits modal ---
function openBuyModal(): void {
  track('buy_credits_open');
  show(buyModal, true);
  buyStatus.textContent = '';
  buyStatus.classList.remove('error');
  const u = auth.getUser();
  if (u) modalBalance.textContent = auth.formatCredits(u.balance);
  void renderPackages();
}

async function renderPackages(): Promise<void> {
  packagesList.innerHTML = '';
  creditPackages = await auth.fetchPackages();
  const pkgs = creditPackages;
  for (const p of pkgs) {
    const btn = document.createElement('button');
    btn.className = 'pkg';
    btn.type = 'button';
    const left = document.createElement('div');
    const name = document.createElement('div');
    name.className = 'pkg-name';
    name.textContent = p.name;
    const credits = document.createElement('div');
    credits.className = 'pkg-credits';
    credits.textContent = `${auth.formatCredits(p.credits_usd)} ${t('history').toLowerCase()}`;
    left.append(name, credits);
    const price = document.createElement('span');
    price.className = 'pkg-price';
    price.textContent = auth.formatCredits(p.price_usd);
    btn.append(left, price);
    btn.addEventListener('click', () => checkout(p.id, btn));
    packagesList.appendChild(btn);
  }
}

async function checkout(pkgId: string, btn: HTMLButtonElement): Promise<void> {
  btn.disabled = true;
  buyStatus.textContent = '';
  buyStatus.classList.remove('error');
  // Track upgrade attempt
  const pkg = creditPackages.find((p) => p.id === pkgId);
  if (pkg) {
    track('upgrade_clicked', {
      package_id: pkgId,
      credits: pkg.credits_usd,
      amount_cents: Math.round(pkg.price_usd * 100),
      location: 'buy_modal',
    });
  }
  try {
    location.href = await auth.startCheckout(pkgId);
  } catch (e) {
    // Surface the failure instead of doing nothing (e.g. Stripe rejected the
    // price — common when the configured price IDs don't match the key's mode).
    console.error('checkout failed:', e);
    buyStatus.textContent = t('checkoutFailed');
    buyStatus.classList.add('error');
    btn.disabled = false;
  }
}

type LedgerTab = 'history' | 'usage' | 'transcripts';

function selectTab(which: LedgerTab): void {
  for (const [id, tab] of [['tab-history', 'history'], ['tab-usage', 'usage'], ['tab-transcripts', 'transcripts']] as const) {
    $(id).classList.toggle('active', which === tab);
    $(id).setAttribute('aria-selected', String(which === tab));
  }
  void loadLedger(which);
}

async function loadLedger(which: LedgerTab): Promise<void> {
  ledgerList.innerHTML = '';
  if (which === 'transcripts') {
    await renderTranscriptRows();
    return;
  }
  let rows: any[] = which === 'history' ? await auth.fetchHistory() : await auth.fetchUsage();
  // "Crediti" shows money in (welcome + purchases); per-call usage lives in the
  // "Utilizzo" tab, so don't repeat each speaking-time deduction here. AI
  // feature charges (kind ai_report/ai_sentiment/ai_email/ai_suggestions) DO
  // show here — they render via the description/kind fallback below.
  if (which === 'history') rows = rows.filter((r) => r.kind !== 'usage');
  if (!rows.length) {
    const empty = document.createElement('div');
    empty.className = 'ledger-empty';
    empty.textContent = t('noActivity');
    ledgerList.appendChild(empty);
    return;
  }
  for (const r of rows) {
    const row = document.createElement('div');
    row.className = 'ledger-row';
    const desc = document.createElement('span');
    desc.className = 'ledger-desc';
    const amount = document.createElement('span');
    amount.className = 'ledger-amount';
    if (which === 'history') {
      desc.textContent = r.description || r.kind;
      amount.textContent = `${r.amount >= 0 ? '+' : ''}${auth.formatCredits(r.amount)}`;
      amount.classList.add(r.amount >= 0 ? 'pos' : 'neg');
    } else {
      desc.textContent = `${r.room} · ${Math.round(r.speaking_seconds)}s`;
      amount.textContent = `-${auth.formatCredits(r.cost)}`;
      amount.classList.add('neg');
    }
    row.append(desc, amount);
    ledgerList.appendChild(row);
  }
}

/** Transcripts tab: one row per recorded call with PDF/JSON download buttons. */
async function renderTranscriptRows(): Promise<void> {
  const sessions = await auth.fetchSessions();
  if (!sessions.length) {
    const empty = document.createElement('div');
    empty.className = 'ledger-empty';
    empty.textContent = t('noActivity');
    ledgerList.appendChild(empty);
    return;
  }
  for (const s of sessions) {
    const row = document.createElement('div');
    row.className = 'ledger-row';
    const desc = document.createElement('span');
    desc.className = 'ledger-desc';
    const date = new Date(s.started_at).toLocaleDateString();
    desc.textContent = `${s.room} · ${date} · ${s.event_count} ${t('eventsLabel')}`;
    const actions = document.createElement('span');
    actions.className = 'ledger-actions';
    // Full session detail screen (specs 0011+) — closes the modal first.
    const open = document.createElement('button');
    open.type = 'button';
    open.className = 'ledger-dl';
    open.textContent = t('openBtn');
    open.addEventListener('click', () => {
      show(buyModal, false);
      // Lazy chunk (spec 0105): the session detail screen loads on open.
      void import('./session-screen').then(({ openSessionScreen }) =>
        openSessionScreen({
          id: s.id,
          room: s.room,
          started_at: s.started_at,
          ended_at: s.ended_at,
          event_count: s.event_count,
        }),
      );
    });
    actions.appendChild(open);
    for (const format of ['pdf', 'json'] as const) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'ledger-dl';
      btn.textContent = format.toUpperCase();
      if (s.event_count === 0) {
        btn.disabled = true;
        btn.title = t('noTranscriptEvents');
      }
      btn.addEventListener('click', async () => {
        btn.disabled = true;
        const r = await auth.downloadTranscript(s.id, format, getUiLang());
        btn.disabled = false;
        if (!r.ok) toast(t(r.status === 429 ? 'downloadRateLimited' : 'downloadFailed'));
      });
      actions.appendChild(btn);
    }
    row.append(desc, actions);
    ledgerList.appendChild(row);
  }
}

// --- Post-call transcript modal (spec 0009) ---
const postcallModal = $('postcall-modal');
let postCallSessionId: string | null = null;
let postCallEvents = 0;

function openPostCallModal(ended: {
  id: string;
  room: string;
  events: number;
  durationMs: number;
}): void {
  // Authenticated users get the full session detail screen (specs 0011+);
  // the modal below stays as the minimal fallback path.
  if (auth.isLoggedIn()) {
    const now = Date.now();
    // Lazy chunk (spec 0105): the session detail screen loads on open.
    void import('./session-screen').then(({ openSessionScreen }) =>
      openSessionScreen({
        id: ended.id,
        room: ended.room,
        started_at: new Date(now - ended.durationMs).toISOString(),
        ended_at: new Date(now).toISOString(),
        event_count: ended.events,
      }),
    );
    return;
  }
  postCallSessionId = ended.id;
  postCallEvents = ended.events;
  $('postcall-room').textContent = ended.room;
  $('postcall-duration').textContent = formatCallDuration(ended.durationMs);
  $('postcall-events').textContent = String(ended.events);
  for (const id of ['postcall-pdf', 'postcall-json']) {
    const btn = $<HTMLButtonElement>(id);
    btn.disabled = ended.events === 0;
    btn.title = ended.events === 0 ? t('noTranscriptEvents') : '';
  }
  show(postcallModal, true);
}

function formatCallDuration(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h > 0
    ? `${h}h ${String(m).padStart(2, '0')}m`
    : `${m}m ${String(s).padStart(2, '0')}s`;
}

async function downloadFromPostCall(format: 'json' | 'pdf', btn: HTMLButtonElement): Promise<void> {
  if (!postCallSessionId || btn.disabled) return;
  const prev = btn.textContent;
  btn.disabled = true;
  btn.textContent = t('processing');
  const r = await auth.downloadTranscript(postCallSessionId, format, getUiLang());
  btn.textContent = prev;
  btn.disabled = postCallEvents === 0;
  if (!r.ok) toast(t(r.status === 429 ? 'downloadRateLimited' : 'downloadFailed'));
}

$('postcall-close').addEventListener('click', () => show(postcallModal, false));
postcallModal.addEventListener('click', (e) => {
  if (e.target === postcallModal) show(postcallModal, false);
});
$('postcall-pdf').addEventListener('click', (e) =>
  void downloadFromPostCall('pdf', e.currentTarget as HTMLButtonElement),
);
$('postcall-json').addEventListener('click', (e) =>
  void downloadFromPostCall('json', e.currentTarget as HTMLButtonElement),
);

$('buy-btn').addEventListener('click', openBuyModal);
$('buy-close').addEventListener('click', () => show(buyModal, false));
buyModal.addEventListener('click', (e) => {
  if (e.target === buyModal) show(buyModal, false);
});
$('billing-buy-btn').addEventListener('click', openBuyModal); // Account → Billing shortcut

// ---- Avatar account menu (spec: account hub) ----
// The single entry point for every secondary account destination. Items with a
// `data-acct-nav` deep-link into the matching Account section; Logout / Workspace keep
// their own handlers and the menu just closes around them.
const accountTrigger = $('account-trigger');
const accountMenu = $('account-menu');
function closeAccountMenu(): void {
  accountMenu.classList.add('hidden');
  accountTrigger.setAttribute('aria-expanded', 'false');
}
accountTrigger.addEventListener('click', (e) => {
  e.stopPropagation(); // don't let the document handler immediately re-close it
  const open = accountMenu.classList.toggle('hidden');
  accountTrigger.setAttribute('aria-expanded', String(!open));
});
accountMenu.addEventListener('click', (e) => {
  const item = (e.target as HTMLElement).closest<HTMLElement>('.acct-item');
  if (!item) return;
  const nav = item.dataset.acctNav as AccountSection | undefined;
  if (nav) openAccount(nav);
  closeAccountMenu();
});
// Dismiss on outside-click + ESC, mirroring the modal-overlay affordances.
document.addEventListener('click', (e) => {
  if (!accountMenu.classList.contains('hidden') && !accountBar.contains(e.target as Node))
    closeAccountMenu();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !accountMenu.classList.contains('hidden')) {
    closeAccountMenu();
    accountTrigger.focus();
  }
});

// ============================================================================
// ---- Friends: persistent list + requests + invite-to-call (spec: friends) --
// Friends live ONLY in Account → Friends now (the standalone modal was removed). The
// request count surfaces at rest on three badges: the avatar (home), the avatar menu
// item, and the Account section tab.
const friendsBadge = $('friends-badge'); // on the avatar trigger
const friendsMenuBadge = $('friends-menu-badge'); // in the avatar dropdown
const friendsTabBadge = $('friends-tab-badge'); // on the Account section tab

// Cached relationship ids so the in-call participant list can hide "add friend" for
// people you're already connected to / have a pending request with either way.
const friendIds = new Set<string>();
const friendIncomingIds = new Set<string>();
const friendOutgoingIds = new Set<string>();

function friendActionBtn(label: string, primary: boolean, danger: boolean, onClick: () => void): HTMLButtonElement {
  const b = document.createElement('button');
  b.className = `${primary ? 'btn-primary' : 'btn-ghost'}${danger ? ' friend-danger' : ''}`;
  b.textContent = label;
  b.addEventListener('click', onClick);
  return b;
}

/** One friend/request row: avatar + name/email + the given action buttons. */
function friendRow(f: Friend, actions: HTMLButtonElement[]): HTMLDivElement {
  const row = document.createElement('div');
  row.className = 'friend-row';
  const av = document.createElement('span');
  av.className = 'friend-avatar';
  fillAvatar(av, f.name, f.avatar_url, 36, 2);
  const info = document.createElement('div');
  info.className = 'friend-row-info';
  const name = document.createElement('div');
  name.className = 'friend-row-name';
  name.textContent = f.name; // textContent: names are user-controlled (XSS, spec 0028)
  const sub = document.createElement('div');
  sub.className = 'friend-row-sub';
  sub.textContent = f.email;
  info.append(name, sub);
  const acts = document.createElement('div');
  acts.className = 'friend-row-actions';
  acts.append(...actions);
  row.append(av, info, acts);
  return row;
}

interface FriendListEls {
  incomingSection: HTMLElement;
  incomingList: HTMLElement;
  outgoingSection: HTMLElement;
  outgoingList: HTMLElement;
  friendsList: HTMLElement;
  friendsEmpty: HTMLElement;
}

/** Paint the three friend lists — incoming requests (accept/reject), outgoing/pending
 *  (cancel), and accepted friends — into the given containers. Shared by the friends
 *  modal and the profile screen so both surfaces show the same live state. */
function renderFriendLists(
  els: FriendListEls,
  friends: Friend[],
  incoming: Friend[],
  outgoing: Friend[],
): void {
  const inCall = !!session?.room;

  els.incomingList.innerHTML = '';
  for (const f of incoming) {
    els.incomingList.appendChild(
      friendRow(f, [
        friendActionBtn(t('friendAccept'), true, false, () => void onFriendAccept(f.id)),
        friendActionBtn(t('friendReject'), false, true, () => void onFriendRemove(f.id)),
      ]),
    );
  }
  show(els.incomingSection, incoming.length > 0);

  els.outgoingList.innerHTML = '';
  for (const f of outgoing) {
    els.outgoingList.appendChild(
      friendRow(f, [friendActionBtn(t('friendCancel'), false, true, () => void onFriendRemove(f.id))]),
    );
  }
  show(els.outgoingSection, outgoing.length > 0);

  els.friendsList.innerHTML = '';
  for (const f of friends) {
    const acts: HTMLButtonElement[] = [];
    if (inCall) acts.push(friendActionBtn(t('friendInvite'), true, false, () => void onFriendInvite(f.id)));
    acts.push(friendActionBtn(t('friendRemove'), false, true, () => void onFriendRemove(f.id)));
    els.friendsList.appendChild(friendRow(f, acts));
  }
  show(els.friendsEmpty, friends.length === 0);
}

/** Mirror the pending-incoming-request count onto all three request badges (avatar,
 *  avatar-menu item, Account tab). Hidden when there are none. */
function setRequestBadges(count: number): void {
  for (const badge of [friendsBadge, friendsMenuBadge, friendsTabBadge]) {
    badge.textContent = String(count);
    show(badge, count > 0);
  }
}

/** Fetch friends + requests, refresh the cached id sets and the request badges, and
 *  (when `render`, or whenever the Account → Friends section is on screen) repaint the
 *  lists. Also refreshes the participant list so in-call "add friend" buttons stay in sync. */
async function loadFriendState(render: boolean): Promise<void> {
  if (!auth.isLoggedIn()) return;
  const [friends, reqs] = await Promise.all([fetchFriends(), fetchFriendRequests()]);
  friendIds.clear();
  friends.forEach((f) => friendIds.add(f.id));
  friendIncomingIds.clear();
  reqs.incoming.forEach((f) => friendIncomingIds.add(f.id));
  friendOutgoingIds.clear();
  reqs.outgoing.forEach((f) => friendOutgoingIds.add(f.id));

  setRequestBadges(reqs.incoming.length);

  if (render || accountFriendsVisible())
    renderProfileFriends(friends, reqs.incoming, reqs.outgoing);
  if (session) {
    updateParticipantsList(); // keep the participants-panel add-friend buttons in sync
    refreshTileFriendButtons(); // and the per-tile add/remove-friend controls
  }
}

function clearFriendState(): void {
  friendIds.clear();
  friendIncomingIds.clear();
  friendOutgoingIds.clear();
  setRequestBadges(0);
}

async function onFriendAccept(id: string): Promise<void> {
  if (await acceptFriend(id)) {
    toast(t('friendAccepted'), 'ok');
    await loadFriendState(true);
  } else {
    toast(t('friendError'), 'err');
  }
}

async function onFriendRemove(id: string): Promise<void> {
  if (await removeFriend(id)) {
    await loadFriendState(true);
  } else {
    toast(t('friendError'), 'err');
  }
}

async function onFriendInvite(id: string): Promise<void> {
  if (!session?.room) return;
  const ok = await inviteFriendToCall(id, session.room);
  toast(ok ? t('friendInvited') : t('friendError'), ok ? 'ok' : 'err');
}

/** Map a server error string to a localized message for the add-by-email form. */
function friendAddErrorText(err: string): string {
  if (err.includes('no user')) return t('friendErrNotFound');
  if (err.includes('already friends')) return t('friendErrAlready');
  if (err.includes('already sent')) return t('friendErrPending');
  if (err.includes('yourself')) return t('friendErrSelf');
  return t('friendError');
}

/** "Add friend" from the in-call participant list (by the peer's account id). */
async function addFriendByPeer(userId: string, btn: HTMLButtonElement): Promise<void> {
  btn.disabled = true;
  const err = await sendFriendRequest({ userId });
  if (err === null) {
    friendOutgoingIds.add(userId); // optimistic: the button drops on next render
    toast(t('friendRequestSent'), 'ok');
    await loadFriendState(false);
  } else {
    btn.disabled = false;
    toast(friendAddErrorText(err), 'err');
  }
}

/** The add/remove-friend control on a participant's video tile (sits next to the
 *  report + block buttons). It reflects the live relationship — add, pending (cancel),
 *  incoming (accept), or friends (remove) — and rebuilds itself after each action via
 *  loadFriendState → refreshTileFriendButtons. `uid` is the peer's account id. */
function tileFriendButton(uid: string): HTMLButtonElement {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'cell-action cell-action-friend';

  let label: string;
  let glyph: string;
  let positive = false;
  let isFriend = false;
  let action: () => Promise<void>;
  if (friendIds.has(uid)) {
    // Already friends: show a clear, solid friend badge (accent, not a neutral "add"
    // control); the label/hover still offers removal.
    label = t('friendRemove');
    glyph = 'user-check';
    isFriend = true;
    action = async () => void (await removeFriend(uid));
  } else if (friendIncomingIds.has(uid)) {
    label = t('friendAccept');
    glyph = 'user-plus';
    positive = true;
    action = async () => {
      if (await acceptFriend(uid)) toast(t('friendAccepted'), 'ok');
    };
  } else if (friendOutgoingIds.has(uid)) {
    label = t('friendCancel'); // request sent, still pending → click cancels it
    glyph = 'timer';
    action = async () => void (await removeFriend(uid));
  } else {
    label = t('friendAdd');
    glyph = 'user-plus';
    positive = true;
    action = async () => {
      const err = await sendFriendRequest({ userId: uid });
      toast(err === null ? t('friendRequestSent') : friendAddErrorText(err), err === null ? 'ok' : 'err');
    };
  }

  btn.title = label;
  btn.setAttribute('aria-label', label);
  btn.innerHTML = icon(glyph, 15);
  if (positive) btn.classList.add('friend-pos'); // accent (not danger) hover for add/accept
  if (isFriend) btn.classList.add('is-friend'); // solid accent badge = "this peer is your friend"
  btn.addEventListener('click', (e) => {
    e.stopPropagation(); // don't trigger the cell's pin/spotlight click
    void (async () => {
      btn.disabled = true;
      await action();
      await loadFriendState(false); // refreshes the id sets and rebuilds this button
    })();
  });
  return btn;
}

/** Repaint the friend button on every open video tile so it matches the current
 *  relationships (called whenever friend state changes while in a call). */
function refreshTileFriendButtons(): void {
  for (const cell of videoGrid.querySelectorAll<HTMLElement>('.video-cell')) {
    const id = cell.dataset.peer;
    const actions = cell.querySelector('.cell-actions');
    if (!id || !actions) continue;
    const uid = auth.isLoggedIn() ? peerNames.get(id)?.userId : undefined;
    const existing = actions.querySelector('.cell-action-friend');
    // No account, or it's MY OWN account on another tile → no friend control.
    if (!uid || uid === auth.getUser()?.id) {
      existing?.remove();
      continue;
    }
    const fresh = tileFriendButton(uid);
    if (existing) existing.replaceWith(fresh);
    else actions.insertBefore(fresh, actions.firstChild); // friend button leads the row
  }
}

/** Add-by-email submit for the Account → Friends section: send the request, show a clear
 *  "sent / pending" (green) or error (red) message, then refresh every list so the new
 *  request immediately appears under "Sent requests". */
async function submitFriendAdd(emailInput: HTMLInputElement, msgEl: HTMLElement): Promise<void> {
  const email = emailInput.value.trim();
  if (!email) return;
  const err = await sendFriendRequest({ email });
  if (err === null) {
    emailInput.value = '';
    msgEl.textContent = t('friendRequestSent');
    msgEl.classList.add('ok');
    msgEl.classList.remove('error');
  } else {
    msgEl.textContent = friendAddErrorText(err);
    msgEl.classList.add('error');
    msgEl.classList.remove('ok');
  }
  show(msgEl, true);
  await loadFriendState(true);
}

// ============================================================================
// ---- Account screen: sections + notification preferences + in-app banner (spec: friends) --
const accountScreen = $('account');
const profileAvatar = $('profile-avatar');
const profileName = $('profile-name');
const profileEmail = $('profile-email');
const profileFriendsList = $('profile-friends-list');
const profileFriendsEmpty = $('profile-friends-empty');
const profileFriendAddForm = $<HTMLFormElement>('profile-friend-add');
const profileFriendAddEmail = $<HTMLInputElement>('profile-friend-add-email');
const profileFriendAddMsg = $('profile-friend-add-msg');
const profileFriendRequestsSection = $('profile-friend-requests-section');
const profileFriendRequestsList = $('profile-friend-requests-list');
const profileFriendOutgoingSection = $('profile-friend-outgoing-section');
const profileFriendOutgoingList = $('profile-friend-outgoing-list');
const notifPrefsEl = $('notif-prefs');

// Account section nav (spec: account hub). Each id matches a `data-acct-section` (the
// on-screen rail) / `data-acct-nav` (the avatar menu) value, so both entry points route
// to the same section.
type AccountSection = 'profile' | 'billing' | 'friends' | 'notifications' | 'privacy' | 'workspace';
const accountSectionEls: Record<AccountSection, HTMLElement> = {
  profile: $('acct-profile'),
  billing: $('acct-billing'),
  friends: $('acct-friends'),
  notifications: $('acct-notifications'),
  privacy: $('acct-privacy'),
  workspace: $('acct-workspace'),
};
const accountTabs = Array.from(
  document.querySelectorAll<HTMLButtonElement>('.acct-tab[data-acct-section]'),
);
let currentAccountSection: AccountSection = 'profile';

$('account-back').innerHTML = icon('chevron-left', 18);

/** True when the Account screen is open on its Friends section, so friend-state polls
 *  know to repaint the lists in place. */
function accountFriendsVisible(): boolean {
  return !accountScreen.classList.contains('hidden') && currentAccountSection === 'friends';
}

/** The Account → Friends section's three lists: incoming requests (accept/reject),
 *  outgoing/pending requests (cancel), and accepted friends (remove). */
function renderProfileFriends(friends: Friend[], incoming: Friend[], outgoing: Friend[]): void {
  renderFriendLists(
    {
      incomingSection: profileFriendRequestsSection,
      incomingList: profileFriendRequestsList,
      outgoingSection: profileFriendOutgoingSection,
      outgoingList: profileFriendOutgoingList,
      friendsList: profileFriendsList,
      friendsEmpty: profileFriendsEmpty,
    },
    friends,
    incoming,
    outgoing,
  );
}

const NOTIF_EVENT_LABEL: Record<string, string> = {
  friend_request: 'notifEvtFriendRequest',
  friend_accepted: 'notifEvtFriendAccepted',
  call_invite: 'notifEvtCallInvite',
  friend_active: 'notifEvtFriendActive',
  meeting_invited: 'notifEvtMeetingInvited',
  meeting_reminder: 'notifEvtMeetingReminder',
  meeting_updated: 'notifEvtMeetingUpdated',
  meeting_cancelled: 'notifEvtMeetingCancelled',
};
const NOTIF_CHANNEL_LABEL: Record<string, string> = {
  push: 'notifChPush',
  email: 'notifChEmail',
  in_app: 'notifChInApp',
};

/** Build the event×channel toggle matrix into #notif-prefs. */
function buildNotifMatrix(prefs: Prefs): void {
  const isOn = (type: string, ch: string) =>
    prefs.preferences.find((p) => p.type === type && p.channel === ch)?.enabled ?? true;
  const grid = document.createElement('div');
  grid.className = 'notif-grid';
  const corner = document.createElement('span');
  corner.className = 'notif-h lead';
  grid.appendChild(corner);
  for (const ch of NOTIF_CHANNELS) {
    const h = document.createElement('span');
    h.className = 'notif-h';
    h.textContent = t(NOTIF_CHANNEL_LABEL[ch]);
    grid.appendChild(h);
  }
  for (const type of NOTIF_EVENTS) {
    const lbl = document.createElement('span');
    lbl.className = 'notif-evt';
    lbl.textContent = t(NOTIF_EVENT_LABEL[type]);
    grid.appendChild(lbl);
    for (const ch of NOTIF_CHANNELS) {
      const cell = document.createElement('label');
      cell.className = 'notif-cell';
      const cb = document.createElement('input');
      cb.type = 'checkbox';
      cb.checked = isOn(type, ch);
      cb.addEventListener('change', () => void onPrefToggle(type, ch, cb));
      cell.appendChild(cb);
      grid.appendChild(cell);
    }
  }
  notifPrefsEl.innerHTML = '';
  notifPrefsEl.appendChild(grid);
}

async function onPrefToggle(type: string, channel: string, cb: HTMLInputElement): Promise<void> {
  const enabled = cb.checked;
  // Turning a push channel ON needs browser permission + a live subscription.
  if (channel === 'push' && enabled) {
    const ok = await enablePush();
    if (!ok) {
      cb.checked = false;
      toast(t('pushBlocked'), 'err');
      return;
    }
  }
  const saved = await savePreferences([{ type, channel, enabled }]);
  if (!saved) {
    cb.checked = !enabled; // revert on failure
    toast(t('notifSaveErr'), 'err');
  } else {
    toast(t('notifSaved'), 'ok');
  }
}

async function loadNotifPrefs(): Promise<void> {
  const prefs = await fetchPreferences();
  if (prefs) buildNotifMatrix(prefs);
}

/** Switch the visible Account section, sync the tab selection, and lazy-load that
 *  section's data (friends list, notif prefs, billing history, voice-clone state). */
function selectAccountSection(section: AccountSection): void {
  currentAccountSection = section;
  for (const tab of accountTabs)
    tab.setAttribute('aria-selected', String(tab.dataset.acctSection === section));
  for (const key of Object.keys(accountSectionEls) as AccountSection[])
    accountSectionEls[key].classList.toggle('hidden', key !== section);

  switch (section) {
    case 'friends':
      profileFriendAddEmail.value = '';
      show(profileFriendAddMsg, false);
      void loadFriendState(true); // section visible → paint the lists
      break;
    case 'notifications':
      void loadNotifPrefs();
      break;
    case 'billing':
      selectTab('history'); // (re)load the credits ledger by default
      break;
    case 'privacy':
      $('privacy-status').textContent = '';
      // Enhanced voice-clone status (spec 0108): per-device signal that this user cloned
      // their voice here. The server holds the authoritative `cartesia_voice_id`.
      $('voice-clone-state').textContent = localStorage.getItem(VOICE_CLONED_KEY)
        ? t('voiceCloneStatusSaved')
        : t('voiceCloneStatusNone');
      break;
    case 'workspace':
      void setupWorkspaceVoice();
      break;
  }
}

/** Open the Account screen on `section` (default Profile). Reached from the avatar menu. */
function openAccount(section: AccountSection = 'profile'): void {
  const u = auth.getUser();
  fillAvatar(profileAvatar, u?.name || '', u?.avatar_url, 56, 2);
  profileName.textContent = u?.name || '';
  profileEmail.textContent = u?.email || '';
  homeScreen.classList.add('hidden');
  accountScreen.classList.remove('hidden');
  selectAccountSection(section);
}
function closeAccount(): void {
  accountScreen.classList.add('hidden');
  homeScreen.classList.remove('hidden');
}
for (const tab of accountTabs)
  tab.addEventListener('click', () => selectAccountSection(tab.dataset.acctSection as AccountSection));
$('account-back').addEventListener('click', closeAccount);
profileFriendAddForm.addEventListener('submit', (e) => {
  e.preventDefault();
  void submitFriendAdd(profileFriendAddEmail, profileFriendAddMsg);
});

// ---- In-app actionable banner (friend in a public room / call invite) ----
const friendBanner = $('friend-banner');
const friendBannerText = $('friend-banner-text');
const shownNotifIds = new Set<string>();
let bannerNotif: InAppNotification | null = null;
let friendNotifTimer: number | null = null;

function hideNotifBanner(): void {
  bannerNotif = null;
  show(friendBanner, false);
}
function showNotifBanner(n: InAppNotification): void {
  bannerNotif = n;
  shownNotifIds.add(n.id);
  friendBannerText.textContent = n.body || n.title;
  show(friendBanner, true);
}

async function pollNotifications(): Promise<void> {
  if (!auth.isLoggedIn() || session) return; // never mid-call (session is set in a call)
  // Keep the pending-request badges (and the Account → Friends list, if it's on screen)
  // fresh without a reload, so an incoming request lights up the avatar badge on its own.
  void loadFriendState(false);
  if (bannerNotif) return; // show one at a time
  const list = await fetchUnread(10);
  const next = list.find(
    (n) => (n.type === 'friend_active' || n.type === 'call_invite') && !shownNotifIds.has(n.id),
  );
  if (next) showNotifBanner(next);
}
function startNotifPolling(): void {
  void pollNotifications();
  if (!friendNotifTimer) friendNotifTimer = window.setInterval(() => void pollNotifications(), 20000);
}
function stopNotifPolling(): void {
  if (friendNotifTimer) {
    clearInterval(friendNotifTimer);
    friendNotifTimer = null;
  }
  hideNotifBanner();
}

$('friend-banner-join').addEventListener('click', () => {
  const n = bannerNotif;
  hideNotifBanner();
  if (!n) return;
  void markRead(n.id);
  const room = typeof n.data.room === 'string' ? n.data.room : '';
  if (room) {
    accountScreen.classList.add('hidden');
    void goPrejoin(room, true);
  } else if (typeof n.data.join_url === 'string') {
    // Only navigate to an http(s) URL — never a `javascript:`/`data:` scheme, in case
    // a notification's join_url is ever influenced by another user (defense-in-depth).
    try {
      const target = new URL(n.data.join_url, window.location.origin);
      if (target.protocol === 'https:' || target.protocol === 'http:') {
        location.href = target.href;
      }
    } catch {
      /* malformed URL — ignore */
    }
  }
});
$('friend-banner-dismiss').addEventListener('click', () => {
  const n = bannerNotif;
  hideNotifBanner();
  if (n) void markRead(n.id);
});

$('low-banner-buy').addEventListener('click', openBuyModal);
$('tab-history').addEventListener('click', () => selectTab('history'));
$('tab-usage').addEventListener('click', () => selectTab('usage'));
$('tab-transcripts').addEventListener('click', () => selectTab('transcripts'));
$('exhausted-dismiss').addEventListener('click', () => show(exhaustedModal, false));
$('exhausted-buy').addEventListener('click', () => {
  show(exhaustedModal, false);
  if (exhaustedIsGuest) {
    // Guests can't buy — send them to the login gate to continue with an account.
    leaveCall();
    showLogin();
  } else {
    openBuyModal();
  }
});

// ============================================================================
// Trust & safety + GDPR
// ============================================================================
// --- Age + ToS consent gate ---
function syncConsentAccept(): void {
  const ok =
    $<HTMLInputElement>('consent-age').checked && $<HTMLInputElement>('consent-tos').checked;
  $<HTMLButtonElement>('consent-accept').disabled = !ok;
}
$('consent-age').addEventListener('change', syncConsentAccept);
$('consent-tos').addEventListener('change', syncConsentAccept);
$('consent-accept').addEventListener('click', async () => {
  const status = $('consent-status');
  status.textContent = '';
  if (!auth.isLoggedIn()) {
    // Guest: record the 18+/ToS attestation locally (no server account to update).
    auth.setGuestConsent();
    show(consentModal, false);
    autoStartHomeWizard();
    return;
  }
  if (await auth.submitConsent(true)) {
    show(consentModal, false);
    renderAccount();
    autoStartHomeWizard();
  } else {
    status.textContent = t('consentFailed');
    status.classList.add('error');
  }
});
$('consent-decline').addEventListener('click', () => {
  show(consentModal, false);
  auth.clearSession();
  accountBar.classList.add('hidden');
  showLogin();
});

// --- Privacy & data (GDPR) — now the Account → Privacy section (opened via the avatar
//     menu; voice-clone state is refreshed in selectAccountSection). ---
$('export-data').addEventListener('click', async () => {
  const data = await auth.exportData();
  if (!data) {
    $('privacy-status').textContent = t('exportFailed');
    return;
  }
  const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
  auth.downloadBlob(blob, 'voxtranslate-data.json');
});
$('delete-account').addEventListener('click', async () => {
  if (!confirm(t('deleteConfirm'))) return;
  if (await auth.deleteAccount()) {
    accountScreen.classList.add('hidden');
    accountBar.classList.add('hidden');
    showLogin();
  } else {
    $('privacy-status').textContent = t('deleteFailed');
  }
});

// --- Report a peer ---
const REPORT_REASONS = ['harassment', 'hate', 'sexual', 'spam', 'other'];
function openReport(peerId: string, name: string): void {
  reportTargetId = peerId;
  $('report-target').textContent = name;
  $('report-status').textContent = '';
  const list = $('report-reasons');
  list.innerHTML = '';
  for (const r of REPORT_REASONS) {
    const btn = document.createElement('button');
    btn.className = 'report-reason';
    btn.type = 'button';
    btn.textContent = t(`reason_${r}`);
    btn.addEventListener('click', () => void submitReport(r));
    list.appendChild(btn);
  }
  show(reportModal, true);
}
async function submitReport(reason: string): Promise<void> {
  const name = peerNames.get(reportTargetId)?.name || '';
  const ok = await auth.reportUser({
    room: session?.room || '',
    reported_peer_id: reportTargetId,
    reported_name: name,
    reason,
  });
  $('report-status').textContent = ok ? t('reportThanks') : t('reportFailed');
  if (ok) setTimeout(() => show(reportModal, false), 1200);
}
$('report-close').addEventListener('click', () => show(reportModal, false));
reportModal.addEventListener('click', (e) => {
  if (e.target === reportModal) show(reportModal, false);
});

// --- Block a peer locally (mute + hide for me only) ---
function toggleBlock(peerId: string): void {
  if (blockedPeers.has(peerId)) blockedPeers.delete(peerId);
  else blockedPeers.add(peerId);
  applyBlocked(peerId);
  applyAudioMode();
}
function applyBlocked(peerId: string): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(peerId)}"]`);
  if (!cell) return;
  const blocked = blockedPeers.has(peerId);
  cell.classList.toggle('blocked', blocked);
  setCameraOff(peerId, blocked || (peerCamOff.get(peerId) ?? false));
}

// --- Cookie / processing banner ---
function initCookieBanner(): void {
  let accepted = false;
  try {
    accepted = localStorage.getItem('vox.cookie') === '1';
  } catch {
    /* storage blocked */
  }
  // Consent Mode v2: load the tag immediately (consent denied by default) so it's
  // detectable + GDPR-safe; grant analytics consent only once the user accepts.
  initAnalytics();
  if (accepted) grantAnalyticsConsent();
  else show(cookieBanner, true);
  $('cookie-accept').addEventListener('click', () => {
    try {
      localStorage.setItem('vox.cookie', '1');
    } catch {
      /* ignore */
    }
    show(cookieBanner, false);
    grantAnalyticsConsent();
  });
}

// ---- Boot ------------------------------------------------------------------
window.addEventListener('resize', layoutVideos);
window.addEventListener('orientationchange', () => setTimeout(layoutVideos, 200));
document.addEventListener('fullscreenchange', setControlState);
$('dice').innerHTML = icon('shuffle', 18);
$('chat-close').innerHTML = icon('close', 16);
$('chat-send').innerHTML = icon('send', 20);
chatAttach.innerHTML = icon('paperclip', 20);
chatRecord.innerHTML = icon('mic', 20);
$('chat-rec-send').innerHTML = icon('send', 20);
$('chat-rec-cancel').innerHTML = icon('trash', 18);
$('buy-close').innerHTML = icon('close', 16);
$('sm-cancel-btn').innerHTML = icon('close', 16);
$('report-close').innerHTML = icon('close', 16);
$('audio-close').innerHTML = icon('close', 16);
$('part-close').innerHTML = icon('close', 16);
// Account bar + menu + section tabs (spec: account hub): every `[data-ico]` span gets its
// glyph here so the label text beside it stays intact (icons live in their own span).
for (const el of document.querySelectorAll<HTMLElement>('[data-ico]'))
  el.innerHTML = icon(el.dataset.ico!, el.classList.contains('balance-ico') ? 15 : 18);
$('invite-close').innerHTML = icon('close', 16); // was missing → empty pill (spec 0090)
$('prejoin-audio-btn').innerHTML = icon('headphones', 18);
$('postcall-close').innerHTML = icon('close', 16);
$('btn-bookmark').innerHTML = icon('bookmark');
$('bookmarks-close').innerHTML = icon('close', 16);
initBookmarks({ layout: layoutVideos }); // panel toggles re-flow the video grid
$('btn-glossary-home').innerHTML = icon('book', 18);
$('glossary-close').innerHTML = icon('close', 16);
initGlossary({ show }); // app's show() gives the modal its focus trap

// ---- Whiteboard wiring (spec 0045 → advanced in 0062 / #96) ----
$('btn-whiteboard').innerHTML = icon('board');
const WB_TOOL_ICON: Record<WbTool, string> = {
  pen: 'pencil',
  highlighter: 'highlighter',
  eraser: 'eraser',
  line: 'line',
  arrow: 'arrow',
  rect: 'square',
  ellipse: 'circle',
};
const wbToolBtns = wbOverlay.querySelectorAll<HTMLButtonElement>('.wb-tool[data-tool]');
wbToolBtns.forEach((b) => (b.innerHTML = icon(WB_TOOL_ICON[b.dataset.tool as WbTool], 20)));
$('wb-clear').innerHTML = icon('trash', 20);
$('wb-export').innerHTML = icon('download', 20);
$('wb-close').innerHTML = icon('close', 18);
$('mg-close').innerHTML = icon('close', 18);
$('quiz-close').innerHTML = icon('close', 18);
$('wb-page-prev').innerHTML = icon('chevron-left', 18);
$('wb-page-next').innerHTML = icon('chevron-right', 18);
$('wb-page-add').innerHTML = icon('plus', 18);
$('wb-page-dup').innerHTML = icon('copy', 18);
$('wb-page-del').innerHTML = icon('trash', 18);

function toggleWhiteboard(open?: boolean): void {
  const show = open ?? wbOverlay.classList.contains('hidden');
  wbOverlay.classList.toggle('hidden', !show);
  if (!show) setWbExportOpen(false);
  // Size the canvas on the NEXT frame, not synchronously: right after un-hiding, the
  // stage isn't always laid out on mobile (dynamic viewport / the ⋯ menu still
  // collapsing), so clientWidth/Height can read 0 and resize() bails — leaving a blank
  // 0×0 board until a window resize. One rAF guarantees a settled, non-zero stage. (#71)
  if (show) {
    renderWbPages(whiteboard.pageCount(), whiteboard.pageIndex()); // sync the strip on open
    requestAnimationFrame(() => whiteboard.resize());
    // Add the board to an in-progress recording so its strokes are captured (#230).
    if (isRecording) recorder?.addParticipant(whiteboardRecordingSource());
  } else {
    recorder?.removeParticipant(WB_RECORDING_ID); // board closed → drop its tile from the recording
  }
}
function setWbTool(tool: WbTool): void {
  whiteboard.tool = tool;
  wbToolBtns.forEach((b) => b.classList.toggle('active', b.dataset.tool === tool));
}
function setWbWidth(width: WbWidth): void {
  whiteboard.widthKey = width;
  wbOverlay.querySelectorAll<HTMLButtonElement>('.wb-width').forEach((b) => b.classList.toggle('active', b.dataset.width === width));
}
// Repaint the page strip + enable/disable nav (called by the board on any page change).
function renderWbPages(count: number, index: number): void {
  $('wb-page-label').textContent = `${index + 1} / ${count}`;
  ($('wb-page-prev') as HTMLButtonElement).disabled = index <= 0;
  ($('wb-page-next') as HTMLButtonElement).disabled = index >= count - 1;
  ($('wb-page-del') as HTMLButtonElement).disabled = count <= 1; // never delete the last page
}

$('btn-whiteboard').addEventListener('click', () => toggleWhiteboard());
$('wb-close').addEventListener('click', () => toggleWhiteboard(false));
$('wb-clear').addEventListener('click', () => whiteboard.clearPage());
wbToolBtns.forEach((b) => b.addEventListener('click', () => setWbTool(b.dataset.tool as WbTool)));
wbOverlay.querySelectorAll<HTMLButtonElement>('.wb-width').forEach((b) => {
  b.addEventListener('click', () => setWbWidth(b.dataset.width as WbWidth));
});
wbOverlay.querySelectorAll<HTMLButtonElement>('.wb-color').forEach((b) => {
  b.addEventListener('click', () => {
    whiteboard.color = b.dataset.color || '#f1f5f9';
    wbOverlay.querySelectorAll('.wb-color').forEach((c) => c.classList.toggle('active', c === b));
  });
});

// Page strip nav (local) + structural ops (relayed).
$('wb-page-prev').addEventListener('click', () => whiteboard.prevPage());
$('wb-page-next').addEventListener('click', () => whiteboard.nextPage());
$('wb-page-add').addEventListener('click', () => whiteboard.addPage());
$('wb-page-dup').addEventListener('click', () => whiteboard.duplicatePage());
$('wb-page-del').addEventListener('click', () => whiteboard.deleteCurrentPage());

// Export menu (PNG / PDF) — a small popover; outside-click / Escape close it.
const wbExportMenu = $('wb-export-menu');
const wbExportWrap = wbOverlay.querySelector<HTMLElement>('.wb-export-wrap')!;
function setWbExportOpen(open: boolean): void {
  wbExportMenu.classList.toggle('hidden', !open);
  $('wb-export').setAttribute('aria-expanded', String(open));
}
$('wb-export').addEventListener('click', (e) => {
  e.stopPropagation();
  setWbExportOpen(wbExportMenu.classList.contains('hidden'));
});
document.addEventListener('click', (e) => {
  if (!wbExportMenu.classList.contains('hidden') && !wbExportWrap.contains(e.target as Node)) setWbExportOpen(false);
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !wbExportMenu.classList.contains('hidden')) setWbExportOpen(false);
});
$('wb-export-png').addEventListener('click', () => {
  setWbExportOpen(false);
  void whiteboard.exportPng();
});
$('wb-export-pdf').addEventListener('click', () => {
  setWbExportOpen(false);
  whiteboard.exportPdf();
});

window.addEventListener('resize', () => {
  if (!wbOverlay.classList.contains('hidden')) whiteboard.resize();
});

// ---- Mini-game wiring (spec 0046) ----
$('btn-minigame').innerHTML = icon('game');
function toggleMinigame(open?: boolean): void {
  const show = open ?? minigameEl.classList.contains('hidden');
  minigameEl.classList.toggle('hidden', !show);
}
$('btn-minigame').addEventListener('click', () => toggleMinigame());
$('mg-close').addEventListener('click', () => toggleMinigame(false));

// ---- Quiz wiring (spec 0047) ----
$('btn-quiz').innerHTML = icon('quiz');
function toggleQuiz(open?: boolean): void {
  const show = open ?? quizEl.classList.contains('hidden');
  quizEl.classList.toggle('hidden', !show);
  if (show) void refreshQuizCost(); // estimate reflects the room's languages right now
}
$('btn-quiz').addEventListener('click', () => toggleQuiz());
$('quiz-close').addEventListener('click', () => toggleQuiz(false));

// AI quiz on demand (spec 0067 / #124): prompt → Groq → credits → play via the
// existing host-authoritative engine. The generated pack rides in the relayed
// state, so peers and late-joiners get it.
const quizAiForm = $('quiz-ai') as HTMLFormElement;
const quizAiInput = $('quiz-ai-prompt') as HTMLInputElement;
const quizAiBtn = $('quiz-ai-gen') as HTMLButtonElement;
const quizAiMsg = $('quiz-ai-msg');
const quizAiBuy = $('quiz-ai-buy');
quizAiBuy.addEventListener('click', openBuyModal); // out-of-credits → purchase modal (spec 0083)
const quizAiCost = $('quiz-ai-cost');
const QUIZ_Q_COUNT = 5; // questions per generated quiz (matches the server clamp default)

/** Distinct languages currently in the room (mine + each peer's), minus auto. The
 *  quiz is localized into these, and the cost scales with how many there are. */
function roomLangCodes(): string[] {
  return Array.from(
    new Set([session?.lang || 'en', ...Array.from(peerNames.values()).map((p) => p.lang)]),
  ).filter((l) => l && l !== 'auto');
}

/** Show the estimated quiz cost, scaled by the room's distinct languages (the quiz
 *  is localized for everyone). Language-neutral (≈ price · 🌐 N) so no new strings. */
async function refreshQuizCost(): Promise<void> {
  if (!quizAiCost || !billing) {
    if (quizAiCost) quizAiCost.hidden = true;
    return;
  }
  const q = (await fetchAiPricing())?.quiz;
  if (!q) {
    quizAiCost.hidden = true;
    return;
  }
  const n = Math.max(1, roomLangCodes().length);
  quizAiCost.textContent = `≈ ${auth.formatCredits(q.base + q.per_question * QUIZ_Q_COUNT * n)} · 🌐 ${n}`;
  quizAiCost.hidden = false;
}

function setQuizAiMsg(text: string, isError: boolean): void {
  quizAiMsg.textContent = text;
  quizAiMsg.classList.toggle('error', isError);
  quizAiMsg.hidden = !text;
}
quizAiForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  const prompt = quizAiInput.value.trim();
  if (!prompt) return;
  // Generating a quiz needs an account + credits (the server requires auth and
  // charges per quiz). Gate guests with the sign-in CTA instead of a dead-end
  // error; the buy CTA below covers the out-of-credits case (spec 0083).
  if (billing && !auth.isLoggedIn()) {
    openSigninGate();
    return;
  }
  // Block a second quiz while one is live — BEFORE spending credits on Groq (R4.2).
  if (quiz.isActive()) {
    setQuizAiMsg(t('quizBusy'), true);
    return;
  }
  show(quizAiBuy, false); // clear the buy CTA from any previous attempt
  quizAiBtn.disabled = true;
  quizAiInput.disabled = true;
  setQuizAiMsg(t('quizAiGenerating'), false);
  // Localize the quiz for everyone: the room's distinct languages. The server
  // translates into these and charges per language (matching the shown estimate).
  const res = await generateAiQuiz(prompt, QUIZ_Q_COUNT, session?.lang || 'en', roomLangCodes());
  quizAiBtn.disabled = false;
  quizAiInput.disabled = false;
  if (res.ok && res.quiz) {
    quiz.startAiQuiz(res.quiz.questions);
    quizAiInput.value = '';
    setQuizAiMsg('', false);
    if (typeof res.quiz.balance === 'number') {
      auth.setBalance(res.quiz.balance);
      setBalanceUi(res.quiz.balance);
    }
  } else if (res.reason === 'insufficient_credits') {
    setQuizAiMsg(t('quizAiNoCredits'), true);
    show(quizAiBuy, true); // tell them HOW to get credits
  } else {
    setQuizAiMsg(t('quizAiError'), true);
  }
});

// ---- Voice-command timer wiring (spec 0052) ----
const btnTimer = $('btn-timer');
const timerPop = $('timer-pop');
btnTimer.innerHTML = icon('timer');
$('timer-cancel').innerHTML = icon('close', 13);
($('timer-badge').querySelector('.ti-ico') as HTMLElement).innerHTML = icon('timer', 15);

function toggleTimerPop(open?: boolean): void {
  const show = open ?? timerPop.classList.contains('hidden');
  timerPop.classList.toggle('hidden', !show);
  if (show) $<HTMLInputElement>('timer-custom-input').focus();
}
btnTimer.addEventListener('click', (e) => {
  e.stopPropagation(); // don't let the just-opened popover see this as an outside click
  if (timerPop.classList.contains('hidden')) setMoreOpen(false); // collapse the ⋯ menu first
  toggleTimerPop();
});
// Explicit close X (top-right), matching every other modal's close affordance.
$('timer-pop-close').innerHTML = icon('close', 14);
$('timer-pop-close').addEventListener('click', () => toggleTimerPop(false));
// Close the popover on an outside click or Escape (mirrors the ⋯ menu).
document.addEventListener('click', (e) => {
  if (
    !timerPop.classList.contains('hidden') &&
    !timerPop.contains(e.target as Node) &&
    !btnTimer.contains(e.target as Node)
  )
    toggleTimerPop(false);
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !timerPop.classList.contains('hidden')) toggleTimerPop(false);
});

// Quick-pick chips → start that many whole minutes.
timerPop.querySelectorAll<HTMLButtonElement>('.timer-chip').forEach((chip) => {
  chip.addEventListener('click', () => {
    const min = Number(chip.dataset.min || '0');
    if (min > 0) callTimer.start({ seconds: min * 60, isBreak: false });
    toggleTimerPop(false);
  });
});
// Custom minutes (1–360) → start on the button or Enter.
function startCustomTimer(): void {
  const input = $<HTMLInputElement>('timer-custom-input');
  const min = Math.floor(Number(input.value));
  if (!Number.isFinite(min) || min < 1) return;
  callTimer.start({ seconds: Math.min(min, 360) * 60, isBreak: false });
  input.value = '';
  toggleTimerPop(false);
}
$('timer-custom-start').addEventListener('click', startCustomTimer);
$('timer-custom-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    startCustomTimer();
  }
});
// "Change" in the detected-language toast (spec 0012): correct the server,
// then restart capture so the next Deepgram stream opens in the new language.
initLangDetect({
  send: (m) => ws?.send(JSON.stringify(m)),
  restartCapture: () => {
    if (micOn) audioCapture?.restart();
  },
});

// ---- Emoji picker ----------------------------------------------------------
// Two sections: quick reactions (sent to the room, float over the video grid)
// and the full grid (inserts into the chat input at the cursor).
const REACTION_LIST = ['👍', '❤️', '😂', '👏', '🎉', '😮'];
const EMOJI_LIST = ['👍','❤️','😂','😮','😢','👏','🎉','🔥','💯','✅','🤔','😍','🙌','💪','🤝','😊','🥳','😎','🤬','👎'];
const emojiToggle = $('emoji-toggle');
const emojiPanel = $('emoji-panel');
const emojiReact = $('emoji-react');
const emojiGrid = $('emoji-grid');

// Cap reaction bursts so a held / runaway click can't flood the room while the
// panel stays open (issue #15): at most 5 reactions per second, excess dropped.
const reactionLimiter = new RateLimiter(5, 1000);

for (const em of REACTION_LIST) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.textContent = em;
  btn.addEventListener('click', (e) => {
    e.stopPropagation(); // keep the panel open so reactions can be sent in a row
    sendEmoji(em);
  });
  emojiReact.appendChild(btn);
}

// Quick reactions in the control bar (spec 0055): a 4-emoji subset, one tap each,
// no panel to open — reuse the rate-limited sendEmoji so bursts past 5/s drop.
const quickReactions = $('quick-reactions');
for (const em of REACTION_LIST.slice(0, 4)) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'react-btn';
  btn.textContent = em;
  btn.addEventListener('click', () => sendEmoji(em));
  quickReactions.appendChild(btn);
}

for (const em of EMOJI_LIST) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.textContent = em;
  btn.addEventListener('click', (e) => {
    e.stopPropagation(); // keep the panel open so multiple emojis can be added
    insertEmoji(em);
  });
  emojiGrid.appendChild(btn);
}

function setEmojiPanelOpen(open: boolean): void {
  emojiPanel.classList.toggle('hidden', !open);
  emojiToggle.setAttribute('aria-expanded', String(open));
}

emojiToggle.addEventListener('click', (e) => {
  e.stopPropagation();
  setEmojiPanelOpen(emojiPanel.classList.contains('hidden'));
});
document.addEventListener('click', () => setEmojiPanelOpen(false));

function sendEmoji(emoji: string): void {
  if (!reactionLimiter.tryAcquire()) return; // drop bursts past the cap
  ws?.send(JSON.stringify({ type: 'emoji', emoji }));
  track('emoji_reaction_sent', { emoji });
  // Panel intentionally stays open so users can fire multiple reactions quickly.
}

function insertEmoji(emoji: string): void {
  const start = chatInput.selectionStart ?? chatInput.value.length;
  const end = chatInput.selectionEnd ?? start;
  const res = insertAt(chatInput.value, emoji, start, end, chatInput.maxLength);
  if (!res) return; // would exceed the cap
  chatInput.value = res.value;
  chatInput.focus();
  chatInput.setSelectionRange(res.caret, res.caret);
  fitChatInput();
  updateChatCounter();
}

initCookieBanner();
initBugReport(); // always-available "report a problem" button (spec 0071)
// Onboarding "?" launchers + home wizard wiring (spec onboarding). forceMore drives the ⋯ menu
// for the tour's share/invite steps without stealing focus, so it doesn't fight driver.js.
onboarding.initOnboarding({
  show,
  isLoggedIn: auth.isLoggedIn,
  // B2B = member of ≥1 org (any subscription status). Reads the warmed org cache so the
  // webinar-explainer step selection stays synchronous; autoStartHomeWizard() warms it first.
  isB2B: () => (bizOrgs?.length ?? 0) > 0,
  forceMore: (open) => document.body.classList.toggle('onb-more-forced', open),
});
// boot() runs the lobby (startLobby) and resumes any session.
void boot();
