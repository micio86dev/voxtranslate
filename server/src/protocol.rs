//! Shared message types for client <-> server communication and for parsing
//! upstream engine events.
//!
//! V2: video-meeting model. Every peer speaks, listens, and connects P2P via
//! WebRTC; the server relays signaling, fans out translations, and relays chat.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A collaborative-whiteboard operation (spec 0045). Coordinates are NORMALISED
/// to 0..1 so each client scales them to its own canvas size (no distortion
/// across desktop/mobile). `Draw` carries a batch of points for one stroke `id`
/// (strokes stream as several batches while drawing); `Clear` wipes the board.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WhiteboardOp {
    Draw {
        id: String,
        tool: String, // "pen" | "eraser"
        color: String,
        width: f32,
        points: Vec<[f32; 2]>,
    },
    Clear,
}

// --- Client -> Server ------------------------------------------------------

/// Messages a peer sends as JSON text frames. (Audio is sent as binary frames.)
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Begin a speaking session (opens a fresh upstream engine session).
    Start,
    /// End the speaking session (flush + close the upstream session).
    Stop,
    /// WebRTC signaling, relayed verbatim to peer `to` (server adds `from`).
    Offer {
        to: String,
        sdp: String,
    },
    Answer {
        to: String,
        sdp: String,
    },
    Ice {
        to: String,
        candidate: serde_json::Value,
    },
    /// A chat message to be translated and broadcast to the room.
    Chat {
        text: String,
    },
    /// Local mute state, broadcast to peers for UI indicators.
    MuteAudio {
        muted: bool,
    },
    MuteVideo {
        muted: bool,
    },
    /// An emoji reaction sent to the room (no translation needed).
    Emoji {
        emoji: String,
    },
    /// Toggle hand-raise state, broadcast to peers.
    HandRaise {
        raised: bool,
    },
    /// Toggle screen-share state, broadcast to peers (spec 0033) so they can mark
    /// the tile (badge + mobile pan/zoom). The media itself flows over WebRTC.
    /// `audio` is true only when the sharer routed the shared tab/system audio to
    /// peers (spec 0085): peers must keep that track audible even across a language
    /// gap, whereas a share without audio still carries the bare mic and must stay
    /// duckable so the original voice doesn't double the TTS (#229 follow-up).
    ScreenShare {
        active: bool,
        #[serde(default)]
        audio: bool,
    },
    /// Manual language correction (spec 0012): overrides a wrong auto-detect
    /// result. The client restarts capture so the next upstream session opens
    /// with the new language.
    SetLang {
        lang: String,
    },
    /// A whiteboard op (spec 0045): relayed to the others and persisted in the
    /// room so a later joiner gets the current drawing.
    Whiteboard {
        op: WhiteboardOp,
    },
    /// A mini-game state update (spec 0046). The server is game-agnostic: it just
    /// relays `state` and keeps the latest for late-joiners. `state == null` ends
    /// the game. Game rules (turns/win) live in the client.
    Game {
        state: serde_json::Value,
    },
    /// The client-direct "Enhanced" (Cartesia) pipeline gave up translating a remote
    /// speaker in-browser (spec 0108): a permanent error, or a transient one that
    /// survived all retries — e.g. Cartesia's concurrency limit (429). Ask the server to
    /// fall this listener back to server-side Standard translation (and re-bill at the
    /// Standard rate). `speaker_id` is for logging only; the downgrade is whole-listener.
    EnhancedFallback {
        speaker_id: String,
    },
    /// Translate one finalized Enhanced transcript (spec 0108). Cartesia does STT + TTS but
    /// NOT translation, so the client-direct listener sends each finalized source-language
    /// segment here and the server replies with [`ServerMessage::TranslatedText`] (Groq,
    /// uncached). `request_id` correlates the reply; text-only — no audio crosses the server.
    TranslateText {
        request_id: String,
        text: String,
        source: String,
        target: String,
    },
}

// --- Server -> Client ------------------------------------------------------

/// A file attached to a chat message (spec 0018). The bytes live in Supabase
/// Storage; `url` is the public link. `content_type` lets the client choose a
/// renderer (inline `<audio>` for audio, a download chip otherwise).
#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub url: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
}

