// VoxTranslate V2 client orchestrator: home/lobby → pre-join (camera + devices)
// → WebRTC video call with translated subtitles + chat.

import { applyI18n, detectLang, ENDONYM, FLAG, getUiLang, setUiLang, SUPPORTED, t } from './i18n';
import {
  type EngineInfo,
  commonLangs,
  engineDescKey,
  engineNeedsPcm,
  formatRate,
  loadEnginePref,
  resolveEnginePref,
  saveEnginePref,
} from './engines';
import { loadRemoteI18n } from './content';
import { icon } from './icons';
import { MeshManager } from './webrtc';
import { resolvePeerId } from './peer-id';
import { AudioCapture } from './audio-capture';
import { PcmCapture } from './pcm-capture';
import { pcmPlayback } from './pcm-playback';
import { MicMeter } from './mic-meter';
import { ChatManager, type ChatPayload } from './chat';
import { CHAT_MAX_HEIGHT, counterLabel, counterState, insertAt, resizeBox } from './chat-input';
import { checkUploadFile, fileUploadEnabled, generateAiQuiz, saveQuizHistory, sendInvites, uploadChatFile } from './api';
import { buildInviteLink, MAX_INVITE_EMAILS, parseRoomParam, validateInviteEmails } from './invite';
import * as auth from './auth';
import { openSessionScreen } from './session-screen';
import { initBookmarks, setBookmarkSession } from './bookmarks';
import { initBugReport } from './bug-report';
import { initGlossary, onGlossaryActive, refreshGlossaryHome, setGlossaryRoom } from './glossary';
import { Whiteboard, type WbTool, type WbWidth } from './whiteboard';
import { TicTacToe } from './tictactoe';
import { Quiz } from './quiz';
import { CallTimer, spokenDuration, formatClock } from './timer';
import { dismissLangToast, initLangDetect, onLanguageDetected } from './lang-detect';
import { initNetStatus, setNetworkDegraded } from './net-status';
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
import { VirtualBackground } from './virtual-background';
import { ScreenSharePip } from './screenshare-pip';
import { CompositeRecorder } from './recording/composite-recorder';
import { formatElapsed, isRecordingSupported, recordingFilename } from './recording/utils';
import type { ParticipantSource } from './recording/types';

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
const callBalance = $('call-balance');
const lowBanner = $('low-banner');
const lowBannerText = $('low-banner-text');
const buyModal = $('buy-modal');
const packagesList = $('packages-list');
const ledgerList = $('ledger-list');
const modalBalance = $('modal-balance');
const buyStatus = $('buy-status');
const exhaustedModal = $('exhausted-modal');
const consentModal = $('consent-modal');
const reportModal = $('report-modal');
const privacyModal = $('privacy-modal');
const cookieBanner = $('cookie-banner');

let billing = false; // accounts/credits enabled on this backend
// Translation engines available on this backend (spec 0093). The selector renders
// from this list; `selectedEngine` is the user's persisted choice (default 'standard').
let availableEngines: EngineInfo[] = [];
let selectedEngine = 'standard';
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
const enterBtn = $<HTMLButtonElement>('enter');
const homeStatus = $('home-status');
const visGroup = $('vis-group');
const visHint = $('vis-hint');
const roomsList = $('rooms-list');

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

// Collaborative whiteboard (spec 0045 → advanced in 0062): ops relay over the same
// WS; onPagesChanged repaints the page strip (multi-page, #96).
const wbOverlay = $('whiteboard');
const whiteboard = new Whiteboard(
  $<HTMLCanvasElement>('wb-canvas'),
  (op) => ws?.send(JSON.stringify({ type: 'whiteboard', op })),
  (count, index) => renderWbPages(count, index),
);

// Mini-games (spec 0046/0047): state relays over the same WS `game` channel.
const gameName = (id: string): string =>
  id === myId ? session?.name || t('you') : peerNames.get(id)?.name || '';
