//! Which translation tier a Live Connect conversation runs on (spec 0121, S4).
//!
//! The host picks the tier when creating the conversation; it is stored as
//! `live_sessions.engine_id` and every seat of that conversation — the host's phone and each
//! guest's — uses it. The owner pays every seat at that tier's per-minute rate, per distinct
//! target language (the meter already works that way for every tier).
//!
//! Two tiers are servable today:
//!
//! * **Standard** (`standard`) — the default and the capacity fallback.
//! * **Premium** (`gemini_live_translate`) — Gemini Live speech to speech. It shares Standard's
//!   PCM input and `TranslatedAudio` output, so the same socket code drives it.
//!
//! **Enhanced** (`cartesia`) is client-direct: each phone runs Cartesia speech-to-text on its OWN
//! microphone and Cartesia text-to-speech for what it hears, the server translates the text in
//! between and opens no engine session (see [`super::enhanced`]). It is selectable when it is
//! registered (`CARTESIA_ENHANCED` plus the key) and the conversation is a consumer one. It is
//! deliberately NOT in [`SELECTABLE`] — that list is the tiers a server engine session can serve,
//! which is what [`resolve`] and the capacity gate reason with — and [`is_enhanced`] is the one
//! question that tells the two kinds of conversation apart.
//!
//! This module is pure policy (no I/O) apart from reading the registry, so the rules are unit
//! tested without a database.

use std::sync::Arc;

use crate::engine::{EngineRegistry, TranslationEngine, CARTESIA_ID, GEMINI_ID, STANDARD_ID};

/// The tier a conversation runs on when nobody chose one, and the one every business
/// conversation and every desk uses.
pub const DEFAULT_TIER: &str = STANDARD_ID;

/// Tiers a server engine session can serve, in display order. Enhanced is client-direct and
/// listed separately (see [`choose`] and [`is_enhanced`]).
pub const SELECTABLE: [&str; 2] = [STANDARD_ID, GEMINI_ID];

/// Why a requested tier was refused. The route turns each into its own stable `code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierError {
    /// Unknown id, not a Live Connect tier, or not enabled on this deployment.
    NotAvailable,
    /// A business conversation or a desk is Standard only.
    NotAllowedForBusiness,
}

impl TierError {
    /// The stable machine code of the 400 answer.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotAvailable => "engine_not_available",
            Self::NotAllowedForBusiness => "engine_not_allowed",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::NotAvailable => "this tier is not available",
            Self::NotAllowedForBusiness => "business conversations use the Standard tier",
        }
    }
}

/// Validate the tier a create request asked for and return the id to store.
///
/// `None` is Standard. The check is against what is actually registered (a tier whose flag or key
/// is not configured is absent from the registry), not against a hard-coded list alone. Enhanced
/// (`cartesia`) is accepted when registered and refused for a business conversation.
pub fn choose(
    engines: &EngineRegistry,
    requested: Option<&str>,
    business: bool,
) -> Result<String, TierError> {
    let Some(id) = requested else {
        return Ok(DEFAULT_TIER.to_string());
    };
    let id = id.trim();
    if id == CARTESIA_ID {
        if engines.get(CARTESIA_ID).is_none() {
            return Err(TierError::NotAvailable);
        }
        if business {
            return Err(TierError::NotAllowedForBusiness);
        }
        return Ok(CARTESIA_ID.to_string());
    }
    // The "Pro" tier (`OPENAI_ID`) is a real engine but not a Live Connect tier: it is refused
    // like any unknown id, so the answer cannot be used to probe what else is configured.
    if !SELECTABLE.contains(&id) {
        return Err(TierError::NotAvailable);
    }
    if engines.get(id).is_none_or(|e| is_client_direct(&*e)) {
        return Err(TierError::NotAvailable);
    }
    if business && id != DEFAULT_TIER {
        return Err(TierError::NotAllowedForBusiness);
    }
    Ok(id.to_string())
}

fn is_client_direct(engine: &dyn TranslationEngine) -> bool {
    engine.metadata().capabilities.client_direct
}

