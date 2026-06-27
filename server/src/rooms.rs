//! Ephemeral room registry. Each room holds its peers and a visibility (public
//! rooms are listed in the lobby). Every peer can speak, listen, connect P2P via
//! WebRTC, and chat. Rooms are capped at `MAX_PEERS`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tokio::sync::mpsc::{self, error::TrySendError, Receiver};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::protocol::{Member, PeerInfo, PublicRoom, WhiteboardOp};

/// Maximum peers per room (WebRTC full mesh stays cheap up to this).
pub const MAX_PEERS: usize = 4;

/// Cap on stored whiteboard ops per room (spec 0045): bounds memory for the
/// late-joiner snapshot. Past this the oldest Draw ops are dropped.
const MAX_WHITEBOARD_OPS: usize = 4000;

/// Room visibility. Public rooms are advertised in the lobby (`GET /rooms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
}

/// Capacity of a peer's bounded outbound channel (issue #123). Outbound frames
/// (signalling, chat, subtitles, snapshots, peer state) are tiny and a healthy
/// client's `pump_to_ws` drains them to the socket immediately, so the backlog
/// sits near zero. The cap exists only to bound memory against a *stalled*
/// reader: 512 frames is far above any legitimate burst (a 4-way join's
/// signalling flurry + snapshots) yet caps a dead socket's queue to a few
/// hundred KiB instead of letting it grow without limit.
pub const OUT_CHANNEL_CAP: usize = 512;

/// Bounded outbound channel to one peer's WebSocket.
///
/// Control/chat/signalling frames must **never** be silently dropped (losing one
/// would desync that peer's signalling or chat), so on persistent saturation — a
/// reader that has stopped draining — we do not drop frames: we signal the peer's
/// handler to tear the connection down cleanly. That bounds memory to
/// [`OUT_CHANNEL_CAP`] frames per peer while preserving correctness for every
/// peer that stays connected. (issue #123)
#[derive(Clone)]
pub struct PeerTx {
    tx: mpsc::Sender<String>,
    /// Notified on overflow so the owning `handle_peer` loop breaks and runs the
    /// normal teardown (PeerLeft broadcast, session finalize, …).
    overflow: Arc<Notify>,
}

impl PeerTx {
    /// Build a bounded peer channel: the cloneable sender, the receiver drained
    /// by `pump_to_ws`, and the overflow signal the handler must await.
    pub fn channel(cap: usize) -> (PeerTx, Receiver<String>, Arc<Notify>) {
        let (tx, rx) = mpsc::channel(cap);
        let overflow = Arc::new(Notify::new());
        (
            PeerTx {
                tx,
                overflow: overflow.clone(),
            },
            rx,
            overflow,
        )
    }

    /// Queue a frame for this peer. Returns `false` only when the receiver is
    /// gone (the peer disconnected) so callers can prune it. On overflow the
    /// frame is dropped *and* the handler is signalled to close, but the peer is
    /// kept until that clean teardown runs — so `false` never means "dropped".
    pub fn send(&self, msg: String) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.overflow.notify_one();
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    }

    /// Whether the receiver has been dropped (the peer is gone), for `prune`.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Signal the owning `handle_peer` loop to tear down now. Used to evict a
    /// stale connection when the same peer id reconnects, so the dead socket
    /// doesn't linger until its TCP timeout (there is no WS heartbeat).
    pub fn signal_close(&self) {
        self.overflow.notify_one();
    }
}

/// A connected peer: identity, the language they speak/receive in, and a channel
/// to push JSON text frames to their WebSocket.
#[derive(Clone)]
pub struct Peer {
    pub id: String,
    /// Unique per *connection*, distinct from the user-facing `id` (which a
    /// reconnect reuses). Removal matches on `(id, conn)` so a stale connection's
    /// teardown can't drop the live one that superseded it (the reconnect bug).
    pub conn: Uuid,
    pub name: String,
    pub lang: String,
    /// The engine whose quality this peer wants to **receive** (spec 0099,
    /// listener-pays). When someone else speaks, this peer's translation is
    /// produced by — and billed at the rate of — this engine. Guests are pinned
    /// to the default engine (they can't be billed for Premium).
    pub engine: String,
    /// Google avatar URL for authenticated users; `None` for guests.
    pub avatar_url: Option<String>,
    /// Cartesia cloned-voice id (spec 0108) for authenticated users who completed voice
    /// prep; `None` otherwise. Propagated to other peers so an Enhanced listener speaks
    /// THIS peer's translated audio in their own voice.
    pub cartesia_voice_id: Option<String>,
    pub tx: PeerTx,
    /// `true` while this peer has an open speaking session (between `Start` and
    /// `Stop`/disconnect). Listener metering (spec 0099) reads this to bill each
    /// listener for the active cross-language sources they are receiving.
    pub speaking: Arc<AtomicBool>,
}

/// One translation a speaker's audio must produce: a target language and the
/// engine that must produce it, because ≥1 current listener of that language
/// chose that engine (spec 0099). De-duplicated per `(lang, engine)` — a lang
/// with both a Standard and a Premium listener yields two routes ("ognuno il suo
/// engine"); the same lang+engine for several listeners is one route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TranslationRoute {
    pub lang: String,
    pub engine: String,
}

/// Pure routing resolver (spec 0099 §4.1): given the OTHER participants'
/// `(lang, engine)` receive-preferences and the speaker's language, return the
/// distinct `(lang, engine)` routes the speaker's audio must be translated into.
/// A listener whose language equals the speaker's, or who is still on `"auto"`,
/// is not a translation target. Order is stable (first-seen) for deterministic
/// tests and logs.
pub fn routes_for_speaker(
    listeners: impl IntoIterator<Item = (String, String)>,
    speaker_lang: &str,
) -> Vec<TranslationRoute> {
    let mut routes: Vec<TranslationRoute> = Vec::new();
    for (lang, engine) in listeners {
        if lang == "auto" || lang == speaker_lang {
            continue;
        }
        let route = TranslationRoute { lang, engine };
        if !routes.contains(&route) {
            routes.push(route);
        }
    }
    routes
}

