//! Webinar presence (SPEC "Webinar Mode" Fase 4): realtime audience count over an
//! Axum WebSocket + the persisted join/leave history.
//!
//! Each participant page (and the host studio) opens a WS to
//! `GET /api/w/{code}/presence`. The server holds an in-memory per-webinar set of
//! live connections; on join/leave it broadcasts the current audience count to
//! everyone in that webinar and records the event + a participant row. "Present" =
//! an open WS — best-effort: a backgrounded mobile tab that drops the socket
//! under-counts (SPEC §10 accepts this). Host-studio connections receive the count
//! but are not counted as audience.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::db::Pool;
use crate::AppState;

/// One live viewer/host connection.
struct Conn {
    is_host: bool,
    /// The viewer's chosen subtitle language (validated with `valid_lang` before
    /// storing). `None` for the host studio and for viewers that didn't send one.
    lang: Option<String>,
    tx: mpsc::UnboundedSender<Message>,
}

/// A realtime subtitle frame broadcast to viewers over the presence WS (Fase 2).
///
/// # Wire contract (EXACT — the participant client matches on `type` + `kind`)
///
/// A finalized utterance carries the original text, its source language, and the
/// full `{ lang: text }` translation map (always including the source language);
/// each viewer renders `translations[my_lang]`, falling back to `original`:
///
/// ```json
/// {"type":"subtitle","kind":"final","original":"<src text>","lang":"<source_language>",
///  "translations":{"en":"...","es":"..."}}
/// ```
///
/// A live partial is untranslated — just the source text and language:
///
/// ```json
/// {"type":"subtitle","kind":"interim","text":"<src text>","lang":"<source_language>"}
/// ```
///
/// Frames carry ONLY text + language — never any host PII (no `host_user_id`,
/// email, or org id). Subtitles originate solely from the STT processor; the
/// presence WS never turns an inbound viewer frame into a subtitle.
#[derive(Debug, Clone, PartialEq)]
pub enum SubtitleEvent {
    /// A live partial in the source language (untranslated).
    Interim { text: String, lang: String },
    /// A finalized utterance + its translation fan-out.
    Final {
        original: String,
        lang: String,
        translations: HashMap<String, String>,
    },
}

impl SubtitleEvent {
    /// Serialize to the exact JSON wire contract documented on [`SubtitleEvent`].
    pub fn to_json(&self) -> String {
        let value = match self {
            SubtitleEvent::Interim { text, lang } => json!({
                "type": "subtitle",
                "kind": "interim",
                "text": text,
                "lang": lang,
            }),
            SubtitleEvent::Final {
                original,
                lang,
                translations,
            } => json!({
                "type": "subtitle",
                "kind": "final",
                "original": original,
                "lang": lang,
                "translations": translations,
            }),
        };
        value.to_string()
    }
}

/// In-memory realtime presence, keyed by webinar code.
#[derive(Default)]
pub struct PresenceRegistry {
    rooms: DashMap<String, HashMap<u64, Conn>>,
    next: AtomicU64,
}

fn audience(room: &HashMap<u64, Conn>) -> usize {
    room.values().filter(|c| !c.is_host).count()
}

