// VoxTranslate V2 client orchestrator: home/lobby → pre-join (camera + devices)
// → WebRTC video call with translated subtitles + chat.

import { applyI18n, detectLang, FLAG, getUiLang, setUiLang, t } from './i18n';
import { loadRemoteI18n } from './content';
import { icon } from './icons';
import { MeshManager } from './webrtc';
import { AudioCapture } from './audio-capture';
import { MicMeter } from './mic-meter';
import { ChatManager, type ChatPayload } from './chat';
import { checkUploadFile, fileUploadEnabled, uploadChatFile } from './api';
import * as auth from './auth';
import { openSessionScreen } from './session-screen';
import { initBookmarks, setBookmarkSession } from './bookmarks';
import { initGlossary, onGlossaryActive, refreshGlossaryHome, setGlossaryRoom } from './glossary';
import { dismissLangToast, initLangDetect, onLanguageDetected } from './lang-detect';
import { playHandRaiseSound, playJoinSound, playScreenShareSound } from './sfx';
import { VirtualBackground } from './virtual-background';
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

// ---- Screens ---------------------------------------------------------------
const loginScreen = $('login');
const homeScreen = $('home');
const prejoinScreen = $('prejoin');
const callScreen = $('call');

// ---- Auth / billing refs ---------------------------------------------------
const accountBar = $('account-bar');
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
let exhaustedIsGuest = false; // last balance_exhausted was a guest trial vs a billed user
const blockedPeers = new Set<string>(); // peers blocked locally (muted + hidden)
let reportTargetId = ''; // peer currently being reported

// ---- Home refs -------------------------------------------------------------
const roomInput = $<HTMLInputElement>('room');
const nameInput = $<HTMLInputElement>('name');
const langSel = $<HTMLSelectElement>('lang');
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
const chatPanel = $('chat-panel');
const chatMessages = $('chat-messages');
const chatInput = $<HTMLInputElement>('chat-input');
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
const notifBanner = $('notif-banner');
const participantsPanel = $('participants-panel');
const participantsList = $('participants-list');
const partClose = $('part-close');

// ---- State -----------------------------------------------------------------
const myId =
  (crypto && crypto.randomUUID && crypto.randomUUID()) ||
  `id-${Math.random().toString(36).slice(2)}-${Date.now()}`;

let session: { room: string; lang: string; name: string; isPublic: boolean } | null = null;
let localStream: MediaStream | null = null;
let ws: WebSocket | null = null;
let mesh: MeshManager | null = null;
let audioCapture: AudioCapture | null = null;
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
let manualClose = false;
let viewMode: 'grid' | 'speaker' = 'grid';
let pinnedPeerId: string | null = null;
let lastSpeakerId: string | null = null;
let isSharingScreen = false;
let screenStream: MediaStream | null = null;
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

const peerNames = new Map<string, { name: string; lang: string; avatar?: string | null }>();
const peerCamOff = new Map<string, boolean>(); // camera-off state from peer_muted
const peerMicMuted = new Map<string, boolean>(); // mic muted state from peer_muted
const peerHandRaised = new Map<string, boolean>(); // hand-raise state
const subtitleTimers = new Map<string, number>();

// ============================================================================
// i18n
// ============================================================================
langSel.value = detectLang();
applyI18n();
langSel.addEventListener('change', () => {
  setUiLang(langSel.value);
  applyI18n();
  updateVisHint();
});