/// Whether a conversation stored as `stored` runs on Enhanced right now: it asked for it, the tier
/// is still registered, and it has not fallen back (`downgraded`) to the default. When this is
/// true no engine session serves anybody: phones send text, see [`super::enhanced`]. When Enhanced
/// was switched off since the conversation was created this is false and [`resolve`] hands the
/// default engine, so the conversation keeps working as Standard and is billed as Standard.
pub fn is_enhanced(engines: &EngineRegistry, stored: Option<&str>, downgraded: bool) -> bool {
    !downgraded && stored == Some(CARTESIA_ID) && engines.get(CARTESIA_ID).is_some()
}

/// The engine that serves a conversation whose stored tier is `stored`, given that `downgraded`
/// says Premium ran out of capacity and the whole conversation moved to the default.
///
/// A stored id that is no longer servable (the tier was switched off since, or the row predates
/// tiers and is `NULL`) falls back to the default engine rather than failing a conversation in
/// progress; the meter then bills what actually served it.
pub fn resolve(
    engines: &EngineRegistry,
    stored: Option<&str>,
    downgraded: bool,
) -> Arc<dyn TranslationEngine> {
    if downgraded {
        return engines.default();
    }
    match stored {
        Some(id) if SELECTABLE.contains(&id) => match engines.get(id) {
            Some(e) if !is_client_direct(&*e) => e,
            _ => {
                tracing::warn!(engine = %id, "live: stored tier is not servable; using the default");
                engines.default()
            }
        },
        _ => engines.default(),
    }
}

/// The engine whose capacity gates the seats of a conversation stored as `stored`: Enhanced's own
/// metadata-only engine (it holds no upstream session, so it never reports a limit) for an
/// Enhanced conversation, otherwise [`resolve`]. Without this an Enhanced conversation would be
/// turned away `live_busy` because Standard, which it does not use, is full.
pub fn capacity_engine(
    engines: &EngineRegistry,
    stored: Option<&str>,
    downgraded: bool,
) -> Arc<dyn TranslationEngine> {
    if is_enhanced(engines, stored, downgraded) {
        if let Some(e) = engines.get(CARTESIA_ID) {
            return e;
        }
    }
    resolve(engines, stored, downgraded)
}

