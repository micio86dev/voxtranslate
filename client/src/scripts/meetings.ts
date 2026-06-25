//! Personal scheduled meetings for the consumer app (spec: scheduled meetings,
//! Phase 1e). Thin client over `/api/meetings`, plus `setupScheduling(t)` which wires
//! the home-screen "Scheduled meetings" card + create modal. Self-contained and
//! best-effort so the core call flow is never affected. Phase 2 (friends) adds a
//! friend-picker invite source alongside the email field.

import { authHeaders, HTTP_BASE, isLoggedIn } from "./auth";

export interface Meeting {
  id: string;
  title: string;
  scheduled_at: string;
  end_at: string;
  join_url: string;
  room_code: string;
  status: string;
}

interface MeetingDetail extends Meeting {
  invitees: { email: string }[];
}

async function listMeetings(): Promise<Meeting[]> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/meetings`, {
      headers: authHeaders(),
    });
    if (!res.ok) return [];
    return (await res.json()) as Meeting[];
  } catch {
    return [];
  }
}

interface Recurrence {
  freq: string;
  count?: number;
}

interface CreateBody {
  title: string;
  scheduled_at: string;
  duration_minutes?: number;
  timezone?: string;
  invitee_emails?: string[];
  recurrence?: Recurrence;
}

/** Returns the created meeting, or a status code on failure (409 = connect calendar). */
async function createMeeting(
  body: CreateBody,
): Promise<MeetingDetail | number> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/meetings`, {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) return res.status;
    return (await res.json()) as MeetingDetail;
  } catch {
    return 0;
  }
}

