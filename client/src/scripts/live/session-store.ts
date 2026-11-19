// Where a Live Connect tab remembers its session (spec 0121).
//
// `sessionStorage` on purpose: the live token is a bearer credential for one conversation,
// so it must die with the tab and never reach another tab, another device or a later visit.
// It survives a reload, which is the whole point — a refresh rejoins instead of stranding
// the other person in an empty room.
//
// Every access is wrapped: storage can throw (private windows, blocked site data, quota) and
// the conversation must work without it.

export const STORAGE_KEY = 'vox.live.session';
export const HEADPHONES_KEY = 'vox.live.headphones';
const DEFAULT_WS_PATH = '/ws/live';

export interface StoredSession {
  sessionId: string;
  token: string;
  role: 'host' | 'guest';
  myLang: string;
  myPeerId: string;
  /** Always `/ws/live` today; stored so a future route move does not strand a rejoin. */
  wsPath: string;
  /** Host only: lets a reload show the invite again while the guest has not joined. */
  inviteUrl?: string;
}

export function saveStoredSession(s: StoredSession): void {
  try {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(s));
  } catch {
    // Persistence is a convenience.
  }
}

export function loadStoredSession(): StoredSession | null {
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Partial<StoredSession>;
    if (
      typeof v.sessionId !== 'string' ||
      typeof v.token !== 'string' ||
      (v.role !== 'host' && v.role !== 'guest') ||
      typeof v.myLang !== 'string' ||
      typeof v.myPeerId !== 'string'
    ) {
      return null;
    }
    return {
      sessionId: v.sessionId,
      token: v.token,
      role: v.role,
      myLang: v.myLang,
      myPeerId: v.myPeerId,
      wsPath: typeof v.wsPath === 'string' ? v.wsPath : DEFAULT_WS_PATH,
      ...(typeof v.inviteUrl === 'string' ? { inviteUrl: v.inviteUrl } : {}),
    };
  } catch {
    return null;
  }
}

export function clearStoredSession(): void {
  try {
    sessionStorage.removeItem(STORAGE_KEY);
  } catch {
    // Nothing to recover.
  }
}

/** "I am wearing headphones": lets the echo guard stop gating the microphone. */
export function loadHeadphones(): boolean {
  try {
    return localStorage.getItem(HEADPHONES_KEY) === '1';
  } catch {
    return false;
  }
}

export function saveHeadphones(on: boolean): void {
  try {
    localStorage.setItem(HEADPHONES_KEY, on ? '1' : '0');
  } catch {
    // Per-device convenience only.
  }
}
