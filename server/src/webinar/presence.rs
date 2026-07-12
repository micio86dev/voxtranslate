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
    tx: mpsc::UnboundedSender<Message>,
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
    let lang = params.lang.clone();

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

    let conn_id = state.webinar_presence.join(&code, Conn { is_host, tx });
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

    // Read until the socket closes (ignore inbound payloads for now).
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
