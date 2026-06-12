//! Shared message types for client <-> server communication and for parsing
//! Deepgram streaming responses.
//!
//! V2: video-meeting model. Every peer speaks, listens, and connects P2P via
//! WebRTC; the server relays signaling, fans out translations, and relays chat.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// --- Client -> Server ------------------------------------------------------

/// Messages a peer sends as JSON text frames. (Audio is sent as binary frames.)
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Begin a speaking session (opens a fresh Deepgram connection).
    Start,
    /// End the speaking session (flush + close Deepgram).
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
    /// Manual language correction (spec 0012): overrides a wrong auto-detect
    /// result. The client restarts capture so the next Deepgram stream opens
    /// with the new language.
    SetLang {
        lang: String,
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
    /// Avatar URL for authenticated peers; absent for guests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
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
    },
    /// A new peer joined (sent to the others).
    PeerJoined {
        peer_id: String,
        user_name: String,
        lang: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        avatar_url: Option<String>,
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

    /// Room-glossary status (spec 0011): sent to a joining peer when the room
    /// has a glossary, and re-broadcast after edits. `entries == 0` (after a
    /// delete) hides the in-call badge.
    GlossaryActive {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        entries: usize,
    },

    /// A peer's language was resolved (spec 0012): auto-detect completed
    /// (`confidence` from Deepgram) or a manual `set_lang` correction
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
}

// --- Lobby (GET /rooms) ----------------------------------------------------

/// One online participant as shown in the public lobby.
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub name: String,
    pub lang: String,
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

// --- Deepgram streaming response parsing -----------------------------------

/// A Deepgram streaming message. We only model the fields we use; everything
/// else (Metadata, UtteranceEnd, SpeechStarted, ...) deserializes with defaults.
#[derive(Debug, Deserialize)]
pub struct DeepgramResponse {
    #[serde(rename = "type", default)]
    pub msg_type: String,
    #[serde(default)]
    pub is_final: bool,
    #[serde(default)]
    pub channel: Option<DeepgramChannel>,
}

#[derive(Debug, Deserialize)]
pub struct DeepgramChannel {
    #[serde(default)]
    pub alternatives: Vec<DeepgramAlternative>,
}

#[derive(Debug, Deserialize)]
pub struct DeepgramAlternative {
    #[serde(default)]
    pub transcript: String,
    #[serde(default)]
    pub confidence: f32,
}

impl DeepgramResponse {
    /// Returns the best (first) alternative's transcript + confidence if this is
    /// a non-empty `Results` message, else `None`.
    pub fn best_alternative(&self) -> Option<(&str, f32)> {
        if self.msg_type != "Results" {
            return None;
        }
        let alt = self.channel.as_ref()?.alternatives.first()?;
        let text = alt.transcript.trim();
        if text.is_empty() {
            return None;
        }
        Some((text, alt.confidence))
    }
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
        }
        .to_json();
        assert!(off.contains("\"type\":\"room_joined\""));
        assert!(!off.contains("session_id"), "omitted when recording is off");
        let on = ServerMessage::RoomJoined {
            peer_id: "p".into(),
            peers: vec![],
            session_id: Some("abc-123".into()),
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
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"bogus"}"#).is_err());
    }

    #[test]
    fn deepgram_best_alternative() {
        let ok = r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"ciao","confidence":0.9}]}}"#;
        let parsed = serde_json::from_str::<DeepgramResponse>(ok).unwrap();
        let (t, c) = parsed.best_alternative().unwrap();
        assert_eq!(t, "ciao");
        assert!((c - 0.9).abs() < 1e-3);

        let empty = r#"{"type":"Results","channel":{"alternatives":[{"transcript":"  ","confidence":0.4}]}}"#;
        assert!(serde_json::from_str::<DeepgramResponse>(empty)
            .unwrap()
            .best_alternative()
            .is_none());

        let meta = r#"{"type":"Metadata"}"#;
        assert!(serde_json::from_str::<DeepgramResponse>(meta)
            .unwrap()
            .best_alternative()
            .is_none());

        let no_alt = r#"{"type":"Results","channel":{"alternatives":[]}}"#;
        assert!(serde_json::from_str::<DeepgramResponse>(no_alt)
            .unwrap()
            .best_alternative()
            .is_none());
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
