// Onboarding step definitions (spec: onboarding tour & help system).
//
// Strings are referenced by i18n key (resolved with t() at render time, so the active UI
// language wins). The home wizard is a custom modal; the call tour drives driver.js. Both
// share the help "?" launcher.

export interface WizardStep {
  key: string;
  /** `authed`/`guest` steps render only for that audience; `all` always renders. */
  role: 'all' | 'guest' | 'authed';
  /** Key into the inline glyph set in home-wizard.ts. */
  glyph: string;
  titleKey: string;
  bodyKey: string;
}

// What VoxTranslate does → how to start → tiers → credits (authed) / sign-up (guest) → CTA.
export const HOME_WIZARD_STEPS: WizardStep[] = [
  { key: 'welcome', role: 'all', glyph: 'globe', titleKey: 'onbHomeWelcomeTitle', bodyKey: 'onbHomeWelcomeBody' },
  { key: 'start', role: 'all', glyph: 'call', titleKey: 'onbHomeStartTitle', bodyKey: 'onbHomeStartBody' },
  { key: 'tiers', role: 'all', glyph: 'tiers', titleKey: 'onbHomeTiersTitle', bodyKey: 'onbHomeTiersBody' },
  { key: 'credits', role: 'authed', glyph: 'credits', titleKey: 'onbHomeCreditsTitle', bodyKey: 'onbHomeCreditsBody' },
  { key: 'guest', role: 'guest', glyph: 'guest', titleKey: 'onbHomeGuestTitle', bodyKey: 'onbHomeGuestBody' },
  { key: 'cta', role: 'all', glyph: 'rocket', titleKey: 'onbHomeCtaTitle', bodyKey: 'onbHomeCtaBody' },
];

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