const sendGame = (state: unknown): void => ws?.send(JSON.stringify({ type: 'game', state }));
const minigameEl = $('minigame');
// peers() feeds seat assignment + spectator rotation (spec 0070 S3): self first,
// then peers in their join order.
const tictactoe = new TicTacToe(minigameEl, myId, gameName, sendGame, t, () => [
  myId,
  ...peerNames.keys(),
]);
const quizEl = $('quiz');
// Each client renders the quiz in its own language (spec 0048). The modal callback
// opens the quiz for EVERY participant when one starts, and closes it on cancel,
// with a toast (spec 0070 R4.1/R4.3).
const quiz = new Quiz(
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
    if (ttsOn) speak(t('timerSetSpeak').replace('{d}', human), getUiLang());
  },
  onDone: () => {
    toast(t('timerDone'));
    playTimerDoneSound();
    if (ttsOn) speak(t('timerDoneSpeak'), getUiLang());
  },
  onCancel: () => toast(t('timerCancelled')),
});
let audioCapture: AudioCapture | PcmCapture | null = null;
// Speakers whose translated audio arrives from the server (Premium engine, spec
// 0093). We never also browser-TTS them — they'd be heard twice, out of sync.
const premiumSpeakers = new Set<string>();
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
let subtitlesOn = true; // show subtitle overlays on video cells
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
let sessionTimerId = 0; // 1s interval driving the session-duration chip (spec 0055)

const peerNames = new Map<string, { name: string; lang: string; avatar?: string | null }>();
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
applyI18n();
// Discover translation engines + restore the saved choice (spec 0093). Async;
// the selector reveals itself once the list arrives. Default engine until then.
void initEngines();
// Global connection-status banner (offline / reconnecting / back online).
initNetStatus();
langSel.addEventListener('change', () => {
  setUiLang(langSel.value);
  writeCache(LANG_CACHE_KEY, langSel.value);
  applyI18n();
  updateVisHint();
});

function updateVisHint(): void {
  visHint.textContent = visibilityPublic ? '' : t('privateHint');
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
    availableEngines = (await res.json()) as EngineInfo[];
  } catch {
    return; // offline / unreachable → default engine, selector hidden
  }
  selectedEngine = resolveEnginePref(loadEnginePref(), availableEngines);
  renderEngineSelector();
  rebuildLangOptions();
}

function renderEngineSelector(): void {
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
  for (const e of availableEngines) {
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
    const rate = document.createElement('span');
    rate.className = 'engine-opt-rate';
    rate.textContent = formatRate(e.rate_per_minute);
    head.append(name, rate);
    const desc = document.createElement('span');
    desc.className = 'engine-opt-desc';
    // Localized, jargon-free copy keyed by tier; fall back to the server string for
    // an unknown/future engine (#236).
    const descKey = engineDescKey(e.tier);
    desc.textContent = descKey ? t(descKey) : e.description;
    btn.append(head, desc);
    // Transparency (spec 0093): when the rate is per translation stream, say so —
    // a group call with more languages costs more.
    if (e.capabilities.cost_scales_per_language) {
      const note = document.createElement('span');
      note.className = 'engine-opt-note';
      note.textContent = t('engineCostPerLanguage');
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
    applyI18n();
    updateVisHint();
  } else {
    langSel.value = next;
  }
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

function startLobby(): void {
  fetchRooms();
  if (!lobbyTimer) lobbyTimer = window.setInterval(fetchRooms, 3000);
}
function stopLobby(): void {
  if (lobbyTimer) {
    clearInterval(lobbyTimer);
    lobbyTimer = null;
  }
}
$('refresh').addEventListener('click', fetchRooms);

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
  stopLobby();
  homeScreen.classList.add('hidden');
  prejoinScreen.classList.remove('hidden');
  prejoinRoom.textContent = room;
  prejoinVis.textContent = isPublic ? t('public') : t('private');
  prejoinStatus.textContent = '';
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
  } catch {
    // Fall back to audio-only (no camera available / denied video).
    localStream = await navigator.mediaDevices.getUserMedia({ audio });
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
  void startCall();
});

// ============================================================================
// Call
// ============================================================================