/// Lightweight peer descriptor sent in `room_joined`.
#[derive(Debug, Clone, Serialize)]
pub struct PeerInfo {
    pub id: String,
    pub user_name: String,
    pub lang: String,
    /// The peer's account id, present only for authenticated (non-guest) peers. Lets
    /// others send an in-call friend request (spec: friends). Absent for guests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<uuid::Uuid>,
    /// Avatar URL for authenticated peers; absent for guests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// The peer's Cartesia cloned-voice id (spec 0108), if they have one. Propagated so an
    /// Enhanced listener can render THIS speaker's translated audio in their own voice.
    /// Absent for guests / users who haven't cloned a voice (→ a default TTS voice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cartesia_voice_id: Option<String>,
}

/// Messages the server pushes to peers as JSON text frames.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent to a peer right after joining: its own id + the existing peers.
    /// `session_id` is present iff transcript recording is on for this call
    /// (the DB is configured), and identifies the downloadable transcript.
    RoomJoined {
        peer_id: String,
        peers: Vec<PeerInfo>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// The room's real visibility, so the client shows the true public/private
        /// state instead of the joiner's own toggle (#50).
        public: bool,
    },
    /// A new peer joined (sent to the others).
    PeerJoined {
        peer_id: String,
        user_name: String,
        lang: String,
        /// The peer's account id (authenticated peers only) — see [`PeerInfo`].
        #[serde(skip_serializing_if = "Option::is_none")]
        user_id: Option<uuid::Uuid>,
        #[serde(skip_serializing_if = "Option::is_none")]
        avatar_url: Option<String>,
        /// The peer's Cartesia cloned-voice id (spec 0108), if any — see [`PeerInfo`].
        #[serde(skip_serializing_if = "Option::is_none")]
        cartesia_voice_id: Option<String>,
    },
    /// A peer left.
    PeerLeft {
        peer_id: String,
    },
    /// The room already has the maximum number of peers; the join is rejected.
    RoomFull,

    /// WebRTC signaling relayed from peer `from`.
    Offer {
        from: String,
        sdp: String,
    },
    Answer {
        from: String,
        sdp: String,
    },
    Ice {
        from: String,
        candidate: serde_json::Value,
    },

    /// A translated chat message (broadcast to everyone, including the sender).
    /// When it carries a file (spec 0018), `attachment` is present and `original`
    /// holds the file's extracted/transcribed text (possibly empty).
    ChatMessage {
        sender_id: String,
        sender_name: String,
        sender_lang: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sender_avatar: Option<String>,
        original: String,
        translations: HashMap<String, String>,
        timestamp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        attachment: Option<Attachment>,
    },

    /// A peer toggled audio/video; `kind` is "audio" or "video".
    PeerMuted {
        peer_id: String,
        kind: String,
        muted: bool,
    },

    /// An emoji reaction from a peer, broadcast to everyone.
    EmojiReaction {
        peer_id: String,
        peer_name: String,
        emoji: String,
    },

    /// A peer raised or lowered their hand.
    HandRaised {
        peer_id: String,
        raised: bool,
    },

    /// A peer started or stopped sharing their screen (spec 0033). `audio` is true
    /// only when shared tab/system audio rides the WebRTC audio track (spec 0085),
    /// so receivers keep it audible instead of ducking it for the translated voice.
    ScreenShare {
        peer_id: String,
        active: bool,
        audio: bool,
    },

    /// A whiteboard op from a peer, relayed to the others (spec 0045).
    Whiteboard {
        peer_id: String,
        op: WhiteboardOp,
    },
    /// The room's current whiteboard op-log, sent to a peer on join so it sees
    /// the existing drawing (spec 0045).
    WhiteboardSnapshot {
        ops: Vec<WhiteboardOp>,
    },

    /// A mini-game state update from a peer, relayed to the others (spec 0046).
    Game {
        peer_id: String,
        state: serde_json::Value,
    },
    /// The room's current mini-game state, sent to a peer on join (spec 0046).
    GameSnapshot {
        state: serde_json::Value,
    },

    /// Room-glossary status (spec 0011): sent to a joining peer when the room
    /// has a glossary, and re-broadcast after edits. `entries == 0` (after a
    /// delete) hides the in-call badge.
    GlossaryActive {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        entries: usize,
    },

    /// A peer's language was resolved (spec 0012): auto-detect completed
    /// (`confidence` from the detector) or a manual `set_lang` correction
    /// (`confidence` omitted). Broadcast so peers update language badges.
    LanguageDetected {
        peer_id: String,
        lang: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },

    /// Live partial transcript for a speaker (original language), broadcast so
    /// everyone can show it on the speaker's video cell.
    SubtitleInterim {
        speaker_id: String,
        speaker_name: String,
        text: String,
        lang: String,
        /// The speaker's ORIGINAL words for this partial, when `text` is a translation.
        ///
        /// A cross-language listener used to receive only the translated partial: the
        /// live caption streamed, but the dual-language ORIGINAL line could not appear
        /// until `subtitle_final`, a full idle-gap later. Clients showing both languages
        /// looked like the original was lagging seconds behind the translation, because
        /// it was — nothing carried it.
        ///
        /// `None` when `text` IS the original (a same-language listener, or a tier whose
        /// partials are raw transcript), so a client can always tell the two apart.
        #[serde(skip_serializing_if = "Option::is_none")]
        original: Option<String>,
    },
    /// Finalized transcript + translations into every language in the room.
    /// Each client renders `translations[my_lang]` (falling back to `original`).
    SubtitleFinal {
        speaker_id: String,
        speaker_name: String,
        original: String,
        lang: String,
        translations: HashMap<String, String>,
    },

    /// A chunk of **premium translated audio** (spec 0093) for the listeners of
    /// `lang`: PCM16 mono @ 24 kHz, base64-encoded. The client plays it via an
    /// AudioWorklet — NOT browser TTS. `seq` orders chunks within the speaker's
    /// stream (the client drops out-of-order/duplicate frames).
    TranslatedAudio {
        speaker_id: String,
        lang: String,
        seq: u64,
        pcm16_b64: String,
    },

    /// Talk to Anyone (spec 0110): the conversation's direction has been resolved for
    /// the current utterance. `spoken` is the language just heard, `target` the one the
    /// translation comes out in.
    ///
    /// Purely a UI signal — the gating it reports has already happened server-side. It is
    /// what turns the status line into "🇮🇹 Italian detected", and it is deliberately
    /// sent only once a direction is COMMITTED: while the resolver is still undecided the
    /// client shows "Listening…" rather than a guess it may have to retract (brief §16).
    /// No confidence value is exposed — that is a diagnostic, not user-facing.
    TalkDirection {
        spoken: String,
        target: String,
    },

    /// Live credit balance after a usage deduction (sent only to the speaker).
    BalanceUpdate {
        balance: f64,
    },
    /// Balance fell below the low-balance threshold (warn the speaker once).
    LowBalance {
        balance: f64,
    },
    /// Credits exhausted: the speaking session (audio → STT) was stopped. The
    /// WebRTC call itself stays up; the user can buy credits and resume.
    BalanceExhausted,

    /// The translation of a [`ClientMessage::TranslateText`] request (spec 0108), echoed
    /// back to the requesting Enhanced listener only. `request_id` matches the request; the
    /// listener then speaks `text` via Cartesia TTS in the speaker's cloned voice.
    TranslatedText {
        request_id: String,
        text: String,
    },

    /// A speaker's translation engine was switched mid-call (spec 0093) — e.g.
    /// Premium downgraded to the cheaper default when its credits ran low.
    /// Broadcast to the room: the speaker (`peer_id == me`) switches their capture
    /// and shows a notice; listeners stop expecting that speaker's premium audio,
    /// so browser TTS resumes for them. `reason` lets the client tailor the notice.
    EngineDowngraded {
        peer_id: String,
        from: String,
        to: String,
        reason: String,
    },

    /// Listener-pays (spec 0099): tells THIS peer which audio format to capture in
    /// when they speak. `pcm = true` → PCM16/24k (a Premium listener is present, so
    /// every live tier is speech-to-speech now); `false` → WebM/Opus
    /// (spec 0043, the default). Pushed on join and whenever the room's Premium
    /// composition changes. Only sent when the listener-pays model is active, so its
    /// presence also signals the client to follow server-driven capture.
    CaptureFormat {
        pcm: bool,
    },

    /// A message (spoken or chat) was blocked by moderation; warn the sender.
    ModerationWarning {
        message: String,
    },

    /// Non-fatal error surfaced to a peer. `code` lets the client branch (e.g.
    /// `insufficient_balance` → show the buy-credits prompt).
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        code: Option<String>,
    },
}