/// A room and its current peers.
struct Room {
    visibility: Visibility,
    /// Identity of this room *lifetime* for transcript persistence. A fresh id
    /// is generated when the room is (re)created after being empty.
    session_id: Uuid,
    peers: Vec<Peer>,
    /// Collaborative-whiteboard op-log (spec 0045): doubles as the snapshot sent
    /// to late-joiners. Cleared by a `Clear` op and reset when the room empties.
    whiteboard: Vec<WhiteboardOp>,
    /// Latest mini-game state (spec 0046), opaque to the server. `None` = no game;
    /// sent as a snapshot to late-joiners. Reset when the room empties.
    game: Option<serde_json::Value>,
}

/// Result of joining a room: the room's call-session id plus the peers that
/// were already present.
pub struct Joined {
    pub session_id: Uuid,
    pub existing: Vec<PeerInfo>,
    /// The room's ACTUAL visibility (set by its creator) — a later joiner's
    /// `public` query param doesn't change it, so the client must display this,
    /// not its own toggle (#50: public/private mismatch).
    pub public: bool,
}

/// Outcome of removing a connection from a room on teardown.
pub enum LeaveOutcome {
    /// This connection was the live entry and is now gone. `Some(session)` when
    /// its departure emptied the room (the call session is over → finalize it).
    Left(Option<Uuid>),
    /// This connection had already been superseded by a same-id reconnect, so the
    /// peer is still present under a newer connection. The caller must NOT
    /// broadcast `PeerLeft` — nobody actually left.
    Superseded,
}

/// Authoritative identity of a connected peer (spec 0018 upload gate).
pub struct PeerSnapshot {
    pub name: String,
    pub lang: String,
    pub avatar_url: Option<String>,
}

/// Manages ephemeral rooms. No persistence — everything lives in memory and is
/// cleaned up as peers disconnect.
#[derive(Default)]
pub struct RoomManager {
    rooms: DashMap<String, Room>,
    /// Engine ids whose translation happens client-direct (spec 0108, Cartesia
    /// "Enhanced"): their listeners translate in the browser, so the server's
    /// "serve everyone" subtitle fan-out skips them — otherwise they'd be
    /// double-translated. Set once at startup from the engine registry; empty
    /// (the default) means no engine is client-direct, so nothing is excluded.
    client_direct_engines: OnceLock<HashSet<String>>,
}