// ICE servers for WebRTC (spec 0026): fetched per-call from the server, which
// returns public STUN plus a time-limited TURN relay when coturn is configured.
// Passed to the mesh; falls back to the mesh's built-in STUN on failure.
let iceServers: RTCIceServer[] | undefined;
async function fetchIceServers(): Promise<RTCIceServer[] | undefined> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/ice`, { cache: 'no-store' });
    const data = await res.json();
    return Array.isArray(data?.iceServers) ? (data.iceServers as RTCIceServer[]) : undefined;
  } catch {
    return undefined;
  }
}

async function startCall(): Promise<void> {
  if (!session || !localStream) return;
  prejoinScreen.classList.add('hidden');
  callScreen.classList.remove('hidden');
  callRoom.textContent = session.room;
  callVis.textContent = session.isPublic ? t('public') : t('private');
  // Your name + lang live in the meta row instead of on the self tile (see #stage-self).
  stageSelfName.textContent = session.name || t('you');
  stageSelfLang.textContent = `${FLAG[session.lang] || ''} ${session.lang.toUpperCase()}`.trim();
  show(stageSelf, true);
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
    micMeter = new MicMeter(localStream, (level) =>
      btnMic.style.setProperty('--mic-level', level.toFixed(3)),
    );
  }

  manualClose = false;
  // Fetch ICE servers (incl. the TURN relay if configured) before opening the
  // socket, so the mesh has them ready when peers arrive — no race (spec 0026).
  iceServers = await fetchIceServers();
  openSocket();
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
    mesh = new MeshManager(
      localStream!,
      (sig) => ws?.send(JSON.stringify(sig)),
      iceServers,
      IS_MOBILE ? VIDEO_BUDGET_MOBILE : VIDEO_BUDGET_DESKTOP, // total upload budget, split per-peer (spec 0030/0031, env-tunable 0044)
      myId, // own id → picks the polite/impolite negotiation role per peer
    );
    mesh.onNetworkWeak = showWeakNetworkWarning;
    mesh.onRemoteStream = (peerId, stream) => {
      remoteStreams.set(peerId, stream);
      recorder?.addParticipant(participantSource(peerId, stream));
      attachStream(peerId, stream);
    };
    mesh.onPeerRemoved = (peerId) => removeCell(peerId);
    mesh.setAudioEnabled(micOn);
    mesh.setVideoEnabled(camOn);

    // Speech-to-speech engines (OpenAI, Gemini) capture raw PCM16/24k; Standard
    // streams WebM/Opus for Deepgram. Decide by the engine's `translated_audio`
    // capability — keying on `id === 'premium'` missed the Gemini engine (id
    // `gemini_live_translate`), which then sent WebM that its PCM session read as
    // noise: no transcript, no translated voice.
    audioCapture = engineNeedsPcm(session?.engine, availableEngines)
      ? new PcmCapture(localStream!, ws!)
      : new AudioCapture(localStream!, ws!);
    if (micOn) audioCapture.start();

    // Tell peers if we joined already muted / camera-off so their UI matches.
    if (!micOn) ws?.send(JSON.stringify({ type: 'mute_audio', muted: true }));
    if (!camOn) ws?.send(JSON.stringify({ type: 'mute_video', muted: true }));

    chat = new ChatManager({ myLang: session!.lang, myId, container: chatMessages, ws: ws! });
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
        peerNames.set(p.id, { name: p.user_name, lang: p.lang, avatar: p.avatar_url });
        addCell(p.id, p.user_name, p.lang, false, p.avatar_url);
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
      peerNames.set(msg.peer_id, { name: msg.user_name, lang: msg.lang, avatar: msg.avatar_url });
      addCell(msg.peer_id, msg.user_name, msg.lang, false, msg.avatar_url);
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
      setScreenShareIndicator(msg.peer_id, msg.active);
      applyAudioMode(); // re-evaluate muting: a sharer's audio is never muted, so shared audio is heard (#229)
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
      if (subtitlesOn) showSubtitle(msg.speaker_id, msg.text, true);
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
    case 'engine_downgraded': {
      // A speaker's engine was switched mid-call (spec 0093), e.g. Premium → Standard
      // when credits ran low.
      if (msg.peer_id === myId) {
        // It's us: swap our capture (PCM→WebM) and continue under the new engine.
        if (session) session.engine = msg.to;
        const wasActive = micOn;
        audioCapture?.stop();
        if (ws && localStream) {
          // Match capture to the new engine's format (downgrade is to Standard →
          // WebM today, but stay capability-correct if that ever changes).
          audioCapture = engineNeedsPcm(msg.to, availableEngines)
            ? new PcmCapture(localStream, ws)
            : new AudioCapture(localStream, ws);
          if (wasActive) audioCapture.start();
        }
        showNotif(t(msg.reason === 'premium_at_capacity' ? 'enginePremiumBusy' : 'enginePremiumPaused'));
      } else {
        // A peer downgraded: stop expecting their premium audio so TTS resumes.
        premiumSpeakers.delete(msg.peer_id);
      }
      break;
    }
    case 'subtitle_final': {
      transcriptEvents++;
      const myLang = session?.lang || 'en';
      const text = msg.translations?.[myLang] ?? msg.original;
      if (subtitlesOn) showSubtitle(msg.speaker_id, text, false, msg.original);
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
        leaveCall();
        homeStatusMsg(t('outOfCredits'), true);
        if (billing) openBuyModal();
      } else if (msg.code === 'login_required') {
        // Public rooms require an account; bounce a guest back to the login gate.
        leaveCall();
        homeStatusMsg(t('publicNeedsLogin'), true);
        if (billing) showLogin();
      } else if (msg.code === 'banned') {
        leaveCall();
        homeStatusMsg(msg.message || t('bannedMsg'), true);
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
  overlay.append(nameEl, langEl, mute);
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
      // A screen-sharing peer's audio track may carry shared tab/system audio
      // (music, a video) that everyone should hear — never mute it for the
      // translated-voice setting, or the shared audio is lost (#229).
      const sharing = cell.classList.contains('sharing');
      video.muted = !sharing && !!(ttsOn && peerLang && myLang && peerLang !== myLang);
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
function setScreenShareIndicator(id: string, active: boolean): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(id)}"]`);
  if (!cell) return;
  cell.classList.toggle('sharing', active);
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
  const items: Array<{ id: string; name: string; lang: string; isSelf: boolean; micMuted: boolean; handRaised: boolean; avatar: string | null }> = [];

  items.push({ id: myId, name: myName, lang: myLang, isSelf: true, micMuted: !micOn, handRaised, avatar: myAvatar });
  for (const [id, info] of peerNames) {
    items.push({ id, name: info.name, lang: info.lang, isSelf: false, micMuted: peerMicMuted.get(id) ?? false, handRaised: peerHandRaised.get(id) ?? false, avatar: info.avatar ?? null });
  }

  $('part-count-n').textContent = String(items.length); // live count (spec 0055)
  updateInviteAvailability(items.length); // show "Invite" only while a seat is free (spec 0082)
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

    el.append(avatar, info, status);
    participantsList.appendChild(el);
  }
}

