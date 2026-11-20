// Live Connect — the host's translation-tier picker (spec 0121, S4).
//
// Radio cards in the look of Meet's tier cards, reusing its wording (`engineDescKey`, the
// per-language cost note). Native radio inputs inside labels: the browser gives the group its
// arrow-key navigation and the labels give every card its accessible name. The list comes from
// `GET /api/live/engines` and arrives AFTER the first paint, so the picker starts on Standard
// only and is redrawn when the list lands; a list that never comes leaves Standard, silently.
//
// The choice is remembered per browser but only ever re-validated against what is offered
// now: a stale id (a tier switched off since) quietly becomes the default.

import { t } from '../i18n';
import { engineDescKey } from '../engines';
import { ENHANCED_ENGINE_ID, fallbackEngines, needsMoreBalance, resolveTier, type LiveEngines } from './tiers';
import { fill, formatCredits } from './view';

export interface TierPickerOptions {
  idPrefix: string;
  /** Where the once-only "rate is per language" explanation goes. */
  hint?: HTMLElement | null;
  /** The signed-in account's cached balance, if known. */
  balance: () => number | undefined;
  load: () => string | null;
  save: (id: string) => void;
  /** The host picked another card (not on the first paint). */
  onChange?: (id: string) => void;
}

export interface TierPicker {
  /** The tier to create the conversation on. */
  get(): string;
  /** Draw the tiers the server listed. */
  setEngines(list: LiveEngines): void;
}

/** `0.0045` -> `$0.0045`, `0.03` -> `$0.03`: a sub-cent rate must not round to a different number. */
function formatTierRate(rate: number): string {
  const [whole, frac = ''] = rate.toFixed(4).replace(/0+$/, '').split('.');
  return `$${whole}.${frac.padEnd(2, '0')}`;
}

function span(doc: Document, cls: string, text: string): HTMLSpanElement {
  const s = doc.createElement('span');
  s.className = cls;
  s.textContent = text;
  return s;
}

export function mountTierPicker(root: HTMLElement, opts: TierPickerOptions): TierPicker {
  const doc = root.ownerDocument;
  let list: LiveEngines = fallbackEngines();
  let selected = list.default;

  const draw = (): void => {
    const balance = opts.balance();
    root.replaceChildren(
      ...list.engines.map((e) => {
        const id = `${opts.idPrefix}-tier-${e.id}`;
        const lacksBalance = needsMoreBalance(e, balance);
        const label = doc.createElement('label');
        label.className = 'lc-tier';
        label.htmlFor = id;
        const input = doc.createElement('input');
        input.type = 'radio';
        input.name = `${opts.idPrefix}-tier`;
        input.id = id;
        input.value = e.id;
        input.className = 'lc-tier-input';
        input.checked = e.id === selected;
        input.disabled = lacksBalance;
        input.addEventListener('change', () => {
          if (!input.checked) return;
          selected = e.id;
          opts.save(e.id);
          opts.onChange?.(e.id);
        });

        const body = doc.createElement('span');
        body.className = 'lc-tier-body';
        body.append(span(doc, 'lc-tier-name', e.display_name));
        // The server's tier name is authoritative; the id is the fallback for Enhanced's wording.
        const descKey = engineDescKey(e.tier) ?? (e.id === ENHANCED_ENGINE_ID ? engineDescKey('enhanced') : null);
        const desc = descKey ? t(descKey) : e.description;
        if (desc) body.append(span(doc, 'lc-tier-desc', desc));
        if (e.rate_per_minute > 0) {
          body.append(span(doc, 'lc-tier-cost', fill(t('liveTierCost'), { rate: formatTierRate(e.rate_per_minute) })));
        }
        // Why the card is off, or a hint that it may be: read out with the radio itself.
        const why = lacksBalance
          ? fill(t('liveTierNeedsBalance'), { amount: formatCredits(e.min_balance) })
          : e.available
            ? ''
            : t('liveTierBusy');
        if (why) {
          const note = span(doc, 'lc-tier-note', why);
          note.id = `${id}-why`;
          input.setAttribute('aria-describedby', note.id);
          body.append(note);
        }
        label.append(input, body);
        return label;
      }),
    );
    if (opts.hint) opts.hint.textContent = list.engines.some((e) => e.scales_per_language) ? t('engineCostPerLanguage') : '';
  };

  draw();

  return {
    get: () => selected,
    setEngines(next) {
      list = next;
      // A click saves the choice, so the remembered one is always the latest one.
      selected = resolveTier(opts.load(), next, opts.balance());
      draw();
      opts.onChange?.(selected);
    },
  };
}
