-- Per-user Vox Voices preferences (Kokoro local-TTS integration). Two additive,
-- backward-compatible columns:
--   tts_engine_pref — the chosen speech engine: 'auto' | 'browser' | 'vox'. Synced across
--                     devices so the user's choice follows them. NULL = default ('auto').
--   tts_voice_id    — the chosen VOX voice id (portable across devices). The browser-voice
--                     choice stays device-local (SpeechSynthesis voiceURIs aren't portable)
--                     and is deliberately NOT stored here. NULL = engine default voice.
-- Every `users` query is SELECT * / RETURNING *, so these column-backed fields map cleanly
-- onto the User row struct.
ALTER TABLE users ADD COLUMN IF NOT EXISTS tts_engine_pref TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS tts_voice_id TEXT;