impl PresenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connection; returns its id.
    fn join(&self, code: &str, conn: Conn) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.rooms
            .entry(code.to_string())
            .or_default()
            .insert(id, conn);
        id
    }

    /// Remove a connection (cleaning up the room when it empties).
    fn leave(&self, code: &str, id: u64) {
        if let Some(mut room) = self.rooms.get_mut(code) {
            room.remove(&id);
            let empty = room.is_empty();
            drop(room);
            if empty {
                self.rooms.remove(code);
            }
        }
    }

    /// Current audience count (non-host connections) for a code.
    pub fn count(&self, code: &str) -> usize {
        self.rooms.get(code).map(|r| audience(&r)).unwrap_or(0)
    }

    /// Push the current audience count to every connection in the room.
    fn broadcast_count(&self, code: &str) {
        if let Some(room) = self.rooms.get(code) {
            let payload = json!({ "type": "count", "count": audience(&room) }).to_string();
            for c in room.values() {
                let _ = c.tx.send(Message::Text(payload.clone().into()));
            }
        }
    }

    /// Distinct subtitle languages of the current non-host viewers, read LIVE from
    /// the registry so a late joiner's language is picked up on the very next
    /// final. Host connections and viewers without a language are excluded.
    /// Order is unspecified (translation fan-out is order-insensitive).
    pub fn target_languages(&self, code: &str) -> Vec<String> {
        let Some(room) = self.rooms.get(code) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        room.values()
            .filter(|c| !c.is_host)
            .filter_map(|c| c.lang.as_deref())
            .filter(|l| seen.insert(l.to_string()))
            .map(str::to_string)
            .collect()
    }

    /// Broadcast a subtitle frame to the room's VIEWER connections (the host
    /// studio doesn't need its own subtitles). Fire-and-forget per connection: a
    /// closed viewer socket is silently skipped, exactly like `broadcast_count`.
    pub fn broadcast_subtitle(&self, code: &str, event: &SubtitleEvent) {
        if let Some(room) = self.rooms.get(code) {
            let payload = event.to_json();
            for c in room.values().filter(|c| !c.is_host) {
                let _ = c.tx.send(Message::Text(payload.clone().into()));
            }
        }
    }
}

#[derive(Deserialize)]
pub struct PresenceParams {
    #[serde(default)]
    guest_id: Option<String>,
    #[serde(default)]
    lang: Option<String>,
    /// The host studio watches the count but isn't part of the audience.
    #[serde(default)]
    host: bool,
}

/// `GET /api/w/{code}/presence` (WebSocket) — join the realtime presence channel.
pub async fn presence_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(params): Query<PresenceParams>,
) -> Response {
    ws.on_upgrade(move |socket| handle_presence(socket, state, code, params))
}

async fn handle_presence(socket: WebSocket, state: AppState, code: String, params: PresenceParams) {
    let guest_id = params
        .guest_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::new_v4);
    let is_host = params.host;
    // Validate the viewer's subtitle language server-side (it decides the
    // translation fan-out). A malformed value is dropped, not trusted — the viewer
    // simply gets no translated subtitle rather than injecting a bad language code.
    let lang = params
        .lang
        .as_deref()
        .map(str::trim)
        .filter(|l| crate::webinar::routes::valid_lang(l).is_ok())
        .map(str::to_string);

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // Resolve the webinar id up front for the DB history (best-effort).
    let webinar_id = match state.pool.as_ref() {
        Some(p) => crate::webinar::find_by_code(p, &code)
            .await
            .ok()
            .flatten()
            .map(|w| w.id),
        None => None,
    };

    let conn_id = state.webinar_presence.join(
        &code,
        Conn {
            is_host,
            lang: lang.clone(),
            tx,
        },
    );
    state.webinar_presence.broadcast_count(&code);

    // Persist the join + a participant row (audience only, best-effort).
    let participant_id = match (state.pool.as_ref(), webinar_id) {
        (Some(p), Some(wid)) if !is_host => record_join(p, wid, guest_id, lang.as_deref()).await,
        _ => None,
    };

    // Pump outgoing count messages to the socket.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Drain until the socket closes, discarding every inbound frame. This is a
    // security boundary, not a TODO: subtitles originate ONLY from the STT
    // processor (`broadcast_subtitle`), so a viewer can never inject or spoof a
    // subtitle by writing to their own presence socket.
    while let Some(Ok(msg)) = ws_rx.next().await {
        if matches!(msg, Message::Close(_)) {
            break;
        }
    }

    // Disconnect: deregister, refresh the count, persist the leave.
    send_task.abort();
    state.webinar_presence.leave(&code, conn_id);
    state.webinar_presence.broadcast_count(&code);
    if let (Some(p), Some(wid)) = (state.pool.as_ref(), webinar_id) {
        if !is_host {
            record_leave(p, wid, participant_id).await;
        }
    }
}