// ---- Subtitles -------------------------------------------------------------
function showSubtitle(speakerId: string, text: string, interim: boolean, original?: string): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(speakerId)}"]`);
  if (!cell) return;
  const area = cell.querySelector('.subtitle-area') as HTMLElement;
  area.innerHTML = '';
  const box = document.createElement('div');
  box.className = `subtitle${interim ? ' subtitle-interim' : ''}`;
  const main = document.createElement('span');
  main.className = 'subtitle-translation';
  main.textContent = text;
  box.appendChild(main);
  if (!interim && original && original !== text) {
    const orig = document.createElement('span');
    orig.className = 'subtitle-original';
    orig.textContent = original;
    box.appendChild(orig);
  }
  area.appendChild(box);

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
  btnSubtitle.classList.toggle('active-success', subtitlesOn);
  btnSubtitle.innerHTML = icon(subtitlesOn ? 'subtitle' : 'subtitle-off');
  setToggleState(btnSubtitle, subtitlesOn, t(subtitlesOn ? 'subtitleTip' : 'subtitleOffTip'));
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

btnSubtitle.addEventListener('click', () => {
  subtitlesOn = !subtitlesOn;
  if (!subtitlesOn) {
    document.querySelectorAll<HTMLElement>('.subtitle-area').forEach((a) => { a.innerHTML = ''; });
  }
  setControlState();
});

function toggleHand(): void {
  handRaised = !handRaised;
  ws?.send(JSON.stringify({ type: 'hand_raise', raised: handRaised }));
  if (handRaised) playHandRaiseSound(); // confirmation cue for the local user
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
  if (!mesh) return;
  try {
    // audio: true makes the browser offer the "share tab/system audio" checkbox
    // (spec 0085). Chrome/Edge desktop only; Firefox/Safari/mobile just ignore it.
    const s = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: true });
    screenStream = s;
    isSharingScreen = true;
    // If the user ticked "share audio", route it to peers. With a mic present we
    // MIX mic + share (peers hear you AND the shared audio); with no mic we send
    // the shared audio alone — previously this was gated on having a mic, so a
    // muted/no-mic sharer transmitted nothing (#229). Either way it leaves on the
    // (now always-present) audio sender via replaceTrack — no renegotiation. No
    // screen-audio track (box unticked / unsupported) → the mic path is untouched.
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
        shareMixTrack = dest.stream.getAudioTracks()[0] ?? null;
        if (shareMixTrack) mesh.replaceAudioTrack(shareMixTrack);
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
    ws?.send(JSON.stringify({ type: 'screen_share', active: true })); // tell peers (spec 0033)
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
  ws?.send(JSON.stringify({ type: 'screen_share', active: false })); // tell peers (spec 0033)
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
    startRecording();
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