async function cancelMeeting(id: string): Promise<boolean> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/meetings/${id}/cancel`, {
      method: "POST",
      headers: authHeaders(),
    });
    return res.ok;
  } catch {
    return false;
  }
}

const $ = (id: string) => document.getElementById(id);
function esc(s: string): string {
  return s.replace(
    /[&<>"]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] || c,
  );
}

// Localised lookup, set by setupScheduling. Falls back to the key.
let T: (k: string) => string = (k) => k;

/** Custom confirm dialog (localised, responsive) replacing window.confirm. Resolves
 *  true/false. Falls back to native confirm if the markup is missing. */
function confirmDialog(
  message: string,
  yesLabel: string,
  noLabel: string,
): Promise<boolean> {
  return new Promise((resolve) => {
    const modal = $("confirm-modal");
    const text = $("confirm-text");
    const yes = $("confirm-yes");
    const no = $("confirm-no");
    if (!modal || !text || !yes || !no) {
      resolve(window.confirm(message));
      return;
    }
    text.textContent = message;
    yes.textContent = yesLabel;
    no.textContent = noLabel;
    const done = (v: boolean) => {
      modal.classList.add("hidden");
      (yes as HTMLElement).onclick = null;
      (no as HTMLElement).onclick = null;
      resolve(v);
    };
    (yes as HTMLElement).onclick = () => done(true);
    (no as HTMLElement).onclick = () => done(false);
    modal.classList.remove("hidden");
  });
}

/** Join a meeting's room without leaving the web app: prefill the room field and
 *  trigger the normal connect flow (which runs the business pre-join binding). */
function joinRoom(code: string): void {
  const roomInput = document.getElementById("room") as HTMLInputElement | null;
  const enterBtn = document.getElementById("enter") as HTMLButtonElement | null;
  if (roomInput && enterBtn) {
    roomInput.value = code;
    enterBtn.click();
  } else {
    location.href = `/?room=${encodeURIComponent(code)}`;
  }
}

async function renderList(): Promise<void> {
  const list = $("schedule-list");
  if (!list) return;
  const meetings = (await listMeetings()).filter(
    (m) => m.status !== "cancelled",
  );
  list.innerHTML = meetings.length
    ? meetings
        .map((m) => {
          const when = new Date(m.scheduled_at).toLocaleString();
          return `<div class="schedule-item" data-id="${m.id}">
            <div class="schedule-item-main">
              <span class="schedule-item-title">${esc(m.title)}</span>
              <span class="schedule-item-when">${esc(when)}</span>
            </div>
            <button class="btn-ghost join-meet" data-room="${esc(m.room_code)}">${esc(T("scheduleJoin"))}</button>
            <button class="btn-ghost sm-cancel" data-id="${m.id}">${esc(T("cancel"))}</button>
          </div>`;
        })
        .join("")
    : `<p class="schedule-empty">${esc(T("scheduleEmpty"))}</p>`;
  list
    .querySelectorAll<HTMLButtonElement>(".join-meet")
    .forEach((b) =>
      b.addEventListener("click", () => joinRoom(b.dataset.room!)),
    );
  list.querySelectorAll<HTMLButtonElement>(".sm-cancel").forEach((b) =>
    b.addEventListener("click", async () => {
      const ok = await confirmDialog(
        T("scheduleCancelConfirm"),
        T("scheduleCancelMeeting"),
        T("scheduleKeep"),
      );
      if (ok && (await cancelMeeting(b.dataset.id!))) void renderList();
    }),
  );
}

let wired = false;

/**
 * Wire the scheduling card + modal, and show the card only when signed in (Google
 * Calendar access comes from the login scope). Idempotent — safe to call on every
 * home entry; listeners attach once. No-ops if the markup is absent.
 */
export function setupScheduling(t: (k: string) => string): void {
  T = t;
  const card = $("schedule-card");
  if (!card) return;
  if (!isLoggedIn()) {
    card.classList.add("hidden");
    return;
  }
  card.classList.remove("hidden");

  if (!wired) {
    wired = true;
    const modal = $("schedule-modal");
    const err = $("sm-err");
    const close = () => {
      modal?.classList.add("hidden");
      err?.classList.add("hidden");
    };
    $("schedule-open")?.addEventListener("click", () =>
      modal?.classList.remove("hidden"),
    );
    $("sm-cancel-btn")?.addEventListener("click", close);
    $("sm-cancel-btn2")?.addEventListener("click", close);

    // Show the "occurrences" field only for a recurring meeting.
    const repeatSel = $("sm-repeat") as HTMLSelectElement | null;
    repeatSel?.addEventListener("change", () =>
      $("sm-count-wrap")?.classList.toggle("hidden", !repeatSel.value),
    );

    $("sm-create")?.addEventListener("click", async () => {
      err?.classList.add("hidden");
      const title = ($("sm-title") as HTMLInputElement)?.value.trim();
      const startVal = ($("sm-start") as HTMLInputElement)?.value;
      if (!title || !startVal) {
        if (err) {
          err.textContent = T("scheduleErrRequired");
          err.classList.remove("hidden");
        }
        return;
      }
      const emails = ($("sm-emails") as HTMLInputElement)?.value
        .split(",")
        .map((e) => e.trim())
        .filter(Boolean);
      const freq = ($("sm-repeat") as HTMLSelectElement)?.value;
      let recurrence: Recurrence | undefined;
      if (freq) {
        const count = Number(($("sm-count") as HTMLInputElement)?.value);
        recurrence = { freq, ...(count > 0 ? { count } : {}) };
      }
      const res = await createMeeting({
        title,
        scheduled_at: new Date(startVal).toISOString(),
        duration_minutes:
          Number(($("sm-duration") as HTMLInputElement)?.value) || 30,
        timezone: Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC",
        invitee_emails: emails,
        recurrence,
      });
      if (typeof res === "number") {
        if (err) {
          err.textContent =
            res === 409 ? T("scheduleConnectCalendar") : T("scheduleErrSave");
          err.classList.remove("hidden");
        }
        return;
      }
      close();
      ($("sm-title") as HTMLInputElement).value = "";
      ($("sm-emails") as HTMLInputElement).value = "";
      ($("sm-repeat") as HTMLSelectElement).value = "";
      $("sm-count-wrap")?.classList.add("hidden");
      void renderList();
    });
  }

  void renderList();
}
