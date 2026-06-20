// In-call onboarding tour, built on driver.js (spotlight + tooltip). Steps come from steps.ts;
// strings resolve through t() at start time. driver.js popovers attach to <body>, so their theme
// lives in a global style block (index.astro). Animation is disabled under prefers-reduced-motion.

import { driver, type DriveStep } from 'driver.js';
import 'driver.js/dist/driver.css';

import { t } from '../i18n';
import { CALL_TOUR_STEPS } from './steps';

export interface CallTourDeps {
  /** Force the ⋯ overflow menu visible (true) or release it (false) — see app.ts wiring. */
  forceMore: (open: boolean) => void;
}

export function startCallTour(deps: CallTourDeps): void {
  const reduce = !!window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;

  const steps: DriveStep[] = CALL_TOUR_STEPS.filter((s) => {
    if (!s.selector) return true; // centered intro/outro popovers
    if (!document.querySelector(s.selector)) return false;
    return s.available ? s.available() : true;
  }).map((s): DriveStep => ({
    element: s.selector,
    popover: {
      title: t(s.titleKey),
      description: t(s.bodyKey),
      side: s.side ?? 'top',
      align: 'center',
    },
    // Set the menu state for THIS step explicitly (so forward and back navigation both behave):
    // share/invite want it open, everything else closed.
    onHighlightStarted: () => deps.forceMore(!!s.openMore),
  }));

  // Map our "{n}/{total}" placeholders onto driver's "{{current}}/{{total}}" tokens.
  const progressText = t('onbStepCounter')
    .replace('{n}', '{{current}}')
    .replace('{total}', '{{total}}');

  driver({
    showProgress: true,
    progressText,
    nextBtnText: t('onbNext'),
    prevBtnText: t('onbBack'),
    doneBtnText: t('onbDone'),
    animate: !reduce,
    smoothScroll: true,
    allowClose: true,
    overlayColor: '#06070c',
    overlayOpacity: 0.72,
    stagePadding: 6,
    stageRadius: 10,
    popoverClass: 'onb-driver',
    steps,
    onDestroyed: () => deps.forceMore(false),
  }).drive();
}