/// Balance an owner must hold to open or Start a conversation on a tier.
///
/// Standard keeps the platform minimum exactly. A dearer tier needs the same **runway** — the
/// same number of seconds of one stream — as Standard's minimum buys, so the minimum scales with
/// the ratio of the tier's rate to Standard's. A tier that is not dearer needs no more than the
/// base, and a zero Standard rate (misconfiguration) cannot divide.
pub fn required_balance(
    base_min: f64,
    tier_rate_per_minute: f64,
    standard_rate_per_minute: f64,
) -> f64 {
    if standard_rate_per_minute <= 0.0 || tier_rate_per_minute <= standard_rate_per_minute {
        return base_min;
    }
    base_min * tier_rate_per_minute / standard_rate_per_minute
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GeminiConfig;
    use crate::engine::{PremiumEngine, StandardEngine, OPENAI_ID};

    fn registry(with_premium: bool) -> EngineRegistry {
        let mut r = EngineRegistry::new(STANDARD_ID);
        r.register(Arc::new(StandardEngine::new(&Default::default())));
        if with_premium {
            r.register(Arc::new(PremiumEngine::new(&GeminiConfig {
                api_key: "k".into(),
                model: "m".into(),
                cost_per_minute: 0.02,
                markup: 0.5,
                max_sessions: 4,
                live_base_url: "ws://127.0.0.1:1".into(),
                voice: None,
            })));
        }
        r
    }

    #[test]
    fn nothing_asked_is_standard() {
        assert_eq!(choose(&registry(false), None, false).unwrap(), "standard");
        assert_eq!(choose(&registry(true), None, true).unwrap(), "standard");
    }

    #[test]
    fn premium_needs_to_be_registered() {
        assert_eq!(
            choose(&registry(true), Some(GEMINI_ID), false).unwrap(),
            GEMINI_ID
        );
        assert_eq!(
            choose(&registry(false), Some(GEMINI_ID), false),
            Err(TierError::NotAvailable)
        );
    }

    #[test]
    fn enhanced_is_chosen_only_when_registered_and_never_for_business() {
        let cfg = || crate::config::CartesiaConfig {
            api_key: "k".into(),
            stt_model: "ink-2".into(),
            stt_model_by_lang: Default::default(),
            tts_model: "sonic-3.5".into(),
            cost_per_minute: 0.036,
            markup: 0.85,
            voice_cloning_enabled: true,
            default_voice_id: None,
            api_base: "http://127.0.0.1:1".into(),
            stt_endpoint: "wss://x/stt".into(),
            tts_endpoint: "wss://x/tts".into(),
            version: "v".into(),
        };
        let mut with = registry(true);
        with.register(Arc::new(crate::engine::CartesiaEngine::new(&cfg())));
        assert_eq!(
            choose(&with, Some(CARTESIA_ID), false).unwrap(),
            CARTESIA_ID
        );
        assert_eq!(
            choose(&with, Some(CARTESIA_ID), true),
            Err(TierError::NotAllowedForBusiness)
        );
        // Not registered here (CARTESIA_ENHANCED off): refused like any unknown tier.
        assert_eq!(
            choose(&registry(true), Some(CARTESIA_ID), false),
            Err(TierError::NotAvailable)
        );
        assert!(is_enhanced(&with, Some(CARTESIA_ID), false));
        assert!(
            !is_enhanced(&with, Some(CARTESIA_ID), true),
            "a fallen-back conversation is Standard"
        );
        assert!(!is_enhanced(&with, Some(STANDARD_ID), false));
        assert!(!is_enhanced(&with, None, false));
        assert!(
            !is_enhanced(&registry(true), Some(CARTESIA_ID), false),
            "a tier switched off since creation is not served as Enhanced"
        );
        // Whatever serves it on the server side is the default: Enhanced is never an engine here.
        assert_eq!(
            resolve(&with, Some(CARTESIA_ID), false).metadata().id,
            STANDARD_ID
        );
    }

    #[test]
    fn unknown_pro_and_blank_ids_are_not_available() {
        for id in ["nope", OPENAI_ID, "", "  ", "STANDARD"] {
            assert_eq!(
                choose(&registry(true), Some(id), false),
                Err(TierError::NotAvailable),
                "{id:?}"
            );
        }
    }

    #[test]
    fn business_conversations_stay_standard() {
        assert_eq!(
            choose(&registry(true), Some(GEMINI_ID), true),
            Err(TierError::NotAllowedForBusiness)
        );
        assert_eq!(
            choose(&registry(true), Some(STANDARD_ID), true).unwrap(),
            "standard"
        );
    }

    #[test]
    fn resolve_follows_the_stored_tier_and_degrades_to_the_default() {
        let r = registry(true);
        assert_eq!(resolve(&r, Some(GEMINI_ID), false).metadata().id, GEMINI_ID);
        assert_eq!(
            resolve(&r, Some(GEMINI_ID), true).metadata().id,
            STANDARD_ID
        );
        assert_eq!(resolve(&r, None, false).metadata().id, STANDARD_ID);
        assert_eq!(
            resolve(&r, Some("legacy"), false).metadata().id,
            STANDARD_ID
        );
        assert_eq!(
            resolve(&registry(false), Some(GEMINI_ID), false)
                .metadata()
                .id,
            STANDARD_ID,
            "a tier switched off since creation must not break the conversation"
        );
    }

    #[test]
    fn the_minimum_balance_scales_with_the_rate_and_standard_keeps_its_own() {
        assert_eq!(required_balance(0.5, 0.01, 0.01), 0.5);
        assert_eq!(
            required_balance(0.5, 0.005, 0.01),
            0.5,
            "never below the base"
        );
        assert!((required_balance(0.5, 0.0345, 0.01) - 1.725).abs() < 1e-9);
        assert_eq!(required_balance(0.5, 0.0345, 0.0), 0.5);
    }
}
