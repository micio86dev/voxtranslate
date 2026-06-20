// Onboarding orchestrator (spec: onboarding tour & help system). Wires the two "?" launchers,
// owns the localStorage "seen" flags, and exposes auto-start hooks called from app.ts.
//
// Flags follow the existing `vox_`-snake_case consent convention and use the same private-mode-safe
// storage guard as auth.ts (a stored value of '1' means "seen"). The "?" buttons always launch the
// relevant guide; auto-start only fires once, when the flag is unset.

import { initHomeWizard, openHomeWizard } from './home-wizard';
import { startCallTour } from './call-tour';
import { wireHelpButton } from './help-button';

const HOME_FLAG = 'vox_home_tour_seen';
const CALL_FLAG = 'vox_call_tour_seen';

const mem = new Map<string, string>();
function store(): Pick<Storage, 'getItem' | 'setItem'> {
  try {
    if (typeof localStorage !== 'undefined') return localStorage;
  } catch {
    /* blocked (private mode) → in-memory fallback */
  }
  return {
    getItem: (k) => (mem.has(k) ? mem.get(k)! : null),
    setItem: (k, v) => void mem.set(k, v),
  };
}
const seen = (flag: string): boolean => store().getItem(flag) === '1';
const markSeen = (flag: string): void => store().setItem(flag, '1');

export interface OnboardingDeps {
  show: (el: HTMLElement, visible: boolean) => void;
  isLoggedIn: () => boolean;
  /** Toggle the in-call ⋯ overflow menu visible for the tour's share/invite steps. */
  forceMore: (open: boolean) => void;
}

let deps: OnboardingDeps;

export function initOnboarding(d: OnboardingDeps): void {
  deps = d;
  initHomeWizard({ show: d.show, isLoggedIn: d.isLoggedIn });
  wireHelpButton('onb-home-help', startHomeWizard);
  wireHelpButton('onb-call-help', startCallTourNow);
}

export function startHomeWizard(): void {
  markSeen(HOME_FLAG);
  openHomeWizard();
}

function startCallTourNow(): void {
  markSeen(CALL_FLAG);
  startCallTour({ forceMore: deps.forceMore });
}

/**
 * Auto-open the home wizard on first visit. `canStart` lets app.ts veto while a blocking overlay
 * (the 18+/ToS consent gate) is up — call this again once that gate is dismissed.
 */
export function maybeAutoStartHome(canStart: () => boolean): void {
  if (seen(HOME_FLAG) || !canStart()) return;
  startHomeWizard();
}

/** Auto-open the call tour on first entry, deferred so the call UI has laid out. */
export function maybeAutoStartCall(): void {
  if (seen(CALL_FLAG)) return;
  requestAnimationFrame(() => requestAnimationFrame(startCallTourNow));
}