/// Upsert the participant + log a `join` event. Returns the participant id.
async fn record_join(
    pool: &Pool,
    webinar_id: Uuid,
    guest_id: Uuid,
    lang: Option<&str>,
) -> Option<Uuid> {
    let pid: Uuid = sqlx::query_scalar(
        "INSERT INTO webinar_participants
            (webinar_id, guest_id, language_code, first_seen, last_seen, joined_at)
         VALUES ($1, $2, $3, now(), now(), now())
         ON CONFLICT (webinar_id, guest_id) DO UPDATE
            SET last_seen = now(),
                language_code = COALESCE($3, webinar_participants.language_code)
         RETURNING id",
    )
    .bind(webinar_id)
    .bind(guest_id)
    .bind(lang)
    .fetch_one(pool)
    .await
    .ok()?;
    let _ = sqlx::query(
        "INSERT INTO webinar_events (webinar_id, participant_id, type, language_code)
         VALUES ($1, $2, 'join', $3)",
    )
    .bind(webinar_id)
    .bind(pid)
    .bind(lang)
    .execute(pool)
    .await;
    Some(pid)
}

/// Log a `leave` event + touch `last_seen` (best-effort).
async fn record_leave(pool: &Pool, webinar_id: Uuid, participant_id: Option<Uuid>) {
    if let Some(pid) = participant_id {
        let _ = sqlx::query("UPDATE webinar_participants SET last_seen = now() WHERE id = $1")
            .bind(pid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query(
        "INSERT INTO webinar_events (webinar_id, participant_id, type) VALUES ($1, $2, 'leave')",
    )
    .bind(webinar_id)
    .bind(participant_id)
    .execute(pool)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Register a connection and return its outbound receiver so a test can read
    /// exactly what the registry pushes to it. Mirrors the real `join` path.
    fn add(
        reg: &PresenceRegistry,
        code: &str,
        is_host: bool,
        lang: Option<&str>,
    ) -> mpsc::UnboundedReceiver<Message> {
        let (tx, rx) = mpsc::unbounded_channel();
        reg.join(
            code,
            Conn {
                is_host,
                lang: lang.map(str::to_string),
                tx,
            },
        );
        rx
    }

    fn text_of(msg: &Message) -> &str {
        match msg {
            Message::Text(t) => t.as_str(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    #[test]
    fn subtitle_event_final_json_matches_contract() {
        let mut translations = HashMap::new();
        translations.insert("en".to_string(), "hello everyone".to_string());
        translations.insert("es".to_string(), "hola a todos".to_string());
        let ev = SubtitleEvent::Final {
            original: "ciao a tutti".to_string(),
            lang: "it".to_string(),
            translations,
        };
        let v: serde_json::Value = serde_json::from_str(&ev.to_json()).unwrap();
        assert_eq!(v["type"], "subtitle");
        assert_eq!(v["kind"], "final");
        assert_eq!(v["original"], "ciao a tutti");
        assert_eq!(v["lang"], "it");
        assert_eq!(v["translations"]["en"], "hello everyone");
        assert_eq!(v["translations"]["es"], "hola a todos");
        // A final frame carries NO interim-only `text` field.
        assert!(v.get("text").is_none(), "final must not carry `text`");
    }

    #[test]
    fn subtitle_event_interim_json_matches_contract() {
        let ev = SubtitleEvent::Interim {
            text: "ciao a".to_string(),
            lang: "it".to_string(),
        };
        let v: serde_json::Value = serde_json::from_str(&ev.to_json()).unwrap();
        assert_eq!(v["type"], "subtitle");
        assert_eq!(v["kind"], "interim");
        assert_eq!(v["text"], "ciao a");
        assert_eq!(v["lang"], "it");
        // Interim is untranslated — no `original`/`translations`.
        assert!(v.get("original").is_none());
        assert!(v.get("translations").is_none());
    }

    #[test]
    fn subtitle_frames_carry_no_host_pii() {
        let mut translations = HashMap::new();
        translations.insert("en".to_string(), "hi".to_string());
        let json = SubtitleEvent::Final {
            original: "ciao".to_string(),
            lang: "it".to_string(),
            translations,
        }
        .to_json();
        for leaked in ["host_user_id", "user_id", "email", "org_id", "speaker_id"] {
            assert!(!json.contains(leaked), "subtitle frame leaked `{leaked}`");
        }
    }

    #[test]
    fn target_languages_are_distinct_non_host_viewers() {
        let reg = PresenceRegistry::new();
        let _es1 = add(&reg, "C", false, Some("es"));
        let _es2 = add(&reg, "C", false, Some("es")); // duplicate collapses
        let _fr = add(&reg, "C", false, Some("fr"));
        let _de = add(&reg, "C", false, Some("de"));
        let _host = add(&reg, "C", true, Some("zh")); // host excluded
        let _no_lang = add(&reg, "C", false, None); // no lang excluded

        let mut langs = reg.target_languages("C");
        langs.sort();
        assert_eq!(langs, vec!["de", "es", "fr"]);
    }

    #[test]
    fn target_languages_reads_live_registry_for_late_joiners() {
        let reg = PresenceRegistry::new();
        let _es = add(&reg, "C", false, Some("es"));
        assert_eq!(reg.target_languages("C"), vec!["es".to_string()]);
        // A late joiner's language shows up immediately on the next read.
        let _fr = add(&reg, "C", false, Some("fr"));
        let mut langs = reg.target_languages("C");
        langs.sort();
        assert_eq!(langs, vec!["es", "fr"]);
    }

    #[test]
    fn target_languages_unknown_code_is_empty() {
        let reg = PresenceRegistry::new();
        assert!(reg.target_languages("nope").is_empty());
    }

    #[test]
    fn broadcast_subtitle_reaches_every_viewer() {
        let reg = PresenceRegistry::new();
        let mut es = add(&reg, "C", false, Some("es"));
        let mut fr = add(&reg, "C", false, Some("fr"));

        let mut translations = HashMap::new();
        translations.insert("es".to_string(), "hola".to_string());
        translations.insert("fr".to_string(), "salut".to_string());
        let ev = SubtitleEvent::Final {
            original: "ciao".to_string(),
            lang: "it".to_string(),
            translations,
        };
        reg.broadcast_subtitle("C", &ev);

        for rx in [&mut es, &mut fr] {
            let msg = rx.try_recv().expect("viewer got a subtitle frame");
            let v: serde_json::Value = serde_json::from_str(text_of(&msg)).unwrap();
            assert_eq!(v["kind"], "final");
            assert_eq!(v["translations"]["es"], "hola");
        }
    }

    #[test]
    fn broadcast_subtitle_skips_the_host() {
        let reg = PresenceRegistry::new();
        let mut viewer = add(&reg, "C", false, Some("es"));
        let mut host = add(&reg, "C", true, None);

        reg.broadcast_subtitle(
            "C",
            &SubtitleEvent::Interim {
                text: "ciao".to_string(),
                lang: "it".to_string(),
            },
        );

        assert!(viewer.try_recv().is_ok(), "viewer receives the subtitle");
        assert!(
            host.try_recv().is_err(),
            "host studio must NOT receive its own subtitle"
        );
    }

    #[test]
    fn broadcast_subtitle_unknown_code_is_a_noop() {
        let reg = PresenceRegistry::new();
        // No panic, nothing to deliver.
        reg.broadcast_subtitle(
            "ghost",
            &SubtitleEvent::Interim {
                text: "x".to_string(),
                lang: "en".to_string(),
            },
        );
    }
}