function startRecording(): void {
  if (recorder || !localStream) return;
  recorder = new CompositeRecorder({
    sources: recorderSources(),
    // Mid-session failure: stop gracefully and save the chunks collected so far.
    onError: () => void stopRecording(true),
  });
  isRecording = true;
  playRecordingStartSound(); // audible cue that recording has started
  showNotif(t('recording'));
  $('rec-timer').textContent = '00:00';
  show($('rec-badge'), true);
  // Reserve the centre lane for the REC badge so the meta/participant badges can't
  // slide under it (spec 0070 R2.1).
  document.querySelector('.video-stage')?.classList.add('recording');
  recTimerId = window.setInterval(() => {
    if (recorder) $('rec-timer').textContent = formatElapsed(Date.now() - recorder.startedAt);
  }, 1000);
  setControlState();
}

async function stopRecording(partial = false): Promise<void> {
  const rec = recorder;
  if (!rec) return;
  recorder = null;
  isRecording = false;
  clearInterval(recTimerId);
  show($('rec-badge'), false);
  document.querySelector('.video-stage')?.classList.remove('recording');
  setControlState();
  showNotif(t('processing'));
  const blob = await rec.stop();
  if (blob.size > 0) {
    auth.downloadBlob(blob, recordingFilename(session?.room || 'call', new Date()));
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
void fileUploadEnabled().then((on) => {
  if (on) chatAttach.hidden = false;
});

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

$('btn-leave').addEventListener('click', leaveCall);
function leaveCall(): void {
  // Meet-style cue: you left the call — only if we actually joined (callStartedAt
  // stays 0 on a room-full bounce), so it never fires for a non-entry (spec 0024).
  if (callStartedAt > 0) playCallLeaveSound();
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
  activeSessionId = null;
  transcriptEvents = 0;
  callStartedAt = 0;
  clearPendingRemovals(); // drop any in-flight reconnect grace timers (#233)
  show($('transcript-indicator'), false);
  manualClose = true;
  setNetworkDegraded(false); // leaving on purpose — don't show "reconnecting"
  audioCapture?.stop();
  micMeter?.stop();
  micMeter = null;
  // Initiate the recording stop BEFORE tearing down the mesh: the chunks are
  // already collected, so the async Blob assembly survives the cleanup below.
  if (isRecording) void stopRecording();
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

// Translated-voice TTS. Utterances are QUEUED and played one at a time (chained on
// `onend`) — we never cancel the in-progress one, otherwise a quickly-following
// sentence would cut off the previous translation mid-word (the reported bug). A
// generous backlog cap keeps a fast talker from pushing playback minutes behind
// live: past the cap the OLDEST still-waiting lines are dropped so we stay near
// real time (a normal conversation, with pauses, never reaches it).
const ttsQueue: SpeechSynthesisUtterance[] = [];
let ttsSpeaking = false;
const TTS_MAX_QUEUE = 8;

// Pick a voice for `lang`, optimised for the owner's hard priority: MINIMAL DELAY.
// Local/offline voices (localService) start instantly, so they win heavily; among
// those we prefer premium/enhanced ones (Apple "Enhanced", etc.) to sound less
// robotic AT NO LATENCY COST. Network voices (e.g. "Google …") sound natural but
// fetch audio over the wire and add start-up lag, so they're a last resort — only
// when no local voice matches the language at all (spec 0042).
function pickVoice(lang: string): SpeechSynthesisVoice | undefined {
  const want = lang.toLowerCase();
  const matches = speechSynthesis.getVoices().filter((v) => v.lang.toLowerCase().startsWith(want));
  if (!matches.length) return undefined;
  const score = (v: SpeechSynthesisVoice): number =>
    (v.localService ? 100 : 0) + // local = instant; the dominant factor
    (/premium|enhanced|neural|natural|siri/i.test(`${v.name} ${v.voiceURI}`) ? 10 : 0) +
    (v.default ? 1 : 0);
  return matches.reduce((best, v) => (score(v) > score(best) ? v : best));
}

function speak(text: string, lang: string): void {
  if (!window.speechSynthesis) return;
  const u = new SpeechSynthesisUtterance(text);
  const v = pickVoice(lang);
  if (v) u.voice = v;
  u.lang = lang;
  u.rate = 1.1;
  ttsQueue.push(u);
  if (ttsQueue.length > TTS_MAX_QUEUE) ttsQueue.splice(0, ttsQueue.length - TTS_MAX_QUEUE);
  pumpTts();
}

/** Speak the next queued utterance once the current one finishes (or errors). */
function pumpTts(): void {
  if (ttsSpeaking || !window.speechSynthesis) return;
  const u = ttsQueue.shift();
  if (!u) return;
  ttsSpeaking = true;
  const next = () => {
    ttsSpeaking = false;
    pumpTts();
  };
  u.onend = next;
  u.onerror = next;
  speechSynthesis.speak(u);
}

// iOS/WebKit gate speechSynthesis behind a real user gesture: unless the FIRST
// speak() runs inside a tap handler, every later (programmatic) translated-voice
// utterance is silently dropped — the reported "no translated audio on iPhone"
// (Chrome on iOS is WebKit too, so it repros there). Prime the engine from the
// join tap / TTS-toggle with one inaudible utterance so the real translations
// play. No-op where it isn't needed; re-armed on leaveCall via stopTts().
let ttsUnlocked = false;
function unlockTts(): void {
  if (ttsUnlocked || !window.speechSynthesis) return;
  try {
    const u = new SpeechSynthesisUtterance(' ');
    u.volume = 0;
    speechSynthesis.speak(u);
    ttsUnlocked = true;
  } catch {
    /* best-effort: a later real speak() will still attempt to unlock */
  }
}

/** Stop playback and drop the queue (TTS toggled off / leaving the call). */
function stopTts(): void {
  ttsQueue.length = 0;
  ttsSpeaking = false;
  ttsUnlocked = false; // re-prime on the next join / TTS-toggle gesture (iOS)
  if (window.speechSynthesis) speechSynthesis.cancel();
}
if (window.speechSynthesis) speechSynthesis.getVoices();

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
    await auth.refreshMe();
    renderAccount();
    history.replaceState(null, '', location.pathname);
  }
}

function showLogin(): void {
  loginScreen.classList.remove('hidden');
  homeScreen.classList.add('hidden');
  setupGoogleSignIn();
}

function enterHome(): void {
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
  }
  updatePublicGate();
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
  }
}

