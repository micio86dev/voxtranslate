//! Ephemeral room registry. Each room holds its peers and a visibility (public
//! rooms are listed in the lobby). Every peer can speak, listen, connect P2P via
//! WebRTC, and chat. Rooms are capped at `MAX_PEERS`.

use dashmap::DashMap;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::protocol::{Member, PeerInfo, PublicRoom};

/// Maximum peers per room (WebRTC full mesh stays cheap up to this).
pub const MAX_PEERS: usize = 4;

/// Room visibility. Public rooms are advertised in the lobby (`GET /rooms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
}

/// A connected peer: identity, the language they speak/receive in, and a channel
/// to push JSON text frames to their WebSocket.
#[derive(Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub lang: String,
    /// Google avatar URL for authenticated users; `None` for guests.
    pub avatar_url: Option<String>,
    pub tx: UnboundedSender<String>,
}

/// A room and its current peers.
struct Room {
    visibility: Visibility,
    /// Identity of this room *lifetime* for transcript persistence. A fresh id
    /// is generated when the room is (re)created after being empty.
    session_id: Uuid,
    peers: Vec<Peer>,
}

/// Result of joining a room: the room's call-session id plus the peers that
/// were already present.
pub struct Joined {
    pub session_id: Uuid,
    pub existing: Vec<PeerInfo>,
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
}

impl RoomManager {
    pub fn new() -> Self {
        Self::default()
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
            });
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
            })
            .collect();
        room.peers.push(peer);
        Ok(Joined {
            session_id: room.session_id,
            existing,
        })
    }

    /// Remove a peer by id, dropping the room once empty. Returns the dropped
    /// room's session id iff this removal emptied it (the session is over).
    pub fn remove(&self, room_id: &str, id: &str) -> Option<Uuid> {
        if let Some(mut room) = self.rooms.get_mut(room_id) {
            room.peers.retain(|p| p.id != id);
        }
        self.rooms
            .remove_if(room_id, |_, room| room.peers.is_empty())
            .map(|(_, room)| room.session_id)
    }

    /// Send to every peer in the room, pruning dead channels.
    pub fn broadcast(&self, room_id: &str, message: &str) {
        if let Some(mut room) = self.rooms.get_mut(room_id) {
            room.peers
                .retain(|p| p.tx.send(message.to_string()).is_ok());
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

    /// Send to a single peer by id. Used for signaling relay and self-feedback.
    pub fn relay_to_peer(&self, room_id: &str, target_id: &str, message: &str) -> bool {
        if let Some(room) = self.rooms.get(room_id) {
            if let Some(p) = room.peers.iter().find(|p| p.id == target_id) {
                return p.tx.send(message.to_string()).is_ok();
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
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn peer(id: &str, lang: &str) -> (Peer, UnboundedReceiver<String>) {
        let (tx, rx) = unbounded_channel();
        (
            Peer {
                id: id.into(),
                name: id.to_uppercase(),
                lang: lang.into(),
                avatar_url: None,
                tx,
            },
            rx,
        )
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
        let (a, _ra) = peer("a", "it");
        rm.join("r", a, Visibility::Public).unwrap();
        rm.remove("r", "a");
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
        let (a, _ra) = peer("a", "it");
        let s1 = rm.join("r", a, Visibility::Public).unwrap().session_id;
        let (b, _rb) = peer("b", "en");
        let s2 = rm.join("r", b, Visibility::Public).unwrap().session_id;
        assert_eq!(s1, s2, "same room lifetime -> same session id");

        assert!(rm.remove("r", "a").is_none(), "room not yet empty");
        assert_eq!(rm.remove("r", "b"), Some(s1), "last leave ends the session");

        let (c, _rc) = peer("c", "es");
        let s3 = rm.join("r", c, Visibility::Public).unwrap().session_id;
        assert_ne!(s3, s1, "re-created room gets a fresh session id");
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
}