impl RoomManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the client-direct engine ids once at startup (from the engine registry,
    /// spec 0101). Idempotent — a second call is ignored.
    pub fn init_client_direct_engines(&self, ids: HashSet<String>) {
        let _ = self.client_direct_engines.set(ids);
    }

    /// Whether `engine` translates client-direct (its listeners self-serve in-browser).
    fn is_client_direct(&self, engine: &str) -> bool {
        self.client_direct_engines
            .get()
            .map(|s| s.contains(engine))
            .unwrap_or(false)
    }

    /// Number of live rooms on this instance (a room is dropped when its last peer
    /// leaves). Backs the `/metrics` gauge (spec 0058).
    pub fn active_rooms(&self) -> usize {
        self.rooms.len()
    }

    /// Total connected peers across all rooms on this instance (spec 0058 gauge).
    pub fn active_peers(&self) -> usize {
        self.rooms.iter().map(|r| r.peers.len()).sum()
    }

    /// Add a peer to a room (creating it with `visibility` if new). Returns the
    /// room's session id + the peers already present, or `Err(())` if full.
    #[allow(clippy::result_unit_err)] // `()` = "room full"; a richer error isn't needed
    pub fn join(&self, room_id: &str, peer: Peer, visibility: Visibility) -> Result<Joined, ()> {
        let mut room = self
            .rooms
            .entry(room_id.to_string())
            .or_insert_with(|| Room {
                visibility,
                session_id: Uuid::new_v4(),
                peers: Vec::new(),
                whiteboard: Vec::new(),
                game: None,
            });
        // A reconnect reuses its peer id (the client keeps the same id across
        // socket drops). Evict any stale entry for that id BEFORE the capacity
        // check and push, so: relays reach the live socket (not a dead one),
        // the reconnect isn't rejected as "room full" by its own ghost, and the
        // stale handler is told to tear down promptly. It's a replace, not a
        // departure, so no `PeerLeft` is broadcast.
        let stale: Vec<PeerTx> = room
            .peers
            .iter()
            .filter(|p| p.id == peer.id)
            .map(|p| p.tx.clone())
            .collect();
        room.peers.retain(|p| p.id != peer.id);
        for tx in &stale {
            tx.signal_close();
        }
        if room.peers.len() >= MAX_PEERS {
            return Err(());
        }
        let existing: Vec<PeerInfo> = room
            .peers
            .iter()
            .map(|p| PeerInfo {
                id: p.id.clone(),
                user_name: p.name.clone(),
                lang: p.lang.clone(),
                avatar_url: p.avatar_url.clone(),
                cartesia_voice_id: p.cartesia_voice_id.clone(),
            })
            .collect();
        room.peers.push(peer);
        Ok(Joined {
            session_id: room.session_id,
            existing,
            public: room.visibility == Visibility::Public,
        })
    }

    /// Remove a connection by `(id, conn)`, dropping the room once empty. Matching
    /// on `conn` (not just `id`) means a stale connection's teardown leaves the
    /// live reconnect untouched — it reports [`LeaveOutcome::Superseded`] so the
    /// caller stays quiet. A real departure returns [`LeaveOutcome::Left`] with
    /// the session id iff this was the last peer (the call is over).
    pub fn remove(&self, room_id: &str, id: &str, conn: Uuid) -> LeaveOutcome {
        let removed = match self.rooms.get_mut(room_id) {
            Some(mut room) => {
                let before = room.peers.len();
                room.peers.retain(|p| !(p.id == id && p.conn == conn));
                room.peers.len() != before
            }
            None => false,
        };
        if !removed {
            // Our entry was already gone — superseded by a reconnect (or the room
            // was torn down). Nobody left; don't disturb the others.
            return LeaveOutcome::Superseded;
        }
        let ended = self
            .rooms
            .remove_if(room_id, |_, room| room.peers.is_empty())
            .map(|(_, room)| room.session_id);
        LeaveOutcome::Left(ended)
    }

    /// Send to every peer in the room, pruning dead channels.
    pub fn broadcast(&self, room_id: &str, message: &str) {
        if let Some(mut room) = self.rooms.get_mut(room_id) {
            room.peers.retain(|p| p.tx.send(message.to_string()));
        }
    }

    /// Send to every peer in the room except `except_id`.
    pub fn broadcast_except(&self, room_id: &str, except_id: &str, message: &str) {
        if let Some(room) = self.rooms.get(room_id) {
            for p in room.peers.iter().filter(|p| p.id != except_id) {
                let _ = p.tx.send(message.to_string());
            }
        }
    }

    /// Send to every peer in the room whose language is `lang`. The Premium engine
    /// translates to one output language per OpenAI session (spec 0093), so its
    /// captions and translated audio target a single language at a time — unlike
    /// the Standard fan-out, which broadcasts one message carrying every language.
    pub fn broadcast_to_lang(&self, room_id: &str, lang: &str, message: &str) {
        if let Some(room) = self.rooms.get(room_id) {
            for p in room.peers.iter().filter(|p| p.lang == lang) {
                let _ = p.tx.send(message.to_string());
            }
        }
    }

    /// Send only to peers who both receive in `lang` **and** chose `engine`
    /// (spec 0099, listener-pays). The engine dimension keeps a Premium listener's
    /// OpenAI output from reaching a Standard listener of the same language, and
    /// vice-versa — each hears the engine they pay for.
    pub fn broadcast_to_lang_engine(&self, room_id: &str, lang: &str, engine: &str, message: &str) {
        if let Some(room) = self.rooms.get(room_id) {
            for p in room
                .peers
                .iter()
                .filter(|p| p.lang == lang && p.engine == engine)
            {
                let _ = p.tx.send(message.to_string());
            }
        }
    }

    /// Broadcast to every peer EXCEPT client-direct-engine listeners (spec 0101), plus
    /// `keep_peer` whatever their engine. The Standard "serve everyone" delivery uses
    /// this in listener-pays mode: a client-direct (Cartesia "Enhanced") listener renders
    /// its OWN in-browser subtitles, so the server must not also push it Standard's —
    /// while every other listener (Standard, plus any Premium listener who fell back to
    /// capacity) is still served, and the speaker still sees their own caption via
    /// `keep_peer`. With no client-direct engine configured this is identical to
    /// [`broadcast`](Self::broadcast).
    pub fn broadcast_excluding_client_direct(&self, room_id: &str, keep_peer: &str, message: &str) {
        if let Some(room) = self.rooms.get(room_id) {
            for p in room
                .peers
                .iter()
                .filter(|p| p.id == keep_peer || !self.is_client_direct(&p.engine))
            {
                let _ = p.tx.send(message.to_string());
            }
        }
    }

    /// Send to every peer who chose `engine`, plus `peer_id` whatever their engine
    /// (spec 0099). The Standard pipeline broadcasts one `SubtitleFinal` carrying
    /// the whole translations map; in listener-pays mode it must reach the Standard
    /// listeners AND the speaker (so they see their own caption even if they
    /// themselves receive Premium), without leaking to Premium listeners (who get
    /// the OpenAI subtitle instead). No duplicate send — the filter is a single OR.
    pub fn broadcast_to_engine_or_peer(
        &self,
        room_id: &str,
        engine: &str,
        peer_id: &str,
        message: &str,
    ) {
        if let Some(room) = self.rooms.get(room_id) {
            for p in room
                .peers
                .iter()
                .filter(|p| p.engine == engine || p.id == peer_id)
            {
                let _ = p.tx.send(message.to_string());
            }
        }
    }

    /// Resolve the distinct `(lang, engine)` translation routes for a speaker's
    /// audio from the room's CURRENT listeners (spec 0099 §4.1). Excludes the
    /// speaker themselves, their own language, and `"auto"` peers. See
    /// [`routes_for_speaker`] for the pure logic.
    pub fn translation_routes(
        &self,
        room_id: &str,
        speaker_id: &str,
        speaker_lang: &str,
    ) -> Vec<TranslationRoute> {
        let Some(room) = self.rooms.get(room_id) else {
            return Vec::new();
        };
        let listeners = room
            .peers
            .iter()
            .filter(|p| p.id != speaker_id)
            .map(|p| (p.lang.clone(), p.engine.clone()))
            .collect::<Vec<_>>();
        routes_for_speaker(listeners, speaker_lang)
    }

    /// Whether any peer other than `exclude_id` currently receives via `engine`
    /// (spec 0099). Drives the speaker's capture format (PCM16 iff a Premium
    /// listener is present) and the per-speaker decision to run the Premium engine.
    pub fn has_engine_listener(&self, room_id: &str, exclude_id: &str, engine: &str) -> bool {
        self.rooms
            .get(room_id)
            .map(|room| {
                room.peers
                    .iter()
                    .any(|p| p.id != exclude_id && p.engine == engine)
            })
            .unwrap_or(false)
    }

    /// `(peer_id, receive_engine)` for every peer in the room — so the server can
    /// push each peer its capture format when the room's engine mix changes (0099).
    pub fn peer_engines(&self, room_id: &str) -> Vec<(String, String)> {
        self.rooms
            .get(room_id)
            .map(|room| {
                room.peers
                    .iter()
                    .map(|p| (p.id.clone(), p.engine.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Mark a peer's speaking session open/closed (spec 0099). Listener meters read
    /// this via [`active_source_count`](Self::active_source_count). Cheap and
    /// lock-free: the flag is an `Arc<AtomicBool>` shared with the peer's handler.
    pub fn set_speaking(&self, room_id: &str, peer_id: &str, speaking: bool) {
        if let Some(room) = self.rooms.get(room_id) {
            if let Some(p) = room.peers.iter().find(|p| p.id == peer_id) {
                p.speaking.store(speaking, Ordering::Relaxed);
            }
        }
    }

    /// How many distinct cross-language sources a `listener` is currently
    /// receiving (spec 0099 listener-metering): peers other than the listener who
    /// are speaking right now and whose language differs from the listener's. This
    /// is the listener's billable stream count — symmetric to the speaker-pays
    /// `scale_by_target_count`. `"auto"` speakers (language not yet known) don't
    /// count: no translation is produced for them.
    pub fn active_source_count(
        &self,
        room_id: &str,
        listener_id: &str,
        listener_lang: &str,
    ) -> usize {
        let Some(room) = self.rooms.get(room_id) else {
            return 0;
        };
        room.peers
            .iter()
            .filter(|p| {
                p.id != listener_id
                    && p.lang != "auto"
                    && p.lang != listener_lang
                    && p.speaking.load(Ordering::Relaxed)
            })
            .count()
    }

    /// Apply a whiteboard op to the room's stored op-log (spec 0045): `Clear`
    /// wipes it, `Draw` appends (dropping the oldest past `MAX_WHITEBOARD_OPS`).
    pub fn whiteboard_apply(&self, room_id: &str, op: WhiteboardOp) {
        if let Some(mut room) = self.rooms.get_mut(room_id) {
            match op {
                WhiteboardOp::Clear => room.whiteboard.clear(),
                draw => {
                    room.whiteboard.push(draw);
                    let len = room.whiteboard.len();
                    if len > MAX_WHITEBOARD_OPS {
                        room.whiteboard.drain(0..len - MAX_WHITEBOARD_OPS);
                    }
                }
            }
        }
    }

    /// The room's current whiteboard op-log, for the late-joiner snapshot.
    pub fn whiteboard_snapshot(&self, room_id: &str) -> Vec<WhiteboardOp> {
        self.rooms
            .get(room_id)
            .map(|r| r.whiteboard.clone())
            .unwrap_or_default()
    }

    /// Store the latest mini-game state (spec 0046); JSON `null` ends the game.
    pub fn game_set(&self, room_id: &str, state: serde_json::Value) {
        if let Some(mut room) = self.rooms.get_mut(room_id) {
            room.game = if state.is_null() { None } else { Some(state) };
        }
    }

    /// The room's current mini-game state, for the late-joiner snapshot.
    pub fn game_snapshot(&self, room_id: &str) -> Option<serde_json::Value> {
        self.rooms.get(room_id).and_then(|r| r.game.clone())
    }

    /// Send to a single peer by id. Used for signaling relay and self-feedback.
    pub fn relay_to_peer(&self, room_id: &str, target_id: &str, message: &str) -> bool {
        if let Some(room) = self.rooms.get(room_id) {
            if let Some(p) = room.peers.iter().find(|p| p.id == target_id) {
                return p.tx.send(message.to_string());
            }
        }
        false
    }

    /// Distinct languages present in the room, excluding `exclude_id`. Used by the
    /// translation fan-out to know which languages to translate into. Peers still
    /// in `"auto"` (detection pending) are skipped — "auto" is never a translation
    /// target.
    pub fn get_room_languages(&self, room_id: &str, exclude_id: &str) -> Vec<String> {
        let mut langs: Vec<String> = Vec::new();
        if let Some(room) = self.rooms.get(room_id) {
            for p in room.peers.iter() {
                if p.id != exclude_id && p.lang != "auto" && !langs.contains(&p.lang) {
                    langs.push(p.lang.clone());
                }
            }
        }
        langs
    }

    /// Distinct languages of the room's listeners who chose `engine`, excluding
    /// `exclude_id` and `"auto"` peers (spec 0099). The listener-pays analog of
    /// [`get_room_languages`](Self::get_room_languages): an engine translates the
    /// speaker's audio only into the languages that ITS listeners receive in, so a
    /// Premium session is opened for a language only when a Premium listener wants
    /// it (and likewise for Standard).
    pub fn target_langs_for_engine(
        &self,
        room_id: &str,
        exclude_id: &str,
        engine: &str,
    ) -> Vec<String> {
        let mut langs: Vec<String> = Vec::new();
        if let Some(room) = self.rooms.get(room_id) {
            for p in room.peers.iter() {
                if p.id != exclude_id
                    && p.engine == engine
                    && p.lang != "auto"
                    && !langs.contains(&p.lang)
                {
                    langs.push(p.lang.clone());
                }
            }
        }
        langs
    }

    /// Change a peer's receive-engine in place (spec 0099). Used when a listener
    /// exhausts their balance: drop them to the default engine so they keep
    /// receiving (free) translation without being billed for Premium. Returns
    /// `false` when the room/peer is gone.
    pub fn set_peer_engine(&self, room_id: &str, peer_id: &str, engine: &str) -> bool {
        if let Some(mut room) = self.rooms.get_mut(room_id) {
            if let Some(p) = room.peers.iter_mut().find(|p| p.id == peer_id) {
                p.engine = engine.to_string();
                return true;
            }
        }
        false
    }

    /// Update a peer's language in place (auto-detect result or a manual
    /// `set_lang` correction). Returns `false` when the room/peer is gone.
    pub fn set_peer_lang(&self, room_id: &str, peer_id: &str, lang: &str) -> bool {
        if let Some(mut room) = self.rooms.get_mut(room_id) {
            if let Some(p) = room.peers.iter_mut().find(|p| p.id == peer_id) {
                p.lang = lang.to_string();
                return true;
            }
        }
        false
    }

    /// Authoritative identity snapshot of a connected peer (spec 0018): name,
    /// live language, and avatar. `None` when the peer isn't in the room — this
    /// is the membership gate for the chat-file upload endpoint (a non-member
    /// can't post into a room they aren't in).
    pub fn peer_snapshot(&self, room_id: &str, peer_id: &str) -> Option<PeerSnapshot> {
        self.rooms
            .get(room_id)?
            .peers
            .iter()
            .find(|p| p.id == peer_id)
            .map(|p| PeerSnapshot {
                name: p.name.clone(),
                lang: p.lang.clone(),
                avatar_url: p.avatar_url.clone(),
            })
    }

    /// A peer's current receive-engine (spec 0099). `None` when the peer is gone.
    pub fn peer_engine(&self, room_id: &str, peer_id: &str) -> Option<String> {
        self.rooms
            .get(room_id)?
            .peers
            .iter()
            .find(|p| p.id == peer_id)
            .map(|p| p.engine.clone())
    }

    /// A peer's *current* language (live, post-detection) — the `lang` captured
    /// at join time goes stale once auto-detect or `set_lang` updates it.
    pub fn peer_lang(&self, room_id: &str, peer_id: &str) -> Option<String> {
        self.rooms
            .get(room_id)?
            .peers
            .iter()
            .find(|p| p.id == peer_id)
            .map(|p| p.lang.clone())
    }

    /// The room's current call-session id (the transcript-persistence lifetime
    /// id). `None` when the room doesn't exist. Used by the chat-file upload to
    /// tie an upload to the call session (spec 0018).
    pub fn session_id(&self, room_id: &str) -> Option<Uuid> {
        self.rooms.get(room_id).map(|r| r.session_id)
    }

    /// Snapshot of all public rooms with their online members, for the lobby.
    pub fn public_rooms(&self) -> Vec<PublicRoom> {
        let mut out: Vec<PublicRoom> = self
            .rooms
            .iter()
            .filter(|r| r.value().visibility == Visibility::Public && !r.value().peers.is_empty())
            .map(|r| PublicRoom {
                room: r.key().clone(),
                count: r.value().peers.len(),
                participants: r
                    .value()
                    .peers
                    .iter()
                    .map(|p| Member {
                        name: p.name.clone(),
                        lang: p.lang.clone(),
                        avatar: p.avatar_url.clone(),
                    })
                    .collect(),
            })
            .collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.room.cmp(&b.room)));
        out
    }

    /// Drop peers whose receiver has been dropped, and remove empty rooms.
    /// Returns the session ids of the rooms that were dropped (sessions over).
    pub fn prune(&self) -> Vec<Uuid> {
        let mut dropped = Vec::new();
        self.rooms.retain(|_, room| {
            room.peers.retain(|p| !p.tx.is_closed());
            if room.peers.is_empty() {
                dropped.push(room.session_id);
                false
            } else {
                true
            }
        });
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, lang: &str) -> (Peer, Receiver<String>) {
        peer_conn(id, lang, Uuid::new_v4())
    }

    /// Like `peer` but with an explicit connection id, for reconnect tests.
    fn peer_conn(id: &str, lang: &str, conn: Uuid) -> (Peer, Receiver<String>) {
        peer_full(id, lang, "standard", conn)
    }

    /// Peer with an explicit receive-engine (spec 0099 routing/billing tests).
    fn peer_full(id: &str, lang: &str, engine: &str, conn: Uuid) -> (Peer, Receiver<String>) {
        let (tx, rx, _overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
        (
            Peer {
                id: id.into(),
                conn,
                name: id.to_uppercase(),
                lang: lang.into(),
                engine: engine.into(),
                avatar_url: None,
                cartesia_voice_id: None,
                tx,
                speaking: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    #[tokio::test]
    async fn peertx_overflow_keeps_peer_and_signals_close() {
        // A stalled reader (the receiver is never drained) fills the bounded
        // buffer; the overflowing frame is dropped, the peer is *kept* (send →
        // true, so it is never silently pruned), and the handler is signalled to
        // close — bounding memory without dropping a control frame on a live peer.
        let (tx, _rx, overflow) = PeerTx::channel(2);
        assert!(tx.send("a".into()));
        assert!(tx.send("b".into()));
        assert!(tx.send("c".into()), "overflow keeps the peer");
        tokio::time::timeout(std::time::Duration::from_millis(200), overflow.notified())
            .await
            .expect("overflow signalled the handler to close");
    }

    #[tokio::test]
    async fn peertx_send_reports_closed_receiver() {
        let (tx, rx, _overflow) = PeerTx::channel(2);
        drop(rx);
        assert!(!tx.send("x".into()), "closed receiver → prune the peer");
        assert!(tx.is_closed());
    }

    #[test]
    fn join_returns_existing_and_caps_at_max() {
        let rm = RoomManager::new();
        let (a, _ra) = peer("a", "it");
        assert_eq!(
            rm.join("r", a, Visibility::Public).unwrap().existing.len(),
            0
        );
        let (b, _rb) = peer("b", "en");
        let existing = rm.join("r", b, Visibility::Public).unwrap().existing;
        assert_eq!(existing.len(), 1);
        assert_eq!(existing[0].id, "a");
        let (c, _rc) = peer("c", "es");
        rm.join("r", c, Visibility::Public).unwrap();
        let (d, _rd) = peer("d", "fr");
        rm.join("r", d, Visibility::Public).unwrap();
        let (e, _re) = peer("e", "de");
        assert!(
            rm.join("r", e, Visibility::Public).is_err(),
            "5th peer rejected"
        );
    }

    #[test]
    fn broadcast_relay_and_except() {
        let rm = RoomManager::new();
        let (a, mut ra) = peer("a", "it");
        let (b, mut rb) = peer("b", "en");
        rm.join("r", a, Visibility::Private).unwrap();
        rm.join("r", b, Visibility::Private).unwrap();

        rm.broadcast("r", "hi");
        assert_eq!(ra.try_recv().unwrap(), "hi");
        assert_eq!(rb.try_recv().unwrap(), "hi");

        assert!(rm.relay_to_peer("r", "b", "yo"));
        assert_eq!(rb.try_recv().unwrap(), "yo");
        assert!(ra.try_recv().is_err());
        assert!(!rm.relay_to_peer("r", "ghost", "x"));

        rm.broadcast_except("r", "a", "z");
        assert_eq!(rb.try_recv().unwrap(), "z");
        assert!(ra.try_recv().is_err());
    }

    #[test]
    fn broadcast_excluding_client_direct_skips_cartesia_keeps_others_and_speaker() {
        // Spec 0108: the Standard "serve everyone" path must reach Standard listeners and
        // a fallen-back Premium listener, but NOT a Cartesia ("enhanced") listener — who
        // renders its own in-browser subtitles — while the speaker still sees their own
        // caption even when the SPEAKER is on the client-direct engine.
        let rm = RoomManager::new();
        rm.init_client_direct_engines(HashSet::from(["cartesia".to_string()]));

        // Speaker `spk` is itself on Cartesia; std + premium are cross-language listeners;
        // `cx` is another Cartesia listener that must be excluded.
        let (spk, mut r_spk) = peer_full("spk", "it", "cartesia", Uuid::new_v4());
        let (std, mut r_std) = peer_full("std", "en", "standard", Uuid::new_v4());
        let (prem, mut r_prem) = peer_full("prem", "fr", "premium", Uuid::new_v4());
        let (cx, mut r_cx) = peer_full("cx", "de", "cartesia", Uuid::new_v4());
        rm.join("r", spk, Visibility::Private).unwrap();
        rm.join("r", std, Visibility::Private).unwrap();
        rm.join("r", prem, Visibility::Private).unwrap();
        rm.join("r", cx, Visibility::Private).unwrap();

        rm.broadcast_excluding_client_direct("r", "spk", "caption");

        assert_eq!(
            r_std.try_recv().unwrap(),
            "caption",
            "Standard listener served"
        );
        assert_eq!(
            r_prem.try_recv().unwrap(),
            "caption",
            "fallen-back Premium listener served"
        );
        assert_eq!(
            r_spk.try_recv().unwrap(),
            "caption",
            "speaker keeps own caption even on a client-direct engine"
        );
        assert!(
            r_cx.try_recv().is_err(),
            "client-direct (Cartesia) listener is NOT double-served"
        );

        // With no client-direct engine configured it behaves like a plain broadcast.
        let rm2 = RoomManager::new();
        let (a, mut ra) = peer_full("a", "it", "cartesia", Uuid::new_v4());
        rm2.join("r", a, Visibility::Private).unwrap();
        rm2.broadcast_excluding_client_direct("r", "nobody", "x");
        assert_eq!(
            ra.try_recv().unwrap(),
            "x",
            "no exclusion set → serve everyone"
        );
    }

    #[test]
    fn broadcast_to_lang_targets_one_language() {
        // Premium captions/audio target a single output language (spec 0093).
        let rm = RoomManager::new();
        let (a, mut ra) = peer("a", "it");
        let (b, mut rb) = peer("b", "en");
        let (c, mut rc) = peer("c", "en");
        rm.join("r", a, Visibility::Public).unwrap();
        rm.join("r", b, Visibility::Public).unwrap();
        rm.join("r", c, Visibility::Public).unwrap();

        rm.broadcast_to_lang("r", "en", "hi-en");
        assert_eq!(rb.try_recv().unwrap(), "hi-en");
        assert_eq!(rc.try_recv().unwrap(), "hi-en");
        assert!(ra.try_recv().is_err(), "italian peer gets nothing");

        // No peers in a language → no-op (and unknown room is a no-op).
        rm.broadcast_to_lang("r", "fr", "x");
        rm.broadcast_to_lang("nope", "en", "x");
        assert!(ra.try_recv().is_err());
    }

    #[test]
    fn languages_distinct_excluding_self() {
        let rm = RoomManager::new();
        for (id, l) in [("a", "it"), ("b", "en"), ("c", "en"), ("d", "it")] {
            let (p, r) = peer(id, l);
            std::mem::forget(r); // keep sender alive
            rm.join("r", p, Visibility::Public).unwrap();
        }
        let mut langs = rm.get_room_languages("r", "a"); // exclude a(it); distinct of b,c,d
        langs.sort();
        assert_eq!(langs, vec!["en".to_string(), "it".to_string()]);
    }

    #[test]
    fn auto_lang_excluded_from_targets_until_set() {
        let rm = RoomManager::new();
        for (id, l) in [("a", "it"), ("b", "auto")] {
            let (p, r) = peer(id, l);
            std::mem::forget(r);
            rm.join("r", p, Visibility::Public).unwrap();
        }
        // b is still detecting -> no "auto" target for a's fan-out.
        assert!(rm.get_room_languages("r", "a").is_empty());

        assert!(rm.set_peer_lang("r", "b", "en"));
        assert_eq!(rm.get_room_languages("r", "a"), vec!["en".to_string()]);
        assert_eq!(rm.peer_lang("r", "b").as_deref(), Some("en"));
        assert_eq!(rm.peer_lang("r", "a").as_deref(), Some("it"));

        assert!(!rm.set_peer_lang("r", "ghost", "fr"), "unknown peer");
        assert!(!rm.set_peer_lang("nope", "b", "fr"), "unknown room");
        assert!(rm.peer_lang("r", "ghost").is_none());
    }

    #[test]
    fn peer_snapshot_gates_on_membership() {
        let rm = RoomManager::new();
        let (mut a, _ra) = peer("a", "it");
        a.avatar_url = Some("https://x/avatar.png".into());
        rm.join("r", a, Visibility::Public).unwrap();

        let snap = rm.peer_snapshot("r", "a").expect("member present");
        assert_eq!(snap.name, "A");
        assert_eq!(snap.lang, "it");
        assert_eq!(snap.avatar_url.as_deref(), Some("https://x/avatar.png"));

        // Non-member and unknown room both yield None (the 403 gate).
        assert!(rm.peer_snapshot("r", "ghost").is_none());
        assert!(rm.peer_snapshot("nope", "a").is_none());
    }

    #[test]
    fn public_rooms_filters_and_counts() {
        let rm = RoomManager::new();
        let (a, _ra) = peer("a", "it");
        let (b, _rb) = peer("b", "en");
        rm.join("plaza", a, Visibility::Public).unwrap();
        rm.join("plaza", b, Visibility::Public).unwrap();
        let (c, _rc) = peer("c", "es");
        rm.join("secret", c, Visibility::Private).unwrap();
        let pr = rm.public_rooms();
        assert_eq!(pr.len(), 1);
        assert_eq!(pr[0].room, "plaza");
        assert_eq!(pr[0].count, 2);
        assert_eq!(pr[0].participants.len(), 2);
    }

    #[test]
    fn remove_and_prune_drop_empty_rooms() {
        let rm = RoomManager::new();
        let ca = Uuid::new_v4();
        let (a, _ra) = peer_conn("a", "it", ca);
        rm.join("r", a, Visibility::Public).unwrap();
        assert!(matches!(
            rm.remove("r", "a", ca),
            LeaveOutcome::Left(Some(_))
        ));
        assert!(rm.public_rooms().is_empty());

        let (b, rb) = peer("b", "en");
        rm.join("r2", b, Visibility::Public).unwrap();
        drop(rb); // close receiver -> sender is_closed
        rm.prune();
        assert!(rm.public_rooms().is_empty());
    }

    #[test]
    fn session_id_stable_within_room_and_fresh_after_empty() {
        let rm = RoomManager::new();
        let (ca, cb) = (Uuid::new_v4(), Uuid::new_v4());
        let (a, _ra) = peer_conn("a", "it", ca);
        let s1 = rm.join("r", a, Visibility::Public).unwrap().session_id;
        let (b, _rb) = peer_conn("b", "en", cb);
        let s2 = rm.join("r", b, Visibility::Public).unwrap().session_id;
        assert_eq!(s1, s2, "same room lifetime -> same session id");

        assert!(
            matches!(rm.remove("r", "a", ca), LeaveOutcome::Left(None)),
            "room not yet empty"
        );
        assert!(
            matches!(rm.remove("r", "b", cb), LeaveOutcome::Left(Some(sid)) if sid == s1),
            "last leave ends the session"
        );

        let (c, _rc) = peer("c", "es");
        let s3 = rm.join("r", c, Visibility::Public).unwrap().session_id;
        assert_ne!(s3, s1, "re-created room gets a fresh session id");
    }

    #[test]
    fn reconnect_supersedes_stale_entry_without_dropping_the_live_one() {
        // Same peer id reconnects (new conn) while the old entry lingers (no WS
        // heartbeat → the dead socket isn't detected yet). Fill the room to
        // capacity so we also prove the reconnect isn't rejected by its own ghost.
        let rm = RoomManager::new();
        for id in ["b", "c", "d"] {
            let (p, r) = peer(id, "en");
            std::mem::forget(r); // keep the sender alive
            rm.join("r", p, Visibility::Public).unwrap();
        }
        let old_conn = Uuid::new_v4();
        let (a_old, mut a_old_rx) = peer_conn("a", "it", old_conn);
        rm.join("r", a_old, Visibility::Public).unwrap(); // room now full (4)

        let new_conn = Uuid::new_v4();
        let (a_new, mut a_new_rx) = peer_conn("a", "it", new_conn);
        // The reconnect evicts its own ghost first, so it's NOT rejected as full,
        // and `existing` lists the real peers (not the ghost).
        let joined = rm.join("r", a_new, Visibility::Public).unwrap();
        assert_eq!(joined.existing.len(), 3);
        assert!(joined.existing.iter().all(|p| p.id != "a"));

        // Eviction signals the old handler via its overflow Notify (not the data
        // channel), so nothing is queued to the stale receiver.
        assert!(a_old_rx.try_recv().is_err());

        // A relay to "a" now reaches the LIVE socket, not the dead one.
        assert!(rm.relay_to_peer("r", "a", "hi-live"));
        assert_eq!(a_new_rx.try_recv().unwrap(), "hi-live");
        assert!(a_old_rx.try_recv().is_err(), "dead socket gets nothing");

        // The stale handler's teardown (remove by the OLD conn) must NOT drop the
        // live peer and must NOT signal a departure.
        assert!(matches!(
            rm.remove("r", "a", old_conn),
            LeaveOutcome::Superseded
        ));
        assert!(rm.relay_to_peer("r", "a", "still-here"));
        assert_eq!(a_new_rx.try_recv().unwrap(), "still-here");

        // The live connection's own teardown removes it for real.
        assert!(matches!(
            rm.remove("r", "a", new_conn),
            LeaveOutcome::Left(None)
        ));
        assert!(!rm.relay_to_peer("r", "a", "gone"));
    }

    #[test]
    fn prune_reports_dropped_sessions() {
        let rm = RoomManager::new();
        let (a, ra) = peer("a", "it");
        let sid = rm.join("r", a, Visibility::Public).unwrap().session_id;
        let (b, rb) = peer("b", "en");
        rm.join("r2", b, Visibility::Public).unwrap();
        std::mem::forget(rb); // keep r2 alive

        drop(ra); // a's receiver closed -> r drops on prune
        let dropped = rm.prune();
        assert_eq!(dropped, vec![sid]);
        assert!(rm.prune().is_empty(), "nothing left to prune");
    }

    // ---- spec 0099: listener-pays routing / delivery / metering ----

    fn route(lang: &str, engine: &str) -> TranslationRoute {
        TranslationRoute {
            lang: lang.into(),
            engine: engine.into(),
        }
    }

    #[test]
    fn routes_skip_speaker_lang_and_auto() {
        // Speaker is `it`. A listener also in `it` is not a target; an `auto`
        // listener (lang unknown) is not a target either.
        let got = routes_for_speaker(
            [
                ("it".into(), "premium".into()),   // same as speaker → skip
                ("auto".into(), "premium".into()), // detection pending → skip
                ("es".into(), "standard".into()),
            ],
            "it",
        );
        assert_eq!(got, vec![route("es", "standard")]);
    }

    #[test]
    fn routes_same_lang_mixed_engines_run_both() {
        // Two `es` listeners, one Standard one Premium → two routes ("ognuno il suo
        // engine"). A third Premium `es` listener de-dups into the same route.
        let got = routes_for_speaker(
            [
                ("es".into(), "standard".into()),
                ("es".into(), "premium".into()),
                ("es".into(), "premium".into()), // dup of the premium route
            ],
            "it",
        );
        assert_eq!(got, vec![route("es", "standard"), route("es", "premium")]);
    }

    #[test]
    fn routes_distinct_langs_each_keep_their_engine() {
        let got = routes_for_speaker(
            [
                ("es".into(), "premium".into()),
                ("fr".into(), "standard".into()),
            ],
            "it",
        );
        assert_eq!(got, vec![route("es", "premium"), route("fr", "standard")]);
    }

    #[test]
    fn routes_empty_when_alone_or_all_same_lang() {
        assert!(routes_for_speaker([], "it").is_empty());
        assert!(routes_for_speaker([("it".into(), "premium".into())], "it").is_empty());
    }

    #[test]
    fn translation_routes_reads_live_room() {
        let rm = RoomManager::new();
        let (a, _ra) = peer_full("a", "it", "premium", Uuid::new_v4());
        let (b, _rb) = peer_full("b", "es", "premium", Uuid::new_v4());
        let (c, _rc) = peer_full("c", "es", "standard", Uuid::new_v4());
        rm.join("r", a, Visibility::Public).unwrap();
        rm.join("r", b, Visibility::Public).unwrap();
        rm.join("r", c, Visibility::Public).unwrap();
        std::mem::forget(_ra);
        std::mem::forget(_rb);
        std::mem::forget(_rc);

        // Speaker `a` (it) → es listeners split by engine: two routes.
        let mut got = rm.translation_routes("r", "a", "it");
        got.sort_by(|x, y| {
            (x.lang.clone(), x.engine.clone()).cmp(&(y.lang.clone(), y.engine.clone()))
        });
        assert_eq!(got, vec![route("es", "premium"), route("es", "standard")]);
    }

    #[test]
    fn target_langs_for_engine_filters_by_engine() {
        let rm = RoomManager::new();
        let (a, _ra) = peer_full("a", "it", "standard", Uuid::new_v4()); // speaker
        let (b, _rb) = peer_full("b", "es", "premium", Uuid::new_v4());
        let (c, _rc) = peer_full("c", "fr", "standard", Uuid::new_v4());
        let (d, _rd) = peer_full("d", "es", "standard", Uuid::new_v4());
        for (p, r) in [(a, _ra), (b, _rb), (c, _rc), (d, _rd)] {
            rm.join("r", p, Visibility::Public).unwrap();
            std::mem::forget(r);
        }
        // Premium listeners of `a`: only `b` (es).
        assert_eq!(rm.target_langs_for_engine("r", "a", "premium"), vec!["es"]);
        // Standard listeners of `a`: `c` (fr) and `d` (es) — distinct langs.
        let mut std_t = rm.target_langs_for_engine("r", "a", "standard");
        std_t.sort();
        assert_eq!(std_t, vec!["es", "fr"]);
    }

    #[test]
    fn broadcast_to_lang_engine_isolates_by_engine() {
        let rm = RoomManager::new();
        let (a, _ra) = peer_full("a", "it", "standard", Uuid::new_v4());
        let (prem, mut r_prem) = peer_full("p", "es", "premium", Uuid::new_v4());
        let (std_l, mut r_std) = peer_full("s", "es", "standard", Uuid::new_v4());
        rm.join("r", a, Visibility::Public).unwrap();
        rm.join("r", prem, Visibility::Public).unwrap();
        rm.join("r", std_l, Visibility::Public).unwrap();
        std::mem::forget(_ra);

        rm.broadcast_to_lang_engine("r", "es", "premium", "OPENAI");
        // Only the premium es listener receives it.
        assert_eq!(r_prem.try_recv().unwrap(), "OPENAI");
        assert!(
            r_std.try_recv().is_err(),
            "standard listener must not get premium output"
        );
    }

    #[test]
    fn broadcast_to_engine_or_peer_reaches_engine_listeners_and_speaker() {
        let rm = RoomManager::new();
        // Speaker `a` is a Premium listener; the Standard final must still reach it.
        let (a, mut ra) = peer_full("a", "it", "premium", Uuid::new_v4());
        let (std_l, mut rstd) = peer_full("s", "es", "standard", Uuid::new_v4());
        let (prem_l, mut rprem) = peer_full("p", "fr", "premium", Uuid::new_v4());
        rm.join("r", a, Visibility::Public).unwrap();
        rm.join("r", std_l, Visibility::Public).unwrap();
        rm.join("r", prem_l, Visibility::Public).unwrap();

        rm.broadcast_to_engine_or_peer("r", "standard", "a", "STD_FINAL");
        assert_eq!(ra.try_recv().unwrap(), "STD_FINAL"); // speaker (own caption)
        assert_eq!(rstd.try_recv().unwrap(), "STD_FINAL"); // standard listener
        assert!(
            rprem.try_recv().is_err(),
            "premium-only listener must not get the standard final"
        );
    }

    #[test]
    fn active_source_count_tracks_speaking_cross_lang() {
        let rm = RoomManager::new();
        let (a, _ra) = peer_full("a", "it", "standard", Uuid::new_v4());
        let (b, _rb) = peer_full("b", "es", "premium", Uuid::new_v4());
        let (c, _rc) = peer_full("c", "it", "premium", Uuid::new_v4()); // same lang as a
        rm.join("r", a, Visibility::Public).unwrap();
        rm.join("r", b, Visibility::Public).unwrap();
        rm.join("r", c, Visibility::Public).unwrap();
        std::mem::forget(_ra);
        std::mem::forget(_rb);
        std::mem::forget(_rc);

        // Listener `b` (es): nobody speaking yet → 0 sources.
        assert_eq!(rm.active_source_count("r", "b", "es"), 0);
        // `a` (it) starts speaking → b receives 1 cross-lang source.
        rm.set_speaking("r", "a", true);
        assert_eq!(rm.active_source_count("r", "b", "es"), 1);
        // `c` (it) also speaks → b now receives 2 (two distinct it speakers).
        rm.set_speaking("r", "c", true);
        assert_eq!(rm.active_source_count("r", "b", "es"), 2);
        // For listener `c` (it), the speaking `a` is SAME language → not billable;
        // only `b` would count, and b isn't speaking → 0.
        assert_eq!(rm.active_source_count("r", "c", "it"), 0);
        // `a` stops → b back to 1.
        rm.set_speaking("r", "a", false);
        assert_eq!(rm.active_source_count("r", "b", "es"), 1);
    }
}