impl ServerMessage {
    /// Serialize to a JSON string for sending over a text frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"type":"error","message":"serialization failed"}"#.to_string())
    }
}

/// Query parameters for the `/ws` upgrade route:
/// `/ws?room=..&lang=..&name=..&id=..&public=..`
///
/// Every peer is symmetric (speaks and listens). `lang` is the single language
/// the peer speaks and receives in. `public` sets visibility on room creation.
#[derive(Debug, Clone, Deserialize)]
pub struct WsParams {
    pub room: String,
    pub lang: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub public: Option<bool>,
    /// Optional session JWT. Absent → guest (no billing). Present + valid → the
    /// peer is a billed user; invalid → the connection is rejected.
    #[serde(default)]
    pub token: Option<String>,
    /// Translation engine id chosen on the pre-join screen (spec 0093). Absent →
    /// the default engine; an unknown/removed id also falls back to the default.
    #[serde(default)]
    pub engine: Option<String>,
}

// --- Lobby (GET /rooms) ----------------------------------------------------

/// One online participant as shown in the public lobby.
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub name: String,
    pub lang: String,
    /// Google avatar URL for authenticated users; `None` for guests (spec 0072).
    pub avatar: Option<String>,
}

/// A public room with its currently online members.
#[derive(Debug, Clone, Serialize)]
pub struct PublicRoom {
    pub room: String,
    pub count: usize,
    pub participants: Vec<Member>,
}

