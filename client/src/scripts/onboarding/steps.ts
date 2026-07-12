// Onboarding step definitions (spec: onboarding tour & help system).
//
// Strings are referenced by i18n key (resolved with t() at render time, so the active UI
// language wins). The home wizard is a custom modal; the call tour drives driver.js. Both
// share the help "?" launcher.

export interface WizardStep {
  key: string;
  /** `authed`/`guest` steps render only for that audience; `all` always renders.
   *  `b2b` renders only for a signed-in member of ≥1 organization (webinar pitch). */
  role: 'all' | 'guest' | 'authed' | 'b2b';
  /** Key into the inline glyph set in home-wizard.ts. */
  glyph: string;
  titleKey: string;
  bodyKey: string;
}

// What VoxTranslate does → how to start → tiers → credits (authed) / sign-up (guest) →
// webinars (B2B) → CTA.
export const HOME_WIZARD_STEPS: WizardStep[] = [
  { key: 'welcome', role: 'all', glyph: 'globe', titleKey: 'onbHomeWelcomeTitle', bodyKey: 'onbHomeWelcomeBody' },
  { key: 'start', role: 'all', glyph: 'call', titleKey: 'onbHomeStartTitle', bodyKey: 'onbHomeStartBody' },
  { key: 'tiers', role: 'all', glyph: 'tiers', titleKey: 'onbHomeTiersTitle', bodyKey: 'onbHomeTiersBody' },
  { key: 'credits', role: 'authed', glyph: 'credits', titleKey: 'onbHomeCreditsTitle', bodyKey: 'onbHomeCreditsBody' },
  { key: 'guest', role: 'guest', glyph: 'guest', titleKey: 'onbHomeGuestTitle', bodyKey: 'onbHomeGuestBody' },
  { key: 'webinar', role: 'b2b', glyph: 'broadcast', titleKey: 'wizWebinarTitle', bodyKey: 'wizWebinarBody' },
  { key: 'cta', role: 'all', glyph: 'rocket', titleKey: 'onbHomeCtaTitle', bodyKey: 'onbHomeCtaBody' },
];

/** The audience a step render decision is made against. `b2b` is a signed-in member of
 *  ≥1 organization (regardless of subscription status — the webinar step exists to tell
 *  them hosting needs a Business/Enterprise plan). */
export interface WizardAudience {
  isLoggedIn: boolean;
  /** Member of ≥1 organization (any subscription status). Implies `isLoggedIn`. */
  isB2B: boolean;
}

/** Whether the webinar-explainer step should be shown: only to B2B users (≥1 org). Pure
 *  so the gate is unit-testable without the DOM. */
export function shouldShowWebinarStep(audience: WizardAudience): boolean {
  return audience.isB2B;
}

/** Select the wizard steps for the current audience. `all` always renders; `authed`/`guest`
 *  key off sign-in; `b2b` uses {@link shouldShowWebinarStep}. Pure — the modal calls this at
 *  open time so the active auth/org state wins. */
export function selectWizardSteps(audience: WizardAudience): WizardStep[] {
  return HOME_WIZARD_STEPS.filter((s) => {
    switch (s.role) {
      case 'all':
        return true;
      case 'authed':
        return audience.isLoggedIn;
      case 'guest':
        return !audience.isLoggedIn;
      case 'b2b':
        return shouldShowWebinarStep(audience);
    }
  });
}

export interface TourStep {
  key: string;
  /** CSS selector for the spotlight target; omit for a centered popover (intro/outro). */
  selector?: string;
  titleKey: string;
  bodyKey: string;
  side?: 'top' | 'bottom' | 'left' | 'right';
  /** Force the ⋯ overflow menu visible while this step is shown (share/invite live in it). */
  openMore?: boolean;
  /** Skip the step when its control is hidden in the current layout (mobile, guest, etc.). */
  available?: () => boolean;
}

// True only when an element exists and isn't hidden by class/attribute (independent of whether
// the ⋯ menu is currently open, so it's safe to test before forcing the menu visible).
const shown = (id: string): boolean => {
  const el = document.getElementById(id);
  return !!el && !el.classList.contains('hidden') && !el.hasAttribute('hidden');
};

// Essentials + share/invite (~11 steps). Language/engine selection lives on the home screen,
// so the in-call "room" step covers the room code + active translation language badge instead.
export const CALL_TOUR_STEPS: TourStep[] = [
  { key: 'intro', titleKey: 'onbCallIntroTitle', bodyKey: 'onbCallIntroBody' },
  { key: 'mic', selector: '#btn-mic', titleKey: 'onbCallMicTitle', bodyKey: 'onbCallMicBody', side: 'top' },
  { key: 'cam', selector: '#btn-cam', titleKey: 'onbCallCamTitle', bodyKey: 'onbCallCamBody', side: 'top' },
  { key: 'captions', selector: '#btn-subtitle', titleKey: 'onbCallCaptionsTitle', bodyKey: 'onbCallCaptionsBody', side: 'top' },
  { key: 'voice', selector: '#btn-tts', titleKey: 'onbCallVoiceTitle', bodyKey: 'onbCallVoiceBody', side: 'top' },
  { key: 'chat', selector: '#btn-chat', titleKey: 'onbCallChatTitle', bodyKey: 'onbCallChatBody', side: 'top' },
  { key: 'room', selector: '#call-room', titleKey: 'onbCallRoomTitle', bodyKey: 'onbCallRoomBody', side: 'bottom' },
  { key: 'more', selector: '#btn-more', titleKey: 'onbCallMoreTitle', bodyKey: 'onbCallMoreBody', side: 'top' },
  { key: 'share', selector: '#btn-share', titleKey: 'onbCallShareTitle', bodyKey: 'onbCallShareBody', side: 'left', openMore: true, available: () => shown('btn-share') },
  { key: 'invite', selector: '#btn-invite', titleKey: 'onbCallInviteTitle', bodyKey: 'onbCallInviteBody', side: 'left', openMore: true, available: () => shown('mi-invite') && shown('btn-invite') },
  { key: 'leave', selector: '#btn-leave', titleKey: 'onbCallLeaveTitle', bodyKey: 'onbCallLeaveBody', side: 'top' },
  { key: 'done', titleKey: 'onbCallDoneTitle', bodyKey: 'onbCallDoneBody' },
];
