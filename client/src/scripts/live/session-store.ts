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
export const DESK_KEY = 'vox.live.desk';
export const STAFF_LANG_KEY = 'vox.live.staffLang';
const DEFAULT_WS_PATH = '/ws/live';

export interface StoredSession {
  sessionId: string;
  token: string;
  role: 'host' | 'guest' | 'staff';
  myLang: string;
  myPeerId: string;
  /** Always `/ws/live` today; stored so a future route move does not strand a rejoin. */
  wsPath: string;
  /** Host only: lets a reload show the invite again while the guest has not joined. */
  inviteUrl?: string;
  /** How many guests the invitation admits (1-3). Absent on a session stored before multi-guest. */
  maxGuests?: number;
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
      (v.role !== 'host' && v.role !== 'guest' && v.role !== 'staff') ||
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
      ...(Number.isInteger(v.maxGuests) && (v.maxGuests as number) >= 1 && (v.maxGuests as number) <= 3
        ? { maxGuests: v.maxGuests }
        : {}),
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

/**
 * What the desk scan page needs after a reload besides the credentials: which desk this is and
 * when the request stops being answerable. Kept apart from the session because the engine
 * rewrites the session record and knows nothing about desks. Keyed by session id, so a new scan
 * can never pick up the previous scan's countdown.
 */
export interface DeskInfo {
  sessionId: string;
  /** The desk's public handle (the part of the QR segment before the dot, which is not secret):
   *  lets a tab that scanned desk B tell that its stored session belongs to desk A. */
  handle: string;
  name: string;
  location: string | null;
  /** Epoch milliseconds after which nobody is going to answer. */
  until: number;
}

export function saveDeskInfo(d: DeskInfo): void {
  try {
    sessionStorage.setItem(DESK_KEY, JSON.stringify(d));
  } catch {
    // A convenience: without it a reload shows no desk name and no countdown.
  }
}

export function loadDeskInfo(sessionId: string): DeskInfo | null {
  try {
    const raw = sessionStorage.getItem(DESK_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Partial<DeskInfo>;
    if (
      v.sessionId !== sessionId ||
      typeof v.handle !== 'string' ||
      typeof v.name !== 'string' ||
      typeof v.until !== 'number' ||
      (v.location !== null && v.location !== undefined && typeof v.location !== 'string')
    ) {
      return null;
    }
    return { sessionId, handle: v.handle, name: v.name, location: v.location ?? null, until: v.until };
  } catch {
    return null;
  }
}

export function clearDeskInfo(): void {
  try {
    sessionStorage.removeItem(DESK_KEY);
  } catch {
    // Nothing to recover.
  }
}

/** The language a staff member spoke last at each desk, so the next answer is one tap. */
function readStaffLangs(): Record<string, string> {
  try {
    const raw = localStorage.getItem(STAFF_LANG_KEY);
    const v = raw ? (JSON.parse(raw) as unknown) : null;
    return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, string>) : {};
  } catch {
    return {};
  }
}

export function loadStaffLang(deskId: string): string | null {
  const lang = readStaffLangs()[deskId];
  return typeof lang === 'string' ? lang : null;
}

export function saveStaffLang(deskId: string, lang: string): void {
  try {
    localStorage.setItem(STAFF_LANG_KEY, JSON.stringify({ ...readStaffLangs(), [deskId]: lang }));
  } catch {
    // Per-device convenience only.
  }
}