/// Response body for `GET /rooms`.
#[derive(Debug, Clone, Serialize)]
pub struct RoomsResponse {
    pub rooms: Vec<PublicRoom>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_joined_session_id_present_iff_recording() {
        let off = ServerMessage::RoomJoined {
            peer_id: "p".into(),
            peers: vec![],
            session_id: None,
            public: false,
        }
        .to_json();
        assert!(off.contains("\"type\":\"room_joined\""));
        assert!(!off.contains("session_id"), "omitted when recording is off");
        let on = ServerMessage::RoomJoined {
            peer_id: "p".into(),
            peers: vec![],
            session_id: Some("abc-123".into()),
            public: true,
        }
        .to_json();
        assert!(on.contains("\"session_id\":\"abc-123\""));
    }

    #[test]
    fn server_message_type_tags() {
        assert!(ServerMessage::RoomFull
            .to_json()
            .contains("\"type\":\"room_full\""));
        assert!(ServerMessage::PeerLeft {
            peer_id: "p".into()
        }
        .to_json()
        .contains("\"type\":\"peer_left\""));
        let m = ServerMessage::PeerMuted {
            peer_id: "a".into(),
            kind: "audio".into(),
            muted: true,
        }
        .to_json();
        assert!(m.contains("\"type\":\"peer_muted\"") && m.contains("\"muted\":true"));
        let s = ServerMessage::SubtitleFinal {
            speaker_id: "s".into(),
            speaker_name: "n".into(),
            original: "ciao".into(),
            lang: "it".into(),
            translations: std::collections::HashMap::new(),
        }
        .to_json();
        assert!(s.contains("\"type\":\"subtitle_final\"") && s.contains("\"original\":\"ciao\""));
        assert!(ServerMessage::Error {
            message: "x".into(),
            code: None,
        }
        .to_json()
        .contains("\"type\":\"error\""));
        // `code` is omitted when None, present when Some.
        let no_code = ServerMessage::Error {
            message: "x".into(),
            code: None,
        }
        .to_json();
        assert!(!no_code.contains("code"));
        let coded = ServerMessage::Error {
            message: "broke".into(),
            code: Some("insufficient_balance".into()),
        }
        .to_json();
        assert!(coded.contains("\"code\":\"insufficient_balance\""));

        // Balance messages.
        assert!(ServerMessage::BalanceUpdate { balance: 1.5 }
            .to_json()
            .contains("\"type\":\"balance_update\""));
        assert!(ServerMessage::LowBalance { balance: 0.4 }
            .to_json()
            .contains("\"type\":\"low_balance\""));
        assert!(ServerMessage::BalanceExhausted
            .to_json()
            .contains("\"type\":\"balance_exhausted\""));
        // Engine downgrade (spec 0093): carries who + from/to + reason.
        let dg = ServerMessage::EngineDowngraded {
            peer_id: "p".into(),
            from: "premium".into(),
            to: "standard".into(),
            reason: "low_balance".into(),
        }
        .to_json();
        assert!(dg.contains("\"type\":\"engine_downgraded\""));
        assert!(dg.contains("\"from\":\"premium\"") && dg.contains("\"to\":\"standard\""));
        assert!(dg.contains("\"reason\":\"low_balance\""));

        // Enhanced translate-hop reply (spec 0108): tagged + carries request id + text.
        let tt = ServerMessage::TranslatedText {
            request_id: "r1".into(),
            text: "hello".into(),
        }
        .to_json();
        assert!(tt.contains("\"type\":\"translated_text\""));
        assert!(tt.contains("\"request_id\":\"r1\"") && tt.contains("\"text\":\"hello\""));

        // Emoji reactions + hand-raise (PR #1).
        let e = ServerMessage::EmojiReaction {
            peer_id: "a".into(),
            peer_name: "Alice".into(),
            emoji: "👍".into(),
        }
        .to_json();
        assert!(e.contains("\"type\":\"emoji_reaction\"") && e.contains("\"emoji\":\"👍\""));
        let h = ServerMessage::HandRaised {
            peer_id: "a".into(),
            raised: true,
        }
        .to_json();
        assert!(h.contains("\"type\":\"hand_raised\"") && h.contains("\"raised\":true"));

        // Language detection (spec 0012): confidence omitted on manual set_lang.
        let det = ServerMessage::LanguageDetected {
            peer_id: "a".into(),
            lang: "it".into(),
            confidence: Some(0.97),
        }
        .to_json();
        assert!(det.contains("\"type\":\"language_detected\""));
        assert!(det.contains("\"lang\":\"it\"") && det.contains("\"confidence\":0.97"));
        let manual = ServerMessage::LanguageDetected {
            peer_id: "a".into(),
            lang: "fr".into(),
            confidence: None,
        }
        .to_json();
        assert!(!manual.contains("confidence"));

        // Chat attachment (spec 0018): present iff a file is attached.
        let plain = ServerMessage::ChatMessage {
            sender_id: "a".into(),
            sender_name: "Alice".into(),
            sender_lang: "it".into(),
            sender_avatar: None,
            original: "ciao".into(),
            translations: std::collections::HashMap::new(),
            timestamp: 1,
            attachment: None,
        }
        .to_json();
        assert!(plain.contains("\"type\":\"chat_message\""));
        assert!(!plain.contains("attachment"), "omitted when None");
        let with_file = ServerMessage::ChatMessage {
            sender_id: "a".into(),
            sender_name: "Alice".into(),
            sender_lang: "it".into(),
            sender_avatar: None,
            original: String::new(),
            translations: std::collections::HashMap::new(),
            timestamp: 1,
            attachment: Some(crate::protocol::Attachment {
                url: "https://x/storage/v1/object/public/chat-files/s/f.mp3".into(),
                name: "memo.mp3".into(),
                content_type: "audio/mpeg".into(),
                size: 12345,
            }),
        }
        .to_json();
        assert!(with_file.contains("\"attachment\""));
        assert!(
            with_file.contains("\"name\":\"memo.mp3\"") && with_file.contains("\"size\":12345")
        );

        // Premium translated audio (spec 0093): tagged + carries base64 PCM.
        let ta = ServerMessage::TranslatedAudio {
            speaker_id: "s".into(),
            lang: "en".into(),
            seq: 7,
            pcm16_b64: "AAAB".into(),
        }
        .to_json();
        assert!(ta.contains("\"type\":\"translated_audio\""));
        assert!(ta.contains("\"lang\":\"en\"") && ta.contains("\"seq\":7"));
        assert!(ta.contains("\"pcm16_b64\":\"AAAB\""));

        // Glossary badge (spec 0011): name omitted when None.
        let g = ServerMessage::GlossaryActive {
            name: Some("Legal".into()),
            entries: 12,
        }
        .to_json();
        assert!(g.contains("\"type\":\"glossary_active\""));
        assert!(g.contains("\"name\":\"Legal\"") && g.contains("\"entries\":12"));
        let unnamed = ServerMessage::GlossaryActive {
            name: None,
            entries: 0,
        }
        .to_json();
        assert!(!unnamed.contains("name"));
    }