/// Logged-in users must accept age + ToS before using the app.
function ensureConsent(): void {
  if (billing && auth.isLoggedIn() && !auth.consentGiven()) {
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
    return;
  }
  accountBar.classList.remove('hidden');
  guestBar.classList.add('hidden');
  accountName.textContent = u.name;
  const av = auth.avatarUrl(u.avatar_url, 72);
  if (av) {
    accountAvatar.src = av;
    accountAvatar.style.display = '';
  } else {
    accountAvatar.style.display = 'none';
  }
  setBalanceUi(u.balance);
}

function setBalanceUi(balance: number): void {
  const low = balance < 0.5;
  accountBalance.textContent = auth.formatCredits(balance);
  accountBalance.classList.toggle('low', low);
  callBalance.classList.remove('hidden');
  callBalance.textContent = auth.formatCredits(balance);
  callBalance.classList.toggle('low', low);
}

// --- Google Identity Services ---
let gsiLoaded = false;
let gsiFallbackTimer: number | undefined;

// Our own Google-branded button opens Google's NATIVE account chooser (FedCM), so
// there is no white personalized card (spec 0087). If the chooser can't show
// (no FedCM / rate-limited), we fall back to Google's official rendered button so
// sign-in always works.
function setupGoogleSignIn(): void {
  const clientId = auth.getGoogleClientId();
  const customBtn = document.getElementById('gsi-signin');
  if (!clientId || !customBtn) return;
  loadGsi()
    .then(() => {
      const g = (window as unknown as { google?: any }).google;
      if (!g?.accounts?.id) return;
      // use_fedcm_for_prompt → the browser's native chooser, not a white GSI card.
      g.accounts.id.initialize({
        client_id: clientId,
        callback: onGoogleCredential,
        use_fedcm_for_prompt: true,
      });
      show(customBtn, true);
      customBtn.onclick = () => triggerGoogleSignIn(g);
    })
    .catch(() => {});
}