function updateVisHint(): void {
  visHint.textContent = visibilityPublic ? '' : t('privateHint');
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
roomInput.value = randomRoom();
$('dice').addEventListener('click', () => (roomInput.value = randomRoom()));

visGroup.addEventListener('click', (e) => {
  const btn = (e.target as HTMLElement).closest('.seg-btn') as HTMLElement | null;
  if (!btn) return;
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

function renderRooms(rooms: Array<{ room: string; count: number; participants: Array<{ name: string; lang: string }> }>): void {
  roomsList.innerHTML = '';
  if (!rooms.length) {
    const empty = document.createElement('div');
    empty.className = 'lobby-empty';
    empty.textContent = t('noPublicRooms');
    roomsList.appendChild(empty);
    return;
  }
  for (const r of rooms) {
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
    const members = document.createElement('div');
    members.className = 'room-item-members';
    for (const m of r.participants) {
      const chip = document.createElement('span');
      chip.className = 'chip';
      chip.textContent = `${FLAG[m.lang] || ''} ${m.name}`.trim();
      members.appendChild(chip);
    }
    item.append(main, members);
    item.addEventListener('click', () => goPrejoin(r.room, true));
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

// ============================================================================
// Pre-join: camera preview + device selectors
// ============================================================================
async function goPrejoin(room: string, isPublic: boolean): Promise<void> {
  session = { room, lang: langSel.value, name: nameInput.value.trim(), isPublic };
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
  return {
    width: { ideal: 1280, max: 1280 },
    height: { ideal: 720, max: 720 },
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
  startCall();
});

// ============================================================================
// Call
// ============================================================================
function startCall(): void {
  if (!session || !localStream) return;
  prejoinScreen.classList.add('hidden');
  callScreen.classList.remove('hidden');
  callRoom.textContent = session.room;
  callVis.textContent = session.isPublic ? t('public') : t('private');
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
  openSocket();
}

function openSocket(): void {
  if (!session) return;
  const params = new URLSearchParams({ room: session.room, lang: session.lang, id: myId, public: String(session.isPublic) });
  if (session.name) params.set('name', session.name);
  ws = new WebSocket(auth.buildWsUrl(params));

  ws.onopen = () => {
    mesh = new MeshManager(localStream!, (sig) => ws?.send(JSON.stringify(sig)));
    mesh.onRemoteStream = (peerId, stream) => {
      remoteStreams.set(peerId, stream);
      recorder?.addParticipant(participantSource(peerId, stream));
      attachStream(peerId, stream);
    };
    mesh.onPeerRemoved = (peerId) => removeCell(peerId);
    mesh.setAudioEnabled(micOn);
    mesh.setVideoEnabled(camOn);

    audioCapture = new AudioCapture(localStream!, ws!);
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
    if (!manualClose && e.code !== 1000) setTimeout(() => !manualClose && openSocket(), 2000);
  };
}

async function handleServer(msg: any): Promise<void> {
  switch (msg.type) {
    case 'room_joined':
      // session_id present = the backend records a transcript of this call.
      activeSessionId = typeof msg.session_id === 'string' ? msg.session_id : null;
      callStartedAt = Date.now();
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
    case 'peer_joined':
      peerNames.set(msg.peer_id, { name: msg.user_name, lang: msg.lang, avatar: msg.avatar_url });
      addCell(msg.peer_id, msg.user_name, msg.lang, false, msg.avatar_url);
      playJoinSound(); // audible cue that someone joined the session
      await mesh?.addPeer(msg.peer_id, true); // we initiate toward the newcomer
      // Re-announce our current mute/camera state so the newcomer's UI matches.
      if (!micOn) ws?.send(JSON.stringify({ type: 'mute_audio', muted: true }));
      if (!camOn) ws?.send(JSON.stringify({ type: 'mute_video', muted: true }));
      updateParticipantsList();
      break;
    case 'peer_left':
      mesh?.removePeer(msg.peer_id);
      removeCell(msg.peer_id);
      peerHandRaised.delete(msg.peer_id);
      updateParticipantsList();
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
      // valid TTS voice/translation to pick — skip until it resolves.
      if (ttsOn && msg.speaker_id !== myId && msg.lang !== myLang && myLang !== 'auto')
        speak(text, myLang);
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
  cell.querySelector('.pan-toggle')?.remove();
  cell.querySelector('.pan-hint')?.remove();
}

function setupPan(cell: HTMLElement): void {
  if (cell.dataset.panSetup) return; // listeners already attached
  cell.dataset.panSetup = '1';

  let tx = 0, ty = 0, startX = 0, startY = 0;

  cell.addEventListener('touchstart', (e: TouchEvent) => {
    if (!cell.classList.contains('pan-mode') || e.touches.length !== 1) return;
    startX = e.touches[0].clientX - tx;
    startY = e.touches[0].clientY - ty;
  }, { passive: true });

  cell.addEventListener('touchmove', (e: TouchEvent) => {
    if (!cell.classList.contains('pan-mode') || e.touches.length !== 1) return;
    e.preventDefault();
    tx = e.touches[0].clientX - startX;
    ty = e.touches[0].clientY - startY;
    const v = cell.querySelector<HTMLVideoElement>('video');
    if (v) v.style.transform = `translate(${tx}px, ${ty}px)`;
  }, { passive: false });

  // Pan toggle button
  const btn = document.createElement('button');
  btn.className = 'pan-toggle';
  btn.title = 'Fit / Pan';
  btn.textContent = '⤢';
  cell.appendChild(btn);

  btn.addEventListener('click', (e) => {
    e.stopPropagation();
    const active = cell.classList.toggle('pan-mode');
    btn.classList.toggle('active', active);
    if (active) {
      // Reset transform and show hint
      tx = 0; ty = 0;
      const v = cell.querySelector<HTMLVideoElement>('video');
      if (v) v.style.transform = '';
      const existing = cell.querySelector('.pan-hint');
      if (!existing) {
        const hint = document.createElement('span');
        hint.className = 'pan-hint';
        hint.textContent = 'Trascina per spostare';
        cell.appendChild(hint);
        hint.addEventListener('animationend', () => hint.remove());
      }
    } else {
      // Reset position on exit
      tx = 0; ty = 0;
      const v = cell.querySelector<HTMLVideoElement>('video');
      if (v) v.style.transform = '';
    }
  });
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
    if (IS_MOBILE) setupPan(focusCell);

    for (const cell of allCells) {
      if (cell === focusCell) continue;
      cell.classList.add('video-thumb');
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
      video.muted = !!(ttsOn && peerLang && myLang && peerLang !== myLang);
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

function showEmojiReaction(peerId: string, emoji: string): void {
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(peerId)}"]`);
  if (!cell) return;
  const floater = document.createElement('span');
  floater.className = 'emoji-float';
  floater.textContent = emoji;
  cell.appendChild(floater);
  setTimeout(() => floater.remove(), 1500);
}

// ---- Notification banner ---------------------------------------------------
let notifTimer: number | null = null;
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

function updateParticipantsList(): void {
  const myLang = session?.lang || 'en';
  const myName = session?.name || t('namePlaceholder');
  const items: Array<{ id: string; name: string; lang: string; isSelf: boolean; micMuted: boolean; handRaised: boolean }> = [];

  items.push({ id: myId, name: myName, lang: myLang, isSelf: true, micMuted: !micOn, handRaised });
  for (const [id, info] of peerNames) {
    items.push({ id, name: info.name, lang: info.lang, isSelf: false, micMuted: peerMicMuted.get(id) ?? false, handRaised: peerHandRaised.get(id) ?? false });
  }

  participantsList.innerHTML = '';
  for (const p of items) {
    const el = document.createElement('div');
    el.className = `part-item${p.isSelf ? ' self' : ''}`;

    const avatar = document.createElement('span');
    avatar.className = 'part-avatar';
    avatar.style.background = avatarGradient(p.name);
    avatar.textContent = p.name.slice(0, 2).toUpperCase();

    const info = document.createElement('div');
    info.className = 'part-info';
    const nameEl = document.createElement('div');
    nameEl.className = 'part-name';
    nameEl.innerHTML = `${FLAG[p.lang] || ''} ${p.name}${p.isSelf ? ` · ${t('you')}` : ''}`.trim();
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
  const chatIco = btnChat.querySelector('.chat-ico');
  if (chatIco) chatIco.innerHTML = icon('chat');
  const leave = document.getElementById('btn-leave');
  if (leave) leave.innerHTML = icon('leave');
}

btnMic.addEventListener('click', () => {
  micOn = !micOn;
  mesh?.setAudioEnabled(micOn);
  audioCapture?.setMuted(!micOn);
  setAudioMuted(myId, !micOn);
  ws?.send(JSON.stringify({ type: 'mute_audio', muted: !micOn }));
  setControlState();
});

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
    setCameraOff(myId, !camOn);
    // While screen-sharing the recorder's self tile shows the screen regardless.
    if (!isSharingScreen) recorder?.setVideoOff(myId, !camOn);
    ws?.send(JSON.stringify({ type: 'mute_video', muted: !camOn }));
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
  if (!isSharingScreen) mesh?.replaceVideoTrack(null);
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
  // (stopScreenShare restores it via replaceVideoTrack); don't disturb the screen.
  if (!isSharingScreen) {
    mesh?.replaceVideoTrack(outgoing); // swap the video sender (transceiver-backed)
    setSelfVideo(localStream);
    recorder?.updateStream(myId, localStream);
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
  if (!ttsOn && window.speechSynthesis) speechSynthesis.cancel();
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

btnHand.addEventListener('click', () => {
  handRaised = !handRaised;
  ws?.send(JSON.stringify({ type: 'hand_raise', raised: handRaised }));
  if (handRaised) playHandRaiseSound(); // confirmation cue for the local user
  // The server relays hand_raised to peers only — update our own tile + list.
  setHandIndicator(myId, handRaised);
  updateParticipantsList();
  setControlState();
});

btnFullscreen.addEventListener('click', () => {
  if (!document.fullscreenElement) {
    document.documentElement.requestFullscreen().catch(() => {});
  } else {
    document.exitFullscreen().catch(() => {});
  }
});

btnPip.addEventListener('click', () => {
  if (pipWindow && !pipWindow.closed) {
    pipWindow.close();
    pipWindow = null;
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
      const stage = document.querySelector('.video-stage') as HTMLElement;
      if (stage) {
        const clone = stage.cloneNode(true) as HTMLElement;
        clone.style.cssText = 'width:100%;height:100dvh';
        // Mirror the live grid layout so columns/rows match the main window.
        const cloneGrid = clone.querySelector<HTMLElement>('.video-grid');
        if (cloneGrid) {
          cloneGrid.style.gridTemplateColumns = videoGrid.style.gridTemplateColumns;
          cloneGrid.style.gridTemplateRows = videoGrid.style.gridTemplateRows;
        }
        w.document.body.appendChild(clone);
        // Re-attach srcObjects and start playback (cloneNode doesn't copy MediaStreams).
        // The clones are DISPLAY-ONLY: force-mute every one of them and let audio keep
        // playing from the live elements in the main window (still in the DOM with the
        // same MediaStream). This is what makes remote tiles render: Chrome's autoplay
        // policy blocks an UNMUTED play() once the transient activation from
        // requestWindow() is consumed, so an unmuted remote clone never starts and shows
        // a silent black frame — while your own (already-muted) clone plays. Muting every
        // clone lets them all autoplay and also avoids double audio from the same track
        // playing in two windows at once.
        clone.querySelectorAll('video').forEach((v) => {
          const peer = (v.closest('[data-peer]') as HTMLElement)?.dataset.peer;
          if (!peer) return;
          const orig = videoGrid.querySelector<HTMLVideoElement>(`[data-peer="${cssEsc(peer)}"] video`);
          if (orig?.srcObject) {
            v.muted = true;
            v.srcObject = orig.srcObject;
            void v.play().catch(() => {});
          }
        });
      }
      w.addEventListener('pagehide', () => { pipWindow = null; });
    })
    .catch(() => {});
});

btnView.addEventListener('click', () => {
  viewMode = viewMode === 'grid' ? 'speaker' : 'grid';
  if (viewMode === 'grid') pinnedPeerId = null;
  setControlState();
  layoutVideos();
  updatePinButtons();
});

btnShare.addEventListener('click', () => {
  if (isSharingScreen) {
    stopScreenShare();
  } else {
    startScreenShare();
  }
});

async function startScreenShare(): Promise<void> {
  // Independent of the camera: works whether you're camera-on, camera-off, or
  // joined audio-only. We keep `localStream` as the real mic/camera stream and
  // only swap the outgoing *video* track for the screen.
  if (!mesh) return;
  try {
    const s = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: false });
    screenStream = s;
    isSharingScreen = true;
    // Send the screen on every peer's video sender (replaces the camera feed,
    // mic audio is untouched). The always-present video transceiver means this
    // reaches peers even when we joined without a camera.
    mesh.replaceVideoTrack(s.getVideoTracks()[0] ?? null);
    // Peers may have us flagged camera-off (their tile would hide the video);
    // tell them to reveal it so the shared screen actually shows.
    ws?.send(JSON.stringify({ type: 'mute_video', muted: false }));
    // Our own tile + recorder show the screen, regardless of camera state.
    setSelfVideo(s);
    setCameraOff(myId, false);
    recorder?.updateStream(myId, s);
    recorder?.setVideoOff(myId, false);
    // Show indicator on self cell
    const cell = videoGrid.querySelector(`[data-peer="${cssEsc(myId)}"]`);
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
    setControlState();
  } catch {
    // User cancelled the picker — roll back the optimistic flag.
    isSharingScreen = false;
    screenStream = null;
  }
}

function stopScreenShare(): void {
  if (!isSharingScreen || !mesh) return;
  isSharingScreen = false;
  if (screenStream) {
    screenStream.getTracks().forEach((t) => t.stop());
    screenStream = null;
  }
  // Restore the camera feed for peers (or clear video when the camera is off /
  // we joined audio-only), honouring the current camera toggle.
  const camTrack = localStream?.getVideoTracks()[0] ?? null;
  mesh.replaceVideoTrack(camTrack);
  mesh.setVideoEnabled(camOn);
  ws?.send(JSON.stringify({ type: 'mute_video', muted: !camOn }));
  // Our own tile + recorder back to the camera (or camera-off avatar).
  setSelfVideo(localStream);
  setCameraOff(myId, !camOn);
  recorder?.updateStream(myId, localStream);
  recorder?.setVideoOff(myId, !camOn);
  // Remove badge
  const cell = videoGrid.querySelector(`[data-peer="${cssEsc(myId)}"]`);
  cell?.querySelector('.screen-share-badge')?.remove();
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

/** Current roster for the compositor: self first, then peers in join order. */
function recorderSources(): ParticipantSource[] {
  const sources = [participantSource(myId, screenStream ?? localStream)];
  for (const [peerId] of peerNames) {
    sources.push(participantSource(peerId, remoteStreams.get(peerId) ?? null));
  }
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
  showNotif(t('recording'));
  $('rec-timer').textContent = '00:00';
  show($('rec-badge'), true);
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
  chat?.sendMessage(text);
  chatInput.value = '';
}
$('chat-send').addEventListener('click', sendChat);
chatInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') sendChat();
});

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
  if (!res.ok) showNotif(t('uploadFailed'));
}

$('btn-leave').addEventListener('click', leaveCall);
function leaveCall(): void {
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
  show($('transcript-indicator'), false);
  manualClose = true;
  audioCapture?.stop();
  micMeter?.stop();
  micMeter = null;
  // Initiate the recording stop BEFORE tearing down the mesh: the chunks are
  // already collected, so the async Blob assembly survives the cleanup below.
  if (isRecording) void stopRecording();
  mesh?.destroy();
  if (pipWindow && !pipWindow.closed) { pipWindow.close(); pipWindow = null; }
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  if (isSharingScreen) stopScreenShare();
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
  if (window.speechSynthesis) speechSynthesis.cancel();
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

function cssEsc(s: string): string {
  return (window.CSS && CSS.escape ? CSS.escape(s) : s.replace(/["\\]/g, '\\$&'));
}

function speak(text: string, lang: string): void {
  if (!window.speechSynthesis) return;
  speechSynthesis.cancel();
  const u = new SpeechSynthesisUtterance(text);
  const v = speechSynthesis.getVoices().find((vo) => vo.lang.toLowerCase().startsWith(lang.toLowerCase()));
  if (v) u.voice = v;
  u.lang = lang;
  u.rate = 1.1;
  speechSynthesis.speak(u);
}
if (window.speechSynthesis) speechSynthesis.getVoices();

// Copy room code from the call header.
callRoom.addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText(callRoom.textContent?.trim() || '');
    const prev = callVis.textContent;
    callVis.textContent = t('copied');
    setTimeout(() => (callVis.textContent = prev), 1200);
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
  // Pull any DB-managed UI strings over the bundled defaults, then re-render
  // (fails safe — keeps the bundled strings if the API is down).
  if (await loadRemoteI18n(HTTP_BASE)) applyI18n();
  billing = await auth.billingEnabled();
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
  const pubBtn = visGroup.querySelector('.seg-btn[data-vis="public"]') as HTMLButtonElement | null;
  if (!pubBtn) return;
  pubBtn.disabled = guest;
  pubBtn.classList.toggle('disabled', guest);
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
  const u = auth.getUser();
  if (!billing || !u) {
    accountBar.classList.add('hidden');
    return;
  }
  accountBar.classList.remove('hidden');
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
function setupGoogleSignIn(): void {
  const clientId = auth.getGoogleClientId();
  const container = document.getElementById('gsi-button');
  if (!clientId || !container) return;
  loadGsi()
    .then(() => {
      const g = (window as unknown as { google?: any }).google;
      if (!g?.accounts?.id) return;
      g.accounts.id.initialize({ client_id: clientId, callback: onGoogleCredential });
      container.innerHTML = '';
      g.accounts.id.renderButton(container, { theme: 'filled_blue', size: 'large', shape: 'pill', text: 'continue_with' });
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

async function onGoogleCredential(resp: { credential?: string }): Promise<void> {
  if (!resp.credential) return;
  try {
    await auth.loginWithGoogle(resp.credential);
    enterHome();
  } catch {
    /* stay on the login screen; the user can retry */
  }
}

$('guest-btn').addEventListener('click', () => enterHome());
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
        const ok = await auth.downloadTranscript(s.id, format, getUiLang());
        btn.disabled = false;
        if (!ok) toast(t('downloadFailed'));
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
  const ok = await auth.downloadTranscript(postCallSessionId, format, getUiLang());
  btn.textContent = prev;
  btn.disabled = postCallEvents === 0;
  if (!ok) toast(t('downloadFailed'));
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
$('postcall-close').innerHTML = icon('close', 16);
$('btn-bookmark').innerHTML = icon('bookmark');
$('bookmarks-close').innerHTML = icon('close', 16);
initBookmarks({ layout: layoutVideos }); // panel toggles re-flow the video grid
$('btn-glossary-home').innerHTML = icon('book', 18);
$('glossary-close').innerHTML = icon('close', 16);
initGlossary({ show }); // app's show() gives the modal its focus trap
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

for (const em of REACTION_LIST) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.textContent = em;
  btn.addEventListener('click', () => sendEmoji(em));
  emojiReact.appendChild(btn);
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
  ws?.send(JSON.stringify({ type: 'emoji', emoji }));
  setEmojiPanelOpen(false);
}

function insertEmoji(emoji: string): void {
  const start = chatInput.selectionStart ?? chatInput.value.length;
  const end = chatInput.selectionEnd ?? start;
  const next = chatInput.value.slice(0, start) + emoji + chatInput.value.slice(end);
  if (next.length > chatInput.maxLength) return;
  chatInput.value = next;
  const pos = start + emoji.length;
  chatInput.focus();
  chatInput.setSelectionRange(pos, pos);
}

initCookieBanner();
// boot() runs the lobby (startLobby) and resumes any session.
void boot();