    #[test]
    fn client_message_parsing() {
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"start"}"#).unwrap(),
            ClientMessage::Start
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"stop"}"#).unwrap(),
            ClientMessage::Stop
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"chat","text":"hi"}"#).unwrap(),
            ClientMessage::Chat { .. }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"offer","to":"p","sdp":"s"}"#)
                .unwrap(),
            ClientMessage::Offer { .. }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"answer","to":"p","sdp":"s"}"#)
                .unwrap(),
            ClientMessage::Answer { .. }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"ice","to":"p","candidate":{}}"#)
                .unwrap(),
            ClientMessage::Ice { .. }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"mute_audio","muted":true}"#).unwrap(),
            ClientMessage::MuteAudio { muted: true }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"mute_video","muted":false}"#)
                .unwrap(),
            ClientMessage::MuteVideo { muted: false }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"emoji","emoji":"👍"}"#).unwrap(),
            ClientMessage::Emoji { emoji } if emoji == "👍"
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"hand_raise","raised":true}"#)
                .unwrap(),
            ClientMessage::HandRaise { raised: true }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"set_lang","lang":"it"}"#).unwrap(),
            ClientMessage::SetLang { lang } if lang == "it"
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(
                r#"{"type":"enhanced_fallback","speaker_id":"p1"}"#
            )
            .unwrap(),
            ClientMessage::EnhancedFallback { speaker_id } if speaker_id == "p1"
        ));
        // Enhanced translate hop (spec 0108): carries request id + text + langs.
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(
                r#"{"type":"translate_text","request_id":"r1","text":"ciao","source":"it","target":"en"}"#
            )
            .unwrap(),
            ClientMessage::TranslateText { request_id, source, target, .. }
                if request_id == "r1" && source == "it" && target == "en"
        ));
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"bogus"}"#).is_err());
    }

    #[test]
    fn subtitle_interim_carries_the_original_only_when_text_is_a_translation() {
        // A cross-language listener gets an already-translated partial. Without the
        // speaker's original riding along, a dual-language client cannot show the source
        // line until `subtitle_final` — a whole idle gap later, which read as the
        // original lagging seconds behind its own translation.
        let translated = ServerMessage::SubtitleInterim {
            speaker_id: "s".into(),
            speaker_name: "Speaker".into(),
            text: "hello everyone".into(),
            lang: "it".into(),
            original: Some("ciao a tutti".into()),
        }
        .to_json();
        assert!(translated.contains("\"original\":\"ciao a tutti\""));

        // A same-language listener's partial IS the original, so the field is OMITTED
        // rather than duplicating `text` — that is how a client tells the two apart.
        let raw = ServerMessage::SubtitleInterim {
            speaker_id: "s".into(),
            speaker_name: "Speaker".into(),
            text: "ciao a tutti".into(),
            lang: "it".into(),
            original: None,
        }
        .to_json();
        assert!(!raw.contains("original"), "absent, not null: {raw}");
    }

    #[test]
    fn ws_params_optional_fields() {
        let p: WsParams = serde_json::from_str(r#"{"room":"r","lang":"it"}"#).unwrap();
        assert_eq!(p.room, "r");
        assert_eq!(p.lang, "it");
        assert!(p.name.is_none() && p.id.is_none() && p.public.is_none());
        let p2: WsParams =
            serde_json::from_str(r#"{"room":"r","lang":"it","name":"A","id":"x","public":true}"#)
                .unwrap();
        assert_eq!(p2.public, Some(true));
    }
}
