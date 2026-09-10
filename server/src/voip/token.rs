//! Single-use tickets for the media socket (spec 0111, §O "SSRF", R27).
//!
//! The provider connects **inbound** to a URL we hand it, from an address we do not
//! control, with no session and no credential of ours. The URL therefore has to be the
//! credential — and a URL that is a credential has to be unguessable, short-lived, bound
//! to exactly one call, and usable once.
//!
//! ## Why signed rather than a database lookup
//!
//! A random opaque id checked against a table would work and would be simpler. It is not
//! what this does, for one reason: the media socket arrives within milliseconds of
//! `streaming_start`, on the hot path of call setup, and a database round-trip there sits
//! directly in the audio's time-to-first-sample. The ticket carries what the bridge needs
//! and proves it with an HMAC, so accepting a connection is a hash, not a query.
//!
//! ## What it does NOT protect against
//!
//! Replay **across processes**. The single-use registry is per-process, so a ticket
//! replayed against a different instance inside its 60-second window would be accepted
//! there. That is not a gap worth closing with shared state today, because the media
//! socket must reach the instance holding the call's room anyway — rooms are in-memory per
//! instance, which is a property of the whole product, not of this feature. When rooms
//! become distributed, this registry has to move with them, and the note is here so that
//! is not discovered the hard way.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// How long a ticket is valid. The provider connects within milliseconds of
/// `streaming_start`; a minute is generous and still short enough that a leaked URL in a
/// log is worthless by the time anyone reads it.
pub const TICKET_TTL_SECS: i64 = 60;

/// What the bridge needs to serve the socket, carried in the ticket so that accepting a
/// connection costs a hash rather than a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTicket {
    /// The call this socket belongs to.
    pub call_id: Uuid,
    /// The provider's leg id, so a ticket for leg A cannot serve leg B.
    pub leg_id: String,
    /// The room the phone peer joined.
    pub room: String,
    /// The synthetic peer id inside that room.
    pub peer_id: String,
    /// Unix seconds after which this ticket is refused.
    pub exp: i64,
    /// Makes every ticket distinct even for the same call and leg, so the single-use
    /// registry has something to key on.
    pub nonce: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    /// Not the right shape at all.
    Malformed,
    /// Signed with a different key, or tampered with.
    BadSignature,
    /// Past its expiry.
    Expired,
    /// Already used. The second attempt is refused even though it verifies.
    Replayed,
}

impl TicketError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "ticket_malformed",
            Self::BadSignature => "ticket_bad_signature",
            Self::Expired => "ticket_expired",
            Self::Replayed => "ticket_replayed",
        }
    }
}

/// Mint a ticket: `<base64url payload>.<base64url hmac>`.
pub fn mint(key: &[u8], ticket: &MediaTicket) -> String {
    let payload = serde_json::to_vec(ticket).expect("MediaTicket serialises");
    let encoded = B64.encode(&payload);
    let sig = sign(key, encoded.as_bytes());
    format!("{encoded}.{sig}")
}

fn sign(key: &[u8], msg: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    B64.encode(mac.finalize().into_bytes())
}

/// Verify a ticket's signature and expiry. Does **not** consume it — see
/// [`TicketRegistry::redeem`], which is what makes it single-use.
pub fn verify(key: &[u8], raw: &str, now_unix: i64) -> Result<MediaTicket, TicketError> {
    let (encoded, sig) = raw.split_once('.').ok_or(TicketError::Malformed)?;

    // Constant-time: `==` on a String leaks position through timing, and a forgery oracle
    // on this signature is a forgery oracle on the media socket.
    let expected = sign(key, encoded.as_bytes());
    if expected.as_bytes().ct_eq(sig.as_bytes()).unwrap_u8() != 1 {
        return Err(TicketError::BadSignature);
    }

    let bytes = B64.decode(encoded).map_err(|_| TicketError::Malformed)?;
    let ticket: MediaTicket = serde_json::from_slice(&bytes).map_err(|_| TicketError::Malformed)?;

    if now_unix > ticket.exp {
        return Err(TicketError::Expired);
    }
    Ok(ticket)
}

