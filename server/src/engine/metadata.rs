//! Engine descriptors: the internal [`EngineMetadata`] each engine declares, and
//! the public [`EngineInfo`] DTO exposed to clients via `GET /api/engines`.
//!
//! The split mirrors the billing convention (`config::PricingConfig`): the raw
//! per-minute **cost** and the **markup** are server-only and are NEVER
//! serialized — clients see only the computed user-facing **rate**
//! (`cost × (1 + markup)`).

use serde::Serialize;

/// What an engine can do, surfaced so the UI can adapt (e.g. a "natural voice"
/// badge) without hard-coding engine ids.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EngineCapabilities {
    /// The engine streams translated **audio** back (premium speech-to-speech).
    /// `false` means subtitles only and the client synthesizes voice via TTS.
    pub translated_audio: bool,
    /// Cost scales with the number of distinct target languages in the room
    /// (spec 0093): the engine opens one paid upstream session per language, so the
    /// speaker is billed per translation stream rather than a flat rate. Now true of
    /// EVERY server-side tier including Standard, which stopped being flat-rate when it
    /// moved from Deepgram+Groq to Qwen realtime.
    ///
    /// Drives the meter multiplier AND — in both the engine selector and the
    /// language-first tier picker — the pre-join price note, so the user is told up front
    /// that the displayed per-minute rate is charged once PER TRANSLATION LANGUAGE and a
    /// group call therefore costs more. Leaving that note off a per-language tier is a
    /// pricing-transparency bug, not a cosmetic one.
    pub cost_scales_per_language: bool,
    /// The browser connects DIRECTLY to the provider (spec 0108, Cartesia "Enhanced"):
    /// the server mints a scoped access token and never proxies the audio. Such an engine
    /// is NOT run on the server speaking path — its listeners translate locally — so
    /// the call sites that fan a speaker's audio to server engines, and the Standard
    /// "serve everyone" delivery, must skip these peers (they'd otherwise be
    /// double-translated). Billing is unaffected: the listener meter bills them per
    /// active source at this engine's rate, just like any other engine.
    pub client_direct: bool,
    /// Largest room size the engine supports end-to-end (the mesh cap is 4).
    pub max_room_size: u8,
}

/// Internal, full description of a translation engine. Holds the **raw** cost and
/// markup used for billing; convert to [`EngineInfo`] before sending to a client.
#[derive(Debug, Clone)]
pub struct EngineMetadata {
    /// Stable id, persisted in `usage_sessions.engine_id` and sent in the join
    /// payload. Extensible string, not an enum — must never change once shipped.
    pub id: String,
    pub display_name: String,
    /// Coarse tier label for the UI (`"standard"` | `"premium"`).
    pub tier: String,
    pub description: String,
    /// Raw server cost per minute (USD) — NEVER serialized to a client.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (`0.5` = 50%) — NEVER serialized. The `*_PERCENT`
    /// env vars are divided by 100 before they reach here.
    pub markup: f64,
    /// Languages the engine can transcribe FROM (speak in).
    pub input_languages: Vec<String>,
    /// Languages the engine can translate INTO.
    pub output_languages: Vec<String>,
    pub capabilities: EngineCapabilities,
}

impl EngineMetadata {
    /// User-facing rate per minute = `cost × (1 + markup)`. The only price ever
    /// shown to a client.
    pub fn user_rate_per_minute(&self) -> f64 {
        self.cost_per_minute * (1.0 + self.markup)
    }

    /// User-facing rate per second — the meter charges per second.
    pub fn user_rate_per_second(&self) -> f64 {
        self.user_rate_per_minute() / 60.0
    }
}

/// Public, client-safe view of an engine for `GET /api/engines`. Carries the
/// computed `rate_per_minute` but neither the raw cost nor the markup.
#[derive(Debug, Clone, Serialize)]
pub struct EngineInfo {
    pub id: String,
    pub display_name: String,
    pub tier: String,
    pub description: String,
    /// USD/minute the user is charged (`cost × (1 + markup)`).
    pub rate_per_minute: f64,
    pub input_languages: Vec<String>,
    pub output_languages: Vec<String>,
    pub capabilities: EngineCapabilities,
}

impl From<&EngineMetadata> for EngineInfo {
    fn from(m: &EngineMetadata) -> Self {
        Self {
            id: m.id.clone(),
            display_name: m.display_name.clone(),
            tier: m.tier.clone(),
            description: m.description.clone(),
            rate_per_minute: m.user_rate_per_minute(),
            input_languages: m.input_languages.clone(),
            output_languages: m.output_languages.clone(),
            capabilities: m.capabilities,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> EngineMetadata {
        EngineMetadata {
            id: "standard".into(),
            display_name: "Standard".into(),
            tier: "standard".into(),
            description: "desc".into(),
            cost_per_minute: 0.008,
            markup: 0.25,
            input_languages: vec!["en".into(), "it".into()],
            output_languages: vec!["en".into(), "it".into()],
            capabilities: EngineCapabilities {
                translated_audio: false,
                cost_scales_per_language: false,
                client_direct: false,
                max_room_size: 4,
            },
        }
    }

    #[test]
    fn user_rate_is_cost_times_markup() {
        let m = meta();
        // 0.008 × 1.25 = 0.01 per minute.
        assert!((m.user_rate_per_minute() - 0.01).abs() < 1e-9);
        assert!((m.user_rate_per_second() - 0.01 / 60.0).abs() < 1e-12);
    }

    #[test]
    fn engine_info_never_leaks_cost_or_markup() {
        let info = EngineInfo::from(&meta());
        let json = serde_json::to_string(&info).unwrap();
        // Display rate is present...
        assert!(json.contains("\"rate_per_minute\""));
        // ...but the raw cost and markup never are (billing-internal).
        assert!(!json.contains("cost_per_minute"), "raw cost must not leak");
        assert!(!json.contains("markup"), "markup must not leak");
        // Sanity: the serialized rate is the computed user rate.
        assert!(json.contains("0.01"));
    }
}