function triggerGoogleSignIn(g: any): void {
  let fellBack = false;
  const fallback = () => {
    if (fellBack) return;
    fellBack = true;
    showGoogleOfficialButton(g);
  };
  try {
    g.accounts.id.prompt(
      (notification: { isNotDisplayed?: () => boolean; isSkippedMoment?: () => boolean }) => {
        try {
          if (notification.isNotDisplayed?.() || notification.isSkippedMoment?.()) fallback();
        } catch {
          /* FedCM: moment methods are unavailable — the timer below is the net */
        }
      },
    );
  } catch {
    fallback();
    return;
  }
  // Safety net: if no credential lands shortly, the chooser likely never opened.
  window.clearTimeout(gsiFallbackTimer);
  gsiFallbackTimer = window.setTimeout(fallback, 7000);
}

// Reveal Google's official rendered button as a working fallback (it carries the
// white personalized card, but it always signs in).
function showGoogleOfficialButton(g: any): void {
  const official = document.getElementById('gsi-official');
  const customBtn = document.getElementById('gsi-signin');
  if (!official || official.childElementCount > 0) return;
  if (customBtn) show(customBtn, false);
  try {
    g.accounts.id.renderButton(official, { theme: 'filled_blue', size: 'large', shape: 'pill', text: 'continue_with' });
  } catch {
    if (customBtn) show(customBtn, true); // render failed → restore our button
  }
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

async function onGoogleCredential(resp: { credential?: string }): Promise<void> {
  window.clearTimeout(gsiFallbackTimer); // sign-in fired — cancel the fallback net
  if (!resp.credential) return;
  try {
    await auth.loginWithGoogle(resp.credential);
    enterHome();
  } catch {
    /* stay on the login screen; the user can retry */
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
  show(buyModal, true);
  buyStatus.textContent = '';
  buyStatus.classList.remove('error');
  const u = auth.getUser();
  if (u) modalBalance.textContent = auth.formatCredits(u.balance);
  void renderPackages();
  selectTab('history');
}

async function renderPackages(): Promise<void> {
  packagesList.innerHTML = '';
  const pkgs = await auth.fetchPackages();
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
    $(id).setAttribute('aria-pressed', String(which === tab));
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
      openSessionScreen({
        id: s.id,
        room: s.room,
        started_at: s.started_at,
        ended_at: s.ended_at,
        event_count: s.event_count,
      });
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
    openSessionScreen({
      id: ended.id,
      room: ended.room,
      started_at: new Date(now - ended.durationMs).toISOString(),
      ended_at: new Date(now).toISOString(),
      event_count: ended.events,
    });
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
function toast(msg: string): void {
  const el = document.createElement('div');
  el.className = 'vox-toast';
  el.textContent = msg;
  document.body.appendChild(el);
  requestAnimationFrame(() => el.classList.add('show'));
  setTimeout(() => {
    el.classList.remove('show');
    setTimeout(() => el.remove(), 300);
  }, 3500);
}

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
  if (await auth.submitConsent(true)) {
    show(consentModal, false);
    renderAccount();
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

// --- Privacy & data (GDPR) ---
$('privacy-open').addEventListener('click', () => {
  $('privacy-status').textContent = '';
  show(privacyModal, true);
});
$('privacy-close').addEventListener('click', () => show(privacyModal, false));
privacyModal.addEventListener('click', (e) => {
  if (e.target === privacyModal) show(privacyModal, false);
});
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
    show(privacyModal, false);
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
  if (!accepted) show(cookieBanner, true);
  $('cookie-accept').addEventListener('click', () => {
    try {
      localStorage.setItem('vox.cookie', '1');
    } catch {
      /* ignore */
    }
    show(cookieBanner, false);
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
$('logout-btn').innerHTML = icon('leave', 16);
$('buy-close').innerHTML = icon('close', 16);
$('privacy-open').innerHTML = icon('shield', 16);
$('report-close').innerHTML = icon('close', 16);
$('privacy-close').innerHTML = icon('close', 16);
$('part-close').innerHTML = icon('close', 16);
$('invite-close').innerHTML = icon('close', 16); // was missing → empty pill (spec 0090)
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
  const res = await generateAiQuiz(prompt, 5, session?.lang || 'en');
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
// boot() runs the lobby (startLobby) and resumes any session.
void boot();