/// Build a ticket for a leg, valid for [`TICKET_TTL_SECS`].
pub fn issue(
    key: &[u8],
    call_id: Uuid,
    leg_id: &str,
    room: &str,
    peer_id: &str,
    now_unix: i64,
) -> String {
    mint(
        key,
        &MediaTicket {
            call_id,
            leg_id: leg_id.to_string(),
            room: room.to_string(),
            peer_id: peer_id.to_string(),
            exp: now_unix + TICKET_TTL_SECS,
            nonce: Uuid::new_v4().simple().to_string(),
        },
    )
}

/// Remembers which tickets have been spent.
///
/// Bounded by the TTL rather than by a cap: entries are dropped once they can no longer be
/// replayed, so the set's size is proportional to the call rate over one minute and cannot
/// grow without bound even under a flood of forged attempts (which never get this far —
/// the signature check comes first).
#[derive(Debug, Default)]
pub struct TicketRegistry {
    spent: Mutex<HashMap<String, i64>>,
}

impl TicketRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify **and** consume. The second presentation of a valid ticket is refused.
    pub fn redeem(&self, key: &[u8], raw: &str, now_unix: i64) -> Result<MediaTicket, TicketError> {
        let ticket = verify(key, raw, now_unix)?;

        let mut spent = self.spent.lock().expect("ticket registry poisoned");
        // Sweep on the way in: anything past its expiry can no longer be replayed, so
        // remembering it is pure cost.
        spent.retain(|_, exp| *exp >= now_unix);

        if spent.insert(ticket.nonce.clone(), ticket.exp).is_some() {
            return Err(TicketError::Replayed);
        }
        Ok(ticket)
    }

    /// How many spent tickets are still remembered. Diagnostics only.
    pub fn len(&self) -> usize {
        self.spent.lock().expect("ticket registry poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Build the URL handed to the provider as `stream_url`.
///
/// The base comes from configuration and the ticket from us — **never** from a request.
/// A `stream_url` an attacker could influence would make the provider a confused deputy
/// pointed at anything reachable from its network, which is the SSRF this guards.
pub fn media_url(base: &str, ticket: &str) -> String {
    format!("{}/voip/media/{}", base.trim_end_matches('/'), ticket)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"a-server-side-media-key";

    fn now() -> i64 {
        1_760_000_000
    }

    fn ticket() -> MediaTicket {
        MediaTicket {
            call_id: Uuid::nil(),
            leg_id: "v3:leg-abc".into(),
            room: "ph-123".into(),
            peer_id: "phone-1".into(),
            exp: now() + TICKET_TTL_SECS,
            nonce: "n1".into(),
        }
    }

    #[test]
    fn a_minted_ticket_round_trips() {
        let raw = mint(KEY, &ticket());
        assert_eq!(verify(KEY, &raw, now()).unwrap(), ticket());
    }

    #[test]
    fn a_ticket_signed_with_another_key_is_refused() {
        let raw = mint(b"someone-elses-key", &ticket());
        assert_eq!(
            verify(KEY, &raw, now()).unwrap_err(),
            TicketError::BadSignature
        );
    }

    #[test]
    fn tampering_with_the_payload_invalidates_the_signature() {
        // The attack this stops: take a valid ticket for your own call, swap the room for
        // somebody else's, and listen in.
        let raw = mint(KEY, &ticket());
        let (encoded, sig) = raw.split_once('.').unwrap();

        let mut stolen = ticket();
        stolen.room = "someone-elses-room".into();
        let forged_payload = B64.encode(serde_json::to_vec(&stolen).unwrap());
        assert_ne!(forged_payload, encoded);

        assert_eq!(
            verify(KEY, &format!("{forged_payload}.{sig}"), now()).unwrap_err(),
            TicketError::BadSignature
        );
    }

    #[test]
    fn an_expired_ticket_is_refused_even_though_it_verifies() {
        let raw = mint(KEY, &ticket());
        assert!(
            verify(KEY, &raw, now() + TICKET_TTL_SECS).is_ok(),
            "still valid at the boundary"
        );
        assert_eq!(
            verify(KEY, &raw, now() + TICKET_TTL_SECS + 1).unwrap_err(),
            TicketError::Expired
        );
    }

    #[test]
    fn junk_is_malformed_rather_than_a_panic() {
        for raw in ["", ".", "no-dot", "!!!.!!!", "a.b.c"] {
            assert!(verify(KEY, raw, now()).is_err(), "{raw:?} must be refused");
        }
        // A well-signed payload that is not a ticket is malformed, not accepted.
        let encoded = B64.encode(b"{\"nope\":true}");
        let sig = sign(KEY, encoded.as_bytes());
        assert_eq!(
            verify(KEY, &format!("{encoded}.{sig}"), now()).unwrap_err(),
            TicketError::Malformed
        );
    }

    #[test]
    fn a_ticket_can_be_redeemed_once() {
        let reg = TicketRegistry::new();
        let raw = mint(KEY, &ticket());

        assert!(reg.redeem(KEY, &raw, now()).is_ok());
        assert_eq!(
            reg.redeem(KEY, &raw, now()).unwrap_err(),
            TicketError::Replayed,
            "the second presentation is refused even though it still verifies"
        );
    }

    #[test]
    fn two_tickets_for_the_same_call_are_independent() {
        // Both legs of a bridged call, or a reconnect after a media drop.
        let reg = TicketRegistry::new();
        let a = issue(KEY, Uuid::nil(), "leg-a", "ph-1", "phone-1", now());
        let b = issue(KEY, Uuid::nil(), "leg-b", "ph-1", "phone-2", now());
        assert_ne!(a, b, "each issue gets its own nonce");
        assert!(reg.redeem(KEY, &a, now()).is_ok());
        assert!(reg.redeem(KEY, &b, now()).is_ok());
    }

    #[test]
    fn spent_tickets_are_forgotten_once_they_can_no_longer_be_replayed() {
        // Otherwise the registry is an unbounded leak on a long-lived process.
        let reg = TicketRegistry::new();
        for i in 0..8 {
            let t = MediaTicket {
                nonce: format!("n{i}"),
                ..ticket()
            };
            reg.redeem(KEY, &mint(KEY, &t), now()).unwrap();
        }
        assert_eq!(reg.len(), 8);

        // One redemption after they have all expired sweeps them.
        let fresh = MediaTicket {
            nonce: "fresh".into(),
            exp: now() + 10_000,
            ..ticket()
        };
        reg.redeem(KEY, &mint(KEY, &fresh), now() + TICKET_TTL_SECS + 1)
            .unwrap();
        assert_eq!(reg.len(), 1, "only the unexpired one is still remembered");
        assert!(!reg.is_empty());
    }

    #[test]
    fn an_expired_ticket_is_never_recorded_as_spent() {
        // It failed before the registry saw it, so it must not occupy a slot.
        let reg = TicketRegistry::new();
        let raw = mint(KEY, &ticket());
        assert!(reg.redeem(KEY, &raw, now() + TICKET_TTL_SECS + 1).is_err());
        assert!(reg.is_empty());
    }

    #[test]
    fn issued_tickets_expire_on_their_own() {
        let raw = issue(KEY, Uuid::nil(), "leg", "room", "peer", now());
        let t = verify(KEY, &raw, now()).unwrap();
        assert_eq!(t.exp, now() + TICKET_TTL_SECS);
        assert_eq!(t.leg_id, "leg");
        assert_eq!(t.room, "room");
        assert_eq!(t.peer_id, "peer");
    }

    #[test]
    fn the_media_url_is_built_from_configuration_and_a_ticket_only() {
        // Nothing from a request reaches this. A stream_url an attacker could influence
        // makes the provider a confused deputy pointed at anything on its network.
        assert_eq!(
            media_url("wss://media.example.com", "tok"),
            "wss://media.example.com/voip/media/tok"
        );
        assert_eq!(
            media_url("wss://media.example.com/", "tok"),
            "wss://media.example.com/voip/media/tok"
        );
    }

    #[test]
    fn a_ticket_is_url_safe_so_it_survives_being_a_path_segment() {
        let raw = issue(KEY, Uuid::new_v4(), "v3:leg/with+chars", "r", "p", now());
        assert!(
            raw.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
            "ticket must be safe in a URL path: {raw}"
        );
    }

    #[test]
    fn error_codes_are_distinct_and_machine_readable() {
        let all = [
            TicketError::Malformed,
            TicketError::BadSignature,
            TicketError::Expired,
            TicketError::Replayed,
        ];
        let codes: std::collections::HashSet<&str> = all.iter().map(|e| e.code()).collect();
        assert_eq!(codes.len(), all.len());
    }
}
